//! Thin Tauri command surface for the M3 runtime hub.
//!
//! The app root must manage an `M3CommandState` constructed with the real
//! runtime, inference, hardware, keychain, and network dependencies. Streaming
//! HTTP/SSE uses `M3RuntimeHub::dispatch_api_stream` directly from the server;
//! the IPC command intentionally handles only non-streaming requests.

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
    M3ActivateComponentVersionRequest, M3ActivateModelVersionRequest, M3ApiDispatchRequest,
    M3ApiDispatchResponse, M3CancelInferenceRequest, M3CatalogMatch, M3CleanupReport,
    M3ComponentCatalogEntry, M3ComponentHub, M3ComponentUpdateCheck, M3DeleteModelRequest,
    M3DownloadRequest, M3HardwareCompatibilityReport, M3HubError, M3InstallComponentRequest,
    M3InstalledComponentView, M3InstalledModelView, M3LoadModelRequest, M3OperationContext,
    M3PruneModelVersionsRequest, M3RuntimeCapabilityView, M3RuntimeHub, M3RuntimeKind,
    M3RuntimeMetricsView, M3RuntimeStatusView, M3SetRuntimeConfigRequest,
    M3SettingCapabilitiesView, M3StorageStatus, M3UnloadModelRequest,
};
use crate::runtime_adapter::{
    HardwareProfile, HardwareSnapshot, LocalOffloadPlanner, LocalRuntimeScheduler, OffloadPlan,
    OffloadPlanInput, ReqwestHttpTransport, RuntimeInventory, RuntimeLifecycleState, RuntimeLogTail,
    SchedulingInput, SchedulingPlan,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

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
    operations: Mutex<BTreeMap<String, CancellationToken>>,
    catalog_mutation: Mutex<()>,
    owned_processes: Option<Arc<dyn M3OwnedProcessShutdown>>,
}

impl M3CommandState {
    pub fn new(hub: Arc<M3RuntimeHub>, component_hub: Arc<M3ComponentHub>) -> Self {
        Self {
            hub,
            component_hub,
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
pub fn m3_chat_template_lab_report(template: Option<String>) -> Result<ChatTemplateLabReport, String> {
    Ok(run_chat_template_lab(TemplateFamily::detect(template.as_deref())))
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
            let reachable = matches!(status.state, RuntimeLifecycleState::Ready | RuntimeLifecycleState::Starting);
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
    let context_tokens_in_use = live
        .as_ref()
        .and_then(|live| live.slots.iter().filter_map(|slot| slot.tokens_in_use).max());
    let context_shift_detected = live.as_ref().and_then(|live| {
        if live.slots.is_empty() {
            None
        } else if live.slots.iter().any(|slot| slot.context_shifted == Some(true)) {
            Some(true)
        } else if live.slots.iter().all(|slot| slot.context_shifted == Some(false)) {
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
        context_headroom_tokens: context_cache::context_headroom(effective_context_tokens, context_tokens_in_use),
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
    request: M3ApiDispatchRequest,
) -> Result<M3ApiDispatchResponse, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state.hub.dispatch_api(&request, &context).await;
    finish(&state, &operation_id, result).await
}

#[tauri::command]
pub async fn m3_api_cancel_inference(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    request: M3CancelInferenceRequest,
) -> Result<bool, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = state.hub.cancel_inference(&request, &context).await;
    finish(&state, &operation_id, result).await
}

#[tauri::command]
pub fn m3_lan_validate_policy(policy: LanServerPolicy) -> Result<(), String> {
    M3RuntimeHub::validate_lan_policy(&policy).map_err(command_error)
}

#[tauri::command]
pub fn m3_lan_configure(
    state: tauri::State<'_, M3CommandState>,
    policy: LanServerPolicy,
) -> Result<LanServerPolicy, String> {
    state.hub.configure_lan(policy).map_err(command_error)
}

#[tauri::command]
pub fn m3_lan_disable(
    state: tauri::State<'_, M3CommandState>,
    confirmation: String,
) -> Result<bool, String> {
    state.hub.disable_lan(&confirmation).map_err(command_error)
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
    let result = state.component_hub.install_component(&request, &context).await;
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
