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
use crate::benchmark::{
    self, BenchmarkFreshness, BenchmarkReport, BenchmarkRequest, MachineIdentity, PeakMemory,
};
use crate::chat_template_lab::{run_chat_template_lab, ChatTemplateLabReport, TemplateFamily};
use crate::compatibility_hub::{
    CanonicalContent, CanonicalInferenceRequest, CanonicalMessage, CanonicalRole,
    CompatibilityProtocol, LanServerPolicy, PairedToken, PairingChallengeView, PairingRequest,
    ScopedTokenView, SecurityAuditEvent, COMPATIBILITY_SCHEMA_VERSION,
};
use crate::context_cache::{
    self, ContextCacheView, ContextFailureClassification, ContextFailureInput, ContextRuntimeKind,
    EffectiveContextInput, EffectiveContextResolution,
};
use crate::m3_production::M3CatalogSourceConfig;
use crate::m3_runtime_hub::M3ComponentKind;
use crate::m3_runtime_hub::{
    M3ActivateComponentVersionRequest, M3ActivateModelVersionRequest, M3ApiCaller,
    M3ApiDispatchRequest, M3ApiDispatchResponse, M3CancelInferenceRequest, M3CatalogMatch,
    M3CleanupReport, M3CompatibilityMatrixReport, M3ComponentCatalogEntry, M3ComponentHub,
    M3ComponentUpdateCheck, M3DeleteModelRequest, M3DownloadRequest, M3HardwareCompatibilityReport,
    M3HubError, M3HubResult, M3InstallComponentRequest, M3InstalledComponentView,
    M3InstalledModelView, M3LoadModelRequest, M3OperationContext, M3PruneModelVersionsRequest,
    M3RuntimeCapabilityView, M3RuntimeHub, M3RuntimeKind, M3RuntimeMetricsView,
    M3RuntimeStatusView, M3SetRuntimeConfigRequest, M3SettingCapabilitiesView, M3StorageStatus,
    M3UnloadModelRequest, M3VerifyProjectorRequest,
};
use crate::profiles::ProfileScopedPaths;
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
    SupportBundle, TraceFieldNote,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::Manager;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

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

/// Hands the MLX memory slot to Studio before an MLX video run. Chat and video
/// use different service entry points, so they cannot share one resident
/// process, but they must not keep both tens-of-gigabytes weight sets alive.
#[cfg(target_os = "macos")]
pub async fn unload_mlx_for_studio(state: &M3CommandState) -> Result<(), String> {
    let has_mlx = state
        .hub
        .list_runtimes()
        .map_err(command_error)?
        .iter()
        .any(|runtime| runtime.descriptor.kind == M3RuntimeKind::Mlx);
    if !has_mlx {
        return Ok(());
    }

    let operation_id = format!("studio-mlx-handoff-{}", Uuid::new_v4());
    let context = state.begin_operation(&operation_id, Some(30_000))?;
    let result = async {
        match state.hub.runtime_status("mlx", &context).await? {
            M3RuntimeStatusView::Mlx {
                status:
                    crate::mlx_runtime::MlxRuntimeStatus::Running { handle, .. },
            } => {
                state
                    .hub
                    .unload_model(
                        &M3UnloadModelRequest {
                            runtime_id: "mlx".to_string(),
                            model_id: handle.model_id,
                            force_exact_owner: true,
                        },
                        &context,
                    )
                    .await
            }
            _ => Ok(()),
        }
    }
    .await;
    state.finish_operation(&operation_id);
    result.map_err(command_error)
}

#[cfg(not(target_os = "macos"))]
pub async fn unload_mlx_for_studio(_state: &M3CommandState) -> Result<(), String> {
    Ok(())
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

/// A benchmark report plus the machine it was measured on.
///
/// The snapshot is attached here rather than inside `BenchmarkReport` because
/// `benchmark.rs` has no way to probe hardware and must not acquire one — it can
/// measure a generation and nothing else. Keeping them separate is also what
/// lets a stored report be invalidated when the hardware changes.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct M3BenchmarkResponse {
    pub report: BenchmarkReport,
    pub hardware: HardwareSnapshot,
}

/// ROADMAP #2. Times `repeats` real streamed generations of `model` on
/// `runtime_id` and reports what this machine measured.
///
/// The whole surface exists to satisfy one sentence — "no number is displayed
/// that was not measured on the machine displaying it" — so it refuses rather
/// than reports in the two cases where it could not honour that: a request too
/// small to measure (rejected by [`BenchmarkRequest::validated`]) and a model
/// that does not run here at all (below).
#[tauri::command]
pub async fn m3_benchmark_run(
    app: tauri::AppHandle,
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    request: BenchmarkRequest,
) -> Result<M3BenchmarkResponse, String> {
    let request = request.validated()?;
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result = benchmark_with_context(&state, &request, &context).await;
    let response = finish(&state, &operation_id, result).await?;

    // Persisted after the operation is finished, not inside it: a benchmark that
    // measured fine but could not be written down is still a valid measurement,
    // and failing the command would throw away the wall-clock the user spent.
    // The write error is surfaced through the log rather than swallowed.
    let path = benchmark_history_path(&app)?;
    let stored = StoredBenchmark {
        report: response.report.clone(),
        machine: machine_identity(&response.hardware),
        measured_at_ms: trusted_now_ms(),
    };
    if let Err(error) = load_benchmarks(&path)
        .and_then(|existing| save_benchmarks(&path, &remember_benchmark(existing, stored)))
    {
        eprintln!("benchmark measured but not saved: {error}");
    }
    Ok(response)
}

/// Every benchmark this machine has kept, most recent first, each labelled with
/// whether its numbers still describe the machine asking.
///
/// The freshness verdict is computed here rather than stored, because the
/// question is "is this still true *now*" — a report that was fresh yesterday
/// and stale today has not changed, the machine has.
#[tauri::command]
pub fn m3_benchmark_history(
    app: tauri::AppHandle,
    state: tauri::State<'_, M3CommandState>,
) -> Result<Vec<BenchmarkHistoryEntry>, String> {
    let current = machine_identity(&state.hub.hardware_snapshot().map_err(command_error)?);
    Ok(load_benchmarks(&benchmark_history_path(&app)?)?
        .into_iter()
        .map(|stored| BenchmarkHistoryEntry {
            freshness: stored.machine.freshness_against(&current),
            stored,
        })
        .collect())
}

fn benchmark_history_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .profile_data_dir()
        .map_err(|error| format!("Failed to resolve app data dir: {error}"))?
        .join(BENCHMARK_FILE))
}

/// The I/O half of the benchmark, kept out of `benchmark.rs` for the same reason
/// `admission.rs` is pure and `engine.rs` is not: everything with a decision in
/// it should be testable without a hub.
async fn benchmark_with_context(
    state: &M3CommandState,
    request: &BenchmarkRequest,
    context: &M3OperationContext,
) -> Result<M3BenchmarkResponse, M3HubError> {
    let hardware = state.hub.hardware_snapshot()?;
    refuse_a_model_this_machine_does_not_run(state, request, context).await?;

    let pid = state
        .hub
        .benchmark_runtime_pid(&request.runtime_id, context)
        .await?;
    let mut memory_notes = Vec::new();
    let before = read_peak_mark(pid, &mut memory_notes);

    let canonical = canonical_benchmark_request(request);
    let mut samples = Vec::with_capacity(request.repeats as usize);
    for repeat in 0..request.repeats {
        let mut canonical = canonical.clone();
        // A distinct id per repeat: the hub tracks an in-flight inference by
        // request id, and reusing one would make the second repeat a duplicate.
        canonical.request_id = format!("{}-{repeat}", canonical.request_id);
        samples.push(
            state
                .hub
                .benchmark_stream_once(&request.runtime_id, &canonical, context)
                .await?,
        );
    }

    let after = read_peak_mark(pid, &mut memory_notes);
    let mut peak_memory = PeakMemory::measure(pid, before, after);
    peak_memory.unavailable.extend(memory_notes);

    let mut report = benchmark::summarize(
        request,
        // Genuinely unavailable rather than skipped: no runtime here reports a
        // quantization scheme for a loaded model, and a GGUF's own
        // `general.quantization_version` is a format version ("2"), not the
        // scheme ("Q4_K_M") a reader would take it for. Naming the model's file
        // is not the same as knowing how it was quantized.
        None,
        benchmark::WARMUP_REPEATS,
        samples,
        peak_memory,
    );
    report.unavailable.push(TraceFieldNote {
        field: "quantization".to_string(),
        reason: "neither this runtime's inventory nor the model's GGUF header reports a \
                 quantization scheme, so the benchmarked triple is identified by model and \
                 runtime only"
            .to_string(),
    });

    Ok(M3BenchmarkResponse { report, hardware })
}

/// Refuses a model whose numbers would have been produced somewhere else.
///
/// The runtime inventory marks a hosted model `is_cloud`, and timing one
/// measures a network round trip to a GPU this user does not own — the exact
/// thing ROADMAP #2's "on the machine displaying it" clause rules out. A model
/// the inventory does not list at all is also refused, since the alternative is
/// a per-repeat "model not found" reported as a failed measurement.
async fn refuse_a_model_this_machine_does_not_run(
    state: &M3CommandState,
    request: &BenchmarkRequest,
    context: &M3OperationContext,
) -> Result<(), M3HubError> {
    let inventory = state
        .hub
        .runtime_inventory(&request.runtime_id, context)
        .await?;
    let Some(model) = inventory
        .models
        .iter()
        .find(|model| model.model_id == request.model)
    else {
        return Err(M3HubError::NotFound(format!(
            "runtime {} does not list model {}",
            request.runtime_id, request.model
        )));
    };
    if model.metadata.get("is_cloud").map(String::as_str) == Some("true") {
        return Err(M3HubError::Unsupported(format!(
            "{} runs in the cloud, so timing it would measure a network round trip and somebody \
             else's hardware rather than this machine",
            request.model
        )));
    }
    Ok(())
}

/// Filename for the persisted benchmark history under the app data directory —
/// the same file-per-feature pattern as `web_settings.json`/`providers.json`.
///
/// Not a table in the run ledger: a benchmark measures *this machine*, not a
/// run, and hanging it off the ledger's run-shaped schema would be the same
/// category error `permission_decisions` had to work around.
const BENCHMARK_FILE: &str = "benchmarks.json";

/// How many reports to keep. One per model per runtime is the useful unit, and a
/// user with more than this many benchmarked models is better served by the most
/// recent ones than by an unbounded file.
const MAX_STORED_BENCHMARKS: usize = 32;

/// A report, the machine it was measured on, and when.
///
/// The machine identity is stored *with* the report rather than derived later,
/// because "was this measured here" is unanswerable once the snapshot is gone —
/// and answering it wrong is exactly the failure the benchmark surface exists to
/// prevent.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredBenchmark {
    pub report: BenchmarkReport,
    pub machine: MachineIdentity,
    pub measured_at_ms: u64,
}

/// A stored report paired with whether its numbers may be shown.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkHistoryEntry {
    #[serde(flatten)]
    pub stored: StoredBenchmark,
    pub freshness: BenchmarkFreshness,
}

/// The stable parts of a snapshot a stored benchmark is valid for.
///
/// Lives here rather than in `benchmark.rs` so that module keeps no dependency
/// on hardware probing — it may compare an identity it is handed, never build
/// one from the host.
fn machine_identity(hardware: &HardwareSnapshot) -> MachineIdentity {
    let mut accelerators: Vec<String> = hardware
        .platform
        .accelerators
        .iter()
        .flat_map(|accelerator| accelerator.device_names.iter().cloned())
        .collect();
    accelerators.sort();
    MachineIdentity {
        os: hardware.platform.os.clone(),
        arch: hardware.platform.arch.clone(),
        total_ram_bytes: hardware.total_ram_bytes,
        logical_cpu_count: hardware.logical_cpu_count,
        accelerators,
    }
}

/// Reads the stored history, treating a missing file as empty.
///
/// A *corrupt* file is an error rather than an empty history: silently starting
/// over would discard measurements the user paid real wall-clock time for, and
/// they would never learn it happened.
fn load_benchmarks(path: &Path) -> Result<Vec<StoredBenchmark>, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(|e| format!("Corrupt {BENCHMARK_FILE}: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("Failed to read {BENCHMARK_FILE}: {e}")),
    }
}

/// Atomic sibling temp file + rename, the same idiom as `web.rs`'s
/// `save_settings_impl` and `mcp.rs`'s `save_config_impl`.
fn save_benchmarks(path: &Path, stored: &[StoredBenchmark]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create the app data dir: {e}"))?;
    }
    let temporary = path.with_extension("json.tmp");
    let serialized = serde_json::to_vec_pretty(stored)
        .map_err(|e| format!("Failed to encode benchmarks: {e}"))?;
    std::fs::write(&temporary, serialized)
        .map_err(|e| format!("Failed to write {BENCHMARK_FILE}: {e}"))?;
    std::fs::rename(&temporary, path).map_err(|e| format!("Failed to save {BENCHMARK_FILE}: {e}"))
}

/// Insert `fresh`, replacing any earlier report for the same runtime and model.
///
/// Most recent first, capped at [`MAX_STORED_BENCHMARKS`]. Replacing rather than
/// appending is deliberate: two reports for one model on one runtime differ only
/// by when they ran, and keeping both invites a reader to compare numbers that
/// were never meant to be a time series.
fn remember_benchmark(
    existing: Vec<StoredBenchmark>,
    fresh: StoredBenchmark,
) -> Vec<StoredBenchmark> {
    let mut kept: Vec<StoredBenchmark> = existing
        .into_iter()
        .filter(|entry| {
            entry.report.runtime_id != fresh.report.runtime_id
                || entry.report.model != fresh.report.model
        })
        .collect();
    kept.insert(0, fresh);
    kept.truncate(MAX_STORED_BENCHMARKS);
    kept
}

/// Reads the runtime process's high-water mark, collecting the platform's own
/// reason when it has none to give.
fn read_peak_mark(pid: Option<i64>, notes: &mut Vec<TraceFieldNote>) -> Option<u64> {
    let pid = pid?;
    let (bytes, note) = PeakMemory::sample_mark(pid);
    if let Some(note) = note {
        if !notes.contains(&note) {
            notes.push(note);
        }
    }
    bytes
}

/// Builds the canonical request directly, with no protocol translation: a
/// benchmark is not an API caller, and `temperature: 0` keeps repeats comparable
/// rather than measuring a different sampling path each time.
fn canonical_benchmark_request(request: &BenchmarkRequest) -> CanonicalInferenceRequest {
    CanonicalInferenceRequest {
        schema_version: COMPATIBILITY_SCHEMA_VERSION,
        protocol: CompatibilityProtocol::OpenAiChatCompletions,
        request_id: format!("benchmark-{}", trusted_now_ms()),
        model: request.model.clone(),
        messages: vec![CanonicalMessage {
            role: CanonicalRole::User,
            content: vec![CanonicalContent::Text {
                text: request.prompt_text().to_string(),
            }],
        }],
        tools: Vec::new(),
        max_output_tokens: request.max_output_tokens,
        temperature: Some(0.0),
        stream: true,
        response_schema: None,
        metadata: serde_json::Value::Null,
    }
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
    app: tauri::AppHandle,
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    request: M3LoadModelRequest,
) -> Result<(), String> {
    if request.runtime_id == "mlx" {
        if let Some(app_state) = app.try_state::<crate::AppState>() {
            app_state.generation_engine.stop()?;
        }
    }
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
        prefix_sharing: context_cache::prefix_sharing(kind),
        context_budget: context_cache::context_budget_enforcement(kind),
        notes,
        sampled_at_ms: context_cache_sampled_at_ms(),
    })
}

/// The stated eviction/compaction policy for every process class (roadmap K11).
///
/// All four at once rather than one per query: the policy is only meaningful as
/// a comparison — "interactive compacts, maintenance stops" is the fact, and one
/// class in isolation reads like a global setting.
#[tauri::command]
pub fn m3_context_policies() -> Vec<ContextPolicyEntry> {
    use crate::run_protocol::ProcessClass;
    [
        ProcessClass::Interactive,
        ProcessClass::Batch,
        ProcessClass::Background,
        ProcessClass::Maintenance,
    ]
    .into_iter()
    .map(|class| ContextPolicyEntry {
        class: class.token(),
        policy: crate::context_cache::context_policy(class),
    })
    .collect()
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextPolicyEntry {
    pub class: &'static str,
    pub policy: crate::context_cache::ContextPolicy,
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

/// Run the published K21 conformance suite against a live node and return
/// its report.
///
/// Takes no hub state on purpose. The suite's whole claim is that it grades a
/// node from the outside, over a socket, exactly as a third party would —
/// handing it a `State<M3CommandState>` would let a future edit shortcut a
/// check by reading the hub the node is built on. `base_url` therefore points
/// wherever the operator says, including at a node this app did not start.
///
/// The token is the operator's own API-server token. Nothing is persisted:
/// the desktop stores only token digests, so a plaintext round trip through
/// here would be the only place one lived.
#[tauri::command]
pub async fn run_conformance_suite(
    base_url: String,
    token: Option<String>,
    sections: Vec<String>,
) -> Result<crate::conformance::ConformanceReport, String> {
    let mut selected = Vec::new();
    for name in &sections {
        selected.push(
            crate::conformance::SectionId::parse(name)
                .ok_or_else(|| format!("Unknown conformance section '{name}'."))?,
        );
    }
    let options = crate::conformance::SuiteOptions {
        base_url,
        token: token.filter(|token| !token.trim().is_empty()),
        sections: selected,
        model: None,
    };
    let client = crate::conformance::client()?;
    Ok(crate::conformance::run_suite(&client, &options).await)
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
#[cfg(target_os = "macos")]
#[tauri::command]
pub fn m3_mlx_install(
    app: tauri::AppHandle,
    package_directory: String,
) -> Result<crate::m3_production::MlxInstalledPackageView, String> {
    let app_data_dir = app
        .profile_data_dir()
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
#[cfg(target_os = "macos")]
#[tauri::command]
pub fn m3_mlx_install_component(
    app: tauri::AppHandle,
    state: tauri::State<'_, M3CommandState>,
    component_id: String,
) -> Result<crate::m3_production::MlxInstalledPackageView, String> {
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
        .profile_data_dir()
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

/// Folds a caller-supplied catalog into the registry, atomically.
///
/// This is what **Import catalog** calls. It is the same adoption path
/// [`m3_component_sync_catalog`] uses — one merge rule, one identity, one write —
/// so a file the user picked and a catalog the app fetched cannot end up adopted
/// by two subtly different sets of rules.
#[tauri::command]
pub fn m3_component_merge_registry_entries(
    state: tauri::State<'_, M3CommandState>,
    entries: Vec<M3ComponentCatalogEntry>,
) -> Result<Vec<M3ComponentCatalogEntry>, String> {
    let _guard = command_lock(&state.catalog_mutation)?;
    crate::m3_production::merge_component_registry_entries(&state.component_hub, entries)
        .map_err(command_error)
}

/// Fetches a component catalog and returns what it lists, without persisting
/// any of it.
///
/// Kept as its own command because fetching and adopting are separate acts and
/// only the second one writes: this is what a diagnostic or a test asks for when
/// the question is "what does the published catalog say?". The panel calls
/// [`m3_component_sync_catalog`], which is this plus the merge, under the lock.
#[tauri::command]
pub async fn m3_component_fetch_catalog(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    url: Option<String>,
) -> Result<Vec<M3ComponentCatalogEntry>, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let url =
        url.unwrap_or_else(|| crate::m3_production::DEFAULT_COMPONENT_CATALOG_URL.to_string());
    let result = crate::m3_runtime_hub::fetch_component_catalog(&url, &context).await;
    finish(&state, &operation_id, result).await
}

/// Fetches the published catalog and adopts it, returning the registry that
/// resulted.
///
/// One command rather than a fetch the frontend then merges and writes back, for
/// the reason `m3_production::merge_component_registry_entries` gives: a
/// read-modify-write split across the IPC boundary can lose a concurrent import or
/// a hand edit, and the backend is the authority on registry integrity. The whole
/// catalog is validated before the lock is taken, so an invalid catalog cannot
/// even reach the file — it is adopted whole or not at all.
///
/// Defaults to the catalog this project publishes, and takes a URL so a
/// self-hosted or air-gapped mirror is a setting rather than a rebuild.
#[tauri::command]
pub async fn m3_component_sync_catalog(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    url: Option<String>,
) -> Result<Vec<M3ComponentCatalogEntry>, String> {
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let endpoint =
        url.unwrap_or_else(|| crate::m3_production::DEFAULT_COMPONENT_CATALOG_URL.to_string());
    let fetched = crate::m3_runtime_hub::fetch_component_catalog(&endpoint, &context).await;
    // The operation is finished before the lock is taken: a fetch that failed must
    // release its operation slot, and the merge below is disk work that the
    // network deadline has nothing to say about.
    let fetched = finish(&state, &operation_id, fetched).await?;
    let _guard = command_lock(&state.catalog_mutation)?;
    crate::m3_production::merge_component_registry_entries(&state.component_hub, fetched)
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

#[cfg(target_os = "macos")]
#[tauri::command]
pub async fn m3_component_install(
    app: tauri::AppHandle,
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    request: M3InstallComponentRequest,
) -> Result<M3InstalledComponentView, String> {
    let app_data_dir = app
        .profile_data_dir()
        .map_err(|error| command_error(M3HubError::Runtime(error.to_string())))?;
    component_install_impl(&state, operation_id, timeout_ms, request, |artifact| {
        crate::m3_production::install_mlx_from_artifact(&app_data_dir, artifact).map(|_| ())
    })
    .await
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
pub async fn m3_component_install(
    state: tauri::State<'_, M3CommandState>,
    operation_id: String,
    timeout_ms: Option<u64>,
    request: M3InstallComponentRequest,
) -> Result<M3InstalledComponentView, String> {
    component_install_impl(&state, operation_id, timeout_ms, request, |_| Ok(())).await
}

async fn component_install_impl<F>(
    state: &M3CommandState,
    operation_id: String,
    timeout_ms: Option<u64>,
    request: M3InstallComponentRequest,
    mlx_precommit: F,
) -> Result<M3InstalledComponentView, String>
where
    F: Fn(&Path) -> M3HubResult<()>,
{
    let context = state.begin_operation(&operation_id, timeout_ms)?;
    let result: M3HubResult<M3InstalledComponentView> = async {
        if request.entry.kind == M3ComponentKind::MlxRuntime && !cfg!(target_os = "macos") {
            return Err(M3HubError::Unsupported(
                "MLX runtime components require macOS".to_string(),
            ));
        }
        #[cfg(target_os = "macos")]
        if request.entry.kind == M3ComponentKind::MlxRuntime {
            return state
                .component_hub
                .install_component_with_precommit(&request, &context, |artifact| {
                    mlx_precommit(artifact)
                })
                .await;
        }
        state
            .component_hub
            .install_component(&request, &context)
            .await
    }
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

    fn stored(runtime_id: &str, model: &str, measured_at_ms: u64) -> StoredBenchmark {
        StoredBenchmark {
            report: BenchmarkReport {
                schema_version: 1,
                runtime_id: runtime_id.to_string(),
                model: model.to_string(),
                quantization: None,
                max_output_tokens: 128,
                repeats_requested: 5,
                warmup_discarded: 1,
                samples: Vec::new(),
                time_to_first_token_ms: None,
                decode_tokens_per_second: None,
                peak_memory: PeakMemory::measure(None, None, None),
                unavailable: Vec::new(),
            },
            machine: MachineIdentity {
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
                total_ram_bytes: 16 << 30,
                logical_cpu_count: 8,
                accelerators: Vec::new(),
            },
            measured_at_ms,
        }
    }

    /// Two reports for one model on one runtime differ only by when they ran, so
    /// keeping both would invite a reader to compare numbers that were never
    /// meant to be a time series.
    #[test]
    fn re_benchmarking_a_pair_replaces_its_entry_rather_than_appending() {
        let history = remember_benchmark(Vec::new(), stored("llama-local", "qwen3-4b", 1));
        let history = remember_benchmark(history, stored("llama-local", "gemma3-4b", 2));
        let history = remember_benchmark(history, stored("llama-local", "qwen3-4b", 3));

        assert_eq!(
            history.len(),
            2,
            "the qwen entry was replaced, not appended"
        );
        assert_eq!(history[0].report.model, "qwen3-4b");
        assert_eq!(history[0].measured_at_ms, 3, "most recent first");
        assert_eq!(history[1].report.model, "gemma3-4b");

        // Same model on a different runtime is a different measurement.
        let history = remember_benchmark(history, stored("ollama", "qwen3-4b", 4));
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn the_history_is_capped_and_drops_the_oldest() {
        let mut history = Vec::new();
        for index in 0..(MAX_STORED_BENCHMARKS + 5) {
            history = remember_benchmark(
                history,
                stored("llama-local", &format!("model-{index}"), index as u64 + 1),
            );
        }
        assert_eq!(history.len(), MAX_STORED_BENCHMARKS);
        assert_eq!(
            history[0].report.model,
            format!("model-{}", MAX_STORED_BENCHMARKS + 4),
            "the newest survives"
        );
        assert!(
            !history.iter().any(|entry| entry.report.model == "model-0"),
            "the oldest was dropped"
        );
    }

    /// A stored report may only be shown when the machine still matches. The
    /// volatile fields — `captured_at_ms`, `available_ram_bytes` — are excluded
    /// from the identity precisely so a fresh report does not read as stale a
    /// second after it was written.
    #[test]
    fn freshness_ignores_volatile_fields_and_names_real_changes() {
        let here = stored("llama-local", "qwen3-4b", 1).machine;
        assert_eq!(
            here.freshness_against(&here.clone()),
            BenchmarkFreshness::ThisMachine
        );

        let mut more_ram = here.clone();
        more_ram.total_ram_bytes = 32 << 30;
        let BenchmarkFreshness::DifferentMachine { changed } = here.freshness_against(&more_ram)
        else {
            panic!("refitted RAM must invalidate a stored report");
        };
        assert_eq!(changed.len(), 1);
        assert!(changed[0].contains("installed RAM"), "got {changed:?}");

        let mut gpu_added = here.clone();
        gpu_added.accelerators = vec!["NVIDIA RTX 4090".to_string()];
        let BenchmarkFreshness::DifferentMachine { changed } = here.freshness_against(&gpu_added)
        else {
            panic!("a new accelerator must invalidate a stored report");
        };
        assert!(changed[0].contains("accelerators"), "got {changed:?}");
    }

    /// A corrupt file must not read as "never benchmarked": silently starting
    /// over would discard measurements the user paid wall-clock time for, and
    /// they would never learn it happened.
    #[test]
    fn a_corrupt_history_is_an_error_but_a_missing_one_is_empty() {
        let directory = std::env::temp_dir().join(format!(
            "little-monkey-benchmarks-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(BENCHMARK_FILE);
        let _ = std::fs::remove_file(&path);

        assert!(load_benchmarks(&path).unwrap().is_empty());

        let history = vec![stored("llama-local", "qwen3-4b", 1)];
        save_benchmarks(&path, &history).unwrap();
        assert_eq!(load_benchmarks(&path).unwrap(), history, "round trip");

        std::fs::write(&path, b"{ not json").unwrap();
        let error = load_benchmarks(&path).unwrap_err();
        assert!(error.contains("Corrupt"), "got {error}");

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    mod mlx_full_ladder {
        use super::*;
        use crate::m3_runtime_hub::{
            fetch_component_catalog, M3Clock, M3ComponentCatalogEntry, M3ComponentChannel,
            M3ComponentHub, M3ComponentHubDependencies, M3DownloadTransport, M3HardwareProbe,
            M3HubConfig, M3RuntimeHub, M3RuntimeHubDependencies, ReqwestM3DownloadTransport,
            M3_COMPONENT_CATALOG_SCHEMA_VERSION, M3_HUB_SCHEMA_VERSION,
        };
        use crate::mlx_runtime::tests::{write_test_signed_archive, TestSignatureVerifier};
        use crate::mlx_runtime::{MlxInstallLimits, MlxPackageInstaller};
        use crate::runtime_adapter::{HardwareSnapshot, PlatformCapabilities};
        use sha2::{Digest, Sha256};
        use std::collections::BTreeMap;
        use std::fs;
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
        use std::sync::Arc;

        struct Directory(PathBuf);
        impl Directory {
            fn new() -> Self {
                static NEXT: AtomicU64 = AtomicU64::new(1);
                let path = std::env::temp_dir().join(format!(
                    "m3-mlx-ladder-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                fs::create_dir_all(&path).expect("test root");
                Self(path)
            }
        }
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        struct Clock(AtomicU64);
        impl M3Clock for Clock {
            fn now_ms(&self) -> M3HubResult<u64> {
                Ok(self.0.fetch_add(1, Ordering::Relaxed))
            }
        }
        struct Hardware;
        impl M3HardwareProbe for Hardware {
            fn snapshot(&self) -> M3HubResult<HardwareSnapshot> {
                Ok(HardwareSnapshot {
                    captured_at_ms: 1,
                    total_ram_bytes: 32 << 30,
                    available_ram_bytes: 24 << 30,
                    logical_cpu_count: 8,
                    platform: PlatformCapabilities::from_host("macos", "aarch64", Vec::new()),
                })
            }
        }
        fn config() -> M3HubConfig {
            M3HubConfig {
                schema_version: M3_HUB_SCHEMA_VERSION,
                storage_quota_bytes: 32 << 20,
                storage_reserve_bytes: 1 << 20,
                download_chunk_bytes: 64 << 10,
                operation_timeout_ms: 10_000,
                max_catalog_results: 16,
            }
        }
        fn state(root: &Path, download: Arc<dyn M3DownloadTransport>) -> M3CommandState {
            let runtime = Arc::new(
                M3RuntimeHub::new(
                    root.join("runtime"),
                    config(),
                    M3RuntimeHubDependencies {
                        clock: Arc::new(Clock(AtomicU64::new(1))),
                        hardware: Arc::new(Hardware),
                        download: download.clone(),
                        catalogs: Vec::new(),
                        runtimes: Vec::new(),
                        runtime_reconciler: None,
                        lan_factory: None,
                    },
                )
                .expect("runtime hub"),
            );
            let components = Arc::new(
                M3ComponentHub::new(
                    root.join("components"),
                    config(),
                    M3ComponentHubDependencies {
                        clock: Arc::new(Clock(AtomicU64::new(2))),
                        download,
                        sources: Vec::new(),
                    },
                )
                .expect("component hub"),
            );
            M3CommandState::new(runtime, components)
        }
        fn sha(bytes: &[u8]) -> String {
            format!("{:x}", Sha256::digest(bytes))
        }
        fn entry(url: String, bytes: &[u8], version: &str) -> M3ComponentCatalogEntry {
            M3ComponentCatalogEntry {
                schema_version: M3_COMPONENT_CATALOG_SCHEMA_VERSION,
                source_id: "fixture-mlx".into(),
                component_id: "mlx-runtime-apple-silicon".into(),
                kind: M3ComponentKind::MlxRuntime,
                display_name: "Fixture MLX".into(),
                accelerator: None,
                version: version.into(),
                channel: M3ComponentChannel::Stable,
                download_url: url,
                sha256: sha(bytes),
                size_bytes: bytes.len() as u64,
                published_at_ms: 1,
                compatibility_note: None,
                metadata: BTreeMap::new(),
            }
        }
        fn active_mlx_version(app: &Path) -> String {
            MlxPackageInstaller::new(
                app.join("m3").join("runtimes").join("mlx"),
                Arc::new(TestSignatureVerifier),
                MlxInstallLimits::default(),
            )
            .expect("test MLX installer")
            .verify_active()
            .expect("verified active MLX package")
            .package_version
        }

        struct Fixture {
            origin: String,
            heads: Arc<AtomicUsize>,
            ranges: Arc<AtomicUsize>,
            if_ranges: Arc<AtomicUsize>,
        }
        impl Fixture {
            async fn spawn(catalog: Vec<M3ComponentCatalogEntry>, artifact: Vec<u8>) -> Self {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let heads = Arc::new(AtomicUsize::new(0));
                let ranges = Arc::new(AtomicUsize::new(0));
                let if_ranges = Arc::new(AtomicUsize::new(0));
                let asset = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let asset_addr = asset.local_addr().unwrap();
                let catalog = serde_json::to_vec(&catalog).unwrap();
                let h = heads.clone();
                let r = ranges.clone();
                let i = if_ranges.clone();
                tokio::spawn(async move {
                    while let Ok((mut s, _)) = asset.accept().await {
                        let catalog = catalog.clone();
                        let artifact = artifact.clone();
                        let h = h.clone();
                        let r = r.clone();
                        let i = i.clone();
                        tokio::spawn(async move {
                            let mut b = vec![0; 8192];
                            let n = s.read(&mut b).await.unwrap_or(0);
                            let req = String::from_utf8_lossy(&b[..n]);
                            let first = req.lines().next().unwrap_or("");
                            let range = req
                                .lines()
                                .find(|x| x.to_ascii_lowercase().starts_with("range:"))
                                .map(|x| x[6..].trim());
                            let if_range = req
                                .lines()
                                .any(|x| x.to_ascii_lowercase().starts_with("if-range:"));
                            let (status, body, extra) = if first.contains("/catalog.json") {
                                (
                                    "200 OK",
                                    catalog,
                                    "content-type: application/json\r\n".to_string(),
                                )
                            } else if first.starts_with("HEAD") {
                                h.fetch_add(1, Ordering::Relaxed);
                                (
                                    "200 OK",
                                    artifact.clone(),
                                    "accept-ranges: bytes\r\netag: \"mlx-fixture\"\r\n".to_string(),
                                )
                            } else {
                                let value = range.unwrap();
                                r.fetch_add(1, Ordering::Relaxed);
                                if if_range {
                                    i.fetch_add(1, Ordering::Relaxed);
                                }
                                let start = value
                                    .trim_start_matches("bytes=")
                                    .split('-')
                                    .next()
                                    .unwrap()
                                    .parse::<usize>()
                                    .unwrap();
                                let end = (start + 65536).min(artifact.len()) - 1;
                                ("206 Partial Content", artifact[start..=end].to_vec(), format!("content-range: bytes {start}-{end}/{}\r\netag: \"mlx-fixture\"\r\n", artifact.len()))
                            };
                            let response=format!("HTTP/1.1 {status}\r\nconnection: close\r\n{extra}content-length: {}\r\n\r\n", body.len());
                            let _ = s.write_all(response.as_bytes()).await;
                            let _ = s.write_all(&body).await;
                        });
                    }
                });
                let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = origin.local_addr().unwrap();
                tokio::spawn(async move {
                    while let Ok((mut s, _)) = origin.accept().await {
                        tokio::spawn(async move {
                            let mut b = vec![0; 1024];
                            let n = s.read(&mut b).await.unwrap_or(0);
                            let path = String::from_utf8_lossy(&b[..n])
                                .split_whitespace()
                                .nth(1)
                                .unwrap_or("/")
                                .to_string();
                            let location = format!("http://{asset_addr}{path}");
                            let response=format!("HTTP/1.1 302 Found\r\nlocation: {location}\r\nconnection: close\r\ncontent-length: 0\r\n\r\n");
                            let _ = s.write_all(response.as_bytes()).await;
                        });
                    }
                });
                Self {
                    origin: format!("http://{addr}"),
                    heads,
                    ranges,
                    if_ranges,
                }
            }
        }

        #[tokio::test]
        async fn catalog_to_signed_mlx_runtime_install_is_transactional() {
            let root = Directory::new();
            let valid = root.0.join("valid.tar.gz");
            let bad = root.0.join("bad.tar.gz");
            write_test_signed_archive(&valid, "mlx-test-v1", true);
            write_test_signed_archive(&bad, "mlx-test-v2", false);
            let valid_bytes = fs::read(&valid).unwrap();
            let bad_bytes = fs::read(&bad).unwrap();
            let valid_fixture = Fixture::spawn(Vec::new(), valid_bytes.clone()).await;
            let valid_entry = entry(
                format!("{}/artifact.tar.gz", valid_fixture.origin),
                &valid_bytes,
                "mlx-test-v1",
            );
            let catalog_fixture = Fixture::spawn(vec![valid_entry], valid_bytes.clone()).await;
            let transport = Arc::new(ReqwestM3DownloadTransport::for_loopback_fixture().unwrap());
            let state = state(&root.0, transport);
            let context = M3OperationContext::new(10_000);
            let fetched = fetch_component_catalog(
                &format!("{}/catalog.json", catalog_fixture.origin),
                &context,
            )
            .await
            .unwrap();
            crate::m3_production::merge_component_registry_entries(&state.component_hub, fetched)
                .unwrap();
            let adopted =
                crate::m3_production::component_registry_entries(state.component_hub.root())
                    .unwrap()
                    .pop()
                    .unwrap();
            let app = root.0.join("app");
            let installed = component_install_impl(
                &state,
                "mlx-op".into(),
                None,
                M3InstallComponentRequest {
                    entry: adopted.clone(),
                },
                |p| {
                    crate::m3_production::install_mlx_from_artifact_for_test(
                        &app,
                        p,
                        Arc::new(TestSignatureVerifier),
                    )
                    .map(|_| ())
                },
            )
            .await
            .unwrap();
            assert_eq!(installed.active_version_key, adopted.version_key());
            assert_eq!(active_mlx_version(&app), "mlx-test-v1");
            assert!(valid_fixture.heads.load(Ordering::Relaxed) >= 1);
            assert!(valid_fixture.ranges.load(Ordering::Relaxed) >= 3);
            assert!(valid_fixture.if_ranges.load(Ordering::Relaxed) >= 1);
            let bad_fixture = Fixture::spawn(Vec::new(), bad_bytes.clone()).await;
            let bad_entry = entry(
                format!("{}/artifact.tar.gz", bad_fixture.origin),
                &bad_bytes,
                "mlx-test-v2",
            );
            let error = component_install_impl(
                &state,
                "mlx-op".into(),
                None,
                M3InstallComponentRequest {
                    entry: bad_entry.clone(),
                },
                |p| {
                    crate::m3_production::install_mlx_from_artifact_for_test(
                        &app,
                        p,
                        Arc::new(TestSignatureVerifier),
                    )
                    .map(|_| ())
                },
            )
            .await
            .unwrap_err();
            assert!(error.contains("signature"));
            let after = state.component_hub.list_installed().unwrap();
            assert_eq!(after[0].active_version_key, adopted.version_key());
            assert!(!after[0]
                .versions
                .iter()
                .any(|v| v.version_key == bad_entry.version_key()));
            assert_eq!(active_mlx_version(&app), "mlx-test-v1");
            component_install_impl(
                &state,
                "mlx-op".into(),
                None,
                M3InstallComponentRequest { entry: adopted },
                |p| {
                    crate::m3_production::install_mlx_from_artifact_for_test(
                        &app,
                        p,
                        Arc::new(TestSignatureVerifier),
                    )
                    .map(|_| ())
                },
            )
            .await
            .expect("failed operation releases its id for immediate reuse");
        }
    }
}
