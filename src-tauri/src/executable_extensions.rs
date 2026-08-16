//! Sandboxed executable extensions.
//!
//! This module intentionally does not reuse `PackageBundle`: declarative
//! packages reject WebAssembly and retain their data-only authority. Extension
//! bundles have a separate manifest, store, grant set and Wasmtime runtime.

use crate::artifact_store::ArtifactStore;
use crate::package_ecosystem::{
    signed_first_party_catalog, Compatibility, InstallSource, PackageProvenance, PackageSignature,
    RingEd25519SignatureVerifier, SemanticVersion, SignatureVerifier, TrustStore,
    VersionConstraint,
};
use futures_util::StreamExt;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component as PathComponent, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;
use url::Url;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{
    Config, Engine, ResourceLimiter, Store, StoreLimits, StoreLimitsBuilder, UpdateDeadline,
};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "extension",
        imports: { default: async },
        exports: { default: async },
        require_store_data_send: true,
    });
}

pub const EXTENSION_MANIFEST_FILE: &str = "extension.json";
pub const EXTENSION_STORE_DIRECTORY: &str = "extensions-v1";
pub const EXTENSION_SCHEMA_VERSION: u32 = 1;
pub const EXTENSION_HOST_API_VERSION: SemanticVersion = SemanticVersion::new(1, 0, 0);
pub const MAX_COMPONENT_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
pub const MAX_INPUT_BYTES: usize = 1024 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_ARTIFACT_READ_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_HTTP_BODY_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_LOG_ROWS: usize = 200;
pub const MAX_LOG_MESSAGE_BYTES: usize = 8 * 1024;
pub const MAX_PRIVATE_STATE_BYTES: usize = 256 * 1024;
/// How many artifacts one invocation may write. Bounds the ownership set a
/// native consumer checks against, and stops a guest turning its output
/// budget into an unbounded number of store entries.
pub const MAX_WRITTEN_ARTIFACTS: usize = 32;
pub const MAX_RANDOM_BYTES: u32 = 64 * 1024;
pub const DEFAULT_FUEL: u64 = 50_000_000;
pub const FUEL_YIELD_INTERVAL: u64 = 100_000;
pub const DEFAULT_MEMORY_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const PROTECTIVE_DISABLE_FAILURES: u32 = 3;
pub const MAX_STORED_INVOCATIONS: usize = 256;

/// What an invocation reports when something outside the guest ended it.
///
/// Named because three separate places have to agree on it exactly:
/// [`ExtensionManager::invoke`] returns it, `record_invocation_failure` reads
/// it to keep a cancellation off the failure counters, and the tests assert on
/// it to tell "cancelled" apart from every other way a run can stop.
pub const CANCELLED_ERROR: &str = "Extension invocation was cancelled";
/// What an invocation reports when the guest spent its whole fuel budget.
///
/// Distinct from [`CANCELLED_ERROR`] and from a plain trap on purpose: fuel is
/// the guest's own ceiling, and rewriting it as anything else would both
/// mislead the reader of the log and let a real runaway be forgiven as an
/// interruption.
pub const FUEL_EXHAUSTED_ERROR: &str = "Component exhausted its fuel budget";
/// What an invocation reports when the wall clock, not the guest, ended it.
pub const TIMEOUT_ERROR: &str = "Component exceeded its wall-clock timeout";
const REGISTRY_SCHEMA_VERSION: u32 = 1;
const KEYCHAIN_SERVICE_BASE: &str = "com.littlemonkey.extensions";

static STORE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static INVOCATION_GATE: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static EXTENSION_ENGINE: LazyLock<Result<Engine, String>> = LazyLock::new(build_engine);
static COMPONENT_CACHE: LazyLock<Mutex<HashMap<String, Component>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static COMPONENT_COMPILATION_GATE: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));
const MAX_CACHED_COMPONENTS: usize = 16;

#[derive(Clone)]
struct ActiveInvocation {
    store_root: PathBuf,
    extension_id: String,
    token: CancellationToken,
}

static CANCELLATIONS: LazyLock<Mutex<HashMap<String, ActiveInvocation>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn ensure_no_active_invocation(
    store_root: &Path,
    extension_id: &str,
    action: &str,
) -> Result<(), String> {
    let cancellations = CANCELLATIONS
        .lock()
        .map_err(|_| "Extension cancellation registry is poisoned".to_string())?;
    if cancellations
        .values()
        .any(|active| active.store_root == store_root && active.extension_id == extension_id)
    {
        Err(format!(
            "Cannot {action} while an extension invocation is active; stop it and retry"
        ))
    } else {
        Ok(())
    }
}

fn cancel_active_invocations(store_root: &Path, extension_id: &str) -> Result<usize, String> {
    let cancellations = CANCELLATIONS
        .lock()
        .map_err(|_| "Extension cancellation registry is poisoned".to_string())?;
    let mut cancelled = 0usize;
    for active in cancellations.values() {
        if active.store_root == store_root && active.extension_id == extension_id {
            active.token.cancel();
            cancelled = cancelled.saturating_add(1);
        }
    }
    Ok(cancelled)
}

fn prune_expired_invocations(record: &mut InstalledRecord) {
    let now = now_ms();
    record
        .active_invocations
        .retain(|_, invocation| invocation.deadline_at_ms >= now);
}

fn has_live_invocations(record: &InstalledRecord) -> bool {
    let now = now_ms();
    record
        .active_invocations
        .values()
        .any(|invocation| invocation.deadline_at_ms >= now)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    Tool,
    Channel,
    ModelProvider,
    EmbeddingProvider,
    Stt,
    Tts,
    RealtimeVoice,
    WebSearch,
    WebFetch,
    DeviceProvider,
    Connector,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CapabilityDeclaration {
    pub capability_id: String,
    pub kind: CapabilityKind,
    pub display_name: String,
    pub description: String,
    #[serde(default = "default_input_schema")]
    pub input_schema: serde_json::Value,
}

fn default_input_schema() -> serde_json::Value {
    serde_json::json!({ "type": "object", "additionalProperties": true })
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    NetworkOrigin,
    WorkspaceRead,
    WorkspaceWrite,
    ArtifactRead,
    ArtifactWrite,
    ModelInvoke,
    SecretUse,
    Device,
    WebhookReceive,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum PermissionRisk {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct PermissionDeclaration {
    pub permission_id: String,
    pub kind: PermissionKind,
    pub scope: String,
    pub reason: String,
}

impl PermissionDeclaration {
    #[must_use]
    pub fn risk(&self) -> PermissionRisk {
        match self.kind {
            PermissionKind::WorkspaceWrite | PermissionKind::Device => PermissionRisk::Critical,
            PermissionKind::NetworkOrigin
            | PermissionKind::SecretUse
            | PermissionKind::WebhookReceive => PermissionRisk::High,
            PermissionKind::WorkspaceRead
            | PermissionKind::ArtifactRead
            | PermissionKind::ArtifactWrite
            | PermissionKind::ModelInvoke => PermissionRisk::Medium,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct PermissionGrant {
    pub permission_id: String,
    /// Host-only binding. Workspace grants carry a canonical path here; the
    /// guest sees only the manifest's opaque handle in `scope`.
    pub binding: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFieldKind {
    String,
    Integer,
    Boolean,
    Select,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigField {
    pub key: String,
    pub label: String,
    pub description: String,
    pub kind: ConfigFieldKind,
    pub required: bool,
    pub default: Option<serde_json::Value>,
    pub options: Vec<String>,
    pub minimum: Option<i64>,
    pub maximum: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecretSlot {
    pub slot_id: String,
    pub label: String,
    pub description: String,
    /// The host applies the secret; the guest never receives its bytes.
    pub auth_header: Option<String>,
    pub auth_scheme: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExtensionDependency {
    pub extension_id: String,
    pub constraint: VersionConstraint,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ComponentReference {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExtensionManifest {
    pub schema_version: u32,
    pub extension_id: String,
    pub version: SemanticVersion,
    pub display_name: String,
    pub description: String,
    pub host_api: VersionConstraint,
    pub component: ComponentReference,
    pub capabilities: Vec<CapabilityDeclaration>,
    pub permissions: Vec<PermissionDeclaration>,
    pub config_schema: Vec<ConfigField>,
    pub secret_slots: Vec<SecretSlot>,
    pub dependencies: Vec<ExtensionDependency>,
    pub compatibility: Compatibility,
    pub publisher: String,
    pub provenance: PackageProvenance,
    pub signature: Option<PackageSignature>,
    pub checksums: BTreeMap<String, String>,
}

impl ExtensionManifest {
    pub fn signing_payload(&self) -> Result<Vec<u8>, String> {
        let mut unsigned = self.clone();
        unsigned.signature = None;
        serde_json::to_vec(&unsigned).map_err(|error| format!("Cannot encode manifest: {error}"))
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != EXTENSION_SCHEMA_VERSION {
            return Err(format!(
                "Unsupported extension manifest schema {}",
                self.schema_version
            ));
        }
        validate_id("extension id", &self.extension_id)?;
        validate_text("display name", &self.display_name, 160)?;
        validate_text("description", &self.description, 8 * 1024)?;
        validate_text("publisher", &self.publisher, 256)?;
        if self.publisher != self.provenance.publisher {
            return Err("Manifest publisher and provenance publisher differ".to_string());
        }
        validate_version_constraint("host API", &self.host_api)?;
        validate_compatibility(&self.compatibility)?;
        validate_provenance(&self.provenance)?;
        if self.capabilities.is_empty() || self.capabilities.len() > 128 {
            return Err("An extension must declare 1-128 capabilities".to_string());
        }
        if self.permissions.len() > 256
            || self.config_schema.len() > 128
            || self.secret_slots.len() > 64
            || self.dependencies.len() > 64
            || self.checksums.len() > 256
        {
            return Err("Extension manifest exceeds a declaration limit".to_string());
        }
        validate_relative_path(&self.component.path)?;
        if !self.component.path.ends_with(".wasm") {
            return Err("The extension component must use a .wasm path".to_string());
        }
        validate_sha256(&self.component.sha256, "component sha256")?;
        if self.checksums.get(&self.component.path) != Some(&self.component.sha256) {
            return Err("checksums must contain the exact component digest".to_string());
        }
        for (path, digest) in &self.checksums {
            validate_relative_path(path)?;
            validate_sha256(digest, "file checksum")?;
        }
        let mut capability_ids = BTreeSet::new();
        for capability in &self.capabilities {
            validate_id("capability id", &capability.capability_id)?;
            validate_text("capability name", &capability.display_name, 160)?;
            validate_text("capability description", &capability.description, 4 * 1024)?;
            if !capability.input_schema.is_object() {
                return Err(format!(
                    "Capability {} input_schema must be a JSON object",
                    capability.capability_id
                ));
            }
            if serde_json::to_vec(&capability.input_schema)
                .map_err(|error| format!("Cannot encode capability input schema: {error}"))?
                .len()
                > 32 * 1024
            {
                return Err(format!(
                    "Capability {} input_schema exceeds 32 KiB",
                    capability.capability_id
                ));
            }
            if !capability_ids.insert(capability.capability_id.clone()) {
                return Err(format!(
                    "Duplicate capability id '{}'",
                    capability.capability_id
                ));
            }
        }
        let mut permission_ids = BTreeSet::new();
        for permission in &self.permissions {
            validate_id("permission id", &permission.permission_id)?;
            validate_text("permission reason", &permission.reason, 4 * 1024)?;
            validate_permission_scope(permission)?;
            if !permission_ids.insert(permission.permission_id.clone()) {
                return Err(format!(
                    "Duplicate permission id '{}'",
                    permission.permission_id
                ));
            }
        }
        let mut config_keys = BTreeSet::new();
        for field in &self.config_schema {
            validate_id("config key", &field.key)?;
            validate_text("config label", &field.label, 160)?;
            validate_text("config description", &field.description, 4 * 1024)?;
            if !config_keys.insert(field.key.clone()) {
                return Err(format!("Duplicate config key '{}'", field.key));
            }
            if field.options.len() > 128
                || (field.kind == ConfigFieldKind::Select && field.options.is_empty())
                || field
                    .minimum
                    .zip(field.maximum)
                    .is_some_and(|(min, max)| min > max)
            {
                return Err(format!("Invalid config field '{}'", field.key));
            }
            if let Some(default) = &field.default {
                validate_config_value(field, default)?;
            }
        }
        let mut slots = BTreeSet::new();
        for slot in &self.secret_slots {
            validate_id("secret slot", &slot.slot_id)?;
            validate_text("secret label", &slot.label, 160)?;
            validate_text("secret description", &slot.description, 4 * 1024)?;
            if !slots.insert(slot.slot_id.clone()) {
                return Err(format!("Duplicate secret slot '{}'", slot.slot_id));
            }
            if let Some(header) = &slot.auth_header {
                validate_header_name(header)?;
            }
            if slot.auth_header.is_none() != slot.auth_scheme.is_none() {
                return Err(format!(
                    "Secret slot '{}' must declare auth_header and auth_scheme together",
                    slot.slot_id
                ));
            }
            if slot.auth_scheme.as_ref().is_some_and(|scheme| {
                scheme.len() > 32
                    || !scheme.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    })
            }) {
                return Err(format!(
                    "Secret slot '{}' has an invalid auth scheme",
                    slot.slot_id
                ));
            }
        }
        for permission in &self.permissions {
            if permission.kind == PermissionKind::SecretUse && !slots.contains(&permission.scope) {
                return Err(format!(
                    "Secret permission '{}' names an undeclared slot",
                    permission.permission_id
                ));
            }
        }
        let mut dependencies = BTreeSet::new();
        for dependency in &self.dependencies {
            validate_id("dependency extension id", &dependency.extension_id)?;
            validate_version_constraint("dependency", &dependency.constraint)?;
            if dependency.extension_id == self.extension_id
                || !dependencies.insert(dependency.extension_id.clone())
            {
                return Err("Extension dependencies must be unique and non-cyclic at self".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrustState {
    Verified,
    Unsigned,
    Untrusted,
    Invalid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TrustEvidence {
    pub state: TrustState,
    pub reason: String,
    pub trust_root_id: Option<String>,
    pub key_id: Option<String>,
    pub manifest_sha256: String,
    pub component_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    NotValidated,
    Stopped,
    Healthy,
    Degraded,
    Unhealthy,
    ProtectiveDisabled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeHealth {
    pub state: HealthState,
    pub validated: bool,
    pub enabled: bool,
    pub running: bool,
    pub consecutive_failures: u32,
    pub trap_count: u64,
    pub undeclared_attempts: u64,
    pub last_error: Option<String>,
    pub last_invocation_at_ms: Option<u64>,
}

impl Default for RuntimeHealth {
    fn default() -> Self {
        Self {
            state: HealthState::NotValidated,
            validated: false,
            enabled: false,
            running: false,
            consecutive_failures: 0,
            trap_count: 0,
            undeclared_attempts: 0,
            last_error: None,
            last_invocation_at_ms: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExtensionLogRow {
    pub at_ms: u64,
    pub level: String,
    pub message: String,
    pub invocation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct InstalledVersion {
    manifest: ExtensionManifest,
    trust: TrustEvidence,
    grants: Vec<PermissionGrant>,
    observed_source: InstallSource,
    installed_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct InstalledRecord {
    extension_id: String,
    active_version: String,
    previous_version: Option<String>,
    versions: BTreeMap<String, InstalledVersion>,
    config: BTreeMap<String, serde_json::Value>,
    configured_secret_slots: BTreeSet<String>,
    health: RuntimeHealth,
    logs: Vec<ExtensionLogRow>,
    private_state: BTreeMap<String, Vec<u8>>,
    last_tool_result: Option<String>,
    last_events: Vec<(String, String)>,
    #[serde(default)]
    completed_invocations: BTreeMap<String, StoredInvocation>,
    #[serde(default)]
    active_invocations: BTreeMap<String, ActiveInvocationLease>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct RegistryState {
    schema_version: u32,
    #[serde(default)]
    revision: u64,
    records: BTreeMap<String, InstalledRecord>,
}

impl Default for RegistryState {
    fn default() -> Self {
        Self {
            schema_version: REGISTRY_SCHEMA_VERSION,
            revision: 0,
            records: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PermissionView {
    pub permission_id: String,
    pub kind: PermissionKind,
    pub scope: String,
    pub reason: String,
    pub risk: PermissionRisk,
    pub granted: bool,
    pub binding_label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PermissionDiff {
    pub added: Vec<PermissionView>,
    pub removed: Vec<PermissionView>,
    pub unchanged: Vec<PermissionView>,
    pub expands_authority: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecretSlotStatus {
    pub slot_id: String,
    pub label: String,
    pub description: String,
    pub configured: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPreview {
    pub source_path: String,
    pub source_digest: String,
    pub manifest: ExtensionManifest,
    pub trust: TrustEvidence,
    pub compatible: bool,
    pub compatibility_reason: Option<String>,
    pub permissions: Vec<PermissionView>,
    pub permission_diff: Option<PermissionDiff>,
    pub approval_digest: String,
    pub requires_unsigned_approval: bool,
    pub requires_untrusted_approval: bool,
    pub requires_high_risk_approval: bool,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExtensionDetail {
    pub manifest: ExtensionManifest,
    pub trust: TrustEvidence,
    pub installed_source: InstallSource,
    pub compatible: bool,
    pub compatibility_reason: Option<String>,
    pub permissions: Vec<PermissionView>,
    pub secret_slots: Vec<SecretSlotStatus>,
    pub config: BTreeMap<String, serde_json::Value>,
    pub health: RuntimeHealth,
    pub active_version: String,
    pub previous_version: Option<String>,
    pub available_versions: Vec<String>,
    pub update_available: bool,
    pub allowed_actions: Vec<String>,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExtensionSecuritySnapshot {
    pub extension_id: String,
    pub version: String,
    pub trust: TrustState,
    pub trust_reason: String,
    pub compatible: bool,
    pub compatibility_reason: Option<String>,
    pub permissions: Vec<PermissionView>,
    pub configured_secret_slots: usize,
    pub health: RuntimeHealth,
    /// Whether the active version's component file is still present on disk
    /// and still hashes to what the manifest promised.
    ///
    /// A registry row is not evidence that the code it names exists: the file
    /// can be deleted, truncated or replaced by anything with write access to
    /// the store. Nothing will *run* in that state — every invocation
    /// re-verifies the digest — but an operator deserves to be told, because a
    /// provider that silently stopped answering looks like a bug rather than a
    /// tampered installation.
    pub component_intact: bool,
    /// Every capability this version declares, so a consumer of the snapshot
    /// can tell whether a persisted provider selection still has an owner.
    pub capabilities: Vec<(CapabilityKind, String)>,
}

/// One unambiguous, runnable capability contribution. Subsystems resolve by
/// `(kind, capability_id)` and receive the immutable owner/version rather than
/// accepting an extension-selected provider identity at dispatch time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ActiveCapability {
    pub kind: CapabilityKind,
    pub capability_id: String,
    pub extension_id: String,
    pub version: String,
    pub display_name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Read-only Security Doctor projection. It does not construct an engine or
/// create a store when executable extensions have never been used.
pub fn extension_security_snapshots(
    app_data: impl AsRef<Path>,
) -> Result<Vec<ExtensionSecuritySnapshot>, String> {
    let app_data = app_data.as_ref();
    let root = app_data.join(EXTENSION_STORE_DIRECTORY);
    if !root.exists() {
        return Ok(Vec::new());
    }
    let metadata = fs::symlink_metadata(&root)
        .map_err(|error| format!("Cannot inspect extension store: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("Extension store must be a real directory".to_string());
    }
    let registry = root.join("registry.json");
    if !registry.exists() {
        return Ok(Vec::new());
    }
    let bytes = read_regular_file(&registry, 8 * 1024 * 1024)?;
    let state: RegistryState = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Invalid extension registry: {error}"))?;
    if state.schema_version != REGISTRY_SCHEMA_VERSION {
        return Err("Unsupported extension registry schema".to_string());
    }
    validate_registry(&state)?;
    let trust_store = load_extension_trust_store(app_data)?;
    state
        .records
        .values()
        .map(|record| {
            let active = active_version(record)?;
            let (trust, _) = current_trust_status(active, &trust_store)?;
            let (compatible, compatibility_reason) = compatibility(&active.manifest);
            Ok(ExtensionSecuritySnapshot {
                extension_id: record.extension_id.clone(),
                version: record.active_version.clone(),
                trust: trust.state,
                trust_reason: trust.reason,
                compatible,
                compatibility_reason,
                permissions: permission_views(&active.manifest, &active.grants),
                configured_secret_slots: record.configured_secret_slots.len(),
                health: record.health.clone(),
                component_intact: component_intact(&root, &active.manifest),
                capabilities: active
                    .manifest
                    .capabilities
                    .iter()
                    .map(|capability| (capability.kind, capability.capability_id.clone()))
                    .collect(),
            })
        })
        .collect()
}

/// Whether the installed component for `manifest` is present and unmodified.
///
/// Read-only and best-effort by construction: it is a Security Doctor
/// projection, so it answers "no" for anything it cannot confirm rather than
/// failing the whole audit over one unreadable file.
fn component_intact(store_root: &Path, manifest: &ExtensionManifest) -> bool {
    let directory = store_root
        .join("versions")
        .join(&manifest.extension_id)
        .join(manifest.version.to_string());
    let Ok(path) = safe_join(&directory, &manifest.component.path) else {
        return false;
    };
    read_regular_file(&path, MAX_COMPONENT_BYTES)
        .is_ok_and(|bytes| sha256_bytes(&bytes) == manifest.component.sha256.to_ascii_lowercase())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    pub approval_digest: String,
    pub grants: Vec<PermissionGrant>,
    pub allow_unsigned: bool,
    pub allow_untrusted: bool,
    pub allow_high_risk: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InvocationRequest {
    pub extension_id: String,
    pub capability_id: String,
    pub input_json: String,
    pub invocation_id: Option<String>,
    #[serde(default)]
    pub input_artifact_ids: Vec<String>,
    /// Optional immutable binding used by native/tool registries. Supplying
    /// only one half is invalid; generic user-driven invocation omits both.
    #[serde(default)]
    pub expected_kind: Option<CapabilityKind>,
    #[serde(default)]
    pub expected_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InvocationResult {
    pub invocation_id: String,
    pub output_json: String,
    pub duration_ms: u64,
    pub fuel_consumed: u64,
    pub emitted_events: Vec<(String, String)>,
    pub tool_result: Option<String>,
    /// Artifacts this invocation wrote, as the host recorded them. Defaulted
    /// so a result remembered by an earlier build still deserializes — an
    /// empty set is the honest answer for a record written before the host
    /// kept the tally, and it fails closed at every consumer.
    #[serde(default)]
    pub written_artifact_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct StoredInvocation {
    request_sha256: String,
    version: String,
    completed_at_ms: u64,
    result: Option<InvocationResult>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ActiveInvocationLease {
    request_sha256: String,
    version: String,
    deadline_at_ms: u64,
}

#[derive(Debug, Clone)]
struct LoadedBundle {
    source: PathBuf,
    manifest: ExtensionManifest,
    component: Vec<u8>,
    additional_files: BTreeMap<String, Vec<u8>>,
    trust: TrustEvidence,
    source_digest: String,
}

#[derive(Clone)]
pub struct ExtensionManager {
    app_data: PathBuf,
    root: PathBuf,
    artifact_root: PathBuf,
    engine: Engine,
    model_hub: Option<Arc<crate::m3_runtime_hub::M3RuntimeHub>>,
    /// The per-invocation fuel ceiling. Always [`DEFAULT_FUEL`] in production —
    /// only [`ExtensionManager::with_fuel`], which exists for tests, moves it.
    fuel: u64,
}

impl ExtensionManager {
    pub fn new(app_data: impl AsRef<Path>) -> Result<Self, String> {
        ensure_real_directory(app_data.as_ref())?;
        let app_data = app_data
            .as_ref()
            .canonicalize()
            .map_err(|error| format!("Cannot resolve app-data directory: {error}"))?;
        let root = app_data.join(EXTENSION_STORE_DIRECTORY);
        ensure_private_directory(&root)?;
        ensure_private_directory(&root.join("versions"))?;
        ensure_private_directory(&root.join("locks"))?;
        ensure_private_directory(&root.join("cancellations"))?;
        Ok(Self {
            artifact_root: app_data.join("content-v1"),
            app_data,
            root,
            engine: match &*EXTENSION_ENGINE {
                Ok(engine) => engine.clone(),
                Err(error) => return Err(error.clone()),
            },
            model_hub: None,
            fuel: DEFAULT_FUEL,
        })
    }

    /// Run guests with a fuel ceiling other than [`DEFAULT_FUEL`].
    ///
    /// Test-only, because a fuel budget is a security control: production has
    /// exactly one, and it is the constant above.
    ///
    /// What it buys a test is a run whose *end* is unambiguous. A guest that
    /// loops is racing its fuel ceiling from its first instruction, so a test
    /// that wants to stop such a guest from outside — and then claim that is
    /// why it stopped — is racing that ceiling too. Raising the budget far past
    /// what the window needs removes the race in one direction; lowering it far
    /// below removes it in the other, for a test that wants the ceiling itself.
    /// Neither changes what production runs, and neither disables the wall
    /// clock, which still ends any run this budget cannot.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_fuel(mut self, fuel: u64) -> Self {
        self.fuel = fuel;
        self
    }

    #[must_use]
    pub fn with_model_hub(mut self, hub: Arc<crate::m3_runtime_hub::M3RuntimeHub>) -> Self {
        self.model_hub = Some(hub);
        self
    }

    pub fn with_artifact_root(mut self, artifact_root: impl AsRef<Path>) -> Result<Self, String> {
        ensure_private_directory(artifact_root.as_ref())?;
        self.artifact_root = artifact_root.as_ref().to_path_buf();
        Ok(self)
    }

    fn acquire_registry_lock(&self) -> Result<crate::process_lock::CrossProcessFileLock, String> {
        crate::process_lock::acquire_cross_process_lock(&self.root.join("registry.lock"))
    }

    fn acquire_extension_lock(
        &self,
        extension_id: &str,
    ) -> Result<crate::process_lock::CrossProcessFileLock, String> {
        validate_id("extension id", extension_id)?;
        crate::process_lock::acquire_cross_process_lock(
            &self.root.join("locks").join(format!("{extension_id}.lock")),
        )
    }

    async fn acquire_extension_lock_async(
        &self,
        extension_id: &str,
    ) -> Result<crate::process_lock::CrossProcessFileLock, String> {
        validate_id("extension id", extension_id)?;
        let path = self.root.join("locks").join(format!("{extension_id}.lock"));
        tokio::task::spawn_blocking(move || crate::process_lock::acquire_cross_process_lock(&path))
            .await
            .map_err(|error| format!("Extension lock task failed: {error}"))?
    }

    fn invocation_cancel_path(&self, invocation_id: &str) -> Result<PathBuf, String> {
        validate_id("invocation id", invocation_id)?;
        Ok(self
            .root
            .join("cancellations")
            .join(format!("invocation-{invocation_id}.cancel")))
    }

    fn extension_cancel_path(&self, extension_id: &str) -> Result<PathBuf, String> {
        validate_id("extension id", extension_id)?;
        Ok(self
            .root
            .join("cancellations")
            .join(format!("extension-{extension_id}.cancel")))
    }

    fn request_extension_cancellation(&self, extension_id: &str) -> Result<(), String> {
        cancel_active_invocations(&self.root, extension_id)?;
        atomic_write(&self.extension_cancel_path(extension_id)?, b"cancel\n")
    }

    fn ensure_no_live_invocations(&self, extension_id: &str, action: &str) -> Result<(), String> {
        let _registry_lock = self.acquire_registry_lock()?;
        let state = self.load_registry()?;
        if state
            .records
            .get(extension_id)
            .is_some_and(has_live_invocations)
        {
            Err(format!(
                "Cannot {action} while an extension invocation is active; stop it and retry"
            ))
        } else {
            Ok(())
        }
    }

    async fn wait_for_no_live_invocations(&self, extension_id: &str) -> Result<(), String> {
        let deadline = tokio::time::Instant::now()
            + Duration::from_millis(DEFAULT_TIMEOUT_MS.saturating_add(2_000));
        loop {
            let state = self.load_registry()?;
            if !state
                .records
                .get(extension_id)
                .is_some_and(has_live_invocations)
            {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("Timed out waiting for extension invocations to stop".to_string());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub fn cancel_invocation(&self, invocation_id: &str) -> Result<bool, String> {
        validate_id("invocation id", invocation_id)?;
        let mut found = false;
        {
            let cancellations = CANCELLATIONS
                .lock()
                .map_err(|_| "Extension cancellation registry is poisoned".to_string())?;
            if let Some(active) = cancellations.get(invocation_id) {
                if active.store_root == self.root {
                    active.token.cancel();
                    found = true;
                }
            }
        }
        let _registry_lock = self.acquire_registry_lock()?;
        let state = self.load_registry()?;
        found |= state.records.values().any(|record| {
            record
                .active_invocations
                .get(invocation_id)
                .is_some_and(|invocation| invocation.deadline_at_ms >= now_ms())
        });
        if found {
            atomic_write(&self.invocation_cancel_path(invocation_id)?, b"cancel\n")?;
        }
        Ok(found)
    }

    pub fn discover(&self, source: impl AsRef<Path>) -> Result<ExtensionPreview, String> {
        let state = self.load_registry()?;
        self.preview_bundle(source.as_ref(), &state, false)
    }

    pub fn list(&self) -> Result<Vec<ExtensionDetail>, String> {
        let state = self.load_registry()?;
        state
            .records
            .values()
            .map(|record| self.detail(record, &state))
            .collect()
    }

    pub fn inspect(&self, extension_id: &str) -> Result<ExtensionDetail, String> {
        validate_id("extension id", extension_id)?;
        let state = self.load_registry()?;
        let record = state
            .records
            .get(extension_id)
            .ok_or_else(|| format!("Extension '{extension_id}' is not installed"))?;
        self.detail(record, &state)
    }

    pub fn active_capabilities(
        &self,
        kind: Option<CapabilityKind>,
    ) -> Result<Vec<ActiveCapability>, String> {
        let state = self.load_registry()?;
        let trust_store = self.load_trust_store()?;
        let mut capabilities = Vec::new();
        for record in state.records.values() {
            if !record.health.enabled
                || !record.health.running
                || !record.health.validated
                || record.health.state != HealthState::Healthy
            {
                continue;
            }
            let active = active_version(record)?;
            if validate_dependencies(&active.manifest, &state).is_err()
                || current_trust_status(active, &trust_store)?.1.is_some()
            {
                continue;
            }
            for capability in &active.manifest.capabilities {
                if kind.is_some_and(|expected| expected != capability.kind) {
                    continue;
                }
                capabilities.push(ActiveCapability {
                    kind: capability.kind,
                    capability_id: capability.capability_id.clone(),
                    extension_id: record.extension_id.clone(),
                    version: record.active_version.clone(),
                    display_name: capability.display_name.clone(),
                    description: capability.description.clone(),
                    input_schema: capability.input_schema.clone(),
                });
            }
        }
        capabilities.sort_by(|left, right| {
            (
                left.kind,
                left.capability_id.as_str(),
                left.extension_id.as_str(),
            )
                .cmp(&(
                    right.kind,
                    right.capability_id.as_str(),
                    right.extension_id.as_str(),
                ))
        });
        Ok(capabilities)
    }

    pub fn resolve_active_capability(
        &self,
        kind: CapabilityKind,
        capability_id: &str,
    ) -> Result<ActiveCapability, String> {
        validate_id("capability id", capability_id)?;
        self.active_capabilities(Some(kind))?
            .into_iter()
            .find(|capability| capability.capability_id == capability_id)
            .ok_or_else(|| {
                format!("No healthy active extension owns capability '{capability_id}:{kind:?}'")
            })
    }

    pub async fn invoke_active_capability(
        &self,
        kind: CapabilityKind,
        capability_id: &str,
        input_json: String,
        invocation_id: Option<String>,
        input_artifact_ids: Vec<String>,
    ) -> Result<InvocationResult, String> {
        let owner = self.resolve_active_capability(kind, capability_id)?;
        self.invoke_owned_active_capability(
            kind,
            &owner.extension_id,
            capability_id,
            input_json,
            invocation_id,
            input_artifact_ids,
        )
        .await
    }

    /// Invoke a native-provider selection bound to its persisted owner. This
    /// prevents a later extension from silently inheriting an uninstalled
    /// provider's capability id.
    pub async fn invoke_owned_active_capability(
        &self,
        kind: CapabilityKind,
        expected_extension_id: &str,
        capability_id: &str,
        input_json: String,
        invocation_id: Option<String>,
        input_artifact_ids: Vec<String>,
    ) -> Result<InvocationResult, String> {
        validate_id("extension id", expected_extension_id)?;
        let owner = self.resolve_active_capability(kind, capability_id)?;
        if owner.extension_id != expected_extension_id {
            return Err(format!(
                "Capability owner changed from '{expected_extension_id}' to '{}'; select the provider again",
                owner.extension_id
            ));
        }
        self.invoke(InvocationRequest {
            extension_id: owner.extension_id,
            capability_id: owner.capability_id,
            input_json,
            invocation_id,
            input_artifact_ids,
            expected_kind: Some(owner.kind),
            expected_version: Some(owner.version),
        })
        .await
    }

    pub fn preview_update(&self, source: impl AsRef<Path>) -> Result<ExtensionPreview, String> {
        let state = self.load_registry()?;
        self.preview_bundle(source.as_ref(), &state, true)
    }

    fn preview_bundle(
        &self,
        source: &Path,
        state: &RegistryState,
        updating: bool,
    ) -> Result<ExtensionPreview, String> {
        let bundle = self.load_bundle(source)?;
        let (compatible, compatibility_reason) = compatibility(&bundle.manifest);
        let existing = state.records.get(&bundle.manifest.extension_id);
        if updating && existing.is_none() {
            return Err("Update source does not match an installed extension".to_string());
        }
        if !updating && existing.is_some() {
            return Err("Extension is already installed; use update".to_string());
        }
        if let Some(record) = existing {
            let active = active_version(record)?;
            if bundle.manifest.version <= active.manifest.version {
                return Err("An update must have a newer semantic version".to_string());
            }
        }
        let current_grants = existing
            .and_then(|record| active_version(record).ok())
            .map(|version| version.grants.as_slice())
            .unwrap_or_default();
        let permissions = permission_views(&bundle.manifest, current_grants);
        let permission_diff = existing
            .and_then(|record| active_version(record).ok())
            .map(|version| permission_diff(&version.manifest, &bundle.manifest, &version.grants));
        let mut blockers = Vec::new();
        if !compatible {
            blockers.push(
                compatibility_reason
                    .clone()
                    .unwrap_or_else(|| "Extension is incompatible".into()),
            );
        }
        if bundle.trust.state == TrustState::Invalid {
            blockers.push(bundle.trust.reason.clone());
        }
        if let Err(error) = validate_dependencies(&bundle.manifest, state) {
            blockers.push(error);
        }
        if let Err(error) = validate_capability_collisions(&bundle.manifest, state) {
            blockers.push(error);
        }
        let expands = permission_diff
            .as_ref()
            .is_some_and(|diff| diff.expands_authority);
        let requires_high_risk_approval = permissions.iter().any(|permission| {
            matches!(
                permission.risk,
                PermissionRisk::High | PermissionRisk::Critical
            ) && (!updating || expands)
        });
        let mut digest_value = serde_json::json!({
            "schema": 1,
            "operation": if updating { "update" } else { "install" },
            "extension_id": bundle.manifest.extension_id,
            "version": bundle.manifest.version,
            "source_digest": bundle.source_digest,
            "trust": bundle.trust.state,
            "permissions": permissions,
            "permission_diff": permission_diff,
        });
        canonicalize_json(&mut digest_value);
        let approval_digest =
            sha256_bytes(&serde_json::to_vec(&digest_value).map_err(|error| error.to_string())?);
        Ok(ExtensionPreview {
            source_path: bundle.source.to_string_lossy().to_string(),
            source_digest: bundle.source_digest,
            manifest: bundle.manifest,
            trust: bundle.trust.clone(),
            compatible,
            compatibility_reason,
            permissions,
            permission_diff,
            approval_digest,
            requires_unsigned_approval: bundle.trust.state == TrustState::Unsigned,
            requires_untrusted_approval: bundle.trust.state == TrustState::Untrusted,
            requires_high_risk_approval,
            blockers,
        })
    }

    fn load_bundle(&self, source: &Path) -> Result<LoadedBundle, String> {
        let source = source
            .canonicalize()
            .map_err(|error| format!("Cannot resolve extension source: {error}"))?;
        let source_meta = fs::symlink_metadata(&source)
            .map_err(|error| format!("Cannot inspect extension source: {error}"))?;
        if !source_meta.is_dir() || source_meta.file_type().is_symlink() {
            return Err("Extension source must be a real directory".to_string());
        }
        let manifest_path = source.join(EXTENSION_MANIFEST_FILE);
        let manifest_bytes = read_regular_file(&manifest_path, MAX_MANIFEST_BYTES)?;
        let manifest: ExtensionManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|error| format!("Invalid extension manifest: {error}"))?;
        manifest.validate()?;
        let component_path = safe_join(&source, &manifest.component.path)?;
        let component = read_regular_file(&component_path, MAX_COMPONENT_BYTES)?;
        let observed_component = sha256_bytes(&component);
        if observed_component != manifest.component.sha256.to_ascii_lowercase() {
            return Err("Extension component checksum does not match the manifest".to_string());
        }
        let mut total = manifest_bytes.len();
        let mut additional_files = BTreeMap::new();
        for (relative, expected) in &manifest.checksums {
            if relative == &manifest.component.path {
                total = total.saturating_add(component.len());
            } else {
                let path = safe_join(&source, relative)?;
                let bytes = read_regular_file(&path, MAX_COMPONENT_BYTES)?;
                total = total.saturating_add(bytes.len());
                if sha256_bytes(&bytes) != expected.to_ascii_lowercase() {
                    return Err(format!("Checksum mismatch for '{relative}'"));
                }
                additional_files.insert(relative.clone(), bytes);
            }
            if total > 64 * 1024 * 1024 {
                return Err("Extension bundle exceeds 64 MiB".to_string());
            }
        }
        self.engine
            .precompile_component(&component)
            .map_err(|error| format!("Component validation failed: {error}"))?;
        let trust = verify_trust(&manifest, &self.load_trust_store()?)?;
        let mut source_hasher = Sha256::new();
        source_hasher.update(&manifest_bytes);
        source_hasher.update(&component);
        Ok(LoadedBundle {
            source,
            manifest,
            component,
            additional_files,
            trust,
            source_digest: format!("{:x}", source_hasher.finalize()),
        })
    }

    fn load_trust_store(&self) -> Result<TrustStore, String> {
        load_extension_trust_store(&self.app_data)
    }

    fn load_registry(&self) -> Result<RegistryState, String> {
        let path = self.root.join("registry.json");
        if !path.exists() {
            return Ok(RegistryState::default());
        }
        let bytes = read_regular_file(&path, 8 * 1024 * 1024)?;
        let state: RegistryState = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Invalid extension registry: {error}"))?;
        if state.schema_version != REGISTRY_SCHEMA_VERSION {
            return Err("Unsupported extension registry schema".to_string());
        }
        validate_registry(&state)?;
        Ok(state)
    }

    fn save_registry(&self, state: &RegistryState) -> Result<(), String> {
        validate_registry(state)?;
        let _cross_process = self.acquire_registry_lock()?;
        let observed = self.load_registry()?;
        if observed.revision != state.revision {
            return Err(
                "Executable extension state changed concurrently; refresh and retry".to_string(),
            );
        }
        self.write_registry_locked(state)
    }

    /// Caller must hold `registry.lock`. This is used for cross-process
    /// read-modify-write transactions whose result cannot safely be retried by
    /// a caller (invocation leases/completions and secret metadata).
    fn write_registry_locked(&self, state: &RegistryState) -> Result<(), String> {
        validate_registry(state)?;
        let next_revision = state
            .revision
            .checked_add(1)
            .ok_or_else(|| "Extension registry revision overflow".to_string())?;
        let mut next = state.clone();
        next.revision = next_revision;
        let bytes = serde_json::to_vec_pretty(&next)
            .map_err(|error| format!("Cannot encode extension registry: {error}"))?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err("Extension registry exceeds its durable-state limit".to_string());
        }
        atomic_write(&self.root.join("registry.json"), &bytes)
    }

    fn detail(
        &self,
        record: &InstalledRecord,
        state: &RegistryState,
    ) -> Result<ExtensionDetail, String> {
        let active = active_version(record)?;
        let (trust, trust_blocker) = current_trust_status(active, &self.load_trust_store()?)?;
        let (compatible, compatibility_reason) = compatibility(&active.manifest);
        let blockers = detail_blockers(
            record,
            active,
            state,
            &trust,
            trust_blocker.as_deref(),
            compatible,
            compatibility_reason.as_deref(),
        );
        let mut actions = vec!["validate".to_string(), "uninstall".to_string()];
        if record.health.enabled {
            actions.push("disable".into());
        } else if blockers.is_empty() {
            actions.push("enable".into());
        }
        if record.health.running {
            actions.push("stop".into());
        } else if record.health.enabled && record.health.validated {
            actions.push("start".into());
        }
        if record.previous_version.is_some() {
            actions.push("rollback".into());
        }
        actions.push("update".into());
        let mut available_versions = record.versions.keys().cloned().collect::<Vec<_>>();
        available_versions.sort();
        Ok(ExtensionDetail {
            manifest: active.manifest.clone(),
            trust,
            installed_source: active.observed_source.clone(),
            compatible,
            compatibility_reason,
            permissions: permission_views(&active.manifest, &active.grants),
            secret_slots: active
                .manifest
                .secret_slots
                .iter()
                .map(|slot| SecretSlotStatus {
                    slot_id: slot.slot_id.clone(),
                    label: slot.label.clone(),
                    description: slot.description.clone(),
                    configured: record.configured_secret_slots.contains(&slot.slot_id),
                })
                .collect(),
            config: record.config.clone(),
            health: record.health.clone(),
            active_version: record.active_version.clone(),
            previous_version: record.previous_version.clone(),
            available_versions,
            update_available: false,
            allowed_actions: actions,
            blockers,
        })
    }
}

impl ExtensionManager {
    pub async fn install(
        &self,
        source: impl AsRef<Path>,
        approval: Approval,
    ) -> Result<ExtensionDetail, String> {
        let preview = self.discover(source.as_ref())?;
        validate_approval(&preview, &approval)?;
        let bundle = self.load_bundle(source.as_ref())?;
        if bundle.source_digest != preview.source_digest {
            return Err("Extension source changed after approval; preview it again".to_string());
        }
        let grants = validate_grants(&bundle.manifest, &approval.grants)?;
        let config = resolved_config(&bundle.manifest, &BTreeMap::new());
        self.instantiate_only(&bundle.manifest, &bundle.component, &grants, &config)
            .await?;
        let _extension_lock = self
            .acquire_extension_lock_async(&bundle.manifest.extension_id)
            .await?;
        let _invocation_gate = INVOCATION_GATE
            .lock()
            .map_err(|_| "Extension invocation gate is poisoned".to_string())?;
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Extension store lock is poisoned".to_string())?;
        let mut state = self.load_registry()?;
        if state.records.contains_key(&bundle.manifest.extension_id) {
            return Err("Extension was installed by another operation; refresh and retry".into());
        }
        validate_dependencies(&bundle.manifest, &state)?;
        validate_capability_collisions(&bundle.manifest, &state)?;
        self.persist_bundle(&bundle)?;
        let version = bundle.manifest.version.to_string();
        let mut versions = BTreeMap::new();
        versions.insert(
            version.clone(),
            InstalledVersion {
                manifest: bundle.manifest.clone(),
                trust: bundle.trust,
                grants,
                observed_source: InstallSource::LocalFolder {
                    canonical_path: bundle.source.to_string_lossy().to_string(),
                },
                installed_at_ms: now_ms(),
            },
        );
        let extension_id = bundle.manifest.extension_id.clone();
        state.records.insert(
            extension_id.clone(),
            InstalledRecord {
                extension_id: extension_id.clone(),
                active_version: version,
                previous_version: None,
                versions,
                config,
                configured_secret_slots: BTreeSet::new(),
                health: RuntimeHealth {
                    validated: true,
                    state: HealthState::Stopped,
                    ..RuntimeHealth::default()
                },
                logs: vec![ExtensionLogRow {
                    at_ms: now_ms(),
                    level: "info".into(),
                    message: "Component verified, compiled, and installed disabled".into(),
                    invocation_id: None,
                }],
                private_state: BTreeMap::new(),
                last_tool_result: None,
                last_events: Vec::new(),
                completed_invocations: BTreeMap::new(),
                active_invocations: BTreeMap::new(),
            },
        );
        self.save_registry(&state)?;
        self.detail(
            state
                .records
                .get(&extension_id)
                .expect("record just inserted"),
            &state,
        )
    }

    pub async fn update(
        &self,
        source: impl AsRef<Path>,
        approval: Approval,
    ) -> Result<ExtensionDetail, String> {
        let preview = self.preview_update(source.as_ref())?;
        if self.inspect(&preview.manifest.extension_id)?.health.running {
            return Err("Stop the extension before updating it".to_string());
        }
        validate_approval(&preview, &approval)?;
        let bundle = self.load_bundle(source.as_ref())?;
        if bundle.source_digest != preview.source_digest {
            return Err("Extension source changed after approval; preview it again".to_string());
        }
        let grants = validate_grants(&bundle.manifest, &approval.grants)?;
        let config = {
            let state = self.load_registry()?;
            let record = state
                .records
                .get(&bundle.manifest.extension_id)
                .ok_or_else(|| "Extension was removed before update validation".to_string())?;
            resolved_config(&bundle.manifest, &record.config)
        };
        self.instantiate_only(&bundle.manifest, &bundle.component, &grants, &config)
            .await?;
        let _extension_lock = self
            .acquire_extension_lock_async(&bundle.manifest.extension_id)
            .await?;
        self.ensure_no_live_invocations(&bundle.manifest.extension_id, "update")?;
        let _invocation_gate = INVOCATION_GATE
            .lock()
            .map_err(|_| "Extension invocation gate is poisoned".to_string())?;
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Extension store lock is poisoned".to_string())?;
        let mut state = self.load_registry()?;
        let extension_id = bundle.manifest.extension_id.clone();
        if let Some(record) = state.records.get_mut(&extension_id) {
            prune_expired_invocations(record);
        }
        validate_dependencies(&bundle.manifest, &state)?;
        validate_capability_collisions(&bundle.manifest, &state)?;
        validate_dependents(&extension_id, bundle.manifest.version, &state)?;
        ensure_no_active_invocation(&self.root, &extension_id, "update")?;
        let record = state
            .records
            .get(&extension_id)
            .ok_or_else(|| "Extension was removed before update completed".to_string())?;
        if record.health.running {
            return Err("Stop the extension before updating it".to_string());
        }
        if bundle.manifest.version <= active_version(record)?.manifest.version {
            return Err("Update is no longer newer than the installed version".to_string());
        }
        self.persist_bundle(&bundle)?;
        let version = bundle.manifest.version.to_string();
        let declared_secret_slots = bundle
            .manifest
            .secret_slots
            .iter()
            .map(|slot| slot.slot_id.clone())
            .collect::<BTreeSet<_>>();
        let record = state.records.get_mut(&extension_id).expect("checked above");
        let removed_secret_slots = record
            .configured_secret_slots
            .difference(&declared_secret_slots)
            .cloned()
            .collect::<Vec<_>>();
        record
            .configured_secret_slots
            .retain(|slot| declared_secret_slots.contains(slot));
        let config = resolved_config(&bundle.manifest, &record.config);
        let previous = record.active_version.clone();
        record.versions.insert(
            version.clone(),
            InstalledVersion {
                manifest: bundle.manifest,
                trust: bundle.trust,
                grants,
                observed_source: InstallSource::LocalFolder {
                    canonical_path: bundle.source.to_string_lossy().to_string(),
                },
                installed_at_ms: now_ms(),
            },
        );
        record.previous_version = Some(previous);
        record.active_version = version;
        record.config = config;
        record.health.validated = true;
        record.health.running = false;
        record.health.state = HealthState::Stopped;
        record.health.consecutive_failures = 0;
        record.health.last_error = None;
        push_log(
            record,
            "info",
            "Update verified and activated; extension is stopped",
            None,
        );
        prune_versions(record);
        self.save_registry(&state)?;
        let mut cleanup_errors = Vec::new();
        for slot in removed_secret_slots {
            if let Err(error) = delete_secret(&extension_id, &slot) {
                cleanup_errors.push(error);
            }
        }
        if !cleanup_errors.is_empty() {
            return Err(format!(
                "Extension was updated, but removed-secret cleanup was incomplete: {}",
                cleanup_errors.join("; ")
            ));
        }
        self.detail(
            state.records.get(&extension_id).expect("record exists"),
            &state,
        )
    }

    pub async fn validate_installed(&self, extension_id: &str) -> Result<ExtensionDetail, String> {
        validate_id("extension id", extension_id)?;
        let _extension_lock = self.acquire_extension_lock_async(extension_id).await?;
        self.ensure_no_live_invocations(extension_id, "validate")?;
        let (manifest, component, grants, config, validated_version) = {
            let state = self.load_registry()?;
            let record = state
                .records
                .get(extension_id)
                .ok_or_else(|| format!("Extension '{extension_id}' is not installed"))?;
            let active = active_version(record)?;
            if let Some(blocker) = current_trust_status(active, &self.load_trust_store()?)?.1 {
                return Err(format!("Extension trust check failed: {blocker}"));
            }
            (
                active.manifest.clone(),
                self.read_installed_component(&active.manifest)?,
                active.grants.clone(),
                record.config.clone(),
                record.active_version.clone(),
            )
        };
        let result = self
            .instantiate_only(&manifest, &component, &grants, &config)
            .await;
        let _invocation_gate = INVOCATION_GATE
            .lock()
            .map_err(|_| "Extension invocation gate is poisoned".to_string())?;
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Extension store lock is poisoned".to_string())?;
        let mut state = self.load_registry()?;
        let record = state
            .records
            .get_mut(extension_id)
            .ok_or_else(|| "Extension was removed during validation".to_string())?;
        prune_expired_invocations(record);
        if record.active_version != validated_version {
            return Err(
                "Extension version changed during validation; result was discarded".to_string(),
            );
        }
        match result {
            Ok(()) => {
                record.health.validated = true;
                record.health.consecutive_failures = 0;
                record.health.last_error = None;
                record.health.state = if record.health.running {
                    HealthState::Healthy
                } else {
                    HealthState::Stopped
                };
                push_log(
                    record,
                    "info",
                    "Installed component validation succeeded",
                    None,
                );
            }
            Err(error) => {
                cancel_active_invocations(&self.root, extension_id)?;
                record.health.validated = false;
                record.health.running = false;
                record.health.state = HealthState::Unhealthy;
                record.health.last_error = Some(bounded(&error, MAX_LOG_MESSAGE_BYTES));
                push_log(
                    record,
                    "error",
                    &format!("Validation failed: {error}"),
                    None,
                );
                self.save_registry(&state)?;
                return Err(error);
            }
        }
        self.save_registry(&state)?;
        let detail = self.detail(
            state.records.get(extension_id).expect("record exists"),
            &state,
        )?;
        let marker = self.extension_cancel_path(extension_id)?;
        if marker.exists() {
            fs::remove_file(marker)
                .map_err(|error| format!("Cannot clear extension cancellation marker: {error}"))?;
        }
        Ok(detail)
    }

    pub async fn set_enabled(
        &self,
        extension_id: &str,
        enabled: bool,
    ) -> Result<ExtensionDetail, String> {
        validate_id("extension id", extension_id)?;
        let _extension_lock = self.acquire_extension_lock_async(extension_id).await?;
        if !enabled {
            self.request_extension_cancellation(extension_id)?;
        }
        if !enabled {
            self.wait_for_no_live_invocations(extension_id).await?;
        }
        let _invocation_gate = INVOCATION_GATE
            .lock()
            .map_err(|_| "Extension invocation gate is poisoned".to_string())?;
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Extension store lock is poisoned".to_string())?;
        let mut state = self.load_registry()?;
        if let Some(record) = state.records.get_mut(extension_id) {
            prune_expired_invocations(record);
        }
        if enabled {
            let record = state
                .records
                .get(extension_id)
                .ok_or_else(|| format!("Extension '{extension_id}' is not installed"))?;
            let active = active_version(record)?;
            let (trust, trust_blocker) = current_trust_status(active, &self.load_trust_store()?)?;
            let (compatible, reason) = compatibility(&active.manifest);
            let blockers = detail_blockers(
                record,
                active,
                &state,
                &trust,
                trust_blocker.as_deref(),
                compatible,
                reason.as_deref(),
            );
            if !blockers.is_empty() {
                return Err(format!(
                    "Extension cannot be enabled: {}",
                    blockers.join("; ")
                ));
            }
        }
        let record = state
            .records
            .get_mut(extension_id)
            .ok_or_else(|| format!("Extension '{extension_id}' is not installed"))?;
        record.health.enabled = enabled;
        if !enabled {
            record.health.running = false;
            if record.health.state != HealthState::ProtectiveDisabled {
                record.health.state = if record.health.validated {
                    HealthState::Stopped
                } else {
                    HealthState::NotValidated
                };
            }
        }
        push_log(
            record,
            "info",
            if enabled {
                "Extension enabled"
            } else {
                "Extension disabled and stopped"
            },
            None,
        );
        self.save_registry(&state)?;
        let detail = self.detail(
            state.records.get(extension_id).expect("record exists"),
            &state,
        )?;
        let marker = self.extension_cancel_path(extension_id)?;
        if marker.exists() {
            fs::remove_file(marker)
                .map_err(|error| format!("Cannot clear extension cancellation marker: {error}"))?;
        }
        Ok(detail)
    }

    pub async fn set_running(
        &self,
        extension_id: &str,
        running: bool,
    ) -> Result<ExtensionDetail, String> {
        validate_id("extension id", extension_id)?;
        if !running {
            let _extension_lock = self.acquire_extension_lock_async(extension_id).await?;
            self.request_extension_cancellation(extension_id)?;
            self.wait_for_no_live_invocations(extension_id).await?;
            let _invocation_gate = INVOCATION_GATE
                .lock()
                .map_err(|_| "Extension invocation gate is poisoned".to_string())?;
            let _guard = STORE_LOCK
                .lock()
                .map_err(|_| "Extension store lock is poisoned".to_string())?;
            let mut state = self.load_registry()?;
            let record = state
                .records
                .get_mut(extension_id)
                .ok_or_else(|| format!("Extension '{extension_id}' is not installed"))?;
            prune_expired_invocations(record);
            record.health.running = false;
            if record.health.state != HealthState::ProtectiveDisabled {
                record.health.state = if record.health.validated {
                    HealthState::Stopped
                } else {
                    HealthState::NotValidated
                };
            }
            push_log(record, "info", "Extension stopped", None);
            self.save_registry(&state)?;
            let detail = self.detail(
                state.records.get(extension_id).expect("record exists"),
                &state,
            )?;
            let marker = self.extension_cancel_path(extension_id)?;
            if marker.exists() {
                fs::remove_file(marker).map_err(|error| {
                    format!("Cannot clear extension cancellation marker: {error}")
                })?;
            }
            return Ok(detail);
        }
        let _extension_lock = self.acquire_extension_lock_async(extension_id).await?;
        self.ensure_no_live_invocations(extension_id, "start")?;
        let (manifest, component, grants, config, starting_version) = {
            let state = self.load_registry()?;
            let record = state
                .records
                .get(extension_id)
                .ok_or_else(|| format!("Extension '{extension_id}' is not installed"))?;
            if !record.health.enabled || !record.health.validated {
                return Err("Extension must be enabled and validated before start".to_string());
            }
            if record.health.state == HealthState::ProtectiveDisabled {
                return Err("Validate the extension before clearing protective disable".to_string());
            }
            let active = active_version(record)?;
            let (trust, trust_blocker) = current_trust_status(active, &self.load_trust_store()?)?;
            let (compatible, reason) = compatibility(&active.manifest);
            let blockers = detail_blockers(
                record,
                active,
                &state,
                &trust,
                trust_blocker.as_deref(),
                compatible,
                reason.as_deref(),
            );
            if !blockers.is_empty() {
                return Err(format!(
                    "Extension cannot be started: {}",
                    blockers.join("; ")
                ));
            }
            (
                active.manifest.clone(),
                self.read_installed_component(&active.manifest)?,
                active.grants.clone(),
                record.config.clone(),
                record.active_version.clone(),
            )
        };
        self.instantiate_only(&manifest, &component, &grants, &config)
            .await?;
        let _invocation_gate = INVOCATION_GATE
            .lock()
            .map_err(|_| "Extension invocation gate is poisoned".to_string())?;
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Extension store lock is poisoned".to_string())?;
        let mut state = self.load_registry()?;
        let record = state
            .records
            .get_mut(extension_id)
            .ok_or_else(|| "Extension was removed during start".to_string())?;
        prune_expired_invocations(record);
        if record.active_version != starting_version {
            return Err("Extension version changed during start; result was discarded".to_string());
        }
        if !record.health.enabled || !record.health.validated {
            return Err("Extension was disabled or invalidated during start".to_string());
        }
        if record.health.state == HealthState::ProtectiveDisabled {
            return Err("Extension entered protective disable during start".to_string());
        }
        record.health.running = true;
        record.health.state = HealthState::Healthy;
        record.health.consecutive_failures = 0;
        record.health.last_error = None;
        push_log(
            record,
            "info",
            "Component instantiated; extension is healthy and running",
            None,
        );
        self.save_registry(&state)?;
        let detail = self.detail(
            state.records.get(extension_id).expect("record exists"),
            &state,
        )?;
        let marker = self.extension_cancel_path(extension_id)?;
        if marker.exists() {
            fs::remove_file(marker)
                .map_err(|error| format!("Cannot clear extension cancellation marker: {error}"))?;
        }
        Ok(detail)
    }

    pub async fn rollback(&self, extension_id: &str) -> Result<ExtensionDetail, String> {
        validate_id("extension id", extension_id)?;
        let (manifest, component, grants, config, target_version, current_version) = {
            let state = self.load_registry()?;
            let record = state
                .records
                .get(extension_id)
                .ok_or_else(|| format!("Extension '{extension_id}' is not installed"))?;
            if record.health.running {
                return Err("Stop the extension before rollback".to_string());
            }
            let target = record
                .previous_version
                .as_ref()
                .ok_or_else(|| "No verified previous version is available".to_string())?;
            let version = record
                .versions
                .get(target)
                .ok_or_else(|| "Previous immutable version is missing".to_string())?;
            let trust_blocker = current_trust_status(version, &self.load_trust_store()?)?.1;
            if trust_blocker.is_some() || !compatibility(&version.manifest).0 {
                return Err("Previous version is no longer trusted or compatible".to_string());
            }
            (
                version.manifest.clone(),
                self.read_installed_component(&version.manifest)?,
                version.grants.clone(),
                resolved_config(&version.manifest, &record.config),
                target.clone(),
                record.active_version.clone(),
            )
        };
        self.instantiate_only(&manifest, &component, &grants, &config)
            .await?;
        let _extension_lock = self.acquire_extension_lock_async(extension_id).await?;
        self.ensure_no_live_invocations(extension_id, "roll back")?;
        let _invocation_gate = INVOCATION_GATE
            .lock()
            .map_err(|_| "Extension invocation gate is poisoned".to_string())?;
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Extension store lock is poisoned".to_string())?;
        let mut state = self.load_registry()?;
        if let Some(record) = state.records.get_mut(extension_id) {
            prune_expired_invocations(record);
        }
        ensure_no_active_invocation(&self.root, extension_id, "roll back")?;
        validate_dependencies(&manifest, &state)?;
        validate_capability_collisions(&manifest, &state)?;
        validate_dependents(extension_id, manifest.version, &state)?;
        let record = state
            .records
            .get_mut(extension_id)
            .ok_or_else(|| "Extension was removed during rollback".to_string())?;
        if record.health.running
            || record.active_version != current_version
            || record.previous_version.as_deref() != Some(target_version.as_str())
        {
            return Err("Extension state changed during rollback; refresh and retry".to_string());
        }
        let declared_secret_slots = manifest
            .secret_slots
            .iter()
            .map(|slot| slot.slot_id.clone())
            .collect::<BTreeSet<_>>();
        let removed_secret_slots = record
            .configured_secret_slots
            .difference(&declared_secret_slots)
            .cloned()
            .collect::<Vec<_>>();
        record
            .configured_secret_slots
            .retain(|slot| declared_secret_slots.contains(slot));
        let current = std::mem::replace(&mut record.active_version, target_version);
        record.previous_version = Some(current);
        record.config = config;
        record.health.validated = true;
        record.health.running = false;
        record.health.state = HealthState::Stopped;
        record.health.consecutive_failures = 0;
        record.health.last_error = None;
        push_log(
            record,
            "info",
            "Rolled back to the verified previous version",
            None,
        );
        self.save_registry(&state)?;
        let mut cleanup_errors = Vec::new();
        for slot in removed_secret_slots {
            if let Err(error) = delete_secret(extension_id, &slot) {
                cleanup_errors.push(error);
            }
        }
        if !cleanup_errors.is_empty() {
            return Err(format!(
                "Extension was rolled back, but removed-secret cleanup was incomplete: {}",
                cleanup_errors.join("; ")
            ));
        }
        self.detail(
            state.records.get(extension_id).expect("record exists"),
            &state,
        )
    }

    pub fn uninstall(&self, extension_id: &str) -> Result<(), String> {
        validate_id("extension id", extension_id)?;
        let _extension_lock = self.acquire_extension_lock(extension_id)?;
        self.ensure_no_live_invocations(extension_id, "uninstall")?;
        let _invocation_gate = INVOCATION_GATE
            .lock()
            .map_err(|_| "Extension invocation gate is poisoned".to_string())?;
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Extension store lock is poisoned".to_string())?;
        let mut state = self.load_registry()?;
        ensure_no_active_invocation(&self.root, extension_id, "uninstall")?;
        if let Some(dependent) = state.records.values().find(|record| {
            record.extension_id != extension_id
                && active_version(record).is_ok_and(|active| {
                    active
                        .manifest
                        .dependencies
                        .iter()
                        .any(|dependency| dependency.extension_id == extension_id)
                })
        }) {
            return Err(format!(
                "Cannot uninstall; extension '{}' depends on it",
                dependent.extension_id
            ));
        }
        let record = state
            .records
            .get(extension_id)
            .ok_or_else(|| format!("Extension '{extension_id}' is not installed"))?;
        if record.health.running {
            return Err("Stop the extension before uninstall".to_string());
        }
        let configured_secret_slots = record.configured_secret_slots.clone();
        state.records.remove(extension_id);
        // Remove executable authority durably before best-effort keychain/file
        // cleanup. A cleanup failure can leave only unreachable material; it
        // cannot resurrect a runnable extension with stale secret metadata.
        self.save_registry(&state)?;
        let mut cleanup_errors = Vec::new();
        for slot in &configured_secret_slots {
            if let Err(error) = delete_secret(extension_id, slot) {
                cleanup_errors.push(error);
            }
        }
        let path = self.root.join("versions").join(extension_id);
        if path.exists() {
            match fs::symlink_metadata(&path) {
                Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {
                    if let Err(error) = fs::remove_dir_all(&path) {
                        cleanup_errors.push(format!("Cannot remove extension files: {error}"));
                    }
                }
                Ok(_) => cleanup_errors
                    .push("Refusing to remove unsafe extension storage entry".to_string()),
                Err(error) => {
                    cleanup_errors.push(format!("Cannot inspect extension files: {error}"));
                }
            }
        }
        let cancellation_marker = self.extension_cancel_path(extension_id)?;
        if cancellation_marker.exists() {
            if let Err(error) = fs::remove_file(&cancellation_marker) {
                cleanup_errors.push(format!(
                    "Cannot remove extension cancellation marker: {error}"
                ));
            }
        }
        if !cleanup_errors.is_empty() {
            return Err(format!(
                "Extension was uninstalled, but cleanup was incomplete: {}",
                cleanup_errors.join("; ")
            ));
        }
        Ok(())
    }

    pub fn logs(&self, extension_id: &str, limit: u32) -> Result<Vec<ExtensionLogRow>, String> {
        validate_id("extension id", extension_id)?;
        let state = self.load_registry()?;
        let rows = &state
            .records
            .get(extension_id)
            .ok_or_else(|| format!("Extension '{extension_id}' is not installed"))?
            .logs;
        let limit = limit.clamp(1, MAX_LOG_ROWS as u32) as usize;
        Ok(rows.iter().rev().take(limit).cloned().collect())
    }

    pub fn set_config(
        &self,
        extension_id: &str,
        values: BTreeMap<String, serde_json::Value>,
    ) -> Result<ExtensionDetail, String> {
        validate_id("extension id", extension_id)?;
        let _extension_lock = self.acquire_extension_lock(extension_id)?;
        self.ensure_no_live_invocations(extension_id, "change configuration")?;
        let _invocation_gate = INVOCATION_GATE
            .lock()
            .map_err(|_| "Extension invocation gate is poisoned".to_string())?;
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Extension store lock is poisoned".to_string())?;
        let mut state = self.load_registry()?;
        if let Some(record) = state.records.get_mut(extension_id) {
            prune_expired_invocations(record);
        }
        ensure_no_active_invocation(&self.root, extension_id, "change configuration")?;
        let record = state
            .records
            .get_mut(extension_id)
            .ok_or_else(|| format!("Extension '{extension_id}' is not installed"))?;
        let manifest = &active_version(record)?.manifest;
        if values.len() > manifest.config_schema.len() {
            return Err("Configuration contains undeclared fields".to_string());
        }
        for key in values.keys() {
            let field = manifest
                .config_schema
                .iter()
                .find(|field| &field.key == key)
                .ok_or_else(|| format!("Undeclared config field '{key}'"))?;
            validate_config_value(field, &values[key])?;
        }
        for field in &manifest.config_schema {
            if field.required && !values.contains_key(&field.key) && field.default.is_none() {
                return Err(format!("Required config field '{}' is missing", field.key));
            }
        }
        if serde_json::to_vec(&values).map_or(usize::MAX, |bytes| bytes.len()) > 256 * 1024 {
            return Err("Extension configuration exceeds 256 KiB".to_string());
        }
        record.config = resolved_config(manifest, &values);
        record.health.running = false;
        if record.health.state != HealthState::ProtectiveDisabled {
            record.health.state = if record.health.validated {
                HealthState::Stopped
            } else {
                HealthState::NotValidated
            };
        }
        push_log(
            record,
            "info",
            "Non-secret configuration updated; extension stopped",
            None,
        );
        self.save_registry(&state)?;
        self.detail(
            state.records.get(extension_id).expect("record exists"),
            &state,
        )
    }

    pub fn set_secret(
        &self,
        extension_id: &str,
        slot_id: &str,
        secret: &str,
    ) -> Result<(), String> {
        validate_id("extension id", extension_id)?;
        validate_id("secret slot", slot_id)?;
        if secret.is_empty() || secret.len() > 64 * 1024 {
            return Err("Extension secrets must contain 1-65536 bytes".to_string());
        }
        let _extension_lock = self.acquire_extension_lock(extension_id)?;
        self.ensure_no_live_invocations(extension_id, "change a secret")?;
        let _invocation_gate = INVOCATION_GATE
            .lock()
            .map_err(|_| "Extension invocation gate is poisoned".to_string())?;
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Extension store lock is poisoned".to_string())?;
        let mut state = self.load_registry()?;
        if let Some(record) = state.records.get_mut(extension_id) {
            prune_expired_invocations(record);
        }
        ensure_no_active_invocation(&self.root, extension_id, "change a secret")?;
        let record = state
            .records
            .get_mut(extension_id)
            .ok_or_else(|| format!("Extension '{extension_id}' is not installed"))?;
        if !active_version(record)?
            .manifest
            .secret_slots
            .iter()
            .any(|slot| slot.slot_id == slot_id)
        {
            return Err("Secret slot is not declared by the active manifest".to_string());
        }
        record.health.running = false;
        if record.health.state != HealthState::ProtectiveDisabled {
            record.health.state = if record.health.validated {
                HealthState::Stopped
            } else {
                HealthState::NotValidated
            };
        }
        push_log(record, "info", "Extension stopped for secret update", None);
        self.save_registry(&state)?;
        write_secret(extension_id, slot_id, secret)?;
        let _registry_lock = self.acquire_registry_lock()?;
        let mut state = self.load_registry()?;
        let record = state
            .records
            .get_mut(extension_id)
            .expect("record was validated before keychain write");
        record.configured_secret_slots.insert(slot_id.to_string());
        push_log(
            record,
            "info",
            &format!("Secret slot '{slot_id}' configured"),
            None,
        );
        self.write_registry_locked(&state)
    }

    pub fn remove_secret(&self, extension_id: &str, slot_id: &str) -> Result<(), String> {
        validate_id("extension id", extension_id)?;
        validate_id("secret slot", slot_id)?;
        let _extension_lock = self.acquire_extension_lock(extension_id)?;
        self.ensure_no_live_invocations(extension_id, "remove a secret")?;
        let _invocation_gate = INVOCATION_GATE
            .lock()
            .map_err(|_| "Extension invocation gate is poisoned".to_string())?;
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Extension store lock is poisoned".to_string())?;
        let mut state = self.load_registry()?;
        if let Some(record) = state.records.get_mut(extension_id) {
            prune_expired_invocations(record);
        }
        ensure_no_active_invocation(&self.root, extension_id, "remove a secret")?;
        let record = state
            .records
            .get_mut(extension_id)
            .ok_or_else(|| format!("Extension '{extension_id}' is not installed"))?;
        record.health.running = false;
        if record.health.state != HealthState::ProtectiveDisabled {
            record.health.state = if record.health.validated {
                HealthState::Stopped
            } else {
                HealthState::NotValidated
            };
        }
        push_log(record, "info", "Extension stopped for secret removal", None);
        self.save_registry(&state)?;
        delete_secret(extension_id, slot_id)?;
        let _registry_lock = self.acquire_registry_lock()?;
        let mut state = self.load_registry()?;
        let record = state
            .records
            .get_mut(extension_id)
            .expect("record was validated before keychain delete");
        record.configured_secret_slots.remove(slot_id);
        push_log(
            record,
            "info",
            &format!("Secret slot '{slot_id}' cleared"),
            None,
        );
        self.write_registry_locked(&state)
    }

    fn persist_bundle(&self, bundle: &LoadedBundle) -> Result<(), String> {
        let directory = self
            .root
            .join("versions")
            .join(&bundle.manifest.extension_id)
            .join(bundle.manifest.version.to_string());
        ensure_private_directory(&directory)?;
        let manifest_bytes = serde_json::to_vec_pretty(&bundle.manifest)
            .map_err(|error| format!("Cannot encode installed manifest: {error}"))?;
        atomic_write(&directory.join(EXTENSION_MANIFEST_FILE), &manifest_bytes)?;
        for relative in bundle.manifest.checksums.keys() {
            let bytes = if relative == &bundle.manifest.component.path {
                bundle.component.as_slice()
            } else {
                bundle
                    .additional_files
                    .get(relative)
                    .ok_or_else(|| format!("Verified bundle file '{relative}' is missing"))?
                    .as_slice()
            };
            let target = directory.join(relative);
            let parent = target
                .parent()
                .ok_or_else(|| "Invalid installed bundle path".to_string())?;
            ensure_private_directory(parent)?;
            atomic_write(&target, bytes)?;
        }
        Ok(())
    }

    fn read_installed_component(&self, manifest: &ExtensionManifest) -> Result<Vec<u8>, String> {
        let directory = self
            .root
            .join("versions")
            .join(&manifest.extension_id)
            .join(manifest.version.to_string());
        let path = safe_join(&directory, &manifest.component.path)?;
        let bytes = read_regular_file(&path, MAX_COMPONENT_BYTES)?;
        if sha256_bytes(&bytes) != manifest.component.sha256.to_ascii_lowercase() {
            return Err("Installed component failed its checksum verification".to_string());
        }
        Ok(bytes)
    }
}

fn validate_approval(preview: &ExtensionPreview, approval: &Approval) -> Result<(), String> {
    if approval.approval_digest != preview.approval_digest {
        return Err("Approval digest does not match this exact extension preview".to_string());
    }
    if !preview.blockers.is_empty() {
        return Err(format!(
            "Extension is blocked: {}",
            preview.blockers.join("; ")
        ));
    }
    if preview.requires_unsigned_approval && !approval.allow_unsigned {
        return Err("Unsigned extension installation requires explicit approval".to_string());
    }
    if preview.requires_untrusted_approval && !approval.allow_untrusted {
        return Err("Untrusted publisher installation requires explicit approval".to_string());
    }
    if preview.requires_high_risk_approval && !approval.allow_high_risk {
        return Err("High-risk permissions require explicit review and approval".to_string());
    }
    Ok(())
}

fn validate_grants(
    manifest: &ExtensionManifest,
    grants: &[PermissionGrant],
) -> Result<Vec<PermissionGrant>, String> {
    let mut ids = BTreeSet::new();
    let mut result = Vec::new();
    for grant in grants {
        if !ids.insert(grant.permission_id.clone()) {
            return Err(format!(
                "Duplicate permission grant '{}'",
                grant.permission_id
            ));
        }
        let permission = manifest
            .permissions
            .iter()
            .find(|permission| permission.permission_id == grant.permission_id)
            .ok_or_else(|| format!("Grant '{}' was not requested", grant.permission_id))?;
        let binding = match permission.kind {
            PermissionKind::WorkspaceRead | PermissionKind::WorkspaceWrite => {
                let value = grant.binding.as_ref().ok_or_else(|| {
                    format!("Workspace grant '{}' needs a binding", grant.permission_id)
                })?;
                let path = Path::new(value)
                    .canonicalize()
                    .map_err(|error| format!("Cannot resolve workspace binding: {error}"))?;
                let metadata = fs::symlink_metadata(&path)
                    .map_err(|error| format!("Cannot inspect workspace binding: {error}"))?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() || !path.is_absolute() {
                    return Err("Workspace binding must be a real absolute directory".to_string());
                }
                Some(path.to_string_lossy().to_string())
            }
            _ => {
                if grant.binding.is_some() {
                    return Err(format!(
                        "Non-workspace grant '{}' cannot carry a host binding",
                        grant.permission_id
                    ));
                }
                None
            }
        };
        result.push(PermissionGrant {
            permission_id: grant.permission_id.clone(),
            binding,
        });
    }
    result.sort_by(|a, b| a.permission_id.cmp(&b.permission_id));
    Ok(result)
}

fn validate_persisted_grants(
    manifest: &ExtensionManifest,
    grants: &[PermissionGrant],
) -> Result<(), String> {
    let mut previous = None::<&str>;
    for grant in grants {
        validate_id("permission grant", &grant.permission_id)?;
        if previous.is_some_and(|value| value >= grant.permission_id.as_str()) {
            return Err("Persisted permission grants must be unique and sorted".to_string());
        }
        previous = Some(&grant.permission_id);
        let permission = manifest
            .permissions
            .iter()
            .find(|permission| permission.permission_id == grant.permission_id)
            .ok_or_else(|| {
                format!(
                    "Persisted grant '{}' was not requested",
                    grant.permission_id
                )
            })?;
        match permission.kind {
            PermissionKind::WorkspaceRead | PermissionKind::WorkspaceWrite => {
                let binding = grant.binding.as_deref().ok_or_else(|| {
                    format!(
                        "Persisted workspace grant '{}' has no binding",
                        grant.permission_id
                    )
                })?;
                if binding.len() > 4096
                    || binding.contains('\0')
                    || !Path::new(binding).is_absolute()
                {
                    return Err("Persisted workspace binding is invalid".to_string());
                }
            }
            _ if grant.binding.is_some() => {
                return Err(format!(
                    "Persisted non-workspace grant '{}' has a binding",
                    grant.permission_id
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn push_log(record: &mut InstalledRecord, level: &str, message: &str, invocation_id: Option<&str>) {
    record.logs.push(ExtensionLogRow {
        at_ms: now_ms(),
        level: bounded(level, 16),
        message: bounded(message, MAX_LOG_MESSAGE_BYTES),
        invocation_id: invocation_id.map(|value| bounded(value, 160)),
    });
    if record.logs.len() > MAX_LOG_ROWS {
        record.logs.drain(..record.logs.len() - MAX_LOG_ROWS);
    }
}

fn bounded(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

fn prune_versions(record: &mut InstalledRecord) {
    let keep = [
        Some(record.active_version.as_str()),
        record.previous_version.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<BTreeSet<_>>();
    if record.versions.len() > 4 {
        let remove = record
            .versions
            .keys()
            .filter(|version| !keep.contains(version.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        for version in remove.into_iter().take(record.versions.len() - 4) {
            record.versions.remove(&version);
        }
    }
}

fn keychain_service() -> String {
    crate::profiles::keychain_service(KEYCHAIN_SERVICE_BASE)
}

fn secret_account(extension_id: &str, slot_id: &str) -> String {
    format!("extension:{extension_id}:{slot_id}")
}

fn write_secret(extension_id: &str, slot_id: &str, secret: &str) -> Result<(), String> {
    keyring::Entry::new(&keychain_service(), &secret_account(extension_id, slot_id))
        .map_err(|error| format!("Cannot open extension keychain entry: {error}"))?
        .set_password(secret)
        .map_err(|error| format!("Cannot save extension secret: {error}"))
}

fn read_secret(extension_id: &str, slot_id: &str) -> Result<String, String> {
    keyring::Entry::new(&keychain_service(), &secret_account(extension_id, slot_id))
        .map_err(|error| format!("Cannot open extension keychain entry: {error}"))?
        .get_password()
        .map_err(|error| format!("Configured extension secret is unavailable: {error}"))
}

fn delete_secret(extension_id: &str, slot_id: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(&keychain_service(), &secret_account(extension_id, slot_id))
        .map_err(|error| format!("Cannot open extension keychain entry: {error}"))?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(format!("Cannot delete extension secret: {error}")),
    }
}

struct RuntimeHost {
    table: ResourceTable,
    wasi: WasiCtx,
    limits: AggregateStoreLimits,
    extension_id: String,
    invocation_id: String,
    manifest: ExtensionManifest,
    grants: Vec<PermissionGrant>,
    config: BTreeMap<String, serde_json::Value>,
    configured_secret_slots: BTreeSet<String>,
    input_artifact_ids: BTreeSet<String>,
    /// Every artifact this invocation actually wrote, in the host's own
    /// tally. A guest that names an artifact id in its output has proved
    /// nothing — the store is content-addressed and shared, so any id it can
    /// guess or was once handed would resolve. A native consumer that accepts
    /// audio, an attachment or a document from an extension checks the id
    /// against this set, which only `artifact_write` can add to.
    written_artifact_ids: BTreeSet<String>,
    artifact_store: ArtifactStore,
    private_state: BTreeMap<String, Vec<u8>>,
    logs: Vec<ExtensionLogRow>,
    events: Vec<(String, String)>,
    tool_result: Option<String>,
    emitted_output_bytes: usize,
    telemetry_values: BTreeMap<String, u64>,
    undeclared_attempts: u64,
    cancellation: CancellationToken,
    cancellation_markers: Vec<PathBuf>,
    model_hub: Option<Arc<crate::m3_runtime_hub::M3RuntimeHub>>,
}

struct AggregateStoreLimits {
    inner: StoreLimits,
    total_memory_bytes: usize,
    pending_memory_growth: usize,
}

impl ResourceLimiter for AggregateStoreLimits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        // A prior allowed growth succeeded when Wasmtime starts the next
        // request without calling `memory_grow_failed`.
        self.pending_memory_growth = 0;
        let growth = desired.saturating_sub(current);
        if self.total_memory_bytes.saturating_add(growth) > DEFAULT_MEMORY_BYTES {
            return Err(wasmtime::Error::msg(format!(
                "forcing trap when aggregate linear memory would grow to {} bytes",
                self.total_memory_bytes.saturating_add(growth)
            )));
        }
        let allowed = self.inner.memory_growing(current, desired, maximum)?;
        if allowed {
            self.total_memory_bytes = self.total_memory_bytes.saturating_add(growth);
            self.pending_memory_growth = growth;
        }
        Ok(allowed)
    }

    fn memory_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.total_memory_bytes = self
            .total_memory_bytes
            .saturating_sub(self.pending_memory_growth);
        self.pending_memory_growth = 0;
        self.inner.memory_grow_failed(error)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.inner.table_growing(current, desired, maximum)
    }

    fn table_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.inner.table_grow_failed(error)
    }

    fn instances(&self) -> usize {
        self.inner.instances()
    }

    fn tables(&self) -> usize {
        self.inner.tables()
    }

    fn memories(&self) -> usize {
        self.inner.memories()
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ModelBrokerOperation {
    ChatCompletions,
    Responses,
    AnthropicMessages,
    Embeddings,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelBrokerRequest {
    runtime_id: String,
    operation: ModelBrokerOperation,
    body: serde_json::Value,
}

impl WasiView for RuntimeHost {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl RuntimeHost {
    fn granted(&self, kind: PermissionKind, scope: &str) -> Option<&PermissionGrant> {
        self.manifest
            .permissions
            .iter()
            .filter(|permission| permission.kind == kind && permission.scope == scope)
            .find_map(|permission| {
                self.grants
                    .iter()
                    .find(|grant| grant.permission_id == permission.permission_id)
            })
    }

    fn deny<T>(&mut self, operation: &str) -> Result<T, String> {
        self.undeclared_attempts = self.undeclared_attempts.saturating_add(1);
        self.log_row(
            "warn",
            &format!("Broker denied undeclared or ungranted access: {operation}"),
        );
        Err(format!("Permission denied: {operation}"))
    }

    fn log_row(&mut self, level: &str, message: &str) {
        if self.logs.len() >= MAX_LOG_ROWS {
            self.logs.remove(0);
        }
        self.logs.push(ExtensionLogRow {
            at_ms: now_ms(),
            level: bounded(level, 16),
            message: bounded(message, MAX_LOG_MESSAGE_BYTES),
            invocation_id: Some(self.invocation_id.clone()),
        });
    }

    fn workspace_binding(&self, kind: PermissionKind, handle: &str) -> Option<PathBuf> {
        self.granted(kind, handle)
            .and_then(|grant| grant.binding.as_ref())
            .map(PathBuf::from)
    }

    fn charge_emitted_output(&mut self, bytes: usize) -> Result<(), String> {
        let next = self.emitted_output_bytes.saturating_add(bytes);
        if next > MAX_OUTPUT_BYTES {
            return Err("Extension emitted output exceeds 4 MiB in total".to_string());
        }
        self.emitted_output_bytes = next;
        Ok(())
    }
}

impl bindings::little_monkey::extension::host::Host for RuntimeHost {
    async fn log(&mut self, level: String, message: String) -> Result<(), String> {
        if !matches!(
            level.as_str(),
            "trace" | "debug" | "info" | "warn" | "error"
        ) {
            return Err("Log level is invalid".to_string());
        }
        if message.len() > MAX_LOG_MESSAGE_BYTES {
            return Err("Log message exceeds 8 KiB".to_string());
        }
        self.log_row(&level, &message);
        Ok(())
    }

    async fn now_ms(&mut self) -> u64 {
        now_ms()
    }

    async fn random_bytes(&mut self, length: u32) -> Result<Vec<u8>, String> {
        if length > MAX_RANDOM_BYTES {
            return Err("Random request exceeds 64 KiB".to_string());
        }
        let mut bytes = vec![0u8; length as usize];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| "Operating-system random generator failed".to_string())?;
        Ok(bytes)
    }

    async fn config_get(&mut self, key: String) -> Result<Option<String>, String> {
        validate_id("config key", &key)?;
        if !self
            .manifest
            .config_schema
            .iter()
            .any(|field| field.key == key)
        {
            return self.deny(&format!("config key {key}"));
        }
        self.config
            .get(&key)
            .map(|value| {
                serde_json::to_string(value)
                    .map_err(|error| format!("Cannot encode extension config value: {error}"))
            })
            .transpose()
    }

    async fn state_get(&mut self, key: String) -> Result<Option<Vec<u8>>, String> {
        validate_id("private state key", &key)?;
        Ok(self.private_state.get(&key).cloned())
    }

    async fn state_put(&mut self, key: String, value: Vec<u8>) -> Result<(), String> {
        validate_id("private state key", &key)?;
        if value.len() > MAX_PRIVATE_STATE_BYTES {
            return Err("Private state value exceeds 256 KiB".to_string());
        }
        let previous = self.private_state.insert(key.clone(), value);
        let size = self
            .private_state
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum::<usize>();
        if size > MAX_PRIVATE_STATE_BYTES {
            match previous {
                Some(value) => {
                    self.private_state.insert(key, value);
                }
                None => {
                    self.private_state.remove(&key);
                }
            }
            return Err("Extension private state exceeds 256 KiB".to_string());
        }
        Ok(())
    }

    async fn send_http(
        &mut self,
        request: bindings::little_monkey::extension::host::HttpRequest,
    ) -> Result<bindings::little_monkey::extension::host::HttpResponse, String> {
        if request.url.len() > 16 * 1024
            || request.body.len() > MAX_HTTP_BODY_BYTES
            || request.headers.len() > 64
        {
            return Err("HTTP request exceeds a broker limit".to_string());
        }
        let url = Url::parse(&request.url).map_err(|_| "HTTP URL is invalid".to_string())?;
        let origin = canonical_origin(&url)?;
        if self
            .granted(PermissionKind::NetworkOrigin, &origin)
            .is_none()
        {
            return self.deny(&format!("network origin {origin}"));
        }
        crate::web::validate_fetch_url(&url, false).map_err(|denial| {
            format!("HTTP target refused ({}): {}", denial.rule().code(), denial)
        })?;
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|_| "HTTP method is invalid".to_string())?;
        if !matches!(
            method,
            reqwest::Method::GET
                | reqwest::Method::HEAD
                | reqwest::Method::POST
                | reqwest::Method::PUT
                | reqwest::Method::PATCH
                | reqwest::Method::DELETE
        ) {
            return Err("HTTP method is not brokered".to_string());
        }
        let client = crate::web::executable_extension_http_client(Duration::from_secs(20))
            .map_err(|error| format!("Cannot initialize HTTP broker: {error}"))?;
        let mut builder = client.request(method, url);
        let mut header_bytes = 0usize;
        for header in request.headers {
            validate_header_name(&header.name)?;
            if self.manifest.secret_slots.iter().any(|slot| {
                slot.auth_header
                    .as_deref()
                    .is_some_and(|managed| managed.eq_ignore_ascii_case(&header.name))
            }) {
                return Err("Secret authentication headers are host-managed".to_string());
            }
            if header.value.contains(['\r', '\n']) {
                return Err("HTTP header contains a line break".to_string());
            }
            header_bytes = header_bytes
                .saturating_add(header.name.len())
                .saturating_add(header.value.len());
            if header_bytes > 64 * 1024 {
                return Err("HTTP headers exceed 64 KiB".to_string());
            }
            builder = builder.header(&header.name, &header.value);
        }
        if let Some(slot_id) = request.auth_slot {
            if self.granted(PermissionKind::SecretUse, &slot_id).is_none()
                || !self.configured_secret_slots.contains(&slot_id)
            {
                return self.deny(&format!("secret slot {slot_id}"));
            }
            let slot = self
                .manifest
                .secret_slots
                .iter()
                .find(|slot| slot.slot_id == slot_id)
                .ok_or_else(|| "Secret slot is no longer declared".to_string())?;
            let header = slot.auth_header.as_ref().ok_or_else(|| {
                "Secret slot is not configured for host-mediated HTTP auth".to_string()
            })?;
            let scheme = slot.auth_scheme.as_deref().unwrap_or_default();
            let secret = read_secret(&self.extension_id, &slot_id)?;
            let value = if scheme.is_empty() {
                secret
            } else {
                format!("{scheme} {secret}")
            };
            builder = builder.header(header, value);
        }
        if !request.body.is_empty() {
            builder = builder.body(request.body);
        }
        let response = tokio::select! {
            _ = self.cancellation.cancelled() => {
                return Err(CANCELLED_ERROR.to_string());
            }
            response = crate::egress::send(builder) => response
                .map_err(|error| format!("Brokered HTTP request failed: {error}"))?,
        };
        if canonical_origin(response.url())? != origin {
            return Err("HTTP redirect left the exact granted origin".to_string());
        }
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter(|(name, _)| !is_restricted_header(name.as_str()))
            .take(64)
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| bindings::little_monkey::extension::host::Header {
                        name: name.as_str().to_string(),
                        value: bounded(value, 8 * 1024),
                    })
            })
            .collect();
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        loop {
            let chunk = tokio::select! {
                _ = self.cancellation.cancelled() => {
                    return Err(CANCELLED_ERROR.to_string());
                }
                chunk = stream.next() => chunk,
            };
            let Some(chunk) = chunk else {
                break;
            };
            let chunk = chunk.map_err(|error| format!("HTTP response failed: {error}"))?;
            if body.len().saturating_add(chunk.len()) > MAX_HTTP_BODY_BYTES {
                return Err("HTTP response exceeds 4 MiB".to_string());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(bindings::little_monkey::extension::host::HttpResponse {
            status,
            headers,
            body,
        })
    }

    async fn artifact_read(&mut self, artifact_id: String) -> Result<Vec<u8>, String> {
        validate_sha256(&artifact_id, "artifact id")?;
        let fixed_grant = self
            .granted(PermissionKind::ArtifactRead, &artifact_id)
            .is_some();
        let input_grant = self
            .granted(PermissionKind::ArtifactRead, "invocation_inputs")
            .is_some()
            && self.input_artifact_ids.contains(&artifact_id);
        if !fixed_grant && !input_grant {
            return self.deny(&format!("artifact read {artifact_id}"));
        }
        let bytes = self
            .artifact_store
            .read(&artifact_id)
            .map_err(|error| format!("Artifact read failed: {error}"))?;
        if bytes.len() > MAX_ARTIFACT_READ_BYTES {
            return Err("Artifact exceeds the 32 MiB extension read limit".to_string());
        }
        Ok(bytes)
    }

    async fn artifact_write(&mut self, bytes: Vec<u8>) -> Result<String, String> {
        if self
            .granted(PermissionKind::ArtifactWrite, "content_v1")
            .is_none()
        {
            return self.deny("artifact write");
        }
        if bytes.len() > MAX_OUTPUT_BYTES {
            return Err("Artifact write exceeds 4 MiB".to_string());
        }
        self.charge_emitted_output(bytes.len())?;
        if self.written_artifact_ids.len() >= MAX_WRITTEN_ARTIFACTS {
            return Err(format!(
                "One invocation may write at most {MAX_WRITTEN_ARTIFACTS} artifacts"
            ));
        }
        let blob = self
            .artifact_store
            .put(&bytes)
            .map_err(|error| format!("Artifact write failed: {error}"))?;
        self.written_artifact_ids.insert(blob.id.clone());
        Ok(blob.id)
    }

    async fn workspace_read(
        &mut self,
        handle: String,
        relative_path: String,
    ) -> Result<Vec<u8>, String> {
        let Some(root) = self.workspace_binding(PermissionKind::WorkspaceRead, &handle) else {
            return self.deny(&format!("workspace read handle {handle}"));
        };
        validate_relative_path(&relative_path)?;
        let path = safe_join(&root, &relative_path)?;
        read_regular_file(&path, MAX_OUTPUT_BYTES)
    }

    async fn workspace_write(
        &mut self,
        handle: String,
        relative_path: String,
        bytes: Vec<u8>,
    ) -> Result<(), String> {
        let Some(root) = self.workspace_binding(PermissionKind::WorkspaceWrite, &handle) else {
            return self.deny(&format!("workspace write handle {handle}"));
        };
        if bytes.len() > MAX_OUTPUT_BYTES {
            return Err("Workspace write exceeds 4 MiB".to_string());
        }
        self.charge_emitted_output(bytes.len())?;
        write_workspace_file(&root, &relative_path, &bytes)
    }

    async fn model_invoke(
        &mut self,
        model_id: String,
        request_json: String,
    ) -> Result<String, String> {
        if request_json.len() > MAX_INPUT_BYTES {
            return Err("Model request must be bounded JSON".to_string());
        }
        let request: ModelBrokerRequest = serde_json::from_str(&request_json)
            .map_err(|error| format!("Invalid model broker request: {error}"))?;
        let (target_runtime, target_model) = validate_model_target(&model_id)?;
        validate_id("runtime id", &request.runtime_id)?;
        let body_model = request
            .body
            .get("model")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "Model request body must declare an exact model".to_string())?;
        validate_model_id(body_model)?;
        let exact_target = format!("{}:{body_model}", request.runtime_id);
        if target_runtime != request.runtime_id
            || target_model != body_model
            || self
                .granted(PermissionKind::ModelInvoke, &exact_target)
                .is_none()
        {
            return self.deny(&format!("model invoke {exact_target}"));
        }
        let Some(hub) = self.model_hub.clone() else {
            return Err("The managed model runtime is not available in this process".to_string());
        };
        let body = serde_json::to_vec(&request.body)
            .map_err(|error| format!("Cannot encode model request: {error}"))?;
        let context = crate::m3_runtime_hub::M3OperationContext {
            cancellation: self.cancellation.clone(),
            timeout_ms: DEFAULT_TIMEOUT_MS.saturating_sub(1_000),
        };
        let response = match request.operation {
            ModelBrokerOperation::Embeddings => {
                hub.dispatch_embeddings(
                    &crate::m3_runtime_hub::M3EmbeddingDispatchRequest {
                        runtime_id: request.runtime_id,
                        request_id: self.invocation_id.clone(),
                        body,
                        caller: crate::m3_runtime_hub::M3ApiCaller::Internal,
                        now_ms: now_ms(),
                    },
                    &context,
                )
                .await
            }
            operation => {
                let protocol = match operation {
                    ModelBrokerOperation::ChatCompletions => {
                        crate::compatibility_hub::CompatibilityProtocol::OpenAiChatCompletions
                    }
                    ModelBrokerOperation::Responses => {
                        crate::compatibility_hub::CompatibilityProtocol::OpenAiResponses
                    }
                    ModelBrokerOperation::AnthropicMessages => {
                        crate::compatibility_hub::CompatibilityProtocol::AnthropicMessages
                    }
                    ModelBrokerOperation::Embeddings => unreachable!(),
                };
                hub.dispatch_api(
                    &crate::m3_runtime_hub::M3ApiDispatchRequest {
                        protocol,
                        runtime_id: request.runtime_id,
                        request_id: self.invocation_id.clone(),
                        body,
                        caller: crate::m3_runtime_hub::M3ApiCaller::Internal,
                        now_ms: now_ms(),
                    },
                    &context,
                )
                .await
            }
        }
        .map_err(|error| format!("Managed model request failed: {error}"))?;
        let response = serde_json::to_string(&response.body)
            .map_err(|error| format!("Cannot encode managed model response: {error}"))?;
        if response.len() > MAX_OUTPUT_BYTES {
            return Err("Managed model response exceeds 4 MiB".to_string());
        }
        Ok(response)
    }

    async fn device_request(
        &mut self,
        device_id: String,
        capability: String,
        request_json: String,
    ) -> Result<String, String> {
        validate_id("device id", &device_id)?;
        validate_id("device capability", &capability)?;
        let exact_target = format!("{device_id}:{capability}");
        if self
            .granted(PermissionKind::Device, &exact_target)
            .is_none()
        {
            return self.deny(&format!("device {exact_target}"));
        }
        if request_json.len() > MAX_INPUT_BYTES {
            return Err("Device request must be bounded JSON".to_string());
        }
        let request: serde_json::Value = serde_json::from_str(&request_json)
            .map_err(|_| "Device request must be bounded JSON".to_string())?;
        if let Some(artifact_id) = request
            .get("artifact_id")
            .and_then(serde_json::Value::as_str)
        {
            validate_sha256(artifact_id, "device artifact id")?;
            let fixed_grant = self
                .granted(PermissionKind::ArtifactRead, artifact_id)
                .is_some();
            let input_grant = self
                .granted(PermissionKind::ArtifactRead, "invocation_inputs")
                .is_some()
                && self.input_artifact_ids.contains(artifact_id);
            if !fixed_grant && !input_grant {
                return self.deny(&format!("device artifact {artifact_id}"));
            }
        }
        let response = crate::daemon_commands::extension_device_action(
            &device_id,
            &capability,
            &request,
            &self.invocation_id,
        )
        .await
        .and_then(|value| {
            serde_json::to_string(&value)
                .map_err(|error| format!("Cannot encode device response: {error}"))
        })?;
        if response.len() > MAX_OUTPUT_BYTES {
            return Err("Device response exceeds 4 MiB".to_string());
        }
        Ok(response)
    }

    async fn emit_event(&mut self, kind: String, payload_json: String) -> Result<(), String> {
        validate_id("event kind", &kind)?;
        if payload_json.len() > MAX_OUTPUT_BYTES
            || serde_json::from_str::<serde_json::Value>(&payload_json).is_err()
            || self.events.len() >= 128
        {
            return Err("Event is invalid or exceeds an invocation limit".to_string());
        }
        self.charge_emitted_output(kind.len().saturating_add(payload_json.len()))?;
        self.events.push((kind, payload_json));
        Ok(())
    }

    async fn set_tool_result(&mut self, payload_json: String) -> Result<(), String> {
        if payload_json.len() > MAX_OUTPUT_BYTES
            || serde_json::from_str::<serde_json::Value>(&payload_json).is_err()
        {
            return Err("Tool result must be bounded JSON".to_string());
        }
        self.charge_emitted_output(payload_json.len())?;
        self.tool_result = Some(payload_json);
        Ok(())
    }

    async fn is_cancelled(&mut self) -> bool {
        self.cancellation.is_cancelled()
    }

    async fn telemetry(&mut self, name: String, value: u64) -> Result<(), String> {
        validate_id("telemetry name", &name)?;
        if self.telemetry_values.len() >= 128 && !self.telemetry_values.contains_key(&name) {
            return Err("Telemetry key limit reached".to_string());
        }
        self.telemetry_values.insert(name, value);
        Ok(())
    }
}

struct RuntimeExecution {
    outcome: Result<String, String>,
    host: RuntimeHost,
    fuel_consumed: u64,
    duration_ms: u64,
}

impl ExtensionManager {
    async fn instantiate_only(
        &self,
        manifest: &ExtensionManifest,
        component: &[u8],
        grants: &[PermissionGrant],
        config: &BTreeMap<String, serde_json::Value>,
    ) -> Result<(), String> {
        let token = CancellationToken::new();
        let host = self.runtime_host(
            manifest.clone(),
            grants.to_vec(),
            config.clone(),
            "validation".to_string(),
            token,
            Vec::new(),
            BTreeSet::new(),
            BTreeSet::new(),
            BTreeMap::new(),
        )?;
        let execution = self.execute_component(component, host, None).await?;
        execution.outcome.map(|_| ())
    }

    pub async fn invoke(&self, request: InvocationRequest) -> Result<InvocationResult, String> {
        let expected = match (&request.expected_kind, &request.expected_version) {
            (Some(kind), Some(version)) => {
                SemanticVersion::parse(version).map_err(|error| error.to_string())?;
                Some((*kind, version.clone()))
            }
            (None, None) => None,
            _ => {
                return Err(
                    "Expected capability kind and version must be supplied together".to_string(),
                )
            }
        };
        self.invoke_checked(request, expected).await
    }

    async fn invoke_checked(
        &self,
        request: InvocationRequest,
        expected: Option<(CapabilityKind, String)>,
    ) -> Result<InvocationResult, String> {
        validate_id("extension id", &request.extension_id)?;
        validate_id("capability id", &request.capability_id)?;
        if request.input_json.len() > MAX_INPUT_BYTES
            || serde_json::from_str::<serde_json::Value>(&request.input_json).is_err()
            || request.input_artifact_ids.len() > 128
        {
            return Err(
                "Extension input must be bounded JSON with at most 128 artifacts".to_string(),
            );
        }
        for artifact_id in &request.input_artifact_ids {
            validate_sha256(artifact_id, "input artifact id")?;
        }
        if request
            .input_artifact_ids
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            != request.input_artifact_ids.len()
        {
            return Err("Extension input artifact ids must be unique".to_string());
        }
        let invocation_id = request
            .invocation_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        validate_id("invocation id", &invocation_id)?;
        let request_sha256 = invocation_request_sha256(&request)?;
        // Registration is serialized with lifecycle changes. The durable
        // lease then protects this version while concurrent calls execute.
        let extension_lock = self
            .acquire_extension_lock_async(&request.extension_id)
            .await?;
        let invocation_cancel_path = self.invocation_cancel_path(&invocation_id)?;
        let extension_cancel_path = self.extension_cancel_path(&request.extension_id)?;
        let token = CancellationToken::new();
        let (component, host, version) = {
            let _invocation_gate = INVOCATION_GATE
                .lock()
                .map_err(|_| "Extension invocation gate is poisoned".to_string())?;
            let _guard = STORE_LOCK
                .lock()
                .map_err(|_| "Extension store lock is poisoned".to_string())?;
            let _registry_lock = self.acquire_registry_lock()?;
            let mut state = self.load_registry()?;
            if state.records.iter().any(|(extension_id, record)| {
                extension_id != &request.extension_id
                    && (record.completed_invocations.contains_key(&invocation_id)
                        || record.active_invocations.contains_key(&invocation_id))
            }) {
                return Err("Invocation id is already owned by another extension".to_string());
            }
            {
                let record = state
                    .records
                    .get_mut(&request.extension_id)
                    .ok_or_else(|| {
                        format!("Extension '{}' is not installed", request.extension_id)
                    })?;
                if let Some(stored) = record.completed_invocations.get(&invocation_id) {
                    if stored.request_sha256 != request_sha256
                        || expected
                            .as_ref()
                            .is_some_and(|(_, version)| version != &stored.version)
                    {
                        return Err(
                            "Invocation id was already used for different immutable input"
                                .to_string(),
                        );
                    }
                    return stored.result.clone().ok_or_else(|| {
                        stored
                            .error
                            .clone()
                            .unwrap_or_else(|| "Stored invocation is invalid".to_string())
                    });
                }
                prune_expired_invocations(record);
                if record.active_invocations.contains_key(&invocation_id) {
                    return Err("Invocation id is already active".to_string());
                }
            }
            if invocation_cancel_path.exists() || extension_cancel_path.exists() {
                return Err("Extension invocation was cancelled before start".to_string());
            }
            let record = state
                .records
                .get(&request.extension_id)
                .expect("record checked above");
            if !record.health.enabled || !record.health.running || !record.health.validated {
                return Err("Extension must be enabled, validated, and running".to_string());
            }
            if !matches!(
                record.health.state,
                HealthState::Healthy | HealthState::Degraded
            ) {
                return Err("Extension runtime is not healthy".to_string());
            }
            let active = active_version(record)?;
            if let Some(blocker) = current_trust_status(active, &self.load_trust_store()?)?.1 {
                return Err(format!("Extension trust check failed: {blocker}"));
            }
            validate_dependencies(&active.manifest, &state)?;
            if expected
                .as_ref()
                .is_some_and(|(_, version)| version != &record.active_version)
            {
                return Err(
                    "Active extension capability version changed before invocation".to_string(),
                );
            }
            let capability = active
                .manifest
                .capabilities
                .iter()
                .find(|capability| capability.capability_id == request.capability_id)
                .ok_or_else(|| "Capability is not declared by this extension".to_string())?;
            if expected
                .as_ref()
                .is_some_and(|(kind, _)| *kind != capability.kind)
            {
                return Err(
                    "Active extension capability kind changed before invocation".to_string()
                );
            }
            let component = self.read_installed_component(&active.manifest)?;
            let host = self.runtime_host(
                active.manifest.clone(),
                active.grants.clone(),
                record.config.clone(),
                invocation_id.clone(),
                token.clone(),
                vec![
                    invocation_cancel_path.clone(),
                    extension_cancel_path.clone(),
                ],
                record.configured_secret_slots.clone(),
                request.input_artifact_ids.iter().cloned().collect(),
                record.private_state.clone(),
            )?;
            let version = record.active_version.clone();
            state
                .records
                .get_mut(&request.extension_id)
                .expect("record checked above")
                .active_invocations
                .insert(
                    invocation_id.clone(),
                    ActiveInvocationLease {
                        request_sha256: request_sha256.clone(),
                        version: version.clone(),
                        deadline_at_ms: now_ms()
                            .saturating_add(DEFAULT_TIMEOUT_MS)
                            .saturating_add(2_000),
                    },
                );
            self.write_registry_locked(&state)?;
            (component, host, version)
        };
        {
            let mut cancellations = CANCELLATIONS
                .lock()
                .map_err(|_| "Extension cancellation registry is poisoned".to_string())?;
            if cancellations.contains_key(&invocation_id) {
                let _registry_lock = self.acquire_registry_lock()?;
                let mut state = self.load_registry()?;
                if let Some(record) = state.records.get_mut(&request.extension_id) {
                    record.active_invocations.remove(&invocation_id);
                    self.write_registry_locked(&state)?;
                }
                return Err("Invocation id is already active in this process".to_string());
            }
            cancellations.insert(
                invocation_id.clone(),
                ActiveInvocation {
                    store_root: self.root.clone(),
                    extension_id: request.extension_id.clone(),
                    token: token.clone(),
                },
            );
        }
        drop(extension_lock);
        let execution = self
            .execute_component(
                &component,
                host,
                Some((&request.capability_id, &request.input_json)),
            )
            .await;
        let _completion_gate = INVOCATION_GATE
            .lock()
            .map_err(|_| "Extension invocation gate is poisoned".to_string())?;
        if let Ok(mut cancellations) = CANCELLATIONS.lock() {
            cancellations.remove(&invocation_id);
        }
        if invocation_cancel_path.exists() {
            let _ = fs::remove_file(&invocation_cancel_path);
        }
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Extension store lock is poisoned".to_string())?;
        let _registry_lock = self.acquire_registry_lock()?;
        let mut state = self.load_registry()?;
        let record = state
            .records
            .get_mut(&request.extension_id)
            .ok_or_else(|| "Extension was removed during invocation".to_string())?;
        let lease = record
            .active_invocations
            .remove(&invocation_id)
            .ok_or_else(|| "Durable invocation lease disappeared during execution".to_string())?;
        if lease.request_sha256 != request_sha256 || lease.version != version {
            return Err("Durable invocation lease changed during execution".to_string());
        }
        if record.active_version != version {
            return Err(
                "Extension version changed during invocation; result was discarded".to_string(),
            );
        }
        record.health.last_invocation_at_ms = Some(now_ms());
        match execution {
            Ok(execution) => {
                record.health.undeclared_attempts = record
                    .health
                    .undeclared_attempts
                    .saturating_add(execution.host.undeclared_attempts);
                for row in execution.host.logs {
                    record.logs.push(row);
                }
                for (name, value) in execution.host.telemetry_values {
                    push_log(
                        record,
                        "info",
                        &format!("Telemetry {name}={value}"),
                        Some(&invocation_id),
                    );
                }
                if record.logs.len() > MAX_LOG_ROWS {
                    record.logs.drain(..record.logs.len() - MAX_LOG_ROWS);
                }
                match execution.outcome {
                    Ok(output_json) => {
                        if output_json
                            .len()
                            .saturating_add(execution.host.emitted_output_bytes)
                            > MAX_OUTPUT_BYTES
                            || serde_json::from_str::<serde_json::Value>(&output_json).is_err()
                        {
                            record_invocation_failure(
                                record,
                                "Guest returned invalid or oversized JSON output",
                                &invocation_id,
                            );
                            if record.health.state == HealthState::ProtectiveDisabled {
                                atomic_write(
                                    &self.extension_cancel_path(&request.extension_id)?,
                                    b"cancel\n",
                                )?;
                            }
                            remember_invocation(
                                record,
                                &invocation_id,
                                &request_sha256,
                                &version,
                                None,
                                Some("Guest returned invalid or oversized JSON output".to_string()),
                            );
                            self.write_registry_locked(&state)?;
                            return Err(
                                "Guest returned invalid or oversized JSON output".to_string()
                            );
                        }
                        if record.health.enabled
                            && record.health.running
                            && record.health.state != HealthState::ProtectiveDisabled
                        {
                            record.health.consecutive_failures = 0;
                            record.health.state = HealthState::Healthy;
                            record.health.last_error = None;
                        }
                        record.private_state = execution.host.private_state;
                        record.last_events = execution.host.events.clone();
                        record.last_tool_result = execution.host.tool_result.clone();
                        push_log(record, "info", "Invocation completed", Some(&invocation_id));
                        let result = InvocationResult {
                            invocation_id: invocation_id.clone(),
                            output_json,
                            duration_ms: execution.duration_ms,
                            fuel_consumed: execution.fuel_consumed,
                            emitted_events: execution.host.events,
                            tool_result: execution.host.tool_result,
                            written_artifact_ids: execution
                                .host
                                .written_artifact_ids
                                .into_iter()
                                .collect(),
                        };
                        remember_invocation(
                            record,
                            &invocation_id,
                            &request_sha256,
                            &version,
                            Some(result.clone()),
                            None,
                        );
                        self.write_registry_locked(&state)?;
                        Ok(result)
                    }
                    Err(error) => {
                        record_invocation_failure(record, &error, &invocation_id);
                        if record.health.state == HealthState::ProtectiveDisabled {
                            atomic_write(
                                &self.extension_cancel_path(&request.extension_id)?,
                                b"cancel\n",
                            )?;
                        }
                        remember_invocation(
                            record,
                            &invocation_id,
                            &request_sha256,
                            &version,
                            None,
                            Some(error.clone()),
                        );
                        self.write_registry_locked(&state)?;
                        Err(error)
                    }
                }
            }
            Err(error) => {
                record_invocation_failure(record, &error, &invocation_id);
                if record.health.state == HealthState::ProtectiveDisabled {
                    atomic_write(
                        &self.extension_cancel_path(&request.extension_id)?,
                        b"cancel\n",
                    )?;
                }
                remember_invocation(
                    record,
                    &invocation_id,
                    &request_sha256,
                    &version,
                    None,
                    Some(error.clone()),
                );
                self.write_registry_locked(&state)?;
                Err(error)
            }
        }
    }

    fn runtime_host(
        &self,
        manifest: ExtensionManifest,
        grants: Vec<PermissionGrant>,
        config: BTreeMap<String, serde_json::Value>,
        invocation_id: String,
        cancellation: CancellationToken,
        cancellation_markers: Vec<PathBuf>,
        configured_secret_slots: BTreeSet<String>,
        input_artifact_ids: BTreeSet<String>,
        private_state: BTreeMap<String, Vec<u8>>,
    ) -> Result<RuntimeHost, String> {
        let artifact_store =
            ArtifactStore::with_max_blob_size(&self.artifact_root, MAX_ARTIFACT_READ_BYTES as u64)
                .map_err(|error| format!("Cannot open artifact store: {error}"))?;
        let mut builder = WasiCtxBuilder::new();
        builder
            .allow_tcp(false)
            .allow_udp(false)
            .allow_ip_name_lookup(false)
            .max_random_size(MAX_RANDOM_BYTES as u64);
        Ok(RuntimeHost {
            table: ResourceTable::new(),
            wasi: builder.build(),
            limits: AggregateStoreLimits {
                inner: StoreLimitsBuilder::new()
                    .memory_size(DEFAULT_MEMORY_BYTES)
                    .table_elements(100_000)
                    .instances(32)
                    .tables(32)
                    .memories(8)
                    .trap_on_grow_failure(true)
                    .build(),
                total_memory_bytes: 0,
                pending_memory_growth: 0,
            },
            extension_id: manifest.extension_id.clone(),
            invocation_id,
            manifest,
            grants,
            config,
            configured_secret_slots,
            input_artifact_ids,
            written_artifact_ids: BTreeSet::new(),
            artifact_store,
            private_state,
            logs: Vec::new(),
            events: Vec::new(),
            tool_result: None,
            emitted_output_bytes: 0,
            telemetry_values: BTreeMap::new(),
            undeclared_attempts: 0,
            cancellation,
            cancellation_markers,
            model_hub: self.model_hub.clone(),
        })
    }

    async fn execute_component(
        &self,
        component_bytes: &[u8],
        host: RuntimeHost,
        call: Option<(&str, &str)>,
    ) -> Result<RuntimeExecution, String> {
        let started = std::time::Instant::now();
        let cancellation = host.cancellation.clone();
        let deadline_reached = Arc::new(AtomicBool::new(false));
        let engine = self.engine.clone();
        let timer_token = cancellation.clone();
        let timer_deadline = deadline_reached.clone();
        let cancellation_markers = host.cancellation_markers.clone();
        let timer = tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(DEFAULT_TIMEOUT_MS);
            loop {
                if cancellation_markers.iter().any(|path| path.exists()) {
                    timer_token.cancel();
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    timer_deadline.store(true, Ordering::Release);
                    break;
                }
                tokio::select! {
                    _ = timer_token.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(1)) => {},
                }
            }
            engine.increment_epoch();
        });
        let component = match tokio::time::timeout(
            Duration::from_millis(DEFAULT_TIMEOUT_MS),
            self.compiled_component(component_bytes),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err("Component compilation exceeded its wall-clock timeout".to_string()),
        };
        let component = match component {
            Ok(component) if !cancellation.is_cancelled() => component,
            Ok(_) => {
                timer.abort();
                return Err(CANCELLED_ERROR.to_string());
            }
            Err(error) => {
                timer.abort();
                return Err(error);
            }
        };
        if deadline_reached.load(Ordering::Acquire) {
            timer.abort();
            return Err(TIMEOUT_ERROR.to_string());
        }
        let mut linker = Linker::<RuntimeHost>::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)
            .map_err(|error| format!("Cannot link restricted WASI: {error}"))?;
        bindings::little_monkey::extension::host::add_to_linker::<_, HasSelf<_>>(
            &mut linker,
            |state| state,
        )
        .map_err(|error| format!("Cannot link extension host API: {error}"))?;
        let mut store = Store::new(&self.engine, host);
        store.limiter(|host| &mut host.limits);
        store
            .set_fuel(self.fuel)
            .map_err(|error| format!("Cannot set extension fuel: {error}"))?;
        store
            .fuel_async_yield_interval(Some(FUEL_YIELD_INTERVAL))
            .map_err(|error| format!("Cannot configure extension fuel yielding: {error}"))?;
        let trap_deadline = deadline_reached.clone();
        let epoch_deadline = deadline_reached.clone();
        let epoch_cancellation = cancellation.clone();
        store.epoch_deadline_callback(move |_| {
            Ok(
                if epoch_deadline.load(Ordering::Acquire) || epoch_cancellation.is_cancelled() {
                    UpdateDeadline::Interrupt
                } else {
                    UpdateDeadline::Continue(1)
                },
            )
        });
        store.set_epoch_deadline(1);
        let remaining_ms = DEFAULT_TIMEOUT_MS
            .saturating_sub(started.elapsed().as_millis().try_into().unwrap_or(u64::MAX));
        let mut outcome = tokio::time::timeout(
            Duration::from_millis(remaining_ms.saturating_add(1_000)),
            async {
                let instance =
                    bindings::Extension::instantiate_async(&mut store, &component, &linker)
                        .await
                        .map_err(|error| format!("Component instantiation failed: {error}"))?;
                match call {
                    Some((capability_id, input_json)) => instance
                        .little_monkey_extension_guest()
                        .call_run(&mut store, capability_id, input_json)
                        .await
                        .map_err(|error| {
                            classify_guest_stop(&error, trap_deadline.load(Ordering::Acquire))
                        })?,
                    None => Ok(String::new()),
                }
            },
        )
        .await
        .unwrap_or_else(|_| Err(TIMEOUT_ERROR.to_string()));
        timer.abort();
        // A guest that never reached a trap — one blocked in a host call, or
        // one that answered in the same instant a cancel landed — still ends
        // as cancelled, because the caller asked for it to stop.
        //
        // A guest that spent its whole fuel budget is the one exception. Fuel
        // is the guest's own ceiling and it had already stopped when the cancel
        // arrived, so relabelling it would both lie about what happened and
        // let a real runaway escape the trap counters that protectively
        // disable it.
        if !matches!(&outcome, Err(error) if error == FUEL_EXHAUSTED_ERROR) {
            if cancellation.is_cancelled()
                || store
                    .data()
                    .cancellation_markers
                    .iter()
                    .any(|path| path.exists())
            {
                outcome = Err(CANCELLED_ERROR.to_string());
            } else if deadline_reached.load(Ordering::Acquire) {
                outcome = Err(TIMEOUT_ERROR.to_string());
            }
        }
        let remaining = store.get_fuel().unwrap_or(0);
        let fuel_consumed = self.fuel.saturating_sub(remaining);
        let duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        Ok(RuntimeExecution {
            outcome,
            host: store.into_data(),
            fuel_consumed,
            duration_ms,
        })
    }

    async fn compiled_component(&self, component_bytes: &[u8]) -> Result<Component, String> {
        let digest = sha256_bytes(component_bytes);
        if let Some(component) = COMPONENT_CACHE
            .lock()
            .map_err(|_| "Compiled-component cache is poisoned".to_string())?
            .get(&digest)
            .cloned()
        {
            return Ok(component);
        }
        let compilation_permit = COMPONENT_COMPILATION_GATE
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "Component compilation gate is closed".to_string())?;
        if let Some(component) = COMPONENT_CACHE
            .lock()
            .map_err(|_| "Compiled-component cache is poisoned".to_string())?
            .get(&digest)
            .cloned()
        {
            return Ok(component);
        }
        let engine = self.engine.clone();
        let bytes = component_bytes.to_vec();
        let cache_digest = digest.clone();
        tokio::task::spawn_blocking(move || {
            // Keep this permit in the blocking task even if the async caller
            // times out and drops its JoinHandle. Only one bounded compiler can
            // consume host CPU/memory at a time.
            let _compilation_permit = compilation_permit;
            let component = Component::new(&engine, &bytes)
                .map_err(|error| format!("Component compilation failed: {error}"))?;
            let mut cache = COMPONENT_CACHE
                .lock()
                .map_err(|_| "Compiled-component cache is poisoned".to_string())?;
            if cache.len() >= MAX_CACHED_COMPONENTS {
                if let Some(oldest) = cache.keys().next().cloned() {
                    cache.remove(&oldest);
                }
            }
            cache.insert(cache_digest, component.clone());
            Ok(component)
        })
        .await
        .map_err(|error| format!("Component compilation task failed: {error}"))?
    }
}

pub fn cancel(invocation_id: &str) -> Result<bool, String> {
    validate_id("invocation id", invocation_id)?;
    let cancellations = CANCELLATIONS
        .lock()
        .map_err(|_| "Extension cancellation registry is poisoned".to_string())?;
    if let Some(active) = cancellations.get(invocation_id) {
        active.token.cancel();
        Ok(true)
    } else {
        Ok(false)
    }
}

pub fn cancel_all() -> Result<usize, String> {
    let cancellations = CANCELLATIONS
        .lock()
        .map_err(|_| "Extension cancellation registry is poisoned".to_string())?;
    for active in cancellations.values() {
        active.token.cancel();
    }
    Ok(cancellations.len())
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------
//
// Some subsystems are not request/response. A live phone call and a streaming
// completion both run for as long as they run, exchanging chunks in both
// directions, and neither can be expressed as one invocation that returns a
// value.
//
// The guest ABI stays one-shot anyway. A session is *host* state: this module
// owns the session table, the identity, the sequence number, the deadline and
// the guest's own scratch state, and each step is an ordinary sandboxed
// invocation carrying the next event. That keeps every property a single
// invocation already has — fuel, memory ceiling, wall timeout, cancellation,
// trap isolation, immutable version binding — and adds no way for a guest to
// hold a resource across the gap. A guest that traps loses its session, not
// the host; a guest that is updated mid-session cannot have the rest of the
// session silently answered by different code, because every step re-checks
// the pinned version.
//
// Sessions are in-memory only. A live call and a live completion cannot
// survive a restart in any case, so persisting them would only create stale
// rows that outlive the thing they described.

/// How many sessions may be open at once across every extension.
pub const MAX_EXTENSION_SESSIONS: usize = 16;
/// The largest scratch state a guest may carry between the steps of one
/// session. Deliberately far below the private-state ceiling: this is echoed
/// into every step's input and back out of every step's output, so it is paid
/// for twice per chunk.
pub const MAX_SESSION_STATE_BYTES: usize = 64 * 1024;
/// The largest single event, in either direction.
pub const MAX_SESSION_EVENT_BYTES: usize = 256 * 1024;
/// How long one session may stay open. A phone call and a completion are both
/// well inside this; a session that reaches it is one nobody closed.
pub const MAX_SESSION_LIFETIME_MS: u64 = 60 * 60 * 1000;
/// How many events one step may return.
pub const MAX_SESSION_STEP_EVENTS: usize = 256;

/// The immutable identity a session is pinned to for its whole life.
///
/// Carrying the version and the manifest digest — not only the ids — is what
/// makes an update or a rollback mid-session a hard failure rather than a
/// silent redirection to different code.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionBinding {
    pub extension_id: String,
    pub version: String,
    pub kind: CapabilityKind,
    pub capability_id: String,
    pub manifest_sha256: String,
}

/// One event a guest emitted during a step. `kind` is capability-defined
/// (`chunk`, `transcript`, `audio`, …); `payload` is bounded JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionEvent {
    pub kind: String,
    pub payload: serde_json::Value,
}

/// What one step produced.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionStep {
    pub session_id: String,
    pub seq: u64,
    pub events: Vec<SessionEvent>,
    /// The guest asked for the session to end. The host closes it before
    /// returning, so a caller never has to remember to.
    pub done: bool,
    /// Artifacts this step wrote, as the host recorded them — the ownership
    /// proof a consumer needs before reading audio or an attachment out of the
    /// store.
    pub written_artifact_ids: Vec<String>,
}

/// One event a trusted host subsystem feeds into a session, together with the
/// artifacts that subsystem is granting the guest for this step alone.
///
/// The two halves are deliberately separate values. An artifact id that
/// appears inside `event` is *data* — a caller's audio clip is named there so
/// the guest knows which of its grants to read — and naming it grants nothing.
/// Authority comes only from `input_artifact_ids`, which is reachable only
/// from Rust: this type has no `Deserialize`, so no channel payload, model
/// output, connector document or guest reply can ever produce one. A host
/// subsystem that means to hand over bytes says so in a second place, in code,
/// beside the artifact it created itself.
#[derive(Debug, Clone, Default)]
pub struct SessionInput {
    event: serde_json::Value,
    input_artifact_ids: Vec<String>,
}

impl SessionInput {
    /// An event carrying no artifact authority — the common case.
    pub fn event(event: serde_json::Value) -> Self {
        Self {
            event,
            input_artifact_ids: Vec::new(),
        }
    }

    /// Grant this step read access to exactly these host-created artifacts.
    ///
    /// Only ever called with ids the caller itself just wrote or already owns.
    /// Passing through an id that arrived from outside the host would defeat
    /// the whole point of the split.
    #[must_use]
    pub fn reading_artifacts(mut self, artifact_ids: Vec<String>) -> Self {
        self.input_artifact_ids = artifact_ids;
        self
    }
}

impl From<serde_json::Value> for SessionInput {
    fn from(event: serde_json::Value) -> Self {
        Self::event(event)
    }
}

/// What a guest returns from a session step.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuestSessionStep {
    #[serde(default)]
    events: Vec<SessionEvent>,
    /// Opaque scratch the host stores and hands back on the next step. The
    /// host never interprets it; it only bounds it.
    #[serde(default)]
    state: Option<serde_json::Value>,
    #[serde(default)]
    done: bool,
}

struct OpenSession {
    binding: SessionBinding,
    guest_state: Option<serde_json::Value>,
    seq: u64,
    deadline_at_ms: u64,
    /// The invocation id of the step currently running, so a close or a
    /// cancel reaches into the sandbox rather than waiting for it.
    active_invocation: Option<String>,
}

static SESSIONS: LazyLock<Mutex<HashMap<String, OpenSession>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn sessions_locked() -> Result<std::sync::MutexGuard<'static, HashMap<String, OpenSession>>, String>
{
    SESSIONS
        .lock()
        .map_err(|_| "Extension session registry is poisoned".to_string())
}

/// Cancel the in-flight step of `session_id` and forget the session.
///
/// Idempotent: closing a session that already ended is the normal path out of
/// a `done` step and out of every error handler, so it must not itself fail.
pub fn close_session(session_id: &str) -> Result<bool, String> {
    validate_id("session id", session_id)?;
    let mut sessions = sessions_locked()?;
    match sessions.remove(session_id) {
        Some(session) => {
            if let Some(invocation_id) = session.active_invocation {
                let _ = cancel(&invocation_id);
            }
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Close every open session. Called on shutdown alongside [`cancel_all`].
pub fn close_all_sessions() -> Result<usize, String> {
    let mut sessions = sessions_locked()?;
    for session in sessions.values() {
        if let Some(invocation_id) = &session.active_invocation {
            let _ = cancel(invocation_id);
        }
    }
    let count = sessions.len();
    sessions.clear();
    Ok(count)
}

/// The binding a session is pinned to, or `None` if it is not open.
pub fn session_binding(session_id: &str) -> Result<Option<SessionBinding>, String> {
    validate_id("session id", session_id)?;
    Ok(sessions_locked()?
        .get(session_id)
        .map(|session| session.binding.clone()))
}

impl ExtensionManager {
    /// Open a session against the extension that owns `capability_id`.
    ///
    /// `expected_extension_id` is the owner the *caller* persisted — a
    /// selection made in settings, or a stack's recorded provider — never a
    /// value a model or a guest produced. Resolving without it would let a
    /// later install inherit an uninstalled provider's capability id.
    pub async fn open_session(
        &self,
        kind: CapabilityKind,
        expected_extension_id: &str,
        capability_id: &str,
        open_input: impl Into<SessionInput>,
    ) -> Result<SessionStep, String> {
        validate_id("extension id", expected_extension_id)?;
        let owner = self.resolve_active_capability(kind, capability_id)?;
        if owner.extension_id != expected_extension_id {
            return Err(format!(
                "Capability owner changed from '{expected_extension_id}' to '{}'; select the provider again",
                owner.extension_id
            ));
        }
        let detail = self.inspect(&owner.extension_id)?;
        let binding = SessionBinding {
            extension_id: owner.extension_id.clone(),
            version: owner.version.clone(),
            kind,
            capability_id: owner.capability_id.clone(),
            manifest_sha256: detail.trust.manifest_sha256.clone(),
        };
        let session_id = format!("xsession-{}", uuid::Uuid::new_v4().simple());
        {
            let mut sessions = sessions_locked()?;
            sessions.retain(|_, session| session.deadline_at_ms > now_ms());
            if sessions.len() >= MAX_EXTENSION_SESSIONS {
                return Err(format!(
                    "At most {MAX_EXTENSION_SESSIONS} extension sessions may be open at once"
                ));
            }
            sessions.insert(
                session_id.clone(),
                OpenSession {
                    binding,
                    guest_state: None,
                    seq: 0,
                    deadline_at_ms: now_ms().saturating_add(MAX_SESSION_LIFETIME_MS),
                    active_invocation: None,
                },
            );
        }
        match self
            .run_session_step(&session_id, "open", open_input.into())
            .await
        {
            Ok(step) => Ok(step),
            Err(error) => {
                let _ = close_session(&session_id);
                Err(error)
            }
        }
    }

    /// Feed one event into an open session.
    pub async fn session_send(
        &self,
        session_id: &str,
        event: impl Into<SessionInput>,
    ) -> Result<SessionStep, String> {
        self.run_session_step(session_id, "event", event.into())
            .await
    }

    /// Tell the guest the session is ending, then close it either way.
    ///
    /// The final step is best-effort: a guest that traps on close still loses
    /// its session, because the host — not the guest — owns the table.
    pub async fn session_close(&self, session_id: &str) -> Result<SessionStep, String> {
        let result = self
            .run_session_step(session_id, "close", SessionInput::default())
            .await;
        let _ = close_session(session_id);
        result
    }

    async fn run_session_step(
        &self,
        session_id: &str,
        phase: &str,
        input: SessionInput,
    ) -> Result<SessionStep, String> {
        validate_id("session id", session_id)?;
        let SessionInput {
            event,
            input_artifact_ids,
        } = input;
        let encoded_event = serde_json::to_string(&event)
            .map_err(|error| format!("Session event is not encodable: {error}"))?;
        if encoded_event.len() > MAX_SESSION_EVENT_BYTES {
            return Err(format!(
                "A session event may not exceed {MAX_SESSION_EVENT_BYTES} bytes"
            ));
        }
        let (binding, seq, guest_state) = {
            let mut sessions = sessions_locked()?;
            let session = sessions
                .get_mut(session_id)
                .ok_or_else(|| "Extension session is not open".to_string())?;
            if session.deadline_at_ms <= now_ms() {
                return Err("Extension session expired".to_string());
            }
            if session.active_invocation.is_some() {
                return Err("Extension session already has a step in flight".to_string());
            }
            session.seq = session.seq.saturating_add(1);
            (
                session.binding.clone(),
                session.seq,
                session.guest_state.clone(),
            )
        };
        let invocation_id = format!("{session_id}-{seq}");
        {
            let mut sessions = sessions_locked()?;
            let session = sessions
                .get_mut(session_id)
                .ok_or_else(|| "Extension session closed while starting a step".to_string())?;
            session.active_invocation = Some(invocation_id.clone());
        }
        let input_json = serde_json::json!({
            "session": {
                "id": session_id,
                "seq": seq,
                "phase": phase,
                "capability_id": binding.capability_id,
            },
            "state": guest_state,
            "event": event,
        })
        .to_string();
        let outcome = self
            .invoke(InvocationRequest {
                extension_id: binding.extension_id.clone(),
                capability_id: binding.capability_id.clone(),
                input_json,
                invocation_id: Some(invocation_id.clone()),
                // The grants this step's trusted call site attached, and only
                // those. `event` was already serialized into `input_json`
                // above; nothing reads an artifact id back out of it.
                input_artifact_ids,
                expected_kind: Some(binding.kind),
                expected_version: Some(binding.version.clone()),
            })
            .await;
        {
            let mut sessions = sessions_locked()?;
            if let Some(session) = sessions.get_mut(session_id) {
                session.active_invocation = None;
            }
        }
        let result = match outcome {
            Ok(result) => result,
            Err(error) => {
                // A failed step ends the session. Half a call and half a
                // completion are both worse than a clean failure the caller
                // can report, and leaving the row would let the next send
                // resume against a guest that has already failed.
                let _ = close_session(session_id);
                return Err(error);
            }
        };
        let step: GuestSessionStep = match serde_json::from_str(&result.output_json) {
            Ok(step) => step,
            Err(error) => {
                let _ = close_session(session_id);
                return Err(format!("Session step returned invalid output: {error}"));
            }
        };
        if step.events.len() > MAX_SESSION_STEP_EVENTS {
            let _ = close_session(session_id);
            return Err(format!(
                "A session step may emit at most {MAX_SESSION_STEP_EVENTS} events"
            ));
        }
        for event in &step.events {
            validate_id("session event kind", &event.kind)?;
        }
        if let Some(state) = &step.state {
            let encoded = serde_json::to_string(state).map_err(|error| error.to_string())?;
            if encoded.len() > MAX_SESSION_STATE_BYTES {
                let _ = close_session(session_id);
                return Err(format!(
                    "Session state may not exceed {MAX_SESSION_STATE_BYTES} bytes"
                ));
            }
        }
        {
            let mut sessions = sessions_locked()?;
            if let Some(session) = sessions.get_mut(session_id) {
                session.guest_state = step.state;
            }
        }
        if step.done {
            let _ = close_session(session_id);
        }
        Ok(SessionStep {
            session_id: session_id.to_string(),
            seq,
            events: step.events,
            done: step.done,
            written_artifact_ids: result.written_artifact_ids,
        })
    }
}

fn invocation_request_sha256(request: &InvocationRequest) -> Result<String, String> {
    let mut artifact_ids = request.input_artifact_ids.clone();
    artifact_ids.sort();
    let mut value = serde_json::json!({
        "schema": 1,
        "extension_id": request.extension_id,
        "capability_id": request.capability_id,
        "input_json": request.input_json,
        "input_artifact_ids": artifact_ids,
        "expected_kind": request.expected_kind,
        "expected_version": request.expected_version,
    });
    canonicalize_json(&mut value);
    serde_json::to_vec(&value)
        .map(|bytes| sha256_bytes(&bytes))
        .map_err(|error| format!("Cannot encode invocation identity: {error}"))
}

fn remember_invocation(
    record: &mut InstalledRecord,
    invocation_id: &str,
    request_sha256: &str,
    version: &str,
    result: Option<InvocationResult>,
    error: Option<String>,
) {
    record.completed_invocations.insert(
        invocation_id.to_string(),
        StoredInvocation {
            request_sha256: request_sha256.to_string(),
            version: version.to_string(),
            completed_at_ms: now_ms(),
            result,
            error: error.map(|value| bounded(&value, MAX_LOG_MESSAGE_BYTES)),
        },
    );
    while record.completed_invocations.len() > MAX_STORED_INVOCATIONS {
        let Some(oldest) = record
            .completed_invocations
            .iter()
            .min_by_key(|(_, invocation)| invocation.completed_at_ms)
            .map(|(id, _)| id.clone())
        else {
            break;
        };
        record.completed_invocations.remove(&oldest);
    }
}

/// Name the reason a guest stopped from the trap it stopped with.
///
/// Wasmtime reports both of this runtime's ceilings as traps, and a caller that
/// only saw "the component trapped" could not tell a runaway that spent its own
/// fuel from one that an outside actor interrupted. The distinction is load
/// bearing twice over: `record_invocation_failure` counts traps towards a
/// protective disable but exempts cancellations, and the cancellation tests
/// assert that an invocation ended for the reason they caused and not for one
/// that happened to arrive first.
///
/// An epoch interrupt says only that something outside the guest cut in, not
/// which something. The wall clock records itself in the deadline flag before
/// it increments the epoch, so an interrupt seen with that flag clear is the
/// cancellation path.
fn classify_guest_stop(error: &wasmtime::Error, deadline_reached: bool) -> String {
    match error.downcast_ref::<wasmtime::Trap>() {
        Some(wasmtime::Trap::OutOfFuel) => FUEL_EXHAUSTED_ERROR.to_string(),
        Some(wasmtime::Trap::Interrupt) if deadline_reached => TIMEOUT_ERROR.to_string(),
        Some(wasmtime::Trap::Interrupt) => CANCELLED_ERROR.to_string(),
        _ => format!("Component trapped: {error}"),
    }
}

fn record_invocation_failure(record: &mut InstalledRecord, error: &str, invocation_id: &str) {
    if error == CANCELLED_ERROR {
        push_log(record, "info", error, Some(invocation_id));
        return;
    }
    record.health.consecutive_failures = record.health.consecutive_failures.saturating_add(1);
    if error.contains("Component trapped")
        || error.contains("wall-clock timeout")
        || error.contains("fuel")
    {
        record.health.trap_count = record.health.trap_count.saturating_add(1);
    }
    record.health.last_error = Some(bounded(error, MAX_LOG_MESSAGE_BYTES));
    if record.health.consecutive_failures >= PROTECTIVE_DISABLE_FAILURES {
        record.health.state = HealthState::ProtectiveDisabled;
        record.health.enabled = false;
        record.health.running = false;
        push_log(
            record,
            "error",
            "Protectively disabled after three consecutive failures",
            Some(invocation_id),
        );
    } else {
        record.health.state = HealthState::Degraded;
        push_log(
            record,
            "error",
            &format!("Invocation failed: {error}"),
            Some(invocation_id),
        );
    }
}

fn write_workspace_file(root: &Path, relative: &str, bytes: &[u8]) -> Result<(), String> {
    validate_relative_path(relative)?;
    let target = root.join(relative);
    let parent = target
        .parent()
        .ok_or_else(|| "Workspace path has no parent".to_string())?
        .canonicalize()
        .map_err(|error| format!("Workspace parent does not exist: {error}"))?;
    if !parent.starts_with(root) {
        return Err("Workspace write path escapes the granted handle".to_string());
    }
    let existing_permissions = match fs::symlink_metadata(&target) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err("Workspace target is not a regular file".to_string());
            }
            Some(metadata.permissions())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("Cannot inspect workspace target: {error}")),
    };
    let temp = parent.join(format!(
        ".little-monkey-extension-{}.tmp",
        uuid::Uuid::new_v4()
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .map_err(|error| format!("Cannot stage workspace write: {error}"))?;
    let result = (|| {
        file.write_all(bytes)
            .map_err(|error| format!("Cannot write workspace file: {error}"))?;
        if let Some(permissions) = existing_permissions {
            file.set_permissions(permissions)
                .map_err(|error| format!("Cannot preserve workspace permissions: {error}"))?;
        }
        file.sync_all()
            .map_err(|error| format!("Cannot sync workspace file: {error}"))?;
        crate::m4_runtime::atomic_replace_file(&temp, &target)
            .map_err(|error| format!("Cannot publish workspace file: {error}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

fn build_engine() -> Result<Engine, String> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    config.epoch_interruption(true);
    config.cranelift_nan_canonicalization(true);
    Engine::new(&config).map_err(|error| format!("Cannot initialize extension runtime: {error}"))
}

/// A stable invocation-id suffix over the parts that identify one durable
/// call.
///
/// Native consumers that want a retried call to replay the runtime's cached
/// result — rather than run the effect a second time — need an id that is the
/// same both times and different for any other call. Length-prefixing each
/// part is what stops two different tuples hashing to one id by running their
/// fields together.
pub fn stable_invocation_suffix(parts: &[&str]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"little-monkey:extension-invocation:v1");
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part.as_bytes());
    }
    format!("{:x}", digest.finalize())[..32].to_string()
}

/// The same identifier rule the runtime enforces, for the native subsystems
/// that persist an extension/capability selection of their own. They validate
/// through this rather than inventing a second rule, so a selection that
/// stores cleanly cannot then be refused at invocation time.
pub fn validate_extension_identifier(label: &str, value: &str) -> Result<(), String> {
    validate_id(label, value)
}

fn validate_id(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 160
        || value.starts_with('-')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Err(format!("{label} must be a bounded ASCII identifier"))
    } else {
        Ok(())
    }
}

fn validate_model_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 512
        || value.bytes().any(|byte| {
            byte.is_ascii_control()
                || byte.is_ascii_whitespace()
                || !matches!(
                    byte,
                    b'A'..=b'Z'
                        | b'a'..=b'z'
                        | b'0'..=b'9'
                        | b'-'
                        | b'_'
                        | b'.'
                        | b'/'
                        | b':'
                        | b'@'
                        | b'+'
                )
        })
    {
        Err("Model id must be a bounded opaque ASCII catalog id".to_string())
    } else {
        Ok(())
    }
}

fn validate_model_target(value: &str) -> Result<(&str, &str), String> {
    let (runtime_id, model_id) = value
        .split_once(':')
        .ok_or_else(|| "Model permission scope must be 'runtime-id:model-id'".to_string())?;
    validate_id("runtime id", runtime_id)?;
    validate_model_id(model_id)?;
    Ok((runtime_id, model_id))
}

fn validate_text(label: &str, value: &str, max: usize) -> Result<(), String> {
    if value.trim().is_empty()
        || value.len() > max
        || value.contains('\0')
        || value.contains(['\r', '\n']) && max <= 256
    {
        Err(format!("Invalid {label}"))
    } else {
        Ok(())
    }
}

fn validate_version_constraint(label: &str, value: &VersionConstraint) -> Result<(), String> {
    if value
        .maximum_exclusive
        .is_some_and(|maximum| maximum <= value.minimum)
    {
        Err(format!("{label} version range is empty"))
    } else {
        Ok(())
    }
}

fn validate_compatibility(value: &Compatibility) -> Result<(), String> {
    if value
        .maximum_app_version_exclusive
        .is_some_and(|maximum| maximum <= value.minimum_app_version)
    {
        return Err("App compatibility range is empty".to_string());
    }
    if let Some(contract) = &value.contract {
        validate_version_constraint("contract", contract)?;
    }
    if value.platforms.len() > 32 || value.architectures.len() > 32 {
        return Err("Compatibility declaration exceeds its item limit".to_string());
    }
    for platform in &value.platforms {
        validate_id("platform", platform)?;
    }
    for architecture in &value.architectures {
        validate_id("architecture", architecture)?;
    }
    Ok(())
}

fn validate_provenance(value: &PackageProvenance) -> Result<(), String> {
    validate_text("provenance publisher", &value.publisher, 256)?;
    validate_text("source revision", &value.source_revision, 512)?;
    validate_install_source(&value.source)
}

fn validate_install_source(value: &InstallSource) -> Result<(), String> {
    match value {
        InstallSource::LocalFolder { canonical_path } => {
            if canonical_path.is_empty()
                || canonical_path.len() > 4096
                || canonical_path.contains('\0')
            {
                return Err("Invalid local-folder provenance".to_string());
            }
        }
        InstallSource::Git { remote, commit_sha } => {
            validate_text("Git remote", remote, 4096)?;
            if commit_sha.len() < 7
                || commit_sha.len() > 128
                || !commit_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err("Invalid Git provenance commit".to_string());
            }
        }
        InstallSource::CuratedRegistry { registry_id } => {
            validate_id("registry id", registry_id)?;
        }
    }
    Ok(())
}

fn validate_sha256(value: &str, label: &str) -> Result<(), String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Err(format!("{label} must be a 64-character SHA-256 digest"))
    } else {
        Ok(())
    }
}

fn validate_relative_path(value: &str) -> Result<(), String> {
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > 512
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                PathComponent::ParentDir | PathComponent::RootDir | PathComponent::Prefix(_)
            )
        })
    {
        Err("Bundle paths must be bounded relative paths without traversal".to_string())
    } else {
        Ok(())
    }
}

fn validate_header_name(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 80
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || is_restricted_header(value)
    {
        Err("Invalid or restricted secret auth header".to_string())
    } else {
        Ok(())
    }
}

fn is_restricted_header(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "host"
            | "content-length"
            | "connection"
            | "transfer-encoding"
            | "proxy-authorization"
            | "cookie"
            | "set-cookie"
            | "x-forwarded-for"
            | "x-forwarded-host"
    )
}

fn validate_permission_scope(permission: &PermissionDeclaration) -> Result<(), String> {
    match permission.kind {
        PermissionKind::NetworkOrigin => {
            let url = Url::parse(&permission.scope)
                .map_err(|_| "Network permission scope must be an exact origin".to_string())?;
            let host = url
                .host_str()
                .ok_or_else(|| "Network permission scope must include a host".to_string())?;
            if !matches!(url.scheme(), "https" | "http")
                || host.contains('*')
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || (url.path() != "/" && !url.path().is_empty())
                || canonical_origin(&url)? != permission.scope.trim_end_matches('/')
            {
                return Err("Network permission must be one canonical exact HTTP(S) origin".into());
            }
        }
        PermissionKind::WorkspaceRead
        | PermissionKind::WorkspaceWrite
        | PermissionKind::SecretUse
        | PermissionKind::Device
        | PermissionKind::WebhookReceive => validate_id("permission scope", &permission.scope)?,
        PermissionKind::ModelInvoke => {
            validate_model_target(&permission.scope)?;
        }
        PermissionKind::ArtifactRead => {
            if permission.scope != "invocation_inputs" {
                validate_sha256(&permission.scope, "artifact scope")?;
            }
        }
        PermissionKind::ArtifactWrite => {
            if permission.scope != "content_v1" {
                return Err("Artifact-write scope must be 'content_v1'".to_string());
            }
        }
    }
    Ok(())
}

fn validate_config_value(field: &ConfigField, value: &serde_json::Value) -> Result<(), String> {
    let valid = match field.kind {
        ConfigFieldKind::String => value.as_str().is_some_and(|text| text.len() <= 16 * 1024),
        ConfigFieldKind::Integer => value.as_i64().is_some_and(|number| {
            field.minimum.is_none_or(|minimum| number >= minimum)
                && field.maximum.is_none_or(|maximum| number <= maximum)
        }),
        ConfigFieldKind::Boolean => value.is_boolean(),
        ConfigFieldKind::Select => value
            .as_str()
            .is_some_and(|choice| field.options.iter().any(|option| option == choice)),
    };
    if valid {
        Ok(())
    } else {
        Err(format!("Invalid value for config field '{}'", field.key))
    }
}

fn resolved_config(
    manifest: &ExtensionManifest,
    values: &BTreeMap<String, serde_json::Value>,
) -> BTreeMap<String, serde_json::Value> {
    manifest
        .config_schema
        .iter()
        .filter_map(|field| {
            values
                .get(&field.key)
                .filter(|value| validate_config_value(field, value).is_ok())
                .cloned()
                .or_else(|| field.default.clone())
                .map(|value| (field.key.clone(), value))
        })
        .collect()
}

fn canonical_origin(url: &Url) -> Result<String, String> {
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?
        .to_ascii_lowercase();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "URL has no effective port".to_string())?;
    let default =
        (url.scheme() == "https" && port == 443) || (url.scheme() == "http" && port == 80);
    Ok(if default {
        format!("{}://{host}", url.scheme())
    } else {
        format!("{}://{host}:{port}", url.scheme())
    })
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, String> {
    validate_relative_path(relative)?;
    let candidate = root.join(relative);
    let canonical = candidate
        .canonicalize()
        .map_err(|error| format!("Cannot resolve bundle path '{relative}': {error}"))?;
    if !canonical.starts_with(root) {
        return Err("Bundle path escapes its source directory".to_string());
    }
    Ok(canonical)
}

fn read_regular_file(path: &Path, max_bytes: usize) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Cannot inspect '{}': {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "'{}' must be a regular non-symlink file",
            path.display()
        ));
    }
    if metadata.len() > max_bytes as u64 {
        return Err(format!("'{}' exceeds its size limit", path.display()));
    }
    let file =
        File::open(path).map_err(|error| format!("Cannot open '{}': {error}", path.display()))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("Cannot inspect opened file '{}': {error}", path.display()))?;
    if opened.len() != metadata.len() || !opened.is_file() {
        return Err(format!("'{}' changed while being opened", path.display()));
    }
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    file.take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Cannot read '{}': {error}", path.display()))?;
    if bytes.len() > max_bytes {
        return Err(format!("'{}' exceeds its size limit", path.display()));
    }
    Ok(bytes)
}

fn ensure_real_directory(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(format!("'{}' must be a real directory", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .map_err(|error| format!("Cannot create '{}': {error}", path.display()))?;
            ensure_real_directory(path)
        }
        Err(error) => Err(format!("Cannot inspect '{}': {error}", path.display())),
    }
}

fn ensure_private_directory(path: &Path) -> Result<(), String> {
    ensure_real_directory(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Cannot secure '{}': {error}", path.display()))?;
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    crate::m4_runtime::atomic_write_private(path, bytes, true)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    if value.len() % 2 != 0 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Invalid hexadecimal value".to_string());
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).map_err(|_| "Invalid hex".to_string())?;
            u8::from_str_radix(text, 16).map_err(|_| "Invalid hex".to_string())
        })
        .collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn load_extension_trust_store(app_data: &Path) -> Result<TrustStore, String> {
    let (mut trust, _, _) = signed_first_party_catalog().map_err(|error| error.to_string())?;
    let user_path = app_data.join("extensions-trust-v1.json");
    if user_path.exists() {
        let bytes = read_regular_file(&user_path, MAX_MANIFEST_BYTES)?;
        let user: TrustStore = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Invalid extension trust store: {error}"))?;
        user.validate().map_err(|error| error.to_string())?;
        for (id, root) in user.roots {
            if trust.roots.insert(id.clone(), root).is_some() {
                return Err(format!("Duplicate trust root '{id}'"));
            }
        }
    }
    Ok(trust)
}

fn verify_trust(manifest: &ExtensionManifest, trust: &TrustStore) -> Result<TrustEvidence, String> {
    let payload = manifest.signing_payload()?;
    let manifest_sha256 = sha256_bytes(&payload);
    let base = |state, reason, root: Option<String>, key: Option<String>| TrustEvidence {
        state,
        reason,
        trust_root_id: root,
        key_id: key,
        manifest_sha256: manifest_sha256.clone(),
        component_sha256: manifest.component.sha256.to_ascii_lowercase(),
    };
    let Some(signature) = &manifest.signature else {
        return Ok(base(
            TrustState::Unsigned,
            "Manifest is unsigned; local installation requires explicit approval".into(),
            None,
            None,
        ));
    };
    let Some(root) = trust.roots.get(&signature.trust_root_id) else {
        return Ok(base(
            TrustState::Untrusted,
            "Signature names a trust root that this installation does not trust".into(),
            Some(signature.trust_root_id.clone()),
            Some(signature.key_id.clone()),
        ));
    };
    let Some(key) = root.keys.get(&signature.key_id) else {
        return Ok(base(
            TrustState::Untrusted,
            "Signature names an unknown signing key".into(),
            Some(signature.trust_root_id.clone()),
            Some(signature.key_id.clone()),
        ));
    };
    let timestamp = now_ms();
    if root.publisher != manifest.publisher
        || !root
            .package_namespaces
            .iter()
            .any(|namespace| manifest.extension_id.starts_with(namespace))
        || timestamp < key.valid_from_unix_ms
        || timestamp >= key.valid_until_unix_ms
        || key
            .revoked_at_unix_ms
            .is_some_and(|revoked| revoked <= timestamp)
        || key.algorithm != signature.algorithm
    {
        return Ok(base(
            TrustState::Untrusted,
            "Signing key is not authorized for this publisher, namespace, or time".into(),
            Some(signature.trust_root_id.clone()),
            Some(signature.key_id.clone()),
        ));
    }
    let public_key = decode_hex(&key.public_key_hex)?;
    let signature_bytes = match decode_hex(&signature.signature_hex) {
        Ok(bytes) => bytes,
        Err(_) => {
            return Ok(base(
                TrustState::Invalid,
                "Signature encoding is invalid".into(),
                Some(signature.trust_root_id.clone()),
                Some(signature.key_id.clone()),
            ))
        }
    };
    let valid = RingEd25519SignatureVerifier
        .verify(
            &signature.algorithm,
            &public_key,
            &payload,
            &signature_bytes,
        )
        .unwrap_or(false);
    Ok(if valid {
        base(
            TrustState::Verified,
            "Ed25519 signature verified against a trusted publisher key".into(),
            Some(signature.trust_root_id.clone()),
            Some(signature.key_id.clone()),
        )
    } else {
        base(
            TrustState::Invalid,
            "Manifest signature verification failed".into(),
            Some(signature.trust_root_id.clone()),
            Some(signature.key_id.clone()),
        )
    })
}

fn current_trust_status(
    installed: &InstalledVersion,
    trust_store: &TrustStore,
) -> Result<(TrustEvidence, Option<String>), String> {
    let current = verify_trust(&installed.manifest, trust_store)?;
    if current.manifest_sha256 != installed.trust.manifest_sha256
        || current.component_sha256 != installed.trust.component_sha256
    {
        return Err("Installed extension trust evidence does not match its manifest".to_string());
    }
    let blocker = if current.state == TrustState::Invalid {
        Some(current.reason.clone())
    } else if installed.trust.state == TrustState::Verified && current.state != TrustState::Verified
    {
        Some(format!(
            "Previously verified publisher trust is no longer valid: {}",
            current.reason
        ))
    } else {
        None
    };
    Ok((current, blocker))
}

fn compatibility(manifest: &ExtensionManifest) -> (bool, Option<String>) {
    if !manifest.host_api.matches(EXTENSION_HOST_API_VERSION) {
        return (
            false,
            Some(format!(
                "Host API {} is outside extension range {}",
                EXTENSION_HOST_API_VERSION, manifest.host_api
            )),
        );
    }
    let app_version =
        SemanticVersion::parse(env!("CARGO_PKG_VERSION")).unwrap_or(SemanticVersion::new(0, 0, 0));
    if app_version < manifest.compatibility.minimum_app_version
        || manifest
            .compatibility
            .maximum_app_version_exclusive
            .is_some_and(|maximum| app_version >= maximum)
    {
        return (
            false,
            Some("App version is outside the manifest compatibility range".into()),
        );
    }
    let platform = std::env::consts::OS;
    let architecture = std::env::consts::ARCH;
    if !manifest.compatibility.platforms.is_empty()
        && !manifest.compatibility.platforms.contains(platform)
    {
        return (
            false,
            Some(format!("Platform '{platform}' is not supported")),
        );
    }
    if !manifest.compatibility.architectures.is_empty()
        && !manifest.compatibility.architectures.contains(architecture)
    {
        return (
            false,
            Some(format!("Architecture '{architecture}' is not supported")),
        );
    }
    (true, None)
}

fn permission_views(
    manifest: &ExtensionManifest,
    grants: &[PermissionGrant],
) -> Vec<PermissionView> {
    manifest
        .permissions
        .iter()
        .map(|permission| {
            let grant = grants
                .iter()
                .find(|grant| grant.permission_id == permission.permission_id);
            PermissionView {
                permission_id: permission.permission_id.clone(),
                kind: permission.kind,
                scope: permission.scope.clone(),
                reason: permission.reason.clone(),
                risk: permission.risk(),
                granted: grant.is_some(),
                binding_label: grant
                    .and_then(|grant| grant.binding.as_ref())
                    .map(|binding| {
                        if matches!(
                            permission.kind,
                            PermissionKind::WorkspaceRead | PermissionKind::WorkspaceWrite
                        ) {
                            Path::new(binding)
                                .file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("workspace")
                                .to_string()
                        } else {
                            permission.scope.clone()
                        }
                    }),
            }
        })
        .collect()
}

fn permission_diff(
    previous: &ExtensionManifest,
    next: &ExtensionManifest,
    previous_grants: &[PermissionGrant],
) -> PermissionDiff {
    let previous_map = previous
        .permissions
        .iter()
        .map(|permission| (permission.permission_id.as_str(), permission))
        .collect::<BTreeMap<_, _>>();
    let next_map = next
        .permissions
        .iter()
        .map(|permission| (permission.permission_id.as_str(), permission))
        .collect::<BTreeMap<_, _>>();
    let view = |permission: &PermissionDeclaration, granted: bool| PermissionView {
        permission_id: permission.permission_id.clone(),
        kind: permission.kind,
        scope: permission.scope.clone(),
        reason: permission.reason.clone(),
        risk: permission.risk(),
        granted,
        binding_label: None,
    };
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut unchanged = Vec::new();
    for (id, permission) in &next_map {
        let granted = previous_grants
            .iter()
            .any(|grant| grant.permission_id == *id);
        match previous_map.get(id) {
            Some(old) if *old == *permission => unchanged.push(view(permission, granted)),
            Some(old) => {
                removed.push(view(old, granted));
                added.push(view(permission, false));
            }
            None => added.push(view(permission, false)),
        }
    }
    for (id, permission) in previous_map {
        if !next_map.contains_key(id) {
            let granted = previous_grants
                .iter()
                .any(|grant| grant.permission_id == id);
            removed.push(view(permission, granted));
        }
    }
    PermissionDiff {
        expands_authority: !added.is_empty(),
        added,
        removed,
        unchanged,
    }
}

fn active_version(record: &InstalledRecord) -> Result<&InstalledVersion, String> {
    record
        .versions
        .get(&record.active_version)
        .ok_or_else(|| "Extension registry lost its active immutable version".to_string())
}

fn validate_dependencies(
    manifest: &ExtensionManifest,
    state: &RegistryState,
) -> Result<(), String> {
    for dependency in &manifest.dependencies {
        let record = state
            .records
            .get(&dependency.extension_id)
            .ok_or_else(|| format!("Missing extension dependency '{}'", dependency.extension_id))?;
        let active = active_version(record)?;
        if !dependency.constraint.matches(active.manifest.version) {
            return Err(format!(
                "Extension dependency '{}' does not satisfy {}",
                dependency.extension_id, dependency.constraint
            ));
        }
    }
    Ok(())
}

fn validate_dependents(
    extension_id: &str,
    candidate_version: SemanticVersion,
    state: &RegistryState,
) -> Result<(), String> {
    for record in state.records.values() {
        if record.extension_id == extension_id {
            continue;
        }
        let active = active_version(record)?;
        if let Some(dependency) = active
            .manifest
            .dependencies
            .iter()
            .find(|dependency| dependency.extension_id == extension_id)
        {
            if !dependency.constraint.matches(candidate_version) {
                return Err(format!(
                    "Version {candidate_version} would break dependent extension '{}'",
                    record.extension_id
                ));
            }
        }
    }
    Ok(())
}

fn builtin_capabilities() -> BTreeSet<(CapabilityKind, String)> {
    let mut builtins = BTreeSet::new();
    if let Some(definitions) = crate::agent_tools::tool_definitions().as_array() {
        for definition in definitions {
            if let Some(name) = definition
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(serde_json::Value::as_str)
            {
                builtins.insert((CapabilityKind::Tool, name.to_string()));
            }
        }
    }
    for channel in crate::channels::types::ChannelKind::ALL {
        builtins.insert((CapabilityKind::Channel, channel.as_str().to_string()));
    }
    for (kind, ids) in [
        (
            CapabilityKind::ModelProvider,
            &[
                "llama_cpp",
                "ollama",
                "mlx",
                "openai",
                "anthropic",
                "gemini",
                "openrouter",
            ][..],
        ),
        (
            CapabilityKind::EmbeddingProvider,
            &["local", "openai", "ollama"][..],
        ),
        (CapabilityKind::Stt, &["local_whisper", "provider"][..]),
        (CapabilityKind::Tts, &["system", "provider"][..]),
        (CapabilityKind::RealtimeVoice, &["provider"][..]),
        (
            CapabilityKind::WebSearch,
            &["web_search", "duckduckgo", "brave", "searxng"][..],
        ),
        (CapabilityKind::WebFetch, &["web_fetch"][..]),
        (
            CapabilityKind::DeviceProvider,
            &["paired_device", "companion"][..],
        ),
        (
            CapabilityKind::Connector,
            &["github", "slack", "notion", "jira", "s3"][..],
        ),
    ] {
        for id in ids {
            builtins.insert((kind, (*id).to_string()));
        }
    }
    builtins
}

fn validate_capability_collisions(
    manifest: &ExtensionManifest,
    state: &RegistryState,
) -> Result<(), String> {
    let builtins = builtin_capabilities();
    for capability in &manifest.capabilities {
        if builtins.contains(&(capability.kind, capability.capability_id.clone())) {
            return Err(format!(
                "Capability '{}' collides with a built-in capability",
                capability.capability_id
            ));
        }
        for record in state.records.values() {
            if record.extension_id == manifest.extension_id {
                continue;
            }
            let active = active_version(record)?;
            if active.manifest.capabilities.iter().any(|other| {
                other.kind == capability.kind && other.capability_id == capability.capability_id
            }) {
                return Err(format!(
                    "Capability '{}:{:?}' is already owned by '{}'",
                    capability.capability_id, capability.kind, record.extension_id
                ));
            }
        }
    }
    Ok(())
}

fn validate_registry(state: &RegistryState) -> Result<(), String> {
    let mut invocation_ids = BTreeSet::new();
    for (id, record) in &state.records {
        validate_id("extension id", id)?;
        if record.extension_id != *id
            || record.versions.is_empty()
            || record.versions.len() > 4
            || !record.versions.contains_key(&record.active_version)
            || record.previous_version.as_ref().is_some_and(|version| {
                version == &record.active_version || !record.versions.contains_key(version)
            })
        {
            return Err(format!("Invalid extension registry record '{id}'"));
        }
        for (version_key, version) in &record.versions {
            version.manifest.validate()?;
            if version.manifest.extension_id != *id
                || version.manifest.version.to_string() != *version_key
                || version.trust.state == TrustState::Invalid
            {
                return Err(format!(
                    "Invalid immutable extension version '{id}:{version_key}'"
                ));
            }
            validate_persisted_grants(&version.manifest, &version.grants)?;
            validate_install_source(&version.observed_source)?;
            validate_sha256(&version.trust.manifest_sha256, "stored manifest sha256")?;
            validate_sha256(&version.trust.component_sha256, "stored component sha256")?;
            if sha256_bytes(&version.manifest.signing_payload()?) != version.trust.manifest_sha256
                || version.manifest.component.sha256.to_ascii_lowercase()
                    != version.trust.component_sha256
            {
                return Err(format!(
                    "Stored trust evidence changed for '{id}:{version_key}'"
                ));
            }
            validate_text("trust reason", &version.trust.reason, MAX_LOG_MESSAGE_BYTES)?;
            if let Some(root) = &version.trust.trust_root_id {
                validate_id("trust root id", root)?;
            }
            if let Some(key) = &version.trust.key_id {
                validate_id("trust key id", key)?;
            }
        }
        let active = active_version(record)?;
        if record.config.len() > active.manifest.config_schema.len() {
            return Err(format!("Extension '{id}' has undeclared configuration"));
        }
        for (key, value) in &record.config {
            let field = active
                .manifest
                .config_schema
                .iter()
                .find(|field| field.key == *key)
                .ok_or_else(|| format!("Extension '{id}' has undeclared config '{key}'"))?;
            validate_config_value(field, value)?;
        }
        for field in &active.manifest.config_schema {
            if field.required && !record.config.contains_key(&field.key) {
                return Err(format!(
                    "Extension '{id}' is missing required config '{}'",
                    field.key
                ));
            }
        }
        if record.configured_secret_slots.iter().any(|slot| {
            !active
                .manifest
                .secret_slots
                .iter()
                .any(|declared| declared.slot_id == *slot)
        }) {
            return Err(format!("Extension '{id}' has undeclared secret metadata"));
        }
        if record.logs.len() > MAX_LOG_ROWS
            || record.completed_invocations.len() > MAX_STORED_INVOCATIONS
            || record.active_invocations.len() > MAX_STORED_INVOCATIONS
            || record
                .private_state
                .iter()
                .map(|(key, value)| key.len() + value.len())
                .sum::<usize>()
                > MAX_PRIVATE_STATE_BYTES
        {
            return Err(format!("Extension '{id}' exceeds a persisted-state limit"));
        }
        for (key, value) in &record.private_state {
            validate_id("private state key", key)?;
            if value.len() > MAX_PRIVATE_STATE_BYTES {
                return Err(format!(
                    "Extension '{id}' has an oversized private-state value"
                ));
            }
        }
        for row in &record.logs {
            if !matches!(
                row.level.as_str(),
                "trace" | "debug" | "info" | "warn" | "error"
            ) || row.message.len() > MAX_LOG_MESSAGE_BYTES
            {
                return Err(format!("Extension '{id}' has an invalid persisted log row"));
            }
            if let Some(invocation_id) = &row.invocation_id {
                validate_id("log invocation id", invocation_id)?;
            }
        }
        validate_persisted_output(record.last_tool_result.as_deref(), &record.last_events)?;
        for (invocation_id, invocation) in &record.completed_invocations {
            validate_id("stored invocation id", invocation_id)?;
            if !invocation_ids.insert(invocation_id) {
                return Err("Persisted invocation ids must be globally unique".to_string());
            }
            validate_sha256(
                &invocation.request_sha256,
                "stored invocation request sha256",
            )?;
            SemanticVersion::parse(&invocation.version).map_err(|error| error.to_string())?;
            if invocation.result.is_some() == invocation.error.is_some()
                || invocation
                    .error
                    .as_ref()
                    .is_some_and(|error| error.len() > MAX_LOG_MESSAGE_BYTES)
            {
                return Err(format!("Extension '{id}' has an invalid stored invocation"));
            }
            if let Some(result) = &invocation.result {
                if result.invocation_id != *invocation_id
                    || result.fuel_consumed > DEFAULT_FUEL
                    || serde_json::from_str::<serde_json::Value>(&result.output_json).is_err()
                {
                    return Err(format!("Extension '{id}' has an invalid stored result"));
                }
                validate_persisted_output(result.tool_result.as_deref(), &result.emitted_events)?;
                let emitted = result
                    .emitted_events
                    .iter()
                    .map(|(kind, payload)| kind.len().saturating_add(payload.len()))
                    .sum::<usize>()
                    .saturating_add(result.tool_result.as_ref().map_or(0, String::len));
                if result.output_json.len().saturating_add(emitted) > MAX_OUTPUT_BYTES {
                    return Err(format!("Extension '{id}' has an oversized stored result"));
                }
            }
        }
        for (invocation_id, invocation) in &record.active_invocations {
            validate_id("active invocation id", invocation_id)?;
            if !invocation_ids.insert(invocation_id) {
                return Err("Persisted invocation ids must be globally unique".to_string());
            }
            validate_sha256(
                &invocation.request_sha256,
                "active invocation request sha256",
            )?;
            SemanticVersion::parse(&invocation.version).map_err(|error| error.to_string())?;
            if !record.versions.contains_key(&invocation.version)
                || invocation.deadline_at_ms == 0
                || invocation.deadline_at_ms
                    > now_ms()
                        .saturating_add(DEFAULT_TIMEOUT_MS)
                        .saturating_add(5 * 60_000)
            {
                return Err(format!("Extension '{id}' has an invalid active invocation"));
            }
        }
        if record.health.running
            && (!record.health.enabled
                || !record.health.validated
                || !matches!(
                    record.health.state,
                    HealthState::Healthy | HealthState::Degraded
                ))
            || matches!(
                record.health.state,
                HealthState::Healthy | HealthState::Degraded
            ) && !record.health.running
            || record.health.state == HealthState::ProtectiveDisabled
                && (record.health.enabled || record.health.running)
            || record
                .health
                .last_error
                .as_ref()
                .is_some_and(|error| error.len() > MAX_LOG_MESSAGE_BYTES)
        {
            return Err(format!("Extension '{id}' has inconsistent runtime health"));
        }
    }
    for record in state.records.values() {
        validate_capability_collisions(&active_version(record)?.manifest, state)?;
    }
    Ok(())
}

fn validate_persisted_output(
    tool_result: Option<&str>,
    events: &[(String, String)],
) -> Result<(), String> {
    if events.len() > 128 {
        return Err("Persisted extension event limit exceeded".to_string());
    }
    let mut total = 0usize;
    if let Some(result) = tool_result {
        if serde_json::from_str::<serde_json::Value>(result).is_err() {
            return Err("Persisted extension tool result is invalid JSON".to_string());
        }
        total = result.len();
    }
    for (kind, payload) in events {
        validate_id("persisted event kind", kind)?;
        if serde_json::from_str::<serde_json::Value>(payload).is_err() {
            return Err("Persisted extension event is invalid JSON".to_string());
        }
        total = total
            .saturating_add(kind.len())
            .saturating_add(payload.len());
    }
    if total > MAX_OUTPUT_BYTES {
        return Err("Persisted extension emitted output exceeds 4 MiB".to_string());
    }
    Ok(())
}

fn detail_blockers(
    record: &InstalledRecord,
    active: &InstalledVersion,
    state: &RegistryState,
    trust: &TrustEvidence,
    trust_blocker: Option<&str>,
    compatible: bool,
    compatibility_reason: Option<&str>,
) -> Vec<String> {
    let mut blockers = Vec::new();
    if !compatible {
        blockers.push(
            compatibility_reason
                .unwrap_or("Incompatible extension")
                .to_string(),
        );
    }
    if trust.state == TrustState::Invalid {
        blockers.push(trust.reason.clone());
    }
    if let Some(blocker) = trust_blocker {
        if !blockers.iter().any(|existing| existing == blocker) {
            blockers.push(blocker.to_string());
        }
    }
    if let Err(error) = validate_dependencies(&active.manifest, state) {
        blockers.push(error);
    }
    if record.health.state == HealthState::ProtectiveDisabled {
        blockers.push("Extension was protectively disabled after repeated failures".into());
    }
    blockers
}

fn canonicalize_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let original = std::mem::take(map);
            let mut entries = original.into_iter().collect::<Vec<_>>();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (key, mut child) in entries {
                canonicalize_json(&mut child);
                map.insert(key, child);
            }
        }
        serde_json::Value::Array(values) => values.iter_mut().for_each(canonicalize_json),
        _ => {}
    }
}

/// Bundle fixtures shared by this module's own tests and by the capability
/// integration tests that drive each native subsystem end to end.
///
/// They build a real Component Model component — not a stub the runtime would
/// treat specially — so a test that installs one exercises verification,
/// instantiation, fuel, the output cap and the lifecycle exactly as a
/// published extension does.
#[cfg(test)]
pub(crate) mod test_fixtures {
    use super::*;

    /// Serializes every test that touches the runtime's process-global state.
    ///
    /// Cancellation, the invocation gate and the session table are per-process
    /// by design — they have to be, because a second process must not be able
    /// to cancel this one's invocation. Tests that *assert* on that state are
    /// therefore mutually exclusive with every other test that invokes an
    /// extension, whichever module they live in: two of them interleaving is
    /// not a race in the product, it is two tests disagreeing about what "the
    /// currently active invocations" means.
    ///
    /// Poisoning is ignored: a test that panicked while holding this has
    /// already reported its own failure, and taking the rest of the suite down
    /// with it would hide whatever else is broken.
    pub(crate) fn runtime_guard() -> std::sync::MutexGuard<'static, ()> {
        static RUNTIME_LOCK: Mutex<()> = Mutex::new(());
        RUNTIME_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub(crate) struct TestRoot(pub(crate) PathBuf);

    impl TestRoot {
        pub(crate) fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "little-monkey-executable-extensions-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A component whose `run` export answers with `output`, whatever it is
    /// asked. `body` is extra core WASM executed first, which is how the
    /// trap/fuel/memory fixtures misbehave on purpose.
    pub(crate) fn component_wat(output: &str, body: &str) -> Vec<u8> {
        let escaped = output
            .bytes()
            .flat_map(|byte| format!("\\{:02x}", byte).into_bytes())
            .map(char::from)
            .collect::<String>();
        wat::parse_str(format!(
            r#"(component
              (core module $m
                (memory (export "memory") 2)
                (global $heap (mut i32) (i32.const 4096))
                (data (i32.const 1024) "{escaped}")
                (func $realloc (export "realloc")
                  (param i32 i32 i32 i32) (result i32)
                  (local $ret i32)
                  global.get $heap
                  local.set $ret
                  global.get $heap
                  local.get 3
                  i32.add
                  global.set $heap
                  local.get $ret)
                (func (export "run")
                  (param i32 i32 i32 i32) (result i32)
                  {body}
                  i32.const 64
                  i32.const 0
                  i32.store8
                  i32.const 68
                  i32.const 1024
                  i32.store
                  i32.const 72
                  i32.const {length}
                  i32.store
                  i32.const 64))
              (core instance $i (instantiate $m))
              (func $run
                (param "capability-id" string)
                (param "input-json" string)
                (result (result string (error string)))
                (canon lift (core func $i "run")
                  (memory $i "memory")
                  (realloc (func $i "realloc"))))
              (instance (export (interface "little-monkey:extension/guest@1.0.0"))
                (export "run" (func $run))))"#,
            length = output.len(),
        ))
        .unwrap()
    }

    fn escape(bytes: &[u8]) -> String {
        bytes
            .iter()
            .flat_map(|byte| format!("\\{byte:02x}").into_bytes())
            .map(char::from)
            .collect()
    }

    /// A component that writes `artifact` through the host's own
    /// `artifact-write` import and answers with `prefix` + the returned
    /// artifact id + `suffix`.
    ///
    /// The point of going through the real import rather than fabricating an
    /// id is that ownership is exactly what the consumers check: a fixture
    /// that only *claimed* an id would prove the refusal path and nothing
    /// else. This one proves a provider that legitimately produced bytes is
    /// accepted, and [`component_wat`] with a fabricated id proves one that
    /// did not is refused.
    pub(crate) fn component_wat_writing_artifact(
        artifact: &[u8],
        prefix: &str,
        suffix: &str,
    ) -> Vec<u8> {
        component_wat_writing_artifact_then(artifact, prefix, suffix, "")
    }

    /// [`component_wat_writing_artifact`] with `tail` executed *after* the
    /// artifact is written and before the guest answers.
    ///
    /// The order is the whole point. A guest whose tail never returns has
    /// already made a change the host can see from outside the sandbox by the
    /// time it gets there, so a test can wait for that change and know the
    /// guest is inside the sandbox right now rather than hoping it is.
    pub(crate) fn component_wat_writing_artifact_then(
        artifact: &[u8],
        prefix: &str,
        suffix: &str,
        tail: &str,
    ) -> Vec<u8> {
        wat::parse_str(format!(
            r#"(component
              (import "little-monkey:extension/host@1.0.0" (instance $host
                (export "artifact-write"
                  (func (param "bytes" (list u8)) (result (result string (error string)))))))
              (core module $libc
                (memory (export "memory") 4)
                (global $heap (mut i32) (i32.const 65536))
                (func $realloc (export "realloc")
                  (param i32 i32 i32 i32) (result i32)
                  (local $ret i32)
                  global.get $heap
                  local.set $ret
                  global.get $heap
                  local.get 3
                  i32.add
                  global.set $heap
                  local.get $ret))
              (core instance $libc_i (instantiate $libc))
              (alias core export $libc_i "memory" (core memory $mem))
              (alias core export $libc_i "realloc" (core func $realloc))
              (core func $write (canon lower (func $host "artifact-write")
                (memory $mem) (realloc $realloc)))
              (core module $m
                (import "libc" "memory" (memory 4))
                (import "host" "artifact-write" (func $write (param i32 i32 i32)))
                (data (i32.const 1024) "{artifact}")
                (data (i32.const 2048) "{prefix}")
                (data (i32.const 3072) "{suffix}")
                (func (export "run")
                  (param i32 i32 i32 i32) (result i32)
                  (local $id_ptr i32) (local $id_len i32) (local $total i32)
                  ;; artifact-write(bytes) -> return area at 4096
                  i32.const 1024
                  i32.const {artifact_len}
                  i32.const 4096
                  call $write
                  ;; a failed write leaves the guest with nothing to say
                  i32.const 4096
                  i32.load
                  if
                    unreachable
                  end
                  i32.const 4100
                  i32.load
                  local.set $id_ptr
                  i32.const 4104
                  i32.load
                  local.set $id_len
                  ;; anything the caller wants running after the host saw us
                  {tail}
                  ;; prefix
                  i32.const 8192
                  i32.const 2048
                  i32.const {prefix_len}
                  memory.copy
                  ;; the artifact id the host just returned
                  i32.const 8192
                  i32.const {prefix_len}
                  i32.add
                  local.get $id_ptr
                  local.get $id_len
                  memory.copy
                  ;; suffix
                  i32.const 8192
                  i32.const {prefix_len}
                  i32.add
                  local.get $id_len
                  i32.add
                  i32.const 3072
                  i32.const {suffix_len}
                  memory.copy
                  i32.const {prefix_len}
                  local.get $id_len
                  i32.add
                  i32.const {suffix_len}
                  i32.add
                  local.set $total
                  i32.const 64
                  i32.const 0
                  i32.store8
                  i32.const 68
                  i32.const 8192
                  i32.store
                  i32.const 72
                  local.get $total
                  i32.store
                  i32.const 64))
              (core instance $i (instantiate $m
                (with "libc" (instance $libc_i))
                (with "host" (instance (export "artifact-write" (func $write))))))
              (func $run
                (param "capability-id" string)
                (param "input-json" string)
                (result (result string (error string)))
                (canon lift (core func $i "run")
                  (memory $mem)
                  (realloc $realloc)))
              (instance (export (interface "little-monkey:extension/guest@1.0.0"))
                (export "run" (func $run))))"#,
            artifact = escape(artifact),
            artifact_len = artifact.len(),
            prefix = escape(prefix.as_bytes()),
            prefix_len = prefix.len(),
            suffix = escape(suffix.as_bytes()),
            suffix_len = suffix.len(),
            tail = tail,
        ))
        .unwrap()
    }

    /// A component that writes the *input* it was handed through the host's
    /// own `artifact-write` import and answers with `prefix` + the returned
    /// artifact id + `suffix`.
    ///
    /// Its purpose is to make "did what the caller sent actually reach the
    /// guest" answerable in bytes. A fixture that answers with a constant
    /// cannot distinguish a subsystem that assembled the right request from
    /// one that assembled nothing at all, and a guest cannot echo JSON into
    /// JSON without an escaper. Writing the raw bytes into the artifact store
    /// sidesteps both: the caller reads back exactly what the sandbox saw.
    pub(crate) fn component_wat_echoing_input(prefix: &str, suffix: &str) -> Vec<u8> {
        wat::parse_str(format!(
            r#"(component
              (import "little-monkey:extension/host@1.0.0" (instance $host
                (export "artifact-write"
                  (func (param "bytes" (list u8)) (result (result string (error string)))))))
              (core module $libc
                (memory (export "memory") 8)
                (global $heap (mut i32) (i32.const 262144))
                (func $realloc (export "realloc")
                  (param i32 i32 i32 i32) (result i32)
                  (local $ret i32)
                  global.get $heap
                  local.set $ret
                  global.get $heap
                  local.get 3
                  i32.add
                  global.set $heap
                  local.get $ret))
              (core instance $libc_i (instantiate $libc))
              (alias core export $libc_i "memory" (core memory $mem))
              (alias core export $libc_i "realloc" (core func $realloc))
              (core func $write (canon lower (func $host "artifact-write")
                (memory $mem) (realloc $realloc)))
              (core module $m
                (import "libc" "memory" (memory 8))
                (import "host" "artifact-write" (func $write (param i32 i32 i32)))
                (data (i32.const 2048) "{prefix}")
                (data (i32.const 3072) "{suffix}")
                (func (export "run")
                  (param i32 i32 i32 i32) (result i32)
                  (local $id_ptr i32) (local $id_len i32) (local $total i32)
                  ;; artifact-write(input-json) -> return area at 4096
                  local.get 2
                  local.get 3
                  i32.const 4096
                  call $write
                  i32.const 4096
                  i32.load
                  if
                    unreachable
                  end
                  i32.const 4100
                  i32.load
                  local.set $id_ptr
                  i32.const 4104
                  i32.load
                  local.set $id_len
                  i32.const 8192
                  i32.const 2048
                  i32.const {prefix_len}
                  memory.copy
                  i32.const 8192
                  i32.const {prefix_len}
                  i32.add
                  local.get $id_ptr
                  local.get $id_len
                  memory.copy
                  i32.const 8192
                  i32.const {prefix_len}
                  i32.add
                  local.get $id_len
                  i32.add
                  i32.const 3072
                  i32.const {suffix_len}
                  memory.copy
                  i32.const {prefix_len}
                  local.get $id_len
                  i32.add
                  i32.const {suffix_len}
                  i32.add
                  local.set $total
                  i32.const 64
                  i32.const 0
                  i32.store8
                  i32.const 68
                  i32.const 8192
                  i32.store
                  i32.const 72
                  local.get $total
                  i32.store
                  i32.const 64))
              (core instance $i (instantiate $m
                (with "libc" (instance $libc_i))
                (with "host" (instance (export "artifact-write" (func $write))))))
              (func $run
                (param "capability-id" string)
                (param "input-json" string)
                (result (result string (error string)))
                (canon lift (core func $i "run")
                  (memory $mem)
                  (realloc $realloc)))
              (instance (export (interface "little-monkey:extension/guest@1.0.0"))
                (export "run" (func $run))))"#,
            prefix = escape(prefix.as_bytes()),
            prefix_len = prefix.len(),
            suffix = escape(suffix.as_bytes()),
            suffix_len = suffix.len(),
        ))
        .unwrap()
    }

    /// The smallest real 16-bit mono WAV every consumer in this app can read.
    pub(crate) fn fixture_wav(sample_rate: u32, samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut wav = Vec::with_capacity(44 + data.len());
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36u32 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        wav
    }

    pub(crate) fn manifest(
        source: &Path,
        component: &[u8],
        version: SemanticVersion,
    ) -> ExtensionManifest {
        manifest_for("dev.example.echo", source, component, version)
    }

    /// The same fixture manifest under a caller-chosen extension id and with
    /// no capabilities yet, so a capability test can declare exactly the kind
    /// it is about to exercise.
    pub(crate) fn manifest_for(
        extension_id: &str,
        source: &Path,
        component: &[u8],
        version: SemanticVersion,
    ) -> ExtensionManifest {
        let digest = sha256_bytes(component);
        ExtensionManifest {
            schema_version: EXTENSION_SCHEMA_VERSION,
            extension_id: extension_id.to_string(),
            version,
            display_name: "Fixture Echo".to_string(),
            description: "Independent Component Model fixture".to_string(),
            host_api: VersionConstraint::at_least(EXTENSION_HOST_API_VERSION),
            component: ComponentReference {
                path: "component.wasm".to_string(),
                sha256: digest.clone(),
            },
            capabilities: vec![CapabilityDeclaration {
                capability_id: "echo".to_string(),
                kind: CapabilityKind::Tool,
                display_name: "Echo".to_string(),
                description: "Returns bounded fixture JSON".to_string(),
                input_schema: default_input_schema(),
            }],
            permissions: Vec::new(),
            config_schema: Vec::new(),
            secret_slots: Vec::new(),
            dependencies: Vec::new(),
            compatibility: Compatibility {
                minimum_app_version: SemanticVersion::new(0, 1, 0),
                maximum_app_version_exclusive: None,
                platforms: [std::env::consts::OS.to_string()].into_iter().collect(),
                architectures: [std::env::consts::ARCH.to_string()].into_iter().collect(),
                contract: None,
            },
            publisher: "Independent Fixture".to_string(),
            provenance: PackageProvenance {
                publisher: "Independent Fixture".to_string(),
                source: InstallSource::LocalFolder {
                    canonical_path: source.to_string_lossy().to_string(),
                },
                source_revision: version.to_string(),
                build_reproducible: true,
            },
            signature: None,
            checksums: BTreeMap::from([("component.wasm".to_string(), digest)]),
        }
    }

    pub(crate) fn write_bundle(
        root: &Path,
        name: &str,
        component: &[u8],
        version: SemanticVersion,
    ) -> PathBuf {
        let source = root.join(name);
        fs::create_dir_all(&source).unwrap();
        let manifest = manifest(&source, component, version);
        write_manifest_bundle(&source, component, &manifest);
        source
    }

    pub(crate) fn write_manifest_bundle(
        source: &Path,
        component: &[u8],
        manifest: &ExtensionManifest,
    ) {
        fs::create_dir_all(source).unwrap();
        fs::write(source.join("component.wasm"), component).unwrap();
        fs::write(
            source.join(EXTENSION_MANIFEST_FILE),
            serde_json::to_vec_pretty(manifest).unwrap(),
        )
        .unwrap();
    }

    /// Install a fixture bundle and bring it all the way to healthy+running,
    /// which is the only state any native provider registry will accept it in.
    #[allow(dead_code)]
    pub(crate) async fn install_running(
        manager: &ExtensionManager,
        source: &Path,
        extension_id: &str,
    ) {
        let preview = manager.discover(source).unwrap();
        manager
            .install(
                source,
                Approval {
                    approval_digest: preview.approval_digest,
                    grants: Vec::new(),
                    allow_unsigned: true,
                    allow_untrusted: false,
                    allow_high_risk: false,
                },
            )
            .await
            .unwrap();
        manager.set_enabled(extension_id, true).await.unwrap();
        manager.set_running(extension_id, true).await.unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::test_fixtures::*;
    use super::*;

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "little-monkey-executable-extensions-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn registry_compare_and_swap_rejects_stale_cross_process_writes() {
        let _runtime = runtime_guard();
        let root = TestRoot::new();
        let manager = ExtensionManager::new(root.0.join("app-data")).unwrap();
        let first = manager.load_registry().unwrap();
        let stale = manager.load_registry().unwrap();
        manager.save_registry(&first).unwrap();
        assert!(manager
            .save_registry(&stale)
            .unwrap_err()
            .contains("changed concurrently"));
        assert_eq!(manager.load_registry().unwrap().revision, 1);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_replacement_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestRoot::new();
        let workspace = root.0.join("workspace");
        fs::create_dir(&workspace).unwrap();
        let target = workspace.join("script.sh");
        fs::write(&target, b"old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

        write_workspace_file(&workspace.canonicalize().unwrap(), "script.sh", b"new").unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[tokio::test]
    async fn independent_component_installs_runs_replays_and_uninstalls() {
        let _runtime = runtime_guard();
        let root = TestRoot::new();
        let app_data = root.0.join("app-data");
        let source = write_bundle(
            &root.0,
            "source",
            &component_wat(r#"{"ok":true}"#, ""),
            SemanticVersion::new(1, 0, 0),
        );
        let manager = ExtensionManager::new(&app_data).unwrap();
        let preview = manager.discover(&source).unwrap();
        assert_eq!(preview.trust.state, TrustState::Unsigned);
        assert!(preview.blockers.is_empty());
        let installed = manager
            .install(
                &source,
                Approval {
                    approval_digest: preview.approval_digest,
                    grants: Vec::new(),
                    allow_unsigned: true,
                    allow_untrusted: false,
                    allow_high_risk: false,
                },
            )
            .await
            .unwrap();
        assert!(!installed.health.enabled);
        manager.set_enabled("dev.example.echo", true).await.unwrap();
        let running = manager.set_running("dev.example.echo", true).await.unwrap();
        assert_eq!(running.health.state, HealthState::Healthy);
        let capabilities = manager
            .active_capabilities(Some(CapabilityKind::Tool))
            .unwrap();
        assert_eq!(capabilities.len(), 1);
        assert_eq!(capabilities[0].extension_id, "dev.example.echo");
        assert!(manager
            .resolve_active_capability(CapabilityKind::Channel, "echo")
            .is_err());
        let request = InvocationRequest {
            extension_id: "dev.example.echo".to_string(),
            capability_id: "echo".to_string(),
            input_json: r#"{"value":"hello"}"#.to_string(),
            invocation_id: Some("fixture-invocation-1".to_string()),
            input_artifact_ids: Vec::new(),
            expected_kind: Some(CapabilityKind::Tool),
            expected_version: Some("1.0.0".to_string()),
        };
        let first = manager
            .invoke_active_capability(
                CapabilityKind::Tool,
                "echo",
                request.input_json.clone(),
                request.invocation_id.clone(),
                Vec::new(),
            )
            .await
            .unwrap();
        assert_eq!(first.output_json, r#"{"ok":true}"#);
        let rebound = manager
            .invoke(InvocationRequest {
                extension_id: "dev.example.echo".to_string(),
                capability_id: "echo".to_string(),
                input_json: "{}".to_string(),
                invocation_id: Some("wrong-owner-binding".to_string()),
                input_artifact_ids: Vec::new(),
                expected_kind: Some(CapabilityKind::Channel),
                expected_version: Some("1.0.0".to_string()),
            })
            .await
            .unwrap_err();
        assert!(rebound.contains("kind changed"));
        assert_eq!(manager.invoke(request).await.unwrap(), first);
        manager
            .set_enabled("dev.example.echo", false)
            .await
            .unwrap();
        manager.uninstall("dev.example.echo").unwrap();
        assert!(manager.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn approval_and_install_persist_the_exact_verified_bundle_bytes() {
        let _runtime = runtime_guard();
        let root = TestRoot::new();
        let app_data = root.0.join("app-data");
        let source = root.0.join("source-race");
        let component = component_wat(r#"{"ok":true}"#, "");
        let original_asset = b"verified asset".to_vec();
        let mut bundle_manifest = manifest(&source, &component, SemanticVersion::new(1, 0, 0));
        bundle_manifest
            .checksums
            .insert("assets/data.bin".to_string(), sha256_bytes(&original_asset));
        write_manifest_bundle(&source, &component, &bundle_manifest);
        fs::create_dir_all(source.join("assets")).unwrap();
        fs::write(source.join("assets/data.bin"), &original_asset).unwrap();

        let manager = ExtensionManager::new(&app_data).unwrap();
        let preview = manager.discover(&source).unwrap();
        let loaded = manager.load_bundle(&source).unwrap();
        fs::write(
            source.join("component.wasm"),
            b"tampered after verification",
        )
        .unwrap();
        fs::write(source.join("assets/data.bin"), b"tampered asset").unwrap();
        manager.persist_bundle(&loaded).unwrap();
        assert_eq!(
            manager.read_installed_component(&loaded.manifest).unwrap(),
            component
        );
        assert_eq!(
            fs::read(
                manager
                    .root
                    .join("versions/dev.example.echo/1.0.0/assets/data.bin")
            )
            .unwrap(),
            original_asset
        );

        let replacement = b"different valid asset".to_vec();
        fs::write(source.join("component.wasm"), &component).unwrap();
        fs::write(source.join("assets/data.bin"), &replacement).unwrap();
        bundle_manifest
            .checksums
            .insert("assets/data.bin".to_string(), sha256_bytes(&replacement));
        fs::write(
            source.join(EXTENSION_MANIFEST_FILE),
            serde_json::to_vec_pretty(&bundle_manifest).unwrap(),
        )
        .unwrap();
        let error = manager
            .install(
                &source,
                Approval {
                    approval_digest: preview.approval_digest,
                    grants: Vec::new(),
                    allow_unsigned: true,
                    allow_untrusted: false,
                    allow_high_risk: false,
                },
            )
            .await
            .unwrap_err();
        assert!(error.contains("Approval digest"));
    }

    #[tokio::test]
    async fn tampered_persisted_manifest_fails_closed_before_runtime_use() {
        let _runtime = runtime_guard();
        let root = TestRoot::new();
        let app_data = root.0.join("app-data");
        let source = write_bundle(
            &root.0,
            "persisted-tamper",
            &component_wat(r#"{"ok":true}"#, ""),
            SemanticVersion::new(1, 0, 0),
        );
        let manager = ExtensionManager::new(&app_data).unwrap();
        let preview = manager.discover(&source).unwrap();
        manager
            .install(
                &source,
                Approval {
                    approval_digest: preview.approval_digest,
                    grants: Vec::new(),
                    allow_unsigned: true,
                    allow_untrusted: false,
                    allow_high_risk: false,
                },
            )
            .await
            .unwrap();
        let registry = manager.root.join("registry.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&registry).unwrap()).unwrap();
        value["records"]["dev.example.echo"]["versions"]["1.0.0"]["manifest"]["display_name"] =
            serde_json::json!("Tampered display name");
        fs::write(&registry, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        assert!(manager
            .list()
            .unwrap_err()
            .contains("Stored trust evidence changed"));
    }

    #[tokio::test]
    async fn permission_expanding_update_requires_exact_digest_and_rolls_back() {
        let _runtime = runtime_guard();
        let root = TestRoot::new();
        let app_data = root.0.join("app-data");
        let v1 = write_bundle(
            &root.0,
            "v1",
            &component_wat(r#"{"version":1}"#, ""),
            SemanticVersion::new(1, 0, 0),
        );
        let manager = ExtensionManager::new(&app_data).unwrap();
        let install_preview = manager.discover(&v1).unwrap();
        manager
            .install(
                &v1,
                Approval {
                    approval_digest: install_preview.approval_digest,
                    grants: Vec::new(),
                    allow_unsigned: true,
                    allow_untrusted: false,
                    allow_high_risk: false,
                },
            )
            .await
            .unwrap();

        let component = component_wat(r#"{"version":2}"#, "");
        let source = root.0.join("v2");
        let mut next = manifest(&source, &component, SemanticVersion::new(1, 1, 0));
        next.permissions.push(PermissionDeclaration {
            permission_id: "api-origin".to_string(),
            kind: PermissionKind::NetworkOrigin,
            scope: "https://api.example.com".to_string(),
            reason: "Fetch the exact test API".to_string(),
        });
        write_manifest_bundle(&source, &component, &next);
        let preview = manager.preview_update(&source).unwrap();
        let diff = preview.permission_diff.as_ref().unwrap();
        assert!(diff.expands_authority);
        assert_eq!(diff.added[0].scope, "https://api.example.com");
        let stale = manager
            .update(
                &source,
                Approval {
                    approval_digest: "0".repeat(64),
                    grants: vec![PermissionGrant {
                        permission_id: "api-origin".to_string(),
                        binding: None,
                    }],
                    allow_unsigned: true,
                    allow_untrusted: false,
                    allow_high_risk: true,
                },
            )
            .await
            .unwrap_err();
        assert!(stale.contains("Approval digest"));
        let updated = manager
            .update(
                &source,
                Approval {
                    approval_digest: preview.approval_digest,
                    grants: vec![PermissionGrant {
                        permission_id: "api-origin".to_string(),
                        binding: None,
                    }],
                    allow_unsigned: true,
                    allow_untrusted: false,
                    allow_high_risk: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.active_version, "1.1.0");
        assert_eq!(updated.previous_version.as_deref(), Some("1.0.0"));
        assert_eq!(updated.health.state, HealthState::Stopped);
        let rolled_back = manager.rollback("dev.example.echo").await.unwrap();
        assert_eq!(rolled_back.active_version, "1.0.0");
        assert_eq!(rolled_back.previous_version.as_deref(), Some("1.1.0"));
    }

    #[tokio::test]
    async fn traps_are_contained_and_third_failure_protectively_disables() {
        let _runtime = runtime_guard();
        let root = TestRoot::new();
        let app_data = root.0.join("app-data");
        let source = write_bundle(
            &root.0,
            "trap",
            &component_wat(r#"{"unreachable":true}"#, "unreachable"),
            SemanticVersion::new(1, 0, 0),
        );
        let manager = ExtensionManager::new(&app_data).unwrap();
        let preview = manager.discover(&source).unwrap();
        manager
            .install(
                &source,
                Approval {
                    approval_digest: preview.approval_digest,
                    grants: Vec::new(),
                    allow_unsigned: true,
                    allow_untrusted: false,
                    allow_high_risk: false,
                },
            )
            .await
            .unwrap();
        manager.set_enabled("dev.example.echo", true).await.unwrap();
        manager.set_running("dev.example.echo", true).await.unwrap();
        for attempt in 1..=3 {
            let error = manager
                .invoke(InvocationRequest {
                    extension_id: "dev.example.echo".to_string(),
                    capability_id: "echo".to_string(),
                    input_json: "{}".to_string(),
                    invocation_id: Some(format!("trap-{attempt}")),
                    input_artifact_ids: Vec::new(),
                    expected_kind: None,
                    expected_version: None,
                })
                .await
                .unwrap_err();
            assert!(error.contains("trapped"));
        }
        let detail = manager.inspect("dev.example.echo").unwrap();
        assert_eq!(detail.health.state, HealthState::ProtectiveDisabled);
        assert_eq!(detail.health.trap_count, 3);
        assert!(!detail.health.enabled);
        assert!(!detail.health.running);
        assert!(manager
            .extension_cancel_path("dev.example.echo")
            .unwrap()
            .exists());
        assert_eq!(
            manager
                .set_running("dev.example.echo", false)
                .await
                .unwrap()
                .health
                .state,
            HealthState::ProtectiveDisabled
        );
        assert_eq!(
            manager
                .set_enabled("dev.example.echo", false)
                .await
                .unwrap()
                .health
                .state,
            HealthState::ProtectiveDisabled
        );
        assert!(manager
            .set_enabled("dev.example.echo", true)
            .await
            .unwrap_err()
            .contains("cannot be enabled"));
        assert_eq!(
            manager
                .validate_installed("dev.example.echo")
                .await
                .unwrap()
                .health
                .state,
            HealthState::Stopped
        );
    }

    #[tokio::test]
    async fn memory_growth_is_capped_and_the_host_survives() {
        let _runtime = runtime_guard();
        let root = TestRoot::new();
        let app_data = root.0.join("app-data");
        let source = write_bundle(
            &root.0,
            "memory",
            &component_wat(r#"{"never":true}"#, "i32.const 2048 memory.grow drop"),
            SemanticVersion::new(1, 0, 0),
        );
        let manager = ExtensionManager::new(&app_data).unwrap();
        let preview = manager.discover(&source).unwrap();
        manager
            .install(
                &source,
                Approval {
                    approval_digest: preview.approval_digest,
                    grants: Vec::new(),
                    allow_unsigned: true,
                    allow_untrusted: false,
                    allow_high_risk: false,
                },
            )
            .await
            .unwrap();
        manager.set_enabled("dev.example.echo", true).await.unwrap();
        manager.set_running("dev.example.echo", true).await.unwrap();
        let error = manager
            .invoke(InvocationRequest {
                extension_id: "dev.example.echo".into(),
                capability_id: "echo".into(),
                input_json: "{}".into(),
                invocation_id: Some("memory-limit".into()),
                input_artifact_ids: Vec::new(),
                expected_kind: None,
                expected_version: None,
            })
            .await
            .unwrap_err();
        assert!(error.contains("memory") || error.contains("grow") || error.contains("trap"));
        assert_eq!(manager.list().unwrap().len(), 1);
        manager
            .validate_installed("dev.example.echo")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn broker_denies_ungranted_resources_and_wasi_has_no_ambient_env_or_files() {
        let _runtime = runtime_guard();
        use wasmtime_wasi::cli::WasiCliView as _;
        use wasmtime_wasi::filesystem::WasiFilesystemView as _;

        let root = TestRoot::new();
        let app_data = root.0.join("app-data");
        let component = component_wat("{}", "");
        let mut extension_manifest = manifest(&root.0, &component, SemanticVersion::new(1, 0, 0));
        extension_manifest.config_schema.push(ConfigField {
            key: "mode".into(),
            label: "Mode".into(),
            description: "Fixture mode".into(),
            kind: ConfigFieldKind::String,
            required: false,
            default: Some(serde_json::json!("default")),
            options: Vec::new(),
            minimum: None,
            maximum: None,
        });
        let manager = ExtensionManager::new(&app_data).unwrap();
        let mut host = manager
            .runtime_host(
                extension_manifest,
                Vec::new(),
                BTreeMap::from([("mode".into(), serde_json::json!("chosen"))]),
                "broker-denials".into(),
                CancellationToken::new(),
                Vec::new(),
                BTreeSet::new(),
                BTreeSet::new(),
                BTreeMap::new(),
            )
            .unwrap();

        let environment = {
            let mut cli = host.cli();
            wasmtime_wasi::p2::bindings::cli::environment::Host::get_environment(&mut cli).unwrap()
        };
        let preopens = {
            let mut filesystem = host.filesystem();
            wasmtime_wasi::p2::bindings::filesystem::preopens::Host::get_directories(
                &mut filesystem,
            )
            .unwrap()
        };
        assert!(environment.is_empty());
        assert!(preopens.is_empty());
        assert_eq!(
            bindings::little_monkey::extension::host::Host::config_get(&mut host, "mode".into(),)
                .await
                .unwrap(),
            Some(r#""chosen""#.into())
        );
        assert!(bindings::little_monkey::extension::host::Host::config_get(
            &mut host,
            "undeclared".into(),
        )
        .await
        .unwrap_err()
        .contains("Permission denied"));
        assert!(bindings::little_monkey::extension::host::Host::state_put(
            &mut host,
            "oversized".into(),
            vec![0; MAX_PRIVATE_STATE_BYTES + 1],
        )
        .await
        .unwrap_err()
        .contains("256 KiB"));
        assert!(
            bindings::little_monkey::extension::host::Host::set_tool_result(
                &mut host,
                "x".repeat(MAX_OUTPUT_BYTES + 1),
            )
            .await
            .unwrap_err()
            .contains("bounded JSON")
        );

        assert!(
            bindings::little_monkey::extension::host::Host::workspace_read(
                &mut host,
                "workspace".into(),
                "secret.txt".into(),
            )
            .await
            .unwrap_err()
            .contains("Permission denied")
        );
        assert!(
            bindings::little_monkey::extension::host::Host::artifact_read(
                &mut host,
                "0".repeat(64),
            )
            .await
            .unwrap_err()
            .contains("Permission denied")
        );
        assert!(bindings::little_monkey::extension::host::Host::send_http(
            &mut host,
            bindings::little_monkey::extension::host::HttpRequest {
                method: "GET".into(),
                url: "https://example.com".into(),
                headers: Vec::new(),
                body: Vec::new(),
                auth_slot: None,
            },
        )
        .await
        .unwrap_err()
        .contains("Permission denied"));
        assert!(bindings::little_monkey::extension::host::Host::model_invoke(
            &mut host,
            "runtime:model".into(),
            r#"{"runtime_id":"runtime","operation":"chat_completions","body":{"model":"model"}}"#.into(),
        )
            .await
            .unwrap_err()
            .contains("Permission denied"));
        assert!(
            bindings::little_monkey::extension::host::Host::device_request(
                &mut host,
                "device".into(),
                "camera".into(),
                "{}".into(),
            )
            .await
            .unwrap_err()
            .contains("Permission denied")
        );
        assert_eq!(host.undeclared_attempts, 6);
        assert_eq!(host.logs.len(), 6);
    }

    /// Block until the guest has written `artifact_id` through the host.
    ///
    /// This is the only signal in the suite that reports *guest instructions
    /// executed* rather than "the host got as far as scheduling one". The store
    /// is content addressed, so the id is known before the run starts and a
    /// hit cannot be anything but this fixture's own write.
    async fn wait_until_written(store: &ArtifactStore, artifact_id: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while !store.exists(artifact_id).unwrap() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no guest ever wrote '{artifact_id}'"
            );
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Block until `invocation_id` has registered for cancellation.
    ///
    /// Its guest has to be compiled and instantiated first, which is the
    /// slowest thing in this suite on the slowest runner in the fleet. The
    /// budget guards against a registration that never happens, so it is
    /// deliberately far longer than the operation should ever take.
    async fn wait_until_registered(invocation_id: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while !CANCELLATIONS.lock().unwrap().contains_key(invocation_id) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "'{invocation_id}' never registered for cancellation"
            );
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// How often the two waits above look at the registry.
    ///
    /// Short because what they are watching is transient: an entry exists only
    /// from the moment its invocation registers to the moment it ends, and for
    /// a guest that is racing its fuel ceiling that can be a fraction of a
    /// second. Cheap to poll, and the alternative — a spin — would slow the
    /// very work being watched.
    const POLL_INTERVAL: Duration = Duration::from_millis(1);

    /// Wait for `invocation_id` to register and cancel it without releasing
    /// the registry in between.
    ///
    /// Observing and cancelling under one lock is what makes the assertion
    /// about the return value sound: an entry is inserted and removed under
    /// this same mutex, so a cancel issued while holding it cannot be aimed at
    /// an invocation that has already finished.
    async fn cancel_once_registered(invocation_id: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(active) = CANCELLATIONS.lock().unwrap().get(invocation_id) {
                active.token.cancel();
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "'{invocation_id}' never registered for cancellation"
            );
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn infinite_guest_can_be_cancelled_and_host_remains_usable() {
        let _runtime = runtime_guard();
        let root = TestRoot::new();
        let app_data = root.0.join("app-data");
        let source = write_bundle(
            &root.0,
            "loop",
            &component_wat(r#"{"never":true}"#, "(loop $forever (br $forever))"),
            SemanticVersion::new(1, 0, 0),
        );
        let manager = ExtensionManager::new(&app_data).unwrap();
        let preview = manager.discover(&source).unwrap();
        manager
            .install(
                &source,
                Approval {
                    approval_digest: preview.approval_digest,
                    grants: Vec::new(),
                    allow_unsigned: true,
                    allow_untrusted: false,
                    allow_high_risk: false,
                },
            )
            .await
            .unwrap();
        manager.set_enabled("dev.example.echo", true).await.unwrap();
        manager.set_running("dev.example.echo", true).await.unwrap();
        // The two cancellation mechanisms are exercised differently on
        // purpose, because at this fixture's production fuel budget only one
        // of them can be aimed at a *running* guest without racing it.
        //
        // A guest that loops forever is racing its own fuel ceiling from its
        // first instruction, and it is registered for cancellation for a
        // little longer than it actually executes — the registry entry is
        // removed after the run, behind a lock and a registry write. So an
        // external actor that waits to see the entry and then acts has no
        // guarantee the guest is still inside the sandbox: on a loaded runner
        // both an earlier `cancel` and a later marker write landed after the
        // guest had already spent its fuel.
        //
        // What is asserted here is therefore split. The in-process token is
        // spent as early as the registry allows and the claim is the one that
        // holds either way — the invocation ends, and the *host* is unharmed
        // by whichever way it ended. The on-disk marker is instead placed
        // before its invocation starts, where the outcome is not a race at
        // all: a marker for that exact invocation id refuses to run it, and
        // the error says so.
        //
        // The remaining case — a marker written while a guest is provably
        // mid-flight — needs a guest that reports its own start and a budget
        // it cannot burn through, and is
        // `an_on_disk_marker_stops_a_guest_that_has_already_executed` below.
        let token_watcher = tokio::spawn(cancel_once_registered("cancel-loop"));
        atomic_write(
            &manager.invocation_cancel_path("second-loop").unwrap(),
            b"cancel\n",
        )
        .unwrap();

        let invocation_id = "cancel-loop".to_string();
        let running_manager = manager.clone();
        let task = tokio::spawn(async move {
            running_manager
                .invoke(InvocationRequest {
                    extension_id: "dev.example.echo".to_string(),
                    capability_id: "echo".to_string(),
                    input_json: "{}".to_string(),
                    invocation_id: Some(invocation_id),
                    input_artifact_ids: Vec::new(),
                    expected_kind: None,
                    expected_version: None,
                })
                .await
        });
        let second_manager = manager.clone();
        let second_task = tokio::spawn(async move {
            second_manager
                .invoke(InvocationRequest {
                    extension_id: "dev.example.echo".to_string(),
                    capability_id: "echo".to_string(),
                    input_json: "{}".to_string(),
                    invocation_id: Some("second-loop".to_string()),
                    input_artifact_ids: Vec::new(),
                    expected_kind: None,
                    expected_version: None,
                })
                .await
        });
        // The watcher panics rather than returning if it never found the
        // invocation, so reaching here means `cancel` was spent on a live
        // registry entry for this exact id.
        token_watcher.await.unwrap();
        assert!(task.await.unwrap().is_err());
        // The marker, by contrast, has an exact expected outcome: it names one
        // invocation id, and that invocation never reaches the sandbox.
        assert_eq!(
            second_task.await.unwrap().unwrap_err(),
            "Extension invocation was cancelled before start"
        );
        // Cancellation is keyed by invocation, not by extension: an id nobody
        // is running cancels nothing, and neither of the two above was reached
        // through anything but its own id.
        assert!(!cancel("no-such-invocation").unwrap());
        assert!(manager.list().is_ok());
        let detail = manager.inspect("dev.example.echo").unwrap();
        assert_eq!(
            detail.health.state,
            HealthState::Healthy,
            "logs: {:?}",
            manager.logs("dev.example.echo", 50).unwrap()
        );
        assert_eq!(detail.health.consecutive_failures, 0);

        let stopping_manager = manager.clone();
        let stopping_task = tokio::spawn(async move {
            stopping_manager
                .invoke(InvocationRequest {
                    extension_id: "dev.example.echo".to_string(),
                    capability_id: "echo".to_string(),
                    input_json: "{}".to_string(),
                    invocation_id: Some("stop-loop".to_string()),
                    input_artifact_ids: Vec::new(),
                    expected_kind: None,
                    expected_version: None,
                })
                .await
        });
        wait_until_registered("stop-loop").await;
        let stopped = manager
            .set_running("dev.example.echo", false)
            .await
            .unwrap();
        assert_eq!(stopped.health.state, HealthState::Stopped);
        assert!(stopping_task.await.unwrap().is_err());
        assert_eq!(
            manager.inspect("dev.example.echo").unwrap().health.state,
            HealthState::Stopped
        );
        manager.uninstall("dev.example.echo").unwrap();
    }

    /// A fuel ceiling a looping guest cannot reach inside a test's window.
    ///
    /// A thousand times the production budget. The point is not the number but
    /// what it removes: with it, "the guest ran out of fuel" and "the test
    /// stopped the guest" are separated by three orders of magnitude of work
    /// rather than by whichever happened first on a loaded machine. Nothing
    /// else is relaxed — the same wall clock still ends the run, and it ends it
    /// with its own distinct error, so a budget that somehow *was* reached
    /// cannot be mistaken for the cancellation this test is about.
    const FUEL_A_TEST_WINDOW_CANNOT_BURN: u64 = DEFAULT_FUEL * 1_000;

    /// A fuel ceiling a looping guest reaches immediately.
    ///
    /// Deliberately far below production so the runaway ends in well under a
    /// second on the slowest runner, while still leaving instantiation — which
    /// spends fuel too — orders of magnitude of headroom.
    const FUEL_A_RUNAWAY_BURNS_AT_ONCE: u64 = 2_000_000;

    /// Install a fixture whose guest writes `evidence` through the host's own
    /// `artifact-write` import and then never returns.
    ///
    /// Both halves matter. The write is a host-observable effect that only
    /// executing guest instructions can produce, and the loop after it means
    /// the guest is still inside the sandbox when a test sees that effect.
    async fn install_guest_that_signals_then_never_returns(
        manager: &ExtensionManager,
        root: &Path,
        extension_id: &str,
        capability_id: &str,
        evidence: &[u8],
    ) {
        let component =
            component_wat_writing_artifact_then(evidence, "", "", "(loop $forever (br $forever))");
        let source = root.join(extension_id);
        let mut value = manifest_for(
            extension_id,
            &source,
            &component,
            SemanticVersion::new(1, 0, 0),
        );
        // Capability ids are owned across the whole registry, so fixtures that
        // are installed side by side cannot share the manifest helper's one.
        value.capabilities[0].capability_id = capability_id.to_string();
        value.permissions = vec![PermissionDeclaration {
            permission_id: "artifact-write".to_string(),
            kind: PermissionKind::ArtifactWrite,
            scope: "content_v1".to_string(),
            reason: "Reports that the guest is executing".to_string(),
        }];
        write_manifest_bundle(&source, &component, &value);
        let preview = manager.discover(&source).unwrap();
        manager
            .install(
                &source,
                Approval {
                    approval_digest: preview.approval_digest,
                    grants: vec![PermissionGrant {
                        permission_id: "artifact-write".to_string(),
                        binding: None,
                    }],
                    allow_unsigned: true,
                    allow_untrusted: false,
                    allow_high_risk: true,
                },
            )
            .await
            .unwrap();
        manager.set_enabled(extension_id, true).await.unwrap();
        manager.set_running(extension_id, true).await.unwrap();
    }

    fn loop_invocation(
        extension_id: &str,
        capability_id: &str,
        invocation_id: &str,
    ) -> InvocationRequest {
        InvocationRequest {
            extension_id: extension_id.to_string(),
            capability_id: capability_id.to_string(),
            input_json: "{}".to_string(),
            invocation_id: Some(invocation_id.to_string()),
            input_artifact_ids: Vec::new(),
            expected_kind: None,
            expected_version: None,
        }
    }

    /// A guest that spends its own budget says so, and is counted as a trap.
    ///
    /// This is the other half of the mid-flight cancellation test below: that
    /// one asserts an invocation ended in cancellation and *not* in fuel, which
    /// is worth nothing unless fuel exhaustion is something this runtime can
    /// actually report under its own name.
    #[tokio::test]
    async fn a_runaway_guest_ends_on_its_own_fuel_budget() {
        let _runtime = runtime_guard();
        let root = TestRoot::new();
        let app_data = root.0.join("app-data");
        let source = write_bundle(
            &root.0,
            "runaway",
            &component_wat(r#"{"never":true}"#, "(loop $forever (br $forever))"),
            SemanticVersion::new(1, 0, 0),
        );
        let manager = ExtensionManager::new(&app_data)
            .unwrap()
            .with_fuel(FUEL_A_RUNAWAY_BURNS_AT_ONCE);
        let preview = manager.discover(&source).unwrap();
        manager
            .install(
                &source,
                Approval {
                    approval_digest: preview.approval_digest,
                    grants: Vec::new(),
                    allow_unsigned: true,
                    allow_untrusted: false,
                    allow_high_risk: false,
                },
            )
            .await
            .unwrap();
        manager.set_enabled("dev.example.echo", true).await.unwrap();
        manager.set_running("dev.example.echo", true).await.unwrap();

        assert_eq!(
            manager
                .invoke(loop_invocation("dev.example.echo", "echo", "runaway-1"))
                .await
                .unwrap_err(),
            FUEL_EXHAUSTED_ERROR
        );
        let detail = manager.inspect("dev.example.echo").unwrap();
        assert_eq!(detail.health.trap_count, 1);
        assert_eq!(detail.health.consecutive_failures, 1);
        assert_eq!(detail.health.state, HealthState::Degraded);
    }

    /// The gap this suite used to have: an on-disk marker stopping a guest that
    /// was **already executing inside the sandbox**.
    ///
    /// Every step is ordered so the claim cannot be satisfied by accident:
    ///
    /// 1. each guest writes a known artifact through the host's own import, so
    ///    the store answering `exists` means that guest's instructions ran;
    /// 2. only *after* observing that does the test write the marker, so the
    ///    guest provably executed before the marker existed;
    /// 3. the guest between those two points is an infinite loop, so it cannot
    ///    have finished on its own;
    /// 4. the assertion is equality with [`CANCELLED_ERROR`], which the runtime
    ///    now reports only for an epoch interrupt taken with the wall-clock
    ///    deadline unset, or for a cancel that landed on a guest which had not
    ///    already spent its fuel. Exhausted fuel, the wall clock, an ordinary
    ///    trap and a pre-start refusal each have their own distinct message;
    /// 5. the fuel ceiling is raised so far past the window that exhaustion is
    ///    not a plausible outcome to begin with — and if it happened anyway,
    ///    step 4 would fail rather than pass.
    ///
    /// The second invocation carries the rest of the marker contract: a marker
    /// names one invocation, so a marker for another id — or for no id at all —
    /// leaves it running.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_on_disk_marker_stops_a_guest_that_has_already_executed() {
        let _runtime = runtime_guard();
        let root = TestRoot::new();
        let app_data = root.0.join("app-data");
        let manager = ExtensionManager::new(&app_data)
            .unwrap()
            .with_fuel(FUEL_A_TEST_WINDOW_CANNOT_BURN);
        let alpha_evidence = b"alpha reached the host";
        let beta_evidence = b"beta reached the host";
        install_guest_that_signals_then_never_returns(
            &manager,
            &root.0,
            "dev.example.alpha",
            "alpha",
            alpha_evidence,
        )
        .await;
        install_guest_that_signals_then_never_returns(
            &manager,
            &root.0,
            "dev.example.beta",
            "beta",
            beta_evidence,
        )
        .await;
        let plain = write_bundle(
            &root.0,
            "plain",
            &component_wat(r#"{"ok":true}"#, ""),
            SemanticVersion::new(1, 0, 0),
        );
        install_running(&manager, &plain, "dev.example.echo").await;
        let store = ArtifactStore::with_max_blob_size(
            &manager.artifact_root,
            MAX_ARTIFACT_READ_BYTES as u64,
        )
        .unwrap();

        let alpha_manager = manager.clone();
        let alpha = tokio::spawn(async move {
            alpha_manager
                .invoke(loop_invocation(
                    "dev.example.alpha",
                    "alpha",
                    "marker-alpha",
                ))
                .await
        });
        let beta_manager = manager.clone();
        let beta = tokio::spawn(async move {
            beta_manager
                .invoke(loop_invocation("dev.example.beta", "beta", "marker-beta"))
                .await
        });
        wait_until_written(&store, &sha256_bytes(alpha_evidence)).await;
        wait_until_written(&store, &sha256_bytes(beta_evidence)).await;

        // A marker naming an invocation nobody is running stops nothing.
        atomic_write(
            &manager.invocation_cancel_path("marker-nobody").unwrap(),
            b"cancel\n",
        )
        .unwrap();
        // Now the one that names a guest which is, at this moment, executing.
        atomic_write(
            &manager.invocation_cancel_path("marker-alpha").unwrap(),
            b"cancel\n",
        )
        .unwrap();
        assert_eq!(
            alpha.await.unwrap().unwrap_err(),
            CANCELLED_ERROR,
            "logs: {:?}",
            manager.logs("dev.example.alpha", 50).unwrap()
        );
        // Marker isolation: the other guest was never named, so it is still
        // inside the sandbox with its own registry entry.
        assert!(CANCELLATIONS.lock().unwrap().contains_key("marker-beta"));
        assert!(!beta.is_finished());
        atomic_write(
            &manager.invocation_cancel_path("marker-beta").unwrap(),
            b"cancel\n",
        )
        .unwrap();
        assert_eq!(beta.await.unwrap().unwrap_err(), CANCELLED_ERROR);

        // A cancelled invocation is not a crashed one, and it leaves nothing
        // behind that could cancel the next one.
        for extension_id in ["dev.example.alpha", "dev.example.beta"] {
            let detail = manager.inspect(extension_id).unwrap();
            assert_eq!(
                detail.health.state,
                HealthState::Healthy,
                "logs: {:?}",
                manager.logs(extension_id, 50).unwrap()
            );
            assert_eq!(detail.health.consecutive_failures, 0);
            assert_eq!(detail.health.trap_count, 0);
        }
        assert!(!manager
            .invocation_cancel_path("marker-alpha")
            .unwrap()
            .exists());
        assert!(!manager
            .invocation_cancel_path("marker-beta")
            .unwrap()
            .exists());
        assert!(CANCELLATIONS.lock().unwrap().is_empty());
        assert_eq!(manager.list().unwrap().len(), 3);
        assert_eq!(
            manager
                .invoke(loop_invocation(
                    "dev.example.echo",
                    "echo",
                    "after-cancellation"
                ))
                .await
                .unwrap()
                .output_json,
            r#"{"ok":true}"#
        );
    }

    #[tokio::test]
    async fn ambient_imports_and_core_modules_fail_closed() {
        let _runtime = runtime_guard();
        let root = TestRoot::new();
        let app_data = root.0.join("app-data");
        let manager = ExtensionManager::new(&app_data).unwrap();
        let core = wat::parse_str("(module)").unwrap();
        let core_source = write_bundle(&root.0, "core", &core, SemanticVersion::new(1, 0, 0));
        assert!(manager
            .discover(&core_source)
            .unwrap_err()
            .contains("Component validation"));

        let ambient = wat::parse_str("(component (import \"evil:process/run\" (func)))").unwrap();
        let ambient_source =
            write_bundle(&root.0, "ambient", &ambient, SemanticVersion::new(1, 0, 0));
        let preview = manager.discover(&ambient_source).unwrap();
        let error = manager
            .install(
                &ambient_source,
                Approval {
                    approval_digest: preview.approval_digest,
                    grants: Vec::new(),
                    allow_unsigned: true,
                    allow_untrusted: false,
                    allow_high_risk: false,
                },
            )
            .await
            .unwrap_err();
        assert!(error.contains("not defined") || error.contains("instantiation"));
    }

    #[tokio::test]
    async fn publisher_revocation_blocks_a_previously_verified_running_extension() {
        let _runtime = runtime_guard();
        use ring::signature::KeyPair as _;

        let root = TestRoot::new();
        let app_data = root.0.join("app-data");
        let source = root.0.join("signed");
        let component = component_wat(r#"{"ok":true}"#, "");
        let mut signed = manifest(&source, &component, SemanticVersion::new(1, 0, 0));
        signed.extension_id = "dev.signed.echo".to_string();
        signed.publisher = "Signed Fixture".to_string();
        signed.provenance.publisher = signed.publisher.clone();
        signed.signature = Some(PackageSignature {
            trust_root_id: "signed-fixture-root".to_string(),
            key_id: "release".to_string(),
            algorithm: "ed25519".to_string(),
            signature_hex: String::new(),
        });
        let key_pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(&[7_u8; 32]).unwrap();
        let signature = key_pair.sign(&signed.signing_payload().unwrap());
        signed.signature.as_mut().unwrap().signature_hex = signature
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        write_manifest_bundle(&source, &component, &signed);

        let manager = ExtensionManager::new(&app_data).unwrap();
        let key = crate::package_ecosystem::TrustedKey {
            key_id: "release".to_string(),
            algorithm: "ed25519".to_string(),
            public_key_hex: key_pair
                .public_key()
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            valid_from_unix_ms: 0,
            valid_until_unix_ms: u64::MAX,
            revoked_at_unix_ms: None,
        };
        let trust_store = |key| TrustStore {
            schema_version: 1,
            roots: BTreeMap::from([(
                "signed-fixture-root".to_string(),
                crate::package_ecosystem::TrustRoot {
                    trust_root_id: "signed-fixture-root".to_string(),
                    publisher: "Signed Fixture".to_string(),
                    package_namespaces: BTreeSet::from(["dev.signed.".to_string()]),
                    keys: BTreeMap::from([("release".to_string(), key)]),
                },
            )]),
        };
        fs::write(
            app_data.join("extensions-trust-v1.json"),
            serde_json::to_vec_pretty(&trust_store(key.clone())).unwrap(),
        )
        .unwrap();
        let preview = manager.discover(&source).unwrap();
        assert_eq!(preview.trust.state, TrustState::Verified);
        manager
            .install(
                &source,
                Approval {
                    approval_digest: preview.approval_digest,
                    grants: Vec::new(),
                    allow_unsigned: false,
                    allow_untrusted: false,
                    allow_high_risk: false,
                },
            )
            .await
            .unwrap();
        manager.set_enabled("dev.signed.echo", true).await.unwrap();
        manager.set_running("dev.signed.echo", true).await.unwrap();
        assert_eq!(manager.active_capabilities(None).unwrap().len(), 1);

        let mut revoked_key = key;
        revoked_key.revoked_at_unix_ms = Some(now_ms().saturating_sub(1));
        fs::write(
            app_data.join("extensions-trust-v1.json"),
            serde_json::to_vec_pretty(&trust_store(revoked_key)).unwrap(),
        )
        .unwrap();
        let detail = manager.inspect("dev.signed.echo").unwrap();
        assert_eq!(detail.trust.state, TrustState::Untrusted);
        assert!(detail
            .blockers
            .iter()
            .any(|blocker| blocker.contains("no longer valid")));
        assert!(manager.active_capabilities(None).unwrap().is_empty());
        assert!(manager
            .invoke(InvocationRequest {
                extension_id: "dev.signed.echo".to_string(),
                capability_id: "echo".to_string(),
                input_json: "{}".to_string(),
                invocation_id: Some("revoked-publisher".to_string()),
                input_artifact_ids: Vec::new(),
                expected_kind: Some(CapabilityKind::Tool),
                expected_version: Some("1.0.0".to_string()),
            })
            .await
            .unwrap_err()
            .contains("trust check failed"));
    }

    #[test]
    fn invalid_origins_schemas_and_builtin_collisions_are_rejected() {
        let root = TestRoot::new();
        let component = component_wat("{}", "");
        let source = root.0.join("invalid");
        let mut value = manifest(&source, &component, SemanticVersion::new(1, 0, 0));
        value.permissions.push(PermissionDeclaration {
            permission_id: "wildcard".to_string(),
            kind: PermissionKind::NetworkOrigin,
            scope: "https://*.example.com".to_string(),
            reason: "Too broad".to_string(),
        });
        assert!(value.validate().unwrap_err().contains("canonical exact"));
        value.permissions.clear();
        value.permissions.push(PermissionDeclaration {
            permission_id: "model".to_string(),
            kind: PermissionKind::ModelInvoke,
            scope: "mlx:mlx-community/Qwen3.5-9B:latest".to_string(),
            reason: "Exact catalog model".to_string(),
        });
        value.validate().unwrap();
        value.permissions.clear();
        value.capabilities[0].input_schema = serde_json::Value::String("not a schema".to_string());
        assert!(value.validate().unwrap_err().contains("input_schema"));
        value.capabilities[0].input_schema = default_input_schema();
        value.capabilities[0].capability_id = "read_file".to_string();
        assert!(
            validate_capability_collisions(&value, &RegistryState::default())
                .unwrap_err()
                .contains("built-in")
        );
    }

    #[test]
    fn invalid_signatures_and_incompatible_host_ranges_are_blocked() {
        let root = TestRoot::new();
        let component = component_wat("{}", "");
        let manager = ExtensionManager::new(root.0.join("app-data")).unwrap();

        let incompatible_source = root.0.join("incompatible");
        let mut incompatible = manifest(
            &incompatible_source,
            &component,
            SemanticVersion::new(1, 0, 0),
        );
        incompatible.host_api = VersionConstraint::at_least(SemanticVersion::new(2, 0, 0));
        write_manifest_bundle(&incompatible_source, &component, &incompatible);
        let preview = manager.discover(&incompatible_source).unwrap();
        assert!(!preview.compatible);
        assert!(!preview.blockers.is_empty());

        let (trust, _, _) = signed_first_party_catalog().unwrap();
        let (root_id, trust_root) = trust.roots.iter().next().unwrap();
        let (key_id, key) = trust_root.keys.iter().next().unwrap();
        let namespace = trust_root.package_namespaces.iter().next().unwrap();
        let invalid_source = root.0.join("invalid-signature");
        let mut invalid = manifest(&invalid_source, &component, SemanticVersion::new(1, 0, 0));
        invalid.extension_id = format!("{namespace}extension-fixture");
        invalid.publisher = trust_root.publisher.clone();
        invalid.provenance.publisher = trust_root.publisher.clone();
        invalid.signature = Some(PackageSignature {
            trust_root_id: root_id.clone(),
            key_id: key_id.clone(),
            algorithm: key.algorithm.clone(),
            signature_hex: "not-hex".into(),
        });
        write_manifest_bundle(&invalid_source, &component, &invalid);
        let preview = manager.discover(&invalid_source).unwrap();
        assert_eq!(preview.trust.state, TrustState::Invalid);
        assert!(preview
            .blockers
            .iter()
            .any(|blocker| blocker.contains("Signature")));
    }
}
