use std::collections::HashMap;
use std::fs::OpenOptions;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use little_monkey_lib::run_protocol::{
    ClientIdentity, ClientKind, PermissionDecision, RepositoryPolicy, RunEvent, RunStatus,
};

use crate::durable_run::{bounded_text, CliRunEventSink, DurableRunRecorder};

use super::ledger::{LeaseToken, SharedLedger};
use super::store::{map_run_status, DaemonConfig, DaemonJob, DaemonPaths, DaemonStore, JobState};
use super::worktree::OwnedWorktree;

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_millis()).ok())
            .unwrap_or(1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonNotification {
    pub run_id: String,
    pub title: String,
    pub body: String,
}

pub trait NotificationAdapter: Send + Sync {
    fn notify(&self, notification: &DaemonNotification) -> Result<(), String>;
}

pub struct OsNotificationAdapter;

impl NotificationAdapter for OsNotificationAdapter {
    fn notify(&self, notification: &DaemonNotification) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        {
            let script = format!(
                "display notification {} with title {}",
                apple_script_string(&notification.body),
                apple_script_string(&notification.title)
            );
            return command_ok("osascript", &["-e", &script]);
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            return command_ok("notify-send", &[&notification.title, &notification.body]);
        }
        #[cfg(windows)]
        {
            let script = format!(
                "$ws=New-Object -ComObject WScript.Shell; $null=$ws.Popup({},5,{},64)",
                powershell_string(&notification.body),
                powershell_string(&notification.title)
            );
            return command_ok(
                "powershell",
                &["-NoProfile", "-NonInteractive", "-Command", &script],
            );
        }
    }
}

#[cfg(target_os = "macos")]
fn apple_script_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(windows)]
fn powershell_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn command_ok(program: &str, args: &[&str]) -> Result<(), String> {
    Command::new(program)
        .args(args)
        .status()
        .map_err(|error| format!("Failed to launch notification adapter: {error}"))
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(format!("Notification adapter exited with {status}"))
            }
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessSignal {
    Pause,
    Resume,
    /// Cooperative wind-down: TERM, a grace period, then KILL if it is ignored.
    Terminate,
    /// Immediate termination, no grace period — what a durable `kill` latch
    /// asks for and a `stop` does not. Kept distinct from `Terminate` because
    /// collapsing them is what made `kill` unobservable in the first place.
    Kill,
}

pub trait ManagedProcess: Send {
    fn id(&self) -> u32;
    fn try_wait(&mut self) -> Result<Option<i32>, String>;
    fn signal(&mut self, signal: ProcessSignal) -> Result<(), String>;
    fn memory_bytes(&self) -> Result<Option<u64>, String>;
}

pub trait ProcessAdapter: Send + Sync {
    fn spawn(
        &self,
        job: &DaemonJob,
        paths: &DaemonPaths,
    ) -> Result<Box<dyn ManagedProcess>, String>;
    fn terminate_orphan(&self, process_id: u32) -> Result<(), String>;
}

pub struct RealProcessAdapter {
    executable: std::path::PathBuf,
}

impl RealProcessAdapter {
    pub fn current() -> Result<Self, String> {
        Ok(Self {
            executable: std::env::current_exe()
                .map_err(|error| format!("Could not resolve monkey executable: {error}"))?,
        })
    }
}

impl ProcessAdapter for RealProcessAdapter {
    fn spawn(
        &self,
        job: &DaemonJob,
        paths: &DaemonPaths,
    ) -> Result<Box<dyn ManagedProcess>, String> {
        let log_path = paths.logs.join(format!("{}.log", job.job_id));
        let log = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)
            .map_err(|error| format!("Failed to open daemon run log: {error}"))?;
        super::store::restrict_file(&log_path)?;
        let stderr = log
            .try_clone()
            .map_err(|error| format!("Failed to clone daemon log handle: {error}"))?;
        let mut command = Command::new(&self.executable);
        command
            .arg("--no-rules")
            .arg("task")
            .arg("run")
            .arg(&job.recipe_snapshot)
            .arg("--run-key")
            .arg(format!("daemon:{}", job.job_id))
            .arg("--json")
            .env_remove("LITTLE_MONKEY_TASK_QUEUE_ONLY")
            .env("LITTLE_MONKEY_DAEMON_APPROVAL_WAIT", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
        if let Some(policy) = &job.repository_policy_json {
            command.env("LITTLE_MONKEY_DAEMON_REPOSITORY_POLICY_JSON", policy);
            let allow_external = serde_json::from_str::<RepositoryPolicy>(policy)
                .map(|policy| {
                    policy.allow_push
                        || policy.allow_create_pull_request
                        || policy.allow_review_comment
                })
                .unwrap_or(false);
            if allow_external {
                command.env("LITTLE_MONKEY_DAEMON_ALLOW_EXTERNAL_MUTATIONS", "1");
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command
            .spawn()
            .map_err(|error| format!("Failed to launch daemon task child: {error}"))?;
        Ok(Box::new(RealManagedProcess { child }))
    }

    fn terminate_orphan(&self, process_id: u32) -> Result<(), String> {
        terminate_process_group(process_id)
    }
}

struct RealManagedProcess {
    child: Child,
}

impl ManagedProcess for RealManagedProcess {
    fn id(&self) -> u32 {
        self.child.id()
    }

    fn try_wait(&mut self) -> Result<Option<i32>, String> {
        self.child
            .try_wait()
            .map(|status| status.map(exit_code))
            .map_err(|error| format!("Failed to poll daemon task child: {error}"))
    }

    fn signal(&mut self, signal: ProcessSignal) -> Result<(), String> {
        match signal {
            ProcessSignal::Pause => little_monkey_lib::os_signal::suspend_process_group(self.child.id()),
            ProcessSignal::Resume => little_monkey_lib::os_signal::resume_process_group(self.child.id()),
            ProcessSignal::Terminate => terminate_process_group(self.child.id()),
            ProcessSignal::Kill => kill_process_group(self.child.id()),
        }
    }

    fn memory_bytes(&self) -> Result<Option<u64>, String> {
        process_memory_bytes(self.child.id())
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(128)
}

#[cfg(unix)]
fn terminate_process_group(process_id: u32) -> Result<(), String> {
    let group = format!("-{process_id}");
    let _ = command_ok("kill", &["-TERM", &group]);
    for _ in 0..40 {
        if !super::service::process_alive(process_id) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = command_ok("kill", &["-KILL", &group]);
    Ok(())
}

#[cfg(windows)]
fn terminate_process_group(process_id: u32) -> Result<(), String> {
    command_ok("taskkill", &["/PID", &process_id.to_string(), "/T", "/F"])
}

/// Whether this job has another attempt coming, per its kind's declared policy.
///
/// Two things had to agree and did not: the daemon's own `max_attempts` column
/// (per job, set at submission) and `ProcessKind::restart_policy` (per kind, the
/// declared rule). The stricter of the two wins, so a job cannot out-live the
/// kind's ceiling by asking for more attempts at submission, and the kind cannot
/// force retries onto a job explicitly submitted with `max_attempts: 1`.
fn retry_permitted(job: &DaemonJob) -> bool {
    use little_monkey_lib::process_table::ProcessKind;
    ProcessKind::DaemonJob.restart_policy().permits_retry(job.attempt)
        && job.attempt.saturating_add(1) < job.max_attempts
}

/// Whether a queued retry has waited out its backoff.
///
/// Derived from `updated_at_ms` rather than a new column: a retry transitions
/// the job to `queued` and stamps that field, and nothing else touches a queued
/// job except a pause or cancel request — both of which are read before this
/// point anyway. That keeps bounded backoff free of a daemon-store schema
/// change, at the cost of a pause/resume during backoff restarting the wait,
/// which is harmless.
fn backoff_elapsed(job: &DaemonJob, now: u64) -> bool {
    use little_monkey_lib::process_table::ProcessKind;
    if job.attempt == 0 {
        return true;
    }
    let wait = ProcessKind::DaemonJob
        .restart_policy()
        .backoff_ms(job.attempt.saturating_sub(1));
    now >= job.updated_at_ms.saturating_add(wait)
}

/// Which attempt of the job the row for its *current* state belongs to.
///
/// Not `job.attempt` directly, because that column counts **starts**: the store
/// increments it on the transition into `running`, so a job that has never
/// retried reads `0` while queued and `1` while running. Keying the process row
/// off it raw would mint a fresh row every time a job merely started. What the
/// row identifies is the attempt itself, which is one behind the counter once
/// that attempt is underway, and equal to it while the next one is still
/// waiting to start.
///
/// A `running` job with `attempt == 0` is not reachable through the store, but
/// `saturating_sub` keeps a hand-edited or future-recovered row on attempt 0
/// rather than panicking over it.
fn attempt_ordinal(job: &DaemonJob) -> u32 {
    match job.state {
        // The attempt about to start.
        JobState::Preparing | JobState::Queued => job.attempt,
        // The attempt that started.
        _ => job.attempt.saturating_sub(1),
    }
}

/// The process-table identity of one attempt of a daemon job.
///
/// Attempt-scoped because `agent_processes` models *processes*, and a retry is a
/// new one: a new spawn, a new pid, its own exit. The table enforces
/// `UNIQUE(kind, external_id)` and `admit` refuses a second row under an id it
/// already holds, so under a bare `job_id` a retry could never get its own row —
/// it would keep reusing the first attempt's, which the state machine then
/// refuses to move backwards from `running` to `admitted` when the job requeues.
/// Scoping the id lets the retry be what it always was.
pub(super) fn process_external_id(job_id: &str, attempt: u32) -> String {
    format!("{job_id}#{attempt}")
}

/// Splits an external id back into `(job_id, attempt)`.
///
/// The attempt is `None` for a row that predates attempt scoping, and for a job
/// id that happens to contain a `#` without a numeric tail — job ids are
/// generated as `job-<uuid>` but `--job-id` lets a caller supply their own, so
/// the suffix is only taken when it actually parses as an attempt number.
fn split_external_id(external_id: &str) -> (&str, Option<u32>) {
    match external_id.rsplit_once('#') {
        Some((job_id, attempt)) => match attempt.parse::<u32>() {
            Ok(attempt) => (job_id, Some(attempt)),
            Err(_) => (external_id, None),
        },
        None => (external_id, None),
    }
}

#[cfg(unix)]
fn kill_process_group(process_id: u32) -> Result<(), String> {
    little_monkey_lib::os_signal::kill_process_group(process_id)
}

/// Windows has no softer option to skip: `taskkill /F` is already immediate, so
/// `Terminate` and `Kill` genuinely coincide here. Stated rather than left to
/// look like an oversight.
#[cfg(windows)]
fn kill_process_group(process_id: u32) -> Result<(), String> {
    terminate_process_group(process_id)
}

#[cfg(unix)]
fn process_memory_bytes(process_id: u32) -> Result<Option<u64>, String> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &process_id.to_string()])
        .output()
        .map_err(|error| format!("Failed to inspect process memory: {error}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    let kib = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .ok();
    Ok(kib.and_then(|value| value.checked_mul(1024)))
}

#[cfg(windows)]
fn process_memory_bytes(process_id: u32) -> Result<Option<u64>, String> {
    let script = format!("(Get-Process -Id {process_id} -ErrorAction Stop).WorkingSet64");
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .map_err(|error| format!("Failed to inspect process memory: {error}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .ok())
}

struct ActiveProcess {
    process: Box<dyn ManagedProcess>,
    lease: LeaseToken,
}

/// `JobState` → the unified [`ProcessState`].
///
/// Deliberately lossy in one direction: `WaitingApproval` and `Cancelling` are
/// both `Running`, because from an arbitration point of view the process exists
/// and still holds its reservations. The distinction stays in `daemon_jobs`,
/// which is the record that owns it.
fn process_state_for(state: JobState) -> little_monkey_lib::process_table::ProcessState {
    use little_monkey_lib::process_table::ProcessState;
    match state {
        JobState::Preparing | JobState::Queued => ProcessState::Admitted,
        JobState::Running | JobState::WaitingApproval | JobState::Cancelling => {
            ProcessState::Running
        }
        JobState::Paused => ProcessState::Suspended,
        JobState::Succeeded
        | JobState::Failed
        | JobState::Cancelled
        | JobState::NeedsReconciliation => ProcessState::Exited,
    }
}

/// Terminal `JobState` → the unified exit. A non-terminal state reaching here
/// means the job vanished from the non-terminal set without a terminal state,
/// which is exactly what `Lost` is for.
fn exit_for(
    state: JobState,
    last_error: Option<&str>,
) -> little_monkey_lib::process_table::ProcessExit {
    use little_monkey_lib::process_table::{ExitStatus, ProcessExit};
    let status = match state {
        JobState::Succeeded => ExitStatus::Succeeded,
        JobState::Failed => ExitStatus::Failed,
        JobState::Cancelled => ExitStatus::Cancelled,
        JobState::NeedsReconciliation => ExitStatus::NeedsReconciliation,
        _ => ExitStatus::Lost,
    };
    ProcessExit {
        status,
        code: None,
        signal: None,
        reason: last_error.map(str::to_string),
    }
}

pub struct DaemonEngine<P, N, C> {
    pub store: DaemonStore,
    pub shared: SharedLedger,
    paths: DaemonPaths,
    config: DaemonConfig,
    process_adapter: P,
    notifier: N,
    clock: C,
    owner_id: String,
    active: HashMap<String, ActiveProcess>,
    last_retention_ms: u64,
    /// Jobs whose durable latch asked for `kill` rather than `stop`, so the
    /// terminator skips the grace period.
    ///
    /// In-memory on purpose. The latch in `agent_processes` is the durable
    /// record; this is only the current tick's reading of it, refreshed by
    /// `apply_signal_intent` every tick, so a restart re-derives it rather than
    /// carrying a second copy that could drift from the one that matters.
    immediate_termination: std::collections::HashSet<String>,
}

impl<P: ProcessAdapter, N: NotificationAdapter, C: Clock> DaemonEngine<P, N, C> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: DaemonStore,
        shared: SharedLedger,
        paths: DaemonPaths,
        config: DaemonConfig,
        process_adapter: P,
        notifier: N,
        clock: C,
        owner_id: String,
    ) -> Self {
        Self {
            store,
            shared,
            paths,
            config,
            process_adapter,
            notifier,
            clock,
            owner_id,
            active: HashMap::new(),
            last_retention_ms: 0,
            immediate_termination: std::collections::HashSet::new(),
        }
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    pub fn recover(&mut self) -> Result<(), String> {
        let now = self.clock.now_ms();
        for job in self.store.active_jobs()? {
            if let Some(process_id) = job.process_id {
                // A service crash can leave the task child detached. Stop the
                // whole process group before deciding from durable events;
                // never run a second copy alongside an orphan.
                let _ = self.process_adapter.terminate_orphan(process_id);
            }
            self.reconcile_interrupted(&job, now, "daemon restarted while the run was active")?;
        }
        self.store.set_meta("last_recovery_ms", &now.to_string())?;
        Ok(())
    }

    pub fn tick(&mut self) -> Result<(), String> {
        let now = self.clock.now_ms();
        self.store.set_meta("heartbeat_ms", &now.to_string())?;
        self.store
            .set_meta("pid", &std::process::id().to_string())?;
        if self.store.kill_switch()? {
            self.store.request_cancel_all(now)?;
        }

        // Read durable signal intent *before* `tick_active`, not after. It used to
        // sit with the projection at the end of the tick, which meant a latched
        // stop was translated into the daemon's own bits only after the loop that
        // acts on them had already run — costing a whole extra poll interval
        // before anything happened.
        if let Err(error) = self.apply_signal_intent_from_table(now) {
            eprintln!("monkey daemon: could not read signal intent: {error}");
        }

        let ids = self.active.keys().cloned().collect::<Vec<_>>();
        for job_id in ids {
            self.tick_active(&job_id, now)?;
        }

        if !self.store.kill_switch()? {
            let available = usize::try_from(self.config.concurrency)
                .unwrap_or(usize::MAX)
                .saturating_sub(self.active.len());
            if available > 0 {
                for job in self
                    .store
                    .ready_jobs(u32::try_from(available).unwrap_or(u32::MAX))?
                {
                    // A retry waits out its backoff. Skipping rather than
                    // sleeping keeps the tick non-blocking, so one backing-off
                    // job never delays every other queued one — it is simply
                    // passed over until a later tick.
                    if !backoff_elapsed(&job, now) {
                        continue;
                    }
                    self.start_job(job, now)?;
                }
            }
        } else {
            self.cancel_queued(now, "global kill switch is engaged")?;
        }

        // Reconciled once per tick from whatever the job store now says, rather
        // than mirrored at each of the a dozen-odd places a job's state
        // changes. One pass cannot miss a call site, and it is idempotent, so a
        // daemon restart converges instead of forking a second record.
        self.sync_process_table(now)?;

        if now.saturating_sub(self.last_retention_ms) >= 60 * 60 * 1_000 {
            self.apply_retention(now)?;
            self.last_retention_ms = now;
        }
        Ok(())
    }

    /// Project every daemon job onto the unified process table.
    ///
    /// A failure here is logged and swallowed: the process table is an
    /// observability and arbitration surface, and a job must never fail to run
    /// because its projection could not be written. The one thing that would be
    /// worse than a missing row is a job that stops working to protect one.
    fn sync_process_table(&mut self, now: u64) -> Result<(), String> {
        if let Err(error) = self.sync_process_table_inner(now) {
            eprintln!("monkey daemon: process table sync failed: {error}");
        }
        Ok(())
    }

    /// Copies durable signal intent from the process table onto the matching
    /// daemon job's own intent bits.
    ///
    /// Deliberately idempotent and level-triggered rather than edge-triggered:
    /// `request_cancel` is a one-way latch and `request_pause` takes the value it
    /// should hold, so re-applying the same intent on every tick is a no-op. That
    /// matters because the process table has no "delivered" flag — state is the
    /// acknowledgement, exactly as `tick_active` already treats
    /// `pause_requested && state != Paused`.
    ///
    /// A job that has gone terminal between the read and here is skipped:
    /// `request_pause` refuses a terminal job, and `request_cancel` silently
    /// succeeds on one, so neither is worth calling.
    /// Reads durable signal intent for every non-terminal job and applies it.
    ///
    /// Called at the top of [`Self::tick`] so `tick_active` sees the intent on the
    /// same pass rather than the next one.
    fn apply_signal_intent_from_table(&mut self, now: u64) -> Result<(), String> {
        let jobs = self.store.nonterminal_jobs()?;
        self.apply_signal_intent(&jobs, now)
    }

    fn apply_signal_intent(&mut self, jobs: &[DaemonJob], now: u64) -> Result<(), String> {
        use little_monkey_lib::process_table::ProcessKind;

        // Decisions are collected before any are applied: reading needs the
        // ledger connection (`self.shared`) and applying needs `&mut self.store`,
        // so the read borrow has to end first.
        enum Intent {
            Cancel,
            Pause(bool),
        }

        let mut decisions: Vec<(String, Intent)> = Vec::new();
        let mut escalated: Vec<String> = Vec::new();
        {
            let table = self.shared.process_table();
            for job in jobs {
                if job.state.is_terminal() {
                    continue;
                }
                let Some(record) = table
                    .find_by_external_id(
                        ProcessKind::DaemonJob,
                        &process_external_id(&job.job_id, attempt_ordinal(job)),
                    )
                    .map_err(|error| error.to_string())?
                else {
                    continue;
                };

                // Stop wins over suspend, and is applied first: a job asked to
                // stop must not be left paused, because a paused child never
                // reaches its own cancellation branch.
                if record.signal_intent.stop_requested {
                    // `kill` and `stop` both cancel; they differ in how the
                    // child is torn down once cancellation reaches it.
                    if record.signal_intent.kill_requested {
                        escalated.push(job.job_id.clone());
                    }
                    if !job.cancel_requested {
                        decisions.push((job.job_id.clone(), Intent::Cancel));
                    }
                    continue;
                }
                if record.signal_intent.suspend_requested != job.pause_requested {
                    decisions.push((
                        job.job_id.clone(),
                        Intent::Pause(record.signal_intent.suspend_requested),
                    ));
                }
            }
        }

        self.immediate_termination.extend(escalated);

        for (job_id, intent) in decisions {
            match intent {
                Intent::Cancel => {
                    self.store.request_cancel(&job_id, now)?;
                }
                Intent::Pause(value) => {
                    self.store.request_pause(&job_id, value, now)?;
                }
            }
        }
        Ok(())
    }

    fn sync_process_table_inner(&mut self, now: u64) -> Result<(), String> {
        use little_monkey_lib::process_table::{
            ExitStatus, ProcessExit, ProcessFilter, ProcessKind, ProcessLimits, ProcessProjection,
            ProcessState,
        };

        let now_ms = i64::try_from(now).map_err(|_| "clock is beyond protocol bounds".to_string())?;
        let jobs = self.store.nonterminal_jobs()?;

        // Keyed by the attempt-scoped external id, not the job id: a job that has
        // retried owns one row per attempt, and only the current one is live.
        let live_external_ids: std::collections::HashSet<String> = jobs
            .iter()
            .map(|job| process_external_id(&job.job_id, attempt_ordinal(job)))
            .collect();

        // Translate any durable signal intent recorded against this job's process
        // row into the daemon's own intent bits, which `tick_active` already
        // honours.
        //
        // The daemon store stays authoritative on purpose. `daemon_jobs` lives in
        // `daemon-v1.sqlite3` and `agent_processes` in `profile-v1.sqlite3`, and
        // ledger connections disable `ATTACH` outright, so there is no
        // transaction, join, or compare-and-set spanning the two — leaving both
        // writable would be a two-writer race with no arbitration primitive.
        // Worse, the ready-queue gate filters on `pause_requested`/
        // `cancel_requested` in SQL inside the daemon's own database, which cannot
        // reference a table in another file. So intent flows one way, latch →
        // daemon bits, and the daemon remains the single source of truth for what
        // it will actually do.
        //
        // This is what makes `monkey processes signal` reach a live daemon job: one
        // extra read per tick on a connection already open.
        let table = self.shared.process_table();

        // The sweep runs before the projections below, not after, so a requeued
        // job's superseded row is closed out before its successor is admitted.
        // Both orders converge within the tick; this one never lets a reader
        // observe two live rows for the same job.
        //
        // Anything this daemon still shows as live whose current attempt is not
        // in the set above has finished — either the job went terminal, or it
        // requeued and this row is the attempt that failed. Reading the job back
        // gives the real outcome; a job pruned by retention before the
        // projection caught up is `Lost`, which is the honest answer rather than
        // a guessed success.
        let live_records = table
            .list(&ProcessFilter {
                kinds: vec![ProcessKind::DaemonJob],
                live_only: true,
                ..ProcessFilter::default()
            })
            .map_err(|error| error.to_string())?;

        for record in live_records {
            if live_external_ids.contains(&record.external_id) {
                continue;
            }
            let (job_id, attempt) = split_external_id(&record.external_id);
            let exit = match self.store.get_job(job_id)? {
                Some(job) if job.state.is_terminal() => {
                    exit_for(job.state, job.last_error.as_deref())
                }
                // The job is still live, so this row is not the attempt it is
                // living as. An attempt-scoped id means a real earlier attempt:
                // it ran and failed, which is precisely why a later one exists,
                // and `last_error` is the failure that triggered the retry.
                Some(job) if attempt.is_some() => ProcessExit {
                    status: ExitStatus::Failed,
                    code: None,
                    signal: None,
                    reason: Some(job.last_error.clone().unwrap_or_else(|| {
                        format!("superseded by attempt {}", attempt_ordinal(&job))
                    })),
                },
                // No attempt in the id: a row written before attempt scoping.
                // Nothing will ever update it again, and the one thing it must
                // not do is keep claiming to be live.
                Some(_) => ProcessExit {
                    status: ExitStatus::Lost,
                    code: None,
                    signal: None,
                    reason: Some(
                        "process row predates attempt-scoped daemon job ids".to_string(),
                    ),
                },
                None => ProcessExit {
                    status: ExitStatus::Lost,
                    code: None,
                    signal: None,
                    reason: Some("daemon job record is gone".to_string()),
                },
            };
            table
                .transition(&record.process_id, ProcessState::Exited, Some(exit), now_ms)
                .map_err(|error| error.to_string())?;
        }

        for job in &jobs {
            // One `reconcile` call rather than a hand-rolled
            // find-or-admit-then-transition: the run id is allocated after the
            // job row exists (`mark_queued`) and the ledger enforces foreign
            // keys, the pid only arrives after spawning, and a tick can
            // legitimately land after a terminal write — `reconcile` owns all
            // three cases so this loop does not re-derive them.
            let projection = ProcessProjection::new(
                ProcessKind::DaemonJob,
                process_external_id(&job.job_id, attempt_ordinal(job)),
                process_state_for(job.state),
            )
            .with_run(job.run_id.clone())
            .with_native_pid(job.process_id.map(i64::from))
            .with_limits(ProcessLimits {
                max_wall_ms: Some(job.max_runtime_ms),
                max_memory_bytes: job.max_memory_bytes,
                max_output_bytes: Some(job.max_log_bytes),
                max_child_processes: None,
            });
            // A non-terminal job never carries an exit, so this cannot be a
            // terminal projection — the terminal case is the sweep above.
            table
                .reconcile(&projection, now_ms)
                .map_err(|error| error.to_string())?;
        }

        Ok(())
    }

    fn start_job(&mut self, job: DaemonJob, now: u64) -> Result<(), String> {
        let run_id = job
            .run_id
            .as_deref()
            .ok_or_else(|| format!("Job '{}' has no durable run", job.job_id))?;
        self.validate_worktree(&job)?;
        let Some(lease) = self.shared.acquire_lease(
            run_id,
            &self.owner_id,
            now,
            self.config.lease_duration_ms,
        )?
        else {
            return Ok(());
        };
        match self.process_adapter.spawn(&job, &self.paths) {
            Ok(process) => {
                let process_id = process.id();
                self.store.transition(
                    &job.job_id,
                    JobState::Running,
                    now,
                    Some(process_id),
                    None,
                )?;
                self.active
                    .insert(job.job_id.clone(), ActiveProcess { process, lease });
                self.notify(run_id, "Background run started", &job.job_id);
                Ok(())
            }
            Err(error) => {
                let _ = self.shared.release_lease(&lease);
                if retry_permitted(&job) {
                    self.store
                        .transition(&job.job_id, JobState::Queued, now, None, Some(&error))
                } else {
                    self.fail_run(run_id, "daemon_spawn_failed", &error)?;
                    self.store
                        .transition(&job.job_id, JobState::Failed, now, None, Some(&error))
                }
            }
        }
    }

    fn tick_active(&mut self, job_id: &str, now: u64) -> Result<(), String> {
        let Some(job) = self.store.get_job(job_id)? else {
            return Err(format!("Active daemon job '{job_id}' disappeared"));
        };
        let run_id = job
            .run_id
            .as_deref()
            .ok_or_else(|| format!("Active job '{job_id}' has no run id"))?;
        let Some(lease) = self.active.get(job_id).map(|active| active.lease.clone()) else {
            return Ok(());
        };
        if !self
            .shared
            .heartbeat_lease(&lease, now, self.config.lease_duration_ms)?
        {
            self.active
                .get_mut(job_id)
                .ok_or_else(|| "active process disappeared".to_string())?
                .process
                .signal(ProcessSignal::Terminate)?;
            self.fail_run(
                run_id,
                "daemon_lease_lost",
                "The daemon lost its exclusive execution lease",
            )?;
            self.finish_active(job_id, JobState::Failed, now, Some("execution lease lost"))?;
            return Ok(());
        }
        if let Some(worktree_json) = &job.worktree_json {
            let owned: OwnedWorktree =
                serde_json::from_str(worktree_json).map_err(|error| error.to_string())?;
            let _ = self
                .shared
                .update_worktree_lease(&owned.lease_id, "active", now);
        }

        if job.cancel_requested || self.store.kill_switch()? {
            // The kill switch is an operator's emergency stop, so it gets the
            // same no-grace-period treatment as an explicit per-job `kill`.
            let immediate =
                self.immediate_termination.contains(job_id) || self.store.kill_switch()?;
            let (signal, detail) = if immediate {
                (
                    ProcessSignal::Kill,
                    "Termination reached the supervised task process",
                )
            } else {
                (
                    ProcessSignal::Terminate,
                    "Cancellation reached the supervised task process",
                )
            };
            self.ensure_cancelling(run_id, "Cancellation requested by daemon controller")?;
            self.active
                .get_mut(job_id)
                .ok_or_else(|| "active process disappeared".to_string())?
                .process
                .signal(signal)?;
            self.cancel_run(run_id, detail)?;
            self.finish_active(job_id, JobState::Cancelled, now, None)?;
            self.immediate_termination.remove(job_id);
            return Ok(());
        }

        if job.pause_requested && job.state != JobState::Paused {
            let process_id = {
                let active = self
                    .active
                    .get_mut(job_id)
                    .ok_or_else(|| "active process disappeared".to_string())?;
                active.process.signal(ProcessSignal::Pause)?;
                active.process.id()
            };
            self.control(run_id)?.emit(RunEvent::Paused {
                reason: Some("Paused by daemon controller".to_string()),
            })?;
            self.store
                .transition(job_id, JobState::Paused, now, Some(process_id), None)?;
            return Ok(());
        }
        if !job.pause_requested && job.state == JobState::Paused {
            let process_id = {
                let active = self
                    .active
                    .get_mut(job_id)
                    .ok_or_else(|| "active process disappeared".to_string())?;
                active.process.signal(ProcessSignal::Resume)?;
                active.process.id()
            };
            self.control(run_id)?.emit(RunEvent::Started {
                engine_id: "monkey-daemon-resume".to_string(),
            })?;
            self.store
                .transition(job_id, JobState::Running, now, Some(process_id), None)?;
        }

        if let Some(started) = job.started_at_ms {
            if now.saturating_sub(started) > job.max_runtime_ms {
                self.cancel_for_budget(job_id, run_id, now, "daemon runtime budget exceeded")?;
                return Ok(());
            }
        }
        if let Some(max_memory) = job.max_memory_bytes {
            if self
                .active
                .get(job_id)
                .ok_or_else(|| "active process disappeared".to_string())?
                .process
                .memory_bytes()?
                .is_some_and(|used| used > max_memory)
            {
                self.cancel_for_budget(job_id, run_id, now, "daemon memory budget exceeded")?;
                return Ok(());
            }
        }
        let log_path = self.paths.logs.join(format!("{}.log", job.job_id));
        if std::fs::metadata(log_path)
            .map(|metadata| metadata.len() > job.max_log_bytes)
            .unwrap_or(false)
        {
            self.cancel_for_budget(job_id, run_id, now, "daemon log budget exceeded")?;
            return Ok(());
        }

        let exit_code = self
            .active
            .get_mut(job_id)
            .ok_or_else(|| "active process disappeared".to_string())?
            .process
            .try_wait()?;
        if let Some(exit_code) = exit_code {
            let stored = self
                .shared
                .load_run(run_id)?
                .ok_or_else(|| format!("Durable run '{run_id}' disappeared"))?;
            if stored.status.is_terminal() {
                let state = map_run_status(stored.status);
                self.finish_active(job_id, state, now, None)?;
                self.notify_terminal(run_id, state);
            } else if stored.status == RunStatus::Queued && exit_code != 0 && retry_permitted(&job)
            {
                // No Started event means the child proved no tool could have
                // executed. This is the only automatic retry boundary.
                self.finish_active(
                    job_id,
                    JobState::Queued,
                    now,
                    Some("child exited before start"),
                )?;
            } else {
                self.reconcile_interrupted(
                    &job,
                    now,
                    &format!("supervised task child exited with code {exit_code}"),
                )?;
                self.active.remove(job_id);
            }
        } else if let Some(stored) = self.shared.load_run(run_id)? {
            let projected = map_run_status(stored.status);
            if projected == JobState::WaitingApproval && job.state != projected {
                let process_id = self.active.get(job_id).map(|active| active.process.id());
                self.store
                    .transition(job_id, projected, now, process_id, None)?;
                self.notify(
                    run_id,
                    "Approval required",
                    "Unsafe background work is waiting",
                );
            }
        }
        Ok(())
    }

    fn cancel_for_budget(
        &mut self,
        job_id: &str,
        run_id: &str,
        now: u64,
        reason: &str,
    ) -> Result<(), String> {
        self.ensure_cancelling(run_id, reason)?;
        if let Some(active) = self.active.get_mut(job_id) {
            active.process.signal(ProcessSignal::Terminate)?;
        }
        self.cancel_run(run_id, reason)?;
        self.finish_active(job_id, JobState::Cancelled, now, Some(reason))
    }

    fn finish_active(
        &mut self,
        job_id: &str,
        state: JobState,
        now: u64,
        error: Option<&str>,
    ) -> Result<(), String> {
        if let Some(active) = self.active.remove(job_id) {
            let _ = self.shared.release_lease(&active.lease);
        }
        self.store.transition(job_id, state, now, None, error)
    }

    fn reconcile_interrupted(
        &mut self,
        job: &DaemonJob,
        now: u64,
        reason: &str,
    ) -> Result<(), String> {
        let Some(run_id) = job.run_id.as_deref() else {
            self.store.transition(
                &job.job_id,
                JobState::Failed,
                now,
                None,
                Some("preparing job has no run id"),
            )?;
            return Ok(());
        };
        let run = self
            .shared
            .load_run(run_id)?
            .ok_or_else(|| format!("Durable run '{run_id}' disappeared"))?;
        if run.status.is_terminal() {
            self.store
                .transition(&job.job_id, map_run_status(run.status), now, None, None)?;
            return Ok(());
        }
        if run.status == RunStatus::Queued {
            self.store
                .transition(&job.job_id, JobState::Queued, now, None, Some(reason))?;
            return Ok(());
        }
        let mutations = self.shared.mutations(run_id)?;
        if let Some(pending) = mutations
            .iter()
            .find(|mutation| mutation.state == "pending")
        {
            self.control(run_id)?.emit(RunEvent::NeedsReconciliation {
                mutation_id: pending.mutation_id.clone(),
                reason: bounded_text(
                    &format!("{reason}; external mutation state is uncertain"),
                    60 * 1024,
                ),
            })?;
            self.store.transition(
                &job.job_id,
                JobState::NeedsReconciliation,
                now,
                None,
                Some(reason),
            )?;
            self.notify(run_id, "Run needs reconciliation", reason);
        } else if run.status == RunStatus::Cancelling || job.cancel_requested {
            self.cancel_run(run_id, reason)?;
            self.store
                .transition(&job.job_id, JobState::Cancelled, now, None, Some(reason))?;
        } else {
            // Confirmed mutations are deliberately not replayed either. A
            // manual retry must explicitly acknowledge their existence.
            let suffix = if mutations
                .iter()
                .any(|mutation| mutation.state == "confirmed")
            {
                "; one or more external mutations were already confirmed and will not be replayed"
            } else {
                ""
            };
            self.fail_run(run_id, "daemon_interrupted", &format!("{reason}{suffix}"))?;
            self.store
                .transition(&job.job_id, JobState::Failed, now, None, Some(reason))?;
        }
        Ok(())
    }

    fn validate_worktree(&self, job: &DaemonJob) -> Result<(), String> {
        match (&job.worktree_json, &job.repository_policy_json) {
            (Some(worktree), Some(policy)) => {
                let worktree: OwnedWorktree =
                    serde_json::from_str(worktree).map_err(|error| error.to_string())?;
                let policy: RepositoryPolicy =
                    serde_json::from_str(policy).map_err(|error| error.to_string())?;
                worktree.validate_live(&self.paths, &policy)
            }
            (None, Some(policy)) => {
                let policy: RepositoryPolicy =
                    serde_json::from_str(policy).map_err(|error| error.to_string())?;
                if policy.owned_worktree_required {
                    Err("Run requires an owned worktree but has no owned lease".to_string())
                } else {
                    Ok(())
                }
            }
            (Some(_), None) => Err("Owned worktree has no repository policy".to_string()),
            (None, None) => Ok(()),
        }
    }

    fn cancel_queued(&mut self, now: u64, reason: &str) -> Result<(), String> {
        for job in self.store.nonterminal_jobs()? {
            if job.state == JobState::Queued || job.state == JobState::Preparing {
                if let Some(run_id) = job.run_id.as_deref() {
                    self.ensure_cancelling(run_id, reason)?;
                    self.cancel_run(run_id, reason)?;
                }
                self.store
                    .transition(&job.job_id, JobState::Cancelled, now, None, Some(reason))?;
            }
        }
        Ok(())
    }

    fn apply_retention(&mut self, now: u64) -> Result<(), String> {
        let retention_ms =
            u64::from(self.config.retention_days).saturating_mul(24 * 60 * 60 * 1_000);
        let before = now.saturating_sub(retention_ms);
        for job in self.store.prune_terminal(before)? {
            remove_if_exists(&job.recipe_snapshot)?;
            remove_if_exists(&self.paths.logs.join(format!("{}.log", job.job_id)))?;
        }
        for job in self.store.terminal_worktree_jobs(before)? {
            let Some(value) = job.worktree_json.as_deref() else {
                continue;
            };
            let owned: OwnedWorktree =
                serde_json::from_str(value).map_err(|error| error.to_string())?;
            match owned.safe_cleanup(&self.paths) {
                Ok(true) => {
                    self.shared
                        .update_worktree_lease(&owned.lease_id, "released", now)?;
                    remove_if_exists(&job.recipe_snapshot)?;
                    remove_if_exists(&self.paths.logs.join(format!("{}.log", job.job_id)))?;
                    self.store.delete_terminal_job(&job.job_id)?;
                }
                Ok(false) => {
                    self.shared.update_worktree_lease(
                        &owned.lease_id,
                        "needs_reconciliation",
                        now,
                    )?;
                }
                Err(error) => {
                    self.shared.update_worktree_lease(
                        &owned.lease_id,
                        "needs_reconciliation",
                        now,
                    )?;
                    eprintln!("daemon retention: {error}");
                }
            }
        }
        Ok(())
    }

    fn control(&self, run_id: &str) -> Result<Arc<DurableRunRecorder>, String> {
        DurableRunRecorder::attach(
            self.shared.run_ledger()?,
            run_id,
            "daemon-controller".to_string(),
            ClientIdentity {
                client_id: "monkey-daemon".to_string(),
                instance_id: self.owner_id.clone(),
                kind: ClientKind::Daemon,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        )
    }

    fn ensure_cancelling(&self, run_id: &str, reason: &str) -> Result<(), String> {
        let run = self
            .shared
            .load_run(run_id)?
            .ok_or_else(|| format!("Unknown run '{run_id}'"))?;
        if run.status != RunStatus::Cancelling {
            let recorder = self.control(run_id)?;
            recorder.emit(RunEvent::CancellationRequested {
                requested_by: recorder.client_identity(),
                reason: Some(bounded_text(reason, 60 * 1024)),
            })?;
            recorder.emit(RunEvent::Cancelling {
                reason: Some(bounded_text(reason, 60 * 1024)),
            })?;
        }
        Ok(())
    }

    fn cancel_run(&self, run_id: &str, reason: &str) -> Result<(), String> {
        let run = self
            .shared
            .load_run(run_id)?
            .ok_or_else(|| format!("Unknown run '{run_id}'"))?;
        if !run.status.is_terminal() {
            self.control(run_id)?.emit(RunEvent::Cancelled {
                reason: Some(bounded_text(reason, 60 * 1024)),
            })?;
        }
        Ok(())
    }

    fn fail_run(&self, run_id: &str, code: &str, message: &str) -> Result<(), String> {
        let run = self
            .shared
            .load_run(run_id)?
            .ok_or_else(|| format!("Unknown run '{run_id}'"))?;
        if !run.status.is_terminal() {
            self.control(run_id)?.emit(RunEvent::Failed {
                code: code.to_string(),
                message: bounded_text(message, 60 * 1024),
                retryable: false,
            })?;
        }
        Ok(())
    }

    pub fn decide_approval(
        &self,
        run_id: &str,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), String> {
        let ledger = self.shared.run_ledger()?;
        let approval = ledger
            .load_approval(run_id, request_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("Unknown approval '{request_id}' for run '{run_id}'"))?;
        if approval.decision.is_some() {
            return Err(format!("Approval '{request_id}' was already decided"));
        }
        let now = self.clock.now_ms();
        let effective = if now >= approval.expires_at_ms {
            PermissionDecision::Expired
        } else {
            decision
        };
        let recorder = self.control(run_id)?;
        recorder.emit(RunEvent::PermissionDecided {
            request_id: request_id.to_string(),
            operation_sha256: approval.operation_sha256,
            decision: effective,
            decided_by: recorder.client_identity(),
        })
    }

    fn notify(&self, run_id: &str, title: &str, body: &str) {
        if !self.config.notifications {
            return;
        }
        let _ = self.notifier.notify(&DaemonNotification {
            run_id: run_id.to_string(),
            title: title.to_string(),
            body: bounded_text(body, 1024),
        });
    }

    fn notify_terminal(&self, run_id: &str, state: JobState) {
        self.notify(
            run_id,
            "Background run finished",
            &format!("Run status: {}", state.token()),
        );
    }
}

fn remove_if_exists(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Failed to remove '{}': {error}", path.display())),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use little_monkey_lib::run_ledger::RunLedger;
    use little_monkey_lib::run_protocol::{
        ClientIdentity, ClientKind, ModelTargetSnapshot, MutationKind,
        PermissionMode as RunPermissionMode, PermissionPolicySnapshot, RootAccess, RootGrant,
        RunBudgets, RunKind, RunSpec, ToolPolicyDecision, WorkspaceContext,
        RUN_PROTOCOL_SCHEMA_VERSION,
    };

    use crate::daemon::store::{NewDaemonJob, DEFAULT_MAX_LOG_BYTES};
    use crate::durable_run::DurableRunRecorder;

    #[derive(Clone)]
    struct FakeClock(Arc<Mutex<u64>>);
    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            *self.0.lock().unwrap()
        }
    }

    #[derive(Default)]
    struct FakeNotifier(Arc<Mutex<Vec<DaemonNotification>>>);
    impl NotificationAdapter for FakeNotifier {
        fn notify(&self, notification: &DaemonNotification) -> Result<(), String> {
            self.0.lock().unwrap().push(notification.clone());
            Ok(())
        }
    }

    struct FakeProcess {
        id: u32,
        exits: Arc<Mutex<VecDeque<Option<i32>>>>,
        signals: Arc<Mutex<Vec<ProcessSignal>>>,
    }
    impl ManagedProcess for FakeProcess {
        fn id(&self) -> u32 {
            self.id
        }
        fn try_wait(&mut self) -> Result<Option<i32>, String> {
            Ok(self.exits.lock().unwrap().pop_front().flatten())
        }
        fn signal(&mut self, signal: ProcessSignal) -> Result<(), String> {
            self.signals.lock().unwrap().push(signal);
            Ok(())
        }
        fn memory_bytes(&self) -> Result<Option<u64>, String> {
            Ok(Some(1024))
        }
    }

    #[derive(Clone)]
    struct FakeProcesses {
        spawns: Arc<Mutex<u32>>,
        exits: Arc<Mutex<VecDeque<Option<i32>>>>,
        signals: Arc<Mutex<Vec<ProcessSignal>>>,
    }
    impl ProcessAdapter for FakeProcesses {
        fn spawn(
            &self,
            _job: &DaemonJob,
            _paths: &DaemonPaths,
        ) -> Result<Box<dyn ManagedProcess>, String> {
            *self.spawns.lock().unwrap() += 1;
            Ok(Box::new(FakeProcess {
                id: 42,
                exits: self.exits.clone(),
                signals: self.signals.clone(),
            }))
        }
        fn terminate_orphan(&self, _process_id: u32) -> Result<(), String> {
            self.signals.lock().unwrap().push(ProcessSignal::Terminate);
            Ok(())
        }
    }

    // Shared with `daemon::tests`, which needs a real durable run to satisfy
    // `agent_processes.run_id`'s foreign key.
    pub(in crate::daemon) fn spec(run_id: &str, now: u64) -> RunSpec {
        RunSpec {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            run_id: run_id.into(),
            idempotency_key: format!("idem-{run_id}"),
            created_at_ms: now,
            kind: RunKind::Background,
            submitted_by: ClientIdentity {
                client_id: "daemon-test".into(),
                instance_id: "daemon-fixture".into(),
                kind: ClientKind::Daemon,
                version: "1".into(),
            },
            task: "fixture".into(),
            instructions: None,
            input_artifact_ids: vec![],
            target: ModelTargetSnapshot::Provider {
                target_id: "fixture-target".into(),
                label: "fixture".into(),
                provider_id: "fixture".into(),
                endpoint: "http://127.0.0.1:1/v1".into(),
                model: "fixture".into(),
                credential_ref_id: "credential-none".into(),
                capabilities: crate::task::cli_capabilities(),
            },
            workspace: Some(WorkspaceContext {
                workspace_id: "fixture-workspace".into(),
                primary_root_id: "root-primary".into(),
                roots: vec![RootGrant {
                    root_id: "root-primary".into(),
                    canonical_path: std::env::temp_dir().to_string_lossy().to_string(),
                    access: RootAccess::ReadWrite,
                    allow_symlinks_within_root: false,
                }],
                repository_policy: None,
            }),
            permission_policy: PermissionPolicySnapshot {
                mode: RunPermissionMode::Auto,
                unattended: true,
                approval_timeout_ms: 60_000,
                default_tool_decision: ToolPolicyDecision::Prompt,
                tool_rules: vec![],
                allow_network: false,
                allow_external_mutations: false,
            },
            budgets: RunBudgets {
                wall_time_ms: 60_000,
                max_iterations: 8,
                max_model_calls: 8,
                max_tool_calls: 8,
                max_input_tokens: 10_000,
                max_output_tokens: 10_000,
                max_cost_micros: None,
                max_artifact_bytes: 1024 * 1024,
                max_event_count: 1000,
            },
        }
    }

    fn fixture(
        label: &str,
    ) -> (
        DaemonPaths,
        DaemonStore,
        SharedLedger,
        Arc<DurableRunRecorder>,
        String,
    ) {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-daemon-engine-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let paths = DaemonPaths::under(&root);
        paths.ensure().unwrap();
        let run_id = format!("run-{label}");
        let ledger = RunLedger::open(&paths.ledger_db).unwrap();
        let (recorder, _) =
            DurableRunRecorder::submit(ledger, &spec(&run_id, 1_000), "daemon-fixture".into())
                .unwrap();
        let mut store = DaemonStore::open(&paths).unwrap();
        let snapshot = paths.snapshots.join(format!("job-{label}.json"));
        std::fs::write(&snapshot, b"{}").unwrap();
        store
            .insert_preparing(
                &NewDaemonJob {
                    job_id: format!("job-{label}"),
                    recipe_snapshot: snapshot,
                    priority: 0,
                    max_attempts: 1,
                    created_at_ms: 1_000,
                    max_runtime_ms: 60_000,
                    max_memory_bytes: None,
                    max_log_bytes: DEFAULT_MAX_LOG_BYTES,
                    repository_policy_json: None,
                    worktree_json: None,
                    parent_run_id: None,
                },
                8,
            )
            .unwrap();
        store
            .mark_queued(&format!("job-{label}"), &run_id, 1_000)
            .unwrap();
        let shared = SharedLedger::open(&paths.ledger_db).unwrap();
        (paths, store, shared, recorder, run_id)
    }

    /// The process-table id of a fixture job's first attempt — what every test
    /// below is looking at, since `max_attempts: 1` leaves them no second one.
    fn first_attempt_id(label: &str) -> String {
        process_external_id(&format!("job-{label}"), 0)
    }

    fn fake_adapter() -> FakeProcesses {
        FakeProcesses {
            spawns: Arc::new(Mutex::new(0)),
            exits: Arc::new(Mutex::new(VecDeque::from([None, None]))),
            signals: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[test]
    fn process_signal_enum_distinguishes_pause_resume_and_cancel() {
        assert_ne!(ProcessSignal::Pause, ProcessSignal::Resume);
        assert_ne!(ProcessSignal::Resume, ProcessSignal::Terminate);
    }

    #[test]
    fn cancellation_reaches_the_active_process_and_terminalizes_the_ledger() {
        let (paths, store, shared, _recorder, run_id) = fixture("cancel");
        let adapter = fake_adapter();
        let signals = adapter.signals.clone();
        let clock = FakeClock(Arc::new(Mutex::new(2_000)));
        let mut engine = DaemonEngine::new(
            store,
            shared,
            paths,
            DaemonConfig::default(),
            adapter,
            FakeNotifier::default(),
            clock.clone(),
            "daemon-test-owner".into(),
        );
        engine.tick().unwrap();
        assert_eq!(engine.active_count(), 1);
        engine.store.request_cancel(&run_id, 2_001).unwrap();
        *clock.0.lock().unwrap() = 2_001;
        engine.tick().unwrap();
        assert!(signals.lock().unwrap().contains(&ProcessSignal::Terminate));
        assert_eq!(engine.active_count(), 0);
        assert_eq!(
            engine.shared.load_run(&run_id).unwrap().unwrap().status,
            RunStatus::Cancelled
        );
    }

    fn retry_job(attempt: u32, max_attempts: u32, updated_at_ms: u64) -> DaemonJob {
        DaemonJob {
            job_id: "job-retry".into(),
            run_id: Some("run-retry".into()),
            recipe_snapshot: std::path::PathBuf::from("/tmp/none.json"),
            state: JobState::Queued,
            priority: 0,
            attempt,
            max_attempts,
            created_at_ms: 1_000,
            updated_at_ms,
            started_at_ms: None,
            finished_at_ms: None,
            process_id: None,
            max_runtime_ms: 60_000,
            max_memory_bytes: None,
            max_log_bytes: DEFAULT_MAX_LOG_BYTES,
            pause_requested: false,
            cancel_requested: false,
            repository_policy_json: None,
            worktree_json: None,
            parent_run_id: None,
            last_error: None,
        }
    }

    #[test]
    fn the_stricter_of_the_job_and_the_kind_bounds_retries() {
        // Two ceilings had to agree and previously did not: the per-job
        // `max_attempts` set at submission, and the per-kind declared policy.
        // Neither may override the other upward.
        //
        // The kind permits 3 attempts, so a job asking for 10 still stops at 3.
        assert!(retry_permitted(&retry_job(0, 10, 1_000)));
        assert!(retry_permitted(&retry_job(1, 10, 1_000)));
        assert!(
            !retry_permitted(&retry_job(2, 10, 1_000)),
            "a job cannot out-live the kind's ceiling by asking for more attempts"
        );

        // And a job submitted with a single attempt is not given more by the
        // kind's policy.
        assert!(
            !retry_permitted(&retry_job(0, 1, 1_000)),
            "the kind must not force retries onto a job that asked for one attempt"
        );
    }

    #[test]
    fn a_retry_waits_out_its_backoff_before_being_dispatched() {
        // A first attempt never waits.
        assert!(backoff_elapsed(&retry_job(0, 3, 10_000), 10_000));

        // After one spent attempt the base backoff applies, measured from the
        // transition that re-queued the job.
        let job = retry_job(1, 3, 10_000);
        assert!(!backoff_elapsed(&job, 10_500), "dispatched during backoff");
        assert!(backoff_elapsed(&job, 11_000), "still waiting after backoff");

        // And it grows with the attempts already spent.
        let later = retry_job(2, 3, 10_000);
        assert!(!backoff_elapsed(&later, 11_000));
        assert!(backoff_elapsed(&later, 12_000));
    }

    #[test]
    fn a_durable_kill_terminates_immediately_while_a_stop_winds_down() {
        use little_monkey_lib::process_table::{ProcessKind, ProcessSignal as TableSignal};

        // The whole point of giving `kill` its own latch: the daemon delivers
        // the two differently, so recording them identically would have thrown
        // away the caller's actual request.
        for (signal, expected) in [
            (TableSignal::Stop, ProcessSignal::Terminate),
            (TableSignal::Kill, ProcessSignal::Kill),
        ] {
            let label = if signal == TableSignal::Kill { "kill" } else { "stop" };
            let (paths, store, shared, _recorder, run_id) = fixture(label);
            let adapter = fake_adapter();
            let signals = adapter.signals.clone();
            let clock = FakeClock(Arc::new(Mutex::new(2_000)));
            let mut engine = DaemonEngine::new(
                store,
                shared,
                paths,
                DaemonConfig::default(),
                adapter,
                FakeNotifier::default(),
                clock.clone(),
                "daemon-test-owner".into(),
            );
            engine.tick().unwrap();
            assert_eq!(engine.active_count(), 1);

            // Written to the durable latch only — never to the daemon's own
            // store — so this exercises the same path a `monkey processes
            // signal` from another process takes.
            let process_id = {
                let table = engine.shared.process_table();
                table
                    .find_by_external_id(ProcessKind::DaemonJob, &first_attempt_id(label))
                    .unwrap()
                    .expect("the job is projected")
                    .process_id
            };
            {
                let table = engine.shared.process_table();
                table
                    .signal(&process_id, signal, Some("from the table"), 2_001)
                    .unwrap();
            }

            *clock.0.lock().unwrap() = 2_001;
            engine.tick().unwrap();

            let delivered = signals.lock().unwrap().clone();
            assert!(
                delivered.contains(&expected),
                "{label} should deliver {expected:?}, got {delivered:?}"
            );
            let unexpected = if expected == ProcessSignal::Kill {
                ProcessSignal::Terminate
            } else {
                ProcessSignal::Kill
            };
            assert!(
                !delivered.contains(&unexpected),
                "{label} must not deliver {unexpected:?}"
            );
            assert_eq!(
                engine.shared.load_run(&run_id).unwrap().unwrap().status,
                RunStatus::Cancelled
            );
        }
    }

    #[test]
    fn a_job_is_projected_onto_the_unified_process_table_through_its_whole_life() {
        use little_monkey_lib::process_table::{ExitStatus, ProcessKind, ProcessState};

        let (paths, store, shared, _recorder, run_id) = fixture("processtable");
        let adapter = fake_adapter();
        let clock = FakeClock(Arc::new(Mutex::new(2_000)));
        let mut engine = DaemonEngine::new(
            store,
            shared,
            paths,
            DaemonConfig::default(),
            adapter,
            FakeNotifier::default(),
            clock.clone(),
            "daemon-test-owner".into(),
        );

        engine.tick().unwrap();

        let record = {
            let table = engine.shared.process_table();
            table
                .find_by_external_id(ProcessKind::DaemonJob, &first_attempt_id("processtable"))
                .unwrap()
                .expect("the daemon must project its job onto the process table")
        };
        assert_eq!(record.state, ProcessState::Running);
        assert_eq!(record.native_pid, Some(42), "the spawned pid is recorded");
        assert_eq!(
            record.run_id.as_deref(),
            Some(run_id.as_str()),
            "the ledger run is linked once its row exists"
        );
        assert_eq!(record.limits.max_wall_ms, Some(60_000));
        assert_eq!(record.limits.max_output_bytes, Some(DEFAULT_MAX_LOG_BYTES));
        assert!(record.started_at_ms.is_some());

        // Idempotent: a second tick must not fork a second record.
        *clock.0.lock().unwrap() = 2_001;
        engine.tick().unwrap();
        {
            let table = engine.shared.process_table();
            let all = table
                .list(&little_monkey_lib::process_table::ProcessFilter {
                    kinds: vec![ProcessKind::DaemonJob],
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(all.len(), 1, "the projection forked a second record");
            assert_eq!(all[0].process_id, record.process_id);
        }

        // Cancelling the job exits the process with the real outcome.
        engine.store.request_cancel(&run_id, 2_002).unwrap();
        *clock.0.lock().unwrap() = 2_002;
        engine.tick().unwrap();

        let finished = {
            let table = engine.shared.process_table();
            table.get(&record.process_id).unwrap().unwrap()
        };
        assert_eq!(finished.state, ProcessState::Exited);
        assert_eq!(
            finished.exit.as_ref().map(|exit| exit.status),
            Some(ExitStatus::Cancelled),
            "a cancelled job must not be projected as a success"
        );
        assert!(finished.exited_at_ms.is_some());

        // And it stays exited — a later tick must not resurrect it.
        *clock.0.lock().unwrap() = 2_003;
        engine.tick().unwrap();
        let table = engine.shared.process_table();
        assert_eq!(
            table.get(&record.process_id).unwrap().unwrap().state,
            ProcessState::Exited
        );
        assert!(table.live_counts().unwrap().is_empty());
    }

    #[test]
    fn a_stop_written_to_the_process_table_latch_cancels_a_live_daemon_job() {
        // The point of durable intent: `monkey processes signal` (or another
        // window, or a previous session) writes SQLite with no access to this
        // daemon, and the running job still stops.
        // The daemon has its own `ProcessSignal` {Pause, Resume, Terminate} — the OS
        // delivery verbs — which is a different vocabulary from the table's
        // {Stop, Suspend, Resume, Kill} request verbs. Aliased so the test reads
        // unambiguously.
        use little_monkey_lib::process_table::{
            ExitStatus, ProcessKind, ProcessSignal as TableSignal, ProcessState,
        };

        let (paths, store, shared, _recorder, run_id) = fixture("latch-stop");
        let adapter = fake_adapter();
        let signals = adapter.signals.clone();
        let clock = FakeClock(Arc::new(Mutex::new(2_000)));
        let mut engine = DaemonEngine::new(
            store,
            shared,
            paths,
            DaemonConfig::default(),
            adapter,
            FakeNotifier::default(),
            clock.clone(),
            "daemon-test-owner".into(),
        );

        engine.tick().unwrap();
        assert_eq!(engine.active_count(), 1, "the job should be running");
        assert!(
            !engine
                .store
                .get_job("job-latch-stop")
                .unwrap()
                .unwrap()
                .cancel_requested,
            "nothing has asked it to stop yet"
        );

        // Written the way an external caller would: straight to the process
        // table, with no daemon involvement.
        {
            let table = engine.shared.process_table();
            let record = table
                .find_by_external_id(ProcessKind::DaemonJob, &first_attempt_id("latch-stop"))
                .unwrap()
                .expect("the tick projected the job");
            table
                .signal(
                    &record.process_id,
                    TableSignal::Stop,
                    Some("stopped from the CLI"),
                    2_001,
                )
                .unwrap();
        }

        *clock.0.lock().unwrap() = 2_001;
        engine.tick().unwrap();

        assert!(
            signals.lock().unwrap().contains(&ProcessSignal::Terminate),
            "the latch did not reach the supervised child process"
        );
        assert_eq!(engine.active_count(), 0);
        assert_eq!(
            engine.shared.load_run(&run_id).unwrap().unwrap().status,
            RunStatus::Cancelled
        );

        let table = engine.shared.process_table();
        let finished = table
            .find_by_external_id(ProcessKind::DaemonJob, &first_attempt_id("latch-stop"))
            .unwrap()
            .unwrap();
        assert_eq!(finished.state, ProcessState::Exited);
        assert_eq!(
            finished.exit.as_ref().map(|exit| exit.status),
            Some(ExitStatus::Cancelled),
            "a latch-driven stop must be recorded as cancelled, not failed"
        );
    }

    #[test]
    fn a_suspend_latch_pauses_and_resumes_without_thrashing_on_every_tick() {
        use little_monkey_lib::process_table::{
            ProcessKind, ProcessSignal as TableSignal, ProcessState,
        };

        let (paths, store, shared, _recorder, _run_id) = fixture("latch-suspend");
        let adapter = fake_adapter();
        let signals = adapter.signals.clone();
        let clock = FakeClock(Arc::new(Mutex::new(2_000)));
        let mut engine = DaemonEngine::new(
            store,
            shared,
            paths,
            DaemonConfig::default(),
            adapter,
            FakeNotifier::default(),
            clock.clone(),
            "daemon-test-owner".into(),
        );
        engine.tick().unwrap();

        let process_id = {
            let table = engine.shared.process_table();
            table
                .find_by_external_id(ProcessKind::DaemonJob, &first_attempt_id("latch-suspend"))
                .unwrap()
                .unwrap()
                .process_id
        };

        // Suspend via the latch.
        {
            let table = engine.shared.process_table();
            table
                .signal(&process_id, TableSignal::Suspend, None, 2_001)
                .unwrap();
        }
        *clock.0.lock().unwrap() = 2_001;
        engine.tick().unwrap();
        assert!(signals.lock().unwrap().contains(&ProcessSignal::Pause));
        assert_eq!(
            engine
                .store
                .get_job("job-latch-suspend")
                .unwrap()
                .unwrap()
                .state,
            JobState::Paused
        );

        // Idle ticks must not re-deliver: state is the acknowledgement, so a
        // suspended job with a set latch is not pending.
        let pauses_after_first = signals
            .lock()
            .unwrap()
            .iter()
            .filter(|signal| **signal == ProcessSignal::Pause)
            .count();
        for extra in 2..5 {
            *clock.0.lock().unwrap() = 2_000 + extra;
            engine.tick().unwrap();
        }
        assert_eq!(
            signals
                .lock()
                .unwrap()
                .iter()
                .filter(|signal| **signal == ProcessSignal::Pause)
                .count(),
            pauses_after_first,
            "the suspend latch was re-delivered on an idle tick"
        );

        // Resume clears it and the job runs again.
        {
            let table = engine.shared.process_table();
            table
                .signal(&process_id, TableSignal::Resume, None, 2_010)
                .unwrap();
        }
        *clock.0.lock().unwrap() = 2_010;
        engine.tick().unwrap();
        assert!(signals.lock().unwrap().contains(&ProcessSignal::Resume));
        assert_eq!(
            engine
                .store
                .get_job("job-latch-suspend")
                .unwrap()
                .unwrap()
                .state,
            JobState::Running
        );
        let table = engine.shared.process_table();
        assert_eq!(
            table.get(&process_id).unwrap().unwrap().state,
            ProcessState::Running
        );
        // Same process, same row: a resumed job is the attempt that was paused,
        // not a new one. Counting it as a start would spend a retry the job
        // never used, and would move its process row out from under this id.
        assert_eq!(
            engine
                .store
                .get_job("job-latch-suspend")
                .unwrap()
                .unwrap()
                .attempt,
            1,
            "pausing and resuming spent an attempt"
        );
    }

    #[test]
    fn job_states_map_onto_the_process_state_machine_without_losing_liveness() {
        use little_monkey_lib::process_table::{ExitStatus, ProcessState};

        // Every JobState must map somewhere, and the terminal ones must map to
        // an exit that is not silently "succeeded".
        for (state, expected) in [
            (JobState::Preparing, ProcessState::Admitted),
            (JobState::Queued, ProcessState::Admitted),
            (JobState::Running, ProcessState::Running),
            (JobState::WaitingApproval, ProcessState::Running),
            (JobState::Cancelling, ProcessState::Running),
            (JobState::Paused, ProcessState::Suspended),
            (JobState::Succeeded, ProcessState::Exited),
            (JobState::Failed, ProcessState::Exited),
            (JobState::Cancelled, ProcessState::Exited),
            (JobState::NeedsReconciliation, ProcessState::Exited),
        ] {
            assert_eq!(
                process_state_for(state),
                expected,
                "{state:?} mapped to the wrong process state"
            );
            assert_eq!(
                state.is_terminal(),
                expected.is_terminal(),
                "{state:?} disagrees with its process state about being terminal"
            );
        }

        assert_eq!(exit_for(JobState::Succeeded, None).status, ExitStatus::Succeeded);
        assert_eq!(exit_for(JobState::Failed, Some("boom")).status, ExitStatus::Failed);
        assert_eq!(
            exit_for(JobState::Failed, Some("boom")).reason.as_deref(),
            Some("boom")
        );
        assert_eq!(exit_for(JobState::Cancelled, None).status, ExitStatus::Cancelled);
        assert_eq!(
            exit_for(JobState::NeedsReconciliation, None).status,
            ExitStatus::NeedsReconciliation
        );
        // A non-terminal state reaching the exit mapper means the job vanished.
        assert_eq!(exit_for(JobState::Running, None).status, ExitStatus::Lost);
    }

    #[test]
    fn an_external_id_round_trips_through_its_job_id_and_attempt() {
        assert_eq!(process_external_id("job-a", 0), "job-a#0");
        assert_eq!(split_external_id("job-a#0"), ("job-a", Some(0)));
        assert_eq!(split_external_id("job-a#12"), ("job-a", Some(12)));

        // A row written before attempt scoping carries no attempt, and must not
        // be mistaken for one — the sweep treats the two differently.
        assert_eq!(split_external_id("job-a"), ("job-a", None));

        // `--job-id` lets a caller supply their own, so a `#` in the id is not
        // proof of an attempt suffix. Only a numeric tail counts, and the real
        // suffix still wins when both are present.
        assert_eq!(split_external_id("job#a"), ("job#a", None));
        assert_eq!(split_external_id("job#a#3"), ("job#a", Some(3)));

        // Signed and overflowing tails are not attempts either.
        assert_eq!(split_external_id("job-a#-1"), ("job-a#-1", None));
        assert_eq!(
            split_external_id("job-a#99999999999999999999"),
            ("job-a#99999999999999999999", None)
        );
    }

    #[test]
    fn the_attempt_a_row_belongs_to_is_one_behind_the_start_counter() {
        // `attempt` counts starts, so it moves at the transition into `running`
        // — mid-attempt, not between attempts. The row's identity must not.
        let ordinal = |state, attempt| {
            let mut job = retry_job(attempt, 5, 1_000);
            job.state = state;
            attempt_ordinal(&job)
        };

        // First attempt: queued at 0, still the same attempt once running at 1.
        assert_eq!(ordinal(JobState::Queued, 0), 0);
        assert_eq!(ordinal(JobState::Running, 1), 0);
        assert_eq!(ordinal(JobState::Failed, 1), 0);

        // Requeued: the counter has not moved, but the attempt has.
        assert_eq!(ordinal(JobState::Queued, 1), 1);
        assert_eq!(ordinal(JobState::Running, 2), 1);

        // A `running` job with a zero counter is not reachable through the
        // store, but must not underflow if one is ever recovered.
        assert_eq!(ordinal(JobState::Running, 0), 0);
    }

    #[test]
    fn a_retried_job_gets_a_new_row_and_the_attempt_it_replaces_is_closed_as_failed() {
        use little_monkey_lib::process_table::{
            ExitStatus, ProcessFilter, ProcessKind, ProcessState,
        };

        let (paths, store, shared, _recorder, _run_id) = fixture("retrysweep");
        let clock = FakeClock(Arc::new(Mutex::new(2_000)));
        let mut engine = DaemonEngine::new(
            store,
            shared,
            paths,
            DaemonConfig::default(),
            fake_adapter(),
            FakeNotifier::default(),
            clock,
            "daemon-test-owner".into(),
        );
        engine.tick().unwrap();

        // The requeue both retry branches perform: back to `queued`, carrying
        // the error that caused it, with the start counter left alone.
        engine
            .store
            .transition(
                "job-retrysweep",
                JobState::Queued,
                3_000,
                None,
                Some("spawn failed: boom"),
            )
            .unwrap();
        // The inner sync, not the wrapper: the wrapper logs and swallows, which
        // is right in production and would hide the very failure this covers.
        engine.sync_process_table_inner(3_000).unwrap();

        let table = engine.shared.process_table();
        let first = table
            .find_by_external_id(ProcessKind::DaemonJob, &first_attempt_id("retrysweep"))
            .unwrap()
            .expect("the interrupted attempt keeps its row");
        assert_eq!(first.state, ProcessState::Exited);
        let exit = first.exit.expect("an exited row carries its exit");
        assert_eq!(
            exit.status,
            ExitStatus::Failed,
            "a superseded attempt did not vanish — it failed, which is why there \
             is another one"
        );
        assert_eq!(
            exit.reason.as_deref(),
            Some("spawn failed: boom"),
            "the failure that triggered the retry is the honest exit reason"
        );

        let second = table
            .find_by_external_id(
                ProcessKind::DaemonJob,
                &process_external_id("job-retrysweep", 1),
            )
            .unwrap()
            .expect("the retry is admitted as its own process");
        assert_eq!(second.state, ProcessState::Admitted);
        assert_eq!(
            second.run_id, first.run_id,
            "a retry is a new process of the same durable run"
        );

        // Exactly one live row per job once the sync returns. That the sweep
        // runs ahead of the projections — so the window between the two is
        // never observable to a reader on another connection — is not what
        // this pins: swapping the two halves keeps every test in this suite
        // green, because a single-threaded test can only look afterwards.
        let live = table
            .list(&ProcessFilter {
                kinds: vec![ProcessKind::DaemonJob],
                live_only: true,
                ..ProcessFilter::default()
            })
            .unwrap();
        assert_eq!(
            live.len(),
            1,
            "one job must never have two live rows: {live:?}"
        );
        assert_eq!(live[0].process_id, second.process_id);
    }

    #[test]
    fn a_row_from_before_attempt_scoping_is_closed_as_lost_not_mislabelled_failed() {
        use little_monkey_lib::process_table::{
            AdmitProcess, ExitStatus, ProcessKind, ProcessState,
        };

        // An existing database can hold `daemon_job` rows keyed by the bare job
        // id. Nothing will ever project onto one again, so the sweep has to
        // close it — but it is not a failed attempt, and saying so would invent
        // a failure that never happened.
        let (paths, store, shared, _recorder, _run_id) = fixture("legacyid");
        let clock = FakeClock(Arc::new(Mutex::new(2_000)));
        let mut engine = DaemonEngine::new(
            store,
            shared,
            paths,
            DaemonConfig::default(),
            fake_adapter(),
            FakeNotifier::default(),
            clock,
            "daemon-test-owner".into(),
        );
        let legacy = {
            let table = engine.shared.process_table();
            let record = table
                .admit(
                    &AdmitProcess::new(ProcessKind::DaemonJob, "job-legacyid"),
                    1_500,
                )
                .unwrap();
            table
                .transition(&record.process_id, ProcessState::Running, None, 1_600)
                .unwrap()
        };

        engine.sync_process_table_inner(2_000).unwrap();

        let table = engine.shared.process_table();
        let swept = table.get(&legacy.process_id).unwrap().unwrap();
        assert_eq!(swept.state, ProcessState::Exited);
        let exit = swept.exit.expect("an exited row carries its exit");
        assert_eq!(exit.status, ExitStatus::Lost);
        assert_eq!(
            exit.reason.as_deref(),
            Some("process row predates attempt-scoped daemon job ids")
        );

        // And the job itself is unaffected — it gets its own attempt-scoped row.
        assert!(table
            .find_by_external_id(ProcessKind::DaemonJob, &first_attempt_id("legacyid"))
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_daemon_crash_leaves_no_process_row_claiming_to_be_running() {
        use little_monkey_lib::process_table::{ProcessKind, ProcessState};

        // Crash injection for the daemon surface. K2's acceptance names a test
        // per surface and none existed: the invariant is that an abrupt death
        // never leaves the process table asserting live work, because a row
        // stuck at `running` is indistinguishable from real work to every
        // reader — the scheduler, the listing, and the user.
        let (paths, store, shared, _recorder, run_id) = fixture("crashdaemon");
        let adapter = fake_adapter();
        let clock = FakeClock(Arc::new(Mutex::new(2_000)));
        let mut engine = DaemonEngine::new(
            store,
            shared,
            paths.clone(),
            DaemonConfig::default(),
            adapter,
            FakeNotifier::default(),
            clock.clone(),
            "daemon-test-owner".into(),
        );
        engine.tick().unwrap();
        {
            let table = engine.shared.process_table();
            let record = table
                .find_by_external_id(ProcessKind::DaemonJob, &first_attempt_id("crashdaemon"))
                .unwrap()
                .expect("the job is projected while running");
            assert_eq!(record.state, ProcessState::Running);
        }

        // The crash: the engine is dropped without any terminal transition, and
        // a fresh one comes up on the same durable state — exactly what a
        // `kill -9` of the daemon looks like to the next start.
        drop(engine);
        let restarted_store = DaemonStore::open(&paths).unwrap();
        let restarted_shared = SharedLedger::open(&paths.ledger_db).unwrap();
        *clock.0.lock().unwrap() = 5_000;
        let mut engine = DaemonEngine::new(
            restarted_store,
            restarted_shared,
            paths,
            DaemonConfig::default(),
            fake_adapter(),
            FakeNotifier::default(),
            clock,
            "daemon-test-owner".into(),
        );
        engine.recover().unwrap();
        engine.tick().unwrap();

        let table = engine.shared.process_table();
        let records = table
            .list(&little_monkey_lib::process_table::ProcessFilter {
                kinds: vec![ProcessKind::DaemonJob],
                ..Default::default()
            })
            .unwrap();
        // Recovery found the run still queued and requeued the job, so the
        // interrupted attempt is over and a second one is waiting. Nothing may
        // still be claiming to run, on either row.
        assert!(
            records
                .iter()
                .all(|record| record.state != ProcessState::Running),
            "a row left claiming to be running after a crash is a lie to every \
             reader: {records:?}"
        );

        let first: Vec<_> = records
            .iter()
            .filter(|record| record.external_id == first_attempt_id("crashdaemon"))
            .collect();
        assert_eq!(
            first.len(),
            1,
            "recovery re-admitted the first attempt instead of reconciling its \
             existing record"
        );
        assert_eq!(
            first[0].state,
            ProcessState::Exited,
            "the attempt the crash interrupted has to be closed out, not left live"
        );

        // And the retry is its own process, not the dead one resurrected —
        // which is the whole reason the id is attempt-scoped.
        let second: Vec<_> = records
            .iter()
            .filter(|record| record.external_id == process_external_id("job-crashdaemon", 1))
            .collect();
        assert_eq!(
            second.len(),
            1,
            "the requeued attempt has no row of its own: {records:?}"
        );
        assert!(
            !second[0].state.is_terminal(),
            "the retry is waiting to run, so its row must still be live"
        );
        assert_ne!(
            first[0].process_id, second[0].process_id,
            "two attempts sharing one process id is the bug this scoping removes"
        );
        assert!(engine.shared.load_run(&run_id).unwrap().is_some());
    }

    #[test]
    fn recovery_never_replays_a_confirmed_mutation() {
        let (paths, mut store, shared, recorder, run_id) = fixture("confirmed");
        recorder
            .emit(RunEvent::Started {
                engine_id: "fixture".into(),
            })
            .unwrap();
        recorder
            .emit(RunEvent::ExternalMutationPrepared {
                mutation_id: "mutation-one".into(),
                tool_call_id: "tool-one".into(),
                kind: MutationKind::Git,
                idempotency_key: Some("fixture-mutation-key".into()),
                summary: "prepare fixture push".into(),
            })
            .unwrap();
        recorder
            .emit(RunEvent::ExternalMutationConfirmed {
                mutation_id: "mutation-one".into(),
                confirmation_ref: Some("abc123".into()),
                summary: "fixture push confirmed".into(),
            })
            .unwrap();
        store
            .transition("job-confirmed", JobState::Running, 2_000, Some(77), None)
            .unwrap();
        let adapter = fake_adapter();
        let spawns = adapter.spawns.clone();
        let signals = adapter.signals.clone();
        let mut engine = DaemonEngine::new(
            store,
            shared,
            paths,
            DaemonConfig::default(),
            adapter,
            FakeNotifier::default(),
            FakeClock(Arc::new(Mutex::new(3_000))),
            "daemon-recovery".into(),
        );
        engine.recover().unwrap();
        engine.tick().unwrap();
        assert_eq!(
            *spawns.lock().unwrap(),
            0,
            "confirmed effects must not replay"
        );
        assert!(signals.lock().unwrap().contains(&ProcessSignal::Terminate));
        assert_eq!(
            engine.shared.load_run(&run_id).unwrap().unwrap().status,
            RunStatus::Failed
        );
        let confirmed = engine
            .shared
            .events(&run_id, 0, 100)
            .unwrap()
            .into_iter()
            .filter(|event| matches!(event.event, RunEvent::ExternalMutationConfirmed { .. }))
            .count();
        assert_eq!(confirmed, 1, "recovery must not duplicate confirmation");
    }

    #[test]
    fn recovery_routes_an_uncertain_prepared_mutation_to_reconciliation() {
        let (paths, mut store, shared, recorder, run_id) = fixture("uncertain");
        recorder
            .emit(RunEvent::Started {
                engine_id: "fixture".into(),
            })
            .unwrap();
        recorder
            .emit(RunEvent::ExternalMutationPrepared {
                mutation_id: "mutation-pending".into(),
                tool_call_id: "tool-pending".into(),
                kind: MutationKind::ExternalService,
                idempotency_key: None,
                summary: "uncertain fixture request".into(),
            })
            .unwrap();
        store
            .transition("job-uncertain", JobState::Running, 2_000, Some(88), None)
            .unwrap();
        let adapter = fake_adapter();
        let mut engine = DaemonEngine::new(
            store,
            shared,
            paths,
            DaemonConfig::default(),
            adapter,
            FakeNotifier::default(),
            FakeClock(Arc::new(Mutex::new(3_000))),
            "daemon-recovery".into(),
        );
        engine.recover().unwrap();
        assert_eq!(
            engine.shared.load_run(&run_id).unwrap().unwrap().status,
            RunStatus::NeedsReconciliation
        );
        assert_eq!(
            engine.store.get_job(&run_id).unwrap().unwrap().state,
            JobState::NeedsReconciliation
        );
    }
}
