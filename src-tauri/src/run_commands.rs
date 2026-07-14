//! Host-owned Tauri boundary for the durable run ledger.
//!
//! Webview callers submit immutable specs and semantic events, but the Rust
//! host assigns event ids, timestamps, monotonically increasing sequences,
//! and approving/requesting client identities. This keeps audit authority out
//! of model/page content while the Tauri-free `run_ledger` remains reusable by
//! the CLI, daemon, ACP server, workflows, and remote runner.

use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{Emitter, Manager};

use crate::run_ledger::{AppendEventOutcome, RunLedger, StoredRun, SubmitRunOutcome};
use crate::run_protocol::{
    ClientIdentity, ClientKind, ModelTargetSnapshot, PermissionDecision, RunEvent,
    RunEventEnvelope, RunSpec, RunStatus, RUN_PROTOCOL_SCHEMA_VERSION,
};
use crate::AppState;

pub const RUNS_CHANGED_EVENT: &str = "runs://changed";
pub const RUN_CANCELLATION_REQUESTED_EVENT: &str = "runs://cancellation-requested";
const DATABASE_FILE: &str = "profile-v1.sqlite3";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
    pub spec: RunSpec,
    pub status: RunStatus,
    pub last_sequence: u64,
    pub terminal_sequence: Option<u64>,
    pub updated_at_ms: u64,
}

impl From<StoredRun> for RunRecord {
    fn from(run: StoredRun) -> Self {
        Self {
            spec: run.spec,
            status: run.status,
            last_sequence: run.last_sequence,
            terminal_sequence: run.terminal_sequence,
            updated_at_ms: run.updated_at_ms,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSubmitResponse {
    pub run: RunRecord,
    pub inserted: bool,
}

impl From<SubmitRunOutcome> for RunSubmitResponse {
    fn from(outcome: SubmitRunOutcome) -> Self {
        Self {
            run: outcome.run.into(),
            inserted: outcome.inserted,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunAppendResponse {
    pub envelope: RunEventEnvelope,
    pub status: RunStatus,
    pub terminal: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunChangedPayload {
    pub run_id: String,
    pub status: RunStatus,
    pub last_sequence: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunCancellationRequestedPayload {
    pub run_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunLedgerIntegrity {
    pub ok: bool,
    pub violations: Vec<String>,
}

pub(crate) fn unix_time_ms() -> Result<u64, String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("System clock is before the Unix epoch: {error}"))?
        .as_millis();
    u64::try_from(millis).map_err(|_| "System timestamp exceeds the run protocol".to_string())
}

pub(crate) fn desktop_identity<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    window: &tauri::Window<R>,
) -> ClientIdentity {
    ClientIdentity {
        client_id: "little-monkey-desktop".to_string(),
        instance_id: window.label().to_string(),
        kind: ClientKind::Desktop,
        version: app.package_info().version.to_string(),
    }
}

pub(crate) fn with_ledger<R: tauri::Runtime, T>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    operation: impl FnOnce(&mut RunLedger) -> Result<T, crate::run_ledger::LedgerError>,
) -> Result<T, String> {
    let mut slot = state
        .run_ledger
        .lock()
        .map_err(|_| "Run ledger state lock was poisoned".to_string())?;
    if slot.is_none() {
        let data_dir = app
            .path()
            .app_data_dir()
            .map_err(|error| format!("Failed to resolve app data dir: {error}"))?;
        std::fs::create_dir_all(&data_dir)
            .map_err(|error| format!("Failed to create app data dir: {error}"))?;
        *slot =
            Some(RunLedger::open(data_dir.join(DATABASE_FILE)).map_err(|error| error.to_string())?);
    }
    operation(slot.as_mut().expect("run ledger initialized")).map_err(|error| error.to_string())
}

pub(crate) fn with_profile_ledger<R: tauri::Runtime, T>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    operation: impl FnOnce(&mut RunLedger) -> Result<T, crate::profile_store::ProfileStoreError>,
) -> Result<T, String> {
    let mut slot = state
        .run_ledger
        .lock()
        .map_err(|_| "Run ledger state lock was poisoned".to_string())?;
    if slot.is_none() {
        let data_dir = app
            .path()
            .app_data_dir()
            .map_err(|error| format!("Failed to resolve app data dir: {error}"))?;
        std::fs::create_dir_all(&data_dir)
            .map_err(|error| format!("Failed to create app data dir: {error}"))?;
        *slot =
            Some(RunLedger::open(data_dir.join(DATABASE_FILE)).map_err(|error| error.to_string())?);
    }
    operation(slot.as_mut().expect("run ledger initialized")).map_err(|error| error.to_string())
}

/// Resolves the provider endpoint exclusively from a host-canonicalized,
/// already-submitted run. A webview cannot combine one provider's key with
/// an arbitrary endpoint, and later provider-setting changes cannot retarget
/// an in-flight request.
pub(crate) fn provider_endpoint_for_run(
    app: &tauri::AppHandle,
    state: &AppState,
    run_id: &str,
    provider_id: &str,
    model: &str,
) -> Result<String, String> {
    let run = with_ledger(app, state, |ledger| {
        ledger
            .load_run(run_id)?
            .ok_or_else(|| crate::run_ledger::LedgerError::NotFound {
                entity: "run",
                id: run_id.to_string(),
            })
    })?;
    match run.spec.target {
        ModelTargetSnapshot::Provider {
            provider_id: frozen_provider,
            endpoint,
            model: frozen_model,
            credential_ref_id,
            ..
        } if frozen_provider == provider_id
            && frozen_model == model
            && credential_ref_id == crate::providers::credential_ref_id(provider_id) =>
        {
            Ok(endpoint)
        }
        ModelTargetSnapshot::Provider { .. } => {
            Err("Provider request does not match the immutable run target".to_string())
        }
        _ => Err("Run target is not a provider model".to_string()),
    }
}

fn emit_changed<R: tauri::Runtime>(app: &tauri::AppHandle<R>, outcome: &AppendEventOutcome) {
    let _ = app.emit(
        RUNS_CHANGED_EVENT,
        RunChangedPayload {
            run_id: outcome.run_id.clone(),
            status: outcome.status,
            last_sequence: outcome.sequence,
        },
    );
}

pub(crate) fn engine_identity<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    instance_id: &str,
) -> ClientIdentity {
    ClientIdentity {
        client_id: "little-monkey-engine".to_string(),
        instance_id: instance_id.to_string(),
        kind: ClientKind::Desktop,
        version: app.package_info().version.to_string(),
    }
}

pub(crate) fn append_event_as<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    run_id: String,
    actor_id: Option<String>,
    mut event: RunEvent,
    identity: ClientIdentity,
) -> Result<RunAppendResponse, String> {
    match &mut event {
        RunEvent::PermissionDecided { decided_by, .. } => *decided_by = identity.clone(),
        RunEvent::CancellationRequested { requested_by, .. } => *requested_by = identity.clone(),
        _ => {}
    }
    let occurred_at_ms = unix_time_ms()?;
    let envelope = with_ledger(app, state, |ledger| {
        let run =
            ledger
                .load_run(&run_id)?
                .ok_or_else(|| crate::run_ledger::LedgerError::NotFound {
                    entity: "run",
                    id: run_id.clone(),
                })?;
        let sequence = run
            .last_sequence
            .checked_add(1)
            .ok_or(crate::run_ledger::LedgerError::NumericOverflow("sequence"))?;
        let envelope = RunEventEnvelope {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            event_id: format!("event-{}", uuid::Uuid::new_v4().simple()),
            run_id: run_id.clone(),
            sequence,
            occurred_at_ms,
            actor_id,
            emitter: identity,
            event,
        };
        let outcome = ledger.append_event(&envelope)?;
        Ok((envelope, outcome))
    })?;
    emit_changed(app, &envelope.1);
    Ok(RunAppendResponse {
        envelope: envelope.0,
        status: envelope.1.status,
        terminal: envelope.1.terminal,
    })
}

pub(crate) fn append_host_event<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    window: &tauri::Window<R>,
    state: &AppState,
    run_id: String,
    actor_id: Option<String>,
    event: RunEvent,
) -> Result<RunAppendResponse, String> {
    let identity = desktop_identity(app, window);
    append_event_as(app, state, run_id, actor_id, event, identity)
}

#[tauri::command]
pub fn run_protocol_version() -> u32 {
    RUN_PROTOCOL_SCHEMA_VERSION
}

#[tauri::command]
pub fn run_submit(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
    mut spec: RunSpec,
) -> Result<RunSubmitResponse, String> {
    spec.submitted_by = desktop_identity(&app, &window);
    match &mut spec.target {
        ModelTargetSnapshot::Provider {
            provider_id,
            endpoint,
            credential_ref_id,
            ..
        } => {
            *endpoint = crate::providers::configured_endpoint(&app, provider_id)?;
            *credential_ref_id = crate::providers::credential_ref_id(provider_id);
        }
        ModelTargetSnapshot::Ollama { base_url, .. } => {
            *base_url = crate::ollama::OLLAMA_BASE_URL.to_string();
        }
        ModelTargetSnapshot::ManagedLlama {
            model_id,
            model_path,
            ..
        } => {
            let active_path = state
                .llama
                .lock()
                .map_err(|_| "Managed llama state lock was poisoned".to_string())?
                .model_path
                .clone()
                .ok_or_else(|| "No managed llama.cpp model is active".to_string())?;
            let canonical = std::fs::canonicalize(&active_path)
                .map_err(|error| format!("Active managed model path is unavailable: {error}"))?;
            *model_path = canonical.to_string_lossy().into_owned();
            *model_id = canonical
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("managed-model")
                .to_string();
        }
    }
    let response: RunSubmitResponse =
        with_ledger(&app, state.inner(), |ledger| ledger.submit_run(&spec))?.into();
    let _ = app.emit(
        RUNS_CHANGED_EVENT,
        RunChangedPayload {
            run_id: response.run.spec.run_id.clone(),
            status: response.run.status,
            last_sequence: response.run.last_sequence,
        },
    );
    Ok(response)
}

#[tauri::command]
pub fn run_append_event(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
    run_id: String,
    actor_id: Option<String>,
    event: RunEvent,
) -> Result<RunAppendResponse, String> {
    if matches!(
        &event,
        RunEvent::PermissionRequested { .. }
            | RunEvent::PermissionDecided { .. }
            | RunEvent::AwaitingApproval { .. }
            | RunEvent::CancellationRequested { .. }
    ) {
        return Err(
            "Permission and cancellation authority events must use their dedicated host command"
                .to_string(),
        );
    }
    append_host_event(&app, &window, state.inner(), run_id, actor_id, event)
}

#[tauri::command]
pub fn run_decide_permission(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
    run_id: String,
    request_id: String,
    operation_sha256: String,
    decision: PermissionDecision,
) -> Result<RunAppendResponse, String> {
    let now = unix_time_ms()?;
    let approval = with_ledger(&app, state.inner(), |ledger| {
        ledger.load_approval(&run_id, &request_id)?.ok_or_else(|| {
            crate::run_ledger::LedgerError::NotFound {
                entity: "approval",
                id: request_id.clone(),
            }
        })
    })?;
    if approval.operation_sha256 != operation_sha256 {
        return Err("Approval operation digest does not match the recorded request".to_string());
    }
    if approval.decision.is_some() {
        return Err("Approval request has already been decided".to_string());
    }
    if now >= approval.expires_at_ms && decision != PermissionDecision::Expired {
        return Err("Approval request has expired".to_string());
    }
    if now < approval.expires_at_ms && decision == PermissionDecision::Expired {
        return Err("Approval request has not expired".to_string());
    }
    let pending_resolution = match &decision {
        PermissionDecision::AllowOnce => (true, false),
        PermissionDecision::AllowForRun => (true, true),
        PermissionDecision::Deny | PermissionDecision::Expired => (false, false),
    };
    let response = append_host_event(
        &app,
        &window,
        state.inner(),
        run_id,
        None,
        RunEvent::PermissionDecided {
            request_id: request_id.clone(),
            operation_sha256,
            decision,
            decided_by: desktop_identity(&app, &window),
        },
    )?;
    // If this run is executing in the current desktop process, wake the exact
    // permission waiter too. Daemon/remote approvals legitimately have no
    // local waiter, so absence is not an error.
    let _ = crate::permissions::respond_if_pending(
        state.inner(),
        &request_id,
        pending_resolution.0,
        pending_resolution.1,
    )?;
    Ok(response)
}

#[tauri::command]
pub fn run_request_cancellation(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
    run_id: String,
    reason: Option<String>,
) -> Result<RunAppendResponse, String> {
    let response = append_host_event(
        &app,
        &window,
        state.inner(),
        run_id,
        None,
        RunEvent::CancellationRequested {
            requested_by: desktop_identity(&app, &window),
            reason,
        },
    )?;
    let _ = app.emit(
        RUN_CANCELLATION_REQUESTED_EVENT,
        RunCancellationRequestedPayload {
            run_id: response.envelope.run_id.clone(),
        },
    );
    Ok(response)
}

#[tauri::command]
pub fn run_get(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    run_id: String,
) -> Result<Option<RunRecord>, String> {
    with_ledger(&app, state.inner(), |ledger| ledger.load_run(&run_id))
        .map(|run| run.map(Into::into))
}

#[tauri::command]
pub fn run_list(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    limit: usize,
) -> Result<Vec<RunRecord>, String> {
    with_ledger(&app, state.inner(), |ledger| ledger.list_runs(limit))
        .map(|runs| runs.into_iter().map(Into::into).collect())
}

#[tauri::command]
pub fn run_events(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    run_id: String,
    after_sequence: u64,
    limit: usize,
) -> Result<Vec<RunEventEnvelope>, String> {
    with_ledger(&app, state.inner(), |ledger| {
        ledger.load_events(&run_id, after_sequence, limit)
    })
}

#[tauri::command]
pub fn run_integrity_check(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<RunLedgerIntegrity, String> {
    let report = with_ledger(&app, state.inner(), |ledger| ledger.integrity_check())?;
    Ok(RunLedgerIntegrity {
        ok: report.is_ok(),
        violations: report.violations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_command_matches_contract() {
        assert_eq!(run_protocol_version(), RUN_PROTOCOL_SCHEMA_VERSION);
    }

    #[test]
    fn host_event_ids_are_protocol_safe() {
        let id = format!("event-{}", uuid::Uuid::new_v4().simple());
        crate::run_protocol::validate_protocol_id("event_id", &id).unwrap();
    }
}
