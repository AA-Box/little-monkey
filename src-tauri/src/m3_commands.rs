//! Thin Tauri command surface for the M3 runtime hub.
//!
//! The app root must manage an `M3CommandState` constructed with the real
//! runtime, inference, hardware, keychain, and network dependencies. Streaming
//! HTTP/SSE uses `M3RuntimeHub::dispatch_api_stream` directly from the server;
//! the IPC command intentionally handles only non-streaming requests.

use crate::agent_launcher::{
    self, AgentConfigDriftReport, AgentTool, DriftCheckInput, GenerateAgentConfigRequest,
    GeneratedAgentConfig,
};
use crate::chat_template_lab::{run_chat_template_lab, ChatTemplateLabReport, TemplateFamily};
use crate::compatibility_hub::{
    LanServerPolicy, PairedToken, PairingChallengeView, PairingRequest, ScopedTokenView,
    SecurityAuditEvent,
};
use crate::context_cache::{
    self, ContextCacheView, ContextFailureClassification, ContextFailureInput, ContextRuntimeKind,
    EffectiveContextInput, EffectiveContextResolution,
};
use crate::m3_production::M3CatalogSourceConfig;
use crate::m3_runtime_hub::{
    M3ActivateComponentVersionRequest, M3ActivateModelVersionRequest, M3ApiCaller,
    M3ApiDispatchRequest, M3ApiDispatchResponse, M3CancelInferenceRequest, M3CatalogMatch,
    M3CleanupReport, M3CompatibilityMatrixReport, M3ComponentCatalogEntry, M3ComponentHub,
    M3ComponentKind, M3ComponentUpdateCheck, M3DeleteModelRequest, M3DownloadRequest,
    M3HardwareCompatibilityReport,
    M3HubError, M3InstallComponentRequest, M3InstalledComponentView, M3InstalledModelView,
    M3LoadModelRequest, M3OperationContext, M3PruneModelVersionsRequest, M3RuntimeCapabilityView,
    M3RuntimeHub, M3RuntimeKind, M3RuntimeMetricsView, M3RuntimeStatusView,
    M3SetRuntimeConfigRequest, M3SettingCapabilitiesView, M3StorageStatus, M3UnloadModelRequest,
    M3VerifyProjectorRequest,
};
use crate::quantization::{
    BackendDescriptor, ConversionReport, ConversionRequest, DeclaredLicense, GgufQuantType,
    QuantizationWorkbench,
};
use crate::runtime_adapter::{
    HardwareProfile, HardwareSnapshot, LocalOffloadPlanner, LocalRuntimeScheduler, OffloadPlan,
    OffloadPlanInput, ReqwestHttpTransport, RuntimeInventory, RuntimeLifecycleState,
    RuntimeLogTail, SchedulingInput, SchedulingPlan,
};
use crate::runtime_telemetry::{
    RecordLoadTraceRequest, RecordRequestTraceRequest, RuntimeTelemetryState, RuntimeTraceRecord,
    SupportBundle,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

/// Bounded per-runtime max for the runtime log tails a support bundle can
/// embed — mirrors `M3RuntimeHub::runtime_logs`'s own cap on a single
/// request, applied per runtime so a bundle across many runtimes stays a
/// predictable size.
const SUPPORT_BUNDLE_MAX_LOG_BYTES_PER_RUNTIME: usize = 64 * 1024;
/// How many recent traces a support bundle embeds.
const SUPPORT_BUNDLE_MAX_TRACES: usize = 200;

fn trusted_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

/// WebView-safe envelope for the non-streaming Runtime Hub diagnostic.
///
/// This is intentionally distinct from [`M3ApiDispatchRequest`], the trusted
/// hub/HTTP envelope. IPC callers may provide request data, but they cannot
/// assert an authenticated principal, authorization receipt, or clock value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3DiagnosticDispatchRequest {
    pub protocol: crate::compatibility_hub::CompatibilityProtocol,
    pub runtime_id: String,
    pub request_id: String,
    pub body: Vec<u8>,
}

impl M3DiagnosticDispatchRequest {
    fn into_hub_request(self, now_ms: u64) -> M3ApiDispatchRequest {
        M3ApiDispatchRequest {
            protocol: self.protocol,
            runtime_id: self.runtime_id,
            request_id: self.request_id,
            body: self.body,
            caller: M3ApiCaller::Internal,
            now_ms,
        }
    }
}

/// WebView-safe cancellation envelope paired with
/// [`M3DiagnosticDispatchRequest`]. The command supplies the trusted internal
/// caller and current time after deserialization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3DiagnosticCancelRequest {
    pub protocol: crate::compatibility_hub::CompatibilityProtocol,
    pub runtime_id: String,
    pub request_id: String,
    pub model_id: String,
}

impl M3DiagnosticCancelRequest {
    fn into_hub_request(self, now_ms: u64) -> M3CancelInferenceRequest {
        M3CancelInferenceRequest {
            protocol: self.protocol,
            runtime_id: self.runtime_id,
            request_id: self.request_id,
            model_id: self.model_id,
            caller: M3ApiCaller::Internal,
            now_ms,
        }
    }
}

pub trait M3OwnedProcessShutdown: Send + Sync {
    fn shutdown_all_blocking(&self, timeout: Duration) -> Result<usize, String>;
}

pub struct M3CommandState {
    pub hub: Arc<M3RuntimeHub>,
    /// Runtime component (llama.cpp/MLX/tokenizer/converter/projector/
    /// accelerator-support) version manager. Kept as a separate hub from
    /// `hub` — see `m3_runtime_hub`'s "Runtime Component Update Channels"
    /// module section for why.
    pub component_hub: Arc<M3ComponentHub>,
    /// Runtime Telemetry and Memory Trace Viewer state: bounded trace ring
    /// buffer plus the redaction pass used for both individual traces and
    /// support-bundle export. Deliberately its own `Arc` (not folded into
    /// `hub`) so it stays trivial to construct in tests that don't need the
    /// rest of the M3 hub.
    pub telemetry: Arc<RuntimeTelemetryState>,
    operations: Mutex<BTreeMap<String, CancellationToken>>,
    catalog_mutation: Mutex<()>,
    owned_processes: Option<Arc<dyn M3OwnedProcessShutdown>>,
}

impl M3CommandState {
    pub fn new(hub: Arc<M3RuntimeHub>, component_hub: Arc<M3ComponentHub>) -> Self {
        Self {
            hub,
            component_hub,
            telemetry: Arc::new(RuntimeTelemetryState::new()),
            operations: Mutex::new(BTreeMap::new()),
            catalog_mutation: Mutex::new(()),
            owned_processes: None,
        }
    }

    pub fn with_owned_processes(
        hub: Arc<M3RuntimeHub>,
        component_hub: Arc<M3ComponentHub>,
        owned_processes: Arc<dyn M3OwnedProcessShutdown>,
    ) -> Self {
        Self {
            hub,
            component_hub,
            telemetry: Arc::new(RuntimeTelemetryState::new()),
            operations: Mutex::new(BTreeMap::new()),
            catalog_mutation: Mutex::new(()),
            owned_processes: Some(owned_processes),
        }
    }

    /// Cancels every in-flight command and synchronously terminates all
    /// runtime processes owned by this application. Tauri's event loop exits
    /// via `std::process::exit`, so this must be called from `RunEvent::Exit`.
    pub fn cancel_all_and_shutdown_owned(&self, timeout: Duration) -> Result<usize, String> {
        {
            let mut operations = command_lock(&self.operations)?;
            for cancellation in operations.values() {
                cancellation.cancel();
            }
            operations.clear();
        }
        match &self.owned_processes {
            Some(processes) => processes.shutdown_all_blocking(timeout),
            None => Ok(0),
        }
    }

    fn begin_operation(
        &self,
        operation_id: &str,
        timeout_ms: Option<u64>,
    ) -> Result<M3OperationContext, String> {
        validate_operation_id(operation_id)?;
        let cancellation = CancellationToken::new();
        let mut operations = command_lock(&self.operations)?;
        if operations.contains_key(operation_id) {
            return Err("An operation with that id is already running".to_string());
        }
        operations.insert(operation_id.to_string(), cancellation.clone());
        Ok(M3OperationContext {
            cancellation,
            timeout_ms: timeout_ms.unwrap_or(self.hub.config().operation_timeout_ms),
        })
    }

    fn finish_operation(&self, operation_id: &str) {
        if let Ok(mut operations) = self.operations.lock() {
            operations.remove(operation_id);
        }
    }

    fn cancel_operation(&self, operation_id: &str) -> Result<bool, String> {
        validate_operation_id(operation_id)?;
        let operations = command_lock(&self.operations)?;
        if let Some(cancellation) = operations.get(operation_id) {
            cancellation.cancel();
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

fn command_error(error: M3HubError) -> String {
    error.to_string()
}

fn command_lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, String> {
    mutex
        .lock()
        .map_err(|_| "M3 command operation lock is poisoned".to_string())
}

fn validate_operation_id(operation_id: &str) -> Result<(), String> {
    if operation_id.is_empty()
        || operation_id.len() > 256
        || operation_id.chars().any(char::is_control)
    {
        Err("operationId must contain 1..=256 bytes without controls".to_string())
    } else {
        Ok(())
    }
}

async fn finish<T>(
    state: &M3CommandState,
    operation_id: &str,
    result: Result<T, M3HubError>,
) -> Result<T, String> {
    state.finish_operation(operation_id);
    result.map_err(command_error)
}

#[tauri::command]
pub fn m3_hardware_snapshot(
    state: tauri::State<'_, M3CommandState>,
) -> Result<HardwareSnapshot, String> {
    state.hub.hardware_snapshot().map_err(command_error)
}

#[tauri::command]
pub fn m3_hardware_profile(
    state: tauri::State<'_, M3CommandState>,
) -> Result<HardwareProfile, String> {
    state.hub.hardware_profile().map_err(command_error)
}

/// Hardware Compatibility Matrix / "Driver Doctor" report. The frontend
/// fetches this before starting a model download, model load, or runtime
/// install so the user sees a concrete compatibility report first.
#[tauri::command]
pub fn m3_hardware_compatibility_report(
    state: tauri::State<'_, M3CommandState>,
) -> Result<M3HardwareCompatibilityReport, String> {
    state
        .hub
        .hardware_compatibility_report()
        .map_err(command_error)
}

#[tauri::command]
pub fn m3_storage_status(
    state: tauri::State<'_, M3CommandState>,
) -> Result<M3StorageStatus, String> {
    state.hub.storage_status().map_err(command_error)
}

#[tauri::command]
pub fn m3_installed_models(
    state: tauri::State<'_, M3CommandState>,
) -> Result<Vec<M3InstalledModelView>, String> {
    state.hub.list_installed_models().map_err(command_error)
}

#[tauri::command]
pub fn m3_catalog_sources(
    state: tauri::State<'_, M3CommandState>,
) -> Result<Vec<M3CatalogSourceConfig>, String> {
    crate::m3_production::catalog_source_configs(state.hub.root()).map_err(command_error)
}

#[tauri::command]
pub fn m3_catalog_replace_sources(
    state: tauri::State<'_, M3CommandState>,
    sources: Vec<M3CatalogSourceConfig>,
) -> Result<Vec<M3CatalogSourceConfig>, String> {
    let _guard = command_lock(&state.catalog_mutation)?;
    crate::m3_production::replace_catalog_source_configs(&state.hub, sources).map_err(command_error)
}

#[tauri::command]
pub fn m3_runtimes(
    state: tauri::State<'_, M3CommandState>,
) -> Result<Vec<M3RuntimeCapabilityView>, String> {
    state.hub.list_runtimes().map_err(command_error)
}

#[tauri::command]
pub async fn m3_refresh_runtimes(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
) -> Result<Vec<M3RuntimeCapabilityView>, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state.hub.refresh_runtimes(&context).await;
    finish(&state, &operation_id, result).await
}

#[tauri::command]
pub fn m3_schedule_plan(input: SchedulingInput) -> Result<SchedulingPlan, String> {
    LocalRuntimeScheduler::plan(&input).map_err(|error| error.to_string())
}

/// Sampler/Batching/Speculative Decoding Controls (ROADMAP Phase 8 item 17):
/// narrows `runtime_id`'s declared advanced settings down to what the
/// current hardware and (if `asset_id` is given) selected model can
/// actually honor — see `m3_runtime_hub.rs`'s `gate_advanced_settings` for
/// the gating rules. `asset_id: None` still resolves the hardware-only
/// gates (flash attention, mixed precision); only the speculative-decoding
/// draft-model gate needs a target model.
#[tauri::command]
pub fn m3_resolve_setting_capabilities(
    state: tauri::State<'_, M3CommandState>,
    runtime_id: String,
    asset_id: Option<String>,
) -> Result<M3SettingCapabilitiesView, String> {
    state
        .hub
        .resolve_setting_capabilities(&runtime_id, asset_id.as_deref())
        .map_err(command_error)
}

/// Chat Template and Renderer Compatibility Lab report for one coarse
/// template family (derived from `M3CatalogModel`/`M3InstalledVersion`'s
/// `template` field). Pure and deterministic — no hub state needed, same as
/// `m3_schedule_plan` above — so the frontend can call this directly for
/// any model's declared `template` string, including `null`/unrecognized
/// ones (which fall back to the `Generic` family).
#[tauri::command]
pub fn m3_chat_template_lab_report(
    template: Option<String>,
) -> Result<ChatTemplateLabReport, String> {
    Ok(run_chat_template_lab(TemplateFamily::detect(
        template.as_deref(),
    )))
}

/// Simulates fit and computes a per-load offload plan (context size, batch
/// size, GPU layers, projector placement, CPU spill, and parallelism) before
/// a model is actually loaded. Pure and read-only like `m3_schedule_plan`:
/// the frontend supplies a live hardware snapshot and the selected model's
/// profile, and this never touches a runtime process.
#[tauri::command]
pub fn m3_offload_plan(input: OffloadPlanInput) -> Result<OffloadPlan, String> {
    LocalOffloadPlanner::plan(&input).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn m3_catalog_search(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    query: String,
    limit: usize,
) -> Result<Vec<M3CatalogMatch>, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state.hub.search_catalog(&query, limit, &context).await;
    finish(&state, &operation_id, result).await
}

/// Model Retirement and Compatibility Warnings (ROADMAP.md Phase 8, item
/// 14): the frontend calls this before a local model actually loads (see
/// `RuntimeHubRuntimes.tsx`'s "Load model" section) so an outdated installed
/// model shows a concrete "update to this" migration path first.
#[tauri::command]
pub async fn m3_model_staleness_check(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    asset_id: String,
) -> Result<Option<crate::model_retirement::LocalModelStalenessWarning>, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state.hub.model_staleness_check(&asset_id, &context).await;
    finish(&state, &operation_id, result).await
}

#[tauri::command]
pub async fn m3_model_download(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    request: M3DownloadRequest,
) -> Result<M3InstalledModelView, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state.hub.download_model(&request, &context).await;
    finish(&state, &operation_id, result).await
}

#[tauri::command]
pub async fn m3_model_update(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    asset_id: String,
    request: M3DownloadRequest,
) -> Result<M3InstalledModelView, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state.hub.update_model(&asset_id, &request, &context).await;
    finish(&state, &operation_id, result).await
}

#[tauri::command]
pub async fn m3_model_activate_version(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    request: M3ActivateModelVersionRequest,
) -> Result<M3InstalledModelView, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state.hub.activate_model_version(&request, &context).await;
    finish(&state, &operation_id, result).await
}

/// Verifies a candidate local file against an installed model version's
/// declared projector reference, promoting its evidence from "declared" to
/// genuinely `Verified` (ROADMAP Phase 8 item 12). See
/// `M3RuntimeHub::verify_projector` for why this checks a user-supplied
/// path rather than downloading anything itself.
#[tauri::command]
pub async fn m3_verify_projector(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    request: M3VerifyProjectorRequest,
) -> Result<M3InstalledModelView, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state.hub.verify_projector(&request, &context).await;
    finish(&state, &operation_id, result).await
}

#[tauri::command]
pub async fn m3_model_prune_versions(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    request: M3PruneModelVersionsRequest,
) -> Result<M3InstalledModelView, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state.hub.prune_model_versions(&request, &context).await;
    finish(&state, &operation_id, result).await
}

#[tauri::command]
pub async fn m3_cleanup_orphans(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    confirmation: String,
) -> Result<M3CleanupReport, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state.hub.cleanup_orphans(&confirmation, &context).await;
    finish(&state, &operation_id, result).await
}

#[tauri::command]
pub async fn m3_model_delete(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    request: M3DeleteModelRequest,
) -> Result<bool, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state.hub.delete_model(&request, &context).await;
    finish(&state, &operation_id, result).await
}

#[tauri::command]
pub fn m3_cancel_operation(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
) -> Result<bool, String> {
    state.cancel_operation(&operation_id)
}

#[tauri::command]
pub async fn m3_runtime_status(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    runtime_id: String,
) -> Result<M3RuntimeStatusView, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state.hub.runtime_status(&runtime_id, &context).await;
    finish(&state, &operation_id, result).await
}

#[tauri::command]
pub async fn m3_runtime_inventory(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    runtime_id: String,
) -> Result<RuntimeInventory, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state.hub.runtime_inventory(&runtime_id, &context).await;
    finish(&state, &operation_id, result).await
}

#[tauri::command]
pub async fn m3_runtime_load_model(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    request: M3LoadModelRequest,
) -> Result<(), String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state.hub.load_model(&request, &context).await;
    finish(&state, &operation_id, result).await
}

#[tauri::command]
pub async fn m3_runtime_unload_model(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    request: M3UnloadModelRequest,
) -> Result<(), String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state.hub.unload_model(&request, &context).await;
    finish(&state, &operation_id, result).await
}

#[tauri::command]
pub async fn m3_runtime_logs(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    runtime_id: String,
    max_bytes: usize,
) -> Result<RuntimeLogTail, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state
        .hub
        .runtime_logs(&runtime_id, max_bytes, &context)
        .await;
    finish(&state, &operation_id, result).await
}

#[tauri::command]
pub async fn m3_runtime_metrics(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    runtime_id: String,
) -> Result<M3RuntimeMetricsView, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state.hub.runtime_metrics(&runtime_id, &context).await;
    finish(&state, &operation_id, result).await
}

fn context_runtime_kind(kind: M3RuntimeKind) -> ContextRuntimeKind {
    match kind {
        M3RuntimeKind::Ollama => ContextRuntimeKind::Ollama,
        M3RuntimeKind::LlamaCpp => ContextRuntimeKind::LlamaCpp,
        M3RuntimeKind::Mlx => ContextRuntimeKind::Mlx,
    }
}

fn context_cache_sampled_at_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Context window / KV-cache state for one runtime: the context size this
/// app has configured (or will configure) for its next load, plus — for a
/// managed, currently running llama.cpp process — whatever live state its
/// `/props` and `/slots` endpoints actually report. Ollama and MLX honestly
/// report only what this app itself knows (the requested setting), since
/// neither backend's API surfaces live KV-cache/context occupancy today.
async fn context_cache_state_impl(
    state: &M3CommandState,
    runtime_id: &str,
    context: &M3OperationContext,
) -> Result<ContextCacheView, M3HubError> {
    let capability = state
        .hub
        .list_runtimes()?
        .into_iter()
        .find(|entry| entry.descriptor.runtime_id == runtime_id)
        .ok_or_else(|| M3HubError::NotFound(format!("unknown runtime {runtime_id}")))?;
    let kind = context_runtime_kind(capability.descriptor.kind);
    let persisted = state.hub.runtime_config(runtime_id)?;
    let configured = context_cache::resolve_configured_context(
        &capability.settings,
        persisted.as_ref(),
        context_cache::configured_context_key_candidates(kind),
    );

    let mut live = None;
    if kind == ContextRuntimeKind::LlamaCpp {
        if let Ok(M3RuntimeStatusView::Adapter { status, .. }) =
            state.hub.runtime_status(runtime_id, context).await
        {
            let reachable = matches!(
                status.state,
                RuntimeLifecycleState::Ready | RuntimeLifecycleState::Starting
            );
            if reachable {
                if let Ok(transport) = ReqwestHttpTransport::new() {
                    let cancellation = CancellationToken::new();
                    live = Some(
                        context_cache::fetch_llama_cpp_live_context_state(
                            &status.runtime.endpoint,
                            &transport,
                            &cancellation,
                        )
                        .await,
                    );
                }
            }
        }
    }

    let reported_context_tokens = live.as_ref().and_then(|live| live.reported_context_tokens);
    let context_tokens_in_use = live.as_ref().and_then(|live| {
        live.slots
            .iter()
            .filter_map(|slot| slot.tokens_in_use)
            .max()
    });
    let context_shift_detected = live.as_ref().and_then(|live| {
        if live.slots.is_empty() {
            None
        } else if live
            .slots
            .iter()
            .any(|slot| slot.context_shifted == Some(true))
        {
            Some(true)
        } else if live
            .slots
            .iter()
            .all(|slot| slot.context_shifted == Some(false))
        {
            Some(false)
        } else {
            None
        }
    });
    let total_slots = live.as_ref().and_then(|live| live.total_slots);
    let notes = context_cache::context_cache_notes(kind, live.as_ref());
    let effective_context_tokens = reported_context_tokens.or(configured.tokens);

    Ok(ContextCacheView {
        runtime_id: runtime_id.to_string(),
        runtime_kind: kind,
        configured,
        reported_context_tokens,
        context_tokens_in_use,
        context_headroom_tokens: context_cache::context_headroom(
            effective_context_tokens,
            context_tokens_in_use,
        ),
        context_shift_detected,
        total_slots,
        notes,
        sampled_at_ms: context_cache_sampled_at_ms(),
    })
}

#[tauri::command]
pub async fn m3_context_cache_state(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    runtime_id: String,
) -> Result<ContextCacheView, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = context_cache_state_impl(&state, &runtime_id, &context).await;
    finish(&state, &operation_id, result).await
}

/// Resolves a safe, user-visible effective context size for a load: the
/// frontend supplies its requested size plus the already-computed offload
/// plan's memory-aware bound (see `m3_offload_plan`) and any known model
/// metadata/runtime setting bounds; this only tightens those bounds further
/// and explains every reduction, never bypasses them.
#[tauri::command]
pub fn m3_context_effective_size(
    input: EffectiveContextInput,
) -> Result<EffectiveContextResolution, String> {
    Ok(context_cache::resolve_effective_context(&input))
}

/// Classifies a plausibly context/cache-related generation failure or
/// degradation into one of five categories (prompt too long, cache
/// exhausted/context shift, memory pressure, runtime limitation, model
/// metadata limit) with a plain-language explanation, or returns `null` when
/// the supplied evidence gives no reason to believe context/cache/memory was
/// the cause.
#[tauri::command]
pub fn m3_classify_context_failure(
    input: ContextFailureInput,
) -> Result<Option<ContextFailureClassification>, String> {
    Ok(context_cache::classify_context_failure(&input))
}

#[tauri::command]
pub fn m3_runtime_set_config(
    state: tauri::State<'_, M3CommandState>,
    request: M3SetRuntimeConfigRequest,
) -> Result<BTreeMap<String, crate::runtime_adapter::SettingValue>, String> {
    state
        .hub
        .set_runtime_config(&request)
        .map_err(command_error)
}

#[tauri::command]
pub fn m3_runtime_config(
    state: tauri::State<'_, M3CommandState>,
    runtime_id: String,
) -> Result<Option<BTreeMap<String, crate::runtime_adapter::SettingValue>>, String> {
    state.hub.runtime_config(&runtime_id).map_err(command_error)
}

#[tauri::command]
pub async fn m3_api_dispatch(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    request: M3DiagnosticDispatchRequest,
) -> Result<M3ApiDispatchResponse, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let request = request.into_hub_request(trusted_now_ms());
    let result = state.hub.dispatch_api(&request, &context).await;
    finish(&state, &operation_id, result).await
}

#[tauri::command]
pub async fn m3_api_cancel_inference(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    request: M3DiagnosticCancelRequest,
) -> Result<bool, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let request = request.into_hub_request(trusted_now_ms());
    let result = state.hub.cancel_inference(&request, &context).await;
    finish(&state, &operation_id, result).await
}

/// Phase 8 item 11 acceptance criterion: "shown in Runtime/API Hub". Builds
/// the per-route × backend × model compatibility matrix from real,
/// currently-registered runtime/model capability state — see
/// `M3RuntimeHub::compatibility_matrix`'s doc for why this is
/// capability-derived rather than a live per-cell network probe.
#[tauri::command]
pub fn m3_compatibility_matrix(
    state: tauri::State<'_, M3CommandState>,
) -> Result<M3CompatibilityMatrixReport, String> {
    state.hub.compatibility_matrix().map_err(command_error)
}

#[tauri::command]
pub fn m3_lan_validate_policy(policy: LanServerPolicy) -> Result<(), String> {
    M3RuntimeHub::validate_lan_policy(&policy).map_err(command_error)
}

#[tauri::command]
pub async fn m3_lan_configure(
    app: tauri::AppHandle,
    policy: LanServerPolicy,
) -> Result<LanServerPolicy, String> {
    crate::server::configure_m3_policy_and_reconcile(&app, policy).await
}

#[tauri::command]
pub async fn m3_lan_disable(app: tauri::AppHandle, confirmation: String) -> Result<bool, String> {
    crate::server::disable_m3_policy_and_reconcile(&app, &confirmation).await
}

#[tauri::command]
pub fn m3_lan_policy(
    state: tauri::State<'_, M3CommandState>,
) -> Result<Option<LanServerPolicy>, String> {
    state.hub.lan_policy().map_err(command_error)
}

#[tauri::command]
pub fn m3_lan_begin_pairing(
    state: tauri::State<'_, M3CommandState>,
    request: PairingRequest,
    now_ms: u64,
    remote_address: String,
) -> Result<PairingChallengeView, String> {
    state
        .hub
        .begin_pairing(request, now_ms, &remote_address)
        .map_err(command_error)
}

#[tauri::command]
pub fn m3_lan_complete_pairing(
    state: tauri::State<'_, M3CommandState>,
    challenge_id: String,
    pairing_code: String,
    now_ms: u64,
    remote_address: String,
) -> Result<PairedToken, String> {
    state
        .hub
        .complete_pairing(&challenge_id, &pairing_code, now_ms, &remote_address)
        .map_err(command_error)
}

#[tauri::command]
pub fn m3_lan_revoke_token(
    state: tauri::State<'_, M3CommandState>,
    token_id: String,
    now_ms: u64,
    remote_address: String,
) -> Result<ScopedTokenView, String> {
    state
        .hub
        .revoke_token(&token_id, now_ms, &remote_address)
        .map_err(command_error)
}

#[tauri::command]
pub fn m3_lan_tokens(
    state: tauri::State<'_, M3CommandState>,
) -> Result<Vec<ScopedTokenView>, String> {
    state.hub.list_tokens().map_err(command_error)
}

#[tauri::command]
pub fn m3_lan_audit_events(
    state: tauri::State<'_, M3CommandState>,
) -> Result<Vec<SecurityAuditEvent>, String> {
    state.hub.security_audit_events().map_err(command_error)
}

// =========================================================================
// Model Conversion and Quantization Workbench (ROADMAP.md Phase 8)
// =========================================================================

/// Owns the [`QuantizationWorkbench`] (its own storage root, independent of
/// the model manifest/blob store owned by `M3CommandState`). The workbench
/// itself holds no Tauri state; this is only the thin managed-state wrapper
/// so commands can cheaply clone the `Arc` before moving it into
/// `spawn_blocking` (conversion shells out to an external process and hashes
/// files, both of which are blocking work).
pub struct M3QuantizationCommandState {
    pub workbench: Arc<QuantizationWorkbench>,
}

impl M3QuantizationCommandState {
    pub fn new(workbench: QuantizationWorkbench) -> Self {
        Self {
            workbench: Arc::new(workbench),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuantTypeDescriptor {
    pub id: String,
    pub cli_name: String,
    pub note: String,
}

#[tauri::command]
pub fn quantization_backends(
    state: tauri::State<'_, M3QuantizationCommandState>,
) -> Result<Vec<BackendDescriptor>, String> {
    Ok(state.workbench.list_backends())
}

#[tauri::command]
pub fn quantization_quant_types(
    state: tauri::State<'_, M3QuantizationCommandState>,
) -> Result<Vec<QuantTypeDescriptor>, String> {
    Ok(state
        .workbench
        .quant_types()
        .into_iter()
        .map(|(_quant, cli_name, note)| QuantTypeDescriptor {
            id: cli_name.to_string(),
            cli_name: cli_name.to_string(),
            note: note.to_string(),
        })
        .collect())
}

fn parse_quant_choice(quant_choice: &str) -> Result<GgufQuantType, String> {
    GgufQuantType::parse(quant_choice)
        .ok_or_else(|| format!("unknown quantization type '{quant_choice}'"))
}

/// Converts/(re)quantizes an arbitrary GGUF or safetensors path on disk (not
/// necessarily a Runtime Hub-installed model). License information is
/// best-effort sniffed directly out of the source file/directory — see
/// [`quantization_convert_installed_model`] to reuse an installed model's
/// verified catalog license instead.
#[tauri::command]
pub async fn quantization_convert_path(
    state: tauri::State<'_, M3QuantizationCommandState>,
    source_path: String,
    quant_choice: String,
    allow_requantize: bool,
) -> Result<ConversionReport, String> {
    let quant_choice = parse_quant_choice(&quant_choice)?;
    let workbench = state.workbench.clone();
    tauri::async_runtime::spawn_blocking(move || {
        workbench.convert(ConversionRequest {
            source_path: PathBuf::from(source_path),
            quant_choice,
            allow_requantize,
            license: DeclaredLicense::SniffFromSource,
        })
    })
    .await
    .map_err(|error| format!("quantization task join failed: {error}"))?
    .map_err(|error| error.to_string())
}

/// Converts/(re)quantizes an already-installed Runtime Hub model version,
/// reusing its verified catalog license (`M3ModelLicense`) instead of
/// sniffing one out of the file. Defaults to the asset's active version
/// when `version_key` is omitted.
#[tauri::command]
pub async fn quantization_convert_installed_model(
    state: tauri::State<'_, M3QuantizationCommandState>,
    m3_state: tauri::State<'_, M3CommandState>,
    asset_id: String,
    version_key: Option<String>,
    quant_choice: String,
    allow_requantize: bool,
) -> Result<ConversionReport, String> {
    let quant_choice = parse_quant_choice(&quant_choice)?;
    let model = m3_state
        .hub
        .list_installed_models()
        .map_err(command_error)?
        .into_iter()
        .find(|model| model.asset_id == asset_id)
        .ok_or_else(|| format!("no installed model with asset id '{asset_id}'"))?;
    let target_version_key = version_key.unwrap_or_else(|| model.active_version_key.clone());
    let version = model
        .versions
        .into_iter()
        .find(|version| version.version_key == target_version_key)
        .ok_or_else(|| {
            format!("no installed version '{target_version_key}' for asset '{asset_id}'")
        })?;

    let workbench = state.workbench.clone();
    tauri::async_runtime::spawn_blocking(move || {
        workbench.convert(ConversionRequest {
            source_path: version.artifact_path,
            quant_choice,
            allow_requantize,
            license: DeclaredLicense::FromInstalledModel(version.license),
        })
    })
    .await
    .map_err(|error| format!("quantization task join failed: {error}"))?
    .map_err(|error| error.to_string())
}

// -------------------------------------------------------------------------
// Runtime Component Update Channels
// -------------------------------------------------------------------------

#[tauri::command]
pub fn m3_component_storage_status(
    state: tauri::State<'_, M3CommandState>,
) -> Result<M3StorageStatus, String> {
    state.component_hub.storage_status().map_err(command_error)
}

#[tauri::command]
pub fn m3_component_installed(
    state: tauri::State<'_, M3CommandState>,
) -> Result<Vec<M3InstalledComponentView>, String> {
    state.component_hub.list_installed().map_err(command_error)
}

/// Installs a built, signed MLX service package and activates it.
///
/// `package_directory` is whatever the user picked — the output of
/// `pnpm mlx:package`. Trust comes from the pinned Ed25519 release key over
/// the manifest and every file digest, not from the path, so this deliberately
/// does not restrict where the directory may live.
///
/// The runtime list is not refreshed here: the MLX driver reads
/// `verify_active()` when it is next asked for status, so the caller's own
/// refresh is what surfaces the newly installed package.
#[tauri::command]
pub fn m3_mlx_install(
    app: tauri::AppHandle,
    package_directory: String,
) -> Result<crate::m3_production::MlxInstalledPackageView, String> {
    use tauri::Manager as _;
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Failed to resolve app data directory: {error}"))?;
    crate::m3_production::install_mlx_package(
        &app_data_dir,
        std::path::Path::new(&package_directory),
    )
    .map_err(command_error)
}

/// Activates an MLX package that the component hub has already downloaded.
///
/// The two halves are deliberately separate commands rather than one: the
/// component hub owns fetching, resuming, digest-checking, and version
/// history, and the MLX installer owns signature verification and the active
/// symlink. This joins them for the one component kind that needs unpacking,
/// and re-running it on an already-installed version is a no-op that
/// re-verifies rather than an error.
#[tauri::command]
pub fn m3_mlx_install_component(
    app: tauri::AppHandle,
    state: tauri::State<'_, M3CommandState>,
    component_id: String,
) -> Result<crate::m3_production::MlxInstalledPackageView, String> {
    use tauri::Manager as _;
    let installed = state
        .component_hub
        .list_installed()
        .map_err(command_error)?
        .into_iter()
        .find(|component| component.component_id == component_id)
        .ok_or_else(|| format!("No component named '{component_id}' is installed"))?;
    if installed.kind != M3ComponentKind::MlxRuntime {
        return Err(format!(
            "'{component_id}' is a {:?} component, not an MLX runtime",
            installed.kind
        ));
    }
    let artifact = installed
        .versions
        .iter()
        .find(|version| version.version_key == installed.active_version_key)
        .ok_or("The installed MLX component has no active version")?
        .artifact_path
        .clone();
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Failed to resolve app data directory: {error}"))?;
    crate::m3_production::install_mlx_from_artifact(&app_data_dir, &artifact).map_err(command_error)
}

#[tauri::command]
pub fn m3_component_registry_entries(
    state: tauri::State<'_, M3CommandState>,
) -> Result<Vec<M3ComponentCatalogEntry>, String> {
    crate::m3_production::component_registry_entries(state.component_hub.root())
        .map_err(command_error)
}

#[tauri::command]
pub fn m3_component_replace_registry_entries(
    state: tauri::State<'_, M3CommandState>,
    entries: Vec<M3ComponentCatalogEntry>,
) -> Result<Vec<M3ComponentCatalogEntry>, String> {
    let _guard = command_lock(&state.catalog_mutation)?;
    crate::m3_production::replace_component_registry_entries(&state.component_hub, entries)
        .map_err(command_error)
}

#[tauri::command]
pub async fn m3_component_list_registry(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
) -> Result<Vec<M3ComponentCatalogEntry>, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state.component_hub.list_registry(&context).await;
    finish(&state, &operation_id, result).await
}

#[tauri::command]
pub async fn m3_component_check_updates(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
) -> Result<Vec<M3ComponentUpdateCheck>, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state.component_hub.check_updates(&context).await;
    finish(&state, &operation_id, result).await
}

#[tauri::command]
pub async fn m3_component_install(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    request: M3InstallComponentRequest,
) -> Result<M3InstalledComponentView, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state
        .component_hub
        .install_component(&request, &context)
        .await;
    finish(&state, &operation_id, result).await
}

#[tauri::command]
pub async fn m3_component_activate_version(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    request: M3ActivateComponentVersionRequest,
) -> Result<M3InstalledComponentView, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state
        .component_hub
        .activate_component_version(&request, &context)
        .await;
    finish(&state, &operation_id, result).await
}

// -------------------------------------------------------------------------
// Runtime Telemetry and Memory Trace Viewer
// -------------------------------------------------------------------------

/// Records a per-load trace. The caller (the Runtime Hub's load flow)
/// already measured `startedAtMs`/`readyAtMs` around its own
/// `m3_runtime_load_model` call and already computed an offload plan via
/// `m3_offload_plan` before loading — this only stores what the caller
/// already has, applying redaction to `errorMessage` before it is ever held
/// in memory.
#[tauri::command]
pub fn m3_telemetry_record_load(
    state: tauri::State<'_, M3CommandState>,
    request: RecordLoadTraceRequest,
) -> Result<RuntimeTraceRecord, String> {
    state.telemetry.record_load(request)
}

/// Records a per-request trace (sampler stats actually used plus token
/// counts/timing) — see `RecordRequestTraceRequest`'s fields for exactly why
/// this cannot carry prompt/response text even in principle.
#[tauri::command]
pub fn m3_telemetry_record_request(
    state: tauri::State<'_, M3CommandState>,
    request: RecordRequestTraceRequest,
) -> Result<RuntimeTraceRecord, String> {
    state.telemetry.record_request(request)
}

#[tauri::command]
pub fn m3_telemetry_recent_traces(
    state: tauri::State<'_, M3CommandState>,
    runtime_id: Option<String>,
    limit: usize,
) -> Result<Vec<RuntimeTraceRecord>, String> {
    Ok(state.telemetry.recent(runtime_id.as_deref(), limit))
}

/// Assembles a redacted support bundle: recent traces (already redacted at
/// record time) plus a fresh, bounded, redacted tail of every runtime's
/// managed-process log (via the existing `M3RuntimeHub::runtime_logs`, the
/// same path `m3_runtime_logs` uses) and hardware/compatibility context.
/// Never writes to disk itself — the frontend follows this app's usual
/// `save()`-then-`writeTextFile()` export pattern (see `RunCapsulePanel`)
/// with the exact JSON this command returns, so the user's native "Save As"
/// dialog is the confirmation step, matching every other export in this app.
#[tauri::command]
pub async fn m3_telemetry_support_bundle(
    app: tauri::AppHandle,
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
) -> Result<SupportBundle, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let outcome = assemble_support_bundle(&app, &state, &context).await;
    state.finish_operation(&operation_id);
    outcome
}

async fn assemble_support_bundle(
    app: &tauri::AppHandle,
    state: &M3CommandState,
    context: &M3OperationContext,
) -> Result<SupportBundle, String> {
    let hardware = state.hub.hardware_snapshot().ok();
    let compatibility = state.hub.hardware_compatibility_report().ok();
    let runtimes = state.hub.list_runtimes().unwrap_or_default();

    let mut raw_logs = Vec::new();
    for runtime in runtimes.iter().filter(|runtime| runtime.can_logs) {
        let runtime_id = runtime.descriptor.runtime_id.clone();
        if let Ok(tail) = state
            .hub
            .runtime_logs(
                &runtime_id,
                SUPPORT_BUNDLE_MAX_LOG_BYTES_PER_RUNTIME,
                context,
            )
            .await
        {
            raw_logs.push((runtime_id, tail.text, tail.truncated));
        }
    }

    let traces = state.telemetry.recent(None, SUPPORT_BUNDLE_MAX_TRACES);

    Ok(crate::runtime_telemetry::build_support_bundle(
        state.telemetry.redactor(),
        app.package_info().version.to_string(),
        std::env::consts::OS.to_string(),
        hardware,
        compatibility,
        traces,
        raw_logs,
        trusted_now_ms(),
    ))
}

// -------------------------------------------------------------------------
// Local Agent Integration Launcher
// -------------------------------------------------------------------------

/// The effective context-window size (in tokens) the hub currently serves
/// for every installed model's runtime, keyed by model id. Shared by both
/// commands below so config generation and drift checks reuse the exact
/// same resolution the user would see in the Runtime Hub's own settings.
fn effective_context_by_model(
    hub: &M3RuntimeHub,
    installed: &[M3InstalledModelView],
) -> Result<BTreeMap<String, u64>, String> {
    let runtimes = hub.list_runtimes().map_err(command_error)?;
    let mut context_by_model = BTreeMap::new();
    for model in installed {
        let capability = runtimes
            .iter()
            .find(|runtime| runtime.descriptor.kind == model.runtime);
        let stored = capability
            .map(|capability| hub.runtime_config(&capability.descriptor.runtime_id))
            .transpose()
            .map_err(command_error)?
            .flatten();
        if let Some(tokens) =
            agent_launcher::effective_context_tokens(capability, stored.as_ref(), model.runtime)
        {
            context_by_model.insert(model.model_id.clone(), tokens);
        }
    }
    Ok(context_by_model)
}

/// Generates a real, working external-tool config snippet pointed at this
/// app's actual M3 HTTP server endpoint, a currently-installed model, and
/// (if supplied) a real paired bearer token. Fails with a clear message if
/// the LAN/API listener has never been configured or the model is not
/// installed, rather than silently falling back to placeholder values.
#[tauri::command]
pub fn agent_launcher_generate_config(
    state: tauri::State<'_, M3CommandState>,
    tool: AgentTool,
    model_id: String,
    auth_token: Option<String>,
) -> Result<GeneratedAgentConfig, String> {
    let hub = &state.hub;
    let policy = hub
        .lan_policy()
        .map_err(command_error)?
        .ok_or_else(|| {
            "Configure and start the LAN/API listener (Settings > Runtime Hub > LAN) before generating external tool configuration".to_string()
        })?;
    let installed = hub.list_installed_models().map_err(command_error)?;
    let model = installed
        .iter()
        .find(|installed_model| installed_model.model_id == model_id)
        .ok_or_else(|| format!("Model '{model_id}' is not currently installed"))?;
    let runtimes = hub.list_runtimes().map_err(command_error)?;
    let capability = runtimes
        .iter()
        .find(|runtime| runtime.descriptor.kind == model.runtime);
    let stored_config = capability
        .map(|capability| hub.runtime_config(&capability.descriptor.runtime_id))
        .transpose()
        .map_err(command_error)?
        .flatten();

    let request = GenerateAgentConfigRequest {
        tool,
        endpoint: agent_launcher::resolve_endpoint(&policy),
        model_id: model.model_id.clone(),
        effective_context_tokens: agent_launcher::effective_context_tokens(
            capability,
            stored_config.as_ref(),
            model.runtime,
        ),
        auth_token,
    };
    Ok(agent_launcher::generate_config(&request))
}

/// Checks a previously-generated or user-pasted external-tool config against
/// this app's current real state (installed models, server endpoint,
/// authentication requirement, and effective context sizes) for drift.
#[tauri::command]
pub fn agent_launcher_check_drift(
    state: tauri::State<'_, M3CommandState>,
    tool: AgentTool,
    pasted_config: String,
) -> Result<AgentConfigDriftReport, String> {
    let hub = &state.hub;
    let policy = hub.lan_policy().map_err(command_error)?;
    let installed = hub.list_installed_models().map_err(command_error)?;
    let effective_context_by_model = effective_context_by_model(hub, &installed)?;
    let input = DriftCheckInput {
        tool,
        pasted_config,
        current_endpoint: policy.as_ref().map(agent_launcher::resolve_endpoint),
        installed_model_ids: installed
            .iter()
            .map(|model| model.model_id.clone())
            .collect(),
        effective_context_by_model,
        auth_currently_required: policy
            .map(|policy| policy.require_authentication)
            .unwrap_or(false),
    };
    agent_launcher::detect_drift(&input).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn dispatch_json() -> Value {
        json!({
            "protocol": "open_ai_chat_completions",
            "runtimeId": "ollama",
            "requestId": "diagnostic-1",
            "body": [123, 125]
        })
    }

    fn cancel_json() -> Value {
        json!({
            "protocol": "open_ai_chat_completions",
            "runtimeId": "ollama",
            "requestId": "diagnostic-1",
            "modelId": "qwen"
        })
    }

    #[test]
    fn diagnostic_dispatch_ipc_shape_supplies_trusted_hub_fields() {
        let request: M3DiagnosticDispatchRequest =
            serde_json::from_value(dispatch_json()).expect("valid diagnostic IPC request");
        assert_eq!(serde_json::to_value(&request).unwrap(), dispatch_json());

        let hub_request = request.into_hub_request(42);
        assert_eq!(hub_request.caller, M3ApiCaller::Internal);
        assert_eq!(hub_request.now_ms, 42);
    }

    #[test]
    fn diagnostic_cancel_ipc_shape_supplies_trusted_hub_fields() {
        let request: M3DiagnosticCancelRequest =
            serde_json::from_value(cancel_json()).expect("valid diagnostic IPC cancel request");
        assert_eq!(serde_json::to_value(&request).unwrap(), cancel_json());

        let hub_request = request.into_hub_request(43);
        assert_eq!(hub_request.caller, M3ApiCaller::Internal);
        assert_eq!(hub_request.now_ms, 43);
    }

    #[test]
    fn diagnostic_ipc_shapes_reject_client_asserted_trust_fields() {
        for forbidden in [
            ("caller", json!({ "type": "internal" })),
            ("nowMs", json!(0)),
            ("authorizationReceipt", json!({ "id": "forged" })),
        ] {
            let mut dispatch = dispatch_json();
            dispatch[forbidden.0] = forbidden.1.clone();
            assert!(serde_json::from_value::<M3DiagnosticDispatchRequest>(dispatch).is_err());

            let mut cancel = cancel_json();
            cancel[forbidden.0] = forbidden.1;
            assert!(serde_json::from_value::<M3DiagnosticCancelRequest>(cancel).is_err());
        }
    }
}
