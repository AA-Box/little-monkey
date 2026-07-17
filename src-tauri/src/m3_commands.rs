//! Thin Tauri command surface for the M3 runtime hub.
//!
//! The app root must manage an `M3CommandState` constructed with the real
//! runtime, inference, hardware, keychain, and network dependencies. Streaming
//! HTTP/SSE uses `M3RuntimeHub::dispatch_api_stream` directly from the server;
//! the IPC command intentionally handles only non-streaming requests.

use crate::compatibility_hub::{
    LanServerPolicy, PairedToken, PairingChallengeView, PairingRequest, ScopedTokenView,
    SecurityAuditEvent,
};
use crate::m3_production::M3CatalogSourceConfig;
use crate::m3_runtime_hub::{
    M3ActivateComponentVersionRequest, M3ActivateModelVersionRequest, M3ApiDispatchRequest,
    M3ApiDispatchResponse, M3CancelInferenceRequest, M3CatalogMatch, M3CleanupReport,
    M3ComponentCatalogEntry, M3ComponentHub, M3ComponentUpdateCheck, M3DeleteModelRequest,
    M3DownloadRequest, M3HardwareCompatibilityReport, M3HubError, M3InstallComponentRequest,
    M3InstalledComponentView, M3InstalledModelView, M3LoadModelRequest, M3OperationContext,
    M3PruneModelVersionsRequest, M3RuntimeCapabilityView, M3RuntimeHub, M3RuntimeMetricsView,
    M3RuntimeStatusView, M3SetRuntimeConfigRequest, M3StorageStatus, M3UnloadModelRequest,
    M3VerifyProjectorRequest,
};
use crate::runtime_adapter::{
    HardwareProfile, HardwareSnapshot, LocalOffloadPlanner, LocalRuntimeScheduler, OffloadPlan,
    OffloadPlanInput, RuntimeInventory, RuntimeLogTail, SchedulingInput, SchedulingPlan,
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
