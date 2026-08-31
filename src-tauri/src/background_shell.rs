//! Long-running background shell commands started by the agent.
//!
//! `tools::tool_run_shell` is deliberately synchronous and time-capped: it
//! blocks the tool call until the command exits or `SHELL_TIMEOUT` fires, and
//! the child is killed on drop. That is the right shape for `cargo check` or
//! `git status`, and the wrong shape for a dev server, a watcher, or a long
//! test run — the model should be able to start those, keep working, and poll
//! their output later. This module is that second shape: the agent's
//! `run_shell` tool with `run_in_background: true` lands here instead, the
//! process is owned by Rust (not by the turn that started it), and it keeps
//! running after the turn ends until it exits on its own or something calls
//! `background_shell_kill`.
//!
//! Every entry point is permission-gated exactly like the foreground tool —
//! same `"run_shell"` tool name, so an existing "allow for session" grant and
//! every mode short-circuit apply unchanged. Output is retained in a bounded
//! in-memory tail (front-truncated once it exceeds [`MAX_OUTPUT_BYTES`]) and
//! mirrored to the frontend over events, the same posture `terminal.rs` uses
//! for PTY output — nothing here is written to disk.

use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use std::process::{Command, Stdio};

use serde::Serialize;
use tauri::{Emitter, Manager};

use crate::{checkpoints, permissions, workspace, AppState};

/// Bounded retained tail per background command, matching `terminal.rs`'s own
/// cap — enough to diagnose a failing watcher, small enough that a chatty
/// process can't grow the heap without limit.
pub const MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// Largest single `background-shell-output` event payload. A burst larger
/// than this is split across events rather than sent as one huge IPC message.
const MAX_EVENT_CHUNK_BYTES: usize = 32 * 1024;
/// How often the exit watcher polls `try_wait`. Polling (rather than a
/// blocking `wait`) keeps the child mutex free so `background_shell_kill` can
/// take it at any moment — same reasoning as `terminal.rs`'s exit watcher.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How many exit-watcher ticks pass between writes of a live command's sampled
/// usage to its ledger row — ten, so one write per second.
///
/// Two different rates, on purpose, because the two costs are not the same:
///
/// - **Sampling** rides every [`EXIT_POLL_INTERVAL`] tick, because it is one
///   syscall and because peak resident size is unreadable once the pid is gone
///   (see `process_usage`). Sampling on the same tick that notices the exit is
///   what bounds how stale the final CPU reading can be — at most one tick — and
///   it needs no thread of its own.
/// - **Writing** is a SQLite `UPDATE`, and once a second is already generous for
///   a ledger. The daemon's watchdog polls at 250ms because it has to *react*
///   inside a budget; this only has to *record*, and the row's peak fields are
///   kernel-maintained high-water marks, so a slower write loses nothing but
///   freshness for a command that is still running.
const USAGE_FLUSH_TICKS: u32 = 10;
/// Hard cap on concurrently running background commands, so a looping model
/// can't spawn processes without bound.
const MAX_RUNNING: usize = 16;

pub const OUTPUT_EVENT: &str = "background-shell-output";
pub const STATUS_EVENT: &str = "background-shell-status";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundShellStatus {
    Running,
    Exited,
    Killed,
    Error,
}

/// The frontend-visible state of one background command. Mirrored by
/// `src/store/backgroundShellStore.ts`.
#[derive(Clone, Debug, Serialize)]
pub struct BackgroundShellView {
    pub id: String,
    /// The exact command line handed to the shell — shown verbatim in the
    /// Background Tasks panel, so the user can always see what is running.
    pub command: String,
    pub cwd: String,
    pub status: BackgroundShellStatus,
    pub exit_code: Option<i32>,
    /// Interleaved stdout/stderr tail, bounded by [`MAX_OUTPUT_BYTES`].
    pub output: String,
    pub output_truncated: bool,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
}

struct BackgroundProcess {
    view: Mutex<BackgroundShellView>,
    child: Mutex<crate::workspace_shell::BackgroundShellChild>,
    /// What is holding this command's process tree.
    ///
    /// Owned here for the same reason the child is: the containment's lifetime
    /// has to be the command's lifetime. On Windows the job handle carries
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so dropping this kills the tree —
    /// which is correct at teardown and catastrophic anywhere earlier.
    controller: Mutex<crate::resource_control::ResourceController>,
    /// The last measurement the exit watcher took of this command's tree.
    ///
    /// Held rather than discarded because that watcher is the only thing that
    /// samples a background shell — nothing is blocked on it — and until this
    /// existed its readings ended inside `check_resource_limits`' `match`, so the
    /// row could state a memory ceiling and never what was held against it.
    last_sample: Mutex<Option<crate::resource_control::ResourceSample>>,
    /// Byte offset into `view.output` already returned by
    /// `background_shell_output` in draining mode — the "only new output"
    /// cursor the model's polling tool reads through. Kept out of the view so
    /// it is never serialized to (or settable by) the frontend, and clamped
    /// on every front-truncation of the buffer.
    read_cursor: Mutex<usize>,
}

impl BackgroundProcess {
    fn view(&self) -> Result<BackgroundShellView, String> {
        Ok(lock(&self.view)?.clone())
    }
}

#[derive(Default)]
pub struct BackgroundShellManager {
    procs: Mutex<HashMap<String, Arc<BackgroundProcess>>>,
}

impl BackgroundShellManager {
    fn get(&self, id: &str) -> Result<Arc<BackgroundProcess>, String> {
        lock(&self.procs)?
            .get(id)
            .cloned()
            .ok_or_else(|| format!("No background command with id '{id}'"))
    }

    fn insert(&self, id: String, process: Arc<BackgroundProcess>) -> Result<(), String> {
        lock(&self.procs)?.insert(id, process);
        Ok(())
    }

    fn list(&self) -> Result<Vec<BackgroundShellView>, String> {
        let procs: Vec<_> = lock(&self.procs)?.values().cloned().collect();
        let mut views = procs
            .into_iter()
            .map(|process| process.view())
            .collect::<Result<Vec<_>, _>>()?;
        views.sort_by_key(|view| view.started_at_ms);
        Ok(views)
    }

    fn running_count(&self) -> Result<usize, String> {
        let procs: Vec<_> = lock(&self.procs)?.values().cloned().collect();
        Ok(procs
            .into_iter()
            .filter(|process| {
                process
                    .view()
                    .map(|view| view.status == BackgroundShellStatus::Running)
                    .unwrap_or(false)
            })
            .count())
    }

    /// Drops every terminal entry from the registry. Running commands are
    /// left untouched — "Clear" in the panel is a list-tidying action, never
    /// a kill.
    fn clear_finished(&self) -> Result<(), String> {
        let mut procs = lock(&self.procs)?;
        let terminal: Vec<String> = procs
            .iter()
            .filter(|(_, process)| {
                process
                    .view()
                    .map(|view| view.status != BackgroundShellStatus::Running)
                    .unwrap_or(false)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in terminal {
            procs.remove(&id);
        }
        Ok(())
    }

    /// Kills every still-running background command. Called on app shutdown
    /// alongside `TerminalManager::kill_all` — a background command outliving
    /// the window that started it would be an orphan no UI can ever reach.
    pub(crate) fn kill_all(&self) {
        let procs: Vec<_> = self
            .procs
            .lock()
            .map(|procs| procs.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for process in procs {
            if let Ok(mut child) = process.child.lock() {
                let _ = child.kill();
            }
            // The whole owned tree, not the leader. On shutdown this is the last
            // chance: anything left after the app exits is an orphan no UI can
            // ever reach again.
            reclaim_owned_tree(&process);
        }
    }
}

/// Reclaim everything one background command owns, through its controller.
///
/// Every deliberate teardown goes through here rather than through the child
/// handle alone: `BackgroundShellChild::kill` signals the process group, which
/// covers the ordinary case and misses the one this exists for — a descendant
/// that left the group, or a Windows tree whose containment is the job rather
/// than any group at all. The controller recorded that membership while the
/// ancestry was still readable, which is the only moment it could be recorded.
///
/// Idempotent and fail-soft: a tree already gone costs one process-table read,
/// and a teardown must not fail because one member could not be signalled.
fn reclaim_owned_tree(process: &Arc<BackgroundProcess>) {
    let Ok(mut controller) = process.controller.lock() else {
        return;
    };
    if let Err(error) = controller.terminate_tree() {
        eprintln!("background shell: could not reclaim the whole owned tree: {error}");
    }
}

fn lock<T>(value: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>, String> {
    value
        .lock()
        .map_err(|_| "Background shell state lock poisoned".to_string())
}

fn now_ms() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .map_err(|error| format!("System clock error: {error}"))
}

/// Appends to a bounded tail, dropping whole leading bytes once the cap is
/// exceeded. Returns how many bytes were dropped from the front so the read
/// cursor can be shifted by the same amount (otherwise a truncation would
/// silently re-serve already-read output as "new").
fn append_bounded(buffer: &mut String, chunk: &str, truncated: &mut bool) -> usize {
    buffer.push_str(chunk);
    if buffer.len() <= MAX_OUTPUT_BYTES {
        return 0;
    }
    let overflow = buffer.len() - MAX_OUTPUT_BYTES;
    // Never split a UTF-8 code point: walk forward to the next boundary.
    let mut cut = overflow;
    while cut < buffer.len() && !buffer.is_char_boundary(cut) {
        cut += 1;
    }
    buffer.drain(..cut);
    *truncated = true;
    cut
}

/// `native_pid` is only ever `Some` at the spawn call site — every later
/// status change (exit, kill, error) passes `None`, which `ProcessTable`'s
/// reconcile treats as "leave whatever was already recorded alone" rather
/// than clearing it (see `process_table.rs`'s `reconcile`: it writes
/// `native_pid` only when the projection supplies `Some`).
fn emit_status<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    view: BackgroundShellView,
    identity: Option<crate::process_tree::ProcessIdentity>,
) {
    project_process(app, &view, identity, None);
    let _ = app.emit(STATUS_EVENT, serde_json::json!({ "task": view }));
}

/// [`emit_status`] for the one status change that carries a resource cause.
///
/// A separate entry point rather than a parameter on `emit_status` because every
/// other call site would have to pass `None`, and a `None` at forty call sites is
/// how a caller eventually passes the wrong thing.
fn emit_status_with_breach<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    view: BackgroundShellView,
    breach: Option<crate::resource_control::LimitBreach>,
) {
    project_process(app, &view, None, breach);
    let _ = app.emit(STATUS_EVENT, serde_json::json!({ "task": view }));
}

/// Projects a background shell onto the unified process table.
///
/// Hooked into [`emit_status`] because that is the single function every status
/// change already funnels through — spawn, exit, kill, and error all call it, so
/// there is no path that changes a shell's state without projecting it.
///
/// Fail-soft, like every other adopter: a shell must not fail to report its
/// status because a bookkeeping row could not be written.
fn project_process<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    view: &BackgroundShellView,
    identity: Option<crate::process_tree::ProcessIdentity>,
    breach: Option<crate::resource_control::LimitBreach>,
) {
    use crate::process_table::{
        ExitStatus, ProcessExit, ProcessKind, ProcessProjection, ProcessState,
    };

    let (state, exit) = match view.status {
        BackgroundShellStatus::Running => (ProcessState::Running, None),
        BackgroundShellStatus::Exited => (
            ProcessState::Exited,
            Some(ProcessExit {
                // A non-zero exit is `Error` in this module's own vocabulary, so
                // reaching here means the command genuinely succeeded.
                status: ExitStatus::Succeeded,
                code: view.exit_code,
                signal: None,
                reason: None,
                breach: None,
            }),
        ),
        BackgroundShellStatus::Killed => (
            ProcessState::Exited,
            Some(ProcessExit {
                status: ExitStatus::Cancelled,
                code: view.exit_code,
                signal: None,
                reason: Some("killed".to_string()),
                breach: None,
            }),
        ),
        // A resource kill also arrives as `Error`, and the two must not look the
        // same: one is the command's own failure and the other is the system
        // working. The breach is what tells them apart.
        BackgroundShellStatus::Error => (
            ProcessState::Exited,
            Some(match &breach {
                Some(breach) => ProcessExit::limit_exceeded(breach.clone()),
                None => ProcessExit {
                    status: ExitStatus::Failed,
                    code: view.exit_code,
                    signal: None,
                    reason: None,
                    breach: None,
                },
            }),
        ),
    };

    // What holds this shell and what it was last measured holding, read from the
    // live process rather than threaded through every status call site. Both are
    // absent once the shell has left the manager, which is exactly when there is
    // nothing left to measure — and `reconcile` leaves a `None` alone rather than
    // clearing what was already recorded.
    let (containment, usage) = match app
        .state::<crate::AppState>()
        .background_shell
        .get(&view.id)
    {
        Ok(process) => (
            process
                .controller
                .lock()
                .ok()
                .map(|controller| controller.containment()),
            process.last_sample.lock().ok().and_then(|held| *held),
        ),
        Err(_) => (None, None),
    };
    let mut projection =
        ProcessProjection::new(ProcessKind::BackgroundShell, view.id.clone(), state)
            .with_workspace(Some(view.cwd.clone()))
            // The identity rather than the pid, so a restart can tell this
            // process from whatever the kernel later gave its pid to.
            .with_native_identity(identity)
            .with_containment(containment)
            .with_usage(usage);
    projection.exit = exit;

    let state_handle = app.state::<crate::AppState>();
    if let Err(error) =
        crate::process_commands::project_process_record(app, state_handle.inner(), &projection)
    {
        eprintln!("background shell: could not project {}: {error}", view.id);
    }
}

/// Folds one sampled reading into a background shell's resource ledger row.
///
/// Split out from the thread that calls it so it can be driven against an
/// in-memory ledger in a test. A row that is not there is **not** an error and
/// records nothing: the exit watcher starts a moment before the spawn's own
/// `emit_status` projects the row, and a missing row later would mean the command
/// was cleared — neither is a reason to invent one.
///
/// Only [`ProcessTable::accumulate_usage`] is used here, never
/// `add_egress_bytes`, and the sample this hands over always carries
/// `bytes_egressed: None` because nothing in `process_usage::sample` measures
/// egress. That is what keeps the two conventions from meeting on one row: the
/// egress column is only ever written additively, by whoever counted the bytes,
/// and the maximum-folding writer leaves it alone.
fn record_usage(
    table: &crate::process_table::ProcessTable<'_>,
    external_id: &str,
    sample: &crate::process_usage::ProcessUsageSample,
    now_ms: i64,
) -> Result<bool, crate::process_table::ProcessTableError> {
    use crate::process_table::ProcessKind;

    let Some(record) = table.find_by_external_id(ProcessKind::BackgroundShell, external_id)? else {
        return Ok(false);
    };
    table.accumulate_usage(&record.process_id, sample, now_ms)?;
    Ok(true)
}

/// Writes what has been sampled so far. Fail-soft like every other bookkeeping
/// call here — a command must not die because its ledger row could not be updated.
fn flush_usage<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    external_id: &str,
    usage: &crate::process_usage::ProcessUsageAccumulator,
) {
    let Ok(now) = crate::run_commands::unix_time_ms() else {
        return;
    };
    let state = app.state::<crate::AppState>();
    if let Err(error) = crate::process_commands::with_process_table(app, state.inner(), |table| {
        record_usage(table, external_id, usage.sample(), now as i64).map(|_| ())
    }) {
        eprintln!("background shell: could not record usage for {external_id}: {error}");
    }
}

/// Delivers a real OS suspend/resume to a background shell's child,
/// immediately — there is no safe point to wait for; the process either can
/// be paused right now or it can't, unlike the cooperative kinds that check a
/// latch at their own round boundary. Reflects the outcome directly in the
/// process table's `state` rather than leaving it at "signal recorded" for
/// some later poll to notice, since delivery here is synchronous with the
/// caller.
///
/// Fail-soft only at the OS layer: if the child can no longer be found (it
/// already exited, or this app instance never held it — e.g. after a
/// restart, since `BackgroundShell` is desktop-owned and reaped at startup),
/// the durable latch `ProcessTable::signal` already wrote is left as-is and
/// this returns the record unchanged; a later reap resolves it honestly
/// rather than this call guessing an outcome.
pub(crate) fn deliver_os_signal<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    record: &crate::process_table::ProcessRecord,
    signal: crate::process_table::ProcessSignal,
) -> Result<crate::process_table::ProcessRecord, String> {
    use crate::process_table::{ProcessSignal, ProcessState};

    let Ok(process) = state.background_shell.get(&record.external_id) else {
        return Ok(record.clone());
    };
    let Ok(child) = lock(&process.child) else {
        return Ok(record.clone());
    };
    let pid = child.id();
    drop(child);

    match signal {
        ProcessSignal::Suspend => crate::os_signal::suspend_process_group(pid)?,
        ProcessSignal::Resume => crate::os_signal::resume_process_group(pid)?,
        _ => return Ok(record.clone()),
    }

    let next_state = if signal == ProcessSignal::Suspend {
        ProcessState::Suspended
    } else {
        ProcessState::Running
    };
    let now = crate::run_commands::unix_time_ms()? as i64;
    crate::process_commands::with_process_table(app, state, |table| {
        table.transition(&record.process_id, next_state, None, now)
    })
}

/// Streams one of the child's pipes into the shared bounded tail, emitting
/// each chunk to the frontend as it arrives. stdout and stderr each get one
/// of these; both append to the same buffer, so the panel shows them
/// interleaved in arrival order exactly like a terminal would.
fn spawn_reader<Rt: tauri::Runtime, R: Read + Send + 'static>(
    app: tauri::AppHandle<Rt>,
    process: Arc<BackgroundProcess>,
    mut reader: R,
) {
    std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let chunk = String::from_utf8_lossy(&buffer[..count]).to_string();
                    let (id, truncated) = {
                        let Ok(mut view) = process.view.lock() else {
                            break;
                        };
                        let mut flag = view.output_truncated;
                        let dropped = append_bounded(&mut view.output, &chunk, &mut flag);
                        view.output_truncated = flag;
                        if dropped > 0 {
                            if let Ok(mut cursor) = process.read_cursor.lock() {
                                *cursor = cursor.saturating_sub(dropped);
                            }
                        }
                        (view.id.clone(), view.output_truncated)
                    };
                    for piece in split_event_chunks(&chunk) {
                        let _ = app.emit(
                            OUTPUT_EVENT,
                            serde_json::json!({
                                "id": id,
                                "chunk": piece,
                                "output_truncated": truncated,
                            }),
                        );
                    }
                }
            }
        }
    });
}

/// Splits a burst into event-sized pieces on UTF-8 boundaries.
fn split_event_chunks(chunk: &str) -> Vec<String> {
    if chunk.len() <= MAX_EVENT_CHUNK_BYTES {
        return vec![chunk.to_string()];
    }
    let mut pieces = Vec::new();
    let mut start = 0;
    while start < chunk.len() {
        let mut end = (start + MAX_EVENT_CHUNK_BYTES).min(chunk.len());
        while end < chunk.len() && !chunk.is_char_boundary(end) {
            end += 1;
        }
        pieces.push(chunk[start..end].to_string());
        start = end;
    }
    pieces
}

/// Watches for the child's exit, and samples its resources on the way.
///
/// The sampling lives here rather than in a thread of its own because this loop
/// already has the two things it needs — the pid and a tick — and because the
/// reading that matters most is the last one before the exit this loop is the
/// first to notice. See [`USAGE_FLUSH_TICKS`] for why sampling and writing run at
/// different rates, and `process_usage` for why sampling has to happen at all
/// (peak resident size cannot be read from a pid that is gone).
/// Ask this command's controller whether a limit has fired.
///
/// One call, [`crate::resource_control::ResourceController::check`], rather than
/// this module's own ordering of the primitives. That ordering is the bug this
/// wrapper used to carry: it sampled and compared, which is the right test for a
/// *supervised* bound and is never true for a kernel-held one — a cgroup with
/// `pids.max = 12` refuses the thirteenth fork and leaves `pids.current` at 12
/// forever. So a background command killed by a real kernel limit was recorded as
/// an unexplained error, while the identical foreground command — which went
/// through `run_under`, which did ask the mechanism — was correctly recorded as
/// `limit_exceeded`.
///
/// `None` covers three different situations on purpose — no limit configured,
/// nothing measurable, and inside every bound — because the watcher's next action
/// is the same for all three: keep waiting. Only a breach changes what happens.
fn check_resource_limits(
    process: &Arc<BackgroundProcess>,
) -> Option<crate::resource_control::LimitBreach> {
    use crate::resource_control::ResourceCheck;

    let mut controller = process.controller.lock().ok()?;
    let remember = |sample: Option<crate::resource_control::ResourceSample>| {
        if let (Some(sample), Ok(mut held)) = (sample, process.last_sample.lock()) {
            *held = Some(sample);
        }
    };
    // Bound before the `match` so the mutable borrow ends here: the arms below
    // ask the same controller what it has recorded.
    let checked = controller.check(now_ms().ok()? as i64);
    match checked {
        // The tree is already reclaimed by `check`: a bound that reports a breach
        // and leaves the workload running has reclaimed nothing.
        Ok(ResourceCheck::Breached { breach, sample }) => {
            remember(sample);
            Some(breach)
        }
        Ok(ResourceCheck::Running(sample)) => {
            remember(Some(sample));
            None
        }
        Ok(ResourceCheck::Gone) => None,
        Err(error) => {
            eprintln!("background shell: could not check a command against its limits: {error}");
            // A `check` that found a breach and then could not finish reclaiming
            // the tree fails here, and the breach it found is the reason the tree
            // is going. Reporting `None` dropped it, and the exit this loop
            // observed a moment later was written down as an ordinary failure.
            controller.recorded_breach()
        }
    }
}

/// Why this command became terminal, when its bounds have an answer.
///
/// Asked *before* the exit status is classified, and that order is the whole
/// point. A kernel limit fires, refuses the work, and the child becomes terminal
/// — often inside one poll interval, so the sampling loop above never gets a tick
/// while the child is still running. Reading the exit status first would call
/// that ordinary failure, which is the same mistake in a different shape: the
/// limit worked and the app could not say so.
///
/// # Two questions, because one of them cannot be asked twice
///
/// [`crate::resource_control::ResourceController::mechanism_breach`] is the rule
/// [`crate::resource_control::run_under`] already applies to the foreground
/// shell, and it is the only one a *kernel* bound answers. It is also structurally
/// silent on the supervisor — the backend macOS always uses and the fallback
/// everywhere else — because a supervised bound is not a counter to read back but
/// a comparison against a live tree, made by the loop above and made once. By the
/// time this is asked the tree is gone and nothing can find it again.
///
/// So the controller's own record is consulted too. Without it the ordering
/// between the two observations decided the answer: a tree that crossed its
/// budget and became terminal before the next tick — or one whose breach was
/// found and then lost to a reclaim that reported a survivor — was classified
/// from the exit status alone, and `137` reads exactly like a command that failed.
fn terminal_resource_breach(
    process: &Arc<BackgroundProcess>,
) -> Option<crate::resource_control::LimitBreach> {
    let mut controller = process.controller.lock().ok()?;
    let asked = controller.mechanism_breach(now_ms().ok()? as i64);
    match asked {
        Ok(breach) => breach.or_else(|| controller.recorded_breach()),
        Err(error) => {
            eprintln!("background shell: could not ask the mechanism why a command ended: {error}");
            controller.recorded_breach()
        }
    }
}

/// Close the row out as `limit_exceeded`, naming the limit and both numbers.
///
/// Deliberately not `Killed`: a command stopped for holding 9 GiB and a command
/// a user stopped are different facts, and the panel showing the same word for
/// both is what made a working budget look like an unexplained disappearance.
fn record_limit_breach<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    process: &Arc<BackgroundProcess>,
    usage: &crate::process_usage::ProcessUsageAccumulator,
    breach: crate::resource_control::LimitBreach,
) {
    let external_id = process.view().map(|view| view.id).unwrap_or_default();
    flush_usage(app, &external_id, usage);
    if let Ok(mut view) = process.view.lock() {
        view.status = BackgroundShellStatus::Error;
        view.finished_at_ms = now_ms().ok();
        view.output
            .push_str(&format!("\n[{}]\n", breach.describe()));
        emit_status_with_breach(app, view.clone(), Some(breach));
    }
}

fn spawn_exit_watcher<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    process: Arc<BackgroundProcess>,
) {
    std::thread::spawn(move || {
        let external_id = process.view().map(|view| view.id).unwrap_or_default();
        // Captured once: `Child::id` keeps answering after the child exits, but
        // the lock is cheaper to take once than every tick.
        let pid = process
            .child
            .lock()
            .ok()
            .and_then(|child| i64::try_from(child.id()).ok());
        let mut usage = crate::process_usage::ProcessUsageAccumulator::new();
        let mut ticks: u32 = 0;

        loop {
            // Before `try_wait`, so the reading is of a process still alive. On the
            // tick that observes the exit this sample fails and folds nothing, which
            // leaves the accumulator holding the previous tick's live reading — at
            // most one `EXIT_POLL_INTERVAL` old.
            if let Some(pid) = pid {
                usage.observe_pid(pid);
            }
            let waited = {
                let Ok(mut child) = process.child.lock() else {
                    break;
                };
                child.try_wait()
            };
            match waited {
                Ok(None) => {
                    // The same tick that samples for the ledger also asks the
                    // controller whether a bound has been crossed. A background
                    // command has no caller blocked on it, so this loop is the
                    // only thing that can notice — and a limit nobody checks is
                    // a limit that never fires.
                    if let Some(breach) = check_resource_limits(&process) {
                        record_limit_breach(&app, &process, &usage, breach);
                        break;
                    }
                    ticks = ticks.wrapping_add(1);
                    if ticks % USAGE_FLUSH_TICKS == 0 {
                        flush_usage(&app, &external_id, &usage);
                    }
                    std::thread::sleep(EXIT_POLL_INTERVAL)
                }
                Ok(Some(status)) => {
                    // The mechanism gets the first word about *why* this child is
                    // terminal, before its exit status is read. See
                    // `terminal_resource_breach`.
                    if let Some(breach) = terminal_resource_breach(&process) {
                        record_limit_breach(&app, &process, &usage, breach);
                        break;
                    }
                    // Written *before* `emit_status`, and the order is load-bearing:
                    // that call projects the terminal state, and `ProcessTable`'s
                    // terminal transition closes the ledger row out — it records which
                    // fields are unavailable and why. A measurement arriving after
                    // close-out would leave a value sitting next to a note saying it
                    // was never measured.
                    flush_usage(&app, &external_id, &usage);
                    if let Ok(mut view) = process.view.lock() {
                        // `kill` already moved the view to `Killed` and stamped
                        // `finished_at_ms`; don't overwrite that with the signal
                        // exit this watcher observes a moment later.
                        if view.status == BackgroundShellStatus::Running {
                            view.status = if status.success() {
                                BackgroundShellStatus::Exited
                            } else {
                                BackgroundShellStatus::Error
                            };
                            view.exit_code = status.code();
                            view.finished_at_ms = now_ms().ok();
                        }
                        emit_status(&app, view.clone(), None);
                    }
                    break;
                }
                Err(error) => {
                    // Same two ordering rules as the exit branch above: the
                    // mechanism is asked first, because a `wait` that fails
                    // against a tree the kernel just reclaimed is a limit kill
                    // wearing an errno.
                    if let Some(breach) = terminal_resource_breach(&process) {
                        record_limit_breach(&app, &process, &usage, breach);
                        break;
                    }
                    flush_usage(&app, &external_id, &usage);
                    if let Ok(mut view) = process.view.lock() {
                        if view.status == BackgroundShellStatus::Running {
                            view.status = BackgroundShellStatus::Error;
                            let mut truncated = view.output_truncated;
                            append_bounded(
                                &mut view.output,
                                &format!("\n[{error}]\n"),
                                &mut truncated,
                            );
                            view.output_truncated = truncated;
                            view.finished_at_ms = now_ms().ok();
                        }
                        emit_status(&app, view.clone(), None);
                    }
                    break;
                }
            }
        }
    });
}

/// Starts a shell command that outlives the calling turn. Permission-gated
/// under the same `"run_shell"` tool name as the foreground command, so the
/// user's existing grants and mode short-circuits apply unchanged, and — like
/// the foreground path — the injected `risk_level`/`risk_reason`/`agent_label`
/// are display-only and can never auto-approve the call.
///
/// Unlike `tools::tool_run_shell`, this returns as soon as the child is
/// spawned: there is no timeout and no `kill_on_drop`, because outliving the
/// tool call is the entire point. The process is reachable afterwards through
/// `background_shell_output`/`background_shell_kill`, and is killed on app
/// shutdown by `BackgroundShellManager::kill_all`.
#[tauri::command(rename_all = "snake_case")]
pub async fn tool_run_shell_background(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    command: String,
    cwd: Option<String>,
    checkpoint_id: Option<String>,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
    risk_level: Option<String>,
    risk_reason: Option<String>,
    agent_label: Option<String>,
    workspace_root_override: Option<String>,
) -> Result<BackgroundShellView, String> {
    let risk = permissions::compute_risk(None, risk_level, risk_reason);
    permissions::request_permission(
        &app,
        state.inner(),
        "run_shell",
        command.clone(),
        turn_id.as_deref(),
        tool_call_id.as_deref(),
        risk,
        agent_label.as_deref(),
    )
    .await?;

    // Same revert-coverage caveat as the foreground tool: a background
    // command's side effects aren't captured by the turn's checkpoint either.
    checkpoints::record_shell(state.inner(), checkpoint_id.as_deref())?;

    if state.background_shell.running_count()? >= MAX_RUNNING {
        return Err(format!(
            "Too many background commands are already running (limit {MAX_RUNNING}). Kill one with shell_kill first."
        ));
    }

    let (cwd_path, workspace_root) = match cwd {
        Some(ref value) => crate::agent_worktrees::resolve_with_override(
            state.inner(),
            value,
            workspace_root_override.as_deref(),
        )?,
        None => match workspace_root_override.as_deref() {
            Some(root) => {
                crate::agent_worktrees::resolve_with_override(state.inner(), ".", Some(root))?
            }
            None => {
                let root = workspace::primary_root_canon(state.inner())?;
                (root.clone(), root)
            }
        },
    };

    let view = start_background_command(
        &app,
        state.inner(),
        command,
        &workspace_root,
        &cwd_path,
        crate::process_table::ProcessLimits::default(),
    )?;
    // Committed at the spawn, not at the exit: this command is *meant* to
    // outlive the call, so waiting for its exit code would leave every
    // still-running background command looking like one that never started.
    // What is being committed is that a process exists, which it does.
    checkpoints::commit_external_effect(
        state.inner(),
        checkpoint_id.as_deref(),
        checkpoints::ExternalEffectKind::Shell,
    )?;
    Ok(view)
}

/// Spawn, register, and start supervising one background command.
///
/// Split out of [`tool_run_shell_background`] so that everything after the
/// permission gate — the confined spawn, the resource controller, the readers,
/// the exit watcher and the first projection — is one function a test can drive.
/// The alternative was a test that re-assembled those five pieces itself, which
/// is a test of the assembly it wrote rather than of the one that ships.
///
/// `limits` tightens the background-shell class defaults and can never loosen
/// them; the tool passes an empty set, and tests pass the bound they are about
/// to breach.
pub(crate) fn start_background_command<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    command: String,
    workspace_root: &std::path::Path,
    cwd_path: &std::path::Path,
    limits: crate::process_table::ProcessLimits,
) -> Result<BackgroundShellView, String> {
    let spawned =
        crate::workspace_shell::spawn_background(workspace_root, cwd_path, &command, limits)
            .map_err(|error| format!("Failed to spawn shell: {error}"))?;
    let identity = spawned.controller.root();
    let crate::workspace_shell::BackgroundSpawn {
        child,
        controller,
        stdout,
        stderr,
    } = spawned;

    let view = BackgroundShellView {
        id: uuid::Uuid::new_v4().to_string(),
        command,
        cwd: cwd_path.to_string_lossy().to_string(),
        status: BackgroundShellStatus::Running,
        exit_code: None,
        output: String::new(),
        output_truncated: false,
        started_at_ms: now_ms()?,
        finished_at_ms: None,
    };
    let process = Arc::new(BackgroundProcess {
        view: Mutex::new(view.clone()),
        child: Mutex::new(child),
        // Held for the life of the background process, not dropped at the end of
        // this function: on Windows the job handle *is* the containment, so
        // releasing it here would free the tree the moment the command was
        // registered. Nothing turn-scoped owns it either — this `Arc` lives in
        // the manager, which outlives every turn, which is what makes a
        // background command's bounds survive the turn that started it.
        controller: Mutex::new(controller),
        last_sample: Mutex::new(None),
        read_cursor: Mutex::new(0),
    });
    state
        .background_shell
        .insert(view.id.clone(), process.clone())?;

    if let Some(stdout) = stdout {
        spawn_reader(app.clone(), process.clone(), stdout);
    }
    if let Some(stderr) = stderr {
        spawn_reader(app.clone(), process.clone(), stderr);
    }
    // Ahead of the exit watcher rather than behind it, which is where this used to
    // be: the row has to exist before the ownership below can be attributed to it,
    // and a watcher that samples before the row is written was already recording
    // usage against nothing.
    emit_status(app, view.clone(), identity);
    // Every native process this command's supervisor observes is recorded against
    // that row from here on, so a descendant that leaves the process group after
    // being seen is still reclaimable after a restart — which for a background
    // command is the whole point: it is the kind most likely to outlive the turn
    // that started it.
    //
    // Fail-closed: a command whose ownership cannot be made durable is reclaimed
    // rather than left running as a tree this app could not find again.
    let journal = crate::bounded_execution::ProjectedOwnership::shared(
        crate::bounded_execution::AppProcessProjector::shared(app.clone()),
        crate::process_table::ProcessKind::BackgroundShell,
        view.id.clone(),
    );
    if let Err(error) = lock(&process.controller)
        .and_then(|mut controller| controller.persist_ownership_to(journal))
    {
        if let Ok(mut controller) = lock(&process.controller) {
            let _ = controller.terminate_tree();
        }
        if let Ok(mut child) = lock(&process.child) {
            let _ = child.kill();
        }
        // The registered entry is moved to `Error` rather than a clone of it: the
        // manager already holds this process, and a view that stays `Running`
        // would be a panel row for a command that was reclaimed before it began —
        // the same phantom the process table's `Drop` backstop exists to prevent.
        let ended = {
            let mut registered = lock(&process.view)?;
            registered.status = BackgroundShellStatus::Error;
            registered.finished_at_ms = Some(now_ms()?);
            registered.clone()
        };
        emit_status(app, ended, identity);
        return Err(format!(
            "Failed to record what this background command owns, so it was not run: {error}"
        ));
    }
    spawn_exit_watcher(app.clone(), process);
    Ok(view)
}

/// One `background_shell_output` reply: the output the caller has not seen
/// yet (in draining mode) plus the command's current lifecycle state.
#[derive(Clone, Debug, Serialize)]
pub struct BackgroundShellOutput {
    pub id: String,
    pub command: String,
    pub status: BackgroundShellStatus,
    pub exit_code: Option<i32>,
    pub output: String,
    /// True when the retained tail has dropped earlier bytes — the caller is
    /// looking at a window, not the whole history.
    pub output_truncated: bool,
}

/// Reads a background command's output. Defaults to draining: each call
/// returns only what has arrived since the previous call, which is what makes
/// polling a chatty watcher cheap for the model. Pass `drain: false` for the
/// full retained tail (what the UI renders) without moving the cursor.
#[tauri::command(rename_all = "snake_case")]
pub fn background_shell_output(
    state: tauri::State<'_, AppState>,
    id: String,
    drain: Option<bool>,
) -> Result<BackgroundShellOutput, String> {
    let process = state.background_shell.get(&id)?;
    let view = process.view()?;
    let output = if drain.unwrap_or(true) {
        let mut cursor = lock(&process.read_cursor)?;
        let start = (*cursor).min(view.output.len());
        let slice = view.output[start..].to_string();
        *cursor = view.output.len();
        slice
    } else {
        view.output.clone()
    };
    Ok(BackgroundShellOutput {
        id: view.id,
        command: view.command,
        status: view.status,
        exit_code: view.exit_code,
        output,
        output_truncated: view.output_truncated,
    })
}

/// Kills a running background command. Idempotent: killing an already-exited
/// command returns its current view instead of erroring, so a retry (or two
/// UI surfaces racing on the same card) is harmless.
#[tauri::command(rename_all = "snake_case")]
pub fn background_shell_kill(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<BackgroundShellView, String> {
    let process = state.background_shell.get(&id)?;
    {
        let current = process.view()?;
        if current.status != BackgroundShellStatus::Running {
            return Ok(current);
        }
    }
    {
        let mut child = lock(&process.child)?;
        child
            .kill()
            .map_err(|error| format!("Failed to kill background command: {error}"))?;
    }
    // And the tree, through the controller. `kill` above signals the process
    // group, which is the ordinary case and misses the one that matters: a
    // descendant that left the group is reachable only through the membership the
    // controller recorded while the ancestry was still readable. The user asked
    // for this command to stop, not for its leader to stop.
    reclaim_owned_tree(&process);
    let view = {
        let mut view = lock(&process.view)?;
        view.status = BackgroundShellStatus::Killed;
        view.finished_at_ms = now_ms().ok();
        view.clone()
    };
    emit_status(&app, view.clone(), None);
    Ok(view)
}

/// Every background command this app session knows about, oldest first — the
/// Background Tasks panel's initial load (events keep it current afterwards).
#[tauri::command(rename_all = "snake_case")]
pub fn background_shell_list(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<BackgroundShellView>, String> {
    state.background_shell.list()
}

/// Drops finished entries from the registry. Running commands are untouched.
#[tauri::command(rename_all = "snake_case")]
pub fn background_shell_clear_finished(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.background_shell.clear_finished()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_bounded_reports_dropped_prefix_once_over_cap() {
        let mut buffer = "a".repeat(MAX_OUTPUT_BYTES);
        let mut truncated = false;
        let dropped = append_bounded(&mut buffer, "bbbb", &mut truncated);
        assert_eq!(dropped, 4);
        assert!(truncated);
        assert_eq!(buffer.len(), MAX_OUTPUT_BYTES);
        assert!(buffer.ends_with("bbbb"));
    }

    #[test]
    fn append_bounded_keeps_buffer_whole_under_cap() {
        let mut buffer = String::from("hello");
        let mut truncated = false;
        assert_eq!(append_bounded(&mut buffer, " world", &mut truncated), 0);
        assert!(!truncated);
        assert_eq!(buffer, "hello world");
    }

    #[test]
    fn split_event_chunks_never_splits_a_code_point() {
        let chunk = "é".repeat(MAX_EVENT_CHUNK_BYTES);
        let pieces = split_event_chunks(&chunk);
        assert!(pieces.len() > 1);
        assert_eq!(pieces.concat(), chunk);
    }

    /// The sampling seam, against an in-memory ledger: a real pid's CPU time
    /// reaches the row, a row that is not there is not invented, and — the one
    /// that guards the double-count rule — sampling never writes the egress
    /// column, which belongs to the additive writer alone.
    #[test]
    fn sampling_reaches_the_ledger_row_without_touching_the_egress_column() {
        use crate::process_table::{AdmitProcess, ProcessKind, ProcessTable, ProcessUsageFilter};
        use crate::process_usage::ProcessUsageAccumulator;
        use crate::run_ledger::RunLedger;

        let ledger = RunLedger::open_in_memory().expect("an in-memory ledger opens");
        let table = ProcessTable::new(ledger.connection());
        let mut usage = ProcessUsageAccumulator::new();
        // This test process is the only pid a test can be certain about.
        usage.observe_pid(std::process::id() as i64);

        assert!(
            !record_usage(&table, "bg-never-projected", usage.sample(), 1_000)
                .expect("a missing row is not an error"),
            "a shell with no row yet must not have one invented for it"
        );

        let record = table
            .admit(
                &AdmitProcess::new(ProcessKind::BackgroundShell, "bg-sampled".to_string()),
                1_000,
            )
            .expect("a row is admitted");
        assert!(
            record_usage(&table, "bg-sampled", usage.sample(), 1_001).expect("the row takes it")
        );

        let row = table
            .usage_rows(&ProcessUsageFilter {
                process_id: Some(record.process_id.clone()),
                ..ProcessUsageFilter::default()
            })
            .expect("the ledger row reads")
            .pop()
            .expect("the row exists");
        assert!(
            row.usage.measured().cpu_time_ms.is_some(),
            "a live sample must reach the row; got {:?}",
            row.usage.unavailable()
        );
        assert_eq!(
            row.usage.measured().bytes_egressed,
            None,
            "the sampler must leave the egress column to `add_egress_bytes`, or the \
             two conventions double-count"
        );
    }

    #[cfg(unix)]
    fn process_state(pid: u32) -> String {
        let output = std::process::Command::new("ps")
            .args(["-o", "state=", "-p", &pid.to_string()])
            .output()
            .expect("ps runs");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// End-to-end through the same path `process_signal` drives: a real
    /// spawned child registered in `BackgroundShellManager`, a matching
    /// process-table row admitted and moved to `Running` (mirroring the real
    /// spawn -> `markProcessRunning` sequence), then `deliver_os_signal`
    /// itself. Proves the whole plumbing — manager lookup, real SIGSTOP/
    /// SIGCONT, and the process-table state transition — not just the bare
    /// primitive (see `os_signal.rs`'s own test for that).
    #[cfg(unix)]
    #[test]
    fn deliver_os_signal_suspends_and_resumes_the_real_child_and_updates_the_process_table() {
        use crate::process_table::{AdmitProcess, ProcessKind, ProcessSignal, ProcessState};
        use crate::run_ledger::RunLedger;
        use std::os::unix::process::CommandExt;

        let state = AppState::default();
        // Pre-seed an in-memory ledger: this test needs no durable rows,
        // and an in-memory one keeps it off disk entirely. (The mock app's
        // app-data directory is per-test either way — see `test_support`.)
        *state.run_ledger.lock().unwrap() = Some(RunLedger::open_in_memory().unwrap());
        let handle = crate::test_support::mock_app().handle().clone();

        let shell_id = "bg-signal-test".to_string();
        let child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("sleep spawns");
        let pid = child.id();
        std::thread::sleep(Duration::from_millis(50));

        let view = BackgroundShellView {
            id: shell_id.clone(),
            command: "sleep 30".to_string(),
            cwd: "/".to_string(),
            status: BackgroundShellStatus::Running,
            exit_code: None,
            output: String::new(),
            output_truncated: false,
            started_at_ms: 0,
            finished_at_ms: None,
        };
        state
            .background_shell
            .insert(
                shell_id.clone(),
                Arc::new(BackgroundProcess {
                    view: Mutex::new(view),
                    child: Mutex::new(
                        crate::workspace_shell::BackgroundShellChild::unconfined_for_lifecycle_test(
                            child,
                        ),
                    ),
                    // Unbounded on purpose: this test asserts the *lifecycle*
                    // (suspend, resume, exit), and a limit here would add a
                    // second reason the child could disappear.
                    controller: Mutex::new(crate::resource_control::ResourceController::new(
                        crate::resource_control::EffectiveLimits::default(),
                    )),
                    last_sample: Mutex::new(None),
                    read_cursor: Mutex::new(0),
                }),
            )
            .unwrap();

        let record = crate::process_commands::with_process_table(&handle, &state, |table| {
            table.admit(
                &AdmitProcess::new(ProcessKind::BackgroundShell, shell_id.clone()),
                1_000,
            )
        })
        .unwrap();
        let record = crate::process_commands::with_process_table(&handle, &state, |table| {
            table.transition(&record.process_id, ProcessState::Running, None, 1_001)
        })
        .unwrap();

        let suspended = deliver_os_signal(&handle, &state, &record, ProcessSignal::Suspend)
            .expect("suspend delivers");
        assert_eq!(suspended.state, ProcessState::Suspended);
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            process_state(pid).starts_with('T'),
            "expected the real child to be OS-stopped, got {:?}",
            process_state(pid)
        );

        let resumed = deliver_os_signal(&handle, &state, &suspended, ProcessSignal::Resume)
            .expect("resume delivers");
        assert_eq!(resumed.state, ProcessState::Running);
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !process_state(pid).starts_with('T'),
            "expected the real child to be running again, got {:?}",
            process_state(pid)
        );

        let _ = state
            .background_shell
            .get(&shell_id)
            .unwrap()
            .child
            .lock()
            .unwrap()
            .kill();
    }

    // --- K4 end-to-end: a background command's bounds fire in production -----
    //
    // Everything below drives `start_background_command`, which is what the
    // `run_shell(run_in_background: true)` tool calls once its permission gate
    // has passed. So each test crosses the whole path: the confined spawn, the
    // real `ResourceController`, a real child and grandchild, the real exit
    // watcher thread, breach detection, whole-tree termination, and the typed
    // `LimitBreach` on the process-table row.
    //
    // They exist because the unit tests could not have caught the bug they now
    // guard: the background watcher sampled and compared, which is the right
    // test for a supervised bound and is never true for a kernel-held one, so a
    // command a real cgroup killed was recorded as an unexplained error.
    #[cfg(unix)]
    mod end_to_end {
        use super::*;
        use crate::process_table::{ExitStatus, ProcessKind, ProcessLimits, ProcessState};
        use crate::run_ledger::RunLedger;
        use tauri::Manager;

        struct TestTree(std::path::PathBuf);

        impl TestTree {
            fn create() -> Self {
                let path = std::env::temp_dir().join(format!(
                    "little-monkey-bg-limits-{}",
                    uuid::Uuid::new_v4().simple()
                ));
                std::fs::create_dir(&path).expect("create test tree");
                Self(path)
            }
        }

        impl Drop for TestTree {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        /// Same gate the foreground suite uses: a host without the confinement
        /// backend cannot run a confined shell at all, and CI must have one.
        fn confinement_available() -> bool {
            if crate::sandbox::sandbox_enforcement()
                == crate::sandbox::SandboxEnforcement::OsEnforced
            {
                return true;
            }
            assert!(
                std::env::var_os("CI").is_none(),
                "CI platform did not provide its required shell confinement backend"
            );
            false
        }

        fn quote(path: &std::path::Path) -> String {
            format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
        }

        /// The workload has to be executable from *inside* the grant, so the
        /// test binary is copied in. Running it from the build tree would be
        /// denied by the confinement and the command would exit instantly — a
        /// pass for the wrong reason.
        fn place_test_binary_in(workspace: &std::path::Path, name: &str) -> std::path::PathBuf {
            let placed = workspace.join(name);
            let current = std::env::current_exe().expect("the test binary knows its own path");
            std::fs::hard_link(&current, &placed)
                .or_else(|_| std::fs::copy(&current, &placed).map(|_| ()))
                .expect("place the workload binary inside the selected workspace");
            placed
        }

        struct Harness {
            _app: tauri::App<tauri::test::MockRuntime>,
            handle: tauri::AppHandle<tauri::test::MockRuntime>,
            _tree: TestTree,
            workspace: std::path::PathBuf,
        }

        impl Harness {
            /// A mock app that *manages* `AppState`, because the production
            /// projection reaches for it through `app.state()` rather than being
            /// handed one.
            fn create() -> Self {
                let app = crate::test_support::build(
                    tauri::test::mock_builder().manage(AppState::default()),
                );
                let handle = app.handle().clone();
                *handle.state::<AppState>().run_ledger.lock().unwrap() =
                    Some(RunLedger::open_in_memory().expect("an in-memory ledger"));
                let tree = TestTree::create();
                let workspace = tree.0.join("workspace");
                std::fs::create_dir(&workspace).expect("create workspace");
                let workspace =
                    crate::sandbox::plain_canonical(&workspace).expect("canonical workspace");
                Harness {
                    _app: app,
                    handle,
                    _tree: tree,
                    workspace,
                }
            }

            fn start(
                &self,
                command: &str,
                limits: ProcessLimits,
            ) -> Result<BackgroundShellView, String> {
                let state = self.handle.state::<AppState>();
                start_background_command(
                    &self.handle,
                    state.inner(),
                    command.to_string(),
                    &self.workspace,
                    &self.workspace,
                    limits,
                )
            }

            fn row(&self, external_id: &str) -> Option<crate::process_table::ProcessRecord> {
                let state = self.handle.state::<AppState>();
                crate::process_commands::with_process_table(&self.handle, state.inner(), |table| {
                    table.find_by_external_id(ProcessKind::BackgroundShell, external_id)
                })
                .expect("the process table reads")
            }

            /// Waits for the watcher thread to close the row out.
            ///
            /// Polls rather than sleeps a fixed time: the watcher ticks every
            /// 100 ms and a memory bound needs a sample or two, so a fixed sleep
            /// is either slow or flaky. The ceiling is generous for CI.
            fn wait_for_exit(&self, external_id: &str) -> crate::process_table::ProcessRecord {
                let deadline = std::time::Instant::now() + Duration::from_secs(60);
                loop {
                    if let Some(record) = self.row(external_id) {
                        if record.state == ProcessState::Exited {
                            return record;
                        }
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the background command never reached a terminal row"
                    );
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }

        /// A stored breach must name a real mechanism, and be judged the way
        /// that mechanism actually reports.
        ///
        /// Kernel and supervised report a firing bound differently, and that
        /// difference is the whole distinction: a supervisor finds a breach by
        /// comparison, so the observation is above the budget; a kernel exists so
        /// the observation never passes it, and carries the refusal counter
        /// instead.
        fn assert_real_backend(breach: &crate::resource_control::LimitBreach) {
            assert!(
                !breach.backend.is_empty(),
                "a breach must name the mechanism that made it: {breach:?}"
            );
            if let Some(required) = crate::resource_control::required_backend() {
                assert_eq!(
                    breach.backend, required,
                    "this host was provisioned to exercise {required}: {breach:?}"
                );
            }
            match breach.level.as_str() {
                "kernel" => {
                    assert!(
                        breach.observed <= breach.configured,
                        "a kernel bound holds the measurement at the cap: {breach:?}"
                    );
                    assert!(
                        breach.evidence.is_some(),
                        "a kernel breach must carry the refusal counter that found it: \
                         {breach:?}"
                    );
                }
                "supervised" => assert!(
                    breach.observed > breach.configured,
                    "a supervised bound is found by comparison: {breach:?}"
                ),
                other => {
                    panic!("a limit on this process must be kernel or supervised, not {other}")
                }
            }
        }

        /// The tree budget the memory tests install, and the hog they run against
        /// it.
        ///
        /// Three constraints fix these numbers, and the third is why they are not
        /// larger:
        ///
        /// - **Above the baseline.** The workload is this test binary re-executed,
        ///   which resides at roughly 17 MiB before it allocates anything. The
        ///   ceiling clears that several times over, so the binary's own footprint
        ///   can never be the thing that trips the bound — a pass for the wrong
        ///   reason, and indistinguishable from the real one in the row.
        /// - **Held, not grazed.** The hog touches every page and then sleeps 30
        ///   seconds, so the tree sits at about twice the ceiling for that whole
        ///   window. A 100 ms sampler cannot miss it.
        /// - **Not the largest thing on the host.** A workload that makes itself
        ///   the machine's biggest memory user invites the OS's own low-memory
        ///   killer to end the tree first, and that arrives as an ordinary exit
        ///   rather than as this budget firing — a green enforcement path
        ///   reported as a plain failure. Halving the peak keeps the test out of
        ///   that role on a loaded machine.
        const MEMORY_CEILING: u64 = 128 * 1024 * 1024;
        const HOG_MIB: usize = 256;

        /// A: a grandchild outgrows the budget, the whole tree goes, and the row
        /// says `limit_exceeded` with both numbers.
        #[test]
        fn a_background_grandchild_past_the_memory_budget_ends_the_tree_and_types_the_exit() {
            if !confinement_available() {
                return;
            }
            let harness = Harness::create();
            let hog = format!(
                "LITTLE_MONKEY_MEMORY_HOG_MIB={HOG_MIB} {} --exact \
                 workspace_shell::tests::memory_hog_child --test-threads=1 >/dev/null 2>&1",
                quote(&place_test_binary_in(&harness.workspace, "bg-memory-hog"))
            );
            let view = harness
                .start(
                    &hog,
                    ProcessLimits {
                        max_memory_bytes: Some(MEMORY_CEILING),
                        ..ProcessLimits::default()
                    },
                )
                .expect("the background command starts");
            let native_pid = self::native_pid_of(&harness, &view.id);

            let record = harness.wait_for_exit(&view.id);
            let exit = record.exit.expect("a terminal row carries its exit");
            assert_eq!(
                exit.status,
                ExitStatus::LimitExceeded,
                "a memory kill must not be recorded as an ordinary failure: {exit:?}"
            );
            let breach = exit.breach.expect("a limit kill carries its typed breach");
            assert_eq!(breach.limit, "max_memory_bytes");
            assert_eq!(breach.configured, MEMORY_CEILING);
            assert_real_backend(&breach);
            assert!(
                crate::process_tree::measure_tree(native_pid)
                    .expect("snapshot")
                    .is_none(),
                "the whole owned tree must be gone once its budget fired"
            );
        }

        /// B: a fork-heavy background tree hits its own process ceiling.
        #[test]
        fn a_background_tree_past_its_process_ceiling_ends_and_types_the_exit() {
            if !confinement_available() {
                return;
            }
            let harness = Harness::create();
            let host_processes = crate::process_tree::snapshot().expect("snapshot").len();
            assert!(
                host_processes > 12,
                "this assertion only means something on a host with real load: {host_processes}"
            );
            let view = harness
                .start(
                    "i=0; while [ $i -lt 40 ]; do sleep 20 & i=$((i+1)); done; wait",
                    ProcessLimits {
                        max_child_processes: Some(12),
                        ..ProcessLimits::default()
                    },
                )
                .expect("the background command starts");
            let native_pid = self::native_pid_of(&harness, &view.id);

            let record = harness.wait_for_exit(&view.id);
            let exit = record.exit.expect("a terminal row carries its exit");
            assert_eq!(exit.status, ExitStatus::LimitExceeded, "{exit:?}");
            let breach = exit.breach.expect("a limit kill carries its typed breach");
            assert_eq!(breach.limit, "max_child_processes");
            assert_eq!(breach.configured, 12);
            assert_real_backend(&breach);
            assert!(
                breach.observed < u64::try_from(host_processes).unwrap(),
                "the count must be of the owned tree, not of everything this uid owns: {breach:?}"
            );
            assert!(crate::process_tree::measure_tree(native_pid)
                .expect("snapshot")
                .is_none());
        }

        /// C: the counter-test. Without it, "every background command is a
        /// resource violation" would satisfy A, B and D.
        #[test]
        fn a_background_command_inside_every_bound_finishes_with_no_breach() {
            if !confinement_available() {
                return;
            }
            let harness = Harness::create();
            let view = harness
                .start("printf ok", ProcessLimits::default())
                .expect("the background command starts");

            let record = harness.wait_for_exit(&view.id);
            let exit = record.exit.expect("a terminal row carries its exit");
            assert_eq!(
                exit.status,
                ExitStatus::Succeeded,
                "a command that finished on its own terms is not a limit kill: {exit:?}"
            );
            assert!(exit.breach.is_none(), "{exit:?}");
        }

        /// C2: a command that fails on its own is `Failed`, never
        /// `LimitExceeded`. The other half of the counter-test — a watcher that
        /// asked the mechanism and believed any answer would pass C and fail
        /// this.
        #[test]
        fn a_background_command_that_fails_on_its_own_is_not_a_resource_kill() {
            if !confinement_available() {
                return;
            }
            let harness = Harness::create();
            let view = harness
                .start("exit 3", ProcessLimits::default())
                .expect("the background command starts");

            let record = harness.wait_for_exit(&view.id);
            let exit = record.exit.expect("a terminal row carries its exit");
            assert_eq!(exit.status, ExitStatus::Failed, "{exit:?}");
            assert!(exit.breach.is_none(), "{exit:?}");
            assert_eq!(exit.code, Some(3));
        }

        /// D: the terminal-before-sample race, and the reason the exit path asks
        /// the mechanism before it reads the exit status.
        ///
        /// With a kernel process ceiling of one, the shell's very first `fork` is
        /// refused and it dies within milliseconds — long before the watcher's
        /// 100 ms tick could sample anything. The old code reached `try_wait`
        /// first and recorded an ordinary non-zero exit; the limit had worked and
        /// the app could not say so.
        ///
        /// Only meaningful where a kernel holds the bound. On a supervised host
        /// there is no mechanism to ask and a process that dies before the first
        /// comparison genuinely died on its own — recording that as a limit kill
        /// would be the opposite lie.
        #[test]
        fn a_kernel_refusal_before_the_first_sample_is_still_a_limit_kill() {
            if !confinement_available() {
                return;
            }
            let probe = crate::resource_control::ResourceController::new(
                crate::workspace_shell::effective_shell_limits(
                    ProcessKind::BackgroundShell,
                    ProcessLimits {
                        max_child_processes: Some(1),
                        ..ProcessLimits::default()
                    },
                ),
            );
            if probe.capabilities().child_processes.level()
                != Some(crate::resource_control::EnforcementLevel::Kernel)
            {
                return;
            }
            drop(probe);

            let harness = Harness::create();
            let view = harness
                .start(
                    // One fork, refused at once, and then the shell is done.
                    "sleep 30 &",
                    ProcessLimits {
                        max_child_processes: Some(1),
                        ..ProcessLimits::default()
                    },
                )
                .expect("the background command starts");

            let record = harness.wait_for_exit(&view.id);
            let exit = record.exit.expect("a terminal row carries its exit");
            assert_eq!(
                exit.status,
                ExitStatus::LimitExceeded,
                "a kernel that refused the fork must not read as an ordinary failure: {exit:?}"
            );
            let breach = exit.breach.expect("a limit kill carries its typed breach");
            assert_eq!(breach.limit, "max_child_processes");
            assert_eq!(breach.level, "kernel");
            assert!(breach.evidence.is_some(), "{breach:?}");
        }

        fn native_pid_of(harness: &Harness, external_id: &str) -> u32 {
            let record = harness
                .row(external_id)
                .expect("the spawn projects a row before it returns");
            u32::try_from(
                record
                    .native_pid
                    .expect("a background shell's row carries its native pid"),
            )
            .expect("a pid fits in u32")
        }
    }
}
