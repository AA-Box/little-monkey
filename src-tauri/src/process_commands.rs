//! IPC surface over [`crate::process_table`].
//!
//! Thin by design, matching `run_commands.rs`: every rule lives in the core
//! module so the CLI (`monkey processes`) and the daemon reach the same
//! implementation without going through Tauri.
//!
//! `process_signal` records durable intent rather than delivering it, and
//! `process_signal_support` exposes which kinds honour which signal so a caller
//! can disable a control *with its reason* instead of offering a button that
//! silently does nothing. Delivery belongs to the owning kind, which reads the
//! latch at its own safe point: the daemon once per tick, the desktop through
//! `processSignalDelivery.ts` — the `processes://changed` event as the fast path
//! and `process_pending_signals` as the catch-up read for intent written by
//! another OS process.

use tauri::Emitter;

use crate::process_table::{
    AdmitProcess, ProcessExit, ProcessFilter, ProcessKind, ProcessRecord, ProcessState,
    ProcessTable, ProcessTableError,
};
use crate::AppState;

/// Emitted after any change, so every window's listing refreshes instead of
/// each one polling. Same convention as `RUNS_CHANGED_EVENT`.
pub const PROCESSES_CHANGED_EVENT: &str = "processes://changed";

fn to_message(error: ProcessTableError) -> String {
    error.to_string()
}

/// Runs `operation` against the process table on the shared ledger connection.
pub(crate) fn with_process_table<R: tauri::Runtime, T>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    operation: impl FnOnce(&ProcessTable<'_>) -> Result<T, ProcessTableError>,
) -> Result<T, String> {
    crate::run_commands::with_ledger(app, state, |ledger| {
        // The closure's error type is the ledger's, so map into it and back out
        // rather than widening `LedgerError` with a variant only this module
        // would ever construct.
        match operation(&ProcessTable::new(ledger.connection())) {
            Ok(value) => Ok(Ok(value)),
            Err(error) => Ok(Err(error)),
        }
    })?
    .map_err(to_message)
}

fn notify<R: tauri::Runtime>(app: &tauri::AppHandle<R>, record: &ProcessRecord) {
    let _ = app.emit(PROCESSES_CHANGED_EVENT, record);
}

/// Filter arguments as the frontend sends them.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessListArgs {
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
    #[serde(default)]
    pub live_only: Option<bool>,
    #[serde(default)]
    pub parent_process_id: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

impl ProcessListArgs {
    fn into_filter(self) -> Result<ProcessFilter, String> {
        let mut kinds = Vec::new();
        for raw in self.kinds.unwrap_or_default() {
            kinds.push(ProcessKind::parse(&raw).map_err(to_message)?);
        }
        Ok(ProcessFilter {
            kinds,
            live_only: self.live_only.unwrap_or(false),
            parent_process_id: self.parent_process_id,
            workspace: self.workspace,
            limit: self.limit,
        })
    }
}

/// Every process, newest first, always bounded.
///
/// This is the cross-surface listing that did not exist: the running-tasks pill
/// counted background shells and subagents, the Background Tasks panel the same
/// two, the Run Center only ledger runs (so never `m4` workflow runs, which
/// live in a JSON file store), and the Agent Inbox four sources whose own
/// header comment claimed workflow coverage it did not have.
#[tauri::command]
pub fn process_list(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    args: Option<ProcessListArgs>,
) -> Result<Vec<ProcessRecord>, String> {
    let filter = args.unwrap_or_default().into_filter()?;
    with_process_table(&app, state.inner(), |table| table.list(&filter))
}

#[tauri::command]
pub fn process_get(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    process_id: String,
) -> Result<Option<ProcessRecord>, String> {
    with_process_table(&app, state.inner(), |table| table.get(&process_id))
}

/// Every descendant of a process, so a turn's subagents and their background
/// shells are reachable as one subtree.
#[tauri::command]
pub fn process_descendants(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    process_id: String,
) -> Result<Vec<ProcessRecord>, String> {
    with_process_table(&app, state.inner(), |table| table.descendants(&process_id))
}

/// Live count per kind.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessLiveCount {
    pub kind: ProcessKind,
    pub count: u32,
}

#[tauri::command]
pub fn process_live_counts(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<ProcessLiveCount>, String> {
    let counts = with_process_table(&app, state.inner(), |table| table.live_counts())?;
    Ok(counts
        .into_iter()
        .map(|(kind, count)| ProcessLiveCount { kind, count })
        .collect())
}

/// Arguments for [`process_admit`].
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessAdmitArgs {
    pub kind: String,
    pub external_id: String,
    #[serde(default)]
    pub parent_process_id: Option<String>,
    /// The parent's *surface* identifier, when the caller knows that but not the
    /// parent's process id — a subagent knows the turn id it was spawned from,
    /// not the turn's process record. Resolved here; ignored when
    /// `parent_process_id` is supplied.
    #[serde(default)]
    pub parent_external_id: Option<String>,
    #[serde(default)]
    pub parent_kind: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub max_wall_ms: Option<u64>,
    #[serde(default)]
    pub max_memory_bytes: Option<u64>,
    #[serde(default)]
    pub max_output_bytes: Option<u64>,
    #[serde(default)]
    pub max_child_processes: Option<u32>,
}

/// Admit a process. Called by the frontend surfaces — a chat turn, a subagent,
/// a crew member — whose execution lives in TypeScript.
#[tauri::command]
pub fn process_admit(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    args: ProcessAdmitArgs,
) -> Result<ProcessRecord, String> {
    let kind = ProcessKind::parse(&args.kind).map_err(to_message)?;
    let now = crate::run_commands::unix_time_ms()? as i64;

    // Resolve a parent named by its surface id. A parent that cannot be found is
    // left unset rather than failing the admit: losing the lineage edge is worth
    // less than refusing to record the process at all, and the alternative would
    // make a subagent's admission depend on its parent's record having landed
    // first.
    let parent_process_id = match (args.parent_process_id, args.parent_external_id) {
        (Some(explicit), _) => Some(explicit),
        (None, Some(external)) => {
            let parent_kind = match args.parent_kind.as_deref() {
                Some(raw) => ProcessKind::parse(raw).map_err(to_message)?,
                None => ProcessKind::ChatTurn,
            };
            with_process_table(&app, state.inner(), |table| {
                table.find_by_external_id(parent_kind, &external)
            })?
            .map(|record| record.process_id)
        }
        (None, None) => None,
    };

    let request = AdmitProcess {
        kind,
        external_id: args.external_id,
        parent_process_id,
        run_id: args.run_id,
        workspace: args.workspace,
        profile: args.profile,
        limits: crate::process_table::ProcessLimits {
            max_wall_ms: args.max_wall_ms,
            max_memory_bytes: args.max_memory_bytes,
            max_output_bytes: args.max_output_bytes,
            max_child_processes: args.max_child_processes,
        },
    };

    let record = with_process_table(&app, state.inner(), |table| table.admit(&request, now))?;
    notify(&app, &record);
    Ok(record)
}

/// Arguments for [`process_transition`].
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessTransitionArgs {
    pub process_id: String,
    pub state: String,
    #[serde(default)]
    pub exit_status: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub exit_signal: Option<String>,
    #[serde(default)]
    pub exit_reason: Option<String>,
}

/// Move a process to a new state. An illegal transition is an error the caller
/// sees, not a silent no-op — that is the whole point of the table.
#[tauri::command]
pub fn process_transition(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    args: ProcessTransitionArgs,
) -> Result<ProcessRecord, String> {
    let next = ProcessState::parse(&args.state).map_err(to_message)?;
    let exit = match args.exit_status.as_deref() {
        Some(raw) => Some(ProcessExit {
            status: crate::process_table::ExitStatus::parse(raw).map_err(to_message)?,
            code: args.exit_code,
            signal: args.exit_signal,
            reason: args.exit_reason,
        }),
        None => None,
    };
    let now = crate::run_commands::unix_time_ms()? as i64;
    let process_id = args.process_id;

    let record = with_process_table(&app, state.inner(), |table| {
        table.transition(&process_id, next, exit.clone(), now)
    })?;
    notify(&app, &record);
    Ok(record)
}

/// Link a ledger run to a process after the run row exists. Needed because the
/// ledger enforces foreign keys, so a process minted before its run cannot
/// carry the link at admission time.
#[tauri::command]
pub fn process_link_run(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    process_id: String,
    run_id: String,
) -> Result<(), String> {
    let now = crate::run_commands::unix_time_ms()? as i64;
    with_process_table(&app, state.inner(), |table| {
        table.link_run(&process_id, &run_id, now)
    })
}

/// Reap live processes whose worker is gone.
///
/// The caller passes the processes it can still account for; everything else
/// that is live *and of a kind this app owns* gets
/// [`crate::process_table::ExitStatus::Lost`]. A turn whose WebView died mid-run
/// previously stayed `running` in the ledger forever, because nothing swept it.
///
/// Scoped to [`ProcessKind::DESKTOP_OWNED`] by default rather than everything
/// live: the resident daemon is a separate service that outlives this app, so an
/// unscoped reap at startup would declare live daemon work lost.
#[tauri::command]
pub fn process_reap_missing(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    live_process_ids: Vec<String>,
    reason: Option<String>,
    kinds: Option<Vec<String>>,
) -> Result<Vec<ProcessRecord>, String> {
    let scope_kinds = match kinds {
        Some(raw) => {
            let mut parsed = Vec::new();
            for value in raw {
                parsed.push(ProcessKind::parse(&value).map_err(to_message)?);
            }
            parsed
        }
        None => ProcessKind::DESKTOP_OWNED.to_vec(),
    };
    let scope = ProcessFilter {
        kinds: scope_kinds,
        ..ProcessFilter::default()
    };
    let now = crate::run_commands::unix_time_ms()? as i64;
    let reason = reason.unwrap_or_else(|| "worker was no longer running at startup".to_string());
    let reaped = with_process_table(&app, state.inner(), |table| {
        table.reap_missing(&scope, &live_process_ids, &reason, now)
    })?;
    for record in &reaped {
        notify(&app, record);
    }
    Ok(reaped)
}

/// Arguments for [`process_reconcile`].
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessReconcileArgs {
    pub kind: String,
    pub external_id: String,
    pub state: String,
    #[serde(default)]
    pub parent_kind: Option<String>,
    #[serde(default)]
    pub parent_external_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub exit_status: Option<String>,
    #[serde(default)]
    pub exit_reason: Option<String>,
}

/// Idempotent projection from the frontend.
///
/// Differs from [`process_admit`] in exactly the way that matters for a caller
/// that may not be first: admitting twice is an error (so a surface cannot fork
/// its own record), while reconciling twice is a no-op. A desktop turn routed to
/// the resident runner uses this to create the daemon job's record with the turn
/// as its parent — the daemon's own per-tick reconcile then finds that record and
/// only moves its state, which is how the parent edge survives crossing the
/// process boundary.
#[tauri::command]
pub fn process_reconcile(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    args: ProcessReconcileArgs,
) -> Result<ProcessRecord, String> {
    let kind = ProcessKind::parse(&args.kind).map_err(to_message)?;
    let target = ProcessState::parse(&args.state).map_err(to_message)?;
    let exit = match args.exit_status.as_deref() {
        Some(raw) => Some(ProcessExit {
            status: crate::process_table::ExitStatus::parse(raw).map_err(to_message)?,
            code: None,
            signal: None,
            reason: args.exit_reason,
        }),
        None => None,
    };
    let parent = match (args.parent_kind.as_deref(), args.parent_external_id) {
        (Some(raw), Some(external)) => {
            Some((ProcessKind::parse(raw).map_err(to_message)?, external))
        }
        (None, Some(external)) => Some((ProcessKind::ChatTurn, external)),
        _ => None,
    };

    let mut projection =
        crate::process_table::ProcessProjection::new(kind, args.external_id, target);
    projection.parent = parent;
    projection.exit = exit;
    projection.run_id = args.run_id;
    projection.workspace = args.workspace;
    projection.profile = args.profile;

    let now = crate::run_commands::unix_time_ms()? as i64;
    let record = with_process_table(&app, state.inner(), |table| {
        table.reconcile(&projection, now).map(|(record, _)| record)
    })?;
    notify(&app, &record);
    Ok(record)
}

/// What signals a kind honours, so a UI can enable only the controls that work.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessSignalSupport {
    pub kind: ProcessKind,
    pub signal: crate::process_table::ProcessSignal,
    pub honoured: bool,
    /// Present when refused — shown to the user instead of a disabled control
    /// with no explanation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

/// The full signal support matrix.
///
/// Exposed so a control can be disabled *with its reason* rather than a UI
/// guessing, or worse, offering a button that silently does nothing.
#[tauri::command]
pub fn process_signal_support() -> Vec<ProcessSignalSupport> {
    let mut out = Vec::new();
    for kind in ProcessKind::ALL {
        for signal in crate::process_table::ProcessSignal::ALL {
            let support = kind.signal_support(*signal);
            out.push(ProcessSignalSupport {
                kind: *kind,
                signal: *signal,
                honoured: support.is_honoured(),
                reason: support.refusal(),
            });
        }
    }
    out
}

/// Ask a process for a signal.
///
/// Records durable intent; the owning kind delivers it at its own safe point. A
/// kind that does not honour the signal returns an error carrying the reason, so
/// the caller can say why rather than appearing to succeed.
#[tauri::command]
pub fn process_signal(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    process_id: String,
    signal: String,
    reason: Option<String>,
) -> Result<ProcessRecord, String> {
    let signal = crate::process_table::ProcessSignal::parse(&signal).map_err(to_message)?;
    let now = crate::run_commands::unix_time_ms()? as i64;
    let record = with_process_table(&app, state.inner(), |table| {
        table.signal(&process_id, signal, reason.as_deref(), now)
    })?;
    // A background shell has no safe-point loop to reach the durable latch
    // itself, so deliver the real OS suspend/resume in this same call rather
    // than waiting on a poll that doesn't exist for this kind. Falls back to
    // the undelivered record on error — the latch is already written, and a
    // later reap or retry resolves it rather than this call failing outright.
    let record = if record.kind == ProcessKind::BackgroundShell
        && matches!(
            signal,
            crate::process_table::ProcessSignal::Suspend
                | crate::process_table::ProcessSignal::Resume
        ) {
        crate::background_shell::deliver_os_signal(&app, state.inner(), &record, signal)
            .unwrap_or(record)
    } else {
        record
    };
    // A chat turn's own pause is cooperative and lands at the loop's next safe
    // point, which for a long `run_shell` can be minutes away. Its foreground
    // shell children are ordinary OS processes, so stop them now — that is
    // what makes a paused turn stop consuming the machine rather than merely
    // promising to at some unbounded later moment. Best-effort by design: a
    // turn with no shell running signals nothing, which is not a failure, and
    // the turn's own cooperative park is unaffected either way.
    if record.kind == ProcessKind::ChatTurn {
        match signal {
            crate::process_table::ProcessSignal::Suspend => {
                crate::tools::signal_turn_shells(state.inner(), &record.external_id, true);
            }
            crate::process_table::ProcessSignal::Resume => {
                crate::tools::signal_turn_shells(state.inner(), &record.external_id, false);
            }
            // Stop/Kill already tear the child down through
            // `tools_cancel_running`'s notify plus `kill_on_drop`.
            _ => {}
        }
    }
    notify(&app, &record);
    Ok(record)
}

/// Bring a background shell's real OS state in line with its durable latch.
///
/// [`process_signal`] delivers the SIGSTOP/SIGCONT in the same call, which is
/// right for a signal raised inside this app but covers only that origin. A
/// `monkey processes signal` writes the latch from another OS process and exits;
/// nothing in Rust runs on its behalf, and a background shell — unlike a
/// workflow run, whose executor polls [`SignalSource`] at every level boundary,
/// or a chat turn, whose loop checks its own registry — has no loop of its own
/// to notice. Without this the desktop's catch-up sweep saw the latch, assumed
/// Rust had already acted, and dropped it: the row said `suspend_requested` and
/// the child kept running.
///
/// Deliberately delivery-only. It writes no intent, so the sweep that calls it
/// cannot re-trigger itself through the event the write would emit, and calling
/// it twice is a no-op. Direction comes from comparing the latch to the state,
/// which is the same "state is the acknowledgement" rule the sweep's own
/// predicate uses: a set latch on a row that is not yet `suspended` means
/// suspend, a cleared latch on a row that still is means resume, and anything
/// else means there is nothing to deliver.
#[tauri::command]
pub fn process_deliver_os_signal(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    process_id: String,
) -> Result<Option<ProcessRecord>, String> {
    let record = with_process_table(&app, state.inner(), |table| table.get(&process_id))?;
    let Some(record) = record else {
        return Ok(None);
    };
    if record.kind != ProcessKind::BackgroundShell {
        return Ok(None);
    }
    let Some(signal) = owed_delivery(record.signal_intent.suspend_requested, record.state) else {
        return Ok(None);
    };

    let delivered =
        crate::background_shell::deliver_os_signal(&app, state.inner(), &record, signal)
            .unwrap_or(record);
    notify(&app, &delivered);
    Ok(Some(delivered))
}

/// Which OS signal, if any, a row's real state still owes its durable latch.
///
/// Split out from the command because it is the whole decision and the command
/// around it needs an `AppHandle` to reach. `Exited` owes nothing in either
/// direction — a reaped child cannot be stopped or continued, and asking would
/// be signalling whatever pid the OS has since reused.
fn owed_delivery(
    suspend_requested: bool,
    state: crate::process_table::ProcessState,
) -> Option<crate::process_table::ProcessSignal> {
    use crate::process_table::{ProcessSignal, ProcessState};
    match (suspend_requested, state) {
        (_, ProcessState::Exited) => None,
        (true, ProcessState::Suspended) => None,
        (true, _) => Some(ProcessSignal::Suspend),
        (false, ProcessState::Suspended) => Some(ProcessSignal::Resume),
        (false, _) => None,
    }
}

#[cfg(test)]
mod delivery_tests {
    use super::owed_delivery;
    use crate::process_table::{ProcessSignal, ProcessState};

    #[test]
    fn a_background_shell_owes_only_the_signal_its_state_disagrees_with() {
        // Latched but not yet stopped: the CLI-originated case that used to be
        // dropped on the floor.
        assert_eq!(
            owed_delivery(true, ProcessState::Running),
            Some(ProcessSignal::Suspend)
        );
        assert_eq!(
            owed_delivery(true, ProcessState::Admitted),
            Some(ProcessSignal::Suspend)
        );

        // Un-latched but still stopped: the direction that stranded a child at
        // `T` with nothing left to resume it.
        assert_eq!(
            owed_delivery(false, ProcessState::Suspended),
            Some(ProcessSignal::Resume)
        );

        // Already agreeing — calling twice must not re-signal.
        assert_eq!(owed_delivery(true, ProcessState::Suspended), None);
        assert_eq!(owed_delivery(false, ProcessState::Running), None);

        // Exited owes nothing either way; the pid may belong to someone else.
        assert_eq!(owed_delivery(true, ProcessState::Exited), None);
        assert_eq!(owed_delivery(false, ProcessState::Exited), None);
    }
}

/// Every process with signal intent still waiting to be delivered.
///
/// The catch-up read behind the `processes://changed` fast path. The event only
/// reaches windows of *this* app, so a signal written by `monkey processes
/// signal` — a different OS process, holding its own SQLite connection, with no
/// way to emit a Tauri event — is invisible to a listener. This is how it lands.
///
/// `kinds` narrows to what the caller can actually deliver to, so a desktop
/// sweep does not walk the daemon's rows: the daemon reads its own intent once
/// per tick and delivering twice from two processes is how you get a stop that
/// races a resume.
///
/// The predicate and the "state is the acknowledgement" rule live in
/// [`ProcessTable::pending_signals`], not here.
#[tauri::command]
pub fn process_pending_signals(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    kinds: Option<Vec<String>>,
) -> Result<Vec<ProcessRecord>, String> {
    let mut parsed = Vec::new();
    for raw in kinds.unwrap_or_default() {
        parsed.push(ProcessKind::parse(&raw).map_err(to_message)?);
    }
    with_process_table(&app, state.inner(), |table| table.pending_signals(&parsed))
}

/// Filter arguments for [`process_usage_ledger`].
#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessUsageArgs {
    #[serde(default)]
    pub process_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub closed_only: Option<bool>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// The resource ledger for a selection of processes: the rows and their totals.
///
/// Both in one response because they are only meaningful together — a total whose
/// `unavailableRows` is nonzero is answering a narrower question than it looks
/// like, and the rows are how a caller sees which processes were left out and
/// why.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessUsageLedger {
    pub rows: Vec<crate::process_table::ProcessUsageRow>,
    pub totals: crate::process_table::ProcessUsageAggregate,
    /// Where each row's allowed egress went, keyed by `processId`. Only the
    /// processes that reached somewhere appear — see
    /// `ProcessTable::egress_destinations_for`.
    ///
    /// Beside the rows rather than inside them because it is the one field here
    /// that is a list per row: folding it in would make every other caller of
    /// `usage_rows` — the daemon engine, the background shell — pay for a query
    /// only this surface reads.
    pub destinations:
        std::collections::BTreeMap<String, crate::process_table::ProcessEgressDestinations>,
}

/// What each process actually consumed, per process and in aggregate.
///
/// The read side of K6(b). Every field is either a measurement or `null` with a
/// reason in `usage.unavailable` — nothing here is inferred, and in particular
/// nothing unmeasured is reported as zero, so a surface showing these numbers can
/// say "not measured, because …" instead of implying a process cost nothing.
#[tauri::command]
pub fn process_usage_ledger(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    args: Option<ProcessUsageArgs>,
) -> Result<ProcessUsageLedger, String> {
    let args = args.unwrap_or_default();
    let filter = crate::process_table::ProcessUsageFilter {
        process_id: args.process_id,
        run_id: args.run_id,
        workspace: args.workspace,
        closed_only: args.closed_only.unwrap_or(false),
        limit: args.limit,
    };
    with_process_table(&app, state.inner(), |table| {
        let rows = table.usage_rows(&filter)?;
        let ids: Vec<String> = rows.iter().map(|row| row.process_id.clone()).collect();
        Ok(ProcessUsageLedger {
            destinations: table.egress_destinations_for(&ids)?,
            totals: table.usage_totals(&filter)?,
            rows,
        })
    })
}

/// Applies a projection through the shared reconcile, for native adopters that
/// already hold an `AppHandle` and `AppState`.
///
/// The Tauri-side counterpart to `LedgerProcessProjector`: that one owns a path
/// and its own connection, which is right for a service with no app handle;
/// this one reuses the pooled ledger the rest of the app is already using.
pub(crate) fn project_process_record<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    projection: &crate::process_table::ProcessProjection,
) -> Result<(), String> {
    let now = crate::run_commands::unix_time_ms()? as i64;
    with_process_table(app, state, |table| {
        table.reconcile(projection, now).map(|_| ())
    })
}

/// Reaps desktop-owned processes at app startup.
///
/// Called from `lib.rs`'s `setup` rather than from the frontend so it runs once
/// per app launch regardless of how many windows open, and before any new turn
/// can admit a process that would then be in the live set. Failure is logged and
/// swallowed — a stale row is not worth refusing to start over.
pub(crate) fn reap_desktop_processes_at_startup<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
) {
    let now = match crate::run_commands::unix_time_ms() {
        Ok(value) => value as i64,
        Err(error) => {
            eprintln!("process table: startup reap skipped, clock unavailable: {error}");
            return;
        }
    };
    let scope = ProcessFilter {
        kinds: ProcessKind::DESKTOP_OWNED.to_vec(),
        ..ProcessFilter::default()
    };
    // Nothing this app instance owns can still be running: its workers died with
    // the previous process, so the accounted-for set is empty by definition.
    let result = with_process_table(app, state, |table| {
        table.reap_missing(
            &scope,
            &[],
            "the app restarted while this process was still running",
            now,
        )
    });
    match result {
        Ok(reaped) if !reaped.is_empty() => {
            eprintln!(
                "process table: reaped {} process(es) left running by a previous app session",
                reaped.len()
            );
        }
        Ok(_) => {}
        Err(error) => eprintln!("process table: startup reap failed: {error}"),
    }

    // Runs are not desktop-owned — the daemon hosts them too — so the pass above
    // deliberately skips them and they were the last kinds with no crash
    // coverage at all. They are swept by host liveness instead, which also
    // cleans up after a daemon that crashed and never restarted.
    let hosted = with_process_table(app, state, |table| {
        crate::process_table::reap_processes_whose_host_died(table, now)
    });
    match hosted {
        Ok(reaped) if !reaped.is_empty() => {
            eprintln!(
                "process table: reaped {} workflow process(es) whose host is gone",
                reaped.len()
            );
        }
        Ok(_) => {}
        Err(error) => eprintln!("process table: host-liveness reap failed: {error}"),
    }
}
