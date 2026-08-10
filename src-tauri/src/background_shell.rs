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
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    child: Mutex<Child>,
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
        }
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
fn emit_status(app: &tauri::AppHandle, view: BackgroundShellView, native_pid: Option<i64>) {
    project_process(app, &view, native_pid);
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
fn project_process(app: &tauri::AppHandle, view: &BackgroundShellView, native_pid: Option<i64>) {
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
            }),
        ),
        BackgroundShellStatus::Killed => (
            ProcessState::Exited,
            Some(ProcessExit {
                status: ExitStatus::Cancelled,
                code: view.exit_code,
                signal: None,
                reason: Some("killed".to_string()),
            }),
        ),
        BackgroundShellStatus::Error => (
            ProcessState::Exited,
            Some(ProcessExit {
                status: ExitStatus::Failed,
                code: view.exit_code,
                signal: None,
                reason: None,
            }),
        ),
    };

    let mut projection =
        ProcessProjection::new(ProcessKind::BackgroundShell, view.id.clone(), state)
            .with_workspace(Some(view.cwd.clone()))
            .with_native_pid(native_pid);
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
fn flush_usage(
    app: &tauri::AppHandle,
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
fn spawn_reader<R: Read + Send + 'static>(
    app: tauri::AppHandle,
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
fn spawn_exit_watcher(app: tauri::AppHandle, process: Arc<BackgroundProcess>) {
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
                    ticks = ticks.wrapping_add(1);
                    if ticks % USAGE_FLUSH_TICKS == 0 {
                        flush_usage(&app, &external_id, &usage);
                    }
                    std::thread::sleep(EXIT_POLL_INTERVAL)
                }
                Ok(Some(status)) => {
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
                    // Same ordering rule as the exit branch above.
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

    let cwd_path = match cwd {
        Some(ref value) => workspace::resolve_path_and_root(state.inner(), value)?.0,
        None => workspace::primary_root_canon(state.inner())?,
    };

    #[cfg(target_os = "windows")]
    let (shell, shell_flag) = ("cmd", "/C");
    #[cfg(not(target_os = "windows"))]
    let (shell, shell_flag) = ("sh", "-c");

    let mut command_builder = Command::new(shell);
    command_builder
        .arg(shell_flag)
        .arg(&command)
        .current_dir(&cwd_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group, so a later suspend/resume/kill-by-group
    // (`os_signal::suspend_process_group` et al.) targets exactly this
    // command's tree rather than whatever group this app itself runs in —
    // mirrors the daemon's own job spawn (`daemon/engine.rs`).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command_builder.process_group(0);
    }
    // Core dumps refused and nothing else, installed between `fork` and `exec`
    // like the other three spawn sites — `apply_std` because this builder is
    // `std::process::Command`, not tokio's. No file-size or descriptor ceiling
    // here on purpose: this child is *meant* to outlive the call that spawned it,
    // so a number for either would be a judgement about what a command nobody
    // has classified is for, which is the process class K4 still lacks. Refusing
    // core dumps carries no such judgement — a dev server that segfaults should
    // not drop gigabytes into the workspace it was started in, and unlike the
    // foreground tool there is no timeout here to end it.
    crate::os_limits::apply_std(
        crate::os_limits::ChildLimits::baseline(),
        &mut command_builder,
    );
    let mut child = command_builder
        .spawn()
        .map_err(|error| format!("Failed to spawn shell: {error}"))?;
    let native_pid = i64::try_from(child.id()).ok();

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

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
    spawn_exit_watcher(app.clone(), process);
    emit_status(&app, view.clone(), native_pid);
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
                    child: Mutex::new(child),
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
}
