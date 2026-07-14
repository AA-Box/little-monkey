//! Production dependency assembly for the M3 runtime hub.
//!
//! This module owns concrete operating-system, HTTP, process, keychain, and
//! runtime implementations. The Tauri root only needs to construct and manage
//! one [`M3CommandState`]; no production dependency is supplied by the UI.

use crate::compatibility_hub::{
    CanonicalContent, CanonicalInferenceRequest, CanonicalInferenceResponse, CanonicalMessage,
    CanonicalRole, CanonicalStreamEvent, CanonicalUsage, LanStateProtector, OsLanEntropy,
};
use crate::m3_commands::{M3CommandState, M3OwnedProcessShutdown};
use crate::m3_runtime_hub::{
    DefaultM3LanAccessFactory, HttpM3CatalogSource, M3CanonicalStreamSink, M3CatalogSource,
    M3Clock, M3HardwareProbe, M3HubConfig, M3HubError, M3HubFuture, M3HubResult, M3InferenceEngine,
    M3InstalledModelView, M3ModelCapabilities, M3OperationContext, M3RuntimeDriver, M3RuntimeHub,
    M3RuntimeHubDependencies, M3RuntimeKind, M3RuntimeReconciler, M3RuntimeStatusView, MlxM3Driver,
    ReqwestM3DownloadTransport, RuntimeAdapterM3Driver, SystemM3Clock,
};
use crate::mlx_runtime::{
    CurrentHostMlxProbe, MlxError, MlxFuture, MlxGenerationRequest, MlxGenerationSummary,
    MlxInstallLimits, MlxLaunchSpec, MlxModelCapabilities, MlxModelRecord, MlxOperationContext,
    MlxPackageInstaller, MlxProcessHandle, MlxProcessMetrics, MlxRuntimeAdapter, MlxRuntimeConfig,
    MlxServiceController, MlxSignatureVerifier, MlxStreamEvent, MlxStreamSink,
};
use crate::runtime_adapter::{
    AcceleratorKind, EndpointOrigin, EndpointPolicy, HardwareSnapshot, HttpTransport,
    ManagedLlamaCppAdapter, ManagedLogChunk, ManagedProcessController, ManagedProcessHandle,
    ManagedProcessSpec, ManagedProcessState, ManagedProcessStatus, ModelCapabilities,
    OllamaHttpAdapter, PlatformCapabilities, PortOwnership, ReqwestHttpTransport,
    ResidencyOwnership, RuntimeAdapter, RuntimeAdapterError, RuntimeFuture, RuntimeKind,
    RuntimeModel, RuntimeOperationContext, RuntimeOperationLimits,
};
use base64::Engine as _;
use futures_util::StreamExt;
use ring::rand::{SecureRandom, SystemRandom};
use ring::{hmac, signature};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::process::Child;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::process::Command;

const M3_DIRECTORY: &str = "m3";
const CATALOG_CONFIG_FILE: &str = "catalog-sources.json";
const CATALOG_CONFIG_SCHEMA_VERSION: u32 = 1;
const MAX_CATALOG_CONFIG_BYTES: u64 = 256 * 1024;
const OLLAMA_RUNTIME_ID: &str = "ollama";
const LLAMA_RUNTIME_ID: &str = "managed-llama";
const OLLAMA_ENDPOINT: &str = "http://127.0.0.1:11434";
const LLAMA_ENDPOINT: &str = "http://127.0.0.1:8090";
const LLAMA_PORT: u16 = 8_090;
const MAX_INFERENCE_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_INFERENCE_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const KEYCHAIN_SERVICE: &str = "com.littlemonkey.m3-lan";
const KEYCHAIN_ACCOUNT: &str = "lan-state-hmac-v1";
const MLX_RELEASE_KEY_ID: &str = "release-2026-1";
const MLX_RELEASE_PUBLIC_KEY_HEX: &str =
    "0fd1a0b2a2e6a90c5f61eb8e9db503bf4e123c4cee11888650748c2f0efc669e";

fn lock<T>(mutex: &Mutex<T>) -> M3HubResult<MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| M3HubError::LockPoisoned)
}

fn runtime_error(error: RuntimeAdapterError) -> M3HubError {
    M3HubError::Runtime(error.to_string())
}

fn now_ms() -> M3HubResult<u64> {
    SystemM3Clock.now_ms()
}

fn now_seconds() -> M3HubResult<u64> {
    Ok(now_ms()? / 1_000)
}

/// Cross-platform, point-in-time hardware probe used for fit decisions.
///
/// `system-memory` supplies physical byte counts on all supported desktop
/// targets. Metal is advertised only for an Apple-Silicon build, where CPU
/// and GPU share the same physical memory; unsupported accelerators are never
/// guessed from model names or environment variables.
#[derive(Default)]
pub struct SystemM3HardwareProbe;

impl crate::m3_runtime_hub::M3HardwareProbe for SystemM3HardwareProbe {
    fn snapshot(&self) -> M3HubResult<HardwareSnapshot> {
        let (total_ram_bytes, available_ram_bytes) =
            std::panic::catch_unwind(|| (system_memory::total(), system_memory::available()))
                .map_err(|_| {
                    M3HubError::Runtime("operating-system memory probe failed".to_string())
                })?;
        if total_ram_bytes == 0 || available_ram_bytes > total_ram_bytes {
            return Err(M3HubError::Runtime(
                "operating-system memory probe returned impossible values".to_string(),
            ));
        }
        let logical_cpu_count = std::thread::available_parallelism()
            .map_err(|error| M3HubError::Runtime(format!("CPU probe failed: {error}")))?
            .get()
            .try_into()
            .map_err(|_| M3HubError::Runtime("logical CPU count overflow".to_string()))?;
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        let mut accelerators = Vec::new();
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        let mut accelerators = vec![crate::runtime_adapter::AcceleratorCapability {
            kind: AcceleratorKind::Metal,
            available: true,
            device_names: vec!["Apple Silicon unified GPU".to_string()],
            total_memory_bytes: Some(total_ram_bytes),
            available_memory_bytes: Some(available_ram_bytes),
        }];
        if let Some(cuda) = detect_nvidia_accelerator() {
            accelerators.push(cuda);
        }
        let snapshot = HardwareSnapshot {
            captured_at_ms: now_ms()?,
            total_ram_bytes,
            available_ram_bytes,
            logical_cpu_count,
            platform: PlatformCapabilities::current(accelerators),
        };
        snapshot.profile().map_err(runtime_error)?;
        Ok(snapshot)
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn detect_nvidia_accelerator() -> Option<crate::runtime_adapter::AcceleratorCapability> {
    let output = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total,memory.free",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_nvidia_smi(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn detect_nvidia_accelerator() -> Option<crate::runtime_adapter::AcceleratorCapability> {
    None
}

#[cfg(any(target_os = "linux", target_os = "windows", test))]
fn parse_nvidia_smi(output: &str) -> Option<crate::runtime_adapter::AcceleratorCapability> {
    const MIB: u64 = 1024 * 1024;
    let mut device_names = Vec::new();
    let mut total_memory_bytes = 0_u64;
    let mut available_memory_bytes = 0_u64;
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let mut fields = line.rsplitn(3, ',').map(str::trim);
        let free_mib = fields.next()?.parse::<u64>().ok()?;
        let total_mib = fields.next()?.parse::<u64>().ok()?;
        let name = fields.next()?.trim();
        if name.is_empty() || total_mib == 0 || free_mib > total_mib {
            return None;
        }
        device_names.push(name.to_string());
        total_memory_bytes = total_memory_bytes.saturating_add(total_mib.saturating_mul(MIB));
        available_memory_bytes =
            available_memory_bytes.saturating_add(free_mib.saturating_mul(MIB));
    }
    if device_names.is_empty() {
        return None;
    }
    Some(crate::runtime_adapter::AcceleratorCapability {
        kind: AcceleratorKind::Cuda,
        available: true,
        device_names,
        total_memory_bytes: Some(total_memory_bytes),
        available_memory_bytes: Some(available_memory_bytes),
    })
}

/// HMAC-SHA256 protection whose key is generated from OS CSPRNG entropy and
/// stored only in the operating-system keychain.
pub struct KeychainLanStateProtector {
    key: hmac::Key,
    protector_id: String,
}

impl KeychainLanStateProtector {
    pub fn load_or_create() -> M3HubResult<Self> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
            .map_err(|error| M3HubError::State(format!("access M3 LAN keychain: {error}")))?;
        let key_bytes = match entry.get_password() {
            Ok(encoded) => base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| M3HubError::State("M3 LAN keychain value is corrupt".to_string()))?,
            Err(keyring::Error::NoEntry) => {
                let mut generated = vec![0_u8; 32];
                SystemRandom::new().fill(&mut generated).map_err(|_| {
                    M3HubError::State("operating-system random source failed".to_string())
                })?;
                entry
                    .set_password(&base64::engine::general_purpose::STANDARD.encode(&generated))
                    .map_err(|error| {
                        M3HubError::State(format!("store M3 LAN key in keychain: {error}"))
                    })?;
                generated
            }
            Err(error) => {
                return Err(M3HubError::State(format!(
                    "read M3 LAN key from keychain: {error}"
                )))
            }
        };
        Self::from_key(key_bytes)
    }

    fn from_key(key_bytes: Vec<u8>) -> M3HubResult<Self> {
        if key_bytes.len() != 32 {
            return Err(M3HubError::State(
                "M3 LAN keychain value has the wrong length".to_string(),
            ));
        }
        let digest = format!("{:x}", Sha256::digest(&key_bytes));
        Ok(Self {
            key: hmac::Key::new(hmac::HMAC_SHA256, &key_bytes),
            protector_id: format!("keychain-hmac-sha256-{}", &digest[..24]),
        })
    }
}

impl LanStateProtector for KeychainLanStateProtector {
    fn protector_id(&self) -> &str {
        &self.protector_id
    }

    fn authenticate(&self, canonical_state: &[u8]) -> Result<Vec<u8>, String> {
        Ok(hmac::sign(&self.key, canonical_state).as_ref().to_vec())
    }

    fn verify(&self, canonical_state: &[u8], tag: &[u8]) -> Result<(), String> {
        hmac::verify(&self.key, canonical_state, tag)
            .map_err(|_| "M3 LAN state authentication tag mismatch".to_string())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProductionCatalogConfig {
    schema_version: u32,
    sources: Vec<M3CatalogSourceConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3CatalogSourceConfig {
    pub source_id: String,
    pub endpoint: String,
}

pub fn catalog_source_configs(root: &Path) -> M3HubResult<Vec<M3CatalogSourceConfig>> {
    let path = root.join(CATALOG_CONFIG_FILE);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(M3HubError::Io {
                operation: "inspect M3 catalog configuration",
                path,
                source: error,
            })
        }
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_CATALOG_CONFIG_BYTES {
        return Err(M3HubError::State(
            "M3 catalog configuration must be a bounded regular file".to_string(),
        ));
    }
    let bytes = fs::read(&path).map_err(|source| M3HubError::Io {
        operation: "read M3 catalog configuration",
        path: path.clone(),
        source,
    })?;
    let config: ProductionCatalogConfig = serde_json::from_slice(&bytes)?;
    if config.schema_version != CATALOG_CONFIG_SCHEMA_VERSION || config.sources.len() > 32 {
        return Err(M3HubError::State(
            "M3 catalog configuration version/count is unsupported".to_string(),
        ));
    }
    let mut ids = BTreeSet::new();
    for entry in &config.sources {
        if !ids.insert(entry.source_id.clone()) {
            return Err(M3HubError::State(
                "M3 catalog source ids must be unique".to_string(),
            ));
        }
        // Constructing the production source is the canonical validation for
        // identifiers, HTTPS/loopback policy, and bounded HTTP behavior.
        HttpM3CatalogSource::new(entry.source_id.clone(), &entry.endpoint)?;
    }
    Ok(config.sources)
}

fn catalog_sources_from_configs(
    configs: &[M3CatalogSourceConfig],
) -> M3HubResult<Vec<Arc<dyn M3CatalogSource>>> {
    if configs.len() > 32 {
        return Err(M3HubError::State(
            "at most 32 M3 catalog sources can be configured".to_string(),
        ));
    }
    let mut ids = BTreeSet::new();
    configs
        .iter()
        .map(|entry| {
            if !ids.insert(entry.source_id.clone()) {
                return Err(M3HubError::State(
                    "M3 catalog source ids must be unique".to_string(),
                ));
            }
            HttpM3CatalogSource::new(entry.source_id.clone(), &entry.endpoint)
                .map(|source| Arc::new(source) as Arc<dyn M3CatalogSource>)
        })
        .collect()
}

fn load_catalog_sources(root: &Path) -> M3HubResult<Vec<Arc<dyn M3CatalogSource>>> {
    let configs = catalog_source_configs(root)?;
    catalog_sources_from_configs(&configs)
}

pub fn replace_catalog_source_configs(
    hub: &M3RuntimeHub,
    configs: Vec<M3CatalogSourceConfig>,
) -> M3HubResult<Vec<M3CatalogSourceConfig>> {
    let sources = catalog_sources_from_configs(&configs)?;
    let document = ProductionCatalogConfig {
        schema_version: CATALOG_CONFIG_SCHEMA_VERSION,
        sources: configs.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&document)?;
    if bytes.len() as u64 > MAX_CATALOG_CONFIG_BYTES {
        return Err(M3HubError::State(
            "M3 catalog configuration exceeds its byte limit".to_string(),
        ));
    }
    let root = hub.root();
    ensure_private_directory(root)?;
    let path = root.join(CATALOG_CONFIG_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(M3HubError::State(
                "M3 catalog configuration target is not a regular file".to_string(),
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(M3HubError::Io {
                operation: "inspect M3 catalog configuration target",
                path,
                source,
            })
        }
    }
    let temporary = root.join(format!(".catalog-sources-{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary).map_err(|source| M3HubError::Io {
        operation: "create staged M3 catalog configuration",
        path: temporary.clone(),
        source,
    })?;
    if let Err(source) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(M3HubError::Io {
            operation: "write staged M3 catalog configuration",
            path: temporary,
            source,
        });
    }
    if let Err(source) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(M3HubError::Io {
            operation: "publish M3 catalog configuration",
            path,
            source,
        });
    }
    #[cfg(unix)]
    File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| M3HubError::Io {
            operation: "sync M3 catalog configuration directory",
            path: root.to_path_buf(),
            source,
        })?;
    hub.replace_catalog_sources(sources)?;
    Ok(configs)
}

/// Canonical inference implementation for Ollama and llama.cpp's local
/// OpenAI-compatible chat-completions endpoints.
pub struct OpenAiCompatibleM3InferenceEngine {
    endpoint: EndpointOrigin,
    client: reqwest::Client,
    active: Mutex<BTreeMap<String, CancellationToken>>,
}

impl OpenAiCompatibleM3InferenceEngine {
    pub fn new(endpoint: &str) -> M3HubResult<Self> {
        let endpoint =
            EndpointOrigin::parse(endpoint, EndpointPolicy::LoopbackOnly).map_err(runtime_error)?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| M3HubError::Transport(error.to_string()))?;
        Ok(Self {
            endpoint,
            client,
            active: Mutex::new(BTreeMap::new()),
        })
    }

    fn begin_request(&self, request_id: &str) -> M3HubResult<CancellationToken> {
        if request_id.is_empty()
            || request_id.len() > 512
            || request_id.chars().any(char::is_control)
        {
            return Err(M3HubError::Runtime(
                "invalid inference request id".to_string(),
            ));
        }
        let token = CancellationToken::new();
        let mut active = lock(&self.active)?;
        if active.contains_key(request_id) {
            return Err(M3HubError::Conflict(format!(
                "inference request {request_id} is already active"
            )));
        }
        active.insert(request_id.to_string(), token.clone());
        Ok(token)
    }

    fn finish_request(&self, request_id: &str) {
        if let Ok(mut active) = self.active.lock() {
            active.remove(request_id);
        }
    }

    async fn send(
        &self,
        request: &CanonicalInferenceRequest,
        stream: bool,
        cancellation: &CancellationToken,
        context: &M3OperationContext,
    ) -> M3HubResult<reqwest::Response> {
        let body = openai_request_body(request, stream)?;
        let encoded = serde_json::to_vec(&body)?;
        if encoded.len() > MAX_INFERENCE_REQUEST_BYTES {
            return Err(M3HubError::Runtime(
                "canonical inference request exceeds the production byte limit".to_string(),
            ));
        }
        let url = self
            .endpoint
            .url("/v1/chat/completions")
            .map_err(runtime_error)?;
        let operation = async {
            tokio::select! {
                _ = context.cancellation.cancelled() => Err(M3HubError::Cancelled { operation: "local inference request".to_string() }),
                _ = cancellation.cancelled() => Err(M3HubError::Cancelled { operation: "local inference request".to_string() }),
                response = self.client.post(url).header(reqwest::header::CONTENT_TYPE, "application/json").body(encoded).send() => {
                    response.map_err(|error| M3HubError::Transport(error.to_string()))
                }
            }
        };
        let response = tokio::time::timeout(Duration::from_millis(context.timeout_ms), operation)
            .await
            .map_err(|_| M3HubError::Timeout {
                operation: "local inference request".to_string(),
                timeout_ms: context.timeout_ms,
            })??;
        if !response.status().is_success() {
            let status = response.status();
            let detail = read_bounded_response(response, 64 * 1024, cancellation, context).await?;
            return Err(M3HubError::Runtime(format!(
                "local inference returned HTTP {status}: {}",
                String::from_utf8_lossy(&detail).trim()
            )));
        }
        Ok(response)
    }

    async fn complete_inner(
        &self,
        request: &CanonicalInferenceRequest,
        cancellation: &CancellationToken,
        context: &M3OperationContext,
    ) -> M3HubResult<CanonicalInferenceResponse> {
        let response = self.send(request, false, cancellation, context).await?;
        let bytes = read_bounded_response(
            response,
            MAX_INFERENCE_RESPONSE_BYTES,
            cancellation,
            context,
        )
        .await?;
        let value: Value = serde_json::from_slice(&bytes)?;
        parse_openai_response(&value, request)
    }

    async fn stream_inner(
        &self,
        request: &CanonicalInferenceRequest,
        sink: &mut dyn M3CanonicalStreamSink,
        cancellation: &CancellationToken,
        context: &M3OperationContext,
    ) -> M3HubResult<()> {
        let response = self.send(request, true, cancellation, context).await?;
        parse_openai_sse(response, request, sink, cancellation, context).await
    }
}

impl M3InferenceEngine for OpenAiCompatibleM3InferenceEngine {
    fn complete<'a>(
        &'a self,
        request: &'a CanonicalInferenceRequest,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, CanonicalInferenceResponse> {
        Box::pin(async move {
            let cancellation = self.begin_request(&request.request_id)?;
            let result = self.complete_inner(request, &cancellation, context).await;
            self.finish_request(&request.request_id);
            result
        })
    }

    fn stream<'a>(
        &'a self,
        request: &'a CanonicalInferenceRequest,
        sink: &'a mut dyn M3CanonicalStreamSink,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, ()> {
        Box::pin(async move {
            let cancellation = self.begin_request(&request.request_id)?;
            let result = self
                .stream_inner(request, sink, &cancellation, context)
                .await;
            self.finish_request(&request.request_id);
            result
        })
    }

    fn cancel<'a>(
        &'a self,
        request_id: &'a str,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, bool> {
        Box::pin(async move {
            let active = lock(&self.active)?;
            if let Some(token) = active.get(request_id) {
                token.cancel();
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }
}

/// Enforces the selected runtime's live model capability inventory before an
/// HTTP request reaches its inference endpoint. This is deliberately a
/// separate layer from wire translation: unsupported tools/structured output
/// fail locally even when an older backend would otherwise accept and ignore
/// those fields.
struct CapabilityCheckedInferenceEngine {
    adapter: Arc<dyn RuntimeAdapter>,
    inner: Arc<OpenAiCompatibleM3InferenceEngine>,
    structured_output_models: BTreeSet<String>,
}

impl CapabilityCheckedInferenceEngine {
    async fn validate(
        &self,
        request: &CanonicalInferenceRequest,
        context: &M3OperationContext,
    ) -> M3HubResult<()> {
        let limits = RuntimeOperationLimits {
            timeout_ms: context.timeout_ms,
            ..RuntimeOperationLimits::default()
        };
        let runtime_context = RuntimeOperationContext::new(limits, context.cancellation.clone());
        let inventory = self
            .adapter
            .inventory(&runtime_context)
            .await
            .map_err(runtime_error)?;
        let model = inventory
            .models
            .iter()
            .find(|model| model.model_id == request.model)
            .ok_or_else(|| M3HubError::NotFound(format!("runtime model {}", request.model)))?;
        if !model.capabilities.chat {
            return Err(M3HubError::Unsupported(format!(
                "model {} does not advertise chat inference",
                request.model
            )));
        }
        let uses_tools = !request.tools.is_empty()
            || request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .any(|content| {
                    matches!(
                        content,
                        CanonicalContent::ToolUse { .. } | CanonicalContent::ToolResult { .. }
                    )
                });
        if uses_tools && !model.capabilities.tool_calling {
            return Err(M3HubError::Unsupported(format!(
                "model {} does not advertise tool calling",
                request.model
            )));
        }
        if request.response_schema.is_some()
            && !self.structured_output_models.contains(&request.model)
        {
            return Err(M3HubError::Unsupported(format!(
                "model {} does not advertise structured output",
                request.model
            )));
        }
        Ok(())
    }
}

impl M3InferenceEngine for CapabilityCheckedInferenceEngine {
    fn complete<'a>(
        &'a self,
        request: &'a CanonicalInferenceRequest,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, CanonicalInferenceResponse> {
        Box::pin(async move {
            self.validate(request, context).await?;
            self.inner.complete(request, context).await
        })
    }

    fn stream<'a>(
        &'a self,
        request: &'a CanonicalInferenceRequest,
        sink: &'a mut dyn M3CanonicalStreamSink,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, ()> {
        Box::pin(async move {
            self.validate(request, context).await?;
            self.inner.stream(request, sink, context).await
        })
    }

    fn cancel<'a>(
        &'a self,
        request_id: &'a str,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, bool> {
        self.inner.cancel(request_id, context)
    }
}

fn openai_request_body(request: &CanonicalInferenceRequest, stream: bool) -> M3HubResult<Value> {
    let messages = request
        .messages
        .iter()
        .flat_map(openai_messages)
        .collect::<M3HubResult<Vec<_>>>()?;
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                    "strict": tool.strict
                }
            })
        })
        .collect::<Vec<_>>();
    let mut body = Map::new();
    body.insert("model".to_string(), Value::String(request.model.clone()));
    body.insert("messages".to_string(), Value::Array(messages));
    body.insert("stream".to_string(), Value::Bool(stream));
    body.insert(
        "max_tokens".to_string(),
        Value::Number(request.max_output_tokens.into()),
    );
    if let Some(temperature) = request.temperature {
        body.insert(
            "temperature".to_string(),
            serde_json::Number::from_f64(temperature)
                .map(Value::Number)
                .ok_or_else(|| M3HubError::Runtime("temperature is not finite".to_string()))?,
        );
    }
    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
        body.insert("tool_choice".to_string(), Value::String("auto".to_string()));
    }
    if let Some(schema) = &request.response_schema {
        body.insert(
            "response_format".to_string(),
            json!({"type":"json_schema","json_schema":{"name":"response","strict":true,"schema":schema}}),
        );
    }
    if stream {
        body.insert("stream_options".to_string(), json!({"include_usage":true}));
    }
    Ok(Value::Object(body))
}

fn openai_messages(message: &CanonicalMessage) -> Vec<M3HubResult<Value>> {
    let role = match message.role {
        CanonicalRole::System => "system",
        CanonicalRole::User => "user",
        CanonicalRole::Assistant => "assistant",
        CanonicalRole::Tool => "tool",
    };
    if message.role == CanonicalRole::Tool {
        return message
            .content
            .iter()
            .map(|content| match content {
                CanonicalContent::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => Ok(json!({
                    "role":"tool",
                    "tool_call_id":tool_use_id,
                    "content": if *is_error { format!("Error: {content}") } else { content.clone() }
                })),
                _ => Err(M3HubError::Runtime(
                    "tool-role messages may contain only tool results".to_string(),
                )),
            })
            .collect();
    }
    let mut text = String::new();
    let mut calls = Vec::new();
    for content in &message.content {
        match content {
            CanonicalContent::Text { text: value } => text.push_str(value),
            CanonicalContent::ToolUse { id, name, input }
                if message.role == CanonicalRole::Assistant =>
            {
                calls.push(json!({
                    "id":id,
                    "type":"function",
                    "function":{"name":name,"arguments":input.to_string()}
                }));
            }
            CanonicalContent::ToolResult { .. } => {
                return vec![Err(M3HubError::Runtime(
                    "tool results require a tool-role message".to_string(),
                ))]
            }
            CanonicalContent::ToolUse { .. } => {
                return vec![Err(M3HubError::Runtime(
                    "tool calls require an assistant-role message".to_string(),
                ))]
            }
        }
    }
    let mut object = Map::new();
    object.insert("role".to_string(), Value::String(role.to_string()));
    object.insert(
        "content".to_string(),
        if text.is_empty() && !calls.is_empty() {
            Value::Null
        } else {
            Value::String(text)
        },
    );
    if !calls.is_empty() {
        object.insert("tool_calls".to_string(), Value::Array(calls));
    }
    vec![Ok(Value::Object(object))]
}

fn parse_openai_response(
    value: &Value,
    request: &CanonicalInferenceRequest,
) -> M3HubResult<CanonicalInferenceResponse> {
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| M3HubError::Runtime("local response contains no choice".to_string()))?;
    let message = choice
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| M3HubError::Runtime("local response choice has no message".to_string()))?;
    let mut content = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            content.push(CanonicalContent::Text {
                text: text.to_string(),
            });
        }
    } else if message.get("content").is_some_and(|value| !value.is_null()) {
        return Err(M3HubError::Runtime(
            "local response content is not text".to_string(),
        ));
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let id = required_string(call, "id", "tool call id")?;
            let function = call
                .get("function")
                .ok_or_else(|| M3HubError::Runtime("tool call function is missing".to_string()))?;
            let name = required_string(function, "name", "tool call name")?;
            let arguments = required_string(function, "arguments", "tool call arguments")?;
            let input = serde_json::from_str(arguments).map_err(|error| {
                M3HubError::Runtime(format!("tool call arguments are not JSON: {error}"))
            })?;
            content.push(CanonicalContent::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input,
            });
        }
    }
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(&request.model)
        .to_string();
    if model != request.model {
        return Err(M3HubError::Runtime(
            "local response model differs from the requested model".to_string(),
        ));
    }
    Ok(CanonicalInferenceResponse {
        response_id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(&request.request_id)
            .to_string(),
        model,
        content,
        finish_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .unwrap_or("stop")
            .to_string(),
        usage: parse_usage(value.get("usage")),
        created_at_seconds: value
            .get("created")
            .and_then(Value::as_u64)
            .unwrap_or(now_seconds()?),
    })
}

fn required_string<'a>(value: &'a Value, key: &str, label: &str) -> M3HubResult<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| M3HubError::Runtime(format!("{label} is missing")))
}

fn parse_usage(value: Option<&Value>) -> CanonicalUsage {
    CanonicalUsage {
        input_tokens: value
            .and_then(|value| value.get("prompt_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .and_then(|value| value.get("completion_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

async fn read_bounded_response(
    response: reqwest::Response,
    limit: usize,
    cancellation: &CancellationToken,
    context: &M3OperationContext,
) -> M3HubResult<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(M3HubError::Runtime(
            "local inference response exceeds the byte limit".to_string(),
        ));
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = tokio::select! {
        _ = context.cancellation.cancelled() => return Err(M3HubError::Cancelled { operation: "read local inference response".to_string() }),
        _ = cancellation.cancelled() => return Err(M3HubError::Cancelled { operation: "read local inference response".to_string() }),
        chunk = stream.next() => chunk,
    } {
        let chunk = chunk.map_err(|error| M3HubError::Transport(error.to_string()))?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(M3HubError::Runtime(
                "local inference response exceeds the byte limit".to_string(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

struct OpenAiStreamState {
    response_id: Option<String>,
    model: Option<String>,
    created: Option<u64>,
    next_index: usize,
    text_index: Option<usize>,
    tools: BTreeMap<u64, OpenAiStreamTool>,
    finish_reason: Option<String>,
    usage: CanonicalUsage,
    saw_done: bool,
}

impl Default for OpenAiStreamState {
    fn default() -> Self {
        Self {
            response_id: None,
            model: None,
            created: None,
            next_index: 0,
            text_index: None,
            tools: BTreeMap::new(),
            finish_reason: None,
            usage: CanonicalUsage {
                input_tokens: 0,
                output_tokens: 0,
            },
            saw_done: false,
        }
    }
}

struct OpenAiStreamTool {
    content_index: usize,
    call_id: String,
    name: String,
    pending_arguments: String,
    started: bool,
}

impl OpenAiStreamState {
    fn ensure_started(
        &mut self,
        chunk: &Value,
        request: &CanonicalInferenceRequest,
        sink: &mut dyn M3CanonicalStreamSink,
    ) -> M3HubResult<()> {
        if self.response_id.is_some() {
            return Ok(());
        }
        let response_id = chunk
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(&request.request_id)
            .to_string();
        let model = chunk
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(&request.model)
            .to_string();
        if model != request.model {
            return Err(M3HubError::Runtime(
                "local stream model differs from the requested model".to_string(),
            ));
        }
        let created = chunk
            .get("created")
            .and_then(Value::as_u64)
            .unwrap_or(now_seconds()?);
        sink.emit(CanonicalStreamEvent::ResponseStart {
            response_id: response_id.clone(),
            model: model.clone(),
            created_at_seconds: created,
        })
        .map_err(M3HubError::Runtime)?;
        self.response_id = Some(response_id);
        self.model = Some(model);
        self.created = Some(created);
        Ok(())
    }

    fn ingest(
        &mut self,
        chunk: &Value,
        request: &CanonicalInferenceRequest,
        sink: &mut dyn M3CanonicalStreamSink,
    ) -> M3HubResult<()> {
        self.ensure_started(chunk, request, sink)?;
        if chunk.get("usage").is_some() {
            self.usage = parse_usage(chunk.get("usage"));
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(());
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        let Some(delta) = choice.get("delta") else {
            return Ok(());
        };
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                let index = match self.text_index {
                    Some(index) => index,
                    None => {
                        let index = self.next_index;
                        self.next_index += 1;
                        self.text_index = Some(index);
                        sink.emit(CanonicalStreamEvent::TextStart { index })
                            .map_err(M3HubError::Runtime)?;
                        index
                    }
                };
                sink.emit(CanonicalStreamEvent::TextDelta {
                    index,
                    text: text.to_string(),
                })
                .map_err(M3HubError::Runtime)?;
            }
        }
        for call in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let upstream_index = call
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| M3HubError::Runtime("stream tool index is missing".to_string()))?;
            let tool = self.tools.entry(upstream_index).or_insert_with(|| {
                let content_index = self.next_index;
                self.next_index += 1;
                OpenAiStreamTool {
                    content_index,
                    call_id: String::new(),
                    name: String::new(),
                    pending_arguments: String::new(),
                    started: false,
                }
            });
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                if tool.started && !id.is_empty() && id != tool.call_id {
                    return Err(M3HubError::Runtime(
                        "stream changed a tool call id after it started".to_string(),
                    ));
                }
                if !id.is_empty() {
                    tool.call_id = id.to_string();
                }
            }
            if let Some(function) = call.get("function") {
                if let Some(name) = function.get("name").and_then(Value::as_str) {
                    if tool.started && !name.is_empty() && name != tool.name {
                        return Err(M3HubError::Runtime(
                            "stream changed a tool name after it started".to_string(),
                        ));
                    }
                    if !tool.started && !name.is_empty() {
                        tool.name.push_str(name);
                    }
                }
                if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                    tool.pending_arguments.push_str(arguments);
                }
            }
            if !tool.started && !tool.call_id.is_empty() && !tool.name.is_empty() {
                sink.emit(CanonicalStreamEvent::ToolCallStart {
                    index: tool.content_index,
                    call_id: tool.call_id.clone(),
                    name: tool.name.clone(),
                })
                .map_err(M3HubError::Runtime)?;
                tool.started = true;
            }
            if tool.started && !tool.pending_arguments.is_empty() {
                sink.emit(CanonicalStreamEvent::ToolCallArgumentsDelta {
                    index: tool.content_index,
                    call_id: tool.call_id.clone(),
                    json_delta: std::mem::take(&mut tool.pending_arguments),
                })
                .map_err(M3HubError::Runtime)?;
            }
        }
        Ok(())
    }

    fn finish(
        mut self,
        request: &CanonicalInferenceRequest,
        sink: &mut dyn M3CanonicalStreamSink,
    ) -> M3HubResult<()> {
        if self.response_id.is_none() {
            self.ensure_started(&Value::Null, request, sink)?;
        }
        if let Some(index) = self.text_index {
            sink.emit(CanonicalStreamEvent::TextEnd { index })
                .map_err(M3HubError::Runtime)?;
        }
        for tool in self.tools.values() {
            if !tool.started || !tool.pending_arguments.is_empty() {
                return Err(M3HubError::Runtime(
                    "local stream ended with an incomplete tool call".to_string(),
                ));
            }
            sink.emit(CanonicalStreamEvent::ToolCallEnd {
                index: tool.content_index,
                call_id: tool.call_id.clone(),
            })
            .map_err(M3HubError::Runtime)?;
        }
        sink.emit(CanonicalStreamEvent::ResponseCompleted {
            response_id: self
                .response_id
                .unwrap_or_else(|| request.request_id.clone()),
            finish_reason: self.finish_reason.unwrap_or_else(|| "stop".to_string()),
            usage: self.usage,
        })
        .map_err(M3HubError::Runtime)
    }
}

async fn parse_openai_sse(
    response: reqwest::Response,
    request: &CanonicalInferenceRequest,
    sink: &mut dyn M3CanonicalStreamSink,
    cancellation: &CancellationToken,
    context: &M3OperationContext,
) -> M3HubResult<()> {
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut observed = 0_usize;
    let mut state = OpenAiStreamState::default();
    while let Some(chunk) = tokio::select! {
        _ = context.cancellation.cancelled() => return Err(M3HubError::Cancelled { operation: "stream local inference".to_string() }),
        _ = cancellation.cancelled() => return Err(M3HubError::Cancelled { operation: "stream local inference".to_string() }),
        chunk = stream.next() => chunk,
    } {
        let chunk = chunk.map_err(|error| M3HubError::Transport(error.to_string()))?;
        observed = observed.saturating_add(chunk.len());
        if observed > MAX_INFERENCE_RESPONSE_BYTES {
            return Err(M3HubError::Runtime(
                "local inference stream exceeds the byte limit".to_string(),
            ));
        }
        buffer.extend_from_slice(&chunk);
        while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = buffer.drain(..=position).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = std::str::from_utf8(&line)
                .map_err(|_| M3HubError::Runtime("local stream is not UTF-8".to_string()))?;
            ingest_sse_line(line, request, sink, &mut state)?;
        }
    }
    if !buffer.is_empty() {
        let line = std::str::from_utf8(&buffer)
            .map_err(|_| M3HubError::Runtime("local stream is not UTF-8".to_string()))?;
        ingest_sse_line(line.trim_end_matches('\r'), request, sink, &mut state)?;
    }
    state.finish(request, sink)
}

fn ingest_sse_line(
    line: &str,
    request: &CanonicalInferenceRequest,
    sink: &mut dyn M3CanonicalStreamSink,
    state: &mut OpenAiStreamState,
) -> M3HubResult<()> {
    if line.is_empty() || line.starts_with(':') || line.starts_with("event:") {
        return Ok(());
    }
    let Some(data) = line.strip_prefix("data:") else {
        return Err(M3HubError::Runtime(
            "local stream contains a non-SSE line".to_string(),
        ));
    };
    let data = data.trim_start();
    if data == "[DONE]" {
        state.saw_done = true;
        return Ok(());
    }
    if state.saw_done {
        return Err(M3HubError::Runtime(
            "local stream emitted data after [DONE]".to_string(),
        ));
    }
    let value: Value = serde_json::from_str(data)?;
    state.ingest(&value, request, sink)
}

// Process/runtime construction and the public constructor follow below.

struct ManagedChildRecord {
    handle: ManagedProcessHandle,
    child: Child,
    log_path: PathBuf,
}

/// Structured, shell-free child-process controller for managed llama.cpp.
/// Children are killed on controller drop, and every launch is readiness-
/// checked on the exact loopback port before it is published to the adapter.
pub struct SystemManagedProcessController {
    log_root: PathBuf,
    children: Mutex<BTreeMap<String, ManagedChildRecord>>,
    mutation: tokio::sync::Mutex<()>,
}

impl SystemManagedProcessController {
    pub fn new(log_root: impl AsRef<Path>) -> M3HubResult<Self> {
        let log_root = log_root.as_ref().to_path_buf();
        ensure_private_directory(&log_root)?;
        Ok(Self {
            log_root,
            children: Mutex::new(BTreeMap::new()),
            mutation: tokio::sync::Mutex::new(()),
        })
    }

    fn runtime_lock(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, ManagedChildRecord>>, RuntimeAdapterError>
    {
        self.children
            .lock()
            .map_err(|_| RuntimeAdapterError::LockPoisoned)
    }

    fn owned_port(&self, port: u16) -> Result<Option<PortOwnership>, RuntimeAdapterError> {
        let mut children = self.runtime_lock()?;
        for record in children.values_mut() {
            if record.handle.port != port {
                continue;
            }
            match record.child.try_wait() {
                Ok(None) => {
                    return Ok(Some(PortOwnership {
                        port,
                        owner_id: record.handle.process_id.clone(),
                        runtime: Some(RuntimeKind::LlamaCpp),
                        ownership: ResidencyOwnership::AppManaged,
                    }))
                }
                Ok(Some(_)) => continue,
                Err(error) => {
                    return Err(RuntimeAdapterError::Controller {
                        operation: "inspect managed process".to_string(),
                        message: error.to_string(),
                    })
                }
            }
        }
        Ok(None)
    }

    async fn loopback_port_reachable(port: u16) -> bool {
        tokio::time::timeout(
            Duration::from_millis(250),
            tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)),
        )
        .await
        .is_ok_and(|result| result.is_ok())
    }

    fn create_log_file(&self, process_id: &str) -> Result<(PathBuf, File), RuntimeAdapterError> {
        let path = self.log_root.join(format!("{process_id}.log"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options
            .open(&path)
            .map_err(|error| RuntimeAdapterError::Controller {
                operation: "create managed runtime log".to_string(),
                message: error.to_string(),
            })?;
        Ok((path, file))
    }

    /// Kills, reaps, and unregisters every child launched by this exact
    /// controller, then verifies its loopback ports are no longer listening.
    /// This is synchronous by design so it remains usable in Tauri's final
    /// `RunEvent::Exit` callback after async task scheduling has stopped.
    pub fn shutdown_all_blocking(&self, timeout: Duration) -> Result<usize, String> {
        let mut records = {
            let mut children = self
                .children
                .lock()
                .map_err(|_| "managed runtime process lock is poisoned".to_string())?;
            std::mem::take(&mut *children)
                .into_values()
                .collect::<Vec<_>>()
        };
        let count = records.len();
        let ports = records
            .iter()
            .map(|record| record.handle.port)
            .collect::<BTreeSet<_>>();
        let mut errors = Vec::new();
        for record in &mut records {
            match record.child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) => {
                    if let Err(error) = record.child.start_kill() {
                        errors.push(format!(
                            "kill managed runtime {}: {error}",
                            record.handle.process_id
                        ));
                    }
                }
                Err(error) => errors.push(format!(
                    "inspect managed runtime {}: {error}",
                    record.handle.process_id
                )),
            }
        }

        let deadline = std::time::Instant::now() + timeout;
        while !records.is_empty() && std::time::Instant::now() < deadline {
            records.retain_mut(|record| match record.child.try_wait() {
                Ok(Some(_)) => false,
                Ok(None) => true,
                Err(error) => {
                    errors.push(format!(
                        "reap managed runtime {}: {error}",
                        record.handle.process_id
                    ));
                    false
                }
            });
            if !records.is_empty() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        for record in &mut records {
            let _ = record.child.start_kill();
            errors.push(format!(
                "managed runtime {} did not exit before the shutdown deadline",
                record.handle.process_id
            ));
        }

        for port in ports {
            while std::time::Instant::now() < deadline
                && std::net::TcpStream::connect_timeout(
                    &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                    Duration::from_millis(20),
                )
                .is_ok()
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            if std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                Duration::from_millis(20),
            )
            .is_ok()
            {
                errors.push(format!(
                    "managed runtime loopback port {port} remained open after shutdown"
                ));
            }
        }

        if errors.is_empty() {
            Ok(count)
        } else {
            Err(errors.join("; "))
        }
    }
}

impl M3OwnedProcessShutdown for SystemManagedProcessController {
    fn shutdown_all_blocking(&self, timeout: Duration) -> Result<usize, String> {
        SystemManagedProcessController::shutdown_all_blocking(self, timeout)
    }
}

impl Drop for SystemManagedProcessController {
    fn drop(&mut self) {
        let _ = self.shutdown_all_blocking(Duration::from_secs(2));
    }
}

impl ManagedProcessController for SystemManagedProcessController {
    fn port_owner<'a>(
        &'a self,
        port: u16,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, Option<PortOwnership>> {
        Box::pin(async move {
            context.preflight("inspect managed runtime port")?;
            if let Some(owner) = self.owned_port(port)? {
                return Ok(Some(owner));
            }
            if Self::loopback_port_reachable(port).await {
                Ok(Some(PortOwnership {
                    port,
                    owner_id: format!("external-loopback-port-{port}"),
                    runtime: None,
                    ownership: ResidencyOwnership::PreExisting,
                }))
            } else {
                Ok(None)
            }
        })
    }

    fn launch<'a>(
        &'a self,
        spec: ManagedProcessSpec,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ManagedProcessHandle> {
        Box::pin(async move {
            context.preflight("launch managed llama.cpp")?;
            spec.validate(context.limits.max_config_bytes)?;
            verify_executable(&spec.program)?;
            let _mutation = self.mutation.lock().await;
            if let Some(owner) = self.port_owner(spec.port, context).await? {
                return Err(RuntimeAdapterError::PortCollision {
                    port: spec.port,
                    owner_id: owner.owner_id,
                });
            }
            let process_id = format!("{}-{}", spec.runtime_id, Uuid::new_v4());
            let (log_path, stdout) = self.create_log_file(&process_id)?;
            let stderr = stdout
                .try_clone()
                .map_err(|error| RuntimeAdapterError::Controller {
                    operation: "clone managed runtime log".to_string(),
                    message: error.to_string(),
                })?;
            let mut command = tokio::process::Command::new(&spec.program);
            command
                .args(&spec.args)
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr))
                .kill_on_drop(true);
            let mut child = command
                .spawn()
                .map_err(|error| RuntimeAdapterError::Controller {
                    operation: "spawn managed llama.cpp".to_string(),
                    message: error.to_string(),
                })?;
            let handle = ManagedProcessHandle {
                process_id: process_id.clone(),
                os_pid: child.id(),
                port: spec.port,
                started_at_ms: now_ms().map_err(|error| RuntimeAdapterError::Controller {
                    operation: "timestamp managed llama.cpp".to_string(),
                    message: error.to_string(),
                })?,
            };
            let deadline = tokio::time::Instant::now()
                + Duration::from_millis(context.limits.timeout_ms.min(60_000));
            loop {
                context.preflight("wait for managed llama.cpp readiness")?;
                match child
                    .try_wait()
                    .map_err(|error| RuntimeAdapterError::Controller {
                        operation: "inspect launched llama.cpp".to_string(),
                        message: error.to_string(),
                    })? {
                    Some(status) => {
                        return Err(RuntimeAdapterError::Controller {
                            operation: "wait for managed llama.cpp readiness".to_string(),
                            message: format!("process exited before readiness: {status}"),
                        })
                    }
                    None if Self::loopback_port_reachable(spec.port).await => break,
                    None if tokio::time::Instant::now() >= deadline => {
                        let _ = child.kill().await;
                        return Err(RuntimeAdapterError::Timeout {
                            operation: "wait for managed llama.cpp readiness".to_string(),
                            timeout_ms: context.limits.timeout_ms.min(60_000),
                        });
                    }
                    None => {
                        tokio::select! {
                            _ = context.cancellation.cancelled() => {
                                let _ = child.kill().await;
                                return Err(RuntimeAdapterError::Cancelled { operation: "wait for managed llama.cpp readiness".to_string() });
                            }
                            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                        }
                    }
                }
            }
            self.runtime_lock()?.insert(
                process_id,
                ManagedChildRecord {
                    handle: handle.clone(),
                    child,
                    log_path,
                },
            );
            Ok(handle)
        })
    }

    fn inspect<'a>(
        &'a self,
        handle: &'a ManagedProcessHandle,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ManagedProcessStatus> {
        Box::pin(async move {
            context.preflight("inspect managed llama.cpp")?;
            let mut children = self.runtime_lock()?;
            let Some(record) = children.get_mut(&handle.process_id) else {
                return Ok(ManagedProcessStatus {
                    handle: handle.clone(),
                    state: ManagedProcessState::Exited,
                    exit_code: None,
                    message: Some("owned process is no longer registered".to_string()),
                });
            };
            if record.handle != *handle {
                return Err(RuntimeAdapterError::Controller {
                    operation: "inspect managed llama.cpp".to_string(),
                    message: "process handle does not match the owned record".to_string(),
                });
            }
            match record.child.try_wait() {
                Ok(None) => Ok(ManagedProcessStatus {
                    handle: handle.clone(),
                    state: ManagedProcessState::Ready,
                    exit_code: None,
                    message: None,
                }),
                Ok(Some(status)) => Ok(ManagedProcessStatus {
                    handle: handle.clone(),
                    state: ManagedProcessState::Exited,
                    exit_code: status.code(),
                    message: Some(format!("managed llama.cpp exited: {status}")),
                }),
                Err(error) => Ok(ManagedProcessStatus {
                    handle: handle.clone(),
                    state: ManagedProcessState::Failed,
                    exit_code: None,
                    message: Some(error.to_string()),
                }),
            }
        })
    }

    fn terminate<'a>(
        &'a self,
        handle: &'a ManagedProcessHandle,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            context.preflight("terminate managed llama.cpp")?;
            let _mutation = self.mutation.lock().await;
            let record = self.runtime_lock()?.remove(&handle.process_id);
            let Some(mut record) = record else {
                return Ok(());
            };
            if record.handle != *handle {
                return Err(RuntimeAdapterError::Controller {
                    operation: "terminate managed llama.cpp".to_string(),
                    message: "process handle does not match the owned record".to_string(),
                });
            }
            if record
                .child
                .try_wait()
                .map_err(|error| RuntimeAdapterError::Controller {
                    operation: "inspect process before termination".to_string(),
                    message: error.to_string(),
                })?
                .is_none()
            {
                record
                    .child
                    .kill()
                    .await
                    .map_err(|error| RuntimeAdapterError::Controller {
                        operation: "kill managed llama.cpp".to_string(),
                        message: error.to_string(),
                    })?;
            }
            Ok(())
        })
    }

    fn tail_logs<'a>(
        &'a self,
        handle: &'a ManagedProcessHandle,
        max_bytes: usize,
        context: &'a RuntimeOperationContext,
    ) -> RuntimeFuture<'a, ManagedLogChunk> {
        Box::pin(async move {
            context.preflight("tail managed llama.cpp logs")?;
            if max_bytes == 0 || max_bytes > context.limits.max_log_bytes {
                return Err(RuntimeAdapterError::LogTooLarge {
                    limit: context.limits.max_log_bytes,
                    actual: max_bytes,
                });
            }
            let path = self
                .runtime_lock()?
                .get(&handle.process_id)
                .filter(|record| record.handle == *handle)
                .map(|record| record.log_path.clone())
                .ok_or_else(|| RuntimeAdapterError::Controller {
                    operation: "tail managed llama.cpp logs".to_string(),
                    message: "owned process is no longer registered".to_string(),
                })?;
            read_log_tail(&path, max_bytes)
        })
    }
}

fn verify_executable(path: &Path) -> Result<(), RuntimeAdapterError> {
    if !path.is_absolute() {
        return Err(RuntimeAdapterError::InvalidProcessSpec {
            message: "managed executable must be absolute".to_string(),
        });
    }
    // Resolve symlinks first: PATH-discovered binaries (e.g. Homebrew's
    // `/opt/homebrew/bin/llama-server`) are routinely symlinks into a
    // versioned Cellar path. The tamper check below must apply to the real
    // target file, not the link itself, or every symlinked install fails
    // closed here.
    let real_path = fs::canonicalize(path).map_err(|error| RuntimeAdapterError::Controller {
        operation: "resolve managed executable".to_string(),
        message: error.to_string(),
    })?;
    let metadata =
        fs::symlink_metadata(&real_path).map_err(|error| RuntimeAdapterError::Controller {
            operation: "inspect managed executable".to_string(),
            message: error.to_string(),
        })?;
    if !metadata.file_type().is_file() {
        return Err(RuntimeAdapterError::InvalidProcessSpec {
            message: "managed executable must be a real regular file".to_string(),
        });
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(RuntimeAdapterError::InvalidProcessSpec {
            message: "managed executable is not executable".to_string(),
        });
    }
    Ok(())
}

fn read_log_tail(path: &Path, max_bytes: usize) -> Result<ManagedLogChunk, RuntimeAdapterError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| RuntimeAdapterError::Controller {
        operation: "inspect managed runtime log".to_string(),
        message: error.to_string(),
    })?;
    if !metadata.file_type().is_file() {
        return Err(RuntimeAdapterError::Controller {
            operation: "inspect managed runtime log".to_string(),
            message: "log path is not a regular file".to_string(),
        });
    }
    let total = metadata.len();
    let start = total.saturating_sub(max_bytes as u64);
    let mut file = File::open(path).map_err(|error| RuntimeAdapterError::Controller {
        operation: "open managed runtime log".to_string(),
        message: error.to_string(),
    })?;
    file.seek(SeekFrom::Start(start))
        .map_err(|error| RuntimeAdapterError::Controller {
            operation: "seek managed runtime log".to_string(),
            message: error.to_string(),
        })?;
    let mut bytes = Vec::with_capacity((total - start) as usize);
    file.read_to_end(&mut bytes)
        .map_err(|error| RuntimeAdapterError::Controller {
            operation: "read managed runtime log".to_string(),
            message: error.to_string(),
        })?;
    Ok(ManagedLogChunk {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        truncated: start > 0,
    })
}

fn ensure_private_directory(path: &Path) -> M3HubResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(M3HubError::State(format!(
                "{} is not a real directory",
                path.display()
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|source| M3HubError::Io {
                operation: "create private M3 directory",
                path: path.to_path_buf(),
                source,
            })?;
        }
        Err(source) => {
            return Err(M3HubError::Io {
                operation: "inspect private M3 directory",
                path: path.to_path_buf(),
                source,
            })
        }
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
        M3HubError::Io {
            operation: "secure private M3 directory",
            path: path.to_path_buf(),
            source,
        }
    })?;
    Ok(())
}

#[derive(Default)]
struct ProductionMlxSignatureVerifier;

impl MlxSignatureVerifier for ProductionMlxSignatureVerifier {
    fn verify(
        &self,
        algorithm: &str,
        key_id: &str,
        signed_payload: &[u8],
        signature_bytes: &[u8],
    ) -> Result<(), String> {
        if algorithm != "ed25519" || key_id != MLX_RELEASE_KEY_ID {
            return Err("MLX package is not signed by the pinned release key".to_string());
        }
        let public_key = decode_hex(MLX_RELEASE_PUBLIC_KEY_HEX)?;
        signature::UnparsedPublicKey::new(&signature::ED25519, public_key)
            .verify(signed_payload, signature_bytes)
            .map_err(|_| "MLX package Ed25519 signature is invalid".to_string())
    }
}

fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    if !value.len().is_multiple_of(2) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("pinned key is not valid hexadecimal".to_string());
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .map_err(|_| "pinned key is not UTF-8 hexadecimal".to_string())
                .and_then(|pair| {
                    u8::from_str_radix(pair, 16)
                        .map_err(|_| "pinned key is not valid hexadecimal".to_string())
                })
        })
        .collect()
}

/// Controller for the verified app-private MLX service package. The package
/// process is supervised by the same structured process boundary as
/// llama.cpp; generation uses a loopback-only SSE endpoint whose data values
/// are the versioned [`MlxStreamEvent`] schema.
struct ProductionMlxServiceController {
    process: Arc<SystemManagedProcessController>,
    handles: Mutex<BTreeMap<String, ManagedProcessHandle>>,
    cancellations: Mutex<BTreeMap<String, CancellationToken>>,
    generated_tokens: AtomicU64,
    client: reqwest::Client,
}

impl ProductionMlxServiceController {
    fn new(process: Arc<SystemManagedProcessController>) -> M3HubResult<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| M3HubError::Transport(error.to_string()))?;
        Ok(Self {
            process,
            handles: Mutex::new(BTreeMap::new()),
            cancellations: Mutex::new(BTreeMap::new()),
            generated_tokens: AtomicU64::new(0),
            client,
        })
    }

    fn runtime_context(context: &MlxOperationContext) -> RuntimeOperationContext {
        let limits = RuntimeOperationLimits {
            timeout_ms: context.timeout_ms,
            ..RuntimeOperationLimits::default()
        };
        RuntimeOperationContext::new(limits, context.cancellation.clone())
    }

    fn controller_error(operation: &str, error: impl std::fmt::Display) -> MlxError {
        MlxError::Controller {
            operation: operation.to_string(),
            message: error.to_string(),
        }
    }
}

impl MlxServiceController for ProductionMlxServiceController {
    fn port_owner<'a>(&'a self, port: u16) -> MlxFuture<'a, Option<String>> {
        Box::pin(async move {
            let context = RuntimeOperationContext::default();
            self.process
                .port_owner(port, &context)
                .await
                .map(|owner| owner.map(|owner| owner.owner_id))
                .map_err(|error| Self::controller_error("inspect MLX port", error))
        })
    }

    fn launch<'a>(
        &'a self,
        spec: MlxLaunchSpec,
        context: &'a MlxOperationContext,
    ) -> MlxFuture<'a, MlxProcessHandle> {
        Box::pin(async move {
            let runtime_context = Self::runtime_context(context);
            let handle = self
                .process
                .launch(
                    ManagedProcessSpec {
                        runtime_id: spec.runtime_id,
                        program: spec.program,
                        args: spec.args,
                        port: spec.port,
                    },
                    &runtime_context,
                )
                .await
                .map_err(|error| Self::controller_error("launch MLX service", error))?;
            lock(&self.handles)
                .map_err(|error| Self::controller_error("record MLX process", error))?
                .insert(handle.process_id.clone(), handle.clone());
            Ok(MlxProcessHandle {
                process_id: handle.process_id,
                os_pid: handle.os_pid,
                port: handle.port,
                model_id: spec.model_id,
                started_at_ms: handle.started_at_ms,
            })
        })
    }

    fn inspect<'a>(
        &'a self,
        handle: &'a MlxProcessHandle,
        context: &'a MlxOperationContext,
    ) -> MlxFuture<'a, MlxProcessMetrics> {
        Box::pin(async move {
            let managed = lock(&self.handles)
                .map_err(|error| Self::controller_error("read MLX process", error))?
                .get(&handle.process_id)
                .cloned();
            let process_alive = if let Some(managed) = managed {
                let runtime_context = Self::runtime_context(context);
                matches!(
                    self.process
                        .inspect(&managed, &runtime_context)
                        .await
                        .map_err(|error| Self::controller_error("inspect MLX service", error))?
                        .state,
                    ManagedProcessState::Starting | ManagedProcessState::Ready
                )
            } else {
                false
            };
            let resident_memory_bytes = if process_alive {
                handle
                    .os_pid
                    .and_then(process_resident_memory_bytes)
                    .unwrap_or(0)
            } else {
                0
            };
            Ok(MlxProcessMetrics {
                process_alive,
                resident_memory_bytes,
                unified_memory_bytes: if cfg!(target_os = "macos") {
                    resident_memory_bytes
                } else {
                    0
                },
                active_requests: lock(&self.cancellations)
                    .map_err(|error| Self::controller_error("read MLX requests", error))?
                    .len() as u64,
                generated_tokens: self.generated_tokens.load(Ordering::Relaxed),
                tokens_per_second: None,
                sampled_at_ms: now_ms()
                    .map_err(|error| Self::controller_error("timestamp MLX metrics", error))?,
            })
        })
    }

    fn stream<'a>(
        &'a self,
        handle: &'a MlxProcessHandle,
        request: &'a MlxGenerationRequest,
        sink: &'a mut dyn MlxStreamSink,
        context: &'a MlxOperationContext,
    ) -> MlxFuture<'a, MlxGenerationSummary> {
        Box::pin(async move {
            let cancellation = CancellationToken::new();
            {
                let mut cancellations = lock(&self.cancellations)
                    .map_err(|error| Self::controller_error("register MLX request", error))?;
                if cancellations.contains_key(&request.request_id) {
                    return Err(MlxError::RequestAlreadyRunning(request.request_id.clone()));
                }
                cancellations.insert(request.request_id.clone(), cancellation.clone());
            }
            let result = async {
                let response = tokio::select! {
                    _ = context.cancellation.cancelled() => return Err(MlxError::Cancelled { operation: "stream".to_string() }),
                    _ = cancellation.cancelled() => return Err(MlxError::Cancelled { operation: "stream".to_string() }),
                    response = self.client.post(format!("http://127.0.0.1:{}/v1/generate", handle.port)).json(request).send() => {
                        response.map_err(|error| Self::controller_error("start MLX stream", error))?
                    }
                };
                if !response.status().is_success() {
                    return Err(Self::controller_error(
                        "start MLX stream",
                        format!("HTTP {}", response.status()),
                    ));
                }
                let mut bytes = response.bytes_stream();
                let mut buffer = Vec::new();
                let mut observed = 0_usize;
                let mut completed = None;
                let mut used_tool = false;
                while let Some(chunk) = tokio::select! {
                    _ = context.cancellation.cancelled() => return Err(MlxError::Cancelled { operation: "stream".to_string() }),
                    _ = cancellation.cancelled() => return Err(MlxError::Cancelled { operation: "stream".to_string() }),
                    chunk = bytes.next() => chunk,
                } {
                    let chunk = chunk.map_err(|error| Self::controller_error("read MLX stream", error))?;
                    observed = observed.saturating_add(chunk.len());
                    if observed > 64 * 1024 * 1024 {
                        return Err(MlxError::Limit {
                            name: "MLX service stream bytes",
                            observed: observed as u64,
                            max: 64 * 1024 * 1024,
                        });
                    }
                    buffer.extend_from_slice(&chunk);
                    while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
                        let mut line = buffer.drain(..=position).collect::<Vec<_>>();
                        line.pop();
                        if line.last() == Some(&b'\r') {
                            line.pop();
                        }
                        ingest_mlx_service_line(
                            &line,
                            sink,
                            &mut completed,
                            &mut used_tool,
                        )?;
                    }
                }
                if !buffer.is_empty() {
                    ingest_mlx_service_line(&buffer, sink, &mut completed, &mut used_tool)?;
                }
                let (input_tokens, output_tokens) = completed.ok_or_else(|| {
                    MlxError::StreamProtocol(
                        "MLX service stream ended without a completed event".to_string(),
                    )
                })?;
                Ok(MlxGenerationSummary {
                    request_id: request.request_id.clone(),
                    input_tokens,
                    output_tokens,
                    finish_reason: if used_tool { "tool_use" } else { "stop" }.to_string(),
                })
            }
            .await;
            if let Ok(summary) = &result {
                self.generated_tokens
                    .fetch_add(summary.output_tokens, Ordering::Relaxed);
            }
            if let Ok(mut cancellations) = self.cancellations.lock() {
                cancellations.remove(&request.request_id);
            }
            result
        })
    }

    fn cancel<'a>(
        &'a self,
        _handle: &'a MlxProcessHandle,
        request_id: &'a str,
    ) -> MlxFuture<'a, ()> {
        Box::pin(async move {
            if let Some(cancellation) = lock(&self.cancellations)
                .map_err(|error| Self::controller_error("cancel MLX request", error))?
                .get(request_id)
            {
                cancellation.cancel();
            }
            Ok(())
        })
    }

    fn terminate_and_wait<'a>(
        &'a self,
        handle: &'a MlxProcessHandle,
        _timeout_ms: u64,
        context: &'a MlxOperationContext,
    ) -> MlxFuture<'a, MlxProcessMetrics> {
        Box::pin(async move {
            for cancellation in lock(&self.cancellations)
                .map_err(|error| Self::controller_error("cancel MLX requests", error))?
                .values()
            {
                cancellation.cancel();
            }
            let managed = lock(&self.handles)
                .map_err(|error| Self::controller_error("remove MLX process", error))?
                .remove(&handle.process_id);
            if let Some(managed) = managed {
                let runtime_context = Self::runtime_context(context);
                self.process
                    .terminate(&managed, &runtime_context)
                    .await
                    .map_err(|error| Self::controller_error("terminate MLX service", error))?;
            }
            Ok(MlxProcessMetrics {
                process_alive: false,
                resident_memory_bytes: 0,
                unified_memory_bytes: 0,
                active_requests: 0,
                generated_tokens: 0,
                tokens_per_second: None,
                sampled_at_ms: now_ms()
                    .map_err(|error| Self::controller_error("timestamp MLX unload", error))?,
            })
        })
    }

    fn tail_logs<'a>(
        &'a self,
        handle: &'a MlxProcessHandle,
        max_bytes: usize,
        context: &'a MlxOperationContext,
    ) -> MlxFuture<'a, String> {
        Box::pin(async move {
            let managed = lock(&self.handles)
                .map_err(|error| Self::controller_error("read MLX process", error))?
                .get(&handle.process_id)
                .cloned()
                .ok_or(MlxError::NotRunning)?;
            let runtime_context = Self::runtime_context(context);
            self.process
                .tail_logs(&managed, max_bytes, &runtime_context)
                .await
                .map(|logs| logs.text)
                .map_err(|error| Self::controller_error("tail MLX logs", error))
        })
    }
}

fn ingest_mlx_service_line(
    raw_line: &[u8],
    sink: &mut dyn MlxStreamSink,
    completed: &mut Option<(u64, u64)>,
    used_tool: &mut bool,
) -> Result<(), MlxError> {
    let line = std::str::from_utf8(raw_line)
        .map_err(|_| MlxError::StreamProtocol("MLX service stream is not UTF-8".to_string()))?
        .trim();
    if line.is_empty() || line.starts_with(':') || line.starts_with("event:") {
        return Ok(());
    }
    let data = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
    if data == "[DONE]" {
        return Ok(());
    }
    let event: MlxStreamEvent = serde_json::from_str(data)?;
    if matches!(event, MlxStreamEvent::ToolCallStart { .. }) {
        *used_tool = true;
    }
    if let MlxStreamEvent::Completed {
        input_tokens,
        output_tokens,
    } = &event
    {
        *completed = Some((*input_tokens, *output_tokens));
    }
    sink.emit(event).map_err(MlxError::StreamProtocol)
}

#[cfg(unix)]
fn process_resident_memory_bytes(pid: u32) -> Option<u64> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?
        .checked_mul(1_024)
}

#[cfg(not(unix))]
fn process_resident_memory_bytes(_pid: u32) -> Option<u64> {
    None
}

struct ProductionMlxComponents {
    installer: Arc<MlxPackageInstaller>,
    controller: Arc<ProductionMlxServiceController>,
}

struct ProductionRuntimeFactory {
    root: PathBuf,
    clock: Arc<dyn M3Clock>,
    process_controller: Arc<SystemManagedProcessController>,
}

impl ProductionRuntimeFactory {
    fn build_all(
        &self,
        installed: &[M3InstalledModelView],
    ) -> M3HubResult<Vec<Arc<dyn M3RuntimeDriver>>> {
        let hardware = SystemM3HardwareProbe.snapshot()?;
        let mut drivers = vec![build_ollama_driver(hardware.platform.clone())?];
        drivers.extend(self.build_managed(installed, hardware.platform)?);
        Ok(drivers)
    }

    fn build_managed(
        &self,
        installed: &[M3InstalledModelView],
        platform: PlatformCapabilities,
    ) -> M3HubResult<Vec<Arc<dyn M3RuntimeDriver>>> {
        let mut drivers = Vec::new();
        if let Some(binary) = find_production_llama_binary(&self.root)? {
            let models = runtime_models(installed, M3RuntimeKind::LlamaCpp)?;
            let structured_output_models = installed
                .iter()
                .filter(|model| {
                    model.runtime == M3RuntimeKind::LlamaCpp && model.capabilities.structured_output
                })
                .map(|model| model.model_id.clone())
                .collect();
            let adapter: Arc<dyn RuntimeAdapter> = Arc::new(
                ManagedLlamaCppAdapter::new(
                    LLAMA_RUNTIME_ID,
                    LLAMA_ENDPOINT,
                    binary,
                    LLAMA_PORT,
                    self.process_controller.clone(),
                    models,
                    platform,
                )
                .map_err(runtime_error)?,
            );
            let inference: Arc<dyn M3InferenceEngine> =
                Arc::new(CapabilityCheckedInferenceEngine {
                    adapter: adapter.clone(),
                    inner: Arc::new(OpenAiCompatibleM3InferenceEngine::new(LLAMA_ENDPOINT)?),
                    structured_output_models,
                });
            drivers.push(Arc::new(RuntimeAdapterM3Driver::new(adapter, inference)?)
                as Arc<dyn M3RuntimeDriver>);
        }
        if let Some(mlx) = production_mlx_components(&self.root, self.process_controller.clone())? {
            let models = mlx_models(installed)?;
            let adapter = Arc::new(
                MlxRuntimeAdapter::new(
                    MlxRuntimeConfig::default(),
                    Arc::new(CurrentHostMlxProbe),
                    mlx.installer,
                    mlx.controller,
                    models,
                )
                .map_err(|error| M3HubError::Runtime(error.to_string()))?,
            );
            drivers.push(
                Arc::new(MlxM3Driver::new("mlx", adapter, self.clock.clone())?)
                    as Arc<dyn M3RuntimeDriver>,
            );
        }
        Ok(drivers)
    }
}

struct ProductionRuntimeReconciler {
    factory: Arc<ProductionRuntimeFactory>,
    current: Mutex<ProductionRuntimeSnapshot>,
}

struct ProductionRuntimeSnapshot {
    inventory_signature: Vec<(String, String)>,
    drivers: Vec<Arc<dyn M3RuntimeDriver>>,
}

impl ProductionRuntimeReconciler {
    fn new(
        factory: Arc<ProductionRuntimeFactory>,
        installed: &[M3InstalledModelView],
        current: &[Arc<dyn M3RuntimeDriver>],
    ) -> Self {
        Self {
            factory,
            current: Mutex::new(ProductionRuntimeSnapshot {
                inventory_signature: runtime_inventory_signature(installed),
                drivers: current.to_vec(),
            }),
        }
    }
}

fn runtime_inventory_signature(installed: &[M3InstalledModelView]) -> Vec<(String, String)> {
    let mut signature = installed
        .iter()
        .map(|model| (model.asset_id.clone(), model.active_version_key.clone()))
        .collect::<Vec<_>>();
    signature.sort();
    signature
}

impl M3RuntimeReconciler for ProductionRuntimeReconciler {
    fn reconcile<'a>(
        &'a self,
        installed: &'a [M3InstalledModelView],
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, Vec<Arc<dyn M3RuntimeDriver>>> {
        Box::pin(async move {
            let (current_signature, current_drivers) = {
                let current = lock(&self.current)?;
                (current.inventory_signature.clone(), current.drivers.clone())
            };
            let requested_signature = runtime_inventory_signature(installed);
            let mut managed_running = false;
            for driver in current_drivers
                .iter()
                .filter(|driver| driver.descriptor().kind != M3RuntimeKind::Ollama)
            {
                let running = match driver.status(context).await? {
                    M3RuntimeStatusView::Adapter { running_models, .. } => {
                        !running_models.is_empty()
                    }
                    M3RuntimeStatusView::Mlx { status } => {
                        matches!(status, crate::mlx_runtime::MlxRuntimeStatus::Running { .. })
                    }
                };
                if running {
                    managed_running = true;
                    break;
                }
            }
            if managed_running {
                if requested_signature != current_signature {
                    return Err(M3HubError::Conflict(
                        "managed runtime inventory cannot change while a model is loaded"
                            .to_string(),
                    ));
                }
                // A factory refresh while a model is resident is safe but
                // deliberately deferred: replacing the driver would lose its
                // ownership handle. Return the live drivers unchanged.
                return Ok(current_drivers);
            }
            let drivers = self.factory.build_all(installed)?;
            *lock(&self.current)? = ProductionRuntimeSnapshot {
                inventory_signature: requested_signature,
                drivers: drivers.clone(),
            };
            Ok(drivers)
        })
    }
}

fn runtime_models(
    installed: &[M3InstalledModelView],
    runtime: M3RuntimeKind,
) -> M3HubResult<Vec<RuntimeModel>> {
    let mut model_ids = BTreeSet::new();
    installed
        .iter()
        .filter(|model| model.runtime == runtime)
        .map(|model| {
            if !model_ids.insert(model.model_id.clone()) {
                return Err(M3HubError::Conflict(format!(
                    "managed runtime cannot expose multiple active variants named {}",
                    model.model_id
                )));
            }
            let version = active_version(model)?;
            Ok(RuntimeModel {
                model_id: model.model_id.clone(),
                display_name: model.display_name.clone(),
                size_bytes: version.size_bytes,
                local_path: Some(version.artifact_path.clone()),
                digest: Some(version.sha256.clone()),
                modified_at: Some(version.installed_at_ms.to_string()),
                capabilities: ModelCapabilities {
                    chat: model_capabilities(model).chat,
                    embeddings: model_capabilities(model).embeddings,
                    tool_calling: model_capabilities(model).tool_calling,
                    vision: model_capabilities(model).vision,
                },
                metadata: BTreeMap::from([
                    ("assetId".to_string(), model.asset_id.clone()),
                    ("variantId".to_string(), model.variant_id.clone()),
                    ("revision".to_string(), version.revision.clone()),
                ]),
            })
        })
        .collect()
}

fn mlx_models(installed: &[M3InstalledModelView]) -> M3HubResult<Vec<MlxModelRecord>> {
    let mut model_ids = BTreeSet::new();
    installed
        .iter()
        .filter(|model| model.runtime == M3RuntimeKind::Mlx)
        .map(|model| {
            if !model_ids.insert(model.model_id.clone()) {
                return Err(M3HubError::Conflict(format!(
                    "MLX cannot expose multiple active variants named {}",
                    model.model_id
                )));
            }
            let version = active_version(model)?;
            let capabilities = model_capabilities(model);
            Ok(MlxModelRecord {
                model_id: model.model_id.clone(),
                display_name: model.display_name.clone(),
                local_path: version.artifact_path.clone(),
                size_bytes: version.size_bytes,
                revision: Some(version.revision.clone()),
                capabilities: MlxModelCapabilities {
                    chat: capabilities.chat,
                    tool_calling: capabilities.tool_calling,
                    vision: capabilities.vision,
                    structured_output: capabilities.structured_output,
                },
            })
        })
        .collect()
}

fn active_version(
    model: &M3InstalledModelView,
) -> M3HubResult<&crate::m3_runtime_hub::M3InstalledVersionView> {
    model
        .versions
        .iter()
        .find(|version| version.version_key == model.active_version_key && version.active)
        .ok_or_else(|| {
            M3HubError::State(format!(
                "installed model {} has no active version",
                model.asset_id
            ))
        })
}

fn model_capabilities(model: &M3InstalledModelView) -> &M3ModelCapabilities {
    &model.capabilities
}

fn find_production_llama_binary(root: &Path) -> M3HubResult<Option<PathBuf>> {
    #[cfg(target_os = "windows")]
    let filename = "llama-server.exe";
    #[cfg(not(target_os = "windows"))]
    let filename = "llama-server";
    let managed = root
        .join("runtimes")
        .join("llama")
        .join("current")
        .join(filename);
    match fs::symlink_metadata(&managed) {
        Ok(_) => {
            verify_executable(&managed).map_err(runtime_error)?;
            return Ok(Some(managed));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(M3HubError::Io {
                operation: "inspect app-private llama.cpp runtime",
                path: managed,
                source,
            })
        }
    }
    match crate::llama::find_llama_server_binary() {
        Ok(path) => {
            let path = PathBuf::from(path);
            verify_executable(&path).map_err(runtime_error)?;
            Ok(Some(path))
        }
        Err(_) => Ok(None),
    }
}

fn production_mlx_components(
    root: &Path,
    process: Arc<SystemManagedProcessController>,
) -> M3HubResult<Option<ProductionMlxComponents>> {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        return Ok(None);
    }
    let installer = Arc::new(
        MlxPackageInstaller::new(
            root.join("runtimes").join("mlx"),
            Arc::new(ProductionMlxSignatureVerifier),
            MlxInstallLimits::default(),
        )
        .map_err(|error| M3HubError::Runtime(error.to_string()))?,
    );
    match installer.verify_active() {
        Ok(_) | Err(MlxError::NotInstalled) => Ok(Some(ProductionMlxComponents {
            installer,
            controller: Arc::new(ProductionMlxServiceController::new(process)?),
        })),
        Err(error) => Err(M3HubError::Runtime(format!(
            "verified MLX installation is corrupt: {error}"
        ))),
    }
}

fn build_ollama_driver(platform: PlatformCapabilities) -> M3HubResult<Arc<dyn M3RuntimeDriver>> {
    let transport: Arc<dyn HttpTransport> =
        Arc::new(ReqwestHttpTransport::new().map_err(runtime_error)?);
    let adapter: Arc<dyn RuntimeAdapter> = Arc::new(
        OllamaHttpAdapter::new(
            OLLAMA_RUNTIME_ID,
            OLLAMA_ENDPOINT,
            EndpointPolicy::LoopbackOnly,
            transport,
            platform,
        )
        .map_err(runtime_error)?,
    );
    let inference: Arc<dyn M3InferenceEngine> = Arc::new(CapabilityCheckedInferenceEngine {
        adapter: adapter.clone(),
        inner: Arc::new(OpenAiCompatibleM3InferenceEngine::new(OLLAMA_ENDPOINT)?),
        // The Ollama inventory API does not expose a reliable structured-
        // output capability flag. Fail closed until a richer model card does.
        structured_output_models: BTreeSet::new(),
    });
    Ok(Arc::new(RuntimeAdapterM3Driver::new(adapter, inference)?) as Arc<dyn M3RuntimeDriver>)
}

/// Builds the fully wired production M3 command state below
/// `<app_data_dir>/m3`.
///
/// This constructor performs no model download and starts no runtime. It does
/// validate persisted state, configured catalog origins, the keychain trust
/// root, installed runtime artifacts, and the initial reconciled inventory so
/// a corrupt dependency fails closed before any command is exposed.
pub fn build_m3_command_state(app_data_dir: impl AsRef<Path>) -> M3HubResult<M3CommandState> {
    let app_data_dir = app_data_dir.as_ref();
    if !app_data_dir.is_absolute() {
        return Err(M3HubError::State(
            "Tauri app-data directory must be absolute".to_string(),
        ));
    }
    ensure_private_directory(app_data_dir)?;
    let root = app_data_dir.join(M3_DIRECTORY);
    ensure_private_directory(&root)?;
    let config = M3HubConfig::default();
    let clock: Arc<dyn M3Clock> = Arc::new(SystemM3Clock);
    let hardware: Arc<dyn M3HardwareProbe> = Arc::new(SystemM3HardwareProbe);
    let hardware_snapshot = hardware.snapshot()?;
    let download = Arc::new(ReqwestM3DownloadTransport::new()?);
    let catalogs = load_catalog_sources(&root)?;
    let protector = Arc::new(KeychainLanStateProtector::load_or_create()?);
    let lan_factory = Arc::new(DefaultM3LanAccessFactory::new(
        Arc::new(OsLanEntropy),
        protector,
    ));
    let ollama = build_ollama_driver(hardware_snapshot.platform.clone())?;

    // First load validates the durable store and exposes its exact active
    // model views. The final hub is then created with a matching runtime
    // inventory, avoiding a startup window with stale managed model paths.
    let index_hub = M3RuntimeHub::new(
        &root,
        config.clone(),
        M3RuntimeHubDependencies {
            clock: clock.clone(),
            hardware: hardware.clone(),
            download: download.clone(),
            catalogs: catalogs.clone(),
            runtimes: vec![ollama.clone()],
            runtime_reconciler: None,
            lan_factory: Some(lan_factory.clone()),
        },
    )?;
    let installed = index_hub.list_installed_models()?;
    drop(index_hub);

    let process = Arc::new(SystemManagedProcessController::new(root.join("logs"))?);
    let factory = Arc::new(ProductionRuntimeFactory {
        root: root.clone(),
        clock: clock.clone(),
        process_controller: process.clone(),
    });
    let runtimes = factory.build_all(&installed)?;
    let reconciler = Arc::new(ProductionRuntimeReconciler::new(
        factory, &installed, &runtimes,
    ));
    let hub = M3RuntimeHub::new(
        root,
        config,
        M3RuntimeHubDependencies {
            clock,
            hardware,
            download,
            catalogs,
            runtimes,
            runtime_reconciler: Some(reconciler),
            lan_factory: Some(lan_factory),
        },
    )?;
    Ok(M3CommandState::with_owned_processes(Arc::new(hub), process))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compatibility_hub::{CompatibilityProtocol, COMPATIBILITY_SCHEMA_VERSION};
    use http_body_util::{BodyExt, Full};
    use hyper::body::{Bytes, Incoming};
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;
    use std::convert::Infallible;
    use tokio::net::TcpListener;
    use tokio::sync::Notify;

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "m3-production-{label}-{}-{}",
                std::process::id(),
                Uuid::new_v4()
            ));
            fs::create_dir_all(&path).expect("create test root");
            Self(path)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Default)]
    struct EventSink(Vec<CanonicalStreamEvent>);

    impl M3CanonicalStreamSink for EventSink {
        fn emit(&mut self, event: CanonicalStreamEvent) -> Result<(), String> {
            self.0.push(event);
            Ok(())
        }
    }

    fn request(request_id: &str, model: &str, stream: bool) -> CanonicalInferenceRequest {
        CanonicalInferenceRequest {
            schema_version: COMPATIBILITY_SCHEMA_VERSION,
            protocol: CompatibilityProtocol::OpenAiChatCompletions,
            request_id: request_id.to_string(),
            model: model.to_string(),
            messages: vec![CanonicalMessage {
                role: CanonicalRole::User,
                content: vec![CanonicalContent::Text {
                    text: "hello".to_string(),
                }],
            }],
            tools: Vec::new(),
            max_output_tokens: 32,
            temperature: Some(0.2),
            stream,
            response_schema: None,
            metadata: Value::Null,
        }
    }

    async fn inference_fixture(
        request: Request<Incoming>,
        slow_started: Arc<Notify>,
    ) -> Result<Response<Full<Bytes>>, Infallible> {
        let body = request
            .into_body()
            .collect()
            .await
            .expect("collect fixture request")
            .to_bytes();
        let value: Value = serde_json::from_slice(&body).expect("fixture request JSON");
        let model = value["model"].as_str().expect("model");
        if model == "slow-model" {
            slow_started.notify_one();
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        let stream = value["stream"].as_bool().unwrap_or(false);
        if stream {
            let first = json!({
                "id":"chatcmpl-stream",
                "created":123,
                "model":model,
                "choices":[{"index":0,"delta":{"content":"hel"},"finish_reason":null}]
            });
            let second = json!({
                "id":"chatcmpl-stream",
                "created":123,
                "model":model,
                "choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":2,"completion_tokens":1}
            });
            let bytes = format!("data: {first}\n\ndata: {second}\n\ndata: [DONE]\n\n");
            Ok(Response::builder()
                .header("content-type", "text/event-stream")
                .body(Full::new(Bytes::from(bytes)))
                .expect("stream response"))
        } else {
            let body = json!({
                "id":"chatcmpl-complete",
                "created":123,
                "model":model,
                "choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":2,"completion_tokens":1}
            });
            Ok(Response::new(Full::new(Bytes::from(body.to_string()))))
        }
    }

    async fn spawn_inference_fixture() -> (String, Arc<Notify>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture");
        let address = listener.local_addr().expect("fixture address");
        let slow_started = Arc::new(Notify::new());
        let notify = slow_started.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let notify = notify.clone();
                tokio::spawn(async move {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(
                            TokioIo::new(stream),
                            service_fn(move |request| inference_fixture(request, notify.clone())),
                        )
                        .await;
                });
            }
        });
        (format!("http://{address}"), slow_started, task)
    }

    #[tokio::test]
    async fn loopback_openai_engine_completes_streams_and_cancels() {
        let (endpoint, slow_started, server) = spawn_inference_fixture().await;
        let engine =
            Arc::new(OpenAiCompatibleM3InferenceEngine::new(&endpoint).expect("production engine"));
        let context = M3OperationContext::new(10_000);
        let complete = engine
            .complete(&request("complete", "local-model", false), &context)
            .await
            .expect("completion");
        assert_eq!(complete.model, "local-model");
        assert_eq!(
            complete.content,
            vec![CanonicalContent::Text {
                text: "hello".to_string()
            }]
        );
        assert_eq!(complete.usage.output_tokens, 1);

        let mut sink = EventSink::default();
        engine
            .stream(&request("stream", "local-model", true), &mut sink, &context)
            .await
            .expect("stream");
        assert!(matches!(
            sink.0.first(),
            Some(CanonicalStreamEvent::ResponseStart { model, .. }) if model == "local-model"
        ));
        assert!(matches!(
            sink.0.last(),
            Some(CanonicalStreamEvent::ResponseCompleted { usage, .. }) if usage.output_tokens == 1
        ));

        let slow_engine = engine.clone();
        let slow = tokio::spawn(async move {
            let mut sink = EventSink::default();
            slow_engine
                .stream(
                    &request("slow", "slow-model", true),
                    &mut sink,
                    &M3OperationContext::new(60_000),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), slow_started.notified())
            .await
            .expect("slow request reached fixture");
        assert!(engine
            .cancel("slow", &M3OperationContext::default())
            .await
            .expect("cancel request"));
        assert!(matches!(
            slow.await.expect("slow task"),
            Err(M3HubError::Cancelled { .. })
        ));
        server.abort();
    }

    #[tokio::test]
    async fn catalog_loader_and_port_probe_use_bounded_loopback_origins() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind catalog fixture");
        let address = listener.local_addr().expect("catalog address");
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept catalog request");
            let service = service_fn(|_request: Request<Incoming>| async move {
                Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
                    br#"{"schemaVersion":1,"entries":[]}"#,
                ))))
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
        let root = TestRoot::new("catalog");
        fs::write(
            root.0.join(CATALOG_CONFIG_FILE),
            serde_json::to_vec(&json!({
                "schemaVersion": CATALOG_CONFIG_SCHEMA_VERSION,
                "sources":[{"sourceId":"fixture","endpoint":format!("http://{address}/catalog")}]
            }))
            .unwrap(),
        )
        .expect("write catalog config");
        let sources = load_catalog_sources(&root.0).expect("load configured sources");
        assert_eq!(sources.len(), 1);
        assert!(sources[0]
            .search("qwen", 5, &M3OperationContext::default())
            .await
            .expect("search loopback catalog")
            .is_empty());
        task.abort();

        let live_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind live catalog fixture");
        let live_address = live_listener.local_addr().expect("live catalog address");
        let live_task = tokio::spawn(async move {
            let (stream, _) = live_listener
                .accept()
                .await
                .expect("accept live catalog request");
            let service = service_fn(|_request: Request<Incoming>| async move {
                Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
                    br#"{"schemaVersion":1,"entries":[]}"#,
                ))))
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
        let live_root = root.0.join("live-hub");
        let hub = M3RuntimeHub::new(
            &live_root,
            M3HubConfig::default(),
            M3RuntimeHubDependencies {
                clock: Arc::new(SystemM3Clock),
                hardware: Arc::new(SystemM3HardwareProbe),
                download: Arc::new(ReqwestM3DownloadTransport::new().expect("download client")),
                catalogs: Vec::new(),
                runtimes: Vec::new(),
                runtime_reconciler: None,
                lan_factory: None,
            },
        )
        .expect("live catalog hub");
        let configured = vec![M3CatalogSourceConfig {
            source_id: "live-fixture".to_string(),
            endpoint: format!("http://{live_address}/catalog"),
        }];
        replace_catalog_source_configs(&hub, configured.clone())
            .expect("persist and hot-reload catalog sources");
        assert_eq!(catalog_source_configs(&live_root).unwrap(), configured);
        assert!(hub
            .search_catalog("qwen", 5, &M3OperationContext::default())
            .await
            .expect("search hot-reloaded source")
            .is_empty());
        live_task.abort();

        let occupied = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind occupied port");
        let controller =
            SystemManagedProcessController::new(root.0.join("logs")).expect("process controller");
        let owner = controller
            .port_owner(
                occupied.local_addr().unwrap().port(),
                &RuntimeOperationContext::default(),
            )
            .await
            .expect("probe occupied port")
            .expect("external owner");
        assert_eq!(owner.ownership, ResidencyOwnership::PreExisting);
    }

    #[test]
    fn hardware_and_keychain_hmac_primitives_fail_closed() {
        let snapshot = SystemM3HardwareProbe.snapshot().expect("hardware snapshot");
        assert!(snapshot.total_ram_bytes > 0);
        assert!(snapshot.available_ram_bytes <= snapshot.total_ram_bytes);
        assert!(snapshot.logical_cpu_count > 0);
        assert!(snapshot.platform.supports_accelerator(AcceleratorKind::Cpu));

        let protector =
            KeychainLanStateProtector::from_key(vec![7_u8; 32]).expect("fixed test HMAC key");
        let tag = protector
            .authenticate(b"state")
            .expect("authenticate state");
        protector.verify(b"state", &tag).expect("verify state");
        assert!(protector.verify(b"tampered", &tag).is_err());
        assert!(KeychainLanStateProtector::from_key(vec![1_u8; 31]).is_err());

        let driver = build_ollama_driver(snapshot.platform).expect("Ollama driver construction");
        assert_eq!(driver.descriptor().runtime_id, OLLAMA_RUNTIME_ID);
        assert_eq!(driver.descriptor().kind, M3RuntimeKind::Ollama);
    }

    #[test]
    fn nvidia_inventory_parser_reports_aggregate_vram_without_guessing() {
        let cuda = parse_nvidia_smi("NVIDIA RTX 4090, 24564, 20100\nNVIDIA RTX 4060, 8188, 4096\n")
            .expect("valid nvidia-smi inventory");
        assert_eq!(cuda.kind, AcceleratorKind::Cuda);
        assert_eq!(cuda.device_names, ["NVIDIA RTX 4090", "NVIDIA RTX 4060"]);
        assert_eq!(
            cuda.total_memory_bytes,
            Some((24_564 + 8_188) * 1024 * 1024)
        );
        assert_eq!(
            cuda.available_memory_bytes,
            Some((20_100 + 4_096) * 1024 * 1024)
        );
        assert!(parse_nvidia_smi("GPU, N/A, N/A").is_none());
    }
}
