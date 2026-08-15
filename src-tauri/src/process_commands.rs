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
    #[serde(default)]
    pub max_context_tokens: Option<u64>,
    /// Drop this kind's class wall budget for this process, rather than replacing
    /// it with a number.
    ///
    /// A flag and not a `maxWallMs: 0`, deliberately. The ledger's own `CHECK`
    /// forbids a non-positive `max_wall_ms`, so zero cannot be stored — and
    /// reading a zero as "unbounded" would be exactly the zero-versus-absent
    /// overloading this codebase avoids elsewhere. `None`/`false` means "use the
    /// class default", which is what every existing caller already sends.
    #[serde(default)]
    pub unbounded_wall: Option<bool>,
}

/// The limit set an admission ends up with: the kind's class defaults, with each
/// stated argument overriding its own field.
///
/// **Merged, not substituted, and that was the bug.** `process_admit` built this
/// from the arguments alone, so every row admitted over IPC was declared
/// unbounded regardless of what its kind said it was subject to. That is why
/// `ProcessKind::default_limits` could be given a wall budget and still fire for
/// nobody: the four kinds it applies to are admitted exactly here, and this
/// overwrote it with `None` on the way past. `AdmitProcess::new` has always
/// seeded from the class for native callers; this is the same rule for the IPC
/// path.
///
/// Split out of the command so it can be asserted without an `AppHandle`, the
/// same reason `verify.rs` tests `run_command_impl` rather than its wrapper.
/// Drop a caller's value for a field this kind's owner will not read.
///
/// K4's contract: a positive value may be recorded only when its owner enforces
/// it or that limit is reported unavailable. `merged_limits` used to `or` the
/// caller's value over the class default for every field, so an IPC caller could
/// put a memory ceiling on a chat turn or a child-process ceiling on anything,
/// and the row would carry a bound nothing consults. The class default is kept
/// either way — it is written by the same code that enforces it — but a caller
/// value survives only where [`ProcessKind::limit_support`] says the owner reads
/// this row's field.
fn caller_value<T>(
    kind: ProcessKind,
    limit: crate::process_table::ProcessLimitKind,
    stated: Option<T>,
) -> Result<Option<T>, String> {
    if stated.is_none() || kind.limit_support(limit).honours_caller_value() {
        return Ok(stated);
    }
    // Refused, not dropped. Silently discarding it is the failure K4 names
    // outright: the caller asked for a safety bound, got an admitted process,
    // and has every reason to believe the bound is active. A typed refusal is
    // the only answer that leaves the caller's next decision correct — retry
    // without the limit, pick a kind that can hold it, or stop.
    Err(format!(
        "{} cannot be enforced for a {} process: {}",
        limit.as_str(),
        kind.as_str(),
        kind.limit_support(limit).detail()
    ))
}

fn merged_limits(
    kind: ProcessKind,
    args: &ProcessAdmitArgs,
) -> Result<crate::process_table::ProcessLimits, String> {
    use crate::process_table::ProcessLimitKind as L;
    // Refusals first, so what reaches the merge is a caller layer this kind's
    // owner will actually read. The two halves are separate on purpose: deciding
    // *whether* a caller may state a field is this path's own question — a native
    // caller has no IPC boundary to refuse at — and deciding which number wins is
    // not. There is one implementation of the second, and it is the one the
    // controller installs from.
    let caller = crate::process_table::ProcessLimits {
        max_wall_ms: caller_value(kind, L::Wall, args.max_wall_ms)?,
        max_memory_bytes: caller_value(kind, L::Memory, args.max_memory_bytes)?,
        max_output_bytes: caller_value(kind, L::Output, args.max_output_bytes)?,
        max_child_processes: caller_value(kind, L::ChildProcesses, args.max_child_processes)?,
        max_context_tokens: caller_value(kind, L::ContextTokens, args.max_context_tokens)?,
    };
    let mut merged = crate::resource_control::EffectiveLimits::resolve(&[
        crate::resource_control::LimitLayer::new(
            crate::resource_control::LimitSource::ClassDefault,
            kind.default_limits(),
        ),
        crate::resource_control::LimitLayer::new(
            crate::resource_control::LimitSource::UserOverride,
            caller,
        ),
    ])
    .to_process_limits();
    // The one field with an explicit opt-out, because it is the one with a class
    // default a user can turn off. Applied after the merge rather than inside it:
    // `EffectiveLimits::resolve` intersects maxima and has no way to express
    // "remove this bound", and it should not gain one — a layer that could widen
    // by omission is the property that makes a guardrail a guardrail.
    //
    // `unbounded_wall` beats a stated `max_wall_ms` too: a caller that says both
    // has contradicted itself, and "no budget" is the safer of the two readings.
    // It is honoured for every kind, unlike a stated value, because turning a
    // budget *off* declares less rather than more and so cannot manufacture a
    // bound nobody enforces.
    if args.unbounded_wall.unwrap_or(false) {
        merged.max_wall_ms = None;
    }
    Ok(merged)
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
    // Computed before `args` starts being moved from, one line below.
    let limits = merged_limits(kind, &args)?;

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
        limits,
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
            breach: None,
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
            breach: None,
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
    /// Each row's measured prompt-cache reuse, keyed by `processId` (roadmap
    /// K11). Only the processes whose runtime reported a figure appear — see
    /// `ProcessTable::context_reuse_for` on why absence is the answer for the
    /// rest rather than a zero.
    ///
    /// Beside the rows for `destinations`' reason: two of the three runtimes
    /// never populate it, so every other caller of `usage_rows` would be paying
    /// for a query only this surface reads.
    pub context_reuse: std::collections::BTreeMap<String, crate::run_scope::ContextReuse>,
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
            context_reuse: table.context_reuse_for(&ids)?,
            totals: table.usage_totals(&filter)?,
            rows,
        })
    })
}

/// Which layer supplied the number on a row, as far as the row can prove it.
///
/// The resolution in [`crate::resource_control::EffectiveLimits::resolve`] knows
/// the answer exactly and records it — but only for the controller that is
/// holding the process right now, and a row outlives its controller. What the row
/// *does* carry is the effective number and the kind, and the class default is a
/// pure function of the kind, so the comparison between them is decidable
/// wherever the row is read: forever, and after a restart.
///
/// Derived in Rust rather than in the UI, for the same reason the enforcement
/// matrix is: two implementations of "who set this" is how the panel and
/// `monkey processes` come to disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitOrigin {
    /// Exactly the kind's class default: nothing tightened it.
    ClassDefault,
    /// Below the class default: a caller supplied a tighter number, which is the
    /// only direction a caller can move one.
    CallerOverride,
    /// The kind declares no class default and this row carries a number.
    CallerSupplied,
    /// The kind declares a class default and this row does not carry it — a row
    /// written before that default existed. Reported as unknown rather than
    /// backfilled: the number this process actually ran under is not recoverable.
    Unrecorded,
    /// Neither the class nor the row states one. This resource is unbounded for
    /// this process, which is a finding rather than a gap.
    Unbounded,
}

/// One resource, for one process: what was asked, what holds, and what it cost.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessLimitReport {
    /// The `ProcessLimits` field name — `max_memory_bytes` — so this row names
    /// the same thing the CLI and the breach do.
    pub limit: &'static str,
    /// The kind's declaration, before any caller had a say.
    pub class_default: Option<u64>,
    /// The number actually installed on this process.
    pub effective: Option<u64>,
    pub origin: LimitOrigin,
    /// The static, per-kind answer to "does anything read this field": the same
    /// matrix `monkey processes limits` prints.
    pub support_status: &'static str,
    pub support_detail: &'static str,
    /// What is holding it *on this host*, for the kinds a resource controller
    /// owns. `None` for a kind that owns no OS process tree, where there is no
    /// host-specific answer to give and inventing one would be the lie this whole
    /// type exists to prevent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<crate::resource_control::LimitCapability>,
    /// The measurement, where one exists. Never zero for "not measured".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed: Option<u64>,
    /// Why `observed` is absent, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_unavailable: Option<&'static str>,
}

/// Everything the Processes panel needs to answer "what is bounding this, and
/// who says so".
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessResourceReport {
    pub process_id: String,
    pub kind: ProcessKind,
    /// The controller this host would build for a native process tree, named.
    /// `None` for a kind that owns no tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tree_primitive: Option<String>,
    pub limits: Vec<ProcessLimitReport>,
    /// The limit that ended this process, with the mechanism's own numbers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breach: Option<crate::resource_control::LimitBreach>,
}

const NOT_MEASURED_PER_PROCESS: &str =
    "nothing measures this per process once it has ended; the limit is enforced while it runs";
const NOT_SAMPLED: &str = "nothing sampled this process's resource use while it ran";
const WALL_NOT_FINAL: &str = "this process has not exited, so its wall time is not final";

/// Whether this kind's OS process tree is owned by a [`ResourceController`].
///
/// The six that own one. Everything else either runs inside the WebView or
/// delegates to a child that carries its own record, and asking a controller
/// about it would produce a host answer for a process no controller holds.
///
/// [`ResourceController`]: crate::resource_control::ResourceController
fn is_controller_owned(kind: ProcessKind) -> bool {
    matches!(
        kind,
        ProcessKind::ForegroundShell
            | ProcessKind::BackgroundShell
            | ProcessKind::BrowserSession
            | ProcessKind::VerifyCommand
            | ProcessKind::HookCommand
            | ProcessKind::SandboxRun
    )
}

fn origin_of(class_default: Option<u64>, effective: Option<u64>) -> LimitOrigin {
    match (class_default, effective) {
        (None, None) => LimitOrigin::Unbounded,
        (None, Some(_)) => LimitOrigin::CallerSupplied,
        (Some(_), None) => LimitOrigin::Unrecorded,
        (Some(class), Some(effective)) if effective == class => LimitOrigin::ClassDefault,
        (Some(_), Some(_)) => LimitOrigin::CallerOverride,
    }
}

/// The resource story of one process, from the one place that knows it.
///
/// # Why this is a command and not four fields on the row
///
/// Three of the four answers a reader needs are not row data. "What does this
/// kind declare" is a pure function of the kind; "what is holding it on this
/// host" depends on the machine the app is running on right now, not on the
/// machine the row was written on; and "who supplied the winning number" is the
/// comparison between the first two. Storing them would freeze a host answer into
/// a durable record and then be wrong the first time the app moved between a
/// laptop with a delegated cgroup and one without.
///
/// What *is* stored is the effective number and the typed breach, because those
/// are facts about this process and nothing else can recover them later.
#[tauri::command]
pub fn process_resource_report(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    process_id: String,
) -> Result<ProcessResourceReport, String> {
    use crate::process_table::{ProcessLimitKind, ProcessUsageFilter};

    let (record, usage) = with_process_table(&app, state.inner(), |table| {
        let record = table.get(&process_id)?.ok_or(ProcessTableError::NotFound {
            process_id: process_id.clone(),
        })?;
        let usage = table
            .usage_rows(&ProcessUsageFilter {
                process_id: Some(process_id.clone()),
                ..ProcessUsageFilter::default()
            })?
            .pop();
        Ok((record, usage))
    })?;

    // Built once for the whole report rather than per limit: constructing a
    // controller creates the host's containment primitive, and doing that five
    // times to answer five questions about one process would be five cgroups.
    let host = is_controller_owned(record.kind).then(|| {
        crate::resource_control::ResourceController::new(crate::resource_control::probe_limits())
            .capabilities()
    });
    let class = record.kind.default_limits();
    let measured = usage.as_ref().map(|row| row.usage.measured());

    let wall_observed = match (record.started_at_ms, record.exited_at_ms) {
        (Some(started), Some(exited)) => Ok(u64::try_from(exited - started).unwrap_or(0)),
        (Some(_), None) => Err(WALL_NOT_FINAL),
        _ => Err(NOT_SAMPLED),
    };

    let limits = ProcessLimitKind::ALL
        .iter()
        .map(|limit| {
            let (class_default, effective) = match limit {
                ProcessLimitKind::Wall => (class.max_wall_ms, record.limits.max_wall_ms),
                ProcessLimitKind::Memory => {
                    (class.max_memory_bytes, record.limits.max_memory_bytes)
                }
                ProcessLimitKind::Output => {
                    (class.max_output_bytes, record.limits.max_output_bytes)
                }
                ProcessLimitKind::ChildProcesses => (
                    class.max_child_processes.map(u64::from),
                    record.limits.max_child_processes.map(u64::from),
                ),
                ProcessLimitKind::ContextTokens => {
                    (class.max_context_tokens, record.limits.max_context_tokens)
                }
            };
            let support = record.kind.limit_support(*limit);
            // Peak rather than current, and only where something actually
            // sampled: a resource ledger with an invented zero is worse than one
            // with a gap, because the zero is indistinguishable from a real
            // measurement of nothing.
            let observed = match limit {
                ProcessLimitKind::Wall => wall_observed.ok(),
                ProcessLimitKind::Memory => measured.and_then(|measured| measured.peak_rss_bytes),
                _ => None,
            };
            let observed_unavailable = match (limit, observed) {
                (_, Some(_)) => None,
                (ProcessLimitKind::Wall, None) => Some(wall_observed.unwrap_err()),
                (ProcessLimitKind::Memory, None) => Some(NOT_SAMPLED),
                _ => Some(NOT_MEASURED_PER_PROCESS),
            };
            ProcessLimitReport {
                limit: limit.as_str(),
                class_default,
                effective,
                origin: origin_of(class_default, effective),
                support_status: support.status(),
                support_detail: support.detail(),
                host: host
                    .as_ref()
                    .map(|host| host.for_limit(*limit).clone())
                    // A resource the kind does not declare and nothing enforces
                    // needs no host answer; reporting one would put a mechanism
                    // beside a limit that is not in force.
                    .filter(|_| support.honours_caller_value()),
                observed,
                observed_unavailable,
            }
        })
        .collect();

    Ok(ProcessResourceReport {
        process_id: record.process_id.clone(),
        kind: record.kind,
        backend: host.as_ref().map(|host| host.backend.clone()),
        tree_primitive: host.as_ref().map(|host| host.tree_primitive.clone()),
        limits,
        breach: record.exit.as_ref().and_then(|exit| exit.breach.clone()),
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
/// End the shell trees a previous app session left running, and prove each one
/// first.
///
/// # What survives a crash, and what does not
///
/// A shell's *bound* and a shell's *supervisor* have different lifetimes, and the
/// distinction is the whole reason this exists. Each platform answers it
/// differently, and only one of the three leaves anything here to do:
///
/// - **Linux.** A cgroup scope's `memory.max` and `pids.max` keep holding after
///   the app dies; the kernel does not care that the process which wrote them is
///   gone. The tree is still running and still bounded, and has nothing watching
///   it.
/// - **Windows.** The job carries `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, and the
///   kernel closes the handles a dead process held — so the tree died with the
///   app and there is nothing to find. Both are asserted in
///   `resource_control`'s own tests rather than assumed here.
/// - **Supervised, on every platform.** The bound died with the supervisor. The
///   tree is running under no limit at all, which is the case this most exists
///   for.
///
/// So after a restart there may be a tree that is still running, may or may not
/// still be bounded, and has nothing watching it. None of those is a state to
/// leave a machine in.
///
/// Reclaiming goes through the recorded identity, never through the pid alone:
/// see [`crate::process_table::still_the_recorded_process`] for why a row that
/// cannot prove which process it named is skipped rather than signalled.
///
/// The stale cgroup directory is not cleaned here. A scope is named with a fresh
/// uuid and lives under a delegated subtree; once its members are gone the kernel
/// leaves an empty directory, which holds nothing and is removed by the same
/// `Drop` on any session that still has the handle. Removing directories this
/// process did not create, by pattern, against a hierarchy another instance may
/// be using, is a worse trade than an empty directory.
fn reclaim_orphaned_shell_trees(table: &ProcessTable<'_>) -> Result<usize, String> {
    let live = table
        .list(&ProcessFilter {
            kinds: vec![ProcessKind::ForegroundShell, ProcessKind::BackgroundShell],
            live_only: true,
            ..ProcessFilter::default()
        })
        .map_err(to_message)?;

    let mut killed = 0;
    for record in live {
        if !crate::process_table::still_the_recorded_process(&record) {
            continue;
        }
        let Some(pid) = record.native_pid.and_then(|pid| u32::try_from(pid).ok()) else {
            continue;
        };
        // The group, because that is what a shell's pid leads and what this
        // session can still reach: the controller that recorded the out-of-group
        // members died with the previous process, and nothing durable replaces
        // it. Stated rather than glossed — a descendant that both re-parented and
        // left the group before the crash is not reclaimable by any later
        // session, which is the same macOS lifetime limit `docs/limitations.md`
        // already records for a live one.
        if crate::os_signal::terminate_process_group(pid).is_ok() {
            killed += 1;
        }
    }
    Ok(killed)
}

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
    // Killed *before* the reap, because the reap is what erases the evidence: it
    // closes every one of these rows, and the recorded pid is the only handle
    // anything has on a native tree the previous app process left running. The
    // WebView kinds need no equivalent — their worker was a loop that died with
    // the app, so there is nothing left to signal.
    match with_process_table(app, state, |table| Ok(reclaim_orphaned_shell_trees(table))) {
        Ok(Ok(killed)) if killed > 0 => {
            eprintln!("process table: reclaimed {killed} shell tree(s) left by a previous session")
        }
        Ok(Ok(_)) => {}
        Ok(Err(error)) | Err(error) => {
            eprintln!("process table: shell orphan reclaim failed: {error}")
        }
    }
    match with_process_table(app, state, |table| {
        Ok(crate::browser_worker::reclaim_orphaned_browser_sessions(
            table,
        ))
    }) {
        Ok(Ok(killed)) if killed > 0 => {
            eprintln!("browser worker: killed {killed} orphaned Chromium process group(s)")
        }
        Ok(Ok(_)) => {}
        Ok(Err(error)) | Err(error) => {
            eprintln!("browser worker: orphan reclaim failed: {error}")
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_table::WEBVIEW_WALL_BUDGET_MS;

    fn args(kind: &str) -> ProcessAdmitArgs {
        ProcessAdmitArgs {
            kind: kind.to_string(),
            external_id: "ext-1".to_string(),
            parent_process_id: None,
            parent_external_id: None,
            parent_kind: None,
            run_id: None,
            workspace: None,
            profile: None,
            max_wall_ms: None,
            max_memory_bytes: None,
            max_output_bytes: None,
            max_child_processes: None,
            max_context_tokens: None,
            unbounded_wall: None,
        }
    }

    /// The defect this slice fixed: a caller that states nothing used to get an
    /// all-`None` limit set, which is how a class default could exist and still
    /// fire for nobody.
    #[test]
    fn an_admission_that_states_nothing_still_carries_its_class_limits() {
        for kind in [
            ProcessKind::ChatTurn,
            ProcessKind::Subagent,
            ProcessKind::CrewMember,
            ProcessKind::SideTask,
        ] {
            let merged = merged_limits(kind, &args(kind.as_str())).expect("states nothing");
            assert_eq!(
                merged,
                kind.default_limits(),
                "{} lost its class limits at the IPC boundary",
                kind.as_str()
            );
            assert_eq!(merged.max_wall_ms, Some(WEBVIEW_WALL_BUDGET_MS));
        }

        // Not only the kinds this slice added a budget for: a background shell's
        // output ceiling reached the row the same way, and did not before.
        assert_eq!(
            merged_limits(
                ProcessKind::BackgroundShell,
                &args(ProcessKind::BackgroundShell.as_str())
            )
            .expect("states nothing"),
            ProcessKind::BackgroundShell.default_limits()
        );
    }

    /// K4's contract at the boundary that used to leak it: a caller may not
    /// record a bound this kind's owner will not read — and must be *told*, not
    /// quietly given a process without it.
    ///
    /// Two defects, one after the other. `process_admit` first `or`-ed every
    /// stated field over the class default, so an IPC caller could put a 512 MiB
    /// ceiling and a 4-process ceiling on a chat turn and the row would advertise
    /// containment that did not exist. That was fixed by dropping the value —
    /// which left the second defect: the admission still succeeded, so a caller
    /// that asked for a safety bound got a running process and no reason to doubt
    /// the bound was active. Refusing is the only answer that leaves the caller's
    /// next decision correct.
    #[test]
    fn a_caller_asking_for_a_bound_nobody_enforces_is_refused_rather_than_ignored() {
        for (field, mutate) in [
            (
                "max_memory_bytes",
                Box::new(|args: &mut ProcessAdmitArgs| {
                    args.max_memory_bytes = Some(512 * 1024 * 1024)
                }) as Box<dyn Fn(&mut ProcessAdmitArgs)>,
            ),
            (
                "max_child_processes",
                Box::new(|args: &mut ProcessAdmitArgs| args.max_child_processes = Some(4)),
            ),
            (
                "max_output_bytes",
                Box::new(|args: &mut ProcessAdmitArgs| args.max_output_bytes = Some(1_024)),
            ),
        ] {
            let mut overreaching = args("chat_turn");
            mutate(&mut overreaching);
            let refusal = merged_limits(ProcessKind::ChatTurn, &overreaching)
                .expect_err("a chat turn holds none of these");
            assert!(
                refusal.contains(field) && refusal.contains("chat_turn"),
                "the refusal must name the field and the kind, got {refusal:?}"
            );
            assert!(
                refusal.len() > 40,
                "the refusal must say why, not merely that: {refusal:?}"
            );
        }

        // The field its owner *does* read is admitted by the same path, which is
        // what stops "refuse everything" from passing the assertions above.
        let mut bounded = args("chat_turn");
        bounded.max_wall_ms = Some(1_000);
        assert_eq!(
            merged_limits(ProcessKind::ChatTurn, &bounded)
                .expect("a wall budget is read off this row")
                .max_wall_ms,
            Some(1_000)
        );

        // A real bound whose number belongs to the owner is refused too: the
        // daemon enforces memory from the job's own recipe, and a caller value
        // would be replaced on the next projection rather than honoured.
        let mut daemon = args("daemon_job");
        daemon.max_memory_bytes = Some(1_024);
        assert!(merged_limits(ProcessKind::DaemonJob, &daemon).is_err());

        // And the one desktop kind that reads a caller value for output keeps
        // it, so this is a contract rather than a blanket refusal.
        let mut shell = args("background_shell");
        shell.max_output_bytes = Some(4_096);
        assert_eq!(
            merged_limits(ProcessKind::BackgroundShell, &shell)
                .expect("a background shell reads its own output ceiling")
                .max_output_bytes,
            Some(4_096)
        );
    }

    /// A caller may tighten a class default and may not widen it.
    ///
    /// `or` used to mean substitution, so a caller stating a *larger* number
    /// replaced the class bound with its own — which turns a class default into a
    /// suggestion. These fields are maxima, so the merge takes the minimum.
    #[test]
    fn a_caller_may_tighten_a_class_default_but_never_widen_it() {
        let mut wider = args("chat_turn");
        wider.max_wall_ms = Some(WEBVIEW_WALL_BUDGET_MS * 10);
        assert_eq!(
            merged_limits(ProcessKind::ChatTurn, &wider)
                .expect("a wall budget is read off this row")
                .max_wall_ms,
            Some(WEBVIEW_WALL_BUDGET_MS),
            "a caller asking for more must not get more"
        );
    }

    #[test]
    fn a_stated_value_wins_over_the_class_default_field_by_field() {
        let mut stated = args("chat_turn");
        stated.max_wall_ms = Some(30_000);
        let merged = merged_limits(ProcessKind::ChatTurn, &stated).expect("a wall budget");
        assert_eq!(merged.max_wall_ms, Some(30_000));

        // …and only that field. A stated context budget must not wipe the class
        // wall budget beside it, which is exactly what substitution used to do.
        let mut tokens_only = args("chat_turn");
        tokens_only.max_context_tokens = Some(8_192);
        let merged = merged_limits(ProcessKind::ChatTurn, &tokens_only).expect("a context budget");
        assert_eq!(merged.max_context_tokens, Some(8_192));
        assert_eq!(merged.max_wall_ms, Some(WEBVIEW_WALL_BUDGET_MS));
    }

    /// The opt-out has to produce an *absent* budget rather than a zero, because
    /// the ledger's `CHECK` refuses a non-positive `max_wall_ms` and a declared
    /// limit nothing enforces is worse than an honest absence.
    #[test]
    fn the_opt_out_drops_the_budget_rather_than_zeroing_it() {
        let mut off = args("chat_turn");
        off.unbounded_wall = Some(true);
        assert_eq!(
            merged_limits(ProcessKind::ChatTurn, &off)
                .expect("turning a budget off declares less")
                .max_wall_ms,
            None
        );

        // A caller that says both has contradicted itself; "no budget" is the
        // reading that declares less.
        let mut both = args("chat_turn");
        both.unbounded_wall = Some(true);
        both.max_wall_ms = Some(30_000);
        assert_eq!(
            merged_limits(ProcessKind::ChatTurn, &both)
                .expect("the opt-out wins")
                .max_wall_ms,
            None
        );

        // And `false` is not an opt-out.
        let mut on = args("chat_turn");
        on.unbounded_wall = Some(false);
        assert_eq!(
            merged_limits(ProcessKind::ChatTurn, &on)
                .expect("false is not an opt-out")
                .max_wall_ms,
            Some(WEBVIEW_WALL_BUDGET_MS)
        );
    }
}

/// The resource report's own rules, where they are decidable without a ledger.
///
/// The report is the frontend's single source for "what is bounding this", so the
/// two things it *derives* — who supplied the number, and whether this kind gets
/// a host answer at all — are the two worth pinning. Everything else it returns
/// is passed through from `ProcessKind::limit_support` or from the controller's
/// own capabilities, which is the design: one implementation, read twice.
#[cfg(test)]
mod resource_report {
    use super::*;
    use crate::process_table::ProcessLimitKind;

    #[test]
    fn the_origin_of_a_number_is_decided_by_comparing_it_with_the_class_default() {
        assert_eq!(origin_of(Some(8), Some(8)), LimitOrigin::ClassDefault);
        // The only direction a caller can move a bound.
        assert_eq!(origin_of(Some(8), Some(2)), LimitOrigin::CallerOverride);
        assert_eq!(origin_of(None, Some(2)), LimitOrigin::CallerSupplied);
        // A row written before the class declared one. Reported as unknown
        // rather than backfilled with today's number, which this process never
        // ran under.
        assert_eq!(origin_of(Some(8), None), LimitOrigin::Unrecorded);
        // Unbounded is a finding, not a gap.
        assert_eq!(origin_of(None, None), LimitOrigin::Unbounded);
    }

    /// A caller cannot widen, so no origin exists for "looser than the default".
    ///
    /// Not an assertion about this function but about the one upstream of it:
    /// `EffectiveLimits::resolve` takes the minimum, so a row can never carry a
    /// number above its class default and `CallerOverride` can only ever mean
    /// tightened. Stated here because the label the UI shows says "tightened",
    /// and a label that could be wrong in the other direction would be worse
    /// than no label.
    #[test]
    fn a_row_can_never_carry_a_looser_number_than_its_class_default() {
        use crate::process_table::{ProcessKind, ProcessLimits};
        use crate::resource_control::{EffectiveLimits, LimitLayer, LimitSource};

        let class = ProcessKind::ForegroundShell.default_limits();
        let effective = EffectiveLimits::resolve(&[
            LimitLayer::new(LimitSource::ClassDefault, class),
            LimitLayer::new(
                LimitSource::UserOverride,
                ProcessLimits {
                    max_memory_bytes: Some(u64::MAX),
                    ..ProcessLimits::default()
                },
            ),
        ])
        .to_process_limits();
        assert_eq!(effective.max_memory_bytes, class.max_memory_bytes);
        assert_eq!(
            origin_of(class.max_memory_bytes, effective.max_memory_bytes),
            LimitOrigin::ClassDefault,
            "a caller that asked for more must not be credited with setting the bound"
        );
    }

    /// A host answer belongs only to a kind that owns a process tree.
    ///
    /// The failure this prevents is specific: reporting "cgroup v2 `memory.max`"
    /// beside a chat turn's row would name a mechanism for a process no
    /// controller holds, which reads as containment that does not exist.
    #[test]
    fn only_the_kinds_that_own_a_tree_get_a_host_answer() {
        use crate::process_table::ProcessKind;

        for kind in ProcessKind::ALL {
            let owned = is_controller_owned(*kind);
            let claims_a_tree = kind
                .limit_support(ProcessLimitKind::Memory)
                .honours_caller_value();
            assert_eq!(
                owned,
                claims_a_tree,
                "{} disagrees between the report's host answer and the enforcement matrix",
                kind.as_str()
            );
        }
    }
}

/// Restart semantics: what a new app session may do to what an old one left.
///
/// The property under test is a refusal, which is why it needs a test at all. A
/// reclaim that kills everything it finds passes every "the orphan is gone"
/// assertion and is catastrophic exactly once, on the day a recorded pid has been
/// handed to something the user cares about. So each case below pairs a reclaim
/// that must happen with one that must not.
#[cfg(test)]
mod restart_semantics {
    use crate::process_table::{
        still_the_recorded_process, ProcessKind, ProcessRecord, ProcessState, SignalIntent,
    };

    fn row(native_pid: Option<i64>, native_start_time: Option<i64>) -> ProcessRecord {
        ProcessRecord {
            process_id: "fgsh-restart".to_string(),
            parent_process_id: None,
            kind: ProcessKind::ForegroundShell,
            external_id: "ext".to_string(),
            state: ProcessState::Running,
            run_id: None,
            workspace: None,
            profile: None,
            native_pid,
            native_start_time,
            limits: ProcessKind::ForegroundShell.default_limits(),
            containment: None,
            usage: None,
            usage_sampled_at_ms: None,
            signal_intent: SignalIntent::default(),
            signal_reason: None,
            signal_requested_at_ms: None,
            exit: None,
            created_at_ms: 1,
            updated_at_ms: 1,
            started_at_ms: Some(1),
            exited_at_ms: None,
        }
    }

    /// This process is the one case a test can be certain about.
    #[test]
    fn a_row_whose_identity_still_matches_is_reclaimable() {
        let pid = std::process::id();
        let identity = crate::process_tree::ProcessIdentity::of(pid)
            .expect("this process can identify itself");
        assert!(still_the_recorded_process(&row(
            Some(i64::from(pid)),
            Some(i64::try_from(identity.start_time).unwrap()),
        )));
    }

    /// The whole safety property: a reused pid is not this row's process.
    ///
    /// The recorded start time is deliberately one the live process cannot have.
    /// Before V22 there was no start time to disagree with, so this row would
    /// have been signalled — and on a restart hours later, against a pid the
    /// kernel had long since reassigned, that is how a reconciler kills a
    /// bystander.
    #[test]
    fn a_row_whose_pid_was_reused_is_never_signalled() {
        let pid = std::process::id();
        let identity = crate::process_tree::ProcessIdentity::of(pid)
            .expect("this process can identify itself");
        let stale = i64::try_from(identity.start_time)
            .unwrap()
            .saturating_sub(1);
        assert!(
            !still_the_recorded_process(&row(Some(i64::from(pid)), Some(stale))),
            "a start time that does not match means the pid is somebody else's now"
        );
    }

    /// A pre-V22 row cannot prove anything, so it is left alone.
    ///
    /// Legacy rows stay readable — nothing is backfilled and no zero is invented
    /// — and the cost is that an orphan from before this schema is not reclaimed.
    /// That is the correct direction to fail in.
    #[test]
    fn a_legacy_row_without_a_start_time_is_left_alone_rather_than_guessed_at() {
        let pid = std::process::id();
        assert!(!still_the_recorded_process(&row(
            Some(i64::from(pid)),
            None
        )));
    }

    /// A row with no pid says nothing about anything running.
    #[test]
    fn a_row_with_no_pid_is_not_reclaimable() {
        assert!(!still_the_recorded_process(&row(None, Some(1))));
    }

    /// A pid nothing occupies is not reclaimable either — there is no tree left.
    #[test]
    fn a_row_whose_process_is_gone_is_not_signalled() {
        // A pid that has certainly exited: spawn one and reap it.
        let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" })
            .args(if cfg!(windows) {
                vec!["/C", "exit"]
            } else {
                vec![]
            })
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("a trivial child spawns");
        let pid = child.id();
        let identity = crate::process_tree::ProcessIdentity::of(pid);
        let _ = child.wait();
        let start = identity
            .map(|identity| i64::try_from(identity.start_time).unwrap())
            .unwrap_or(1);
        assert!(!still_the_recorded_process(&row(
            Some(i64::from(pid)),
            Some(start)
        )));
    }
}

/// The K7/K8 re-audit, as assertions rather than as a paragraph.
///
/// # What was actually checked
///
/// The scheduler's admission (`daemon/admission.rs`, driven from
/// `daemon/engine.rs`) reserves against **model footprint** — RAM and per-device
/// VRAM for a model's weights — keyed by `ModelTargetSnapshot::target_id()` and
/// summed with a `GROUP BY` over `daemon_jobs`. It never reads
/// `ProcessLimits`, never counts rows in this table, and its concurrency bound
/// is a fixed integer over daemon jobs. So the four things this work changed —
/// foreground shells gaining rows, background shells outliving their turn,
/// browser sessions being routed through a controller, and rows storing the
/// *effective* limit rather than the requested one — reach it through no path at
/// all.
///
/// That is a finding worth pinning rather than restating, because the failure it
/// rules out is a specific one: if admission had summed `max_memory_bytes` over
/// live rows, this work would have quietly added 8 GiB of "reservation" per
/// agent shell and 4 GiB per browser session to a machine's accounting, and the
/// queue would have stopped admitting work for a reason no operator could see.
///
/// The other half — "does the scheduler reserve on the same semantics execution
/// uses" — is the daemon's own budgets, and there the answer is yes by
/// construction: `engine.rs` projects the recipe's `max_runtime_ms` /
/// `max_memory_bytes` / `max_log_bytes` onto the row and its watchdog enforces
/// those same fields. One owner, one number, reported as `owner-sourced`.
#[cfg(test)]
mod scheduler_assumptions {
    use super::*;
    use crate::process_table::{LimitEnforcement, ProcessLimitKind};

    /// A daemon job's memory bound belongs to the daemon, and this table says so.
    ///
    /// The distinction that keeps two killers off one resource: the daemon
    /// watchdog measures the job's process group against the recipe's number,
    /// and no `ResourceController` is attached to that same process. A kind that
    /// claimed both would be two owners of one resource with two numbers.
    #[test]
    fn a_daemon_job_s_memory_is_owner_sourced_and_not_controller_held() {
        assert!(matches!(
            ProcessKind::DaemonJob.limit_support(ProcessLimitKind::Memory),
            LimitEnforcement::OwnerSourced(_)
        ));
        assert!(
            !super::is_controller_owned(ProcessKind::DaemonJob),
            "a daemon job's tree is bounded by its own watchdog; attaching a controller \
             would put two owners on one resource"
        );
    }

    /// The kinds a controller owns are exactly the kinds the scheduler does not
    /// reserve for.
    ///
    /// Admission reserves for work that loads a *model*; a shell and a browser
    /// session load none. If that ever stops being true, this fails and whoever
    /// changed it has to decide how the two accountings compose rather than
    /// discovering it from a queue that stopped moving.
    #[test]
    fn no_controller_owned_kind_is_a_kind_the_scheduler_reserves_for() {
        for kind in ProcessKind::ALL {
            if !super::is_controller_owned(*kind) {
                continue;
            }
            assert!(
                !matches!(kind, ProcessKind::DaemonJob | ProcessKind::RemoteRun),
                "{} is both controller-owned and a scheduler-reserved kind; its memory \
                 would be charged twice",
                kind.as_str()
            );
        }
    }

    /// A restart must not close a live daemon job.
    ///
    /// The startup reap is scoped to `DESKTOP_OWNED` precisely because the daemon
    /// is a separate service that outlives the app. A background shell *is* in
    /// that scope and is meant to be — its owner died with the app — while a
    /// daemon job's reservations are still held by a process that is still
    /// running.
    #[test]
    fn the_startup_reap_leaves_the_kinds_it_does_not_own_alone() {
        assert!(!ProcessKind::DESKTOP_OWNED.contains(&ProcessKind::DaemonJob));
        assert!(!ProcessKind::DESKTOP_OWNED.contains(&ProcessKind::WorkflowRun));
        assert!(ProcessKind::DESKTOP_OWNED.contains(&ProcessKind::BackgroundShell));
        assert!(ProcessKind::DESKTOP_OWNED.contains(&ProcessKind::ForegroundShell));
    }
}
