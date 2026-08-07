use std::collections::HashMap;
use std::fs::OpenOptions;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use little_monkey_lib::m3_runtime_hub::M3ModelFootprint;
use little_monkey_lib::run_protocol::{
    ClientIdentity, ClientKind, ModelTargetSnapshot, PermissionDecision, RepositoryPolicy,
    RunEvent, RunStatus,
};
use little_monkey_lib::runtime_adapter::MemoryRequirement;

use crate::durable_run::{bounded_text, CliRunEventSink, DurableRunRecorder};

use super::admission::{self, Fit, Reservation};
use super::ledger::{LeaseToken, SharedLedger};
use super::scheduler::{self, Candidate, ProcessClass, Running};
use super::store::{
    map_run_status, DaemonConfig, DaemonJob, DaemonPaths, DaemonStore, JobState, SchedulerDecision,
    DECISION_ADMITTED, DECISION_HELD, DECISION_PREEMPTED, DECISION_REJECTED, DECISION_RESUMED,
};
use super::worktree::OwnedWorktree;

/// How the engine learns what this machine has.
///
/// A plain `fn` pointer rather than a fourth generic parameter or a boxed trait:
/// there is exactly one production answer and one test answer, and every
/// existing `DaemonEngine::new` call site keeps its arity.
pub type HardwareProbe = fn() -> Option<little_monkey_lib::runtime_adapter::HardwareSnapshot>;

/// The real probe. Shells out to `nvidia-smi` on CUDA hosts, which is why the
/// admission loop calls it at most once per tick and only when something is
/// actually queued.
fn probe_hardware() -> Option<little_monkey_lib::runtime_adapter::HardwareSnapshot> {
    use little_monkey_lib::m3_runtime_hub::M3HardwareProbe;
    little_monkey_lib::m3_production::SystemM3HardwareProbe
        .snapshot()
        .ok()
}

/// Collapse the per-model reservations into the one pair the fit computation
/// compares against.
fn sum_reservations(resident: &HashMap<String, MemoryRequirement>) -> MemoryRequirement {
    resident
        .values()
        .fold(admission::ZERO_MEMORY, |sum, claim| MemoryRequirement {
            ram_bytes: sum.ram_bytes.saturating_add(claim.ram_bytes),
            vram_bytes: sum.vram_bytes.saturating_add(claim.vram_bytes),
        })
}

/// How many losers a decision row names. Bounded so one row cannot grow with the
/// queue; the ranking is deterministic, so the next few are the ones that were
/// genuinely close.
const DECISION_PASSED_OVER_MAX: usize = 4;

/// Measurement tokens for the decision log. Each names a real field of a real
/// reading, so `measured_at_ms` can be that reading's own timestamp rather than
/// the time the row was written.
const MEASUREMENT_AVAILABLE_RAM: &str = "available_ram_bytes";
const MEASUREMENT_TOTAL_RAM: &str = "total_ram_bytes";
const MEASUREMENT_SUSPENDED_MS: &str = "suspended_ms";

/// The run's primary workspace root, which is the fair-share key.
///
/// The *root path* rather than `workspace_id`, because `agent_processes.workspace`
/// is documented as the owning workspace root and every other producer writes a
/// path there. Two producers keying the same directory differently would silently
/// split it into two share groups, each of which would then look half as busy as
/// it is.
///
/// ponytail: workspaces only. The roadmap asks for fair-share across workspaces
/// *and profiles*, and `agent_processes` has a `profile` column, but `RunSpec`
/// carries no profile at all — there is nothing to read. When a profile lands on
/// the spec the share key becomes the pair and the arbitration is unchanged:
/// `Candidate::workspace` becomes a composite key and `rank` never mentions it.
fn workspace_root(spec: &little_monkey_lib::run_protocol::RunSpec) -> Option<String> {
    let workspace = spec.workspace.as_ref()?;
    workspace
        .roots
        .iter()
        .find(|root| root.root_id == workspace.primary_root_id)
        .map(|root| root.canonical_path.clone())
}

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
            ProcessSignal::Pause => {
                little_monkey_lib::os_signal::suspend_process_group(self.child.id())
            }
            ProcessSignal::Resume => {
                little_monkey_lib::os_signal::resume_process_group(self.child.id())
            }
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

/// The daemon's own copy of this lived here, as `kill -TERM -<pgid>` plus up to
/// forty `kill -0` liveness polls — around forty fork+execs per terminate, and a
/// second implementation of a rule the library already owned. Its Windows arm was
/// also the only tree-kill primitive in the codebase, and the app could not reach
/// it, which is why the app's own timeouts leaked orphans. One implementation now,
/// in `os_signal`, syscall-based on unix.
fn terminate_process_group(process_id: u32) -> Result<(), String> {
    little_monkey_lib::os_signal::terminate_process_group(process_id)
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
    ProcessKind::DaemonJob
        .restart_policy()
        .permits_retry(job.attempt)
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

/// Sums the RSS of every process in the group, given `ps` output of
/// `pgid rss` rows.
///
/// Pure so the rule is testable without spawning a process tree: the platform
/// command only produces rows, and the arithmetic that decides whether a budget
/// was exceeded lives here.
///
/// `None` when the group has no rows at all — it has exited, which is not the same
/// answer as "using zero bytes" and must not read as a budget satisfied.
fn sum_group_rss_kib(ps_output: &str, process_group_id: u32) -> Option<u64> {
    let mut total: Option<u64> = None;
    for line in ps_output.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pgid), Some(rss)) = (fields.next(), fields.next()) else {
            continue;
        };
        if pgid.parse::<u32>().ok() != Some(process_group_id) {
            continue;
        }
        let Ok(kib) = rss.parse::<u64>() else {
            continue;
        };
        total = Some(total.unwrap_or(0).saturating_add(kib));
    }
    total
}

/// The job's memory, measured across its whole process group.
///
/// Was `ps -o rss= -p <pid>`, the direct child only — so a job whose child spawned
/// the actual work escaped its own memory budget entirely, which is the normal
/// case rather than an edge one: the child is a shell, and `cargo build` or a
/// model server is its grandchild. Every other signal on this process already
/// treats the pid as a group id (`process_group(0)` at spawn); only the
/// measurement did not.
///
/// `ps -eo pgid=,rss=` and filter in Rust, rather than `ps -g <pgid>`: `-g` selects
/// by process group on BSD but by *effective group* on procps, so the same command
/// would silently measure something else on Linux. This form uses only portable
/// `-o` keywords and costs one fork either way.
#[cfg(unix)]
fn process_memory_bytes(process_group_id: u32) -> Result<Option<u64>, String> {
    let output = Command::new("ps")
        .args(["-eo", "pgid=,rss="])
        .output()
        .map_err(|error| format!("Failed to inspect process memory: {error}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    let kib = sum_group_rss_kib(&String::from_utf8_lossy(&output.stdout), process_group_id);
    Ok(kib.and_then(|value| value.checked_mul(1024)))
}

/// Sums the working set of `root` and every descendant, given rows of
/// `pid parent_pid working_set_bytes`.
///
/// Windows has no process group to select on, so the tree is walked by parent.
/// Kept pure for the same reason as [`sum_group_rss_kib`], and more so: this is the
/// arm that cannot be exercised on a macOS or Linux developer machine, so the walk
/// being ordinary Rust is what makes it testable at all. Only the PowerShell
/// invocation itself goes unverified outside CI.
///
/// Iterates to a fixed point rather than recursing, so a cycle in reported parent
/// ids cannot hang the watchdog — pid reuse can legitimately produce one.
///
/// Compiled on every platform on purpose, not behind `cfg(windows)`: this machine
/// cannot build the Windows target, so gating it would leave the logic neither
/// typechecked nor tested until CI — which is exactly how the last Windows-only
/// break got there. The allow marks that the non-Windows build has no caller.
#[cfg_attr(not(windows), allow(dead_code))]
fn sum_process_tree_working_set(rows: &str, root: u32) -> Option<u64> {
    struct Row {
        pid: u32,
        parent: u32,
        bytes: u64,
    }
    let parsed: Vec<Row> = rows
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let parent = fields.next()?.parse().ok()?;
            let bytes = fields.next()?.parse().ok()?;
            Some(Row { pid, parent, bytes })
        })
        .collect();

    let mut members: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    if parsed.iter().any(|row| row.pid == root) {
        members.insert(root);
    }
    loop {
        let before = members.len();
        for row in &parsed {
            // `row.pid != row.parent` guards the self-parent case, which would
            // otherwise make any process its own descendant.
            if row.pid != row.parent && members.contains(&row.parent) {
                members.insert(row.pid);
            }
        }
        if members.len() == before {
            break;
        }
    }
    if members.is_empty() {
        return None;
    }
    Some(
        parsed
            .iter()
            .filter(|row| members.contains(&row.pid))
            .fold(0u64, |total, row| total.saturating_add(row.bytes)),
    )
}

/// See the unix version: this measures the job's whole tree, not just the child
/// the daemon spawned.
#[cfg(windows)]
fn process_memory_bytes(process_id: u32) -> Result<Option<u64>, String> {
    // One row per process, so the tree walk and the arithmetic stay in Rust where
    // they are tested. `Format-Table -HideTableHeaders` would still pad and wrap;
    // an explicit joined string does not.
    let script = "Get-CimInstance Win32_Process | ForEach-Object { \
        \"$($_.ProcessId) $($_.ParentProcessId) $($_.WorkingSetSize)\" }";
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .map_err(|error| format!("Failed to inspect process memory: {error}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(sum_process_tree_working_set(
        &String::from_utf8_lossy(&output.stdout),
        process_id,
    ))
}

struct ActiveProcess {
    process: Box<dyn ManagedProcess>,
    lease: LeaseToken,
}

/// Everything the scheduler needs about a job that is *not* in `daemon_jobs`,
/// read once and kept.
///
/// All three fields come from the run's frozen `RunSpec`, which is immutable by
/// construction — that is the whole premise of a durable run — so caching them
/// is not an optimization with a staleness risk, it is the correct way to read an
/// immutable record. It is also what makes the candidate window the *whole*
/// queue affordable: without it, widening the window meant one ledger read per
/// queued job per poll interval, which is exactly the trade the previous
/// `// ponytail:` comment declined to make blind.
/// The answer to "may this suspended job have its memory back".
enum Reacquired {
    Fits(Option<(String, MemoryRequirement)>),
    Held(String),
}

#[derive(Clone)]
struct JobFacts {
    class: ProcessClass,
    /// The run's primary workspace root, the fair-share key.
    workspace: Option<String>,
    reservation: Reservation,
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

/// Which declared budget a job blew.
///
/// The daemon enforces three of the four limits the unified process table
/// declares. All three tear the child down by cancelling the run, so without
/// this distinction a budget kill and a user pressing Stop are the same row: one
/// of them means the system worked and the other means someone changed their
/// mind, and an operator reading the ledger could not tell which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BudgetLimit {
    Wall,
    Memory,
    Output,
}

impl BudgetLimit {
    /// The [`ProcessLimits`] field this maps to.
    ///
    /// The unified vocabulary is used rather than the daemon's own column names
    /// (`max_runtime_ms`, `max_log_bytes`), because this string ends up in
    /// `ProcessExit::reason`, whose documented contract is to name the limit
    /// that fired — and the reader of that field is looking at
    /// `agent_processes`, not at `daemon_jobs`.
    ///
    /// [`ProcessLimits`]: little_monkey_lib::process_table::ProcessLimits
    const fn field(self) -> &'static str {
        match self {
            BudgetLimit::Wall => "max_wall_ms",
            BudgetLimit::Memory => "max_memory_bytes",
            BudgetLimit::Output => "max_output_bytes",
        }
    }

    /// What to call this budget in text meant for whoever launched the job.
    const fn label(self) -> &'static str {
        match self {
            BudgetLimit::Wall => "runtime",
            BudgetLimit::Memory => "memory",
            BudgetLimit::Output => "log",
        }
    }
}

/// Marker stamped into `daemon_jobs.last_error` so that a budget kill survives
/// the round-trip through the daemon database.
///
/// It has to survive one: the projection reads the job back with `get_job`
/// after the kill has already been written, so nothing of the kill is left in
/// memory by the time an exit status is chosen. The only two columns available
/// are `state`, which is CHECK-constrained to a fixed list, and `last_error`,
/// which is free text.
///
/// A typed column would be the better home. It is not used because the daemon
/// store has no migration framework at all — `DAEMON_SCHEMA` is one
/// `CREATE TABLE IF NOT EXISTS` with no version key, so neither a new state nor
/// a new column can be added without first building one, which is a change of
/// its own and not this one. To keep that future move cheap the encoding is
/// private to this module: [`limit_exceeded_reason`] is the only writer and
/// [`parse_limit_exceeded`] the only reader, so a real column replaces two
/// functions rather than a convention spread across the file.
const LIMIT_EXCEEDED_PREFIX: &str = "limit_exceeded:";

/// Encode a budget kill for storage in `last_error`.
fn limit_exceeded_reason(limit: BudgetLimit, detail: &str) -> String {
    format!("{LIMIT_EXCEEDED_PREFIX}{}: {detail}", limit.field())
}

/// The inverse: `Some` for a budget kill, carrying the reason with the marker
/// stripped but the limit name kept, which is what `ProcessExit::reason` owes
/// its reader.
fn parse_limit_exceeded(last_error: &str) -> Option<&str> {
    last_error.strip_prefix(LIMIT_EXCEEDED_PREFIX)
}

/// Terminal `JobState` → the unified exit. A non-terminal state reaching here
/// means the job vanished from the non-terminal set without a terminal state,
/// which is exactly what `Lost` is for.
fn exit_for(
    state: JobState,
    last_error: Option<&str>,
) -> little_monkey_lib::process_table::ProcessExit {
    use little_monkey_lib::process_table::{ExitStatus, ProcessExit};
    let limit = last_error.and_then(parse_limit_exceeded);
    let status = match state {
        JobState::Succeeded => ExitStatus::Succeeded,
        JobState::Failed => ExitStatus::Failed,
        // A budget kill cancels the run, because cancelling is how the child is
        // torn down, so it arrives here as `Cancelled` like any other stop. The
        // marker is the only thing that separates the two.
        JobState::Cancelled if limit.is_some() => ExitStatus::LimitExceeded,
        JobState::Cancelled => ExitStatus::Cancelled,
        JobState::NeedsReconciliation => ExitStatus::NeedsReconciliation,
        _ => ExitStatus::Lost,
    };
    ProcessExit {
        status,
        code: None,
        signal: None,
        // `limit` first so the marker never leaks into a human-facing reason,
        // whatever state it was found on.
        reason: limit.or(last_error).map(str::to_string),
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
    hardware: HardwareProbe,
    /// Per-job scheduling facts, memoized. See [`JobFacts`].
    facts: HashMap<String, JobFacts>,
    /// Jobs the *scheduler* suspended, and when, so a scheduler-driven
    /// suspension is distinguishable from an operator pressing Pause.
    ///
    /// In-memory for the same reason `immediate_termination` is: `recover` moves
    /// every active job — paused included — out of an active state, so no
    /// preemption can outlive the daemon that made it. The durable half of the
    /// suspension is the `agent_processes` suspend latch, which is what actually
    /// stops the child; this map only records *who asked*, and if that answer
    /// were lost the worst case is that the job resumes on its own.
    preempted: HashMap<String, u64>,
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
            hardware: probe_hardware,
            facts: HashMap::new(),
            preempted: HashMap::new(),
        }
    }

    /// Swap the hardware probe. Exists so admission can be tested against a
    /// machine of a chosen size rather than whichever one CI happens to run on.
    /// Production never swaps it — there is one real machine to measure.
    #[cfg(test)]
    pub fn set_hardware_probe(&mut self, probe: HardwareProbe) {
        self.hardware = probe;
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    /// Where the model hub keeps its installed inventory.
    ///
    /// Derived from `DaemonPaths` rather than from `crate::app_data_dir()` so a
    /// test daemon rooted in a temp directory looks at that directory's hub and
    /// not at the developer's real one.
    fn app_data_dir(&self) -> Option<&Path> {
        self.paths.root.parent()
    }

    /// What every admitted job holds right now, deduplicated by resident model.
    ///
    /// Read from `daemon_jobs` rather than summed over `self.active`, which is
    /// what makes a reservation outlive the process that took it: the map
    /// vanishes with the daemon, the rows do not. It also means the accounting
    /// is correct during the window between a restart and `recover`, when jobs
    /// are still marked active but nothing is in `self.active` yet.
    fn committed(&self) -> Result<HashMap<String, MemoryRequirement>, String> {
        Ok(self
            .store
            .committed_reservations()?
            .into_iter()
            .map(|(key, ram_bytes, vram_bytes)| {
                (
                    key,
                    MemoryRequirement {
                        ram_bytes,
                        vram_bytes,
                    },
                )
            })
            .collect())
    }

    /// What a queued job will make resident: the target frozen at submission,
    /// plus whatever the model hub knows about that target's model id.
    ///
    /// An unreadable run reserves nothing rather than blocking: the admission
    /// bound exists to stop thrashing, and failing a job because its ledger row
    /// could not be read would be a stricter policy than the one asked for.
    ///
    /// `footprints` memoizes the hub lookup for this tick. A queue of turns
    /// against one model is the common case and it would otherwise re-read the
    /// same state file once per queued job.
    fn read_facts(
        &self,
        job: &DaemonJob,
        footprints: &mut HashMap<String, M3ModelFootprint>,
    ) -> JobFacts {
        let Some(run) = job
            .run_id
            .as_deref()
            .and_then(|run_id| self.shared.load_run(run_id).ok().flatten())
        else {
            // Unreadable: reserves nothing, and takes the *lowest* class rather
            // than a middling one. A job whose spec could not be read has not
            // proved it is interactive, and guessing upward would let an
            // unreadable run outrank a desktop turn.
            return JobFacts {
                class: ProcessClass::Maintenance,
                workspace: None,
                reservation: Reservation::Remote,
            };
        };
        let target = &run.spec.target;
        let model_id = match target {
            ModelTargetSnapshot::Provider { .. } => None,
            ModelTargetSnapshot::Ollama { model, .. } => Some(model.clone()),
            ModelTargetSnapshot::ManagedLlama { model_id, .. } => Some(model_id.clone()),
        };
        let footprint = match model_id {
            Some(model_id) => footprints
                .entry(model_id.clone())
                .or_insert_with(|| match self.app_data_dir() {
                    Some(app_data) => little_monkey_lib::m3_runtime_hub::installed_model_footprint(
                        app_data, &model_id,
                    ),
                    None => M3ModelFootprint::Unknown,
                })
                .clone(),
            None => M3ModelFootprint::Unknown,
        };
        JobFacts {
            class: scheduler::classify(&run.spec.kind, job.priority),
            workspace: workspace_root(&run.spec),
            reservation: admission::reservation(target, &footprint),
        }
    }

    /// [`Self::read_facts`], memoized for the life of the daemon.
    fn facts_for(
        &mut self,
        job: &DaemonJob,
        footprints: &mut HashMap<String, M3ModelFootprint>,
    ) -> JobFacts {
        if let Some(cached) = self.facts.get(&job.job_id) {
            return cached.clone();
        }
        // Bounded rather than pruned per exit. The cache is derived from
        // immutable data, so a stale entry for a job that has gone terminal is
        // harmless — it is only wasted bytes — and dropping the lot when it grows
        // past four queues' worth costs one ledger read per live job, once.
        if self.facts.len() > usize::try_from(self.config.max_queue).unwrap_or(128) * 4 {
            self.facts.clear();
        }
        let facts = self.read_facts(job, footprints);
        self.facts.insert(job.job_id.clone(), facts.clone());
        facts
    }

    /// The fair-share charge for one workspace: measured `cpu_time_ms` summed
    /// over its most recent processes.
    ///
    /// Read from the unified process table, which is where K6's per-process
    /// measurement lands, so this is a real measured number rather than a proxy
    /// for one. A workspace nothing has ever measured charges zero, which is
    /// correct — it has not had the device.
    ///
    /// A run with no workspace snapshot at all charges nothing and shares nothing:
    /// model-only chat has no workspace to be fair between.
    /// ponytail: one bounded query per distinct workspace per tick, memoized only
    /// within the tick. The charge moves as jobs run, so a longer-lived cache
    /// would be wrong rather than merely stale; the cost is linear in the number
    /// of *distinct workspaces* with queued work, which is a small number in every
    /// shape this runs in today. If it stops being small, the fix is a rolling
    /// per-workspace total maintained on exit rather than recomputed on read.
    fn workspace_charge(&self, workspace: Option<&str>) -> u64 {
        use little_monkey_lib::process_table::ProcessUsageFilter;
        let Some(workspace) = workspace else {
            return 0;
        };
        self.shared
            .process_table()
            .usage_totals(&ProcessUsageFilter {
                workspace: Some(workspace.to_string()),
                limit: Some(scheduler::FAIR_SHARE_WINDOW_ROWS),
                ..ProcessUsageFilter::default()
            })
            .ok()
            .and_then(|totals| totals.cpu_time_ms.value)
            .unwrap_or(0)
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
        // Reservations belong to processes that no longer exist. The loop above
        // already moved every active job out of an active state, so this is the
        // belt to that braces: any row still carrying a claim after it is one
        // nothing released, and holding it would shrink the budget of a daemon
        // that has nothing running at all.
        self.store.sweep_stale_reservations()?;
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
            self.schedule(now)?;
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

    /// One scheduling pass (K8).
    ///
    /// The order of operations matters and is not arbitrary. Candidates are
    /// gathered and ranked *before* anything is admitted, because preemption and
    /// preemption-release both need to know what is waiting; releases happen
    /// before admissions, so a job whose suspension is no longer justified gets
    /// its slot back in the same tick it becomes eligible rather than the next
    /// one; and admissions walk the ranking in order, holding rather than
    /// stopping at whatever does not fit.
    fn schedule(&mut self, now: u64) -> Result<(), String> {
        // The candidate window is the whole queue, not the free-slot count.
        //
        // This is the starvation bug the `// ponytail:` comment that used to live
        // here named: with a window as wide as the free slots, a 2 GiB job queued
        // behind `concurrency` larger *held* jobs was never even looked at until
        // one of them left, because the held jobs consumed the window itself. The
        // reason the window was narrow was cost — one ledger read per candidate
        // per poll interval — and `JobFacts` removes that reason: a run's spec is
        // immutable, so it is read once per job for the life of the daemon and the
        // window can be as wide as the queue is allowed to be.
        let queued = self.store.ready_jobs(self.config.max_queue)?;
        let mut footprints = HashMap::new();
        let mut charges: HashMap<String, u64> = HashMap::new();
        let mut jobs: HashMap<String, DaemonJob> = HashMap::new();
        let mut facts_by_job: HashMap<String, JobFacts> = HashMap::new();
        let mut candidates = Vec::new();
        for job in queued {
            // A retry waits out its backoff. Skipping rather than sleeping keeps
            // the tick non-blocking, so one backing-off job never delays every
            // other queued one — it is simply passed over until a later tick.
            if !backoff_elapsed(&job, now) {
                continue;
            }
            let facts = self.facts_for(&job, &mut footprints);
            let charged_ms = match facts.workspace.as_deref() {
                Some(workspace) => match charges.get(workspace) {
                    Some(charge) => *charge,
                    None => {
                        let charge = self.workspace_charge(Some(workspace));
                        charges.insert(workspace.to_string(), charge);
                        charge
                    }
                },
                None => 0,
            };
            candidates.push(Candidate {
                job_id: job.job_id.clone(),
                class: facts.class,
                workspace: facts.workspace.clone(),
                priority: job.priority,
                created_at_ms: job.created_at_ms,
                charged_ms,
            });
            facts_by_job.insert(job.job_id.clone(), facts);
            jobs.insert(job.job_id.clone(), job);
        }
        scheduler::rank(&mut candidates, now);

        self.release_preemptions(&candidates, now)?;

        let mut slots = usize::try_from(self.config.concurrency)
            .unwrap_or(usize::MAX)
            .saturating_sub(self.active.len());
        if slots == 0 || candidates.is_empty() {
            return Ok(());
        }

        // Probed once per pass and only when something is actually queued: the
        // real probe forks `nvidia-smi` on CUDA hosts, and an idle daemon must not
        // pay for that every poll interval.
        //
        // `None` means the probe failed, and that falls through to the pre-K7
        // behaviour deliberately. A machine we could not measure is not a reason
        // to refuse work — it is a reason to stop claiming the queue is
        // resource-aware for this tick.
        let snapshot = (self.hardware)();
        // Keyed by resident model, so admitting a second job against a model
        // already loaded adds nothing to the total.
        let mut resident = self.committed()?;
        let mut running = self.running_jobs()?;

        for index in 0..candidates.len() {
            if slots == 0 {
                break;
            }
            let candidate = candidates[index].clone();
            let Some(job) = jobs.get(&candidate.job_id).cloned() else {
                continue;
            };
            let reservation = facts_by_job
                .get(&candidate.job_id)
                .map(|facts| facts.reservation.clone())
                .unwrap_or(Reservation::Remote);
            // What this job was chosen over: the candidates ranked immediately
            // behind it. Bounded, because a decision row must not grow with the
            // queue.
            let passed_over: Vec<String> = candidates[index + 1..]
                .iter()
                .take(DECISION_PASSED_OVER_MAX)
                .map(|other| other.job_id.clone())
                .collect();
            let runner_up = candidates.get(index + 1);
            // Set only by an admission that actually measured something. A tick
            // with no hardware snapshot records no claim rather than a zero-byte
            // one: it did not decide this job fitted, so it must not leave a row
            // saying the model is free.
            let mut claim: Option<(String, MemoryRequirement)> = None;
            if let Some(snapshot) = snapshot.as_ref() {
                let already = reservation
                    .model_key()
                    .and_then(|key| resident.get(key))
                    .cloned();
                let outcome = match already {
                    // Already resident: this job is another turn against a model
                    // whose memory is paid for. It still records the same claim,
                    // which is what keeps the model reserved until the *last*
                    // holder exits.
                    Some(paid) => Fit::Fits { claim: paid },
                    None => admission::fit(&reservation, &sum_reservations(&resident), snapshot),
                };
                match outcome {
                    Fit::Fits { claim: fitted } => {
                        claim = reservation.model_key().map(|key| (key.to_string(), fitted));
                    }
                    Fit::Unmeasured => {
                        if let Reservation::Unmeasured { model_id } = &reservation {
                            eprintln!(
                                "monkey daemon: admitting job '{}' unmeasured — no installed footprint for model '{model_id}', so this admission was not bounded by memory",
                                job.job_id
                            );
                        }
                    }
                    Fit::Hold {
                        resource,
                        shortfall_bytes,
                    } => {
                        // Somebody lower may be able to step aside. Suspension
                        // takes effect on the next tick — `tick_active` is what
                        // delivers the signal and releases the claim — so this
                        // candidate is still held now, and its hold reason says
                        // what it is waiting for rather than just "memory".
                        let victim = scheduler::preemption_victim(
                            candidate.effective_class(now),
                            resource,
                            shortfall_bytes,
                            &running,
                        )
                        .map(|victim| victim.job_id.clone());
                        let mut reason = format!(
                            "needs {} more {} than is free",
                            admission::describe_bytes(shortfall_bytes),
                            resource.label()
                        );
                        if let Some(victim_id) = victim {
                            if self.preempt(
                                &victim_id,
                                &candidate,
                                now,
                                resource,
                                shortfall_bytes,
                                snapshot,
                            )? {
                                reason =
                                    format!("{reason}; suspended '{victim_id}' to make room");
                                if let Some(entry) = running
                                    .iter_mut()
                                    .find(|entry| entry.job_id == victim_id)
                                {
                                    // Nothing left to give, so it cannot be
                                    // chosen again in this same pass.
                                    entry.preempted = true;
                                }
                            }
                        }
                        // Stays `queued`. This is the case that used to be an
                        // admission, and then a thrash.
                        //
                        // The decision is logged only when the hold reason
                        // actually changed, for the same reason `hold` only writes
                        // then: admission re-evaluates every held job four times a
                        // second, and a row per evaluation would churn the whole
                        // 512-row log every two minutes and bury the decisions
                        // worth reading.
                        if self.hold(&job, &reason)? {
                            // The other half of the starvation bound, recorded:
                            // how much longer this job can still be outranked
                            // before aging puts it at the head of the ranking.
                            let bound = scheduler::starvation_bound_ms();
                            let promoted_in_ms = bound
                                .saturating_sub(now.saturating_sub(candidate.created_at_ms).min(bound));
                            self.log_decision(
                                &candidate,
                                now,
                                DECISION_HELD,
                                passed_over,
                                format!("{reason}; ranked first in at most {promoted_in_ms} ms"),
                                MEASUREMENT_AVAILABLE_RAM,
                                Some(snapshot.available_ram_bytes),
                                Some(snapshot.captured_at_ms),
                            );
                        }
                        continue;
                    }
                    Fit::Never {
                        resource,
                        shortfall_bytes,
                    } => {
                        self.reject_oversized(&job, now, resource, shortfall_bytes)?;
                        self.log_decision(
                            &candidate,
                            now,
                            DECISION_REJECTED,
                            passed_over,
                            format!(
                                "needs {} more {} than this machine has",
                                admission::describe_bytes(shortfall_bytes),
                                resource.label()
                            ),
                            MEASUREMENT_TOTAL_RAM,
                            Some(snapshot.total_ram_bytes),
                            Some(snapshot.captured_at_ms),
                        );
                        continue;
                    }
                }
            }
            let job_id = job.job_id.clone();
            self.start_job(job, now, claim.as_ref())?;
            // Only count what actually became active: `start_job` returns `Ok`
            // without admitting when another owner holds the lease.
            if self.active.contains_key(&job_id) {
                slots -= 1;
                if let Some((key, claimed)) = claim {
                    resident.entry(key).or_insert(claimed);
                }
                // The measurement cited for an admission is the ranking key that
                // put this job first, not the memory reading — the memory reading
                // is why it was *allowed* to start, and goes in the detail, but
                // the ranking key is why it went before the others.
                let (measurement, measured_value) =
                    scheduler::deciding_key(&candidate, runner_up, now);
                let detail = match snapshot.as_ref() {
                    Some(snapshot) => format!(
                        "priority {}, workspace charged {} ms, available RAM {}",
                        candidate.priority,
                        candidate.charged_ms,
                        admission::describe_bytes(snapshot.available_ram_bytes)
                    ),
                    None => format!(
                        "priority {}, workspace charged {} ms; hardware unmeasured, so this admission was bounded by concurrency alone",
                        candidate.priority, candidate.charged_ms
                    ),
                };
                self.log_decision(
                    &candidate,
                    now,
                    DECISION_ADMITTED,
                    passed_over,
                    detail,
                    measurement,
                    measured_value,
                    Some(now),
                );
            }
        }
        Ok(())
    }

    /// Every running job that could be asked to step aside.
    ///
    /// Only a job that holds a *measured* reservation is here, and only if it is
    /// the sole holder of its resident model. Both exclusions are the same point:
    /// preemption is worth doing when it returns accountable bytes, and suspending
    /// one of two jobs sharing a loaded model returns none of them — the model
    /// stays resident for the other holder.
    fn running_jobs(&mut self) -> Result<Vec<Running>, String> {
        let reservations = self.store.job_reservations()?;
        let mut holders: HashMap<&str, usize> = HashMap::new();
        for (_, model_key, _, _) in &reservations {
            *holders.entry(model_key.as_str()).or_insert(0) += 1;
        }
        let active = self.store.active_jobs()?;
        let mut footprints = HashMap::new();
        let mut out = Vec::new();
        for job in active {
            // Only a `running` job has anything to suspend. One already paused,
            // cancelling, or waiting on an approval is either stopped or on its
            // way out.
            if job.state != JobState::Running {
                continue;
            }
            let Some((_, model_key, ram_bytes, vram_bytes)) = reservations
                .iter()
                .find(|(job_id, _, _, _)| *job_id == job.job_id)
            else {
                continue;
            };
            if holders.get(model_key.as_str()).copied().unwrap_or(0) > 1 {
                continue;
            }
            let preempted = self.preempted.contains_key(&job.job_id);
            let started_at_ms = job.started_at_ms.unwrap_or(job.created_at_ms);
            let facts = self.facts_for(&job, &mut footprints);
            out.push(Running {
                job_id: job.job_id.clone(),
                class: facts.class,
                ram_bytes: *ram_bytes,
                vram_bytes: *vram_bytes,
                preempted,
                started_at_ms,
            });
        }
        Ok(out)
    }

    /// Ask a running job to step aside, by setting the durable suspend latch K2
    /// already built.
    ///
    /// The latch rather than `pause_requested` directly, and this is the
    /// load-bearing detail: `apply_signal_intent` copies intent one way, table →
    /// daemon, and it is *level-triggered*. Setting `pause_requested` without the
    /// latch would be undone on the very next tick, when that function saw the
    /// latch clear and the daemon bit set and dutifully cleared the bit. Going
    /// through the latch also means a preemption is visible in
    /// `monkey processes list` like any other suspension, and survives a restart
    /// exactly as far as the suspended job itself does.
    ///
    /// Returns whether the latch was actually set — a job whose process row has
    /// gone is not an error, it is a job that has already left.
    #[allow(clippy::too_many_arguments)]
    fn preempt(
        &mut self,
        victim_job_id: &str,
        claimant: &Candidate,
        now: u64,
        resource: admission::Resource,
        shortfall_bytes: u64,
        snapshot: &little_monkey_lib::runtime_adapter::HardwareSnapshot,
    ) -> Result<bool, String> {
        use little_monkey_lib::process_table::{ProcessKind, ProcessSignal as TableSignal};

        let Some(victim) = self.store.get_job(victim_job_id)? else {
            return Ok(false);
        };
        let now_ms = i64::try_from(now).map_err(|_| "clock is beyond bounds".to_string())?;
        let reason = format!(
            "preempted so '{}' ({}) can start",
            claimant.job_id,
            claimant.effective_class(now).token()
        );
        let external_id = process_external_id(&victim.job_id, attempt_ordinal(&victim));
        let latched = {
            let table = self.shared.process_table();
            match table
                .find_by_external_id(ProcessKind::DaemonJob, &external_id)
                .map_err(|error| error.to_string())?
            {
                // Fail-soft, and specifically because of `AlreadyExited`: the
                // victim was `running` when `running_jobs` read it, but its
                // process row can go terminal between that read and this write,
                // and a job that has already left is not an error — it is a
                // preemption that is no longer needed.
                Some(record) => match table.signal(
                    &record.process_id,
                    TableSignal::Suspend,
                    Some(&reason),
                    now_ms,
                ) {
                    Ok(_) => true,
                    Err(error) => {
                        eprintln!(
                            "monkey daemon: could not suspend '{}': {error}",
                            victim.job_id
                        );
                        false
                    }
                },
                None => false,
            }
        };
        if !latched {
            return Ok(false);
        }
        self.preempted.insert(victim.job_id.clone(), now);
        eprintln!("monkey daemon: {} — {reason}", victim.job_id);
        let victim_class = self
            .facts
            .get(&victim.job_id)
            .map(|facts| facts.class)
            .unwrap_or(ProcessClass::Batch);
        self.store
            .record_decision(&SchedulerDecision {
                decided_at_ms: now,
                job_id: victim.job_id.clone(),
                outcome: DECISION_PREEMPTED.to_string(),
                process_class: victim_class.token().to_string(),
                effective_class: victim_class.token().to_string(),
                workspace: self
                    .facts
                    .get(&victim.job_id)
                    .and_then(|facts| facts.workspace.clone()),
                passed_over: vec![claimant.job_id.clone()],
                detail: format!(
                    "{reason}; it was short {} of {}",
                    admission::describe_bytes(shortfall_bytes),
                    resource.label()
                ),
                measurement: MEASUREMENT_AVAILABLE_RAM.to_string(),
                measured_value: Some(snapshot.available_ram_bytes),
                measured_at_ms: Some(snapshot.captured_at_ms),
            })
            .unwrap_or_else(|error| {
                eprintln!("monkey daemon: could not record preemption: {error}")
            });
        Ok(true)
    }

    /// Clear the suspend latch on preempted jobs whose preemption is no longer
    /// justified.
    ///
    /// "No longer justified" is: nothing queued outranks the suspended job any
    /// more. `PREEMPTION_MIN_SUSPENDED_MS` is the floor that stops one interactive
    /// job arriving and leaving every poll interval from suspending and resuming
    /// the same background job four times a second.
    ///
    /// Clearing the latch is all this does. Whether the job can actually have its
    /// memory back is decided in `tick_active`, which is the only place that can
    /// ask, and a job that no longer fits stays suspended there rather than being
    /// resumed into a machine that has no room for it.
    fn release_preemptions(&mut self, candidates: &[Candidate], now: u64) -> Result<(), String> {
        use little_monkey_lib::process_table::{ProcessKind, ProcessSignal as TableSignal};

        if self.preempted.is_empty() {
            return Ok(());
        }
        let mut releasable: Vec<(String, ProcessClass)> = Vec::new();
        for (job_id, suspended_at) in &self.preempted {
            if now.saturating_sub(*suspended_at) < scheduler::PREEMPTION_MIN_SUSPENDED_MS {
                continue;
            }
            let class = self
                .facts
                .get(job_id)
                .map(|facts| facts.class)
                .unwrap_or(ProcessClass::Batch);
            let still_outranked = candidates
                .iter()
                .any(|candidate| candidate.effective_class(now).rank() < class.rank());
            if !still_outranked {
                releasable.push((job_id.clone(), class));
            }
        }
        let now_ms = i64::try_from(now).map_err(|_| "clock is beyond bounds".to_string())?;
        for (job_id, class) in releasable {
            // Dropped from the map either way. A job that has gone terminal since
            // it was suspended has nothing to release, and leaving it here would
            // retry the same dead lookup on every tick forever.
            self.preempted.remove(&job_id);
            let Some(job) = self.store.get_job(&job_id)? else {
                continue;
            };
            if job.state.is_terminal() {
                continue;
            }
            let external_id = process_external_id(&job.job_id, attempt_ordinal(&job));
            let cleared = {
                let table = self.shared.process_table();
                match table
                    .find_by_external_id(ProcessKind::DaemonJob, &external_id)
                    .map_err(|error| error.to_string())?
                {
                    // Fail-soft for the same reason `preempt` is: the row can
                    // reach a terminal state between the two reads, and
                    // `AlreadyExited` then means the release is moot rather than
                    // that the tick failed.
                    Some(record) => match table.signal(
                        &record.process_id,
                        TableSignal::Resume,
                        Some("preemption released; nothing queued outranks it"),
                        now_ms,
                    ) {
                        Ok(_) => true,
                        Err(error) => {
                            eprintln!("monkey daemon: could not resume '{job_id}': {error}");
                            false
                        }
                    },
                    None => false,
                }
            };
            if !cleared {
                continue;
            }
            let workspace = self
                .facts
                .get(&job_id)
                .and_then(|facts| facts.workspace.clone());
            self.store
                .record_decision(&SchedulerDecision {
                    decided_at_ms: now,
                    job_id,
                    outcome: DECISION_RESUMED.to_string(),
                    process_class: class.token().to_string(),
                    effective_class: class.token().to_string(),
                    workspace,
                    passed_over: Vec::new(),
                    detail: "nothing queued outranks it any more".to_string(),
                    measurement: MEASUREMENT_SUSPENDED_MS.to_string(),
                    measured_value: Some(scheduler::PREEMPTION_MIN_SUSPENDED_MS),
                    measured_at_ms: Some(now),
                })
                .unwrap_or_else(|error| {
                    eprintln!("monkey daemon: could not record resume: {error}")
                });
        }
        Ok(())
    }

    /// Append one decision, fail-soft.
    ///
    /// Swallowed for the same reason the process-table projection is: the log is
    /// an inspection surface, and the one thing worse than a missing row is a job
    /// that refused to run to protect one.
    #[allow(clippy::too_many_arguments)]
    fn log_decision(
        &mut self,
        candidate: &Candidate,
        now: u64,
        outcome: &str,
        passed_over: Vec<String>,
        detail: String,
        measurement: &str,
        measured_value: Option<u64>,
        measured_at_ms: Option<u64>,
    ) {
        if let Err(error) = self.store.record_decision(&SchedulerDecision {
            decided_at_ms: now,
            job_id: candidate.job_id.clone(),
            outcome: outcome.to_string(),
            process_class: candidate.class.token().to_string(),
            effective_class: candidate.effective_class(now).token().to_string(),
            workspace: candidate.workspace.clone(),
            passed_over,
            detail,
            measurement: measurement.to_string(),
            measured_value,
            measured_at_ms,
        }) {
            eprintln!("monkey daemon: could not record scheduling decision: {error}");
        }
    }

    /// Fold this job's per-process measurement into the resource ledger (K6).
    ///
    /// **What is measured, and what is not.** This samples the *supervised child's
    /// own pid* — the `monkey task` process that runs the agent loop — so
    /// `cpu_time_ms`, `peak_rss_bytes`, `bytes_read` and `bytes_written` describe
    /// that process and not its descendants. The memory *budget* deliberately
    /// measures something wider: `process_memory_bytes` sums the whole process
    /// group, because a job whose tool spawned `cargo build` must not escape its
    /// ceiling through a grandchild. Those two scopes are genuinely different and
    /// the difference is kept rather than papered over — a budget that only
    /// watched one pid would be trivially escapable, and a ledger row whose
    /// `peak_rss_bytes` silently included every tool subprocess would answer a
    /// question nobody asked of a row that represents the agent.
    ///
    /// So: the group total drives the budget, the per-pid sample feeds the ledger,
    /// and `peak_rss_bytes` on an `agent_processes` row means "the high-water
    /// footprint of the agent process itself".
    ///
    /// ponytail: no group-wide CPU or disk roll-up. `process_usage::sample` reads
    /// one pid, and summing a process group's CPU needs a per-platform walk of the
    /// group — the same walk `sum_group_rss_kib` and
    /// `sum_process_tree_working_set` already do for memory, extended to two more
    /// counters. The upgrade path is to widen those two functions and feed their
    /// output through the same `accumulate_usage` call as here; nothing else
    /// changes.
    ///
    /// Fail-soft, like every other write to the process table.
    fn record_usage(&self, job: &DaemonJob, now: u64) {
        use little_monkey_lib::process_table::ProcessKind;

        let Some(pid) = self.active.get(&job.job_id).map(|active| active.process.id()) else {
            return;
        };
        let Ok(now_ms) = i64::try_from(now) else {
            return;
        };
        let sample = little_monkey_lib::process_usage::sample(i64::from(pid));
        let table = self.shared.process_table();
        let record = table.find_by_external_id(
            ProcessKind::DaemonJob,
            &process_external_id(&job.job_id, attempt_ordinal(job)),
        );
        match record {
            Ok(Some(record)) => {
                // `accumulate_usage` folds by maximum in SQL, so an accumulator
                // in the engine would be a second copy of a rule the ledger
                // already enforces — and a failed read comes back as `NULL`,
                // which the same statement leaves alone rather than treating as a
                // zero. Nothing here has to remember the previous reading.
                if let Err(error) = table.accumulate_usage(&record.process_id, &sample, now_ms) {
                    eprintln!(
                        "monkey daemon: could not record usage for '{}': {error}",
                        job.job_id
                    );
                }
            }
            Ok(None) => {}
            Err(error) => eprintln!(
                "monkey daemon: could not find the process row for '{}': {error}",
                job.job_id
            ),
        }
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

        let now_ms =
            i64::try_from(now).map_err(|_| "clock is beyond protocol bounds".to_string())?;
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
                    reason: Some("process row predates attempt-scoped daemon job ids".to_string()),
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
            // Set so fair-share can charge this job's device time to the right
            // workspace: `usage_totals` selects on this column, and a row without
            // it is invisible to the charge. `reconcile` only writes it on admit,
            // which is fine — the scheduling pass runs before this projection, so
            // the fact is already cached by the time a queued job is first
            // projected.
            .with_workspace(
                self.facts
                    .get(&job.job_id)
                    .and_then(|facts| facts.workspace.clone()),
            )
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

    /// Leave a job queued and record why.
    ///
    /// The unified process table already reports a held job as
    /// `ProcessState::Admitted` — `process_state_for` maps `Queued` there — so
    /// the roadmap's "sits in `admitted`" holds without a new `JobState`. What
    /// `Admitted` alone cannot say is *why* it is still sitting there, which is
    /// the question an operator actually has, so the reason goes on the row.
    ///
    /// Written only when it changes. Admission re-evaluates every held job every
    /// tick, and re-writing the same sentence four times a second would turn one
    /// waiting job into a write loop and a log flood.
    ///
    /// Returns whether anything was written, which is also what gates the
    /// scheduling decision log — a decision worth recording is one that changed
    /// something.
    fn hold(&mut self, job: &DaemonJob, reason: &str) -> Result<bool, String> {
        if job.hold_reason.as_deref() == Some(reason) {
            return Ok(false);
        }
        eprintln!("monkey daemon: holding job '{}' — {reason}", job.job_id);
        self.store.record_hold(&job.job_id, Some(reason))?;
        Ok(true)
    }

    /// Whether a suspended job may have the claim it released back.
    ///
    /// `Fits(None)` and `Held` are not the same answer with different labels:
    /// `None` means there is nothing to re-claim (a provider run, or a machine
    /// nothing could measure), which is a resume, and `Held` means the bytes are
    /// gone, which is not.
    fn reacquire(&mut self, job: &DaemonJob) -> Result<Reacquired, String> {
        let Some(snapshot) = (self.hardware)() else {
            // A machine we could not measure is not a reason to refuse to resume
            // a job that was already running a moment ago.
            return Ok(Reacquired::Fits(None));
        };
        let mut footprints = HashMap::new();
        let reservation = self.facts_for(job, &mut footprints).reservation;
        let resident = self.committed()?;
        let already = reservation
            .model_key()
            .and_then(|key| resident.get(key))
            .cloned();
        let outcome = match already {
            // Another holder kept the model resident, so there is nothing to buy
            // back — recording the same claim is what keeps it reserved until the
            // last of them exits.
            Some(paid) => Fit::Fits { claim: paid },
            None => admission::fit(&reservation, &sum_reservations(&resident), &snapshot),
        };
        Ok(match outcome {
            Fit::Fits { claim } => Reacquired::Fits(
                reservation
                    .model_key()
                    .map(|key| (key.to_string(), claim)),
            ),
            Fit::Unmeasured => Reacquired::Fits(None),
            Fit::Hold {
                resource,
                shortfall_bytes,
            } => Reacquired::Held(format!(
                "suspended and cannot resume: needs {} more {} than is free",
                admission::describe_bytes(shortfall_bytes),
                resource.label()
            )),
            // The machine shrank under it — a card was unplugged, or the reserve
            // moved. Still a hold rather than a failure: the job is intact and
            // suspended, and failing work the operator can fix by closing
            // something is a worse answer than leaving it parked.
            Fit::Never {
                resource,
                shortfall_bytes,
            } => Reacquired::Held(format!(
                "suspended and cannot resume: needs {} more {} than this machine now has",
                admission::describe_bytes(shortfall_bytes),
                resource.label()
            )),
        })
    }

    /// A job that cannot fit on this machine even when it is idle is failed
    /// without ever being spawned.
    ///
    /// Holding it instead would be waiting for memory that cannot appear, and
    /// starting it is what the memory watchdog used to clean up after — a kill
    /// several minutes in, indistinguishable from the job's own failure.
    fn reject_oversized(
        &mut self,
        job: &DaemonJob,
        now: u64,
        resource: admission::Resource,
        shortfall_bytes: u64,
    ) -> Result<(), String> {
        let reason = format!(
            "needs {} more {} than this machine has; refused at admission rather than started and killed",
            admission::describe_bytes(shortfall_bytes),
            resource.label()
        );
        if let Some(run_id) = job.run_id.as_deref() {
            self.fail_run(run_id, "daemon_admission_rejected", &reason)?;
        }
        self.store
            .transition(&job.job_id, JobState::Failed, now, None, Some(&reason))
    }

    fn start_job(
        &mut self,
        job: DaemonJob,
        now: u64,
        claim: Option<&(String, MemoryRequirement)>,
    ) -> Result<(), String> {
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
                if let Some((model_key, claim)) = claim {
                    self.store.record_reservation(
                        &job.job_id,
                        model_key,
                        claim.ram_bytes,
                        claim.vram_bytes,
                    )?;
                }
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

        // Sampled while the process is alive and before every branch that can tear
        // it down — cancellation, a budget kill, a clean exit. Peak resident size
        // is unreadable once a pid is gone, so whichever of those fires later in
        // this same tick must not be the reason the last reading was lost.
        self.record_usage(&job, now);

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
            // The suspended half of the reservation round-trip (K8). A suspended
            // job gives its claim back, which is what lets a higher class start —
            // and it is what makes resuming a decision rather than a formality.
            //
            // Accounting, not eviction, and the distinction is worth being blunt
            // about: `SIGSTOP` does not return one page to the OS, and for a local
            // model the weights are resident in a model server that is not even in
            // this process group. Releasing the claim trades a possible swap for
            // interactive latency, which is the trade the roadmap asks for.
            //
            // ponytail: the model stays loaded. Actually reclaiming those bytes
            // means asking the runtime hub to unload the model, which is a
            // cross-component request this engine has no seam for yet; the upgrade
            // path is one call here, and the accounting above is already correct
            // for it.
            self.store.release_reservation(job_id)?;
            self.store
                .transition(job_id, JobState::Paused, now, Some(process_id), None)?;
            return Ok(());
        }
        if !job.pause_requested && job.state == JobState::Paused {
            // The reacquire half, and the interesting one: the memory this job
            // let go of may be gone. It is checked here rather than by the
            // scheduler because this is the only place a resume actually happens —
            // an operator clearing a pause by hand arrives here too, and must not
            // be able to resume a job into a machine with no room for it.
            //
            // A job that cannot have its claim back stays `paused` with a hold
            // reason and no signal delivered. That is the no-thrash property:
            // nothing is sent to the child, so re-evaluating on the next tick
            // costs one fit computation and changes nothing until the answer
            // changes.
            let claim = match self.reacquire(&job)? {
                Reacquired::Fits(claim) => claim,
                Reacquired::Held(reason) => {
                    self.hold(&job, &reason)?;
                    return Ok(());
                }
            };
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
            match claim {
                Some((model_key, claim)) => self.store.record_reservation(
                    job_id,
                    &model_key,
                    claim.ram_bytes,
                    claim.vram_bytes,
                )?,
                // Nothing to re-claim, but a stale hold reason from an earlier
                // failed resume would still be sitting on the row.
                None if job.hold_reason.is_some() => self.store.record_hold(job_id, None)?,
                None => {}
            }
            self.store
                .transition(job_id, JobState::Running, now, Some(process_id), None)?;
        }

        // Each of the three budgets reports the measurement that tripped it, not
        // only that something did: "held 700 MiB against a 512 MiB budget" tells
        // whoever reads the exit whether the budget was wrong or the job was.
        if let Some(started) = job.started_at_ms {
            let elapsed = now.saturating_sub(started);
            if elapsed > job.max_runtime_ms {
                self.cancel_for_budget(
                    job_id,
                    run_id,
                    now,
                    BudgetLimit::Wall,
                    &format!(
                        "ran for {elapsed} ms against a {} ms budget",
                        job.max_runtime_ms
                    ),
                )?;
                return Ok(());
            }
        }
        if let Some(max_memory) = job.max_memory_bytes {
            let used = self
                .active
                .get(job_id)
                .ok_or_else(|| "active process disappeared".to_string())?
                .process
                .memory_bytes()?;
            if let Some(used) = used.filter(|used| *used > max_memory) {
                self.cancel_for_budget(
                    job_id,
                    run_id,
                    now,
                    BudgetLimit::Memory,
                    &format!(
                        "the process group held {used} bytes against a {max_memory} byte budget"
                    ),
                )?;
                return Ok(());
            }
        }
        let log_path = self.paths.logs.join(format!("{}.log", job.job_id));
        if let Some(written) = std::fs::metadata(log_path)
            .ok()
            .map(|metadata| metadata.len())
            .filter(|written| *written > job.max_log_bytes)
        {
            self.cancel_for_budget(
                job_id,
                run_id,
                now,
                BudgetLimit::Output,
                &format!(
                    "the log reached {written} bytes against a {} byte budget",
                    job.max_log_bytes
                ),
            )?;
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

    /// Tear a job down for blowing one of its budgets.
    ///
    /// Two spellings of the same fact, because they have different readers. The
    /// run ledger gets prose, since its events are shown to whoever launched the
    /// job. `daemon_jobs.last_error` gets the marked form, because the
    /// projection reads that column back and has to recover *which* limit fired
    /// — see [`limit_exceeded_reason`].
    fn cancel_for_budget(
        &mut self,
        job_id: &str,
        run_id: &str,
        now: u64,
        limit: BudgetLimit,
        detail: &str,
    ) -> Result<(), String> {
        let announced = format!("daemon {} budget exceeded: {detail}", limit.label());
        self.ensure_cancelling(run_id, &announced)?;
        if let Some(active) = self.active.get_mut(job_id) {
            active.process.signal(ProcessSignal::Terminate)?;
        }
        self.cancel_run(run_id, &announced)?;
        self.finish_active(
            job_id,
            JobState::Cancelled,
            now,
            Some(&limit_exceeded_reason(limit, detail)),
        )
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
        self.preempted.remove(job_id);
        // Released here as well as implied by the state change, because this is
        // the one place every ordinary exit passes through and an explicit
        // release is what makes the columns readable by anything but
        // `committed_reservations`. A model with other holders keeps its bytes:
        // the total is grouped by model key, so it only comes back when the last
        // of them has let go.
        self.store.release_reservation(job_id)?;
        self.store.transition(job_id, state, now, None, error)
    }

    fn reconcile_interrupted(
        &mut self,
        job: &DaemonJob,
        now: u64,
        reason: &str,
    ) -> Result<(), String> {
        // The crash funnel: a child that died, a lease that lapsed, a daemon that
        // restarted. None of those went through `finish_active`, so this is where
        // their claim comes back.
        self.preempted.remove(&job.job_id);
        self.store.release_reservation(&job.job_id)?;
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

    /// The escape this closes: a job's memory budget measured only the process the
    /// daemon spawned, which for an agent job is a shell. The work that consumes
    /// memory — a build, a model server — is its grandchild, so the budget was
    /// evadable by the normal case rather than by a trick.
    mod memory_is_measured_across_the_group {
        use super::*;

        // `ps -eo pgid=,rss=`: process group id, then resident set in KiB.
        const PS: &str = "  100  1000\n  200  2000\n  200  3000\n  200   500\n  300  9999\n";

        #[test]
        fn every_process_in_the_group_counts_and_others_do_not() {
            // 2000 + 3000 + 500. The old behaviour reported one row, so the
            // assertion that matters is that this exceeds any single member.
            assert_eq!(sum_group_rss_kib(PS, 200), Some(5500));
            assert!(sum_group_rss_kib(PS, 200).unwrap() > 3000);
            assert_eq!(sum_group_rss_kib(PS, 100), Some(1000));
        }

        #[test]
        fn a_group_with_no_processes_reads_as_gone_not_as_zero() {
            // `Some(0)` would be a budget trivially satisfied forever; `None` means
            // there is nothing to measure, which is what an exited job is.
            assert_eq!(sum_group_rss_kib(PS, 999), None);
        }

        #[test]
        fn malformed_and_short_rows_are_skipped_rather_than_poisoning_the_total() {
            // `ps` output can carry a header, a warning line, or a truncated final
            // line; none of those may make a live job look like it used nothing.
            let noisy = "PGID RSS\n  200  2000\nnot-a-row\n  200\n  200  abc\n  200  1000\n";
            assert_eq!(sum_group_rss_kib(noisy, 200), Some(3000));
        }
    }

    /// Windows has no process group, so the tree is walked by parent. Tested here
    /// rather than only on CI because this machine cannot build that target at all.
    mod windows_tree_walk {
        use super::*;

        // pid parent working_set_bytes
        const ROWS: &str = "10 1 100\n11 10 200\n12 11 400\n20 1 800\n";

        #[test]
        fn the_root_and_every_descendant_count_transitively() {
            // 100 + 200 + 400; the grandchild (12) is the case a single
            // `Get-Process -Id` missed.
            assert_eq!(sum_process_tree_working_set(ROWS, 10), Some(700));
            assert_eq!(sum_process_tree_working_set(ROWS, 11), Some(600));
            assert_eq!(sum_process_tree_working_set(ROWS, 20), Some(800));
        }

        #[test]
        fn an_absent_root_reads_as_gone() {
            assert_eq!(sum_process_tree_working_set(ROWS, 999), None);
        }

        #[test]
        fn a_parent_cycle_terminates_instead_of_hanging_the_watchdog() {
            // Pid reuse can legitimately produce a cycle in reported parents. The
            // fixed-point loop must stop; a recursive walk would not.
            let cyclic = "10 12 100\n11 10 200\n12 11 400\n";
            assert_eq!(sum_process_tree_working_set(cyclic, 10), Some(700));
        }

        #[test]
        fn a_self_parented_process_does_not_adopt_the_whole_machine() {
            // Windows reports pid 0 as its own parent; without the guard, every
            // process whose parent is itself would pull in unrelated trees.
            let self_parented = "4 4 100\n10 4 200\n99 1 400\n";
            assert_eq!(sum_process_tree_working_set(self_parented, 4), Some(300));
        }
    }
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
        /// What this process claims to be using. Shared with the adapter so a
        /// test can move it after the process is spawned.
        memory: Arc<Mutex<Option<u64>>>,
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
            Ok(*self.memory.lock().unwrap())
        }
    }

    #[derive(Clone)]
    struct FakeProcesses {
        spawns: Arc<Mutex<u32>>,
        exits: Arc<Mutex<VecDeque<Option<i32>>>>,
        signals: Arc<Mutex<Vec<ProcessSignal>>>,
        memory: Arc<Mutex<Option<u64>>>,
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
                memory: self.memory.clone(),
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
        fixture_with_memory_budget(label, None)
    }

    /// `fixture`, but with a declared memory ceiling — the one budget a test can
    /// trip on demand, since the fake process reports whatever it is told to.
    fn fixture_with_memory_budget(
        label: &str,
        max_memory_bytes: Option<u64>,
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
                    max_memory_bytes,
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
            memory: Arc::new(Mutex::new(Some(1024))),
        }
    }

    /// A 16 GiB machine with 16 GiB free, so admission is decided by the
    /// reservations rather than by whatever CI is running on.
    fn sixteen_gig_machine() -> Option<little_monkey_lib::runtime_adapter::HardwareSnapshot> {
        Some(little_monkey_lib::runtime_adapter::HardwareSnapshot {
            captured_at_ms: 1,
            total_ram_bytes: 16 * 1024 * 1024 * 1024,
            available_ram_bytes: 16 * 1024 * 1024 * 1024,
            logical_cpu_count: 8,
            platform: little_monkey_lib::runtime_adapter::PlatformCapabilities::from_host(
                "macos",
                "aarch64",
                Vec::new(),
            ),
        })
    }

    /// Several queued jobs, each with its own durable run whose frozen target is
    /// a *local* model of the given size — the case the reservation is read
    /// from. The shared `spec` fixture targets a provider, which correctly
    /// reserves nothing and so cannot exercise admission at all.
    ///
    /// Each tuple is `(job label, estimated bytes, model name)`. The model name
    /// is separate from the job label because reservations are keyed by resident
    /// model: two jobs sharing a name deliberately share one reservation, so a
    /// fixture that wants N independent claims has to say N different names.
    fn admission_fixture(
        label: &str,
        jobs: &[(&str, u64, &str)],
    ) -> (DaemonPaths, DaemonStore, SharedLedger) {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-daemon-admission-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let paths = DaemonPaths::under(&root);
        paths.ensure().unwrap();
        let mut store = DaemonStore::open(&paths).unwrap();

        for (job_label, estimated_memory_bytes, model) in jobs {
            let run_id = format!("run-{job_label}");
            let mut run_spec = spec(&run_id, 1_000);
            // Zero keeps the shared fixture's provider target: the protocol
            // rejects a zero estimate, and a zero-sized local model is not the
            // thing being modelled — a remote one is.
            if *estimated_memory_bytes > 0 {
                run_spec.target = ModelTargetSnapshot::ManagedLlama {
                    target_id: format!("local-target-{model}"),
                    label: "local".into(),
                    model_id: (*model).into(),
                    model_path: format!("/tmp/{model}.gguf"),
                    capabilities: crate::task::cli_capabilities(),
                    estimated_memory_bytes: Some(*estimated_memory_bytes),
                };
            }
            let ledger = RunLedger::open(&paths.ledger_db).unwrap();
            DurableRunRecorder::submit(ledger, &run_spec, "daemon-fixture".into()).unwrap();

            let job_id = format!("job-{job_label}");
            let snapshot = paths.snapshots.join(format!("{job_id}.json"));
            std::fs::write(&snapshot, b"{}").unwrap();
            store
                .insert_preparing(
                    &NewDaemonJob {
                        job_id: job_id.clone(),
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
            store.mark_queued(&job_id, &run_id, 1_000).unwrap();
        }

        let shared = SharedLedger::open(&paths.ledger_db).unwrap();
        (paths, store, shared)
    }

    fn admission_engine(
        paths: DaemonPaths,
        store: DaemonStore,
        shared: SharedLedger,
        adapter: FakeProcesses,
    ) -> DaemonEngine<FakeProcesses, FakeNotifier, FakeClock> {
        let mut engine = DaemonEngine::new(
            store,
            shared,
            paths,
            DaemonConfig::default(),
            adapter,
            FakeNotifier::default(),
            FakeClock(Arc::new(Mutex::new(2_000))),
            "daemon-test-owner".into(),
        );
        engine.set_hardware_probe(sixteen_gig_machine);
        engine
    }

    /// The roadmap's own example, end to end through the real tick: four 12 GB
    /// jobs on a 16 GB machine were all admitted and all thrashed, because
    /// `concurrency = 4` was the only question asked.
    #[test]
    fn four_twelve_gig_jobs_on_a_sixteen_gig_machine_admit_one_at_a_time() {
        const TWELVE_GIB: u64 = 12 * 1024 * 1024 * 1024;
        let (paths, store, shared) = admission_fixture(
            "thrash",
            &[
                ("a", TWELVE_GIB, "model-a"),
                ("b", TWELVE_GIB, "model-b"),
                ("c", TWELVE_GIB, "model-c"),
                ("d", TWELVE_GIB, "model-d"),
            ],
        );
        let adapter = fake_adapter();
        let spawns = adapter.spawns.clone();
        let mut engine = admission_engine(paths, store, shared, adapter);

        engine.tick().unwrap();

        assert_eq!(
            engine.active_count(),
            1,
            "concurrency alone would have admitted all four"
        );
        assert_eq!(*spawns.lock().unwrap(), 1, "only one job may be spawned");

        // The three held jobs are still queued — held, not failed.
        let queued = engine
            .store
            .ready_jobs(8)
            .unwrap()
            .into_iter()
            .map(|job| job.job_id)
            .collect::<Vec<_>>();
        assert_eq!(queued.len(), 3, "the rest wait rather than being rejected");
    }

    /// Without the probe the queue must behave exactly as it did before K7 —
    /// a machine we could not measure is not a reason to refuse work.
    #[test]
    fn an_unavailable_hardware_probe_falls_back_to_concurrency_alone() {
        const TWELVE_GIB: u64 = 12 * 1024 * 1024 * 1024;
        let (paths, store, shared) = admission_fixture(
            "noprobe",
            &[("a", TWELVE_GIB, "m-a"), ("b", TWELVE_GIB, "m-b")],
        );
        let adapter = fake_adapter();
        let mut engine = admission_engine(paths, store, shared, adapter);
        engine.set_hardware_probe(|| None);

        engine.tick().unwrap();

        assert_eq!(engine.active_count(), 2, "pre-K7 behaviour is preserved");
    }

    /// A job too big for the machine is failed at admission with the shortfall,
    /// rather than started and killed by the memory watchdog minutes later.
    #[test]
    fn a_job_larger_than_the_machine_is_rejected_without_being_spawned() {
        const SIXTY_FOUR_GIB: u64 = 64 * 1024 * 1024 * 1024;
        let (paths, store, shared) =
            admission_fixture("oversized", &[("a", SIXTY_FOUR_GIB, "big")]);
        let adapter = fake_adapter();
        let spawns = adapter.spawns.clone();
        let mut engine = admission_engine(paths, store, shared, adapter);

        engine.tick().unwrap();

        assert_eq!(*spawns.lock().unwrap(), 0, "must never be spawned");
        assert_eq!(engine.active_count(), 0);

        let job = engine.store.get_job("job-a").unwrap().unwrap();
        assert_eq!(job.state, JobState::Failed);
        let error = job.last_error.unwrap_or_default();
        assert!(
            error.contains("GiB"),
            "refusal must name the shortfall, got {error:?}"
        );
    }

    /// A cloud target holds no local weights, so a local memory bound must not
    /// serialize provider jobs behind each other.
    #[test]
    fn provider_jobs_are_not_held_by_a_local_memory_bound() {
        let (paths, store, shared) =
            admission_fixture("remote", &[("a", 0, "remote-a"), ("b", 0, "remote-b")]);
        let adapter = fake_adapter();
        let mut engine = admission_engine(paths, store, shared, adapter);

        engine.tick().unwrap();

        assert_eq!(engine.active_count(), 2);
    }

    /// The common case the per-job reservation got wrong: a queue of turns
    /// against one local model. The model is resident once, so it is charged
    /// once, and all four run.
    #[test]
    fn jobs_sharing_one_local_model_reserve_it_once() {
        const TWELVE_GIB: u64 = 12 * 1024 * 1024 * 1024;
        let (paths, store, shared) = admission_fixture(
            "dedup",
            &[
                ("a", TWELVE_GIB, "shared-model"),
                ("b", TWELVE_GIB, "shared-model"),
                ("c", TWELVE_GIB, "shared-model"),
                ("d", TWELVE_GIB, "shared-model"),
            ],
        );
        let adapter = fake_adapter();
        let mut engine = admission_engine(paths, store, shared, adapter);

        engine.tick().unwrap();

        assert_eq!(
            engine.active_count(),
            4,
            "one resident model must not be paid for four times"
        );
        let committed = engine.store.committed_reservations().unwrap();
        assert_eq!(
            committed.len(),
            1,
            "one row per resident model, got {committed:?}"
        );
        assert_eq!(committed[0].1, TWELVE_GIB);
    }

    /// The subtle half of keying reservations by model: the bytes come back when
    /// the *last* holder exits, not the first.
    #[test]
    fn a_shared_reservation_survives_until_the_last_holder_exits() {
        const TWELVE_GIB: u64 = 12 * 1024 * 1024 * 1024;
        let (paths, store, shared) = admission_fixture(
            "lastholder",
            &[("a", TWELVE_GIB, "shared"), ("b", TWELVE_GIB, "shared")],
        );
        let adapter = fake_adapter();
        let mut engine = admission_engine(paths, store, shared, adapter);
        engine.tick().unwrap();
        assert_eq!(engine.active_count(), 2);

        engine
            .finish_active("job-a", JobState::Succeeded, 2_001, None)
            .unwrap();
        let after_first = engine.store.committed_reservations().unwrap();
        assert_eq!(
            after_first.len(),
            1,
            "the model is still loaded for job-b, got {after_first:?}"
        );

        engine
            .finish_active("job-b", JobState::Succeeded, 2_002, None)
            .unwrap();
        assert!(
            engine.store.committed_reservations().unwrap().is_empty(),
            "the last holder releases it"
        );
    }

    /// The reservation lives in `daemon_jobs`, not in a `HashMap` that dies with
    /// the process, and a claim left behind by a daemon that never came back is
    /// swept rather than shrinking the budget forever.
    #[test]
    fn a_reservation_outlives_the_daemon_and_a_stale_one_is_swept() {
        const EIGHT_GIB: u64 = 8 * 1024 * 1024 * 1024;
        let (paths, store, shared) = admission_fixture("restart", &[("a", EIGHT_GIB, "durable")]);
        let adapter = fake_adapter();
        let mut engine = admission_engine(paths.clone(), store, shared, adapter);
        engine.tick().unwrap();
        assert_eq!(engine.active_count(), 1);
        // The daemon dies here: drop everything it held in memory.
        drop(engine);

        let reopened = DaemonStore::open(&paths).unwrap();
        let committed = reopened.committed_reservations().unwrap();
        assert_eq!(
            committed.len(),
            1,
            "a restarted daemon must still see the claim, got {committed:?}"
        );
        assert_eq!(committed[0].1, EIGHT_GIB);

        let shared = SharedLedger::open(&paths.ledger_db).unwrap();
        let mut engine = admission_engine(paths, reopened, shared, fake_adapter());
        engine.recover().unwrap();
        assert!(
            engine.store.committed_reservations().unwrap().is_empty(),
            "nothing is running, so nothing may still be reserved"
        );
    }

    /// A held job is not silently indistinguishable from one nothing has looked
    /// at: the reason is on the row, and it is cleared once the job starts.
    #[test]
    fn a_held_job_records_why_and_is_retried_on_a_later_tick() {
        const TWELVE_GIB: u64 = 12 * 1024 * 1024 * 1024;
        let (paths, store, shared) = admission_fixture(
            "heldreason",
            &[("a", TWELVE_GIB, "first"), ("b", TWELVE_GIB, "second")],
        );
        let adapter = fake_adapter();
        let mut engine = admission_engine(paths, store, shared, adapter);
        engine.tick().unwrap();

        let held = engine.store.get_job("job-b").unwrap().unwrap();
        assert_eq!(held.state, JobState::Queued, "held, not failed");
        let reason = held.hold_reason.unwrap_or_default();
        assert!(
            reason.contains("system memory") && reason.contains("GiB"),
            "the hold must name the resource and the shortfall, got {reason:?}"
        );

        // Freeing the first job's reservation lets the later tick admit it —
        // a hold is a retry, not a terminal decision.
        engine
            .finish_active("job-a", JobState::Succeeded, 2_001, None)
            .unwrap();
        engine.tick().unwrap();
        let admitted = engine.store.get_job("job-b").unwrap().unwrap();
        assert_eq!(admitted.state, JobState::Running);
        assert_eq!(
            admitted.hold_reason, None,
            "the hold reason is stale once the job starts"
        );
    }

    /// A held job is skipped, not stopped at: strictly smaller work queued behind
    /// it still starts in the same tick.
    #[test]
    fn a_hold_does_not_block_smaller_jobs_behind_it() {
        const GIB: u64 = 1024 * 1024 * 1024;
        // 16 GiB total puts the machine in the Balanced tier, whose reserve is
        // 3 GiB, so 13 GiB is schedulable.
        let (paths, store, shared) = admission_fixture(
            "headofline",
            &[
                ("a", 10 * GIB, "ten"),
                ("b", 12 * GIB, "twelve"),
                ("c", 2 * GIB, "two"),
            ],
        );
        let adapter = fake_adapter();
        let mut engine = admission_engine(paths, store, shared, adapter);

        engine.tick().unwrap();

        let state = |id: &str| engine.store.get_job(id).unwrap().unwrap().state;
        assert_eq!(state("job-a"), JobState::Running);
        assert_eq!(state("job-b"), JobState::Queued, "12 GiB no longer fits");
        assert_eq!(
            state("job-c"),
            JobState::Running,
            "a 2 GiB job must not wait behind a held 12 GiB one"
        );
    }

    /// An unmeasured model is admitted, but it is never counted as having fitted:
    /// its claim stays absent rather than becoming a zero that satisfies a bound.
    #[test]
    fn an_unmeasured_model_is_admitted_without_a_reservation() {
        // Zero means the fixture keeps its provider target, so an unmeasured
        // *local* model needs its own run spec.
        let (paths, store, shared) = admission_fixture("unmeasured", &[("a", 0, "unmeasured")]);
        let mut store = store;
        {
            let ledger = RunLedger::open(&paths.ledger_db).unwrap();
            let mut run_spec = spec("run-solo", 1_000);
            run_spec.target = ModelTargetSnapshot::ManagedLlama {
                target_id: "local-target-solo".into(),
                label: "local".into(),
                model_id: "never-installed".into(),
                model_path: "/tmp/solo.gguf".into(),
                capabilities: crate::task::cli_capabilities(),
                estimated_memory_bytes: None,
            };
            DurableRunRecorder::submit(ledger, &run_spec, "daemon-fixture".into()).unwrap();
            let snapshot = paths.snapshots.join("job-solo.json");
            std::fs::write(&snapshot, b"{}").unwrap();
            store
                .insert_preparing(
                    &NewDaemonJob {
                        job_id: "job-solo".into(),
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
            store.mark_queued("job-solo", "run-solo", 1_000).unwrap();
        }
        let mut engine = admission_engine(paths, store, shared, fake_adapter());

        engine.tick().unwrap();

        assert_eq!(
            engine.store.get_job("job-solo").unwrap().unwrap().state,
            JobState::Running,
            "an unmeasured model still runs"
        );
        assert!(
            engine
                .store
                .committed_reservations()
                .unwrap()
                .iter()
                .all(|(key, _, _)| key != "local-target-solo"),
            "an unknown footprint must not become a zero-byte reservation"
        );
    }

    // ---------------------------------------------------------------------
    // K8 — the scheduler.
    // ---------------------------------------------------------------------

    const GIB: u64 = 1024 * 1024 * 1024;

    /// One job for [`sched_job`], spelled out because the scheduler tests need to
    /// vary things `admission_fixture` fixes: the run kind (which is where the
    /// process class comes from), the workspace (which is the fair-share key), the
    /// priority and the queue age.
    struct SchedJob<'a> {
        label: &'a str,
        kind: RunKind,
        bytes: u64,
        model: &'a str,
        workspace: &'a str,
        priority: i32,
        created_at_ms: u64,
    }

    impl<'a> SchedJob<'a> {
        fn new(label: &'a str, kind: RunKind, bytes: u64, model: &'a str) -> Self {
            Self {
                label,
                kind,
                bytes,
                model,
                workspace: "/tmp/lm-sched-workspace",
                priority: 0,
                created_at_ms: 1_000,
            }
        }
    }

    fn sched_paths(label: &str) -> DaemonPaths {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-daemon-sched-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let paths = DaemonPaths::under(&root);
        paths.ensure().unwrap();
        paths
    }

    /// Submits the durable run and queues the daemon job for one [`SchedJob`].
    fn sched_job(paths: &DaemonPaths, store: &mut DaemonStore, job: &SchedJob<'_>) {
        let run_id = format!("run-{}", job.label);
        let mut run_spec = spec(&run_id, 1_000);
        run_spec.kind = job.kind.clone();
        if let Some(workspace) = run_spec.workspace.as_mut() {
            workspace.roots[0].canonical_path = job.workspace.to_string();
        }
        if job.bytes > 0 {
            run_spec.target = ModelTargetSnapshot::ManagedLlama {
                target_id: format!("local-target-{}", job.model),
                label: "local".into(),
                model_id: job.model.into(),
                model_path: format!("/tmp/{}.gguf", job.model),
                capabilities: crate::task::cli_capabilities(),
                estimated_memory_bytes: Some(job.bytes),
            };
        }
        let ledger = RunLedger::open(&paths.ledger_db).unwrap();
        DurableRunRecorder::submit(ledger, &run_spec, "daemon-fixture".into()).unwrap();

        let job_id = format!("job-{}", job.label);
        let snapshot = paths.snapshots.join(format!("{job_id}.json"));
        std::fs::write(&snapshot, b"{}").unwrap();
        store
            .insert_preparing(
                &NewDaemonJob {
                    job_id: job_id.clone(),
                    recipe_snapshot: snapshot,
                    priority: job.priority,
                    max_attempts: 1,
                    created_at_ms: job.created_at_ms,
                    max_runtime_ms: 60_000,
                    max_memory_bytes: None,
                    max_log_bytes: DEFAULT_MAX_LOG_BYTES,
                    repository_policy_json: None,
                    worktree_json: None,
                    parent_run_id: None,
                },
                64,
            )
            .unwrap();
        store
            .mark_queued(&job_id, &run_id, job.created_at_ms)
            .unwrap();
    }

    fn sched_engine(
        paths: &DaemonPaths,
        adapter: FakeProcesses,
        concurrency: u32,
        clock: FakeClock,
    ) -> DaemonEngine<FakeProcesses, FakeNotifier, FakeClock> {
        let store = DaemonStore::open(paths).unwrap();
        let shared = SharedLedger::open(&paths.ledger_db).unwrap();
        let mut engine = DaemonEngine::new(
            store,
            shared,
            paths.clone(),
            DaemonConfig {
                concurrency,
                ..DaemonConfig::default()
            },
            adapter,
            FakeNotifier::default(),
            clock,
            "daemon-test-owner".into(),
        );
        engine.set_hardware_probe(sixteen_gig_machine);
        engine
    }

    fn job_state(
        engine: &DaemonEngine<FakeProcesses, FakeNotifier, FakeClock>,
        job_id: &str,
    ) -> JobState {
        engine.store.get_job(job_id).unwrap().unwrap().state
    }

    /// The starvation bug the previous `// ponytail:` comment named, fixed: the
    /// candidate window used to be exactly as wide as the free slots, so four held
    /// 12 GiB jobs consumed the window and a 1 GiB job behind them was never even
    /// looked at — not on this tick and not on any later one, because the held jobs
    /// were still there.
    #[test]
    fn a_small_job_is_considered_past_a_window_full_of_held_ones() {
        let paths = sched_paths("window");
        let mut store = DaemonStore::open(&paths).unwrap();
        // 16 GiB total, Balanced tier reserve 3 GiB, so 13 GiB is schedulable.
        // job-a and job-e fit together; b, c and d do not fit at all alongside a.
        for label in ["a", "b", "c", "d"] {
            sched_job(
                &paths,
                &mut store,
                &SchedJob::new(label, RunKind::Background, 12 * GIB, &format!("m-{label}")),
            );
        }
        sched_job(
            &paths,
            &mut store,
            &SchedJob::new("e", RunKind::Background, GIB, "m-e"),
        );
        drop(store);
        let mut engine = sched_engine(
            &paths,
            fake_adapter(),
            4,
            FakeClock(Arc::new(Mutex::new(2_000))),
        );

        engine.tick().unwrap();

        assert_eq!(job_state(&engine, "job-a"), JobState::Running);
        for held in ["job-b", "job-c", "job-d"] {
            assert_eq!(job_state(&engine, held), JobState::Queued, "{held}");
        }
        assert_eq!(
            job_state(&engine, "job-e"),
            JobState::Running,
            "a 1 GiB job must not be invisible behind four held 12 GiB ones"
        );
    }

    /// Preemption: a lower class is **suspended**, not killed, its reservation
    /// comes back, and the interactive job that displaced it then starts.
    #[test]
    fn an_interactive_job_suspends_a_background_one_and_takes_its_reservation() {
        let paths = sched_paths("preempt");
        let mut store = DaemonStore::open(&paths).unwrap();
        sched_job(
            &paths,
            &mut store,
            &SchedJob::new("bg", RunKind::Background, 12 * GIB, "bg-model"),
        );
        drop(store);
        let adapter = fake_adapter();
        let signals = adapter.signals.clone();
        // Long-lived: the fake adapter's default exit queue is two `None`s, and a
        // preemption test ticks more than twice.
        *adapter.exits.lock().unwrap() = VecDeque::new();
        let clock = FakeClock(Arc::new(Mutex::new(2_000)));
        let mut engine = sched_engine(&paths, adapter, 4, clock.clone());

        engine.tick().unwrap();
        assert_eq!(job_state(&engine, "job-bg"), JobState::Running);
        assert_eq!(engine.store.committed_reservations().unwrap().len(), 1);

        // A desktop turn arrives. 12 + 12 does not fit in 13 GiB, and the only
        // thing holding memory is a lower class.
        sched_job(
            &paths,
            &mut engine.store,
            &SchedJob {
                created_at_ms: 2_001,
                ..SchedJob::new("ui", RunKind::Interactive, 12 * GIB, "ui-model")
            },
        );
        *clock.0.lock().unwrap() = 2_002;
        engine.tick().unwrap();

        // The suspend latch is set, which is what survives a restart and what
        // `monkey processes` shows. The daemon's own bit follows on the next tick.
        assert!(
            engine
                .store
                .recent_decisions(8)
                .unwrap()
                .iter()
                .any(|entry| entry.job_id == "job-bg"
                    && entry.outcome == DECISION_PREEMPTED
                    && entry.measurement == MEASUREMENT_AVAILABLE_RAM
                    && entry.measured_value == Some(16 * GIB)
                    && entry.measured_at_ms == Some(1)),
            "the preemption must be recorded with the measurement that decided it"
        );

        *clock.0.lock().unwrap() = 2_003;
        engine.tick().unwrap();
        assert_eq!(
            job_state(&engine, "job-bg"),
            JobState::Paused,
            "preemption suspends"
        );
        let delivered = signals.lock().unwrap().clone();
        assert!(delivered.contains(&ProcessSignal::Pause));
        assert!(
            !delivered.contains(&ProcessSignal::Terminate)
                && !delivered.contains(&ProcessSignal::Kill),
            "preemption must never kill, got {delivered:?}"
        );
        assert!(
            engine
                .store
                .committed_reservations()
                .unwrap()
                .iter()
                .all(|(key, _, _)| key != "local-target-bg-model"),
            "a suspended job gives its claim back"
        );

        // With the memory released the interactive turn starts.
        *clock.0.lock().unwrap() = 2_004;
        engine.tick().unwrap();
        assert_eq!(job_state(&engine, "job-ui"), JobState::Running);
        assert_eq!(
            job_state(&engine, "job-bg"),
            JobState::Paused,
            "the dwell floor keeps it parked while the turn it made room for runs"
        );

        // The turn finishes and nothing queued outranks the parked job any more,
        // so its suspension is released and it reacquires what it gave up. Past
        // the dwell floor, or it would be resumed and re-suspended four times a
        // second.
        engine
            .finish_active("job-ui", JobState::Succeeded, 2_005, None)
            .unwrap();
        for step in 5..12 {
            *clock.0.lock().unwrap() = 2_000 + step * 1_000;
            engine.tick().unwrap();
        }
        assert_eq!(
            job_state(&engine, "job-bg"),
            JobState::Running,
            "a released preemption resumes"
        );
        assert!(
            engine
                .store
                .committed_reservations()
                .unwrap()
                .iter()
                .any(|(key, ram, _)| key == "local-target-bg-model" && *ram == 12 * GIB),
            "and reacquires the claim it released"
        );
        assert!(
            engine
                .store
                .recent_decisions(32)
                .unwrap()
                .iter()
                .any(|entry| entry.job_id == "job-bg" && entry.outcome == DECISION_RESUMED),
            "the release is inspectable too"
        );
    }

    /// The interesting half of the round-trip: a resume can fail. A job whose
    /// memory was taken while it was suspended stays suspended with a reason, and
    /// nothing is delivered to the child — so re-evaluating every tick costs one
    /// fit computation and thrashes nothing.
    #[test]
    fn a_suspended_job_that_no_longer_fits_stays_held_instead_of_thrashing() {
        use little_monkey_lib::process_table::{ProcessKind, ProcessSignal as TableSignal};

        let paths = sched_paths("reacquire");
        let mut store = DaemonStore::open(&paths).unwrap();
        sched_job(
            &paths,
            &mut store,
            &SchedJob::new("parked", RunKind::Background, 12 * GIB, "parked-model"),
        );
        drop(store);
        let adapter = fake_adapter();
        let signals = adapter.signals.clone();
        *adapter.exits.lock().unwrap() = VecDeque::new();
        let clock = FakeClock(Arc::new(Mutex::new(2_000)));
        let mut engine = sched_engine(&paths, adapter, 4, clock.clone());
        engine.tick().unwrap();
        assert_eq!(job_state(&engine, "job-parked"), JobState::Running);

        let process_id = {
            let table = engine.shared.process_table();
            table
                .find_by_external_id(
                    ProcessKind::DaemonJob,
                    &process_external_id("job-parked", 0),
                )
                .unwrap()
                .unwrap()
                .process_id
        };
        {
            let table = engine.shared.process_table();
            table
                .signal(&process_id, TableSignal::Suspend, None, 2_001)
                .unwrap();
        }
        *clock.0.lock().unwrap() = 2_001;
        engine.tick().unwrap();
        assert_eq!(job_state(&engine, "job-parked"), JobState::Paused);
        assert!(engine.store.committed_reservations().unwrap().is_empty());

        // Somebody else takes the memory while it is parked.
        sched_job(
            &paths,
            &mut engine.store,
            &SchedJob::new("hog", RunKind::Background, 12 * GIB, "hog-model"),
        );
        *clock.0.lock().unwrap() = 2_002;
        engine.tick().unwrap();
        assert_eq!(job_state(&engine, "job-hog"), JobState::Running);

        // Now try to resume. There is no room, so it must not resume.
        {
            let table = engine.shared.process_table();
            table
                .signal(&process_id, TableSignal::Resume, None, 2_003)
                .unwrap();
        }
        let resumes_before = signals
            .lock()
            .unwrap()
            .iter()
            .filter(|signal| **signal == ProcessSignal::Resume)
            .count();
        for extra in 3..8 {
            *clock.0.lock().unwrap() = 2_000 + extra;
            engine.tick().unwrap();
        }
        assert_eq!(
            job_state(&engine, "job-parked"),
            JobState::Paused,
            "a resume with no memory to reacquire must not resume"
        );
        let reason = engine
            .store
            .get_job("job-parked")
            .unwrap()
            .unwrap()
            .hold_reason
            .unwrap_or_default();
        assert!(
            reason.contains("cannot resume") && reason.contains("system memory"),
            "the failed reacquire must say what it is waiting for, got {reason:?}"
        );
        assert_eq!(
            signals
                .lock()
                .unwrap()
                .iter()
                .filter(|signal| **signal == ProcessSignal::Resume)
                .count(),
            resumes_before,
            "five ticks of a failed reacquire must deliver nothing to the child"
        );

        // Free the memory and the same job resumes, reclaiming its reservation.
        engine
            .finish_active("job-hog", JobState::Succeeded, 2_009, None)
            .unwrap();
        *clock.0.lock().unwrap() = 2_010;
        engine.tick().unwrap();
        assert_eq!(job_state(&engine, "job-parked"), JobState::Running);
        assert!(
            engine
                .store
                .committed_reservations()
                .unwrap()
                .iter()
                .any(|(key, ram, _)| key == "local-target-parked-model" && *ram == 12 * GIB),
            "the reservation has to come back with the job"
        );
    }

    /// Fair-share, measured rather than rotated: the workspace that has already
    /// had the device loses to one that has not, even though its job is older and
    /// has the higher declared priority. The number that decides it is K6's
    /// `cpu_time_ms`, which is why the decision log cites that field.
    #[test]
    fn a_workspace_that_used_the_device_is_outranked_by_one_that_has_not() {
        use little_monkey_lib::process_table::ProcessKind;
        use little_monkey_lib::process_usage::ProcessUsageSample;

        const BUSY: &str = "/tmp/lm-sched-busy";
        const QUIET: &str = "/tmp/lm-sched-quiet";

        let paths = sched_paths("fairshare");
        let mut store = DaemonStore::open(&paths).unwrap();
        sched_job(
            &paths,
            &mut store,
            &SchedJob {
                workspace: BUSY,
                ..SchedJob::new("first", RunKind::Background, 12 * GIB, "shared-model")
            },
        );
        drop(store);
        let clock = FakeClock(Arc::new(Mutex::new(2_000)));
        // One slot, so each tick admits at most one job and the ranking is what
        // decides which.
        let mut engine = sched_engine(&paths, fake_adapter(), 1, clock.clone());
        engine.tick().unwrap();
        assert_eq!(job_state(&engine, "job-first"), JobState::Running);

        // Ten minutes of measured CPU, charged to the busy workspace. Written
        // through the same call the daemon's own K6 sampler uses.
        {
            let table = engine.shared.process_table();
            let record = table
                .find_by_external_id(ProcessKind::DaemonJob, &process_external_id("job-first", 0))
                .unwrap()
                .unwrap();
            assert_eq!(
                record.workspace.as_deref(),
                Some(BUSY),
                "without the workspace on the row there is nothing to charge"
            );
            table
                .accumulate_usage(
                    &record.process_id,
                    &ProcessUsageSample {
                        cpu_time_ms: Some(10 * 60 * 1_000),
                        ..ProcessUsageSample::default()
                    },
                    2_000,
                )
                .unwrap();
        }
        engine
            .finish_active("job-first", JobState::Succeeded, 2_001, None)
            .unwrap();

        // The busy workspace's next job is older and higher priority; the quiet
        // workspace's is neither.
        sched_job(
            &paths,
            &mut engine.store,
            &SchedJob {
                workspace: BUSY,
                priority: 5,
                created_at_ms: 1_000,
                ..SchedJob::new("busy-next", RunKind::Background, 12 * GIB, "busy-model")
            },
        );
        sched_job(
            &paths,
            &mut engine.store,
            &SchedJob {
                workspace: QUIET,
                priority: 0,
                created_at_ms: 1_500,
                ..SchedJob::new("quiet-next", RunKind::Background, 12 * GIB, "quiet-model")
            },
        );

        *clock.0.lock().unwrap() = 2_002;
        engine.tick().unwrap();

        assert_eq!(
            job_state(&engine, "job-quiet-next"),
            JobState::Running,
            "the workspace that already had ten CPU-minutes must not go first"
        );
        assert_eq!(job_state(&engine, "job-busy-next"), JobState::Queued);

        let decision = engine
            .store
            .recent_decisions(16)
            .unwrap()
            .into_iter()
            .find(|entry| entry.job_id == "job-quiet-next" && entry.outcome == DECISION_ADMITTED)
            .expect("an admission is recorded");
        assert_eq!(
            decision.measurement,
            scheduler::KEY_FAIR_SHARE,
            "fair-share decided it, so the log must name the measured field"
        );
        assert_eq!(decision.measured_value, Some(0), "the quiet charge");
        assert_eq!(decision.workspace.as_deref(), Some(QUIET));
        assert_eq!(
            decision.passed_over,
            ["job-busy-next"],
            "the log has to say what it was chosen over"
        );
        assert_eq!(decision.process_class, "background");
    }

    /// A held job's decision cites the reading that produced the shortfall, with
    /// that reading's own timestamp — not the time the row was written.
    #[test]
    fn a_hold_decision_names_the_measurement_and_its_own_timestamp() {
        let paths = sched_paths("decision");
        let mut store = DaemonStore::open(&paths).unwrap();
        for label in ["a", "b"] {
            sched_job(
                &paths,
                &mut store,
                &SchedJob::new(label, RunKind::Background, 12 * GIB, &format!("m-{label}")),
            );
        }
        drop(store);
        let mut engine = sched_engine(
            &paths,
            fake_adapter(),
            4,
            FakeClock(Arc::new(Mutex::new(2_000))),
        );
        engine.tick().unwrap();

        let held = engine
            .store
            .recent_decisions(16)
            .unwrap()
            .into_iter()
            .find(|entry| entry.job_id == "job-b" && entry.outcome == DECISION_HELD)
            .expect("the held job's decision is recorded");
        assert_eq!(held.measurement, MEASUREMENT_AVAILABLE_RAM);
        assert_eq!(held.measured_value, Some(16 * GIB));
        assert_eq!(
            held.measured_at_ms,
            Some(1),
            "the snapshot's own capture time, not the write time (2_000)"
        );
        assert!(
            held.detail.contains("ranked first in at most"),
            "a hold states its starvation bound, got {:?}",
            held.detail
        );

        // Re-evaluated four times a second; recorded once, or the bounded log is
        // churned away within minutes.
        let before = engine.store.recent_decisions(512).unwrap().len();
        for _ in 0..4 {
            engine.tick().unwrap();
        }
        assert_eq!(
            engine.store.recent_decisions(512).unwrap().len(),
            before,
            "an unchanged hold must not append a row per tick"
        );
    }

    /// The backpressure signal, through the engine that owns the counts.
    #[test]
    fn backpressure_closes_when_the_queue_is_full_and_refuses_in_words() {
        let paths = sched_paths("backpressure");
        let mut store = DaemonStore::open(&paths).unwrap();
        for label in ["a", "b"] {
            sched_job(
                &paths,
                &mut store,
                &SchedJob::new(label, RunKind::Background, 12 * GIB, &format!("m-{label}")),
            );
        }
        drop(store);
        let mut engine = sched_engine(
            &paths,
            fake_adapter(),
            4,
            FakeClock(Arc::new(Mutex::new(2_000))),
        );
        engine.config.max_queue = 2;

        let signal = super::super::backpressure_for(&engine.store, &engine.config).unwrap();
        assert_eq!(signal.state, scheduler::BackpressureState::Closed);
        assert!(!signal.accepting);
        assert_eq!(signal.reason, Some(scheduler::BACKPRESSURE_QUEUE_FULL));
        assert!(signal.refusal().is_some_and(|text| text.contains("retry after")));

        // One tick holds job-b for memory, which is a different sentence: the
        // queue has room, the machine does not.
        engine.tick().unwrap();
        engine.config.max_queue = 8;
        let signal = super::super::backpressure_for(&engine.store, &engine.config).unwrap();
        assert_eq!(signal.state, scheduler::BackpressureState::Slow);
        assert_eq!(
            signal.reason,
            Some(scheduler::BACKPRESSURE_MEMORY_SATURATED)
        );
        assert_eq!(signal.held, 1);
        assert!(signal.refusal().is_none(), "slow still accepts work");
    }

    /// K6, wired: an active job's own measurement reaches the resource ledger
    /// while it is alive, and the row it lands on is the one representing the job.
    #[test]
    fn an_active_job_writes_its_own_measurement_into_the_resource_ledger() {
        use little_monkey_lib::process_table::{ProcessKind, ProcessUsageFilter};

        let (paths, store, shared, _recorder, _run_id) = fixture("usage");
        let adapter = fake_adapter();
        *adapter.exits.lock().unwrap() = VecDeque::new();
        let mut engine = DaemonEngine::new(
            store,
            shared,
            paths,
            DaemonConfig::default(),
            adapter,
            FakeNotifier::default(),
            FakeClock(Arc::new(Mutex::new(2_000))),
            "daemon-test-owner".into(),
        );
        engine.tick().unwrap();
        engine.tick().unwrap();

        let table = engine.shared.process_table();
        let record = table
            .find_by_external_id(ProcessKind::DaemonJob, &first_attempt_id("usage"))
            .unwrap()
            .expect("the job has a process row");
        let rows = table
            .usage_rows(&ProcessUsageFilter {
                process_id: Some(record.process_id.clone()),
                ..ProcessUsageFilter::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1, "the ledger row exists and is reachable");
        // The fake pid is not a real process, so the sampler's fields come back
        // unavailable *with a reason* — which is the contract that matters here. A
        // missing measurement must never read as a measured zero, and this is the
        // assertion that would fail if `record_usage` ever started writing one.
        let measured = rows[0].usage.measured();
        for (field, value) in [
            ("cpuTimeMs", measured.cpu_time_ms),
            ("peakRssBytes", measured.peak_rss_bytes),
            ("bytesEgressed", measured.bytes_egressed),
        ] {
            assert!(
                value.is_some() || rows[0].usage.reason_for(field).is_some(),
                "{field} must be either measured or explained"
            );
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
            hold_reason: None,
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
            let label = if signal == TableSignal::Kill {
                "kill"
            } else {
                "stop"
            };
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

        assert_eq!(
            exit_for(JobState::Succeeded, None).status,
            ExitStatus::Succeeded
        );
        assert_eq!(
            exit_for(JobState::Failed, Some("boom")).status,
            ExitStatus::Failed
        );
        assert_eq!(
            exit_for(JobState::Failed, Some("boom")).reason.as_deref(),
            Some("boom")
        );
        assert_eq!(
            exit_for(JobState::Cancelled, None).status,
            ExitStatus::Cancelled
        );
        assert_eq!(
            exit_for(JobState::NeedsReconciliation, None).status,
            ExitStatus::NeedsReconciliation
        );
        // A non-terminal state reaching the exit mapper means the job vanished.
        assert_eq!(exit_for(JobState::Running, None).status, ExitStatus::Lost);
    }

    /// The marker is the whole mechanism: it is what a budget kill leaves behind
    /// in a column that survives the daemon, and what tells the projection that a
    /// `Cancelled` job was not a person changing their mind.
    #[test]
    fn a_budget_kill_round_trips_through_last_error_and_a_plain_cancel_does_not() {
        use little_monkey_lib::process_table::{ExitStatus, ProcessLimits};

        // Compile-time proof that `field()` names fields that exist: rename one
        // in `ProcessLimits` and this destructuring stops building.
        let ProcessLimits {
            max_wall_ms,
            max_memory_bytes,
            max_output_bytes,
            max_child_processes: _,
        } = ProcessLimits::default();
        assert_eq!(max_wall_ms, None);
        assert_eq!(max_memory_bytes, None);
        assert_eq!(max_output_bytes, None);
        assert_eq!(BudgetLimit::Wall.field(), stringify!(max_wall_ms));
        assert_eq!(BudgetLimit::Memory.field(), stringify!(max_memory_bytes));
        assert_eq!(BudgetLimit::Output.field(), stringify!(max_output_bytes));

        for limit in [BudgetLimit::Wall, BudgetLimit::Memory, BudgetLimit::Output] {
            let stored = limit_exceeded_reason(limit, "held 9 bytes against 4");
            let exit = exit_for(JobState::Cancelled, Some(&stored));
            assert_eq!(
                exit.status,
                ExitStatus::LimitExceeded,
                "{limit:?} must not be projected as an ordinary cancel"
            );
            let reason = exit.reason.expect("a limit kill must name its limit");
            assert!(
                reason.starts_with(limit.field()),
                "the reason must name the limit that fired, got {reason:?}"
            );
            assert!(
                reason.ends_with("held 9 bytes against 4"),
                "the measurement must survive, got {reason:?}"
            );
            assert!(
                !reason.contains(LIMIT_EXCEEDED_PREFIX),
                "the storage marker must not leak into a human-facing reason"
            );
        }

        // The other half: an ordinary stop is still an ordinary stop. Without
        // this, "everything is a limit kill" would pass the assertions above.
        let stopped = exit_for(JobState::Cancelled, Some("stopped by the user"));
        assert_eq!(stopped.status, ExitStatus::Cancelled);
        assert_eq!(stopped.reason.as_deref(), Some("stopped by the user"));
    }

    /// End to end, through the two databases: a job that blows its memory budget
    /// is killed, and the process row says the system worked rather than that
    /// someone pressed Stop.
    #[test]
    fn a_memory_budget_kill_is_projected_as_limit_exceeded_not_as_a_cancel() {
        use little_monkey_lib::process_table::{ExitStatus, ProcessKind, ProcessState};

        let (paths, store, shared, _recorder, run_id) =
            fixture_with_memory_budget("membudget", Some(4_096));
        let adapter = fake_adapter();
        let clock = FakeClock(Arc::new(Mutex::new(2_000)));
        let mut engine = DaemonEngine::new(
            store,
            shared,
            paths,
            DaemonConfig::default(),
            adapter.clone(),
            FakeNotifier::default(),
            clock.clone(),
            "daemon-test-owner".into(),
        );

        // First tick spawns and projects while the job is inside its budget.
        engine.tick().unwrap();
        let process_id = {
            let table = engine.shared.process_table();
            table
                .find_by_external_id(ProcessKind::DaemonJob, &first_attempt_id("membudget"))
                .unwrap()
                .expect("the job is projected")
                .process_id
        };
        assert_eq!(
            engine
                .shared
                .process_table()
                .get(&process_id)
                .unwrap()
                .unwrap()
                .state,
            ProcessState::Running,
            "a job inside its budget must not be killed"
        );

        // Now the group grows past the ceiling.
        *adapter.memory.lock().unwrap() = Some(8_192);
        *clock.0.lock().unwrap() = 2_001;
        engine.tick().unwrap();

        assert!(
            adapter
                .signals
                .lock()
                .unwrap()
                .contains(&ProcessSignal::Terminate),
            "blowing the budget must actually tear the child down"
        );

        // The daemon's own row carries the marked reason...
        let job = engine
            .store
            .get_job(&format!("job-{}", "membudget"))
            .unwrap()
            .expect("the job row survives its kill");
        assert_eq!(job.state, JobState::Cancelled);
        let last_error = job.last_error.expect("a budget kill records why");
        assert_eq!(
            parse_limit_exceeded(&last_error),
            Some("max_memory_bytes: the process group held 8192 bytes against a 4096 byte budget"),
            "got {last_error:?}"
        );

        // ...and the unified table shows the distinguishable exit.
        let finished = {
            let table = engine.shared.process_table();
            table.get(&process_id).unwrap().unwrap()
        };
        assert_eq!(finished.state, ProcessState::Exited);
        let exit = finished.exit.expect("an exited row carries its exit");
        assert_eq!(
            exit.status,
            ExitStatus::LimitExceeded,
            "a budget kill recorded as `Cancelled` is indistinguishable from a user pressing Stop"
        );
        let reason = exit.reason.expect("a limit kill must name its limit");
        assert!(
            reason.starts_with("max_memory_bytes:")
                && reason.contains("8192")
                && reason.contains("4096"),
            "the exit must name the limit and both measurements, got {reason:?}"
        );

        // The run ledger has no `limit_exceeded` status, so the run is cancelled
        // there. That is honest rather than a silent protocol change, and the
        // prose event is what the person who launched the job reads.
        assert_eq!(
            engine.shared.load_run(&run_id).unwrap().unwrap().status,
            RunStatus::Cancelled
        );
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
