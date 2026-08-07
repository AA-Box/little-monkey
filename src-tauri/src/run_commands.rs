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
/// The ledger filename under the app data directory.
///
/// `pub(crate)` so `subsystem_audit` can open the same file from a process that
/// has only a path — one spelling, rather than a second literal that could drift.
pub(crate) const DATABASE_FILE: &str = "profile-v1.sqlite3";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
    pub spec: RunSpec,
    pub status: RunStatus,
    pub last_sequence: u64,
    pub terminal_sequence: Option<u64>,
    pub updated_at_ms: u64,
    pub archived_at_ms: Option<u64>,
}

impl From<StoredRun> for RunRecord {
    fn from(run: StoredRun) -> Self {
        Self {
            spec: run.spec,
            status: run.status,
            last_sequence: run.last_sequence,
            terminal_sequence: run.terminal_sequence,
            updated_at_ms: run.updated_at_ms,
            archived_at_ms: run.archived_at_ms,
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

/// The single place the host opens the shared profile/run database.
///
/// # Why tests get their own directory
///
/// Under `cargo test` this resolves to a per-process temp directory instead of
/// the real app data dir. `tauri::test::mock_app()` has no bundle identifier,
/// so its `app_data_dir()` is the bare platform app-data root
/// (`~/Library/Application Support` on macOS) — one file shared by every
/// checkout, worktree, and branch on the machine. Any branch carrying a newer
/// migration that ran its tests first leaves a `schema_migrations` row this
/// binary refuses to open (`apply_migrations`' "newer than this binary" guard,
/// correctly, for a real user database), and from then on every test that
/// records a permission request fails on a developer machine while passing on
/// a clean CI runner. Isolating the path keeps that guard intact and makes the
/// database this process opens always one this binary created.
fn open_ledger<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<RunLedger, String> {
    #[cfg(test)]
    let data_dir = test_ledger_dir();
    #[cfg(not(test))]
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Failed to resolve app data dir: {error}"))?;
    #[cfg(test)]
    let _ = app;

    std::fs::create_dir_all(&data_dir)
        .map_err(|error| format!("Failed to create app data dir: {error}"))?;
    RunLedger::open(data_dir.join(DATABASE_FILE)).map_err(|error| error.to_string())
}

/// Created once per test process and wiped on first use, so a run never
/// inherits the database a previous run of this binary (or of a differently
/// migrated one) left behind.
#[cfg(test)]
fn test_ledger_dir() -> std::path::PathBuf {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("little_monkey_test_ledger_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    })
    .clone()
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
        *slot = Some(open_ledger(app)?);
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
        *slot = Some(open_ledger(app)?);
    }
    operation(slot.as_mut().expect("run ledger initialized")).map_err(|error| error.to_string())
}

/// Resolves the provider endpoint exclusively from a host-canonicalized,
/// already-submitted run. A webview cannot combine one provider's key with
/// an arbitrary endpoint, and later provider-setting changes cannot retarget
/// an in-flight request.
///
/// # This is also where the run's own network permission becomes binding
///
/// `permission_policy.allow_network` has been part of every frozen `RunSpec`
/// since the protocol was written, and until now **nothing read it on any
/// outbound path**. Its only enforcement anywhere was a same-named field on
/// `sandbox.rs`'s Seatbelt request, which governs sandboxed shell children and is
/// not this flag; `recipes.rs` compares it against a tool profile but enforces
/// nothing. So a run submitted with `allow_network: false` — which is the default
/// when a submitter omits it — reached every cloud provider unimpeded.
///
/// The check lives here rather than in a new gate because this function already
/// does the hard half: it loads the frozen spec by run id, refuses to trust the
/// caller's claimed target, and is fail-closed on an unknown run. The destination
/// it returns is the thing the permission is about, so a separate consult would be
/// a second lookup of the same row with a chance of disagreeing with this one.
///
/// Loopback is exempt, and that is not a loophole — see
/// [`crate::egress::is_loopback_target`]. A local-inference run legitimately
/// carries `allow_network: false`.
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
            enforce_run_network_permission(
                &endpoint,
                run_id,
                run.spec.permission_policy.allow_network,
            )?;
            Ok(endpoint)
        }
        ModelTargetSnapshot::Provider { .. } => {
            Err("Provider request does not match the immutable run target".to_string())
        }
        _ => Err("Run target is not a provider model".to_string()),
    }
}

/// Names this gate in a denial record.
const RUN_TARGET_GUARD: &str = "run.provider-target";

/// Refuses a destination that leaves this machine when the run said it would not.
///
/// Deny-by-default in the only sense a run can express today: the permission is
/// frozen at submission, so neither the model, a skill, a package nor a routing
/// decision can widen it afterwards — the spec row is written once and there is no
/// update path to it. That is the acceptance clause's "cannot be widened at
/// runtime", for this one destination.
///
/// An endpoint that will not parse is refused rather than allowed. It reached here
/// out of a frozen spec, so a malformed one means the row is not what this build
/// expects, and guessing in the permissive direction is the wrong way to be wrong.
fn enforce_run_network_permission(
    endpoint: &str,
    run_id: &str,
    allow_network: bool,
) -> Result<(), String> {
    if allow_network {
        return Ok(());
    }

    let parsed = url::Url::parse(endpoint).map_err(|error| {
        let denial = crate::egress::EgressDenial::about(
            crate::egress::EgressRule::UrlMalformed,
            format!("the run's frozen provider endpoint does not parse: {error}"),
        );
        crate::denial_sink::record(RUN_TARGET_GUARD, &denial, Some(run_id));
        denial.to_string()
    })?;

    if crate::egress::is_loopback_target(&parsed) {
        return Ok(());
    }

    let denial = crate::egress::EgressDenial::about(
        crate::egress::EgressRule::RunNetworkDenied,
        // The origin, never the endpoint as given: these strings surface in the UI
        // and a custom provider's endpoint can carry a token in its query.
        format!(
            "this run's frozen permission_policy.allow_network is false, so it may \
             not reach {}",
            crate::egress::origin_label(&parsed)
        ),
    );
    crate::denial_sink::record(RUN_TARGET_GUARD, &denial, Some(run_id));
    Err(denial.to_string())
}

/// Installs the process-wide source [`crate::egress::send`] consults for a run's
/// frozen egress allowlist.
///
/// # Why the ledger read lives behind an installed closure
///
/// The 92 sites that route through `egress::send` have no `AppHandle` and no
/// `AppState`, and giving them one is the parameter threading `run_scope` exists to
/// replace. So the identity travels implicitly (the task-local) and the *row* behind
/// it is fetched through a closure installed once at startup, holding the one handle
/// that can reach the ledger. This is the only file that knows both halves.
///
/// Every outcome is deliberate and [`crate::egress::RunEgressPolicy`] documents which
/// direction each fails in. In particular a run id the ledger has never seen is
/// `Unknown` and permitted — `browser_worker` and `m4_runtime` both scope work under
/// ids that are not ledger runs — while a read that *fails* is `Unavailable` and
/// refused.
///
/// The read is cached per run inside `egress`, so this closure runs once per run
/// rather than once per request; a run spec is written once and never updated, so
/// there is nothing for a cache to go stale against.
///
/// One caller obligation, the same one [`drain_egress`] has and for the same reason:
/// this locks the ledger, so nothing may hold [`with_ledger`]'s guard across an
/// `egress::send`. A `std::sync::Mutex` is not reentrant, so that would deadlock
/// rather than block. No caller does, and none should.
pub(crate) fn install_run_egress_policy_source<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    let app = app.clone();
    crate::egress::install_run_policy_source(move |run_id| {
        let state = app.state::<AppState>();
        match with_ledger(&app, state.inner(), |ledger| ledger.load_run(run_id)) {
            Ok(Some(run)) => match run.spec.permission_policy.egress_allowlist {
                Some(allowlist) => {
                    crate::egress::RunEgressPolicy::Declared(std::sync::Arc::new(allowlist))
                }
                None => crate::egress::RunEgressPolicy::Undeclared,
            },
            Ok(None) => crate::egress::RunEgressPolicy::Unknown,
            Err(_) => crate::egress::RunEgressPolicy::Unavailable,
        }
    });
}

/// How often a still-running scope's counted egress is written to its row.
///
/// Drained on a timer *and* once more when the scope ends, and both halves are
/// load-bearing. Without the timer a long inference stream shows zero bytes for
/// its entire life and then jumps at the end, and a run that is killed — which is
/// the whole point of `agent_processes`' signal latch — would take every byte it
/// moved with it. Without the final drain the bytes since the last tick are lost
/// for exactly the runs that are shortest. Five seconds because the write is one
/// `UPDATE` on a row this process already has open, so the cost is negligible
/// beside a stream that runs for minutes, and the ledger stays close enough to
/// live for the Processes view to be worth watching.
const EGRESS_DRAIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Runs `future` under `scope`, with egress counted against the run's process row.
///
/// # What `agent_processes.bytes_egressed` means
///
/// **Every HTTP entity body byte the scoped work moved, in either direction,
/// loopback included.** This is the one place that decision is made, so it is
/// written down here rather than at the ~60 call sites that feed it.
///
/// Counting a request to this machine's own `llama-server`, Ollama or `sd-server`
/// is deliberate and it is worth saying why, because the field's name argues the
/// other way. The column sits beside `cpu_time_ms` and `peak_memory_bytes` on a
/// row that answers "what did this process consume": a 4 GB model pulled over a
/// loopback socket is real consumption, and a number that silently omitted it
/// would make the biggest transfers in the app the invisible ones. The *privacy*
/// question — did anything leave this machine — is a different question, and it is
/// already answered elsewhere and better: by the egress guards, by
/// [`crate::egress::is_loopback_target`], and by `denial_sink`'s record of what was
/// refused. Splitting this column in two would need a second column to be honest
/// about it (an implicit split is worse than either), and nothing yet asks for one.
///
/// # Attribution, and what happens when there is none
///
/// A run's bytes belong to its process row, so the row is resolved here — once, up
/// front — and the counter travels with it (see [`crate::run_scope::ProcessScope`]).
/// A scope with no run, or a run with no process row, enters without one: those
/// bytes land in [`crate::egress::unattributed_egress_bytes`] under the reason they
/// could not be attributed rather than being charged to a nearby row.
pub(crate) async fn scoped_with_egress<F, R>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    scope: crate::run_scope::RunScope,
    future: F,
) -> F::Output
where
    F: std::future::Future,
    R: tauri::Runtime,
{
    scoped_with_egress_every(app, state, EGRESS_DRAIN_INTERVAL, scope, future).await
}

/// [`scoped_with_egress`] with the cadence injected, so a test can watch a drain
/// happen mid-stream instead of waiting out the production five seconds. Same
/// reason `egress::hardened_with_timeouts` exists beside `hardened`.
async fn scoped_with_egress_every<F, R>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    interval: std::time::Duration,
    scope: crate::run_scope::RunScope,
    future: F,
) -> F::Output
where
    F: std::future::Future,
    R: tauri::Runtime,
{
    let Some(process) = scope
        .run_id()
        .and_then(|run_id| process_scope_for_run(app, state, run_id))
    else {
        return crate::run_scope::scoped(scope, future).await;
    };

    let scoped = crate::run_scope::scoped_with_process(scope, process.clone(), future);
    tokio::pin!(scoped);
    // First tick a full interval out, not immediately: `interval`'s first tick
    // completes at once, which would only ever drain an empty counter.
    let mut drains = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    loop {
        tokio::select! {
            output = &mut scoped => {
                drain_egress(app, state, &process);
                return output;
            }
            _ = drains.tick() => drain_egress(app, state, &process),
        }
    }
}

/// The process row `run_id`'s egress belongs to, when exactly one row claims it.
///
/// `None` for none and `None` for several, and the second case is the one worth
/// stating: a run with two live rows gives no way to say which of them made a
/// request, and guessing would put one process's bytes on another's row —
/// precisely the failure `run_scope`'s task-local exists to prevent. The honest
/// record for that is the unattributed tally, which is what returning `None`
/// selects.
fn process_scope_for_run<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    run_id: &str,
) -> Option<crate::run_scope::ProcessScope> {
    let rows = crate::process_commands::with_process_table(app, state, |table| {
        table.usage_rows(&crate::process_table::ProcessUsageFilter {
            run_id: Some(run_id.to_string()),
            ..crate::process_table::ProcessUsageFilter::default()
        })
    })
    .ok()?;
    match rows.as_slice() {
        [row] => Some(crate::run_scope::ProcessScope::new(row.process_id.clone())),
        _ => None,
    }
}

/// Moves everything counted so far onto the row, additively.
///
/// Fail-soft like every other bookkeeping call at this boundary — a stream must not
/// die because its ledger row could not be updated — but the bytes are handed back
/// to the counter if the write fails, so a transient error delays them to the next
/// drain instead of destroying them.
///
/// Runs in the scoped task itself, so it can only interleave at one of `future`'s
/// await points. That means `future` must not hold [`with_ledger`]'s guard across
/// an await — a `std::sync::Mutex` is not reentrant, so the drain would deadlock
/// against it rather than block. No caller does, and none should.
fn drain_egress<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    process: &crate::run_scope::ProcessScope,
) {
    let bytes = process.take_egress();
    if bytes == 0 {
        return;
    }
    let Ok(now) = unix_time_ms() else {
        process.charge_egress(bytes);
        return;
    };
    if let Err(error) = crate::process_commands::with_process_table(app, state, |table| {
        table.add_egress_bytes(process.process_id(), bytes, now as i64)
    }) {
        process.charge_egress(bytes);
        eprintln!(
            "run egress: could not record {bytes} bytes for {}: {error}",
            process.process_id()
        );
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
    include_archived: Option<bool>,
) -> Result<Vec<RunRecord>, String> {
    with_ledger(&app, state.inner(), |ledger| {
        ledger.list_runs(limit, include_archived.unwrap_or(false))
    })
    .map(|runs| runs.into_iter().map(Into::into).collect())
}

#[tauri::command]
pub fn run_archive(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    run_id: String,
) -> Result<RunRecord, String> {
    let archived_at_ms = unix_time_ms()?;
    let run = with_ledger(&app, state.inner(), |ledger| {
        ledger.archive_run(&run_id, archived_at_ms)
    })?;
    let _ = app.emit(
        RUNS_CHANGED_EVENT,
        RunChangedPayload {
            run_id: run.spec.run_id.clone(),
            status: run.status,
            last_sequence: run.last_sequence,
        },
    );
    Ok(run.into())
}

#[tauri::command]
pub fn run_unarchive(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    run_id: String,
) -> Result<RunRecord, String> {
    let run = with_ledger(&app, state.inner(), |ledger| ledger.unarchive_run(&run_id))?;
    let _ = app.emit(
        RUNS_CHANGED_EVENT,
        RunChangedPayload {
            run_id: run.spec.run_id.clone(),
            status: run.status,
            last_sequence: run.last_sequence,
        },
    );
    Ok(run.into())
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

    /// Pins the isolation `open_ledger` exists for. Without it every test in
    /// this binary opens the machine-global app-data database — the same file
    /// every other checkout and branch on the machine opens — and one branch
    /// with a newer migration turns every permission-recording test red here
    /// while CI's clean runners stay green.
    #[test]
    fn the_ledger_a_test_opens_is_never_the_real_app_data_database() {
        let app = tauri::test::mock_app();
        let handle = app.handle().clone();
        let real_app_data = handle
            .path()
            .app_data_dir()
            .expect("mock app resolves an app data dir");

        let dir = test_ledger_dir();
        assert!(
            dir.starts_with(std::env::temp_dir()),
            "test ledger must live under the temp dir, got {dir:?}"
        );
        assert_ne!(
            dir, real_app_data,
            "test ledger must not be the real app data dir"
        );

        let state = AppState::default();
        with_ledger(&handle, &state, |_| Ok(())).expect("opening the test ledger");
        assert!(
            dir.join(DATABASE_FILE).exists(),
            "the ledger was opened somewhere other than the isolated dir"
        );
    }

    #[test]
    fn host_event_ids_are_protocol_safe() {
        let id = format!("event-{}", uuid::Uuid::new_v4().simple());
        crate::run_protocol::validate_protocol_id("event_id", &id).unwrap();
    }

    /// The flag becomes binding. Before this, `permission_policy.allow_network`
    /// was read by nothing on any outbound path — only by `recipes.rs`, which
    /// compares it to a tool profile and enforces nothing, and by `sandbox.rs`'s
    /// same-named field, which is a different field on a different request and
    /// governs shell children.
    #[test]
    fn a_run_without_network_permission_cannot_reach_a_remote_provider() {
        let denial = enforce_run_network_permission(
            "https://api.openai.com/v1",
            "run-under-declared",
            false,
        )
        .expect_err("a run that declared no network must not reach a cloud provider");

        assert!(
            denial.contains(crate::egress::EgressRule::RunNetworkDenied.code()),
            "the refusal must name the rule: {denial}"
        );
        // The origin, not the endpoint as given — a custom provider endpoint can
        // carry a token in its query and this string reaches the UI.
        assert!(denial.contains("https://api.openai.com:443"));
    }

    /// The counter-test that stops this from being a blanket kill switch, and the
    /// reason the loopback exemption exists at all: a local-inference run is
    /// submitted with `allow_network: false` quite correctly, because it uses no
    /// network in the sense the flag means. Reading the flag as "no sockets" would
    /// refuse every local run — not a stricter policy, a broken one.
    #[test]
    fn a_run_without_network_permission_still_reaches_this_machine() {
        for endpoint in [
            "http://127.0.0.1:8090/v1",
            "http://localhost:11434/v1",
            "http://[::1]:8090/v1",
        ] {
            enforce_run_network_permission(endpoint, "run-local", false)
                .unwrap_or_else(|error| panic!("{endpoint} must stay reachable: {error}"));
        }
    }

    /// And the permission still permits what it says it permits, so "deny
    /// everything" cannot pass the test above.
    #[test]
    fn a_run_with_network_permission_is_unaffected() {
        enforce_run_network_permission("https://api.anthropic.com/v1", "run-declared", true)
            .expect("a run that declared network may use it");
    }

    /// A frozen endpoint that will not parse is refused rather than allowed. It
    /// came out of a spec row, so a malformed one means the row is not what this
    /// build expects, and guessing permissively is the wrong way to be wrong.
    #[test]
    fn an_unparseable_frozen_endpoint_is_refused_not_waved_through() {
        let denial = enforce_run_network_permission("not a url", "run-corrupt", false)
            .expect_err("a malformed frozen endpoint must not be treated as local");
        assert!(denial.contains(crate::egress::EgressRule::UrlMalformed.code()));
    }

    /// The seam between a frozen run row and the choke point: what the installed
    /// policy source answers for a real ledger, and what `egress` then does with it.
    ///
    /// Worth its own module because it is the only place the two halves meet. The
    /// `egress` tests install a hand-written source, so they prove the *rules*; this
    /// proves the source reads the frozen field and maps each ledger outcome to the
    /// direction it is supposed to fail in.
    mod allowlist_source {
        use super::*;
        use crate::run_ledger::RunLedger;
        use crate::run_protocol::{
            CapabilityAssessment, CapabilityState, EgressAllowlist, ModelCapabilitiesSnapshot,
            ModelTargetSnapshot, PermissionMode, PermissionPolicySnapshot,
            RUN_PROTOCOL_SCHEMA_VERSION, RunBudgets, RunKind, RunSpec, ToolPolicyDecision,
        };
        use crate::run_scope::{self, RunScope};

        fn capability(state: CapabilityState) -> CapabilityAssessment {
            CapabilityAssessment {
                state,
                evidence: "fixture".to_string(),
            }
        }

        /// The smallest spec this ledger accepts, with `egress_allowlist` as the only
        /// thing the test varies.
        fn spec(run_id: &str, allowlist: Option<EgressAllowlist>) -> RunSpec {
            let unknown = || capability(CapabilityState::Unknown);
            RunSpec {
                schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
                run_id: run_id.to_string(),
                idempotency_key: run_id.to_string(),
                created_at_ms: 1_784_000_000_000,
                kind: RunKind::Interactive,
                submitted_by: ClientIdentity {
                    client_id: "test".to_string(),
                    instance_id: "window-01".to_string(),
                    kind: ClientKind::Desktop,
                    version: "1.0.0-test".to_string(),
                },
                task: "fixture".to_string(),
                instructions: None,
                input_artifact_ids: Vec::new(),
                target: ModelTargetSnapshot::Provider {
                    target_id: "provider-main-model".to_string(),
                    label: "Provider model".to_string(),
                    provider_id: "provider-main".to_string(),
                    endpoint: "https://api.example.com/v1".to_string(),
                    model: "example-model".to_string(),
                    credential_ref_id: "provider-key-main".to_string(),
                    capabilities: ModelCapabilitiesSnapshot {
                        tool_calling: unknown(),
                        vision: unknown(),
                        embeddings: unknown(),
                        structured_output: unknown(),
                        image_generation: unknown(),
                        audio: unknown(),
                        runtime_lifecycle: unknown(),
                        fim: capability(CapabilityState::Unsupported),
                        code_completion: unknown(),
                        inline_edit: unknown(),
                        fim_metadata: None,
                    },
                },
                workspace: None,
                permission_policy: PermissionPolicySnapshot {
                    mode: PermissionMode::Auto,
                    unattended: true,
                    approval_timeout_ms: 60_000,
                    default_tool_decision: ToolPolicyDecision::Allow,
                    tool_rules: Vec::new(),
                    allow_network: true,
                    allow_external_mutations: false,
                    egress_allowlist: allowlist,
                },
                budgets: RunBudgets {
                    wall_time_ms: 60_000,
                    max_iterations: 10,
                    max_model_calls: 10,
                    max_tool_calls: 20,
                    max_input_tokens: 100_000,
                    max_output_tokens: 10_000,
                    max_cost_micros: None,
                    max_artifact_bytes: 10_000_000,
                    max_event_count: 10_000,
                },
            }
        }

        fn refusal(run_id: &str, url: &str) -> Option<crate::egress::EgressRule> {
            let url = url::Url::parse(url).expect("parses");
            run_scope::scoped_sync(RunScope::run(run_id), || {
                crate::egress::check_run_allowlist(&url)
                    .err()
                    .map(|denial| denial.rule())
            })
        }

        /// One ledger, four run states, one installed source.
        ///
        /// Written as one test rather than four because they share an installed
        /// process-wide source, and four tests would be four races over it.
        #[test]
        fn what_the_ledger_says_is_what_the_choke_point_enforces() {
            let _guard = crate::denial_sink::test_lock();
            let state = AppState::default();
            *state.run_ledger.lock().unwrap() =
                Some(RunLedger::open_in_memory().expect("an in-memory ledger opens"));
            let app = tauri::test::mock_app().handle().clone();
            // Managed, not held on the side, because the installed source resolves the
            // state through the handle exactly as it does in production.
            app.manage(state);
            let state = app.state::<AppState>();

            with_ledger(&app, state.inner(), |ledger| {
                ledger.submit_run(&spec(
                    "run:declared",
                    Some(EgressAllowlist {
                        hosts: vec!["api.example.com".to_string()],
                        ports: vec![443],
                        protocols: vec!["https".to_string()],
                    }),
                ))?;
                ledger.submit_run(&spec("run:silent", None))?;
                // A row this build cannot parse. Seeded directly, because a spec that
                // will not deserialize cannot be submitted through the front door.
                ledger
                    .connection()
                    .execute(
                        "INSERT INTO runs (run_id, idempotency_key, spec_json, created_at_ms,
                                           updated_at_ms, status, last_sequence, max_event_count)
                         VALUES ('run:corrupt', 'run:corrupt', x'7b7d', 1000, 1000, 'running', 0, 1000)",
                        [],
                    )
                    .expect("a corrupt row is seeded");
                Ok(())
            })
            .expect("the ledger opens");

            // The production installer, not a stand-in for it.
            install_run_egress_policy_source(&app);

            assert_eq!(
                refusal("run:declared", "https://api.example.com/v1"),
                None,
                "the frozen declaration must permit what it names"
            );
            assert_eq!(
                refusal("run:declared", "https://other.example.com/v1"),
                Some(crate::egress::EgressRule::RunHostNotAllowlisted),
                "a host the frozen spec did not name must be refused"
            );
            assert_eq!(
                refusal("run:silent", "https://other.example.com/v1"),
                None,
                "a run that declares nothing keeps today's behaviour"
            );
            assert_eq!(
                refusal("run:absent-from-the-ledger", "https://other.example.com/v1"),
                None,
                "a scope id that is not a ledger run is permitted, not refused"
            );
            assert_eq!(
                refusal("run:corrupt", "https://other.example.com/v1"),
                Some(crate::egress::EgressRule::RunPolicyUnavailable),
                "a row this build cannot read must fail closed"
            );

            crate::egress::clear_run_policy_source();
        }
    }

    /// The egress accounting seam, end to end and against a real socket.
    mod egress {
        use super::*;
        use crate::process_table::{
            AdmitProcess, ProcessKind, ProcessUsageFilter, ProcessUsageRow,
        };
        use crate::run_ledger::RunLedger;
        use crate::run_scope::RunScope;
        use futures_util::StreamExt;
        use std::io::{Read, Write};
        use std::time::Duration;

        /// A loopback peer that writes its head at once and then one body byte per
        /// `gap`. Trickling on purpose: a drain that lands bytes on the row while
        /// this peer is still writing is a drain that saw a *frame*, which only a
        /// non-buffering body can deliver.
        fn trickling_peer(chunks: usize, gap: Duration) -> String {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
            let origin = format!("http://{}", listener.local_addr().expect("has an address"));
            std::thread::spawn(move || {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut head = [0u8; 2048];
                let _ = stream.read(&mut head);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                     Content-Length: {chunks}\r\nConnection: close\r\n\r\n"
                );
                if stream.write_all(header.as_bytes()).is_ok() {
                    for _ in 0..chunks {
                        std::thread::sleep(gap);
                        if stream.write_all(b"x").is_err() || stream.flush().is_err() {
                            break;
                        }
                    }
                }
            });
            origin
        }

        /// A mock app whose ledger is in memory, so nothing here resolves a real
        /// app-data directory on disk.
        fn ledgered_app() -> (tauri::AppHandle<tauri::test::MockRuntime>, AppState) {
            let state = AppState::default();
            *state.run_ledger.lock().unwrap() =
                Some(RunLedger::open_in_memory().expect("an in-memory ledger opens"));
            (tauri::test::mock_app().handle().clone(), state)
        }

        /// A `runs` row and a process row that claims it.
        ///
        /// The bare SQL insert follows `process_table`'s own fixture: what this
        /// needs is only that `agent_processes.run_id`'s foreign key resolves, and
        /// a full `RunSpec` would be forty lines of irrelevant detail.
        fn admit(
            app: &tauri::AppHandle<tauri::test::MockRuntime>,
            state: &AppState,
            external_id: &str,
            run_id: &str,
        ) -> String {
            with_ledger(app, state, |ledger| {
                ledger
                    .connection()
                    .execute(
                        "INSERT INTO runs (run_id, idempotency_key, spec_json, created_at_ms,
                                           updated_at_ms, status, last_sequence, max_event_count)
                         VALUES (?1, ?1, x'7b7d', 1000, 1000, 'running', 0, 1000)",
                        rusqlite::params![run_id],
                    )
                    .expect("a run row is seeded");
                Ok(())
            })
            .expect("the ledger opens");
            crate::process_commands::with_process_table(app, state, |table| {
                table.admit(
                    &AdmitProcess::new(ProcessKind::CrewMember, external_id.to_string())
                        .with_run(run_id.to_string()),
                    1_000,
                )
            })
            .expect("a row is admitted")
            .process_id
        }

        fn stored_egress(
            app: &tauri::AppHandle<tauri::test::MockRuntime>,
            state: &AppState,
            process_id: &str,
        ) -> Option<u64> {
            crate::process_commands::with_process_table(app, state, |table| {
                table.usage_rows(&ProcessUsageFilter {
                    process_id: Some(process_id.to_string()),
                    ..ProcessUsageFilter::default()
                })
            })
            .expect("the ledger row reads")
            .pop()
            .map(|row: ProcessUsageRow| row.usage.measured().bytes_egressed)
            .expect("the row exists")
        }

        /// The whole point of the exercise: a real run's bytes reach
        /// `agent_processes.bytes_egressed`, and they get there **while the body is
        /// still arriving** rather than only when the run ends.
        ///
        /// Three claims in one test because they share a fixture and each is
        /// worthless alone. The mid-stream read proves the timer drain exists (a
        /// teardown-only drain reads `None` there) *and* that the counting body is
        /// still a passthrough (a buffering one hands over nothing until the last
        /// byte, so there would be nothing to drain). The final read proves the
        /// teardown drain does not lose the tail, and that the two drains add
        /// rather than overwrite — which is what `add_egress_bytes` is for.
        #[tokio::test]
        async fn a_runs_bytes_reach_its_process_row_while_the_stream_is_still_running() {
            let (app, state) = ledgered_app();
            let process_id = admit(&app, &state, "crew-egress", "run:egress");
            let chunks = 8usize;
            let gap = Duration::from_millis(60);
            let origin = trickling_peer(chunks, gap);
            let client = crate::egress::hardened().build().expect("client builds");

            let mid = scoped_with_egress_every(
                &app,
                &state,
                Duration::from_millis(20),
                RunScope::run("run:egress"),
                async {
                    let response = crate::egress::send(client.get(&origin))
                        .await
                        .expect("the peer answers");
                    let mut stream = response.bytes_stream();
                    for _ in 0..2 {
                        stream
                            .next()
                            .await
                            .expect("a frame arrives")
                            .expect("the frame is not an error");
                    }
                    // One drain interval, well inside the peer's remaining gaps.
                    tokio::time::sleep(gap).await;
                    let mid = stored_egress(&app, &state, &process_id);
                    while let Some(frame) = stream.next().await {
                        frame.expect("the rest of the body arrives");
                    }
                    mid
                },
            )
            .await;

            let mid = mid.expect(
                "no bytes had reached the row while the body was still trickling: either \
                 the scheduled drain is gone, or the body is being buffered before the \
                 caller sees a frame",
            );
            assert!(
                mid < chunks as u64,
                "the row already held the whole body ({mid} bytes) at the halfway read, \
                 so this test is not measuring a mid-stream drain"
            );
            assert_eq!(
                stored_egress(&app, &state, &process_id),
                Some(chunks as u64),
                "the teardown drain must add the tail rather than replace what the \
                 scheduled drains already wrote"
            );
        }

        /// A run whose process row nobody resolved is *unattributed*, not somebody
        /// else's. The bystander row keeps its `NULL` — a measured zero would be a
        /// claim nobody made — and the bytes are still counted, under why they had
        /// no row.
        #[tokio::test]
        async fn a_run_with_no_row_of_its_own_charges_no_other_row() {
            fn tally() -> u64 {
                crate::egress::unattributed_egress_bytes()
                    .into_iter()
                    .find(|(label, _)| *label == "egress.run-without-process")
                    .map(|(_, bytes)| bytes)
                    .expect("the tally exists")
            }

            let (app, state) = ledgered_app();
            let bystander = admit(&app, &state, "crew-bystander", "run:bystander");
            let origin = trickling_peer(4, Duration::from_millis(1));
            let client = crate::egress::hardened().build().expect("client builds");

            let before = tally();
            scoped_with_egress_every(
                &app,
                &state,
                Duration::from_millis(20),
                RunScope::run("run:has-no-row"),
                async {
                    crate::egress::send(client.get(&origin))
                        .await
                        .expect("the peer answers")
                        .text()
                        .await
                        .expect("body reads");
                },
            )
            .await;

            assert_eq!(
                stored_egress(&app, &state, &bystander),
                None,
                "another run's bytes must never land on this row"
            );
            // `>=` and not `==`: the tally is process-wide and other tests in this
            // binary share it.
            assert!(
                tally() >= before + 4,
                "the bytes must still be counted somewhere, not dropped"
            );
        }
    }
}
