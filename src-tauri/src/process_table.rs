//! The unified agent process table.
//!
//! Before this module, at least five things in this app behaved like processes
//! and none of them shared a representation: a desktop chat turn, a daemon job,
//! a `task`-tool subagent, a workflow run, and a remote-runner run. They had
//! five identifier schemes with no cross-surface uniqueness, four incompatible
//! state vocabularies (`RunStatus`'s 9, `JobState`'s 10, `WorkflowRunStatus`'s
//! 5, `SubagentStatus`'s 4), transitions enforced in one place and nowhere
//! else, no parent pointer that meant hierarchy rather than retry lineage, and
//! no single answer to "what is running right now". Every listing surface —
//! the running-tasks pill, the Background Tasks panel, the Run Center, the
//! Agent Inbox — aggregated a different subset and missed the rest.
//!
//! This table is the shared representation. It deliberately does *not* replace
//! any of those records: a daemon job still owns its queue position, budgets
//! and pid; a ledger run still owns its event stream. What lives here is only
//! what every kind has in common and what a scheduler will need to arbitrate
//! between them — identity, lineage, state, ownership, limits, and how it
//! ended.
//!
//! Two invariants are enforced in both Rust and SQL, deliberately duplicated
//! because a companion store reaching the shared connection directly must not
//! be able to bypass them (see [`MIGRATION_V5_SQL`] in `run_ledger.rs`):
//!
//! 1. **Legal transitions only.** `admitted → running | exited`,
//!    `running → suspended | exited`, `suspended → running | exited`, and
//!    `exited` is terminal. Anything else is refused rather than silently
//!    applied — the gap that let `DaemonStore::transition` move a job from any
//!    state to any other with an unguarded `UPDATE`.
//! 2. **Terminal consistency.** A row is `exited` if and only if it carries an
//!    exit status, mirroring how `runs` binds `terminal_sequence` to a terminal
//!    status.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

use crate::process_usage::{
    ProcessUsageSample, FIELD_BYTES_EGRESSED, FIELD_BYTES_READ, FIELD_BYTES_WRITTEN,
    FIELD_CPU_TIME_MS, FIELD_GPU_DEVICE_MS, FIELD_GPU_RESIDENT_BYTES, FIELD_PEAK_RSS_BYTES,
    FIELD_TOKENS_IN, FIELD_TOKENS_OUT, FIELD_WALL_TIME_MS,
};
use crate::runtime_telemetry::TraceFieldNote;

/// Which execution surface a process belongs to.
///
/// The `as_str` values are the stored SQL enum and must match
/// `MIGRATION_V5_SQL`'s `CHECK` constraint. The short `tag` is the id prefix
/// (see [`new_process_id`]) so a bare id says what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessKind {
    /// An interactive desktop chat turn (`agentLoop.ts`'s `runAgentTurn`).
    ChatTurn,
    /// A queued `monkey daemon` job.
    DaemonJob,
    /// A `task`-tool subagent, child of the turn that spawned it.
    Subagent,
    /// One member of a Crew run, child of the coordinator's turn.
    CrewMember,
    /// A workflow run (`m4` executor).
    WorkflowRun,
    /// A single node instance inside a workflow run.
    WorkflowNode,
    /// Work queued by a paired remote controller or mobile device.
    RemoteRun,
    /// A backgrounded `run_shell` command.
    BackgroundShell,
    /// A foreground `run_shell`/verify command — the agent shell a turn blocks
    /// on.
    ///
    /// The most common native process this app creates, and until now the only
    /// one with no row at all. A turn's shell is where the compiler, the test
    /// runner and the model server actually live, so "what is this machine
    /// doing on behalf of the agent" was answerable for a backgrounded shell and
    /// not for the foreground one beside it. It also meant the process holding
    /// the memory had no record to carry a limit, which is why K4's memory and
    /// child-process legs had nowhere to land.
    ///
    /// Short-lived by construction — bounded by the caller's timeout — so its
    /// rows are numerous and brief. That is a property of the work, not a reason
    /// to keep it invisible: the same is true of a subagent.
    ForegroundShell,
    /// A side task running beside the main conversation.
    SideTask,
    /// An isolated Chromium the browser tool owns (`browser_worker.rs`).
    ///
    /// The one desktop kind that owns a real OS process *tree*: Chromium forks
    /// renderer, GPU and utility children, and before this kind existed nothing
    /// outside the owning process could name any of them. A crash left an orphan
    /// that the startup sweep could collect the profile directory of but never
    /// kill.
    BrowserSession,
    /// One stored verification command (`verify.rs`) — a build or a test run,
    /// started after an edit.
    ///
    /// The three kinds below are the same shape as [`Self::ForegroundShell`] and
    /// were bounded by the same controller before they had a row: the limit was
    /// installed, the tree was reclaimed, and none of it was visible. A limit
    /// that fires on a verify command was reported in that command's own result
    /// and nowhere a reader could find it afterwards, so "what did the agent run
    /// on this machine, and what held it" had three blind spots.
    VerifyCommand,
    /// One user-authored lifecycle hook (`hooks.rs`).
    HookCommand,
    /// One disposable-copy Sandbox panel run (`sandbox.rs`).
    SandboxRun,
}

/// The wall-clock budget the four WebView kinds carry by default.
///
/// # Why this number, and why one number for all four
///
/// Six hours. A `chat_turn`, a `subagent`, a `crew_member` and a `side_task` are
/// the same shape of process — a WebView agent loop issuing an unbounded number
/// of bounded tool calls — so giving them four different numbers would be
/// inventing policy where there is only one question: how long may a loop keep
/// *starting new work* before something concludes it is not going to stop?
///
/// Six hours answers that and nothing else. The longest legitimate run this app
/// produces is a long agentic session, which is minutes to an hour of wall time
/// even with hundreds of tool calls; a runaway loop has no bound at all. Anything
/// between those is unaffected, which is the property a default has to have.
///
/// # The precondition this had to clear
///
/// `processWallBudget.ts` shipped inert on a stated reason, not on timidity:
/// [`ProcessState`] has no state for "parked waiting on a human". A turn blocked
/// on an unanswered permission dialog reads as `Running` and its `started_at_ms`
/// keeps ageing, so a *tight* default would kill a turn for the user's own
/// slowness — "the app cancelled my work while I was reading the prompt", which
/// is worse than an unbounded turn.
///
/// That argument is an argument against a tight default, and it is why this one is
/// not tight. Six hours of an unanswered dialog is not a user reading a prompt; it
/// is a session nobody came back to, and ending it is the correct outcome rather
/// than the regrettable one. Suspended time counting against the budget (see
/// `processWallBudget.ts`) is bounded by the same reasoning.
///
/// # It is a floor, not a ceiling
///
/// The latch is observed at a safe point, so the real bound is this plus the
/// longest tool timeout in flight — 120 s for a shell, 300 s for a verify. This
/// bounds how long a runaway keeps starting new work. It is not a hard kill and
/// must not be documented as one: a hard kill needs an OS process to signal,
/// which is exactly what these kinds do not have.
pub const WEBVIEW_WALL_BUDGET_MS: u64 = 6 * 60 * 60 * 1_000;

/// The resident-memory ceiling an agent shell's whole process tree carries by
/// default.
///
/// # Why 8 GiB, and why one number for both shell kinds
///
/// A class default is not a tuning knob; it is the answer to "what is so far
/// outside normal that it is certainly a runaway". The legitimate heavy cases
/// here are a Rust or LLVM build and a local model server, and both fit inside
/// 8 GiB on the machines this app targets — a 16 GiB laptop is the floor, and a
/// tree past half of it is not a build, it is a leak or a fork bomb's memory
/// twin.
///
/// A *tight* number is the failure mode to avoid. A limit that kills working
/// commands gets turned off, and a limit that is off bounds nothing; a limit that
/// only ever fires on genuine runaways stays on. Callers who know their command's
/// real appetite tighten it per call, which the resolution in
/// [`crate::resource_control::EffectiveLimits`] always allows.
///
/// It bounds the **tree**, not the shell process: the shell itself holds a few
/// hundred kilobytes and the compiler underneath it holds everything.
pub const SHELL_MEMORY_BUDGET_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Live processes an agent shell's tree may hold at once.
///
/// Sized against the widest legitimate case rather than the common one: a
/// parallel build on a high-core machine is the shape that spawns most, and
/// `make -j64` with a compiler and a linker per job is comfortably inside 512.
/// A fork bomb is not near 512, it is unbounded, so the two are not close enough
/// for the exact number to be load-bearing.
///
/// This is the same figure the Windows shell job has always used as a fixed
/// guardrail. It becomes a *class default* here — a number a caller can tighten
/// and the row records — while the fixed job ceiling stays as an independent
/// defence in depth that a caller cannot widen.
pub const SHELL_PROCESS_BUDGET: u32 = 512;

/// The resident-memory ceiling one browser session's Chromium tree carries.
///
/// Lower than a shell's, and for a different reason rather than a stricter mood:
/// a shell legitimately hosts a compiler and a model server, while a browser
/// session hosts one page. 4 GiB is several times what a heavy real page costs
/// across its dozen renderer, GPU and utility processes, and well under what a
/// runaway page — a leaking script, an infinite canvas — reaches within seconds.
///
/// Held by [`crate::resource_control::ResourceController`], not by
/// `browser_worker`: the session's own quotas bound *browser* things (actions,
/// session clock, profile disk) and none of them is an answer to a renderer
/// taking the machine's memory.
pub const BROWSER_MEMORY_BUDGET_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Live processes one browser session's tree may hold at once.
///
/// Chromium's own process model is the sizing input: one browser process, one
/// GPU, one network service, one storage service, a utility or two, and a
/// renderer per site instance. A page with many cross-origin frames is the widest
/// legitimate case and stays well inside this; a tab spawning without bound is
/// nowhere near it.
pub const BROWSER_PROCESS_BUDGET: u32 = 128;

impl ProcessKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProcessKind::ChatTurn => "chat_turn",
            ProcessKind::DaemonJob => "daemon_job",
            ProcessKind::Subagent => "subagent",
            ProcessKind::CrewMember => "crew_member",
            ProcessKind::WorkflowRun => "workflow_run",
            ProcessKind::WorkflowNode => "workflow_node",
            ProcessKind::RemoteRun => "remote_run",
            ProcessKind::BackgroundShell => "background_shell",
            ProcessKind::ForegroundShell => "foreground_shell",
            ProcessKind::SideTask => "side_task",
            ProcessKind::BrowserSession => "browser_session",
            ProcessKind::VerifyCommand => "verify_command",
            ProcessKind::HookCommand => "hook_command",
            ProcessKind::SandboxRun => "sandbox_run",
        }
    }

    /// Short, stable id prefix. Kept distinct from [`Self::as_str`] so the
    /// stored enum can be renamed for readability without invalidating ids
    /// already minted.
    pub fn tag(self) -> &'static str {
        match self {
            ProcessKind::ChatTurn => "turn",
            ProcessKind::DaemonJob => "job",
            ProcessKind::Subagent => "sub",
            ProcessKind::CrewMember => "crew",
            ProcessKind::WorkflowRun => "wf",
            ProcessKind::WorkflowNode => "wfn",
            ProcessKind::RemoteRun => "remote",
            ProcessKind::BackgroundShell => "sh",
            ProcessKind::ForegroundShell => "fgsh",
            ProcessKind::SideTask => "side",
            ProcessKind::BrowserSession => "browser",
            ProcessKind::VerifyCommand => "vfy",
            ProcessKind::HookCommand => "hook",
            ProcessKind::SandboxRun => "sbx",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ProcessTableError> {
        Ok(match value {
            "chat_turn" => ProcessKind::ChatTurn,
            "daemon_job" => ProcessKind::DaemonJob,
            "subagent" => ProcessKind::Subagent,
            "crew_member" => ProcessKind::CrewMember,
            "workflow_run" => ProcessKind::WorkflowRun,
            "workflow_node" => ProcessKind::WorkflowNode,
            "remote_run" => ProcessKind::RemoteRun,
            "background_shell" => ProcessKind::BackgroundShell,
            "foreground_shell" => ProcessKind::ForegroundShell,
            "side_task" => ProcessKind::SideTask,
            "browser_session" => ProcessKind::BrowserSession,
            "verify_command" => ProcessKind::VerifyCommand,
            "hook_command" => ProcessKind::HookCommand,
            "sandbox_run" => ProcessKind::SandboxRun,
            other => {
                return Err(ProcessTableError::UnknownKind {
                    kind: other.to_string(),
                })
            }
        })
    }

    /// The kinds whose worker dies with the desktop app, and which the app may
    /// therefore reap at startup.
    ///
    /// Deliberately excludes [`ProcessKind::DaemonJob`] and
    /// [`ProcessKind::RemoteRun`]: the resident daemon is a separate service
    /// that outlives the app, so an app that reaped everything it could not
    /// account for would declare live daemon work lost every time it launched.
    /// The daemon reaps its own through its engine tick.
    ///
    /// [`ProcessKind::WorkflowRun`]/[`ProcessKind::WorkflowNode`] are excluded
    /// for the same reason — a workflow can be hosted by the daemon via a
    /// trigger — even though a desktop-started one does die with the app. Those
    /// are swept by host liveness instead: see [`Self::HOST_RECORDED`].
    pub const DESKTOP_OWNED: &'static [ProcessKind] = &[
        ProcessKind::ChatTurn,
        ProcessKind::Subagent,
        ProcessKind::CrewMember,
        ProcessKind::BackgroundShell,
        ProcessKind::ForegroundShell,
        ProcessKind::SideTask,
        // Its Chromium is spawned by this app and dies with it — but only if the
        // app got to run its teardown. The reap is what covers the crash, and
        // for this kind alone the reap has something to kill first: see
        // [`crate::browser_worker::reclaim_orphaned_browser_sessions`].
        ProcessKind::BrowserSession,
        // The three bounded executions a turn blocks on. Each is started by this
        // app and dies with it when teardown runs; the reap is what covers the
        // crash, and like the shells they carry a durable containment identity
        // the reclaim can still reach.
        ProcessKind::VerifyCommand,
        ProcessKind::HookCommand,
        ProcessKind::SandboxRun,
    ];

    /// Kinds that record their host's pid, and are therefore swept by whether
    /// that host is still alive rather than by who is asking.
    ///
    /// These are the kinds with no fixed owner. Any process can host a workflow
    /// run — the desktop app and `monkey workflow run` both do, through the same
    /// `WorkflowService` and into the same ledger — so neither existing reaper
    /// could touch them: [`ProcessTable::reap_missing`] needs a caller that can
    /// enumerate its own live work, and the daemon's engine tick sweeps only
    /// `daemon_job`. They were the one gap left in crash coverage.
    ///
    /// Liveness also has a property ownership does not: a *dead* host's rows can
    /// be reaped by whoever starts next, so a daemon that crashes and is never
    /// restarted no longer leaves rows that only it could have cleaned up.
    pub const HOST_RECORDED: &'static [ProcessKind] =
        &[ProcessKind::WorkflowRun, ProcessKind::WorkflowNode];

    /// What happens to this kind when it exits without being asked to.
    ///
    /// Declared per kind for the same reason [`Self::signal_support`] is: the
    /// answer differs by kind, and the honest answer for most of them is
    /// "nothing". Stating it here makes that a decision rather than an
    /// omission, and gives a supervisor one place to read instead of each
    /// subsystem inventing its own retry rule — which is what it did before.
    ///
    /// Restarting means *re-running the work*, which requires a supervisor that
    /// outlives the process and a durable description of what to run. Only the
    /// daemon has both. A desktop-owned kind's loop lives in the WebView, so
    /// after a crash there is no loop left to restart and no supervisor awake to
    /// do it — hence [`RestartPolicy::Never`], not as a limitation to fix here
    /// but as the truth about what this process is. Making those restartable is
    /// K13 (freeze and restore), a different capability entirely.
    pub fn restart_policy(self) -> RestartPolicy {
        match self {
            // The one kind with a real supervisor: the daemon ticks
            // independently of any window, and a job carries a durable recipe
            // snapshot that fully describes how to run it again.
            ProcessKind::DaemonJob => RestartPolicy::OnFailure {
                max_attempts: 3,
                base_backoff_ms: 1_000,
            },
            // A workflow run owns per-node retry inside the executor, with its
            // own budgets and its own idempotency rules about which effects may
            // be replayed. Restarting the whole run from out here would re-run
            // committed side effects that the executor deliberately did not.
            ProcessKind::WorkflowRun | ProcessKind::WorkflowNode => RestartPolicy::Never,
            // A remote run records that a remote controller *asked* for work,
            // not the work itself — the daemon job it spawns is the process,
            // and that job carries the restart policy. Restarting a request
            // would mean submitting it a second time, which is the caller's
            // decision and not this supervisor's.
            ProcessKind::RemoteRun => RestartPolicy::Never,
            // Desktop-owned: the loop died with the window. See above.
            ProcessKind::ChatTurn
            | ProcessKind::Subagent
            | ProcessKind::CrewMember
            | ProcessKind::BackgroundShell
            // A foreground shell is a *tool call's* process: the turn that
            // blocked on it is what decides whether to run the command again,
            // and re-running it from out here would repeat a side effect the
            // model never asked for twice.
            | ProcessKind::ForegroundShell
            | ProcessKind::SideTask
            // A verify command, a hook and a sandbox run are each one caller's
            // tool call. Re-running one from out here would repeat a side effect
            // — a formatter rewriting files, a build publishing artifacts — that
            // nobody asked for twice.
            | ProcessKind::VerifyCommand
            | ProcessKind::HookCommand
            | ProcessKind::SandboxRun => RestartPolicy::Never,
            // A browser session is a *tool call's* resource, not work in its own
            // right: relaunching Chromium would restore a blank profile with no
            // navigation history and no grant, which is not the session that was
            // lost. The turn that owned it is what retries, if anything does.
            ProcessKind::BrowserSession => RestartPolicy::Never,
        }
    }

    /// Whether this kind honours `signal`, and if not, why.
    ///
    /// This is a statement about what the code does *today*, not an aspiration.
    /// Suspend/resume exist in three shapes: real OS suspend of a child this
    /// app owns (`DaemonJob`, `BackgroundShell`, via a shared SIGSTOP/SIGCONT
    /// primitive — see `os_signal.rs`), a cooperative durable latch a loop
    /// checks at its own safe point (`ChatTurn`, `Subagent`, `CrewMember`,
    /// `SideTask`), and a blocking wait at a coarse-grained boundary
    /// (`WorkflowRun`, at each level). `kill` is only meaningful where this app
    /// owns an OS process.
    ///
    /// Two deliberate holdouts. `WorkflowNode`: nothing ever signals a node's
    /// own process id, and a node mid-execution has no yield point of its own —
    /// the executor observes intent only at the *run's* level boundary.
    /// `RemoteRun`: its row records that a remote controller asked for work,
    /// and the only writer (`project_queue_origin`) closes it as soon as the
    /// job is queued; the daemon job it spawned is the process that can be
    /// suspended or killed. Claiming `Honoured` on either would be exactly the
    /// dishonesty this function exists to prevent — and for `RemoteRun` it
    /// briefly was: the matrix said `Honoured` while no delivery path for that
    /// kind existed in the daemon or the desktop fan-out.
    ///
    /// Each refusal names the mechanism that is missing, or the correct target
    /// to signal instead, rather than saying "unsupported" — because the
    /// caller's next question is always "why not".
    /// Who enforces each [`ProcessLimits`] field for this kind, or why nobody
    /// does.
    ///
    /// This is K4's declaration contract made answerable. `ProcessLimits` is a
    /// declaration record, and a positive value in it used to mean nothing in
    /// particular: `process_admit` accepted any override for any kind, so a row
    /// could carry a memory ceiling on a kind with no memory watchdog and a
    /// child-process ceiling that no platform primitive here can hold. A limit
    /// nobody reads is worse than an absent one, because it reads as a bound to
    /// everything downstream — the run dashboard, `monkey processes show`, and
    /// anyone auditing what this app promised to contain.
    ///
    /// Exhaustive over both enums on purpose: a new kind or a new field cannot
    /// be added without answering this question for it, and
    /// `every_kind_and_limit_pair_is_answered` fails if an arm is ever left to a
    /// catch-all.
    ///
    /// The three `Unavailable` reasons that repeat are the deferred platform
    /// legs, and they are stated here rather than only in the roadmap so the
    /// answer travels with the code: memory needs delegated cgroups v2 or a
    /// class-derived job object (`RLIMIT_RSS` is a no-op on Darwin and advisory
    /// on Linux, `RLIMIT_AS` bounds address space rather than residency), and a
    /// child-process ceiling needs the cgroup `pids` controller or a job object
    /// (`RLIMIT_NPROC` counts per real uid, not per tree, so a value low enough
    /// to matter fires whenever the user's own session is busy).
    pub fn limit_support(self, limit: ProcessLimitKind) -> LimitEnforcement {
        use LimitEnforcement as E;
        use ProcessKind as K;
        use ProcessLimitKind as L;

        // Both name what is missing *for this kind*, which changed when the
        // resource controller landed: it is no longer that no mechanism exists —
        // the two shell kinds are bounded by one — but that these kinds own no OS
        // process for a controller to attach to. Leaving the old "no mechanism on
        // any host" would read as a platform gap rather than as what it is.
        const NO_MEMORY_MECHANISM: &str =
            "this kind owns no OS process tree to measure: its work runs inside the WebView, \
             or is delegated to a child process that carries its own record";
        const NO_PIDS_MECHANISM: &str =
            "this kind owns no OS process tree to count: bound the native process it spawns, \
             which is its child in this table";
        const NO_MODEL_REQUEST: &str = "this kind issues no model request of its own";
        const NO_CAPTURED_OUTPUT: &str = "this kind captures no output stream of its own";

        match (self, limit) {
            // --- Wall ------------------------------------------------------
            // The four WebView kinds are the only ones whose wall bound is read
            // off the row: `processWallBudget.ts` sweeps `max_wall_ms` against
            // `started_at_ms`, and the six-hour class default plus the Settings
            // override both arrive through this field.
            (K::ChatTurn | K::Subagent | K::CrewMember | K::SideTask, L::Wall) => {
                E::Enforced("swept by the WebView wall-budget sweep against `started_at_ms`")
            }
            // Real bounds whose number comes from the owner, not the caller.
            (K::DaemonJob, L::Wall) => E::OwnerSourced(
                "the daemon watchdog enforces the job recipe's own `max_runtime_ms`",
            ),
            (K::WorkflowRun, L::Wall) => E::OwnerSourced(
                "the executor enforces the definition's `budgets.maximum_wall_time_ms`",
            ),
            (K::BrowserSession, L::Wall) => {
                E::OwnerSourced("the browser watchdog enforces its session's own `max_session_ms`")
            }
            // The two native shell kinds. Both now run under a
            // `ResourceController`, whose sampling loop reads this row's field —
            // so a value set here is the value that fires, which is what
            // `Enforced` means. A background shell is still spawned to outlive
            // its turn, so its class default states no wall bound; a caller that
            // sets one now gets it.
            (
                K::BackgroundShell
                | K::ForegroundShell
                | K::VerifyCommand
                | K::HookCommand
                | K::SandboxRun,
                L::Wall,
            ) => E::Enforced(
                "the resource controller's sampling loop compares elapsed time against this \
                 row's `max_wall_ms` and terminates the whole owned tree",
            ),
            (K::WorkflowNode, L::Wall) => E::OwnerSourced(
                "a node is bounded by its own `timeout_ms`, which the definition validates \
                 against the run's wall budget",
            ),
            (K::RemoteRun, L::Wall) => E::Unavailable(
                "the row is terminal from birth — it records that a controller asked for \
                 work; bound the daemon job it queued instead",
            ),

            // --- Memory ----------------------------------------------------
            (K::DaemonJob, L::Memory) => E::OwnerSourced(
                "the daemon's sampling watchdog measures the job's whole process group \
                 against the recipe's own `max_memory_bytes`",
            ),
            // The kinds that own a native process tree read this field through
            // the resource controller: a cgroup v2 `memory.max` where the host
            // delegates one, a job object's `JobMemoryLimit` on Windows, and a
            // supervised sum over the owned tree everywhere else. All three
            // measure the *tree*, so a shell whose grandchild holds the memory is
            // bounded by the number on this row.
            (
                K::BackgroundShell
                | K::ForegroundShell
                | K::VerifyCommand
                | K::HookCommand
                | K::SandboxRun,
                L::Memory,
            ) => E::Enforced(
                "the resource controller bounds the owned process tree at this row's \
                 `max_memory_bytes`, kernel-held where the host offers a mechanism and \
                 supervised otherwise",
            ),
            // The third kind that owns a real process tree, and now routed through
            // the same controller. The reconciliation the old note said was
            // needed is done by *splitting by resource*: the controller holds
            // memory and child-process count over Chromium's whole tree, while
            // `browser_worker` keeps the browser-domain quotas — session clock,
            // action budget, profile disk — which no controller can express.
            (K::BrowserSession, L::Memory) => E::Enforced(
                "the resource controller bounds the whole Chromium tree at this row's \
                 `max_memory_bytes`, kernel-held where the host offers a mechanism and \
                 supervised otherwise",
            ),
            (_, L::Memory) => E::Unavailable(NO_MEMORY_MECHANISM),

            // --- Output ----------------------------------------------------
            // The one field a desktop kind genuinely reads off its class default.
            (K::BackgroundShell, L::Output) => E::Enforced(
                "the in-memory tail is front-truncated at this many bytes by \
                 `background_shell`",
            ),
            (K::ForegroundShell | K::VerifyCommand | K::HookCommand | K::SandboxRun, L::Output) => {
                E::Enforced(
                    "both pipes are drained concurrently into a buffer front-truncated at this \
                 many bytes, so the bound holds while the child is still producing",
                )
            }
            (K::DaemonJob, L::Output) => E::OwnerSourced(
                "the daemon watchdog enforces the recipe's own `max_log_bytes` against the \
                 job's log file",
            ),
            (
                K::ChatTurn
                | K::Subagent
                | K::CrewMember
                | K::SideTask
                | K::WorkflowRun
                | K::WorkflowNode
                | K::RemoteRun
                | K::BrowserSession,
                L::Output,
            ) => E::Unavailable(NO_CAPTURED_OUTPUT),

            // --- Child processes -------------------------------------------
            // Nothing, anywhere. The Windows shell job's fixed 512-process
            // ceiling is a containment guardrail on one spawn site, not a
            // per-class policy this field could express.
            // Per *tree*, which is what makes this expressible at all: cgroup
            // v2's `pids` controller, a job object's `ActiveProcessLimit`, or a
            // supervised count of the owned tree's live members. None of them is
            // `RLIMIT_NPROC`, which counts per real uid and so fires whenever the
            // logged-in user's own session is busy.
            (
                K::BackgroundShell
                | K::ForegroundShell
                | K::VerifyCommand
                | K::HookCommand
                | K::SandboxRun,
                L::ChildProcesses,
            ) => E::Enforced(
                "the resource controller counts the owned tree's live members against this \
                 row's `max_child_processes`, per tree rather than per uid",
            ),
            (K::BrowserSession, L::ChildProcesses) => E::Enforced(
                "the resource controller counts Chromium's renderer, GPU and utility children \
                 against this row's `max_child_processes`, per tree rather than per uid",
            ),
            (_, L::ChildProcesses) => E::Unavailable(NO_PIDS_MECHANISM),

            // --- Context tokens --------------------------------------------
            // Enforced pre-flight for a runtime that can count exactly. The
            // qualification is the runtime, not the kind: a budget set on a
            // process running Ollama or MLX is reported unenforceable rather
            // than silently ignored, which is why this stays `Enforced` here and
            // the runtime check lives at the request.
            (K::ChatTurn | K::Subagent | K::CrewMember | K::SideTask, L::ContextTokens) => {
                E::Enforced(
                    "checked before the request by `m3_production` for runtimes that count \
                     exactly; a runtime without a tokenizer reports it unenforceable",
                )
            }
            (
                K::DaemonJob
                | K::WorkflowRun
                | K::WorkflowNode
                | K::RemoteRun
                | K::BackgroundShell
                | K::ForegroundShell
                | K::BrowserSession
                | K::VerifyCommand
                | K::HookCommand
                | K::SandboxRun,
                L::ContextTokens,
            ) => E::Unavailable(NO_MODEL_REQUEST),
        }
    }

    pub fn signal_support(self, signal: ProcessSignal) -> SignalSupport {
        use ProcessKind as K;
        use ProcessSignal as S;
        match (self, signal) {
            // Stop is universal: every kind has a cancellation path. A
            // `RemoteRun` row is terminal from birth, so `signal` answers
            // `AlreadyExited` before this is ever consulted for one.
            (_, S::Stop) => SignalSupport::Honoured,

            // Kill needs an OS process this app owns.
            // The browser session earns `Kill` for the same reason the other two
            // do, and it is the reason this kind exists at all: its `native_pid`
            // leads a real process group, so a terminate reaches Chromium's
            // renderer and GPU children rather than orphaning them.
            (
                K::DaemonJob | K::BackgroundShell | K::ForegroundShell | K::BrowserSession,
                S::Kill,
            ) => SignalSupport::Honoured,
            (K::RemoteRun, S::Kill | S::Suspend | S::Resume) => SignalSupport::Refused(
                "a remote run records the request, not the work: it owns no process of its \
                 own and is closed as soon as the job is queued; signal the daemon job it \
                 spawned, which is its child in this table",
            ),
            // These three own a real tree and their controller can reclaim it —
            // what is missing is a *deliverer*: nothing reads the durable latch
            // between the spawn and the wait, because each is a single blocking
            // call inside the turn that started it. Refused with the target that
            // does work rather than downgraded to a `Stop` that lands nowhere,
            // which is the same rule `workflow_node` is held to.
            (
                K::VerifyCommand | K::HookCommand | K::SandboxRun,
                S::Kill | S::Suspend | S::Resume,
            ) => SignalSupport::Refused(
                "this execution is one blocking step of the turn that started it, and nothing \
                 reads a signal latch while it runs; stop that turn instead, which cancels the \
                 command and reclaims its whole tree",
            ),
            (_, S::Kill) => SignalSupport::Refused(
                "this kind owns no OS process to terminate; stop it instead, which winds it \
                 down at its next safe point",
            ),

            // Suspend/resume: real OS suspend where we own a child, cooperative
            // where the loop now checks a durable latch at a safe point, or a
            // blocking wait at a coarse boundary for a workflow run.
            (K::DaemonJob, S::Suspend | S::Resume) => SignalSupport::Honoured,
            (K::BackgroundShell | K::ForegroundShell, S::Suspend | S::Resume) => {
                SignalSupport::Honoured
            }
            (K::SideTask, S::Suspend | S::Resume) => SignalSupport::Honoured,
            (K::ChatTurn | K::Subagent | K::CrewMember, S::Suspend | S::Resume) => {
                SignalSupport::Honoured
            }
            (K::WorkflowRun, S::Suspend | S::Resume) => SignalSupport::Honoured,
            // Refused deliberately, and by the same rule as `workflow_node`:
            // nothing delivers it. A SIGSTOP'd Chromium would keep its CDP
            // socket open while answering nothing, so every in-flight action
            // would hang to its own timeout instead of pausing — and the latch
            // would sit `suspend_requested` with no deliverer to clear it.
            (K::BrowserSession, S::Suspend | S::Resume) => SignalSupport::Refused(
                "a browser session has no pause that a caller could resume from: stopping its \
                 Chromium mid-action leaves the DevTools connection open but unanswering, so \
                 every in-flight action would time out rather than park. Stop the session \
                 instead, which tears it down and reclaims its profile",
            ),
            (K::WorkflowNode, S::Suspend | S::Resume) => SignalSupport::Refused(
                "a workflow node has no independent pause mechanism and no safe point of its \
                 own; suspend the owning workflow run instead, which the executor observes at \
                 each level boundary",
            ),
        }
    }

    /// Every signal this kind honours.
    pub fn honoured_signals(self) -> Vec<ProcessSignal> {
        ProcessSignal::ALL
            .iter()
            .copied()
            .filter(|signal| self.signal_support(*signal).is_honoured())
            .collect()
    }

    /// Every kind, for exhaustive tests and for `monkey processes --kind`
    /// validation.
    pub const ALL: &'static [ProcessKind] = &[
        ProcessKind::ChatTurn,
        ProcessKind::DaemonJob,
        ProcessKind::Subagent,
        ProcessKind::CrewMember,
        ProcessKind::WorkflowRun,
        ProcessKind::WorkflowNode,
        ProcessKind::RemoteRun,
        ProcessKind::BackgroundShell,
        ProcessKind::ForegroundShell,
        ProcessKind::SideTask,
        ProcessKind::BrowserSession,
        ProcessKind::VerifyCommand,
        ProcessKind::HookCommand,
        ProcessKind::SandboxRun,
    ];

    /// The bounds a process of this kind is *actually* subject to, seeded into
    /// every [`AdmitProcess`] and [`ProcessProjection`] so a row carries its
    /// class's declaration without each adopter restating it — K4's "limits are
    /// set from the process's class, not hardcoded".
    ///
    /// Declared per kind for the same reason [`Self::restart_policy`] is: the
    /// answer differs by kind, and stating it in one place makes it a decision
    /// rather than an omission. Before this, only the daemon populated a limit
    /// set at all, so the other eight kinds recorded all-`None` — which reads as
    /// "unbounded" and was indistinguishable from "nobody looked".
    ///
    /// # `None` is a finding, not an unfinished cell
    ///
    /// A `None` here means this app genuinely does not bound that resource for
    /// that kind, and the row should say so rather than carry a number nothing
    /// enforces. Most fields remain absent. The four WebView kinds do carry the
    /// configurable wall budget below, while their tools also have independent
    /// call timeouts (`SHELL_TIMEOUT`, `DEFAULT_VERIFY_TIMEOUT_SECS`).
    ///
    /// Deliberately not invented here: a memory number per kind would be a guess
    /// presented as policy. Kernel enforcement now exists for Unix tool children
    /// ([`crate::os_limits`]) and as fixed Windows shell-job guardrails, but
    /// neither supplies portable `ProcessLimits` semantics — see that module for
    /// why `RLIMIT_CPU`, `NPROC`, `RSS` and `AS` are the wrong instruments.
    pub fn default_limits(self) -> ProcessLimits {
        match self {
            // The one desktop kind with a real, enforced ceiling: a backgrounded
            // shell keeps a front-truncated output tail bounded by
            // `background_shell::MAX_OUTPUT_BYTES`. That bound has always been
            // enforced while the row claimed `None`, so the record was actively
            // wrong rather than merely silent.
            //
            // The subsystem's constant is referenced rather than copied. It puts
            // a dependency from the generic ledger onto one subsystem, which is
            // the lesser evil: a second copy of the number could drift from the
            // code that enforces it, and a declaration that disagrees with the
            // enforcement is worse than a slightly untidy dependency.
            // The agent shell a turn blocks on, and the first kind whose class
            // default states a memory and a process-count bound — because it is
            // the first kind with a mechanism that holds them.
            //
            // Both numbers are deliberately generous rather than tuned. The
            // question a class default answers is "what is so far outside normal
            // that it is certainly a runaway", not "what should a build need":
            // this is the site that legitimately compiles a Rust workspace and
            // downloads a 40 GB model, so a number chosen to be *tight* would
            // break working commands, which is the failure mode that makes people
            // turn a limit off. A per-command override tightens it where a caller
            // knows better.
            ProcessKind::ForegroundShell => ProcessLimits {
                max_wall_ms: None,
                max_memory_bytes: Some(SHELL_MEMORY_BUDGET_BYTES),
                max_output_bytes: Some(
                    u64::try_from(crate::output_cap::MODEL_OUTPUT_CAP).unwrap_or(u64::MAX),
                ),
                max_child_processes: Some(SHELL_PROCESS_BUDGET),
                max_context_tokens: None,
            },
            ProcessKind::BackgroundShell => ProcessLimits {
                max_output_bytes: Some(
                    u64::try_from(crate::background_shell::MAX_OUTPUT_BYTES).unwrap_or(u64::MAX),
                ),
                // The same two class bounds as the foreground shell, for the same
                // reason and through the same controller. A backgrounded command
                // is if anything the one more worth bounding: nothing is blocked
                // on it, so a runaway is noticed later.
                max_memory_bytes: Some(SHELL_MEMORY_BUDGET_BYTES),
                max_child_processes: Some(SHELL_PROCESS_BUDGET),
                // No wall bound on purpose: a background shell is meant to
                // outlive the turn that started it, so it is spawned with
                // neither a timeout nor `kill_on_drop`.
                ..ProcessLimits::default()
            },
            // The daemon writes its own per-job `max_runtime_ms`/
            // `max_memory_bytes`/`max_log_bytes` onto the row, which are truer
            // than any class default because they came from the job's own
            // recipe. A non-empty default here would be overwritten on the next
            // projection anyway, so claiming one would only mislead a reader
            // between admission and the first tick.
            ProcessKind::DaemonJob => ProcessLimits::default(),
            // The four WebView kinds. Each runs an unbounded number of *bounded*
            // tool calls, so the tools were capped and the process issuing them
            // was not — `processWallBudget.ts` built the enforcement and then
            // shipped it inert, because nothing chose a number.
            //
            // This chooses one. [`WEBVIEW_WALL_BUDGET_MS`] explains the value; what
            // matters here is that it is a *class* default, seeded into every row
            // at admission, and that an explicit budget from the caller still wins
            // — which is how the wall-budget setting overrides it, and how a
            // caller turns it off entirely.
            ProcessKind::ChatTurn
            | ProcessKind::Subagent
            | ProcessKind::CrewMember
            | ProcessKind::SideTask => ProcessLimits {
                max_wall_ms: Some(WEBVIEW_WALL_BUDGET_MS),
                ..ProcessLimits::default()
            },
            // Still nothing per process. A workflow run and node carry the
            // executor's own per-node budgets, which are not these fields; and a
            // remote run records that a controller *asked* for work rather than
            // the work itself, so the daemon job it spawns is what carries limits.
            ProcessKind::WorkflowRun | ProcessKind::WorkflowNode | ProcessKind::RemoteRun => {
                ProcessLimits::default()
            }
            // The second kind with a real, enforced wall bound, and the only
            // desktop one. `browser_worker`'s watchdog already reclaims a
            // session past `BrowserLimits::max_session_ms` on a 30-second sweep;
            // this declares the number the sweep enforces rather than inventing
            // a second one, on the same terms as `BackgroundShell` above.
            //
            // A per-session override is not read here: this is the *class*
            // default, and a caller that starts a session with a tighter budget
            // writes its own `max_wall_ms` onto the row through the projection,
            // exactly as the daemon does with its per-job recipe.
            //
            // The two process bounds beneath it belong to the *other* owner. A
            // browser session owns a real process tree — Chromium's renderer, GPU
            // and utility children — and it is now routed through the same
            // `ResourceController` the shells use, which holds memory and
            // child-process count while the sweep keeps the session clock. One
            // resource, one owner, both stated on this row.
            //
            // Generous rather than tuned, for the shell's reason and more so: a
            // page with video across a dozen renderers is legitimately hundreds of
            // megabytes, and a number tight enough to argue about would break real
            // browsing. These answer "is this tab now the largest thing on the
            // machine".
            ProcessKind::BrowserSession => ProcessLimits {
                max_wall_ms: Some(crate::browser_worker::DEFAULT_MAX_SESSION_MS),
                max_memory_bytes: Some(BROWSER_MEMORY_BUDGET_BYTES),
                max_child_processes: Some(BROWSER_PROCESS_BUDGET),
                ..ProcessLimits::default()
            },
            // The three bounded executions. Each already ran under the shell's
            // tree bounds plus a deadline and an output cap of its own; what was
            // missing was a row to state them on, so the numbers are referenced
            // from the modules that enforce them rather than copied — a
            // declaration that can drift from its enforcement is the failure this
            // whole matrix exists to prevent.
            ProcessKind::VerifyCommand => ProcessLimits {
                max_wall_ms: Some(crate::verify::DEFAULT_VERIFY_TIMEOUT_SECS * 1_000),
                max_memory_bytes: Some(SHELL_MEMORY_BUDGET_BYTES),
                max_output_bytes: Some(
                    u64::try_from(crate::verify::VERIFY_OUTPUT_CAP).unwrap_or(u64::MAX),
                ),
                max_child_processes: Some(SHELL_PROCESS_BUDGET),
                max_context_tokens: None,
            },
            ProcessKind::HookCommand => ProcessLimits {
                max_wall_ms: Some(
                    u64::try_from(crate::hooks::HOOK_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
                ),
                max_memory_bytes: Some(SHELL_MEMORY_BUDGET_BYTES),
                max_output_bytes: Some(
                    u64::try_from(crate::hooks::HOOK_OUTPUT_CAP).unwrap_or(u64::MAX),
                ),
                max_child_processes: Some(SHELL_PROCESS_BUDGET),
                max_context_tokens: None,
            },
            // No class wall bound: a sandbox run's deadline is chosen per run by
            // the caller and always supplied, so a number here would be a second
            // default that never applies.
            ProcessKind::SandboxRun => ProcessLimits {
                max_wall_ms: None,
                max_memory_bytes: Some(SHELL_MEMORY_BUDGET_BYTES),
                max_output_bytes: Some(
                    u64::try_from(crate::sandbox::SANDBOX_OUTPUT_CAP).unwrap_or(u64::MAX),
                ),
                max_child_processes: Some(SHELL_PROCESS_BUDGET),
                max_context_tokens: None,
            },
        }
    }
}

/// The one state vocabulary, replacing four incompatible ones.
///
/// Intentionally coarse. A daemon job's `WaitingApproval` and a ledger run's
/// `WaitingForPermission` are both `Running` here — the process exists, holds
/// its reservations, and is not making progress; that distinction belongs to
/// the kind's own record, not to the arbitration layer. `Suspended` means the
/// process has been deliberately stopped and can be resumed, which today only
/// the daemon can actually do (see K2 in `docs/agent-os-roadmap.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    /// Accepted and accounted for, not yet executing.
    Admitted,
    /// Executing, or waiting on something while still holding its place.
    Running,
    /// Deliberately stopped, resumable.
    Suspended,
    /// Finished. Terminal, and always carries an [`ExitStatus`].
    Exited,
}

impl ProcessState {
    pub fn as_str(self) -> &'static str {
        match self {
            ProcessState::Admitted => "admitted",
            ProcessState::Running => "running",
            ProcessState::Suspended => "suspended",
            ProcessState::Exited => "exited",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ProcessTableError> {
        Ok(match value {
            "admitted" => ProcessState::Admitted,
            "running" => ProcessState::Running,
            "suspended" => ProcessState::Suspended,
            "exited" => ProcessState::Exited,
            other => {
                return Err(ProcessTableError::UnknownState {
                    state: other.to_string(),
                })
            }
        })
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, ProcessState::Exited)
    }

    /// The authoritative transition table. Mirrored by the
    /// `agent_processes_validate_transition` SQL trigger; the two are asserted
    /// to agree in this module's tests.
    pub fn can_transition_to(self, next: ProcessState) -> bool {
        match (self, next) {
            (a, b) if a == b => true,
            (ProcessState::Admitted, ProcessState::Running | ProcessState::Exited) => true,
            (ProcessState::Running, ProcessState::Suspended | ProcessState::Exited) => true,
            (ProcessState::Suspended, ProcessState::Running | ProcessState::Exited) => true,
            _ => false,
        }
    }
}

/// How a process ended. One vocabulary in place of `Failed{code,message,
/// retryable}` / `last_error: Option<String>` / a bare `exit_code` / nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitStatus {
    Succeeded,
    Failed,
    Cancelled,
    /// Terminated for exceeding one of its [`ProcessLimits`]. Distinguishable
    /// from `Failed` on purpose: a limit kill is the system working, and the
    /// ledger event must name which limit.
    LimitExceeded,
    /// The worker went away without reporting — a crashed WebView, a killed
    /// child, an expired lease. Recorded by a reaper rather than by the
    /// process itself.
    Lost,
    /// Ended with external effects that could not be safely undone.
    NeedsReconciliation,
    /// The row had to be closed and **nothing proved the work was gone**.
    ///
    /// Deliberately not [`Self::Lost`], which asserts a fact: a lost process is
    /// one whose worker demonstrably went away. This one asserts the absence of a
    /// fact — a restart found a row whose containment identity could not be
    /// validated, or could be validated and would not empty, so the app stopped
    /// tracking a workload that may still be executing.
    ///
    /// The distinction is the whole point. Overloading `Lost` to mean "we stopped
    /// looking" is what turns a process table into a record of what the app
    /// believes rather than of what happened, and a reader auditing a machine
    /// after a crash needs to be able to tell "confirmed dead" from "ownership
    /// could not be recovered".
    ContainmentLost,
}

impl ExitStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ExitStatus::Succeeded => "succeeded",
            ExitStatus::Failed => "failed",
            ExitStatus::Cancelled => "cancelled",
            ExitStatus::LimitExceeded => "limit_exceeded",
            ExitStatus::Lost => "lost",
            ExitStatus::NeedsReconciliation => "needs_reconciliation",
            ExitStatus::ContainmentLost => "containment_lost",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ProcessTableError> {
        Ok(match value {
            "succeeded" => ExitStatus::Succeeded,
            "failed" => ExitStatus::Failed,
            "cancelled" => ExitStatus::Cancelled,
            "limit_exceeded" => ExitStatus::LimitExceeded,
            "lost" => ExitStatus::Lost,
            "needs_reconciliation" => ExitStatus::NeedsReconciliation,
            "containment_lost" => ExitStatus::ContainmentLost,
            other => {
                return Err(ProcessTableError::UnknownExitStatus {
                    status: other.to_string(),
                })
            }
        })
    }
}

/// The prefix a wall-budget stop carries in `signal_reason`.
///
/// The durable channel for "this stop came from a budget, not a person". Same
/// shape, and the same reason, as the daemon's marked `last_error`: the row is
/// written by whoever latches the stop and read back later by whoever records the
/// exit, with nothing in memory connecting the two.
///
/// **This constant is the authority and `processWallBudget.ts` mirrors it.** The
/// enforcer that writes the reason runs in the WebView and cannot import a Rust
/// const, so the literal exists on both sides; the TypeScript copy names this one
/// and a test on each side pins the string. A generated shared constant would be
/// better and is not worth a codegen step for one string.
pub const WALL_BUDGET_REASON_PREFIX: &str = "wall budget exceeded: max_wall_ms";

/// Reclassify a `cancelled` exit as [`ExitStatus::LimitExceeded`] when the stop
/// that caused it was a budget kill.
///
/// Only ever *upgrades*, and only from `Cancelled`. A `Failed` or `Succeeded` exit
/// is the work's own verdict and a pending budget stop does not override it — a
/// turn that failed on its own while a budget stop was in flight failed, and
/// relabelling that would hide a real error behind a limit.
///
/// The reason is replaced too, not just the status: the loops write "stopped by
/// the user", which for a budget kill is not merely imprecise but false.
fn upgrade_a_budget_kill(
    exit: Option<ProcessExit>,
    signal_reason: Option<&str>,
) -> Option<ProcessExit> {
    let exit = exit?;
    let is_budget_kill =
        signal_reason.is_some_and(|reason| reason.starts_with(WALL_BUDGET_REASON_PREFIX));
    if exit.status != ExitStatus::Cancelled || !is_budget_kill {
        return Some(exit);
    }
    Some(ProcessExit {
        status: ExitStatus::LimitExceeded,
        reason: signal_reason.map(str::to_string),
        ..exit
    })
}

/// Whether the pid on this row still belongs to the process the row is about.
///
/// # The rule every reconciler has to obey
///
/// A row outlives the app session that wrote it, which is the entire point of a
/// durable process table — and it is why "is something alive at that pid" is not
/// a safe question to act on. Pids are reused; across a restart, hours later, on
/// a busy machine, the number a previous session recorded is as likely to name
/// the user's editor as the shell this app abandoned.
///
/// So a startup reclaim signals only what it can *prove*: the pid is still the
/// process whose start time this row recorded. Three things answer `false`,
/// deliberately:
///
/// - no pid — the row says nothing about anything running;
/// - no start time — a pre-V22 row, or a host that would not report one, so the
///   pid cannot be tied to a process;
/// - a start time that does not match — the pid was reused, and the process
///   behind it is somebody else's;
/// - the process is no longer *executing* — it exited and is waiting to be
///   collected, so there is no tree left to reclaim and the next thing to occupy
///   that pid will be somebody else's.
///
/// The asymmetry is the safety property: failing to reclaim one stale process is
/// recoverable, and killing an unrelated one is not.
#[must_use]
pub fn still_the_recorded_process(record: &ProcessRecord) -> bool {
    let Some(pid) = record.native_pid.and_then(|pid| u32::try_from(pid).ok()) else {
        return false;
    };
    let Some(recorded) = record
        .native_start_time
        .and_then(|start| u64::try_from(start).ok())
    else {
        return false;
    };
    crate::process_tree::ProcessIdentity {
        pid,
        start_time: recorded,
    }
    .is_running()
}

/// The limit set attached to a process.
///
/// `None` means "not bounded by this process record" — honest, and different
/// from zero. Nothing in *this module* enforces these; it records them.
/// [`ProcessKind::default_limits`] is where each kind's set comes from, and the
/// enforcement, where it exists, lives with whoever owns the process.
///
/// Recording a value here still does not make it enforced. The field docs below
/// say which are backed by something and which are declaration only, rather than
/// implying a uniform guarantee that does not exist.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessLimits {
    /// Wall-clock budget. Enforced by the daemon watchdog for `daemon_job`, the
    /// WebView process sweep for chat/subagent/Crew/side-task loops, and the
    /// browser-session watchdog. Other kinds leave it absent or use subsystem
    /// budgets that are not represented by this field.
    pub max_wall_ms: Option<u64>,
    /// Resident memory ceiling. Enforced only for `daemon_job`, by a sampling
    /// watchdog that measures the whole process group.
    ///
    /// Deliberately *not* enforced by `setrlimit`, which does exist now for tool
    /// children ([`crate::os_limits`]): `RLIMIT_RSS` is a no-op on Darwin and
    /// advisory on Linux, and `RLIMIT_AS` bounds virtual address space rather than
    /// resident memory. `ProcessLimits`-backed enforcement would need delegated
    /// cgroups v2 or class-derived Windows jobs. Windows shell jobs do exist, but
    /// their fixed containment ceiling is deliberately not recorded as this
    /// field or advertised as portable per-process policy.
    pub max_memory_bytes: Option<u64>,
    /// Captured output ceiling. Enforced for `daemon_job` (log-file size) and for
    /// `background_shell`, whose in-memory tail is front-truncated at this many
    /// bytes.
    pub max_output_bytes: Option<u64>,
    /// Child-process ceiling. Declaration only, for every kind.
    ///
    /// `RLIMIT_NPROC` cannot deliver this: it counts processes per real uid rather
    /// than per tree, so a value low enough to matter fails whenever the user's
    /// own session is busy. Portable field-backed enforcement needs the cgroup
    /// `pids` controller or a class-derived job object; the fixed Windows shell
    /// job ceiling is a narrower containment guardrail, not this declaration.
    pub max_child_processes: Option<u32>,
    /// Prompt-token ceiling for one request (roadmap K11). **Enforced**, for
    /// runtimes that can count exactly — today that is llama.cpp, via
    /// `m3_production`'s pre-flight; Ollama and MLX expose no tokenizer, so a
    /// budget set on a process running either is reported as unenforceable rather
    /// than silently ignored.
    ///
    /// Enforced *before* the request rather than discovered from the runtime's
    /// refusal afterwards, which is the whole point of the acceptance criterion:
    /// `classify_context_failure` explains a failure that already happened, and a
    /// limit that only explains is not a limit.
    ///
    /// # It ships enforced and unset
    ///
    /// Nothing picks a number: `default_limits` returns `None` for every kind and
    /// no admit call site passes one. Unlike the WebView wall budget, which now
    /// has a configurable six-hour default, this mechanism is live and fires for
    /// nobody until a context budget is configured. Choosing one is a judgement
    /// about what a conversation is *for* — too low silently ends long sessions
    /// that were working fine — and belongs to settings, not to a constant.
    pub max_context_tokens: Option<u64>,
}

impl ProcessLimits {
    pub fn is_unbounded(&self) -> bool {
        *self == ProcessLimits::default()
    }
}

/// What a process actually consumed, as opposed to what it was allowed to.
///
/// The reading counterpart to [`ProcessLimits`]' declarations, and the reason
/// this table needed migration V8: nothing here recorded a measurement before,
/// so "what did that turn cost" had no answer.
///
/// **`None` means not measured. It never means zero.** A ledger that reports 0
/// bytes egressed for a process nobody measured is worse than one that reports
/// nothing, because the zero is indistinguishable from a real measurement of no
/// egress — and a cost attribution built on inferred zeros is wrong in the
/// direction that looks fine. Which is why this type cannot be constructed with
/// a gap that has no stated reason: see [`ProcessUsage::new`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeasuredUsage {
    /// User + system CPU, milliseconds. Sampled from the OS while the process
    /// lived — see [`crate::process_usage`].
    pub cpu_time_ms: Option<u64>,
    pub peak_rss_bytes: Option<u64>,
    pub bytes_read: Option<u64>,
    pub bytes_written: Option<u64>,
    /// Network bytes attributed to this process. No platform reports this per
    /// process, so it is only ever populated by whoever accounts for egress.
    pub bytes_egressed: Option<u64>,
    /// From the run's `UsageRecorded`/`Completed` events. Structurally
    /// unavailable for a kind whose `run_id` is NULL — a subagent and the `m4`
    /// workflow kinds have no ledger run to read.
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    /// Always `None` today. Nothing in this tree measures per-process GPU
    /// residency or device time; the columns exist so a runtime that starts
    /// reporting them needs no schema change, and until one does the reason is
    /// recorded rather than a zero invented.
    pub gpu_resident_bytes: Option<u64>,
    pub gpu_device_ms: Option<u64>,
}

impl MeasuredUsage {
    /// Every field paired with the wire name a note must use to explain it.
    ///
    /// This list *is* the invariant's definition: a column added to V8's set
    /// without an entry here would be free to go NULL with nothing recorded
    /// about why, which is the single failure mode the resource ledger exists to
    /// prevent.
    fn fields(&self) -> [(&'static str, Option<u64>); 9] {
        [
            (FIELD_CPU_TIME_MS, self.cpu_time_ms),
            (FIELD_PEAK_RSS_BYTES, self.peak_rss_bytes),
            (FIELD_BYTES_READ, self.bytes_read),
            (FIELD_BYTES_WRITTEN, self.bytes_written),
            (FIELD_BYTES_EGRESSED, self.bytes_egressed),
            (FIELD_TOKENS_IN, self.tokens_in),
            (FIELD_TOKENS_OUT, self.tokens_out),
            (FIELD_GPU_RESIDENT_BYTES, self.gpu_resident_bytes),
            (FIELD_GPU_DEVICE_MS, self.gpu_device_ms),
        ]
    }
}

/// A [`MeasuredUsage`] that has been checked: every gap carries its reason.
///
/// Constructible only through [`Self::new`], which is the whole point — the
/// fields are private so there is no way to hand the write path a row with an
/// unexplained NULL in it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessUsage {
    #[serde(flatten)]
    measured: MeasuredUsage,
    /// Why each unmeasured field is unmeasured, in the same `{field, reason}`
    /// vocabulary `runtime_telemetry.rs` uses for runtime traces. Deliberately
    /// not a second vocabulary: a support bundle that reads one shape is worth
    /// more than one that reads two.
    unavailable: Vec<TraceFieldNote>,
}

impl ProcessUsage {
    /// Fails when any field is `None` and nothing in `unavailable` names it.
    ///
    /// A hard error rather than a warning or a silently-added placeholder note,
    /// because the alternative is the ledger quietly acquiring gaps nobody can
    /// account for — and a gap nobody can account for is the thing a reader will
    /// eventually read as a zero.
    ///
    /// The inverse — a note naming a field that *was* measured — is not refused.
    /// It is stale bookkeeping rather than a claim about a number, and refusing
    /// it would turn a harmless leftover into a blocked terminal write.
    pub fn new(
        measured: MeasuredUsage,
        unavailable: Vec<TraceFieldNote>,
    ) -> ProcessTableResult<Self> {
        for (field, value) in measured.fields() {
            if value.is_none() && !unavailable.iter().any(|note| note.field == field) {
                return Err(ProcessTableError::UsageGapWithoutReason { field });
            }
        }
        Ok(ProcessUsage {
            measured,
            unavailable,
        })
    }

    pub fn measured(&self) -> MeasuredUsage {
        self.measured
    }

    pub fn unavailable(&self) -> &[TraceFieldNote] {
        &self.unavailable
    }

    /// Why `field` was not measured, if it was not.
    pub fn reason_for(&self, field: &str) -> Option<&str> {
        self.unavailable
            .iter()
            .find(|note| note.field == field)
            .map(|note| note.reason.as_str())
    }
}

/// Reasons the close-out records, as consts so the same gap is always explained
/// the same way whichever path closed the row.
const NO_RUN_REASON: &str =
    "this process kind has no ledger run, so there is no token accounting to read";
const NO_RUN_USAGE_REASON: &str = "the run recorded no readable usage event before it closed";
const GPU_NOT_REPORTED_REASON: &str =
    "no runtime in this build reports per-process GPU residency or device time";
const NOT_SAMPLED_REASON: &str = "nothing sampled this process's resource use while it ran";
const REAPED_REASON: &str =
    "this process was reaped after its host went away, so nothing sampled it";
const NO_EGRESS_ACCOUNTING_REASON: &str =
    "no egress was attributed to this process; nothing fed the ledger a byte count";
const NOT_CLOSED_OUT_REASON: &str =
    "this process has not exited, so its resource ledger row is not closed out";
const PREDATES_LEDGER_REASON: &str =
    "this process exited before the resource ledger existed, so nothing was recorded";
const WALL_TIME_NOT_FINAL_REASON: &str =
    "this process has not exited, so its wall time is not final";

/// The exit detail carried by an `exited` row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessExit {
    pub status: ExitStatus,
    /// Native exit code where one exists (daemon children, background shells).
    pub code: Option<i32>,
    /// Signal name where the process was signalled.
    pub signal: Option<String>,
    /// Human-readable reason. For [`ExitStatus::LimitExceeded`] this must name
    /// the limit that fired.
    pub reason: Option<String>,
    /// The structured form of that reason, when a resource controller made the
    /// kill: which limit, what was configured, what was measured, which backend
    /// noticed, and whether it was kernel-held or supervised.
    ///
    /// Beside `reason` rather than instead of it. The prose is what a person
    /// reads and what older surfaces already show; this is what a query filters
    /// on and what a UI formats. Before V21 only the prose existed, so "how much
    /// memory did it actually hold" could be answered only by parsing a sentence
    /// — which the daemon genuinely did, with a marker string.
    #[serde(default)]
    pub breach: Option<crate::resource_control::LimitBreach>,
}

impl ProcessExit {
    pub fn succeeded() -> Self {
        ProcessExit {
            status: ExitStatus::Succeeded,
            code: None,
            signal: None,
            reason: None,
            breach: None,
        }
    }

    /// The exit a resource controller produces. The prose reason is derived from
    /// the breach rather than written separately, so the two can never disagree.
    pub fn limit_exceeded(breach: crate::resource_control::LimitBreach) -> Self {
        ProcessExit {
            status: ExitStatus::LimitExceeded,
            code: None,
            signal: None,
            reason: Some(breach.describe()),
            breach: Some(breach),
        }
    }

    pub fn failed(reason: impl Into<String>) -> Self {
        ProcessExit {
            status: ExitStatus::Failed,
            code: None,
            signal: None,
            reason: Some(reason.into()),
            breach: None,
        }
    }

    pub fn cancelled(reason: impl Into<String>) -> Self {
        ProcessExit {
            status: ExitStatus::Cancelled,
            code: None,
            signal: None,
            reason: Some(reason.into()),
            breach: None,
        }
    }
}

/// One process, whatever surface it came from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessRecord {
    /// Stable, globally unique, and self-describing — see [`new_process_id`].
    pub process_id: String,
    /// The spawning process, when there is one. This means hierarchy, unlike
    /// `daemon_jobs.parent_run_id`, which carries retry lineage and is never
    /// read.
    pub parent_process_id: Option<String>,
    pub kind: ProcessKind,
    /// The surface's own identifier for this unit — a `turnId`, a `job_id`, a
    /// subagent `cancelId`, a workflow `run_id`. Unique per kind so an adopter
    /// can find its record again without storing a second id, which is what
    /// makes adoption idempotent across restarts.
    pub external_id: String,
    pub state: ProcessState,
    /// The ledger run this process projects onto, when it has one. Subagents
    /// and `m4` workflow runs have none today.
    pub run_id: Option<String>,
    /// Owning workspace root. A first-class column here, unlike `RunSpec`'s
    /// `workspace` which is buried in `spec_json` and cannot be queried — so
    /// "what is running in this folder" is now answerable.
    pub workspace: Option<String>,
    /// Owning profile/persona.
    pub profile: Option<String>,
    /// OS process id, where the process owns one.
    pub native_pid: Option<i64>,
    /// The platform's own start-time stamp for that pid, which together with it
    /// names a *process* rather than a slot in the pid space.
    ///
    /// Opaque and host-local by design — `/proc` jiffies, a BSD start timeval, a
    /// Windows creation FILETIME — and only ever compared against
    /// [`crate::process_tree::ProcessIdentity::of`] on the machine that wrote it.
    /// `None` on a row written before V22, and on a host that will not report
    /// one; a reconciler that finds `None` must not signal, because it cannot
    /// prove the pid is still the process this row is about.
    #[serde(default)]
    pub native_start_time: Option<i64>,
    pub limits: ProcessLimits,
    /// The mechanism that actually enforced this process, recorded when it was
    /// attached rather than derived from the host reading the row.
    ///
    /// `None` for a kind that owns no OS process tree, and on a row written
    /// before V23. Never re-derived: the whole point is that a row read on a
    /// different machine, or on the same machine after its delegation changed,
    /// still names what held *this* process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containment: Option<crate::resource_control::Containment>,
    /// The session this process's root *led*, where it led one.
    ///
    /// Captured while the root was alive, for the reason the process group on
    /// `containment.scope` is: both are read off the root's own row, so neither
    /// is discoverable once the root exits — which is precisely when a descendant
    /// that stayed in the session becomes unattributable to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervised_session_id: Option<u32>,
    /// The host boot this row's native identities belong to, on a platform whose
    /// start-time clock restarts with the machine.
    ///
    /// `None` on macOS and Windows, whose start times are absolute and need no
    /// disambiguation — see [`crate::process_tree::boot_marker`]. A reclaim that
    /// finds a *different* marker treats every recorded identity as gone rather
    /// than signalling a pid the new boot may have reissued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_boot_marker: Option<String>,
    /// The most recent measurement of the owned tree, with the peaks it has
    /// reached. `None` where nothing sampled it — never a zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::resource_control::RecordedUsage>,
    /// When that measurement was taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_sampled_at_ms: Option<i64>,
    /// Durable signal intent. Survives a restart, unlike the in-memory
    /// `AbortController`/`CancellationToken` each kind used before, and is
    /// readable by a worker in another process — which is what makes an
    /// out-of-process run signallable at all.
    pub signal_intent: SignalIntent,
    /// Why the most recent signal was asked for, as the caller stated it.
    pub signal_reason: Option<String>,
    pub signal_requested_at_ms: Option<i64>,
    /// Present if and only if `state == Exited`.
    pub exit: Option<ProcessExit>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub exited_at_ms: Option<i64>,
}

impl ProcessRecord {
    pub fn is_live(&self) -> bool {
        !self.state.is_terminal()
    }
}

/// What a caller supplies to admit a process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmitProcess {
    pub kind: ProcessKind,
    pub external_id: String,
    pub parent_process_id: Option<String>,
    pub run_id: Option<String>,
    pub workspace: Option<String>,
    pub profile: Option<String>,
    pub limits: ProcessLimits,
}

impl AdmitProcess {
    pub fn new(kind: ProcessKind, external_id: impl Into<String>) -> Self {
        AdmitProcess {
            kind,
            external_id: external_id.into(),
            parent_process_id: None,
            run_id: None,
            workspace: None,
            profile: None,
            // Seeded from the class, so a row is never accidentally declared
            // unbounded just because its adopter did not think about limits.
            // `with_limits` still overrides, which is how the daemon supplies its
            // truer per-job values.
            limits: kind.default_limits(),
        }
    }

    pub fn with_parent(mut self, parent_process_id: impl Into<String>) -> Self {
        self.parent_process_id = Some(parent_process_id.into());
        self
    }

    pub fn with_run(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }

    pub fn with_workspace(mut self, workspace: Option<String>) -> Self {
        self.workspace = workspace;
        self
    }

    pub fn with_profile(mut self, profile: Option<String>) -> Self {
        self.profile = profile;
        self
    }

    pub fn with_limits(mut self, limits: ProcessLimits) -> Self {
        self.limits = limits;
        self
    }
}

/// A signal asked of a process.
///
/// Deliberately small and OS-shaped. Delivery is per kind and is *not* assumed:
/// see [`ProcessKind::signal_support`], which either implements a signal or
/// refuses it with a reason. A command that appeared to succeed and silently did
/// nothing would be worse than one that says "chat turns cannot be suspended".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessSignal {
    /// Wind down cooperatively at the next safe point, running whatever cleanup
    /// the kind owes. Every kind honours this.
    Stop,
    /// Stop making progress but stay resumable, holding the process's place.
    Suspend,
    /// Undo a [`ProcessSignal::Suspend`].
    Resume,
    /// Terminate now, without waiting for a safe point. Only meaningful where
    /// this app owns an OS process; elsewhere it is refused rather than quietly
    /// downgraded to `Stop`, because a caller asking for `Kill` is asking for a
    /// guarantee `Stop` does not give.
    Kill,
}

impl ProcessSignal {
    pub fn as_str(self) -> &'static str {
        match self {
            ProcessSignal::Stop => "stop",
            ProcessSignal::Suspend => "suspend",
            ProcessSignal::Resume => "resume",
            ProcessSignal::Kill => "kill",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ProcessTableError> {
        Ok(match value {
            "stop" => ProcessSignal::Stop,
            "suspend" => ProcessSignal::Suspend,
            "resume" => ProcessSignal::Resume,
            "kill" => ProcessSignal::Kill,
            other => {
                return Err(ProcessTableError::UnknownSignal {
                    signal: other.to_string(),
                })
            }
        })
    }

    pub const ALL: &'static [ProcessSignal] = &[
        ProcessSignal::Stop,
        ProcessSignal::Suspend,
        ProcessSignal::Resume,
        ProcessSignal::Kill,
    ];
}

/// Whether a kind honours a signal, and if not, why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "reason")]
pub enum SignalSupport {
    /// The kind delivers this signal.
    Honoured,
    /// The kind cannot deliver it. The reason is shown to the caller verbatim —
    /// it is the whole value of refusing rather than no-opping.
    Refused(&'static str),
}

impl SignalSupport {
    pub fn is_honoured(self) -> bool {
        matches!(self, SignalSupport::Honoured)
    }

    pub fn refusal(self) -> Option<&'static str> {
        match self {
            SignalSupport::Honoured => None,
            SignalSupport::Refused(reason) => Some(reason),
        }
    }
}

/// One field of [`ProcessLimits`], as a value, so the enforcement matrix can be
/// iterated and asserted exhaustively rather than described in prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessLimitKind {
    Wall,
    Memory,
    Output,
    ChildProcesses,
    ContextTokens,
}

impl ProcessLimitKind {
    pub const ALL: &'static [ProcessLimitKind] = &[
        ProcessLimitKind::Wall,
        ProcessLimitKind::Memory,
        ProcessLimitKind::Output,
        ProcessLimitKind::ChildProcesses,
        ProcessLimitKind::ContextTokens,
    ];

    /// The `ProcessLimits` field name, so a message names the thing a caller set.
    pub fn as_str(self) -> &'static str {
        match self {
            ProcessLimitKind::Wall => "max_wall_ms",
            ProcessLimitKind::Memory => "max_memory_bytes",
            ProcessLimitKind::Output => "max_output_bytes",
            ProcessLimitKind::ChildProcesses => "max_child_processes",
            ProcessLimitKind::ContextTokens => "max_context_tokens",
        }
    }
}

/// What happens to a positive value in one [`ProcessLimits`] field, for one kind.
///
/// K4's acceptance is that a positive value may be recorded only when its owner
/// enforces it or that specific limit is reported unavailable. Three answers are
/// needed rather than two, because "the owner enforces a bound" and "the owner
/// reads *this field*" are different facts, and collapsing them is what let
/// generic admission record ceilings nobody consults:
///
/// - [`Enforced`](Self::Enforced) — the owner reads this row's field. A caller's
///   override takes effect.
/// - [`OwnerSourced`](Self::OwnerSourced) — the owner enforces a real bound but
///   supplies the number itself, from a recipe, a workflow definition, or its own
///   settings, and writes it onto the row. A caller's override would be silently
///   replaced, so it is refused at admission instead.
/// - [`Unavailable`](Self::Unavailable) — nothing enforces it for this kind. The
///   reason names the missing mechanism, never the word "unsupported", because
///   the caller's next question is always "why not".
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "detail")]
pub enum LimitEnforcement {
    Enforced(&'static str),
    OwnerSourced(&'static str),
    Unavailable(&'static str),
}

impl LimitEnforcement {
    /// Whether a caller-supplied value for this field would actually be obeyed.
    pub fn honours_caller_value(self) -> bool {
        matches!(self, LimitEnforcement::Enforced(_))
    }

    pub fn detail(self) -> &'static str {
        match self {
            LimitEnforcement::Enforced(d)
            | LimitEnforcement::OwnerSourced(d)
            | LimitEnforcement::Unavailable(d) => d,
        }
    }

    pub fn status(self) -> &'static str {
        match self {
            LimitEnforcement::Enforced(_) => "enforced",
            LimitEnforcement::OwnerSourced(_) => "owner-sourced",
            LimitEnforcement::Unavailable(_) => "unavailable",
        }
    }
}

/// A durable request for a signal, recorded on the process record.
///
/// Two independent latches rather than one "requested signal" field: a stop and
/// a suspend are not alternatives. A process can be suspended and then asked to
/// stop, and the stop must win without the suspend intent being lost from the
/// audit trail. `Resume` clears the suspend latch and nothing else, because the
/// alternative turns "stop this" into "keep going" on a race.
///
/// `kill_requested` never appears without `stop_requested`. A kill IS a stop
/// carrying a stronger delivery promise — terminate now, do not wait for a safe
/// point — so a reader that only cares "is this winding down?" checks
/// `stop_requested` and is right for both, while a supervisor deciding *how* to
/// deliver reads `kill_requested` to tell them apart. Before this the two were
/// indistinguishable once written, and only the free-text reason survived.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalIntent {
    pub stop_requested: bool,
    pub suspend_requested: bool,
    pub kill_requested: bool,
}

impl SignalIntent {
    pub fn is_clear(&self) -> bool {
        !self.stop_requested && !self.suspend_requested && !self.kill_requested
    }

    /// How a stop should be delivered, for a caller that can honour the
    /// difference. `false` for a row with no stop pending at all.
    pub fn wants_immediate_termination(&self) -> bool {
        self.kill_requested
    }
}

/// Whether a kind's process is re-run after an unrequested exit, and how often.
///
/// Bounded by construction: there is no "always" variant. An unbounded restart
/// loop is how a crashing process becomes a resource leak that survives every
/// attempt to stop it, and nothing in this system needs one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RestartPolicy {
    /// Never re-run. Either nothing can restart this kind, or something else
    /// already owns its retry — [`ProcessKind::restart_policy`] says which.
    Never,
    /// Re-run after a failure, up to `max_attempts` total attempts, waiting
    /// longer after each one.
    #[serde(rename_all = "camelCase")]
    OnFailure {
        max_attempts: u32,
        base_backoff_ms: u64,
    },
}

impl RestartPolicy {
    /// Whether an exit with `attempt` attempts already spent earns another try.
    ///
    /// `attempt` counts attempts *completed*, so the first failure arrives with
    /// `attempt == 0` and a `max_attempts` of 3 permits two retries.
    pub fn permits_retry(self, attempt: u32) -> bool {
        match self {
            RestartPolicy::Never => false,
            RestartPolicy::OnFailure { max_attempts, .. } => {
                attempt.saturating_add(1) < max_attempts
            }
        }
    }

    /// How long to wait before the next attempt — exponential in the number
    /// already spent, and capped so a late attempt cannot park a job for hours.
    pub fn backoff_ms(self, attempt: u32) -> u64 {
        const MAX_BACKOFF_MS: u64 = 60_000;
        match self {
            RestartPolicy::Never => 0,
            RestartPolicy::OnFailure {
                base_backoff_ms, ..
            } => base_backoff_ms
                .saturating_mul(1_u64 << attempt.min(16))
                .min(MAX_BACKOFF_MS),
        }
    }
}

/// One idempotent statement of "this surface unit exists and is in this state".
///
/// Callers describe the world as they see it; [`ProcessTable::reconcile`] works
/// out whether that means admitting a record, moving an existing one, or doing
/// nothing. This exists because every adopter otherwise hand-rolls the same
/// find-or-admit-then-transition dance — the daemon did, and the frontend
/// client did it differently — which is the shape that produced four
/// incompatible state vocabularies in the first place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessProjection {
    pub kind: ProcessKind,
    pub external_id: String,
    /// The parent named by *its* surface id and kind, since an adopter usually
    /// knows that rather than the parent's process id.
    pub parent: Option<(ProcessKind, String)>,
    pub state: ProcessState,
    /// Required when `state` is [`ProcessState::Exited`], ignored otherwise.
    pub exit: Option<ProcessExit>,
    pub run_id: Option<String>,
    pub workspace: Option<String>,
    pub profile: Option<String>,
    pub native_pid: Option<i64>,
    pub native_start_time: Option<i64>,
    pub limits: ProcessLimits,
    /// What is holding this process, as its own controller reported it.
    ///
    /// Written once, when the workload is attached, and never recomputed: see
    /// [`crate::resource_control::Containment`] for why a host answer derived at
    /// read time is the wrong answer for a row that outlives its host.
    pub containment: Option<crate::resource_control::Containment>,
    /// The most recent measurement of the owned tree.
    ///
    /// Overwritten on every sample rather than appended, because this answers
    /// "what is it holding *now*" — the history question is the resource
    /// ledger's, and duplicating it here would be a second series to disagree
    /// with. Peaks travel inside the sample, so a row that stops being sampled
    /// keeps the highest value anything ever saw.
    pub usage: Option<crate::resource_control::ResourceSample>,
}

impl ProcessProjection {
    pub fn new(kind: ProcessKind, external_id: impl Into<String>, state: ProcessState) -> Self {
        ProcessProjection {
            kind,
            external_id: external_id.into(),
            parent: None,
            state,
            exit: None,
            run_id: None,
            workspace: None,
            profile: None,
            native_pid: None,
            native_start_time: None,
            containment: None,
            usage: None,
            // Same seeding as `AdmitProcess::new`, and it has to be here too:
            // `reconcile` admits through a projection, so a kind whose only
            // adopter projects (every desktop kind) would otherwise never pick up
            // its class's limits at all.
            limits: kind.default_limits(),
        }
    }

    pub fn exited(kind: ProcessKind, external_id: impl Into<String>, exit: ProcessExit) -> Self {
        let mut projection = Self::new(kind, external_id, ProcessState::Exited);
        projection.exit = Some(exit);
        projection
    }

    pub fn with_parent(mut self, kind: ProcessKind, external_id: impl Into<String>) -> Self {
        self.parent = Some((kind, external_id.into()));
        self
    }

    pub fn with_run(mut self, run_id: Option<String>) -> Self {
        self.run_id = run_id;
        self
    }

    pub fn with_workspace(mut self, workspace: Option<String>) -> Self {
        self.workspace = workspace;
        self
    }

    pub fn with_profile(mut self, profile: Option<String>) -> Self {
        self.profile = profile;
        self
    }

    pub fn with_native_pid(mut self, native_pid: Option<i64>) -> Self {
        self.native_pid = native_pid;
        self
    }

    /// The pid *and* the start time that says which process it is.
    ///
    /// Preferred over [`Self::with_native_pid`] wherever the caller holds a
    /// [`crate::process_tree::ProcessIdentity`], which every resource-controlled
    /// spawn does: a row with a bare pid cannot be safely reconciled after a
    /// restart, because nothing can prove the pid was not reused.
    pub fn with_native_identity(
        mut self,
        identity: Option<crate::process_tree::ProcessIdentity>,
    ) -> Self {
        self.native_pid = identity.and_then(|identity| i64::try_from(identity.pid).ok());
        self.native_start_time =
            identity.and_then(|identity| i64::try_from(identity.start_time).ok());
        self
    }

    pub fn with_limits(mut self, limits: ProcessLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Record what actually holds this process, from its own controller.
    pub fn with_containment(
        mut self,
        containment: Option<crate::resource_control::Containment>,
    ) -> Self {
        self.containment = containment;
        self
    }

    /// Record the latest measurement of the owned tree.
    pub fn with_usage(mut self, usage: Option<crate::resource_control::ResourceSample>) -> Self {
        self.usage = usage;
        self
    }
}

/// What [`ProcessTable::reconcile`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileOutcome {
    /// A new record was admitted.
    Admitted,
    /// An existing record moved state.
    Transitioned,
    /// Already in the requested state; only metadata may have been filled in.
    Unchanged,
    /// The record has already exited and the projection described a live state.
    /// Left alone — a late projection must never resurrect a finished process,
    /// and must not be an error either, because an adopter reconciling on a
    /// timer will legitimately arrive after the terminal write.
    AlreadyExited,
}

/// One native process a supervised workload owns, as it was durably recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnedMember {
    pub identity: crate::process_tree::ProcessIdentity,
    pub first_seen_at_ms: i64,
    pub last_seen_at_ms: i64,
}

/// Everything a restart needs in order to find what one workload still owns.
///
/// Sent whole rather than one member at a time, because the three parts are one
/// fact: a set of identities is only reclaimable alongside the supervision
/// metadata that says which host boot they belong to and which session they may
/// still be discoverable through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedProcesses {
    pub kind: ProcessKind,
    pub external_id: String,
    /// Every `(pid, start_time)` this workload has been observed to own.
    pub members: Vec<crate::process_tree::ProcessIdentity>,
    /// The session the root led, where it led one.
    pub session: Option<u32>,
    /// The host boot these identities belong to, where the platform needs one.
    pub boot_marker: Option<String>,
}

/// A sink for [`ProcessProjection`]s, and for the ownership facts a restart
/// depends on.
///
/// A port, not a ledger handle. Services that must not depend on storage —
/// `WorkflowService` keeps its history in a JSON file store and is deliberately
/// database-agnostic — depend on this instead, so their unit tests use a
/// recording fake rather than standing up SQLite, and every caller of theirs
/// (desktop, CLI, daemon-triggered) gets the projection from one place.
///
/// # The two methods keep opposite contracts, deliberately
///
/// [`Self::project`] is bookkeeping: implementations must be fail-soft at their
/// own boundary if the caller cannot tolerate an error, and a missed periodic
/// sample costs a stale number on a panel. [`Self::record_owned`] is
/// **recovery-critical state** — it is the only record that a descendant which
/// has since escaped every discovery primitive was ever this workload's — so a
/// caller that cannot persist it must not go on claiming the workload is
/// restart-recoverable. `ResourceController` reclaims the tree instead.
pub trait ProcessProjector: Send + Sync {
    fn project(&self, projection: &ProcessProjection) -> Result<(), String>;

    /// Durably record the native processes a supervised workload owns.
    ///
    /// Must be an upsert keyed by `(process_id, pid, start_time)`: this is
    /// called on the supervision tick, and a row per sample would grow the table
    /// with the clock rather than with the processes actually observed.
    ///
    /// No default implementation, and that is the point: a projector that cannot
    /// persist ownership has to say so at compile time rather than silently
    /// accept the write and drop it.
    fn record_owned(&self, owned: &OwnedProcesses) -> Result<(), String>;
}

/// A read port mirroring [`ProcessProjector`]'s shape in the opposite
/// direction: "what does the process table currently want from this unit,"
/// for a caller — the workflow executor — that has no SQLite connection and
/// must not depend on storage directly.
///
/// `None` means "no intent recorded," which includes "the row does not exist
/// yet" and "the lookup failed" — never an error the caller should fail its
/// run over. That is the same fail-soft contract every [`ProcessProjector`]
/// implementation already keeps, just for a read instead of a write.
pub trait SignalSource: Send + Sync {
    fn signal_intent(&self, kind: ProcessKind, external_id: &str) -> Option<SignalIntent>;
}

/// A [`SignalSource`] backed by the ledger at a path — the read-side twin of
/// [`LedgerProcessProjector`], for the same reason: path-based rather than
/// Tauri state, because the desktop, the CLI, and the daemon all need to
/// read, and only one of them has an `AppHandle`.
pub struct LedgerSignalSource {
    path: std::path::PathBuf,
    ledger: Mutex<Option<crate::run_ledger::RunLedger>>,
}

impl LedgerSignalSource {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        LedgerSignalSource {
            path: path.into(),
            ledger: Mutex::new(None),
        }
    }
}

impl SignalSource for LedgerSignalSource {
    fn signal_intent(&self, kind: ProcessKind, external_id: &str) -> Option<SignalIntent> {
        let mut slot = self.ledger.lock().ok()?;
        if slot.is_none() {
            *slot = Some(crate::run_ledger::RunLedger::open(&self.path).ok()?);
        }
        let ledger = slot.as_ref()?;
        ledger
            .process_table()
            .find_by_external_id(kind, external_id)
            .ok()?
            .map(|record| record.signal_intent)
    }
}

/// Filter for [`ProcessTable::list`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessFilter {
    /// Only these kinds. Empty means every kind.
    pub kinds: Vec<ProcessKind>,
    /// Only live processes (anything not `exited`).
    pub live_only: bool,
    /// Only children of this process.
    pub parent_process_id: Option<String>,
    /// Only processes owning this workspace root.
    pub workspace: Option<String>,
    /// Hard row cap. `None` uses [`DEFAULT_LIST_LIMIT`].
    pub limit: Option<u32>,
}

/// Bounded by default, following `DaemonStore::managed_run_ids`' precedent:
/// a listing surface must not be able to ask for an unbounded scan.
pub const DEFAULT_LIST_LIMIT: u32 = 500;
/// Ceiling on an explicitly requested limit.
pub const MAX_LIST_LIMIT: u32 = 5_000;

#[derive(Debug)]
pub enum ProcessTableError {
    Sqlite(rusqlite::Error),
    UnknownKind {
        kind: String,
    },
    UnknownState {
        state: String,
    },
    UnknownExitStatus {
        status: String,
    },
    UnknownSignal {
        signal: String,
    },
    /// The kind does not honour this signal. Carries the reason so a caller can
    /// show it rather than guessing.
    SignalRefused {
        process_id: String,
        kind: ProcessKind,
        signal: ProcessSignal,
        reason: &'static str,
    },
    /// A signal was asked of a process that has already exited.
    AlreadyExited {
        process_id: String,
    },
    NotFound {
        process_id: String,
    },
    /// The surface already has a record under this `(kind, external_id)`.
    DuplicateExternalId {
        kind: ProcessKind,
        external_id: String,
        existing_process_id: String,
    },
    /// Refused rather than applied — see this module's invariant 1.
    IllegalTransition {
        process_id: String,
        from: ProcessState,
        to: ProcessState,
    },
    /// `exited` without an exit status, or an exit status without `exited`.
    TerminalMismatch {
        process_id: String,
    },
    /// A stored breach names a limit and is missing one of the values that make
    /// it mean anything.
    ///
    /// The schema constrains the five columns to arrive as a group, so this can
    /// only be reached by a row some other writer produced. It is an error rather
    /// than a default because the alternative — reading a missing `configured` as
    /// zero — manufactures "a budget of 0 bytes was exceeded", which is a
    /// confident sentence about a number nobody ever set.
    PartialBreach {
        limit: String,
        missing: &'static str,
    },
    /// A parent id that names no row. Refused so the tree can never be broken.
    UnknownParent {
        parent_process_id: String,
    },
    /// A process cannot be its own ancestor.
    ParentCycle {
        process_id: String,
    },
    InvalidField {
        field: &'static str,
        reason: String,
    },
    /// A resource ledger field was left unmeasured with nothing recorded about
    /// why — the one thing the ledger must never contain, because a reader will
    /// eventually treat an unexplained gap as a zero.
    UsageGapWithoutReason {
        field: &'static str,
    },
}

impl fmt::Display for ProcessTableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProcessTableError::Sqlite(error) => write!(f, "process table storage error: {error}"),
            ProcessTableError::UnknownKind { kind } => write!(f, "unknown process kind \"{kind}\""),
            ProcessTableError::UnknownState { state } => {
                write!(f, "unknown process state \"{state}\"")
            }
            ProcessTableError::UnknownExitStatus { status } => {
                write!(f, "unknown process exit status \"{status}\"")
            }
            ProcessTableError::UnknownSignal { signal } => {
                write!(f, "unknown process signal \"{signal}\"")
            }
            ProcessTableError::PartialBreach { limit, missing } => write!(
                f,
                "the stored breach names {limit} but has no {missing}, so what it was measured \
                 against is unknown rather than zero"
            ),
            ProcessTableError::SignalRefused {
                process_id,
                kind,
                signal,
                reason,
            } => write!(
                f,
                "{} {process_id} does not honour {}: {reason}",
                kind.as_str(),
                signal.as_str()
            ),
            ProcessTableError::AlreadyExited { process_id } => write!(
                f,
                "process {process_id} has already exited; there is nothing to signal"
            ),
            ProcessTableError::NotFound { process_id } => {
                write!(f, "no process \"{process_id}\"")
            }
            ProcessTableError::DuplicateExternalId {
                kind,
                external_id,
                existing_process_id,
            } => write!(
                f,
                "{} \"{external_id}\" is already admitted as {existing_process_id}",
                kind.as_str()
            ),
            ProcessTableError::IllegalTransition {
                process_id,
                from,
                to,
            } => write!(
                f,
                "process {process_id} cannot move from {} to {}",
                from.as_str(),
                to.as_str()
            ),
            ProcessTableError::TerminalMismatch { process_id } => write!(
                f,
                "process {process_id} must carry an exit status if and only if it has exited"
            ),
            ProcessTableError::UnknownParent { parent_process_id } => {
                write!(f, "no parent process \"{parent_process_id}\"")
            }
            ProcessTableError::ParentCycle { process_id } => {
                write!(f, "process {process_id} would become its own ancestor")
            }
            ProcessTableError::InvalidField { field, reason } => {
                write!(f, "invalid {field}: {reason}")
            }
            ProcessTableError::UsageGapWithoutReason { field } => write!(
                f,
                "usage field \"{field}\" is unmeasured with no reason recorded; \
                 an unmeasured field must say why, never default to zero"
            ),
        }
    }
}

impl std::error::Error for ProcessTableError {}

impl From<rusqlite::Error> for ProcessTableError {
    fn from(error: rusqlite::Error) -> Self {
        ProcessTableError::Sqlite(error)
    }
}

pub type ProcessTableResult<T> = Result<T, ProcessTableError>;

/// Mints a process id: `p-<kind tag>-<uuid>`.
///
/// One namespace for every surface, replacing seven schemes that shared no
/// convention and guaranteed uniqueness only within their own subsystem (one of
/// them, the subagent store key, was a provider-supplied `ToolCall.id` that
/// could collide with `call_0`).
pub fn new_process_id(kind: ProcessKind) -> String {
    format!("p-{}-{}", kind.tag(), uuid::Uuid::new_v4().simple())
}

/// Companion store over the shared ledger connection.
///
/// Borrows rather than owns, matching `profile_store.rs` and
/// `approval_chains.rs`: the ledger owns the database, and a companion store
/// gets a narrow transactional view of its own tables.
pub struct ProcessTable<'a> {
    connection: &'a Connection,
}

impl<'a> ProcessTable<'a> {
    pub fn new(connection: &'a Connection) -> Self {
        ProcessTable { connection }
    }

    /// Admit a process. Idempotent by `(kind, external_id)`: a second admit for
    /// the same surface identifier is refused with the id already assigned, so
    /// an adopter that re-runs after a restart cannot silently fork its record.
    pub fn admit(&self, request: &AdmitProcess, now_ms: i64) -> ProcessTableResult<ProcessRecord> {
        if request.external_id.trim().is_empty() {
            return Err(ProcessTableError::InvalidField {
                field: "external_id",
                reason: "must not be empty".to_string(),
            });
        }
        if now_ms <= 0 {
            return Err(ProcessTableError::InvalidField {
                field: "now_ms",
                reason: "must be a positive unix millisecond timestamp".to_string(),
            });
        }

        if let Some(existing) = self.find_by_external_id(request.kind, &request.external_id)? {
            return Err(ProcessTableError::DuplicateExternalId {
                kind: request.kind,
                external_id: request.external_id.clone(),
                existing_process_id: existing.process_id,
            });
        }

        if let Some(parent) = request.parent_process_id.as_deref() {
            if self.get(parent)?.is_none() {
                return Err(ProcessTableError::UnknownParent {
                    parent_process_id: parent.to_string(),
                });
            }
        }

        let process_id = new_process_id(request.kind);
        self.connection.execute(
            "INSERT INTO agent_processes (
                process_id, parent_process_id, kind, external_id, state, run_id,
                workspace, profile, native_pid,
                max_wall_ms, max_memory_bytes, max_output_bytes, max_child_processes,
                max_context_tokens,
                exit_status, exit_code, exit_signal, exit_reason,
                created_at_ms, updated_at_ms, started_at_ms, exited_at_ms
             ) VALUES (
                ?1, ?2, ?3, ?4, 'admitted', ?5,
                ?6, ?7, NULL,
                ?8, ?9, ?10, ?11,
                ?13,
                NULL, NULL, NULL, NULL,
                ?12, ?12, NULL, NULL
             )",
            params![
                process_id,
                request.parent_process_id,
                request.kind.as_str(),
                request.external_id,
                request.run_id,
                request.workspace,
                request.profile,
                request.limits.max_wall_ms.map(|v| v as i64),
                request.limits.max_memory_bytes.map(|v| v as i64),
                request.limits.max_output_bytes.map(|v| v as i64),
                request.limits.max_child_processes.map(|v| v as i64),
                now_ms,
                request.limits.max_context_tokens.map(|v| v as i64),
            ],
        )?;

        self.get(&process_id)?
            .ok_or_else(|| ProcessTableError::NotFound {
                process_id: process_id.clone(),
            })
    }

    /// Move a process to `next`. Refuses an illegal transition rather than
    /// applying it, and requires exit detail exactly when moving to `Exited`.
    ///
    /// **Reaching `Exited` also closes out the resource ledger row.** This is the
    /// only `UPDATE` in the codebase that writes `state`, so it is the only place
    /// a close-out can be guaranteed rather than hoped for: the reapers
    /// ([`Self::reap_missing`], [`Self::reap_dead_hosts`]) and [`Self::reconcile`]
    /// all route through here, which is how a process nobody was watching still
    /// gets a row with honest reasons instead of no row at all. State and usage go
    /// out in one statement so `agent_processes_close_out_states_its_gaps` can
    /// enforce it in SQL too.
    pub fn transition(
        &self,
        process_id: &str,
        next: ProcessState,
        exit: Option<ProcessExit>,
        now_ms: i64,
    ) -> ProcessTableResult<ProcessRecord> {
        let current = self
            .get(process_id)?
            .ok_or_else(|| ProcessTableError::NotFound {
                process_id: process_id.to_string(),
            })?;

        if !current.state.can_transition_to(next) {
            return Err(ProcessTableError::IllegalTransition {
                process_id: process_id.to_string(),
                from: current.state,
                to: next,
            });
        }
        if (next == ProcessState::Exited) != exit.is_some() {
            return Err(ProcessTableError::TerminalMismatch {
                process_id: process_id.to_string(),
            });
        }

        // A budget kill and a user pressing Stop tear a turn down through the
        // identical path — one cancellation, one `cancelled` exit — so without
        // this the ledger cannot tell "the system worked" from "someone changed
        // their mind". Upgraded here rather than at each loop's own `finally`
        // because this is the one place every host passes through: the four
        // WebView loops, the daemon, and `monkey processes` alike.
        let exit = upgrade_a_budget_kill(exit, current.signal_reason.as_deref());

        // First entry into `running` is what `started_at_ms` records; a
        // resume from `suspended` must not overwrite it.
        let started_at_ms = match (current.started_at_ms, next) {
            (None, ProcessState::Running) => Some(now_ms),
            (existing, _) => existing,
        };
        let exited_at_ms = if next == ProcessState::Exited {
            Some(now_ms)
        } else {
            current.exited_at_ms
        };

        // Only a terminal move closes the ledger; every other transition binds
        // NULLs below, and the `COALESCE`s keep whatever was accumulated while
        // the process ran.
        let usage = match next {
            ProcessState::Exited => Some(self.close_out_usage(&current, exit.as_ref())?),
            _ => None,
        };
        let unavailable_json = usage
            .as_ref()
            .map(|usage| {
                serde_json::to_string(usage.unavailable()).map_err(|error| {
                    ProcessTableError::InvalidField {
                        field: "usage_unavailable_json",
                        reason: error.to_string(),
                    }
                })
            })
            .transpose()?;
        let measured = usage
            .as_ref()
            .map(ProcessUsage::measured)
            .unwrap_or_default();

        let breach = exit.as_ref().and_then(|exit| exit.breach.clone());
        self.connection.execute(
            "UPDATE agent_processes
                SET state = ?2,
                    exit_status = ?3,
                    exit_code = ?4,
                    exit_signal = ?5,
                    exit_reason = ?6,
                    updated_at_ms = ?7,
                    started_at_ms = ?8,
                    exited_at_ms = ?9,
                    cpu_time_ms = COALESCE(?10, cpu_time_ms),
                    peak_rss_bytes = COALESCE(?11, peak_rss_bytes),
                    bytes_read = COALESCE(?12, bytes_read),
                    bytes_written = COALESCE(?13, bytes_written),
                    bytes_egressed = COALESCE(?14, bytes_egressed),
                    tokens_in = COALESCE(?15, tokens_in),
                    tokens_out = COALESCE(?16, tokens_out),
                    gpu_resident_bytes = COALESCE(?17, gpu_resident_bytes),
                    gpu_device_ms = COALESCE(?18, gpu_device_ms),
                    usage_unavailable_json = COALESCE(?19, usage_unavailable_json),
                    limit_kind = ?20,
                    limit_configured = ?21,
                    limit_observed = ?22,
                    limit_backend = ?23,
                    limit_level = ?24,
                    limit_observed_at_ms = ?25,
                    limit_evidence = ?26
              WHERE process_id = ?1",
            params![
                process_id,
                next.as_str(),
                exit.as_ref().map(|value| value.status.as_str()),
                exit.as_ref().and_then(|value| value.code),
                exit.as_ref().and_then(|value| value.signal.clone()),
                exit.as_ref().and_then(|value| value.reason.clone()),
                now_ms,
                started_at_ms,
                exited_at_ms,
                to_sql_u64(measured.cpu_time_ms),
                to_sql_u64(measured.peak_rss_bytes),
                to_sql_u64(measured.bytes_read),
                to_sql_u64(measured.bytes_written),
                to_sql_u64(measured.bytes_egressed),
                to_sql_u64(measured.tokens_in),
                to_sql_u64(measured.tokens_out),
                to_sql_u64(measured.gpu_resident_bytes),
                to_sql_u64(measured.gpu_device_ms),
                unavailable_json,
                // The typed breach, which is why V21 exists: a limit kill used to
                // survive only as prose in `exit_reason` and, for the daemon, as a
                // marker encoded into `last_error` and parsed back out.
                breach.as_ref().map(|breach| breach.limit.clone()),
                breach
                    .as_ref()
                    .map(|breach| to_sql_u64(Some(breach.configured))),
                breach
                    .as_ref()
                    .map(|breach| to_sql_u64(Some(breach.observed))),
                breach.as_ref().map(|breach| breach.backend.clone()),
                breach.as_ref().map(|breach| breach.level.clone()),
                breach.as_ref().map(|breach| breach.observed_at_ms),
                breach.as_ref().and_then(|breach| breach.evidence.clone()),
            ],
        )?;

        self.get(process_id)?
            .ok_or_else(|| ProcessTableError::NotFound {
                process_id: process_id.to_string(),
            })
    }

    /// The ledger row for a process about to become `exited`.
    ///
    /// Assembled from three sources, none of which is allowed to guess:
    /// whatever [`Self::accumulate_usage`] managed to sample while the process
    /// lived, the run's own token accounting where the process has a run, and an
    /// explicit reason for everything else. A reaped row gets the reaper's reason
    /// rather than zeros — which is the case that matters most, because a process
    /// whose host crashed is exactly the one nobody sampled.
    fn close_out_usage(
        &self,
        record: &ProcessRecord,
        exit: Option<&ProcessExit>,
    ) -> ProcessTableResult<ProcessUsage> {
        let mut measured = self.stored_usage(&record.process_id)?;
        let mut unavailable: Vec<TraceFieldNote> = Vec::new();
        let mut note = |field: &str, reason: &str| {
            unavailable.push(TraceFieldNote {
                field: field.to_string(),
                reason: reason.to_string(),
            });
        };

        if measured.tokens_in.is_none() && measured.tokens_out.is_none() {
            match record.run_id.as_deref() {
                // The snapshot is cumulative and the newest one wins, matching
                // every other consumer of these events (`durable_run.rs`,
                // `runCapsule.ts`).
                Some(run_id) => match self.latest_run_usage(run_id) {
                    Some(usage) => {
                        measured.tokens_in = Some(usage.input_tokens);
                        measured.tokens_out = Some(usage.output_tokens);
                    }
                    None => {
                        note(FIELD_TOKENS_IN, NO_RUN_USAGE_REASON);
                        note(FIELD_TOKENS_OUT, NO_RUN_USAGE_REASON);
                    }
                },
                // Structural, not incidental: `agent_processes.run_id` is NULL
                // for `subagent` and the `m4` workflow kinds, so there is no
                // event stream to read. A zero here would claim those kinds spend
                // no tokens, which is the opposite of true.
                None => {
                    note(FIELD_TOKENS_IN, NO_RUN_REASON);
                    note(FIELD_TOKENS_OUT, NO_RUN_REASON);
                }
            }
        }

        note(FIELD_GPU_RESIDENT_BYTES, GPU_NOT_REPORTED_REASON);
        note(FIELD_GPU_DEVICE_MS, GPU_NOT_REPORTED_REASON);

        // A reaped process was never sampled *and nothing could have sampled it*,
        // which is a different fact from "sampling was available and nobody did
        // it". Both are honest; only one is actionable.
        let sampling_reason = match exit.map(|exit| exit.status) {
            Some(ExitStatus::Lost) => REAPED_REASON,
            _ => NOT_SAMPLED_REASON,
        };
        for (field, value) in [
            (FIELD_CPU_TIME_MS, measured.cpu_time_ms),
            (FIELD_PEAK_RSS_BYTES, measured.peak_rss_bytes),
            (FIELD_BYTES_READ, measured.bytes_read),
            (FIELD_BYTES_WRITTEN, measured.bytes_written),
        ] {
            if value.is_none() {
                note(field, sampling_reason);
            }
        }
        if measured.bytes_egressed.is_none() {
            note(FIELD_BYTES_EGRESSED, NO_EGRESS_ACCOUNTING_REASON);
        }

        ProcessUsage::new(measured, unavailable)
    }

    /// The newest cumulative usage snapshot on a run, or `None`.
    ///
    /// Reads the event stream because `UsageSnapshot` is stored inside
    /// `run_events.envelope_json` and is not queryable by SQL.
    ///
    /// **Every failure is `None`, deliberately.** This runs inside a terminal
    /// write, and a missing run, an unreadable blob or a SQLite hiccup must not be
    /// able to prevent a process being recorded as exited — a row stuck `running`
    /// forever is a worse outcome than a token count nobody could read, and the
    /// caller records the latter as an explicit unavailability either way.
    fn latest_run_usage(&self, run_id: &str) -> Option<crate::run_protocol::UsageSnapshot> {
        use crate::run_protocol::{RunEvent, RunEventEnvelope};

        let bytes: Vec<u8> = self
            .connection
            .query_row(
                "SELECT envelope_json FROM run_events
                  WHERE run_id = ?1 AND event_type IN ('usage_recorded', 'completed')
                  ORDER BY sequence DESC LIMIT 1",
                params![run_id],
                |row| row.get(0),
            )
            .optional()
            .ok()??;
        let envelope: RunEventEnvelope = serde_json::from_slice(&bytes).ok()?;
        match envelope.event {
            RunEvent::UsageRecorded { usage } | RunEvent::Completed { usage, .. } => Some(usage),
            _ => None,
        }
    }

    fn stored_usage(&self, process_id: &str) -> ProcessTableResult<MeasuredUsage> {
        Ok(self
            .connection
            .query_row(
                &format!(
                    "SELECT {SELECT_USAGE_COLUMNS} FROM agent_processes WHERE process_id = ?1"
                ),
                params![process_id],
                |row| map_measured_usage(row, 0),
            )
            .optional()?
            .unwrap_or_default())
    }

    /// Fold one live sample into the row, keeping the highest reading per field.
    ///
    /// The accumulate half of the ledger's write path, and it has to exist
    /// separately from close-out for a physical reason: peak resident size is
    /// unreadable once a pid is gone, so the peak has to be captured while the
    /// process is alive. A caller polls into a
    /// [`crate::process_usage::ProcessUsageAccumulator`] and flushes through here
    /// as often as it likes.
    ///
    /// The maximum is taken in SQL rather than read-modify-written in Rust, so two
    /// samplers — the desktop and a `monkey` process watching the same row —
    /// cannot lose each other's reading to a lost update. A `NULL` in the sample
    /// leaves the stored value alone; it never overwrites a measurement with
    /// "unknown".
    ///
    /// `sample.bytes_egressed` is folded by maximum too, because an accumulator's
    /// egress figure is a running total. A caller holding an *increment* instead
    /// wants [`Self::add_egress_bytes`] — use one or the other for a given
    /// process, not both, or the two conventions double-count.
    pub fn accumulate_usage(
        &self,
        process_id: &str,
        sample: &ProcessUsageSample,
        now_ms: i64,
    ) -> ProcessTableResult<()> {
        let updated = self.connection.execute(
            "UPDATE agent_processes
                SET cpu_time_ms = COALESCE(MAX(cpu_time_ms, ?2), cpu_time_ms, ?2),
                    peak_rss_bytes = COALESCE(MAX(peak_rss_bytes, ?3), peak_rss_bytes, ?3),
                    bytes_read = COALESCE(MAX(bytes_read, ?4), bytes_read, ?4),
                    bytes_written = COALESCE(MAX(bytes_written, ?5), bytes_written, ?5),
                    bytes_egressed = COALESCE(MAX(bytes_egressed, ?6), bytes_egressed, ?6),
                    updated_at_ms = ?7
              WHERE process_id = ?1",
            params![
                process_id,
                to_sql_u64(sample.cpu_time_ms),
                to_sql_u64(sample.peak_rss_bytes),
                to_sql_u64(sample.bytes_read),
                to_sql_u64(sample.bytes_written),
                to_sql_u64(sample.bytes_egressed),
                now_ms,
            ],
        )?;
        if updated == 0 {
            return Err(ProcessTableError::NotFound {
                process_id: process_id.to_string(),
            });
        }
        Ok(())
    }

    /// Add network bytes attributed to this process.
    ///
    /// Additive, unlike [`Self::accumulate_usage`]'s maxima, because egress
    /// arrives as increments from whoever counted them rather than as a running
    /// total. The first call is what turns the column from "not measured" into a
    /// measurement — a process nobody reports egress for keeps its NULL and its
    /// stated reason, rather than being credited with zero bytes.
    pub fn add_egress_bytes(
        &self,
        process_id: &str,
        bytes: u64,
        now_ms: i64,
    ) -> ProcessTableResult<()> {
        let updated = self.connection.execute(
            "UPDATE agent_processes
                SET bytes_egressed = COALESCE(bytes_egressed, 0) + ?2,
                    updated_at_ms = ?3
              WHERE process_id = ?1",
            params![process_id, bytes as i64, now_ms],
        )?;
        if updated == 0 {
            return Err(ProcessTableError::NotFound {
                process_id: process_id.to_string(),
            });
        }
        Ok(())
    }

    /// Add one flush of measured prompt-cache reuse to this process (K11).
    ///
    /// Additive for [`Self::add_egress_bytes`]'s reason — a drain carries the
    /// tokens since the last flush — and the first call is likewise what turns
    /// both columns from "no runtime under this process reported reuse" into a
    /// measurement. Both are written together because a hit rate needs both terms:
    /// a reused count with no denominator is not a rate.
    pub fn add_context_reuse(
        &self,
        process_id: &str,
        reuse: crate::run_scope::ContextReuse,
        now_ms: i64,
    ) -> ProcessTableResult<()> {
        let updated = self.connection.execute(
            "UPDATE agent_processes
                SET context_tokens_reused =
                        COALESCE(context_tokens_reused, 0) + ?2,
                    context_tokens_evaluated =
                        COALESCE(context_tokens_evaluated, 0) + ?3,
                    updated_at_ms = ?4
              WHERE process_id = ?1",
            params![
                process_id,
                reuse.reused_tokens as i64,
                reuse.evaluated_tokens as i64,
                now_ms
            ],
        )?;
        if updated == 0 {
            return Err(ProcessTableError::NotFound {
                process_id: process_id.to_string(),
            });
        }
        Ok(())
    }

    /// Record who this process's allowed egress went to.
    ///
    /// Additive in the same sense as [`Self::add_egress_bytes`] and for the same
    /// reason: a drain carries the requests since the last flush, not a running
    /// total, so a destination already named has its count raised rather than
    /// replaced.
    ///
    /// # Why one transaction
    ///
    /// A drain is consumed — [`crate::run_scope::ProcessScope::take_destinations`]
    /// empties the log — so a flush that half-succeeded would leave the caller
    /// unable to retry: putting the drain back would double-count the rows that
    /// did land. All-or-nothing makes the retry safe, which is what
    /// [`crate::run_scope::ProcessScope::return_destinations`] relies on.
    pub fn add_egress_destinations(
        &self,
        process_id: &str,
        drain: &crate::run_scope::DestinationDrain,
        now_ms: i64,
    ) -> ProcessTableResult<()> {
        if drain.is_empty() {
            return Ok(());
        }
        let transaction = self.connection.unchecked_transaction()?;
        // Checked first and inside the transaction: the foreign key would catch an
        // unknown process anyway, but as a constraint violation rather than as the
        // `NotFound` every other writer here reports.
        let updated = transaction.execute(
            "UPDATE agent_processes
                SET egress_destinations_dropped =
                        COALESCE(egress_destinations_dropped, 0) + ?2,
                    updated_at_ms = ?3
              WHERE process_id = ?1",
            params![process_id, drain.overflowed as i64, now_ms],
        )?;
        if updated == 0 {
            return Err(ProcessTableError::NotFound {
                process_id: process_id.to_string(),
            });
        }
        for (destination, requests) in &drain.seen {
            transaction.execute(
                UPSERT_DESTINATION_SQL,
                params![
                    process_id,
                    None::<&str>,
                    destination.scheme,
                    destination.host,
                    i64::from(destination.port),
                    *requests as i64,
                    now_ms,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Where each of these processes' allowed egress went.
    ///
    /// Takes a set rather than one id because the caller that wants this — the
    /// resource ledger surface — is showing a page of processes at once, and a
    /// query per row is the shape that makes a list view slow for no reason.
    ///
    /// A process with no recorded destinations is absent from the map rather
    /// than present with an empty list: "nothing was recorded" and "this process
    /// reached nowhere" are the same fact here, and inventing an entry for every
    /// id asked about would make the map's size say nothing.
    ///
    /// Destinations are ordered by traffic so the noisiest is first, with host,
    /// port and scheme as tiebreaks so the order is total rather than merely
    /// mostly-determined.
    pub fn egress_destinations_for(
        &self,
        process_ids: &[String],
    ) -> ProcessTableResult<BTreeMap<String, ProcessEgressDestinations>> {
        let mut found: BTreeMap<String, ProcessEgressDestinations> = BTreeMap::new();
        if process_ids.is_empty() {
            return Ok(found);
        }
        let placeholders = vec!["?"; process_ids.len()].join(", ");
        let bindings: Vec<&dyn rusqlite::ToSql> = process_ids
            .iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();

        let mut statement = self.connection.prepare(&format!(
            "SELECT process_id, scheme, host, port, requests, first_seen_ms, last_seen_ms
               FROM egress_destinations
              WHERE process_id IN ({placeholders})
              ORDER BY requests DESC, host ASC, port ASC, scheme ASC"
        ))?;
        let rows = statement
            .query_map(bindings.as_slice(), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    EgressDestinationRow {
                        scheme: row.get(1)?,
                        host: row.get(2)?,
                        // The `CHECK` keeps this in range, so a value that is not
                        // is a corrupted database rather than a case to model.
                        port: u16::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                        requests: row.get::<_, i64>(4)? as u64,
                        first_seen_ms: row.get(5)?,
                        last_seen_ms: row.get(6)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (process_id, destination) in rows {
            found
                .entry(process_id)
                .or_default()
                .destinations
                .push(destination);
        }

        // The dropped count is a second query rather than a join: it lives on
        // `agent_processes`, and a process can have dropped destinations while
        // naming none at all — a join would lose exactly that row.
        let mut statement = self.connection.prepare(&format!(
            "SELECT process_id, egress_destinations_dropped
               FROM agent_processes
              WHERE process_id IN ({placeholders})
                AND egress_destinations_dropped IS NOT NULL
                AND egress_destinations_dropped > 0"
        ))?;
        let dropped = statement
            .query_map(bindings.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (process_id, count) in dropped {
            found.entry(process_id).or_default().dropped = count;
        }
        Ok(found)
    }

    /// The measured prompt-cache reuse of each of `process_ids` that has any.
    ///
    /// A process absent from the map reported no reuse figure at all — an Ollama
    /// or MLX turn, or a process that ran no completion. That is why absence is
    /// the answer rather than a zeroed entry: a zero would say the runtime
    /// measured no reuse, and nothing measured anything.
    ///
    /// Read separately rather than folded into [`MeasuredUsage`] for the same
    /// reason `egress_destinations_dropped` is: `MeasuredUsage`'s every-gap-has-a-
    /// reason invariant is checked on write by every writer of a row, and a
    /// measurement only two of three runtimes can produce would make every
    /// close-out carry a note about a runtime it never used.
    pub fn context_reuse_for(
        &self,
        process_ids: &[String],
    ) -> ProcessTableResult<BTreeMap<String, crate::run_scope::ContextReuse>> {
        let mut found = BTreeMap::new();
        if process_ids.is_empty() {
            return Ok(found);
        }
        let placeholders = vec!["?"; process_ids.len()].join(", ");
        let bindings: Vec<&dyn rusqlite::ToSql> = process_ids
            .iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();
        let mut statement = self.connection.prepare(&format!(
            "SELECT process_id, context_tokens_reused, context_tokens_evaluated
               FROM agent_processes
              WHERE process_id IN ({placeholders})
                AND context_tokens_reused IS NOT NULL
                AND context_tokens_evaluated IS NOT NULL"
        ))?;
        let rows = statement
            .query_map(bindings.as_slice(), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    crate::run_scope::ContextReuse {
                        reused_tokens: row.get::<_, i64>(1)?.max(0) as u64,
                        evaluated_tokens: row.get::<_, i64>(2)?.max(0) as u64,
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        found.extend(rows);
        Ok(found)
    }

    /// Ask a process for a signal, durably.
    ///
    /// Records intent; does not deliver it. Delivery is the owning kind's job,
    /// which reads the latch at its own safe point — that separation is what
    /// makes a signal work across a process boundary and across a restart.
    ///
    /// Refuses rather than no-ops in two cases the caller genuinely needs
    /// distinguished: a kind that cannot honour the signal
    /// ([`ProcessTableError::SignalRefused`], carrying the reason), and a process
    /// that has already exited ([`ProcessTableError::AlreadyExited`]).
    ///
    /// `Stop` and `Suspend` set independent latches, so asking a suspended
    /// process to stop does not erase the record that it was suspended. `Resume`
    /// clears only the suspend latch — it must never cancel a pending stop,
    /// which would turn "stop this" into "keep going" on a race.
    pub fn signal(
        &self,
        process_id: &str,
        signal: ProcessSignal,
        reason: Option<&str>,
        now_ms: i64,
    ) -> ProcessTableResult<ProcessRecord> {
        let record = self
            .get(process_id)?
            .ok_or_else(|| ProcessTableError::NotFound {
                process_id: process_id.to_string(),
            })?;

        if record.state.is_terminal() {
            return Err(ProcessTableError::AlreadyExited {
                process_id: process_id.to_string(),
            });
        }
        if let Some(refusal) = record.kind.signal_support(signal).refusal() {
            return Err(ProcessTableError::SignalRefused {
                process_id: process_id.to_string(),
                kind: record.kind,
                signal,
                reason: refusal,
            });
        }

        // Each arm states all three latches, so the interaction between them is
        // readable in one place rather than inferred from what is missing.
        //
        // A `Kill` sets `stop` as well: killing IS stopping, with a stronger
        // promise about how. Every reader that only asks "is this winding down?"
        // therefore stays correct without knowing the distinction exists.
        //
        // Neither `Stop` nor `Kill` clears a pending suspend, and `Resume`
        // clears neither of them. The latches are independent because the
        // alternatives are both wrong: erasing the suspend would lose why the
        // process stopped making progress, and letting a resume clear a stop
        // would turn "stop this" into "keep going" on a race. A kill is never
        // downgraded back to a plain stop either — a caller who escalated does
        // not get un-escalated by a later, weaker request.
        let (stop, suspend, kill) = match signal {
            ProcessSignal::Stop => (
                true,
                record.signal_intent.suspend_requested,
                record.signal_intent.kill_requested,
            ),
            ProcessSignal::Kill => (true, record.signal_intent.suspend_requested, true),
            ProcessSignal::Suspend => (
                record.signal_intent.stop_requested,
                true,
                record.signal_intent.kill_requested,
            ),
            ProcessSignal::Resume => (
                record.signal_intent.stop_requested,
                false,
                record.signal_intent.kill_requested,
            ),
        };

        self.connection.execute(
            "UPDATE agent_processes
                SET stop_requested = ?2,
                    suspend_requested = ?3,
                    signal_reason = ?4,
                    signal_requested_at_ms = ?5,
                    updated_at_ms = ?5,
                    kill_requested = ?6
              WHERE process_id = ?1",
            params![
                process_id,
                i64::from(stop),
                i64::from(suspend),
                reason,
                now_ms,
                i64::from(kill),
            ],
        )?;

        self.get(process_id)?
            .ok_or_else(|| ProcessTableError::NotFound {
                process_id: process_id.to_string(),
            })
    }

    /// Live processes with a signal still waiting to be delivered.
    ///
    /// What a supervisor reads after a restart, and what a worker in another
    /// process checks at its safe point.
    ///
    /// The predicate is in SQL rather than a filter over [`Self::list`]: a
    /// post-filter would first truncate at [`MAX_LIST_LIMIT`] and could therefore
    /// *hide a latched stop* behind 5,000 quiet rows, which is the one failure
    /// this function must not have. It also lets
    /// `agent_processes_pending_signal_idx` do its job.
    ///
    /// **State is the acknowledgement.** A `suspend_requested` row whose state is
    /// already `suspended` has had its signal delivered, so it is not pending;
    /// otherwise a suspended process would be re-delivered on every read forever.
    /// This mirrors the convention the daemon already uses
    /// (`pause_requested && state != Paused`), and needs no extra column. A
    /// `stop_requested` row self-clears by leaving the live set when it exits.
    pub fn pending_signals(&self, kinds: &[ProcessKind]) -> ProcessTableResult<Vec<ProcessRecord>> {
        let mut sql = format!(
            "{SELECT_COLUMNS} WHERE state <> 'exited' AND (\
                 stop_requested = 1 \
                 OR (suspend_requested = 1 AND state <> 'suspended') \
                 OR (suspend_requested = 0 AND state = 'suspended')\
             )"
        );
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if !kinds.is_empty() {
            let placeholders = kinds.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            sql.push_str(&format!(" AND kind IN ({placeholders})"));
            for kind in kinds {
                values.push(Box::new(kind.as_str().to_string()));
            }
        }
        sql.push_str(" ORDER BY signal_requested_at_ms ASC, process_id ASC");

        let mut statement = self.connection.prepare(&sql)?;
        let bindings: Vec<&dyn rusqlite::ToSql> =
            values.iter().map(|value| value.as_ref()).collect();
        let rows = statement.query_map(bindings.as_slice(), map_row)?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row??);
        }
        Ok(records)
    }

    /// Record the OS process id once the kind has one. Separate from
    /// [`Self::transition`] because the daemon learns the pid after spawning,
    /// which is after it moves the job to `running`.
    pub fn set_native_pid(
        &self,
        process_id: &str,
        native_pid: Option<i64>,
        now_ms: i64,
    ) -> ProcessTableResult<()> {
        self.set_native_identity(process_id, native_pid, None, now_ms)
    }

    /// [`Self::set_native_pid`] with the start time that makes the pid an
    /// identity.
    ///
    /// The start time is written only when supplied, so a caller that has a pid
    /// and nothing else does not erase one a better-informed caller recorded —
    /// the same rule `reconcile` already applies to the pid itself.
    pub fn set_native_identity(
        &self,
        process_id: &str,
        native_pid: Option<i64>,
        native_start_time: Option<i64>,
        now_ms: i64,
    ) -> ProcessTableResult<()> {
        let updated = self.connection.execute(
            "UPDATE agent_processes
                SET native_pid = ?2,
                    native_start_time = COALESCE(?3, native_start_time),
                    updated_at_ms = ?4
              WHERE process_id = ?1",
            params![process_id, native_pid, native_start_time, now_ms],
        )?;
        if updated == 0 {
            return Err(ProcessTableError::NotFound {
                process_id: process_id.to_string(),
            });
        }
        Ok(())
    }

    /// Link a ledger run after the fact. Some surfaces mint their process
    /// before the run row exists (the ledger enforces foreign keys, so the
    /// link cannot be written first).
    pub fn link_run(&self, process_id: &str, run_id: &str, now_ms: i64) -> ProcessTableResult<()> {
        let updated = self.connection.execute(
            "UPDATE agent_processes SET run_id = ?2, updated_at_ms = ?3 WHERE process_id = ?1",
            params![process_id, run_id, now_ms],
        )?;
        if updated == 0 {
            return Err(ProcessTableError::NotFound {
                process_id: process_id.to_string(),
            });
        }
        Ok(())
    }

    /// Record what is holding this process, once its controller has attached it.
    ///
    /// Write-once in practice and idempotent by construction: a controller's
    /// answer does not change over a process's life, and a caller that projects
    /// on a timer must not rewrite the row on every tick. It is a plain
    /// overwrite rather than a `COALESCE` because the one case that *does*
    /// rewrite it is real — a spawn that fell back to the supervisor after the
    /// row was admitted — and the later answer is the true one.
    pub fn set_containment(
        &self,
        process_id: &str,
        containment: &crate::resource_control::Containment,
        now_ms: i64,
    ) -> ProcessTableResult<()> {
        let enforcement = serde_json::to_string(&containment.enforcement).map_err(|error| {
            ProcessTableError::InvalidField {
                field: "resource_enforcement_json",
                reason: error.to_string(),
            }
        })?;
        let updated = self.connection.execute(
            "UPDATE agent_processes
                SET resource_backend = ?2,
                    resource_tree_primitive = ?3,
                    resource_scope = ?4,
                    resource_enforcement_json = ?5,
                    updated_at_ms = ?6
              WHERE process_id = ?1",
            params![
                process_id,
                containment.backend,
                containment.tree_primitive,
                containment.scope,
                enforcement,
                now_ms,
            ],
        )?;
        if updated == 0 {
            return Err(ProcessTableError::NotFound {
                process_id: process_id.to_string(),
            });
        }
        Ok(())
    }

    /// Durably record what one supervised workload owns, and how to find it after
    /// a restart.
    ///
    /// # Why this is one transaction
    ///
    /// The members and the supervision metadata are read back together by the
    /// startup reclaim, and a half-written pair is worse than neither: a member
    /// set recorded without its boot marker cannot be safely validated on Linux,
    /// and a boot marker recorded without its members claims a recovery the
    /// database cannot perform.
    ///
    /// # Never a row per sample
    ///
    /// `ON CONFLICT` updates `last_seen_at_ms` rather than inserting, so the
    /// table grows with the processes actually observed and not with the sampling
    /// interval. `first_seen_at_ms` is left alone, because when a member was
    /// first attributed to this workload is the audit fact worth keeping.
    ///
    /// A missing row is an error rather than a no-op: ownership with no lifecycle
    /// to hang it on is exactly the state the fail-closed admission exists to
    /// prevent, and swallowing it here would put it back.
    pub fn record_owned(
        &self,
        kind: ProcessKind,
        external_id: &str,
        members: &[crate::process_tree::ProcessIdentity],
        session: Option<u32>,
        boot_marker: Option<&str>,
        now_ms: i64,
    ) -> ProcessTableResult<()> {
        let Some(record) = self.find_by_external_id(kind, external_id)? else {
            return Err(ProcessTableError::NotFound {
                process_id: format!("{}:{external_id}", kind.as_str()),
            });
        };
        self.record_owned_members(&record.process_id, members, session, boot_marker, now_ms)
    }

    /// [`Self::record_owned`] against a process id the caller already resolved.
    pub fn record_owned_members(
        &self,
        process_id: &str,
        members: &[crate::process_tree::ProcessIdentity],
        session: Option<u32>,
        boot_marker: Option<&str>,
        now_ms: i64,
    ) -> ProcessTableResult<()> {
        let transaction = self.connection.unchecked_transaction()?;
        // The session guard mirrors the SQL `CHECK`: 0 is "this process's own"
        // and 1 is init's, and neither names a workload. Recording one would hand
        // the startup reclaim a set containing this app's own processes.
        let session = session.filter(|session| *session > 1);
        let updated = transaction.execute(
            "UPDATE agent_processes
                SET supervised_session_id = COALESCE(?2, supervised_session_id),
                    native_boot_marker = COALESCE(?3, native_boot_marker),
                    updated_at_ms = ?4
              WHERE process_id = ?1",
            params![process_id, session.map(i64::from), boot_marker, now_ms],
        )?;
        if updated == 0 {
            return Err(ProcessTableError::NotFound {
                process_id: process_id.to_string(),
            });
        }
        for member in members {
            transaction.execute(
                "INSERT INTO agent_process_owned_members (
                     process_id, native_pid, native_start_time, first_seen_at_ms, last_seen_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT (process_id, native_pid, native_start_time)
                 DO UPDATE SET last_seen_at_ms = ?4",
                params![
                    process_id,
                    i64::from(member.pid),
                    // Saturating rather than refusing: a start time this app
                    // cannot store is still an identity it must not forget, and
                    // the reclaim compares the stored value against a freshly
                    // read one that would saturate identically.
                    i64::try_from(member.start_time).unwrap_or(i64::MAX),
                    now_ms,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Every native identity this row has been recorded as owning.
    ///
    /// An out-of-range stored value is an **error**, not a skipped row: the
    /// startup reclaim's only safe answer to ownership metadata it cannot
    /// validate is [`ExitStatus::ContainmentLost`], and quietly dropping the
    /// member would turn that into a confident `lost`.
    pub fn owned_members(&self, process_id: &str) -> ProcessTableResult<Vec<OwnedMember>> {
        let mut statement = self.connection.prepare(
            "SELECT native_pid, native_start_time, first_seen_at_ms, last_seen_at_ms
               FROM agent_process_owned_members
              WHERE process_id = ?1
              ORDER BY first_seen_at_ms, native_pid",
        )?;
        let rows = statement.query_map(params![process_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut members = Vec::new();
        for row in rows {
            let (pid, start_time, first_seen_at_ms, last_seen_at_ms) = row?;
            let pid = u32::try_from(pid).map_err(|_| ProcessTableError::InvalidField {
                field: "native_pid",
                reason: format!("{pid} is outside the pid space"),
            })?;
            let start_time =
                u64::try_from(start_time).map_err(|_| ProcessTableError::InvalidField {
                    field: "native_start_time",
                    reason: format!("{start_time} is not a start time this host could have read"),
                })?;
            members.push(OwnedMember {
                identity: crate::process_tree::ProcessIdentity { pid, start_time },
                first_seen_at_ms,
                last_seen_at_ms,
            });
        }
        Ok(members)
    }

    /// Record the latest measurement of the owned tree.
    ///
    /// Peaks are folded in SQL with `MAX` rather than trusted from the caller, so
    /// a controller that is rebuilt mid-life — or a second writer that only knows
    /// the current value — cannot lower a peak the row already reached. The
    /// current values are plain overwrites, which is what "current" means.
    ///
    /// Wall time is deliberately not stored: it is `now - started_at_ms` for a
    /// live row and `exited_at_ms - started_at_ms` for a finished one, both of
    /// which the row already carries. A third copy would be a number to disagree
    /// with the other two.
    pub fn set_tree_usage(
        &self,
        process_id: &str,
        sample: &crate::resource_control::ResourceSample,
        now_ms: i64,
    ) -> ProcessTableResult<()> {
        let updated = self.connection.execute(
            // Each peak is folded only when the new sample *has* one. A bare
            // `MAX(COALESCE(peak, 0), COALESCE(?, 0))` turns an unmeasured
            // reading into a recorded zero — a backend that measures nothing
            // would write `peak = 0`, which reads as "this tree held nothing"
            // rather than "nobody looked", and those are the two things this
            // whole surface exists to keep apart.
            "UPDATE agent_processes
                SET tree_rss_bytes = ?2,
                    tree_peak_rss_bytes = CASE
                        WHEN ?3 IS NULL THEN tree_peak_rss_bytes
                        ELSE MAX(COALESCE(tree_peak_rss_bytes, 0), ?3)
                    END,
                    tree_process_count = ?4,
                    tree_peak_process_count = CASE
                        WHEN ?5 IS NULL THEN tree_peak_process_count
                        ELSE MAX(COALESCE(tree_peak_process_count, 0), ?5)
                    END,
                    tree_output_bytes = COALESCE(?6, tree_output_bytes),
                    tree_sampled_at_ms = ?7,
                    updated_at_ms = ?7
              WHERE process_id = ?1",
            params![
                process_id,
                to_sql_u64(sample.rss_bytes),
                to_sql_u64(sample.peak_rss_bytes.or(sample.rss_bytes)),
                sample.process_count.map(i64::from),
                sample
                    .peak_process_count
                    .or(sample.process_count)
                    .map(i64::from),
                to_sql_u64(sample.output_bytes),
                now_ms,
            ],
        )?;
        if updated == 0 {
            return Err(ProcessTableError::NotFound {
                process_id: process_id.to_string(),
            });
        }
        Ok(())
    }

    /// Apply a [`ProcessProjection`] idempotently.
    ///
    /// This is the single find-or-admit-then-transition implementation. Every
    /// adopter goes through it rather than composing [`Self::admit`] and
    /// [`Self::transition`] itself, because those compositions are where the
    /// subtle mistakes live: forgetting that a resume must not overwrite
    /// `started_at_ms`, treating a late projection after a terminal write as an
    /// error, or re-admitting after a restart and forking the record.
    ///
    /// Semantics worth being explicit about:
    ///
    /// - A projection describing a live state for a record that has already
    ///   exited is **ignored**, not an error
    ///   ([`ReconcileOutcome::AlreadyExited`]). An adopter that reconciles on a
    ///   timer will legitimately arrive after the terminal write, and
    ///   resurrecting a finished process would be worse than a stale read.
    /// - An illegal transition that is not that terminal case is still an
    ///   error. Reconcile is forgiving about ordering, not about the state
    ///   machine.
    /// - A parent that cannot be resolved leaves the edge unset rather than
    ///   refusing the projection: losing a lineage edge is worth less than
    ///   losing the record, and a child can legitimately be projected before
    ///   its parent.
    pub fn reconcile(
        &self,
        projection: &ProcessProjection,
        now_ms: i64,
    ) -> ProcessTableResult<(ProcessRecord, ReconcileOutcome)> {
        if (projection.state == ProcessState::Exited) != projection.exit.is_some() {
            return Err(ProcessTableError::TerminalMismatch {
                process_id: format!("{}:{}", projection.kind.as_str(), projection.external_id),
            });
        }

        let existing = self.find_by_external_id(projection.kind, &projection.external_id)?;

        let (record, admitted) = match existing {
            Some(record) => (record, false),
            None => {
                let parent_process_id = match &projection.parent {
                    Some((parent_kind, parent_external)) => self
                        .find_by_external_id(*parent_kind, parent_external)?
                        .map(|parent| parent.process_id),
                    None => None,
                };
                let request = AdmitProcess {
                    kind: projection.kind,
                    external_id: projection.external_id.clone(),
                    parent_process_id,
                    run_id: projection.run_id.clone(),
                    workspace: projection.workspace.clone(),
                    profile: projection.profile.clone(),
                    limits: projection.limits,
                };
                (self.admit(&request, now_ms)?, true)
            }
        };

        // Fill in what could not be known at admission time.
        if record.run_id.is_none() {
            if let Some(run_id) = projection.run_id.as_deref() {
                self.link_run(&record.process_id, run_id, now_ms)?;
            }
        }
        if projection.native_pid.is_some()
            && (record.native_pid != projection.native_pid
                || (projection.native_start_time.is_some()
                    && record.native_start_time != projection.native_start_time))
        {
            self.set_native_identity(
                &record.process_id,
                projection.native_pid,
                projection.native_start_time,
                now_ms,
            )?;
        }
        // Written before the transition below, so a projection that both attaches
        // and exits — a command that finished inside one tick — still records what
        // held it. The close-out reads the row back, and a row that gains its
        // containment afterwards would have been closed without it.
        if let Some(containment) = projection.containment.as_ref() {
            if record.containment.as_ref() != Some(containment) {
                self.set_containment(&record.process_id, containment, now_ms)?;
            }
        }
        if let Some(sample) = projection.usage.as_ref() {
            self.set_tree_usage(&record.process_id, sample, now_ms)?;
        }

        if record.state == projection.state {
            let refreshed = self.get(&record.process_id)?.unwrap_or(record);
            return Ok((
                refreshed,
                if admitted {
                    ReconcileOutcome::Admitted
                } else {
                    ReconcileOutcome::Unchanged
                },
            ));
        }

        if record.state.is_terminal() {
            return Ok((record, ReconcileOutcome::AlreadyExited));
        }

        let moved = self.transition(
            &record.process_id,
            projection.state,
            projection.exit.clone(),
            now_ms,
        )?;
        Ok((
            moved,
            if admitted {
                ReconcileOutcome::Admitted
            } else {
                ReconcileOutcome::Transitioned
            },
        ))
    }

    pub fn get(&self, process_id: &str) -> ProcessTableResult<Option<ProcessRecord>> {
        self.connection
            .query_row(
                &format!("{SELECT_COLUMNS} WHERE process_id = ?1"),
                params![process_id],
                map_row,
            )
            .optional()?
            .transpose()
    }

    pub fn find_by_external_id(
        &self,
        kind: ProcessKind,
        external_id: &str,
    ) -> ProcessTableResult<Option<ProcessRecord>> {
        self.connection
            .query_row(
                &format!("{SELECT_COLUMNS} WHERE kind = ?1 AND external_id = ?2"),
                params![kind.as_str(), external_id],
                map_row,
            )
            .optional()?
            .transpose()
    }

    /// Newest first, always bounded.
    pub fn list(&self, filter: &ProcessFilter) -> ProcessTableResult<Vec<ProcessRecord>> {
        let mut sql = String::from(SELECT_COLUMNS);
        let mut clauses: Vec<String> = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if !filter.kinds.is_empty() {
            let placeholders = filter
                .kinds
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(", ");
            clauses.push(format!("kind IN ({placeholders})"));
            for kind in &filter.kinds {
                values.push(Box::new(kind.as_str().to_string()));
            }
        }
        if filter.live_only {
            clauses.push("state != 'exited'".to_string());
        }
        if let Some(parent) = filter.parent_process_id.as_deref() {
            clauses.push("parent_process_id = ?".to_string());
            values.push(Box::new(parent.to_string()));
        }
        if let Some(workspace) = filter.workspace.as_deref() {
            clauses.push("workspace = ?".to_string());
            values.push(Box::new(workspace.to_string()));
        }
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }

        let limit = filter
            .limit
            .unwrap_or(DEFAULT_LIST_LIMIT)
            .clamp(1, MAX_LIST_LIMIT);
        sql.push_str(" ORDER BY created_at_ms DESC, process_id DESC LIMIT ?");
        values.push(Box::new(limit as i64));

        let mut statement = self.connection.prepare(&sql)?;
        let bindings: Vec<&dyn rusqlite::ToSql> =
            values.iter().map(|value| value.as_ref()).collect();
        let rows = statement.query_map(bindings.as_slice(), map_row)?;

        let mut records = Vec::new();
        for row in rows {
            records.push(row??);
        }
        Ok(records)
    }

    /// Every descendant of `process_id`, breadth-first. Bounded by
    /// [`MAX_LIST_LIMIT`] so a cycle written by some future direct-SQL writer
    /// cannot hang a listing surface.
    pub fn descendants(&self, process_id: &str) -> ProcessTableResult<Vec<ProcessRecord>> {
        let mut out: Vec<ProcessRecord> = Vec::new();
        let mut frontier = vec![process_id.to_string()];
        let mut seen: std::collections::HashSet<String> =
            std::collections::HashSet::from([process_id.to_string()]);

        while let Some(parent) = frontier.pop() {
            if out.len() >= MAX_LIST_LIMIT as usize {
                break;
            }
            let children = self.list(&ProcessFilter {
                parent_process_id: Some(parent),
                limit: Some(MAX_LIST_LIMIT),
                ..ProcessFilter::default()
            })?;
            for child in children {
                if seen.insert(child.process_id.clone()) {
                    frontier.push(child.process_id.clone());
                    out.push(child);
                }
            }
        }
        Ok(out)
    }

    /// Live count per kind, for the listing surfaces that today each aggregate
    /// a different subset of reality.
    pub fn live_counts(&self) -> ProcessTableResult<Vec<(ProcessKind, u32)>> {
        let mut statement = self.connection.prepare(
            "SELECT kind, COUNT(*) FROM agent_processes WHERE state != 'exited' GROUP BY kind",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (kind, count) = row?;
            out.push((ProcessKind::parse(&kind)?, count.max(0) as u32));
        }
        Ok(out)
    }

    /// Mark every live process whose worker is gone as [`ExitStatus::Lost`].
    ///
    /// This is the reaper guarantee nothing had outside the daemon: a desktop
    /// turn's ledger run whose WebView died stayed `running` forever because
    /// nothing swept it. `live_process_ids` is what the caller can still
    /// account for; anything live, matching `scope`, and absent from that list
    /// is reaped.
    ///
    /// `scope` is not optional in practice and the reason is important: a
    /// process's owner is not always the caller. The daemon is a separate
    /// service that outlives the desktop app, so an app reaping "everything I
    /// cannot account for" at startup would declare every live daemon job lost
    /// while it is still running. Each caller passes the kinds it actually owns.
    /// `live_only` is forced on regardless of what `scope` says.
    pub fn reap_missing(
        &self,
        scope: &ProcessFilter,
        live_process_ids: &[String],
        reason: &str,
        now_ms: i64,
    ) -> ProcessTableResult<Vec<ProcessRecord>> {
        let live: std::collections::HashSet<&str> =
            live_process_ids.iter().map(String::as_str).collect();
        let candidates = self.list(&ProcessFilter {
            live_only: true,
            limit: Some(MAX_LIST_LIMIT),
            ..scope.clone()
        })?;

        let mut reaped = Vec::new();
        for candidate in candidates {
            if live.contains(candidate.process_id.as_str()) {
                continue;
            }
            reaped.push(self.transition(
                &candidate.process_id,
                ProcessState::Exited,
                Some(ProcessExit {
                    status: ExitStatus::Lost,
                    code: None,
                    signal: None,
                    reason: Some(reason.to_string()),
                    breach: None,
                }),
                now_ms,
            )?);
        }
        Ok(reaped)
    }

    /// Mark every live process in `scope` whose *host process* is gone as
    /// [`ExitStatus::Lost`].
    ///
    /// The companion to [`Self::reap_missing`], for work whose owner is not the
    /// caller. That one asks "what can I still account for", which only answers
    /// for kinds the caller itself runs — which is why it is scoped to
    /// [`ProcessKind::DESKTOP_OWNED`] at startup and deliberately leaves a live
    /// daemon job alone. A workflow run has neither property: any process can
    /// host one, so no caller can enumerate the live set, and nothing swept them
    /// at all. A crashed host left its run `running` forever.
    ///
    /// The liveness question is answered from `native_pid`, which the host
    /// records when it projects. Two rules matter:
    ///
    /// - A row with **no** pid is never reaped. An adopter that records no host
    ///   has said nothing about liveness, and reading that silence as "dead"
    ///   would close rows for work that is running fine.
    /// - `host_is_alive` is injected rather than called directly so the rule is
    ///   testable without spawning and killing real processes, and so a caller
    ///   can scope it (the daemon and the desktop pass the same
    ///   `os_signal::process_is_alive`).
    ///
    /// Pid reuse can only make this reap *less* than it should — see
    /// `os_signal::process_is_alive`.
    pub fn reap_dead_hosts(
        &self,
        scope: &ProcessFilter,
        host_is_alive: &dyn Fn(i64) -> bool,
        reason: &str,
        now_ms: i64,
    ) -> ProcessTableResult<Vec<ProcessRecord>> {
        let candidates = self.list(&ProcessFilter {
            live_only: true,
            limit: Some(MAX_LIST_LIMIT),
            ..scope.clone()
        })?;

        let mut reaped = Vec::new();
        for candidate in candidates {
            let Some(pid) = candidate.native_pid else {
                continue;
            };
            if host_is_alive(pid) {
                continue;
            }
            reaped.push(self.transition(
                &candidate.process_id,
                ProcessState::Exited,
                Some(ProcessExit {
                    status: ExitStatus::Lost,
                    code: None,
                    signal: None,
                    reason: Some(reason.to_string()),
                    breach: None,
                }),
                now_ms,
            )?);
        }
        Ok(reaped)
    }

    /// Resource ledger rows, newest first, always bounded.
    ///
    /// Reads back what [`Self::transition`] closed out, plus the wall time
    /// derived from the row's own timestamps. A row that has not been closed out
    /// still comes back — with its gaps explained as "not closed out yet" rather
    /// than omitted, so a caller can tell "no measurement" from "no row".
    pub fn usage_rows(
        &self,
        filter: &ProcessUsageFilter,
    ) -> ProcessTableResult<Vec<ProcessUsageRow>> {
        let mut sql = format!(
            "SELECT process_id, kind, external_id, run_id, workspace, state, exit_status, \
             created_at_ms, started_at_ms, exited_at_ms, {SELECT_USAGE_COLUMNS}, \
             usage_unavailable_json FROM agent_processes"
        );
        let mut clauses: Vec<&str> = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(process_id) = filter.process_id.as_deref() {
            clauses.push("process_id = ?");
            values.push(Box::new(process_id.to_string()));
        }
        if let Some(run_id) = filter.run_id.as_deref() {
            clauses.push("run_id = ?");
            values.push(Box::new(run_id.to_string()));
        }
        if let Some(workspace) = filter.workspace.as_deref() {
            clauses.push("workspace = ?");
            values.push(Box::new(workspace.to_string()));
        }
        if filter.closed_only {
            clauses.push("state = 'exited'");
        }
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY created_at_ms DESC, process_id DESC LIMIT ?");
        values.push(Box::new(i64::from(
            filter
                .limit
                .unwrap_or(DEFAULT_LIST_LIMIT)
                .clamp(1, MAX_LIST_LIMIT),
        )));

        let mut statement = self.connection.prepare(&sql)?;
        let bindings: Vec<&dyn rusqlite::ToSql> =
            values.iter().map(|value| value.as_ref()).collect();
        let rows = statement.query_map(bindings.as_slice(), map_usage_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    /// The same rows folded into one total per field.
    ///
    /// Folded in Rust over [`Self::usage_rows`] rather than by a second SQL
    /// aggregate, for one reason worth stating: `SUM` over a column with NULLs
    /// silently reports the total of the rows that happened to have a value, and
    /// a resource ledger whose whole premise is "never infer" cannot ship a
    /// headline number that quietly drops the processes nobody measured. Each
    /// [`ProcessUsageTotal`] therefore carries how many rows contributed and how
    /// many could not.
    pub fn usage_totals(
        &self,
        filter: &ProcessUsageFilter,
    ) -> ProcessTableResult<ProcessUsageAggregate> {
        let rows = self.usage_rows(filter)?;
        let mut aggregate = ProcessUsageAggregate {
            rows: u32::try_from(rows.len()).unwrap_or(u32::MAX),
            ..ProcessUsageAggregate::default()
        };
        for row in &rows {
            let measured = row.usage.measured();
            for (total, value) in [
                (&mut aggregate.wall_time_ms, row.wall_time_ms),
                (&mut aggregate.cpu_time_ms, measured.cpu_time_ms),
                (&mut aggregate.bytes_read, measured.bytes_read),
                (&mut aggregate.bytes_written, measured.bytes_written),
                (&mut aggregate.bytes_egressed, measured.bytes_egressed),
                (&mut aggregate.tokens_in, measured.tokens_in),
                (&mut aggregate.tokens_out, measured.tokens_out),
                (&mut aggregate.gpu_device_ms, measured.gpu_device_ms),
            ] {
                fold_total(total, value, Fold::Sum);
            }
            for (total, value) in [
                (&mut aggregate.peak_rss_bytes, measured.peak_rss_bytes),
                (
                    &mut aggregate.gpu_resident_bytes,
                    measured.gpu_resident_bytes,
                ),
            ] {
                fold_total(total, value, Fold::Max);
            }
        }
        Ok(aggregate)
    }
}

/// What [`ProcessTable::usage_rows`] selects over. Every field is optional; an
/// empty filter means every process, still bounded by [`DEFAULT_LIST_LIMIT`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessUsageFilter {
    pub process_id: Option<String>,
    pub run_id: Option<String>,
    pub workspace: Option<String>,
    /// Only rows that have exited, and therefore have a closed-out ledger row.
    pub closed_only: bool,
    pub limit: Option<u32>,
}

/// One process's resource ledger row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessUsageRow {
    pub process_id: String,
    pub kind: ProcessKind,
    pub external_id: String,
    pub run_id: Option<String>,
    pub workspace: Option<String>,
    pub state: ProcessState,
    pub exit_status: Option<ExitStatus>,
    /// Derived from the row's timestamps, never stored — see `MIGRATION_V8_SQL`.
    /// `None` until the process exits, with the reason recorded in
    /// `usage.unavailable` like any other gap.
    pub wall_time_ms: Option<u64>,
    pub usage: ProcessUsage,
}

/// One process's egress destinations, and what the cap cost.
///
/// `dropped` is beside the list rather than folded into it because a truncated
/// list that does not say it is truncated reads as a complete one — see
/// `run_scope::MAX_DESTINATIONS` for why there is a cap at all.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessEgressDestinations {
    pub destinations: Vec<EgressDestinationRow>,
    /// Requests to destinations past the cap: counted, not named. Zero means the
    /// list above is complete.
    pub dropped: u64,
}

/// One destination a process's allowed egress reached.
///
/// The one insert both destination writers use.
///
/// `ON CONFLICT` names the same `COALESCE` expressions as
/// `egress_destinations_key_idx`, which is what a nullable attribution column
/// costs: SQLite permits NULLs in a non-`INTEGER` primary key, so the key that
/// used to deduplicate these rows would silently stop doing so. Targeting the
/// index by its expressions restores exactly the old behaviour for a process row
/// and gives the unattributed rows the same.
const UPSERT_DESTINATION_SQL: &str = "INSERT INTO egress_destinations
     (process_id, unattributed_reason, scheme, host, port, requests, first_seen_ms, last_seen_ms)
 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
 ON CONFLICT(COALESCE(process_id, ''), COALESCE(unattributed_reason, ''), scheme, host, port)
 DO UPDATE SET
     requests = requests + excluded.requests,
     last_seen_ms = MAX(last_seen_ms, excluded.last_seen_ms)";

impl ProcessTable<'_> {
    /// Record where egress that belongs to *no run* went (roadmap K5).
    ///
    /// The counterpart to [`Self::add_egress_destinations`], and deliberately the
    /// same table: "which hosts did this app reach" is one question, and splitting
    /// its answer across two surfaces by whether a run happened to be in scope
    /// would make every reader join twice to ask it.
    ///
    /// `reason` is [`crate::run_scope::Unattributed::code`]'s own string — the
    /// vocabulary that already persists in `UNATTRIBUTED_EGRESS`'s labels and in
    /// the permission ledger's `attribution` — plus `egress.no-scope` and
    /// `egress.run-without-process`, the two cases that enum does not cover.
    /// Nothing new is invented here, so a reader correlating volume with
    /// destinations is matching on one vocabulary rather than two.
    ///
    /// Additive and all-or-nothing, exactly like the attributed writer, and for
    /// the same reason: the drain that produced this is consumed, so a
    /// half-written flush could not be retried without double-counting.
    pub fn add_unattributed_egress_destinations(
        &self,
        reason: &str,
        drain: &crate::run_scope::DestinationDrain,
        now_ms: i64,
    ) -> ProcessTableResult<()> {
        if drain.is_empty() {
            return Ok(());
        }
        let transaction = self.connection.unchecked_transaction()?;
        if drain.overflowed > 0 {
            transaction.execute(
                "INSERT INTO unattributed_egress_overflow (reason, dropped, updated_at_ms)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(reason) DO UPDATE SET
                     dropped = dropped + excluded.dropped,
                     updated_at_ms = excluded.updated_at_ms",
                params![reason, drain.overflowed as i64, now_ms],
            )?;
        }
        for (destination, requests) in &drain.seen {
            transaction.execute(
                UPSERT_DESTINATION_SQL,
                params![
                    None::<&str>,
                    reason,
                    destination.scheme,
                    destination.host,
                    i64::from(destination.port),
                    *requests as i64,
                    now_ms,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Every destination reached outside a run, by the reason it had no run.
    ///
    /// Ordered like [`Self::egress_destinations_for`]'s — noisiest first, then
    /// host, port and scheme — so the two surfaces read the same way.
    pub fn unattributed_egress_destinations(
        &self,
    ) -> ProcessTableResult<BTreeMap<String, ProcessEgressDestinations>> {
        let mut found: BTreeMap<String, ProcessEgressDestinations> = BTreeMap::new();
        let mut statement = self.connection.prepare(
            "SELECT unattributed_reason, scheme, host, port, requests, first_seen_ms, last_seen_ms
               FROM egress_destinations
              WHERE unattributed_reason IS NOT NULL
              ORDER BY requests DESC, host ASC, port ASC, scheme ASC",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    EgressDestinationRow {
                        scheme: row.get(1)?,
                        host: row.get(2)?,
                        port: u16::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                        requests: row.get::<_, i64>(4)? as u64,
                        first_seen_ms: row.get(5)?,
                        last_seen_ms: row.get(6)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (reason, destination) in rows {
            found
                .entry(reason)
                .or_default()
                .destinations
                .push(destination);
        }

        // A second query for the same reason the attributed reader uses one: the
        // dropped count lives elsewhere, and a reason can have overflowed without
        // any surviving named destination — which is precisely the case a reader
        // must not mistake for "reached nowhere".
        let mut dropped = self.connection.prepare(
            "SELECT reason, dropped FROM unattributed_egress_overflow WHERE dropped > 0",
        )?;
        let counts = dropped
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (reason, count) in counts {
            found.entry(reason).or_default().dropped = count;
        }
        Ok(found)
    }
}

/// A summary rather than an event — see `MIGRATION_V14_SQL` on why this is not
/// part of a hash chain and does not claim to be tamper-evident.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EgressDestinationRow {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub requests: u64,
    pub first_seen_ms: i64,
    pub last_seen_ms: i64,
}

/// One field's total across a set of ledger rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessUsageTotal {
    /// `None` when no row in the set measured this field. The total of nothing is
    /// unknown, not zero — the same rule the rows themselves follow.
    pub value: Option<u64>,
    pub measured_rows: u32,
    pub unavailable_rows: u32,
}

/// Totals by run or workspace, as [`ProcessTable::usage_totals`] folds them.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessUsageAggregate {
    pub rows: u32,
    pub wall_time_ms: ProcessUsageTotal,
    pub cpu_time_ms: ProcessUsageTotal,
    pub bytes_read: ProcessUsageTotal,
    pub bytes_written: ProcessUsageTotal,
    pub bytes_egressed: ProcessUsageTotal,
    pub tokens_in: ProcessUsageTotal,
    pub tokens_out: ProcessUsageTotal,
    pub gpu_device_ms: ProcessUsageTotal,
    /// Maxima, not sums. Adding two processes' peak footprints invents a number
    /// no moment in time ever saw — the machine only ever held one of them at a
    /// time unless the two overlapped, which this table does not know.
    pub peak_rss_bytes: ProcessUsageTotal,
    pub gpu_resident_bytes: ProcessUsageTotal,
}

enum Fold {
    Sum,
    Max,
}

fn fold_total(total: &mut ProcessUsageTotal, value: Option<u64>, fold: Fold) {
    match value {
        Some(value) => {
            total.measured_rows = total.measured_rows.saturating_add(1);
            total.value = Some(match (total.value, fold) {
                (Some(held), Fold::Sum) => held.saturating_add(value),
                (Some(held), Fold::Max) => held.max(value),
                (None, _) => value,
            });
        }
        None => total.unavailable_rows = total.unavailable_rows.saturating_add(1),
    }
}

/// The pid to record as this work's host, for a projection in `state`.
///
/// The one fact that made workflow runs unreapable. Both existing reapers work by
/// ownership — the daemon sweeps its own `daemon_job` rows each tick, the desktop
/// its own kinds at startup — and a workflow run belongs to neither: the desktop
/// app and `monkey workflow run` both host runs, through the same
/// `WorkflowService`, into the same ledger. A crashed host left its row `running`
/// with nothing able to tell that from work still going on in the other process.
///
/// `std::process::id()` is the right answer in every host precisely because this
/// is library code: whichever process is executing the work is the one calling it.
///
/// `None` once the state is terminal. Reconcile only overwrites a pid it is
/// given, so an exited row keeps the host that ran it; writing one for a state
/// nothing will ever sweep would invite a liveness read that means nothing.
pub fn hosting_pid(state: ProcessState) -> Option<i64> {
    if state.is_terminal() {
        return None;
    }
    Some(i64::from(std::process::id()))
}

/// Closes rows whose host process no longer exists, for the kinds that record
/// one ([`ProcessKind::HOST_RECORDED`]).
///
/// The single entry point both hosts call at startup — the desktop app from
/// `lib.rs`'s setup, the daemon before its first tick — so the rule, the scope
/// and the reason text live in one place instead of being restated per binary.
/// Whichever starts first cleans up after whichever died, including the case
/// neither existing reaper could reach: a host that crashed and never came back.
///
/// The clock and the liveness syscall are the impure parts, which is why they are
/// here rather than in [`ProcessTable::reap_dead_hosts`].
pub fn reap_processes_whose_host_died(
    table: &ProcessTable<'_>,
    now_ms: i64,
) -> ProcessTableResult<Vec<ProcessRecord>> {
    table.reap_dead_hosts(
        &ProcessFilter {
            kinds: ProcessKind::HOST_RECORDED.to_vec(),
            ..ProcessFilter::default()
        },
        &|pid| {
            u32::try_from(pid)
                // A pid that does not fit the OS type was never written by
                // `hosting_pid`; treating it as dead would reap a row on the
                // strength of a value we cannot interpret.
                .map(crate::os_signal::process_is_alive)
                .unwrap_or(true)
        },
        "the process hosting this run exited without closing it",
        now_ms,
    )
}

/// A [`ProcessProjector`] backed by the ledger at a path.
///
/// Deliberately path-based rather than holding Tauri state: the desktop, the
/// CLI, and the daemon all need to project, and only one of them has an
/// `AppHandle`. The connection is opened lazily on first use and then reused, so
/// a service that projects on every state change does not re-run migrations each
/// time.
///
/// This is the impure edge of the process table — it reads the clock, which is
/// why [`ProcessTable`] itself takes timestamps as parameters and stays testable
/// without one.
pub struct LedgerProcessProjector {
    path: std::path::PathBuf,
    ledger: Mutex<Option<crate::run_ledger::RunLedger>>,
}

impl LedgerProcessProjector {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        LedgerProcessProjector {
            path: path.into(),
            ledger: Mutex::new(None),
        }
    }
}

impl LedgerProcessProjector {
    /// The ledger, opened on first use, and the clock read once.
    ///
    /// Shared by both port methods so neither can drift into opening a second
    /// connection or reading a second clock.
    fn with_table<T>(
        &self,
        work: impl FnOnce(&ProcessTable<'_>, i64) -> Result<T, String>,
    ) -> Result<T, String> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "system clock is before the unix epoch".to_string())?
            .as_millis();
        let now_ms = i64::try_from(now_ms).map_err(|_| "clock is beyond bounds".to_string())?;

        let mut slot = self
            .ledger
            .lock()
            .map_err(|_| "process projector lock was poisoned".to_string())?;
        if slot.is_none() {
            *slot = Some(
                crate::run_ledger::RunLedger::open(&self.path)
                    .map_err(|error| error.to_string())?,
            );
        }
        let ledger = slot.as_ref().expect("ledger initialized above");
        work(&ledger.process_table(), now_ms)
    }
}

impl ProcessProjector for LedgerProcessProjector {
    fn project(&self, projection: &ProcessProjection) -> Result<(), String> {
        self.with_table(|table, now_ms| {
            table
                .reconcile(projection, now_ms)
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }

    fn record_owned(&self, owned: &OwnedProcesses) -> Result<(), String> {
        self.with_table(|table, now_ms| {
            table
                .record_owned(
                    owned.kind,
                    &owned.external_id,
                    &owned.members,
                    owned.session,
                    owned.boot_marker.as_deref(),
                    now_ms,
                )
                .map_err(|error| error.to_string())
        })
    }
}

const SELECT_COLUMNS: &str = "SELECT process_id, parent_process_id, kind, external_id, state, \
     run_id, workspace, profile, native_pid, max_wall_ms, max_memory_bytes, max_output_bytes, \
     max_child_processes, exit_status, exit_code, exit_signal, exit_reason, created_at_ms, \
     updated_at_ms, started_at_ms, exited_at_ms, stop_requested, suspend_requested, \
     signal_reason, signal_requested_at_ms, kill_requested, max_context_tokens, \
     limit_kind, limit_configured, limit_observed, limit_backend, limit_level, \
     limit_observed_at_ms, limit_evidence, native_start_time, \
     resource_backend, resource_tree_primitive, resource_scope, resource_enforcement_json, \
     tree_rss_bytes, tree_peak_rss_bytes, tree_process_count, tree_peak_process_count, \
     tree_output_bytes, tree_sampled_at_ms, supervised_session_id, native_boot_marker \
     FROM agent_processes";

/// The nine V8 measurement columns, in [`MeasuredUsage::fields`]' order so the
/// column list and the invariant's field list cannot drift apart.
const SELECT_USAGE_COLUMNS: &str = "cpu_time_ms, peak_rss_bytes, bytes_read, bytes_written, \
     bytes_egressed, tokens_in, tokens_out, gpu_resident_bytes, gpu_device_ms";

/// SQLite has no unsigned integer type, so a count wider than `i64::MAX` cannot
/// be stored. Clamping rather than failing: a byte count that large is a bug
/// somewhere upstream, and refusing the terminal write over it would strand the
/// row instead of recording a number that is merely saturated.
fn to_sql_u64(value: Option<u64>) -> Option<i64> {
    value.map(|value| i64::try_from(value).unwrap_or(i64::MAX))
}

fn map_measured_usage(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<MeasuredUsage> {
    let field = |index: usize| row.get::<_, Option<i64>>(offset + index);
    Ok(MeasuredUsage {
        cpu_time_ms: field(0)?.map(|value| value.max(0) as u64),
        peak_rss_bytes: field(1)?.map(|value| value.max(0) as u64),
        bytes_read: field(2)?.map(|value| value.max(0) as u64),
        bytes_written: field(3)?.map(|value| value.max(0) as u64),
        bytes_egressed: field(4)?.map(|value| value.max(0) as u64),
        tokens_in: field(5)?.map(|value| value.max(0) as u64),
        tokens_out: field(6)?.map(|value| value.max(0) as u64),
        gpu_resident_bytes: field(7)?.map(|value| value.max(0) as u64),
        gpu_device_ms: field(8)?.map(|value| value.max(0) as u64),
    })
}

/// Row → ledger row, deriving wall time and back-filling a reason for any gap the
/// stored note list does not cover.
///
/// The back-fill is what keeps a read of a row that predates V8, or one that has
/// not closed out yet, from failing [`ProcessUsage::new`]'s check: those rows have
/// real gaps and no stored reasons, and a read must describe them rather than
/// refuse them.
fn map_usage_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProcessTableResult<ProcessUsageRow>> {
    let kind = match ProcessKind::parse(&row.get::<_, String>(1)?) {
        Ok(value) => value,
        Err(error) => return Ok(Err(error)),
    };
    let state = match ProcessState::parse(&row.get::<_, String>(5)?) {
        Ok(value) => value,
        Err(error) => return Ok(Err(error)),
    };
    let exit_status = match row.get::<_, Option<String>>(6)? {
        Some(raw) => match ExitStatus::parse(&raw) {
            Ok(value) => Some(value),
            Err(error) => return Ok(Err(error)),
        },
        None => None,
    };

    let created_at_ms: i64 = row.get(7)?;
    let started_at_ms: Option<i64> = row.get(8)?;
    let exited_at_ms: Option<i64> = row.get(9)?;
    let measured = map_measured_usage(row, 10)?;
    let mut unavailable: Vec<TraceFieldNote> = row
        .get::<_, Option<String>>(19)?
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();

    // Wall time is the span from when the work actually started, falling back to
    // admission for a process that exited without ever running.
    let wall_time_ms = exited_at_ms.map(|exited| {
        u64::try_from(exited.saturating_sub(started_at_ms.unwrap_or(created_at_ms))).unwrap_or(0)
    });
    if wall_time_ms.is_none() {
        unavailable.push(TraceFieldNote {
            field: FIELD_WALL_TIME_MS.to_string(),
            reason: WALL_TIME_NOT_FINAL_REASON.to_string(),
        });
    }

    let back_fill = if state.is_terminal() {
        PREDATES_LEDGER_REASON
    } else {
        NOT_CLOSED_OUT_REASON
    };
    for (field, value) in measured.fields() {
        if value.is_none() && !unavailable.iter().any(|note| note.field == field) {
            unavailable.push(TraceFieldNote {
                field: field.to_string(),
                reason: back_fill.to_string(),
            });
        }
    }

    let usage = match ProcessUsage::new(measured, unavailable) {
        Ok(usage) => usage,
        Err(error) => return Ok(Err(error)),
    };
    Ok(Ok(ProcessUsageRow {
        process_id: row.get(0)?,
        kind,
        external_id: row.get(2)?,
        run_id: row.get(3)?,
        workspace: row.get(4)?,
        state,
        exit_status,
        wall_time_ms,
        usage,
    }))
}

/// Row → record. Returns a nested `Result` because a stored enum that fails to
/// parse is a data error, not a SQLite error, and must not be reported as one.
fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProcessTableResult<ProcessRecord>> {
    let kind_raw: String = row.get(2)?;
    let state_raw: String = row.get(4)?;
    let exit_status_raw: Option<String> = row.get(13)?;

    let kind = match ProcessKind::parse(&kind_raw) {
        Ok(value) => value,
        Err(error) => return Ok(Err(error)),
    };
    let state = match ProcessState::parse(&state_raw) {
        Ok(value) => value,
        Err(error) => return Ok(Err(error)),
    };
    let exit = match exit_status_raw {
        Some(raw) => match ExitStatus::parse(&raw) {
            Ok(status) => Some(ProcessExit {
                status,
                code: row.get(14)?,
                signal: row.get(15)?,
                reason: row.get(16)?,
                // Present only for a row a resource controller closed. Read as a
                // group, and a group that does not arrive whole is reported as
                // such: the SQL constrains these five columns to be all-or-none,
                // so a partial row means some other writer bypassed the schema,
                // and defaulting the gaps would turn that into "a budget of 0 was
                // exceeded" — a confident sentence about a number nobody set.
                breach: match row.get::<_, Option<String>>(27)? {
                    Some(limit) => {
                        let require_u64 = |value: Option<i64>, missing| match value {
                            Some(value) => Ok(value as u64),
                            None => Err(ProcessTableError::PartialBreach {
                                limit: limit.clone(),
                                missing,
                            }),
                        };
                        let require_text = |value: Option<String>, missing| match value {
                            Some(value) => Ok(value),
                            None => Err(ProcessTableError::PartialBreach {
                                limit: limit.clone(),
                                missing,
                            }),
                        };
                        let configured = require_u64(row.get(28)?, "limit_configured");
                        let observed = require_u64(row.get(29)?, "limit_observed");
                        let backend = require_text(row.get(30)?, "limit_backend");
                        let level = require_text(row.get(31)?, "limit_level");
                        match (configured, observed, backend, level) {
                            (Ok(configured), Ok(observed), Ok(backend), Ok(level)) => {
                                Some(crate::resource_control::LimitBreach {
                                    limit,
                                    configured,
                                    observed,
                                    backend,
                                    level,
                                    // A stamp is the one field a breach can
                                    // honestly lack — see `now_ms`'s own note on
                                    // a clock that will not read.
                                    observed_at_ms: row
                                        .get::<_, Option<i64>>(32)?
                                        .unwrap_or_default(),
                                    // Absent for every supervised bound, by
                                    // design: the supervisor's evidence is the
                                    // two numbers beside it.
                                    evidence: row.get::<_, Option<String>>(33)?,
                                })
                            }
                            (Err(error), ..)
                            | (_, Err(error), ..)
                            | (_, _, Err(error), _)
                            | (_, _, _, Err(error)) => return Ok(Err(error)),
                        }
                    }
                    None => None,
                },
            }),
            Err(error) => return Ok(Err(error)),
        },
        None => None,
    };

    // The mechanism that held *this* process, as its own controller reported it
    // — read back rather than recomputed from the host doing the reading. A
    // stored enforcement map that will not parse is dropped rather than guessed
    // at: an invented capability would say a limit was kernel-held when nothing
    // knows whether it was.
    let containment = match row.get::<_, Option<String>>(35)? {
        Some(backend) => Some(crate::resource_control::Containment {
            backend,
            tree_primitive: row.get::<_, Option<String>>(36)?.unwrap_or_default(),
            scope: row.get(37)?,
            enforcement: row
                .get::<_, Option<String>>(38)?
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default(),
        }),
        None => None,
    };

    // Read as a group with `wall_ms`, because that is the one field of a sample
    // that is always measurable: a row with a sample stamp has been measured, and
    // a row without one has not. Every other field stays `Option`, so a backend
    // that measures nothing reports nothing rather than zero.
    let usage_sampled_at_ms: Option<i64> = row.get(44)?;
    let usage = usage_sampled_at_ms
        .map(|_| -> rusqlite::Result<_> {
            Ok(crate::resource_control::RecordedUsage {
                rss_bytes: row.get::<_, Option<i64>>(39)?.map(|v| v as u64),
                peak_rss_bytes: row.get::<_, Option<i64>>(40)?.map(|v| v as u64),
                process_count: row.get::<_, Option<i64>>(41)?.map(|v| v as u32),
                peak_process_count: row.get::<_, Option<i64>>(42)?.map(|v| v as u32),
                output_bytes: row.get::<_, Option<i64>>(43)?.map(|v| v as u64),
            })
        })
        .transpose()?;

    Ok(Ok(ProcessRecord {
        process_id: row.get(0)?,
        parent_process_id: row.get(1)?,
        kind,
        external_id: row.get(3)?,
        state,
        run_id: row.get(5)?,
        workspace: row.get(6)?,
        profile: row.get(7)?,
        native_pid: row.get(8)?,
        native_start_time: row.get(34)?,
        limits: ProcessLimits {
            max_wall_ms: row.get::<_, Option<i64>>(9)?.map(|v| v as u64),
            max_memory_bytes: row.get::<_, Option<i64>>(10)?.map(|v| v as u64),
            max_output_bytes: row.get::<_, Option<i64>>(11)?.map(|v| v as u64),
            max_child_processes: row.get::<_, Option<i64>>(12)?.map(|v| v as u32),
            // Appended at 26 rather than slotted beside the other four, so every
            // index above stays where it was.
            max_context_tokens: row.get::<_, Option<i64>>(26)?.map(|v| v as u64),
        },
        containment,
        supervised_session_id: row
            .get::<_, Option<i64>>(45)?
            .and_then(|v| u32::try_from(v).ok()),
        native_boot_marker: row.get(46)?,
        usage,
        usage_sampled_at_ms,
        exit,
        signal_intent: SignalIntent {
            stop_requested: row.get::<_, i64>(21)? != 0,
            suspend_requested: row.get::<_, i64>(22)? != 0,
            kill_requested: row.get::<_, i64>(25)? != 0,
        },
        signal_reason: row.get(23)?,
        signal_requested_at_ms: row.get(24)?,
        created_at_ms: row.get(17)?,
        updated_at_ms: row.get(18)?,
        started_at_ms: row.get(19)?,
        exited_at_ms: row.get(20)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_ledger::RunLedger;

    const T0: i64 = 1_800_000_000_000;

    fn ledger() -> RunLedger {
        RunLedger::open_in_memory().expect("in-memory ledger opens")
    }

    fn admit(table: &ProcessTable<'_>, kind: ProcessKind, external: &str) -> ProcessRecord {
        table
            .admit(&AdmitProcess::new(kind, external), T0)
            .expect("admit succeeds")
    }

    /// A `runs` row plus one `usage_recorded` event, written straight to SQL.
    ///
    /// Deliberately not `RunLedger::submit_run`: this needs a run to exist so the
    /// `agent_processes.run_id` foreign key resolves and so
    /// `ProcessTable::latest_run_usage` has an event stream to read, and a full
    /// `RunSpec` would be forty lines of irrelevant detail. The envelope itself is
    /// built through serde so its stored shape is the real one.
    fn seed_run_with_usage(
        ledger: &RunLedger,
        run_id: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) {
        use crate::run_protocol::{
            ClientIdentity, ClientKind, RunEvent, RunEventEnvelope, UsageSnapshot,
            RUN_PROTOCOL_SCHEMA_VERSION,
        };

        ledger
            .connection()
            .execute(
                "INSERT INTO runs (run_id, idempotency_key, spec_json, created_at_ms,
                                   updated_at_ms, status, last_sequence, max_event_count)
                 VALUES (?1, ?1, x'7b7d', ?2, ?2, 'running', 0, 1000)",
                params![run_id, T0],
            )
            .expect("a run row is seeded");

        let envelope = RunEventEnvelope {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            event_id: format!("event-{run_id}-1"),
            run_id: run_id.to_string(),
            sequence: 1,
            occurred_at_ms: T0 as u64,
            actor_id: None,
            emitter: ClientIdentity {
                client_id: "client-usage-test".to_string(),
                instance_id: "instance-usage-test".to_string(),
                kind: ClientKind::Test,
                version: "1".to_string(),
            },
            event: RunEvent::UsageRecorded {
                usage: UsageSnapshot {
                    input_tokens,
                    output_tokens,
                    cached_input_tokens: 0,
                    model_calls: 1,
                    tool_calls: 0,
                    cost_micros: None,
                },
            },
        };
        ledger
            .connection()
            .execute(
                "INSERT INTO run_events (event_id, run_id, sequence, occurred_at_ms, actor_id,
                                        emitter_json, event_type, envelope_json, derived_status,
                                        is_terminal)
                 VALUES (?1, ?2, 1, ?3, NULL, x'7b7d', 'usage_recorded', ?4, NULL, 0)",
                params![
                    envelope.event_id,
                    run_id,
                    T0,
                    serde_json::to_vec(&envelope).expect("the envelope serialises"),
                ],
            )
            .expect("a usage event is seeded");
    }

    fn usage_of(table: &ProcessTable<'_>, process_id: &str) -> ProcessUsageRow {
        table
            .usage_rows(&ProcessUsageFilter {
                process_id: Some(process_id.to_string()),
                ..ProcessUsageFilter::default()
            })
            .expect("the ledger reads back")
            .pop()
            .expect("a row exists for an admitted process")
    }

    /// The invariant the whole resource ledger exists for. Without this check a
    /// gap reaches storage with nothing recorded about it, and the next reader
    /// treats the NULL as a zero.
    #[test]
    fn a_usage_row_with_an_unexplained_gap_cannot_be_constructed() {
        let error = ProcessUsage::new(MeasuredUsage::default(), Vec::new())
            .expect_err("nine unexplained gaps must be refused");
        assert!(
            matches!(error, ProcessTableError::UsageGapWithoutReason { .. }),
            "got {error:?}"
        );

        // One field short of complete is still refused, and the error names the
        // field that is missing its reason.
        let mut notes: Vec<TraceFieldNote> = MeasuredUsage::default()
            .fields()
            .iter()
            .filter(|(field, _)| *field != FIELD_BYTES_WRITTEN)
            .map(|(field, _)| TraceFieldNote {
                field: (*field).to_string(),
                reason: "not measured in this test".to_string(),
            })
            .collect();
        assert!(matches!(
            ProcessUsage::new(MeasuredUsage::default(), notes.clone()),
            Err(ProcessTableError::UsageGapWithoutReason {
                field: FIELD_BYTES_WRITTEN
            })
        ));

        // The counter-test: with every gap explained it constructs. Without this,
        // a constructor that refused everything would pass the assertions above.
        notes.push(TraceFieldNote {
            field: FIELD_BYTES_WRITTEN.to_string(),
            reason: "not measured in this test".to_string(),
        });
        let usage =
            ProcessUsage::new(MeasuredUsage::default(), notes).expect("every gap is accounted for");
        assert_eq!(
            usage.reason_for(FIELD_BYTES_WRITTEN),
            Some("not measured in this test")
        );

        // A measured field needs no note, and none is invented for it.
        let usage = ProcessUsage::new(
            MeasuredUsage {
                cpu_time_ms: Some(7),
                ..MeasuredUsage::default()
            },
            MeasuredUsage::default()
                .fields()
                .iter()
                .filter(|(field, _)| *field != FIELD_CPU_TIME_MS)
                .map(|(field, _)| TraceFieldNote {
                    field: (*field).to_string(),
                    reason: "not measured in this test".to_string(),
                })
                .collect(),
        )
        .expect("a measured field needs no reason");
        assert_eq!(usage.measured().cpu_time_ms, Some(7));
        assert_eq!(usage.reason_for(FIELD_CPU_TIME_MS), None);
    }

    /// The wire shape a listing surface reads. Worth pinning for the same reason
    /// the signal-intent shape is: `#[serde(flatten)]` puts the nine measurement
    /// fields at the top level of `usage`, and a rename or an accidental nesting
    /// would compile fine while every number silently read as `undefined`.
    #[test]
    fn a_usage_row_serialises_with_its_measurements_flat_and_its_reasons_beside_them() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::SideTask, "task-usage-wire");
        table
            .accumulate_usage(
                &record.process_id,
                &ProcessUsageSample {
                    cpu_time_ms: Some(42),
                    ..ProcessUsageSample::default()
                },
                T0 + 1,
            )
            .expect("a sample folds in");

        let json =
            serde_json::to_value(usage_of(&table, &record.process_id)).expect("the row serialises");

        assert_eq!(json["processId"], record.process_id);
        assert_eq!(json["kind"], "side_task");
        assert_eq!(json["state"], "admitted");
        assert_eq!(json["wallTimeMs"], serde_json::Value::Null);
        // Flat, not nested under a `measured` key.
        assert_eq!(json["usage"]["cpuTimeMs"], 42);
        assert_eq!(json["usage"]["peakRssBytes"], serde_json::Value::Null);
        // A NULL always travels with the reason that explains it.
        let reasons = json["usage"]["unavailable"]
            .as_array()
            .expect("unavailable is an array");
        assert!(reasons
            .iter()
            .any(|note| note["field"] == FIELD_PEAK_RSS_BYTES && note["reason"].is_string()));
        assert!(
            !reasons
                .iter()
                .any(|note| note["field"] == FIELD_CPU_TIME_MS),
            "a measured field must not appear in the reason list"
        );
    }

    /// A reaped process is the case that matters most: nobody was watching it, so
    /// there is nothing to measure — and a ledger that answered "0 bytes, 0ms" for
    /// it would be lying about the one process it knows least about.
    #[test]
    fn a_reaped_process_still_closes_with_a_reason_for_every_field_and_no_zeros() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::WorkflowRun, "wf-crashed-host");
        table
            .transition(&record.process_id, ProcessState::Running, None, T0 + 1)
            .expect("the run starts");
        table
            .set_native_pid(&record.process_id, Some(999_999), T0 + 1)
            .expect("the host records its pid");

        let reaped = table
            .reap_dead_hosts(
                &ProcessFilter::default(),
                &|_| false,
                "the process hosting this run exited without closing it",
                T0 + 5,
            )
            .expect("the reaper runs");
        assert_eq!(reaped.len(), 1, "the row whose host is gone is closed");

        let row = usage_of(&table, &record.process_id);
        assert_eq!(row.exit_status, Some(ExitStatus::Lost));
        assert_eq!(
            row.wall_time_ms,
            Some(4),
            "wall time is derived, not stored"
        );

        let measured = row.usage.measured();
        for (field, value) in measured.fields() {
            assert_eq!(value, None, "{field} was never measured for a reaped row");
            assert!(
                row.usage.reason_for(field).is_some(),
                "{field} is NULL and must therefore state why"
            );
        }
        assert_eq!(
            row.usage.reason_for(FIELD_CPU_TIME_MS),
            Some(REAPED_REASON),
            "a reaped row must say nothing could have sampled it, not merely that nothing did"
        );
        assert_eq!(
            row.usage.reason_for(FIELD_GPU_DEVICE_MS),
            Some(GPU_NOT_REPORTED_REASON)
        );
    }

    /// The SQL half of the same guarantee: a writer that bypassed
    /// `close_out_usage` cannot land an `exited` row with no reason list. Mirrors
    /// how V5's transition rules are enforced in both Rust and SQL.
    #[test]
    fn sql_refuses_an_exited_row_that_states_nothing_about_its_gaps() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::ChatTurn, "turn-direct-sql");

        let error = ledger
            .connection()
            .execute(
                "UPDATE agent_processes SET state = 'exited', exit_status = 'succeeded'
                  WHERE process_id = ?1",
                params![record.process_id],
            )
            .expect_err("the trigger must abort a close-out with no reason list");
        assert!(
            error
                .to_string()
                .contains("must state its unmeasured fields"),
            "got {error}"
        );
    }

    /// Destinations accumulate across flushes rather than each flush replacing
    /// the last, which is what makes the drain-and-write cycle safe to repeat.
    #[test]
    fn egress_destinations_accumulate_across_flushes_and_keep_their_first_sighting() {
        use crate::run_scope::{Destination, DestinationDrain};

        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::ChatTurn, "turn-destinations");

        let destination = |host: &str| Destination {
            scheme: "https".to_string(),
            host: host.to_string(),
            port: 443,
        };
        table
            .add_egress_destinations(
                &record.process_id,
                &DestinationDrain {
                    seen: vec![(destination("api.example.com"), 2)],
                    overflowed: 0,
                },
                T0 + 1,
            )
            .expect("the first flush lands");
        table
            .add_egress_destinations(
                &record.process_id,
                &DestinationDrain {
                    seen: vec![
                        (destination("api.example.com"), 3),
                        (destination("cdn.example.com"), 1),
                    ],
                    overflowed: 7,
                },
                T0 + 5,
            )
            .expect("the second flush lands");

        let found = table
            .egress_destinations_for(&[record.process_id.clone(), "p-untouched".to_string()])
            .expect("the destinations read back");
        assert_eq!(
            found.keys().collect::<Vec<_>>(),
            vec![&record.process_id],
            "a process with nothing recorded is absent, not present and empty"
        );
        let recorded = &found[&record.process_id];
        let rows = &recorded.destinations;
        assert_eq!(rows.len(), 2, "one row per destination, not per flush");
        assert_eq!(rows[0].host, "api.example.com", "busiest destination first");
        assert_eq!(rows[0].requests, 5, "the two flushes are summed");
        assert_eq!(
            (rows[0].first_seen_ms, rows[0].last_seen_ms),
            (T0 + 1, T0 + 5),
            "the first sighting is kept and only the last one moves"
        );
        assert_eq!(rows[1].requests, 1);
        assert_eq!(
            recorded.dropped, 7,
            "requests past the cap are recorded on the process, not lost"
        );
    }

    /// An unknown process is refused the same way every other writer here
    /// refuses one, and leaves nothing behind.
    #[test]
    fn egress_destinations_for_an_unknown_process_are_refused_whole() {
        use crate::run_scope::{Destination, DestinationDrain};

        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let outcome = table.add_egress_destinations(
            "p-does-not-exist",
            &DestinationDrain {
                seen: vec![(
                    Destination {
                        scheme: "https".to_string(),
                        host: "api.example.com".to_string(),
                        port: 443,
                    },
                    1,
                )],
                overflowed: 0,
            },
            T0 + 1,
        );
        assert!(matches!(outcome, Err(ProcessTableError::NotFound { .. })));
        assert!(
            table
                .egress_destinations_for(&["p-does-not-exist".to_string()])
                .expect("the read succeeds")
                .is_empty(),
            "a refused flush must not leave a partial write behind"
        );
    }

    /// Two flushes sum, and a process no runtime reported reuse for stays absent
    /// rather than reading back as a measured zero.
    #[test]
    fn context_reuse_sums_across_flushes_and_absence_is_not_a_zero() {
        use crate::run_scope::ContextReuse;

        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let measured = admit(&table, ProcessKind::ChatTurn, "turn-measured");
        let silent = admit(&table, ProcessKind::ChatTurn, "turn-ollama");

        table
            .add_context_reuse(
                &measured.process_id,
                ContextReuse {
                    reused_tokens: 0,
                    evaluated_tokens: 1_000,
                },
                T0 + 1,
            )
            .expect("the cold turn's flush lands");
        table
            .add_context_reuse(
                &measured.process_id,
                ContextReuse {
                    reused_tokens: 9,
                    evaluated_tokens: 1,
                },
                T0 + 2,
            )
            .expect("the warm turn's flush lands");

        let found = table
            .context_reuse_for(&[measured.process_id.clone(), silent.process_id.clone()])
            .expect("the measurements read back");
        assert_eq!(
            found.keys().collect::<Vec<_>>(),
            vec![&measured.process_id],
            "a process whose runtime reported no reuse figure is absent, not zero"
        );
        assert_eq!(
            found[&measured.process_id],
            ContextReuse {
                reused_tokens: 9,
                evaluated_tokens: 1_001
            },
            "the flushes are summed, not replaced"
        );
        assert!(matches!(
            table.add_context_reuse("p-does-not-exist", ContextReuse::default(), T0 + 3),
            Err(ProcessTableError::NotFound { .. })
        ));
    }

    /// Peak resident size is unreadable once a pid is gone, so the value that
    /// reaches the ledger has to be the highest one sampled while the process
    /// lived — and close-out must keep it rather than overwrite it with a reason.
    #[test]
    fn a_sampled_peak_survives_close_out_and_only_the_unsampled_fields_get_reasons() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::BackgroundShell, "sh-sampled");
        table
            .transition(&record.process_id, ProcessState::Running, None, T0 + 1)
            .expect("the shell starts");

        for (cpu, rss) in [(10, 4_000), (25, 9_000), (30, 5_000)] {
            table
                .accumulate_usage(
                    &record.process_id,
                    &ProcessUsageSample {
                        cpu_time_ms: Some(cpu),
                        peak_rss_bytes: Some(rss),
                        ..ProcessUsageSample::default()
                    },
                    T0 + 2,
                )
                .expect("a sample folds in");
        }
        table
            .add_egress_bytes(&record.process_id, 1_024, T0 + 3)
            .expect("egress is attributed");
        table
            .add_egress_bytes(&record.process_id, 512, T0 + 3)
            .expect("egress accumulates");

        let closed = table
            .transition(
                &record.process_id,
                ProcessState::Exited,
                Some(ProcessExit::succeeded()),
                T0 + 9,
            )
            .expect("the shell exits");
        assert_eq!(closed.state, ProcessState::Exited);

        let row = usage_of(&table, &record.process_id);
        let measured = row.usage.measured();
        assert_eq!(measured.cpu_time_ms, Some(30));
        assert_eq!(
            measured.peak_rss_bytes,
            Some(9_000),
            "the peak must survive a later, smaller sample"
        );
        assert_eq!(measured.bytes_egressed, Some(1_536));
        assert_eq!(row.wall_time_ms, Some(8));
        // A measured field carries no "not measured" note...
        assert_eq!(row.usage.reason_for(FIELD_PEAK_RSS_BYTES), None);
        assert_eq!(row.usage.reason_for(FIELD_BYTES_EGRESSED), None);
        // ...and one nothing sampled still does.
        assert_eq!(measured.bytes_read, None);
        assert_eq!(
            row.usage.reason_for(FIELD_BYTES_READ),
            Some(NOT_SAMPLED_REASON)
        );
    }

    /// Tokens come from the run's own event stream, because `UsageSnapshot` lives
    /// inside `run_events.envelope_json` and is not queryable by SQL.
    #[test]
    fn tokens_are_read_from_the_runs_usage_events_when_the_process_has_a_run() {
        let ledger = ledger();
        seed_run_with_usage(&ledger, "run-tokens", 1_200, 340);
        let table = ProcessTable::new(ledger.connection());
        let record = table
            .admit(
                &AdmitProcess::new(ProcessKind::DaemonJob, "job-tokens").with_run("run-tokens"),
                T0,
            )
            .expect("a job with a run is admitted");
        table
            .transition(
                &record.process_id,
                ProcessState::Exited,
                Some(ProcessExit::succeeded()),
                T0 + 1,
            )
            .expect("the job finishes");

        let row = usage_of(&table, &record.process_id);
        assert_eq!(row.usage.measured().tokens_in, Some(1_200));
        assert_eq!(row.usage.measured().tokens_out, Some(340));
        assert_eq!(row.usage.reason_for(FIELD_TOKENS_IN), None);
    }

    /// `agent_processes.run_id` is NULL for `subagent` and the `m4` workflow
    /// kinds, so their token counts are *structurally* unavailable. Reporting zero
    /// would claim a subagent spends no tokens, which is the opposite of true.
    #[test]
    fn a_process_with_no_run_marks_its_tokens_structurally_unavailable() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::Subagent, "sub-no-run");
        table
            .transition(
                &record.process_id,
                ProcessState::Exited,
                Some(ProcessExit::succeeded()),
                T0 + 1,
            )
            .expect("the subagent finishes");

        let row = usage_of(&table, &record.process_id);
        assert_eq!(row.usage.measured().tokens_in, None);
        assert_eq!(row.usage.measured().tokens_out, None);
        assert_eq!(row.usage.reason_for(FIELD_TOKENS_IN), Some(NO_RUN_REASON));
        assert_eq!(row.usage.reason_for(FIELD_TOKENS_OUT), Some(NO_RUN_REASON));
    }

    /// A live row has no closed-out ledger yet, and a read must say so rather
    /// than fail the invariant check or omit the process entirely.
    #[test]
    fn a_live_row_reads_back_with_its_gaps_explained_as_not_closed_out() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::ChatTurn, "turn-still-running");

        let row = usage_of(&table, &record.process_id);
        assert_eq!(row.state, ProcessState::Admitted);
        assert_eq!(row.wall_time_ms, None);
        assert_eq!(
            row.usage.reason_for(FIELD_WALL_TIME_MS),
            Some(WALL_TIME_NOT_FINAL_REASON)
        );
        assert_eq!(
            row.usage.reason_for(FIELD_CPU_TIME_MS),
            Some(NOT_CLOSED_OUT_REASON)
        );
    }

    /// An aggregate over rows where some fields were never measured must report
    /// the shortfall. A bare `SUM` reports the total of whatever happened to have
    /// a value and looks like the total of everything.
    #[test]
    fn totals_report_how_many_rows_could_not_be_measured_rather_than_summing_nulls_as_zero() {
        let ledger = ledger();
        seed_run_with_usage(&ledger, "run-agg", 100, 20);
        let table = ProcessTable::new(ledger.connection());

        // Two processes on one run: one sampled, one not.
        for (external, sample) in [
            ("job-agg-sampled", Some((40_u64, 8_000_u64))),
            ("job-agg-unsampled", None),
        ] {
            let record = table
                .admit(
                    &AdmitProcess::new(ProcessKind::DaemonJob, external).with_run("run-agg"),
                    T0,
                )
                .expect("admitted");
            if let Some((cpu, rss)) = sample {
                table
                    .accumulate_usage(
                        &record.process_id,
                        &ProcessUsageSample {
                            cpu_time_ms: Some(cpu),
                            peak_rss_bytes: Some(rss),
                            ..ProcessUsageSample::default()
                        },
                        T0 + 1,
                    )
                    .expect("a sample folds in");
            }
            table
                .transition(
                    &record.process_id,
                    ProcessState::Exited,
                    Some(ProcessExit::succeeded()),
                    T0 + 3,
                )
                .expect("finished");
        }

        let totals = table
            .usage_totals(&ProcessUsageFilter {
                run_id: Some("run-agg".to_string()),
                ..ProcessUsageFilter::default()
            })
            .expect("totals fold");

        assert_eq!(totals.rows, 2);
        assert_eq!(totals.cpu_time_ms.value, Some(40));
        assert_eq!(totals.cpu_time_ms.measured_rows, 1);
        assert_eq!(
            totals.cpu_time_ms.unavailable_rows, 1,
            "the unsampled process must be visible as a gap, not folded in as a zero"
        );
        // Both processes read the same run, so tokens are measured for both.
        assert_eq!(totals.tokens_in.value, Some(200));
        assert_eq!(totals.tokens_in.unavailable_rows, 0);
        // Peaks are maxima: summing two footprints would invent a number no
        // moment ever saw.
        assert_eq!(totals.peak_rss_bytes.value, Some(8_000));
        // Nothing measures GPU residency, so the total is unknown rather than 0.
        assert_eq!(totals.gpu_resident_bytes.value, None);
        assert_eq!(totals.gpu_resident_bytes.unavailable_rows, 2);
        assert_eq!(totals.wall_time_ms.value, Some(6));
    }

    /// The wire shape `processSignalDelivery.ts` reads to decide what to deliver.
    ///
    /// Worth its own assertion because nothing else catches it: a rename here
    /// compiles, and the TypeScript side keeps typechecking against its own
    /// interface, so the only symptom would be a signal that silently never
    /// arrives — `stopRequested` reading as `undefined` is falsy.
    #[test]
    fn a_records_signal_intent_serialises_as_the_frontend_reads_it() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::SideTask, "task-wire");
        let signalled = table
            .signal(
                &record.process_id,
                ProcessSignal::Suspend,
                Some("wire check"),
                T0 + 1,
            )
            .expect("suspend is honoured for a side task");

        let json = serde_json::to_value(&signalled).expect("record serialises");

        assert_eq!(json["signalIntent"]["suspendRequested"], true);
        assert_eq!(json["signalIntent"]["stopRequested"], false);
        assert_eq!(json["signalReason"], "wire check");
        assert_eq!(json["signalRequestedAtMs"], T0 + 1);
        assert_eq!(json["externalId"], "task-wire");
        assert_eq!(json["kind"], "side_task");
        assert_eq!(json["state"], "admitted");
    }

    /// The frontend enforcer writes this reason and Rust reads it back, so the two
    /// literals have to agree. Pinned here; `processWallBudget.test.ts` pins the
    /// other side.
    #[test]
    fn the_wall_budget_marker_is_the_string_the_frontend_writes() {
        assert_eq!(
            WALL_BUDGET_REASON_PREFIX,
            "wall budget exceeded: max_wall_ms"
        );
    }

    /// A kernel-reported breach survives the round trip whole, including the
    /// counter that proved it.
    ///
    /// The evidence matters most in exactly the case the two numbers cannot
    /// carry: a `pids.max` refusal leaves configured and observed **equal**,
    /// which read alone says the limit did not fire.
    #[test]
    fn a_kernel_breach_round_trips_with_the_counter_that_proved_it() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::ForegroundShell, "fgsh-kernel-breach");
        table
            .transition(&record.process_id, ProcessState::Running, None, T0 + 1)
            .expect("the shell starts");

        let breach = crate::resource_control::LimitBreach {
            limit: "max_child_processes".to_string(),
            configured: 12,
            observed: 12,
            backend: "cgroup v2".to_string(),
            level: "kernel".to_string(),
            observed_at_ms: T0 + 2,
            evidence: Some("cgroup v2 `pids.events` max".to_string()),
        };
        table
            .transition(
                &record.process_id,
                ProcessState::Exited,
                Some(ProcessExit::limit_exceeded(breach.clone())),
                T0 + 3,
            )
            .expect("a limit kill is a terminal transition");

        let stored = table
            .get(&record.process_id)
            .expect("read back")
            .expect("the row exists")
            .exit
            .expect("an exited row carries its exit")
            .breach
            .expect("a limit kill carries its breach");
        assert_eq!(stored, breach);
        assert!(
            stored.describe().contains("pids.events"),
            "with both numbers equal, the evidence is the only thing that says the limit fired: {}",
            stored.describe()
        );
    }

    /// A supervised breach has no kernel counter, and is not made to invent one.
    #[test]
    fn a_supervised_breach_round_trips_with_no_evidence_rather_than_an_empty_one() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::ForegroundShell, "fgsh-supervised");
        table
            .transition(&record.process_id, ProcessState::Running, None, T0 + 1)
            .expect("the shell starts");
        table
            .transition(
                &record.process_id,
                ProcessState::Exited,
                Some(ProcessExit::limit_exceeded(
                    crate::resource_control::LimitBreach {
                        limit: "max_memory_bytes".to_string(),
                        configured: 1_024,
                        observed: 4_096,
                        backend: "supervisor".to_string(),
                        level: "supervised".to_string(),
                        observed_at_ms: T0 + 2,
                        evidence: None,
                    },
                )),
                T0 + 3,
            )
            .expect("terminal");
        let stored = table
            .get(&record.process_id)
            .expect("read back")
            .expect("row")
            .exit
            .expect("exit")
            .breach
            .expect("breach");
        assert_eq!(stored.evidence, None);
    }

    /// A breach row missing one of its grouped values is reported, never
    /// completed with zeros.
    ///
    /// The schema constrains the five to arrive together, so this writes past it
    /// deliberately — which is the only way the case can exist, and exactly why
    /// the reader must not paper over it: `configured: 0` reads as "a budget of
    /// zero was exceeded", a confident sentence about a number nobody set.
    #[test]
    fn a_partial_stored_breach_is_an_error_rather_than_a_manufactured_zero() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::ForegroundShell, "fgsh-partial");
        table
            .transition(&record.process_id, ProcessState::Running, None, T0 + 1)
            .expect("running");
        table
            .transition(
                &record.process_id,
                ProcessState::Exited,
                Some(ProcessExit::limit_exceeded(
                    crate::resource_control::LimitBreach {
                        limit: "max_memory_bytes".to_string(),
                        configured: 1_024,
                        observed: 4_096,
                        backend: "supervisor".to_string(),
                        level: "supervised".to_string(),
                        observed_at_ms: T0 + 2,
                        evidence: None,
                    },
                )),
                T0 + 3,
            )
            .expect("terminal");
        // The schema refuses this write, which is the first line of defence and is
        // asserted below. Suspending the check is how a row written by some other
        // tool — an older build, a repair script, a hand-edited database — is
        // reproduced, because that is the only way this state can arise.
        assert!(
            ledger
                .connection()
                .execute(
                    "UPDATE agent_processes SET limit_configured = NULL WHERE process_id = ?1",
                    rusqlite::params![record.process_id],
                )
                .is_err(),
            "the schema must refuse a half-written breach in the first place"
        );
        ledger
            .connection()
            .pragma_update(None, "ignore_check_constraints", true)
            .expect("suspend the guard for this one write");
        ledger
            .connection()
            .execute(
                "UPDATE agent_processes SET limit_configured = NULL WHERE process_id = ?1",
                rusqlite::params![record.process_id],
            )
            .expect("a writer that bypassed the schema");
        ledger
            .connection()
            .pragma_update(None, "ignore_check_constraints", false)
            .expect("restore the guard");

        let error = table
            .get(&record.process_id)
            .expect_err("a half-written breach is a data error");
        assert!(
            matches!(
                error,
                ProcessTableError::PartialBreach {
                    missing: "limit_configured",
                    ..
                }
            ),
            "{error}"
        );
    }

    /// Without this, a turn killed for exceeding its budget is indistinguishable
    /// from one a user stopped — and its recorded reason says "stopped by the
    /// user", which is not imprecise but false.
    #[test]
    fn a_wall_budget_stop_is_recorded_as_limit_exceeded_and_an_ordinary_stop_is_not() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());

        let budgeted = admit(&table, ProcessKind::ChatTurn, "turn-budget");
        table
            .transition(&budgeted.process_id, ProcessState::Running, None, T0 + 1)
            .expect("a turn starts running");
        let reason = format!("{WALL_BUDGET_REASON_PREFIX}=1000ms, ran 2000ms");
        table
            .signal(
                &budgeted.process_id,
                ProcessSignal::Stop,
                Some(&reason),
                T0 + 2,
            )
            .expect("the budget latches a stop");
        let exited = table
            .transition(
                &budgeted.process_id,
                ProcessState::Exited,
                // What every loop writes today, budget kill or not.
                Some(ProcessExit::cancelled("stopped by the user")),
                T0 + 3,
            )
            .expect("the turn winds down");
        let exit = exited.exit.expect("an exited row carries its exit");
        assert_eq!(exit.status, ExitStatus::LimitExceeded);
        assert_eq!(
            exit.reason.as_deref(),
            Some(reason.as_str()),
            "the false \"stopped by the user\" reason must be replaced, not kept"
        );

        // The counter-test: an ordinary stop stays an ordinary stop. Without it,
        // upgrading everything would pass the assertions above.
        let stopped = admit(&table, ProcessKind::ChatTurn, "turn-user-stop");
        table
            .transition(&stopped.process_id, ProcessState::Running, None, T0 + 1)
            .unwrap();
        table
            .signal(
                &stopped.process_id,
                ProcessSignal::Stop,
                Some("stopped by the user"),
                T0 + 2,
            )
            .unwrap();
        let exit = table
            .transition(
                &stopped.process_id,
                ProcessState::Exited,
                Some(ProcessExit::cancelled("stopped by the user")),
                T0 + 3,
            )
            .unwrap()
            .exit
            .expect("an exited row carries its exit");
        assert_eq!(exit.status, ExitStatus::Cancelled);
    }

    /// A budget stop in flight must not relabel a failure. The work's own verdict
    /// wins, or a real error would hide behind a limit.
    #[test]
    fn a_pending_budget_stop_does_not_relabel_a_failure_or_a_success() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let reason = format!("{WALL_BUDGET_REASON_PREFIX}=1000ms, ran 2000ms");

        for (external, exit, expected) in [
            (
                "turn-failed",
                ProcessExit::failed("the tool blew up"),
                ExitStatus::Failed,
            ),
            (
                "turn-succeeded",
                ProcessExit::succeeded(),
                ExitStatus::Succeeded,
            ),
        ] {
            let record = admit(&table, ProcessKind::ChatTurn, external);
            table
                .transition(&record.process_id, ProcessState::Running, None, T0 + 1)
                .unwrap();
            table
                .signal(
                    &record.process_id,
                    ProcessSignal::Stop,
                    Some(&reason),
                    T0 + 2,
                )
                .unwrap();
            let actual = table
                .transition(&record.process_id, ProcessState::Exited, Some(exit), T0 + 3)
                .unwrap()
                .exit
                .expect("an exited row carries its exit")
                .status;
            assert_eq!(actual, expected, "{external} was relabelled");
        }
    }

    /// `ProcessKind::ALL` is hand-maintained, and nothing made adding a variant
    /// fail without it.
    ///
    /// That is not a tidiness point. `ALL` is what `monkey processes limits`
    /// enumerates and what every "for each kind" invariant in this file iterates,
    /// so a variant missing from it is a kind whose limit matrix, signal matrix
    /// and class defaults are simply never checked — and the check that would
    /// have caught it passes, because it never looked. `ForegroundShell` was
    /// added and every one of those invariants still passed until this existed.
    ///
    /// Checked against the SQL vocabulary rather than against a second Rust list,
    /// because two copies of the same list agree by construction. The `CHECK`
    /// constraint is written by a different author for a different reason, so its
    /// agreement is evidence.
    #[test]
    fn the_kind_list_and_the_stored_vocabulary_name_the_same_kinds() {
        let database = crate::run_ledger::RunLedger::open(temp_ledger_path("kind-vocab"))
            .expect("ledger opens");
        let stored = database
            .stored_process_kinds()
            .expect("the CHECK is readable");
        let declared: std::collections::BTreeSet<String> = ProcessKind::ALL
            .iter()
            .map(|kind| kind.as_str().to_string())
            .collect();
        assert_eq!(
            stored, declared,
            "ProcessKind::ALL and the agent_processes.kind CHECK disagree; a kind in one and \
             not the other is either unstorable or silently exempt from every per-kind invariant"
        );
    }

    /// The declaration must match what the app really enforces, so this asserts
    /// the *shape of the honesty*: which kinds carry a bound, and that each
    /// number is the constant the enforcing code uses.
    #[test]
    fn every_kind_declares_the_bounds_it_is_actually_subject_to() {
        let bounded: Vec<ProcessKind> = ProcessKind::ALL
            .iter()
            .copied()
            .filter(|kind| !kind.default_limits().is_unbounded())
            .collect();
        assert_eq!(
            bounded,
            vec![
                ProcessKind::ChatTurn,
                ProcessKind::Subagent,
                ProcessKind::CrewMember,
                ProcessKind::BackgroundShell,
                ProcessKind::ForegroundShell,
                ProcessKind::SideTask,
                ProcessKind::BrowserSession,
                ProcessKind::VerifyCommand,
                ProcessKind::HookCommand,
                ProcessKind::SandboxRun
            ],
            "a kind gained or lost a class-level bound; if that is intended, the \
             field docs on ProcessLimits and the K4 roadmap entry have to move with it"
        );

        // Two owners on one row, and the assertions are per field because that is
        // exactly the split. The wall number is the one `browser_worker`'s
        // watchdog actually sweeps on — not a second ceiling declared beside it —
        // while the two process bounds belong to the resource controller, which
        // now holds Chromium's whole tree. Output stays absent: this kind
        // captures no stream of its own.
        let browser = ProcessKind::BrowserSession.default_limits();
        assert_eq!(
            browser.max_wall_ms,
            Some(crate::browser_worker::DEFAULT_MAX_SESSION_MS)
        );
        assert_eq!(browser.max_output_bytes, None);
        assert_eq!(browser.max_memory_bytes, Some(BROWSER_MEMORY_BUDGET_BYTES));
        assert_eq!(browser.max_child_processes, Some(BROWSER_PROCESS_BUDGET));

        let shell = ProcessKind::BackgroundShell.default_limits();
        assert_eq!(
            shell.max_output_bytes,
            Some(u64::try_from(crate::background_shell::MAX_OUTPUT_BYTES).unwrap()),
            "the declared output ceiling must be the one the tail truncation uses"
        );
        // A background shell is meant to outlive its turn, so it is spawned with
        // no timeout at all. Declaring a wall bound here would be a lie.
        assert_eq!(shell.max_wall_ms, None);

        // The two native shell kinds carry the same tree bounds, because they are
        // the same process under the same controller — the only difference is who
        // is waiting for it. Asserted per field rather than as a whole record so
        // the two output ceilings, which genuinely differ, stay visible: a
        // background shell's tail is read by a human and a foreground shell's by
        // a model.
        let foreground = ProcessKind::ForegroundShell.default_limits();
        for (kind, limits) in [
            (ProcessKind::BackgroundShell, shell),
            (ProcessKind::ForegroundShell, foreground),
        ] {
            assert_eq!(
                limits.max_memory_bytes,
                Some(SHELL_MEMORY_BUDGET_BYTES),
                "{} must declare the tree memory bound its controller holds",
                kind.as_str()
            );
            assert_eq!(
                limits.max_child_processes,
                Some(SHELL_PROCESS_BUDGET),
                "{} must declare the per-tree process bound its controller holds",
                kind.as_str()
            );
            assert_eq!(limits.max_context_tokens, None);
        }
        assert_eq!(
            foreground.max_output_bytes,
            Some(u64::try_from(crate::output_cap::MODEL_OUTPUT_CAP).unwrap()),
            "a foreground shell's output reaches a model, so it takes the model ceiling"
        );
        // Bounded by the caller's own tool timeout rather than by a class number:
        // `SHELL_TIMEOUT` and `DEFAULT_VERIFY_TIMEOUT_SECS` differ by more than
        // twice, so one class default would be wrong for one of them.
        assert_eq!(foreground.max_wall_ms, None);

        // The four WebView kinds all carry the same wall budget, and only that:
        // one number for four kinds because they are the same shape of process,
        // and four different numbers would be inventing policy.
        for kind in [
            ProcessKind::ChatTurn,
            ProcessKind::Subagent,
            ProcessKind::CrewMember,
            ProcessKind::SideTask,
        ] {
            let limits = kind.default_limits();
            assert_eq!(
                limits.max_wall_ms,
                Some(WEBVIEW_WALL_BUDGET_MS),
                "{} must carry the WebView wall budget",
                kind.as_str()
            );
            assert_eq!(
                limits,
                ProcessLimits {
                    max_wall_ms: Some(WEBVIEW_WALL_BUDGET_MS),
                    ..ProcessLimits::default()
                },
                "{} must declare the wall budget and nothing else — the other \
                 resources still have no enforcer for this kind",
                kind.as_str()
            );
        }
        // Six hours, asserted rather than left to a reader to infer, because the
        // *value* is the decision this slice made.
        assert_eq!(WEBVIEW_WALL_BUDGET_MS, 6 * 60 * 60 * 1_000);

        // The daemon is bounded, but by its own per-job recipe rather than by its
        // class — a class default would be overwritten on the next projection and
        // would only mislead a reader in between.
        assert!(ProcessKind::DaemonJob.default_limits().is_unbounded());
        // A workflow node and a remote run stay genuinely unbounded here.
        assert!(ProcessKind::WorkflowNode.default_limits().is_unbounded());
        assert!(ProcessKind::RemoteRun.default_limits().is_unbounded());
    }

    /// The seeding has to reach the stored row, not just the builder, and it has
    /// to lose to an explicit value — which is how the daemon supplies its truer
    /// per-job numbers.
    #[test]
    fn a_row_carries_its_class_limits_unless_the_caller_states_better_ones() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());

        let shell = admit(&table, ProcessKind::BackgroundShell, "shell-classlimits");
        assert_eq!(
            shell.limits,
            ProcessKind::BackgroundShell.default_limits(),
            "an adopter that never mentions limits must still record its class's"
        );
        assert_eq!(
            table
                .get(&shell.process_id)
                .expect("the row reads back")
                .expect("the row exists")
                .limits,
            ProcessKind::BackgroundShell.default_limits(),
            "the class limits must survive the round-trip through SQL"
        );

        // A turn declares its own class's wall budget and nothing else — it must
        // not inherit another kind's ceiling on the way past.
        let turn = admit(&table, ProcessKind::ChatTurn, "turn-classlimits");
        assert_eq!(turn.limits, ProcessKind::ChatTurn.default_limits());
        assert_eq!(turn.limits.max_wall_ms, Some(WEBVIEW_WALL_BUDGET_MS));
        assert_eq!(turn.limits.max_output_bytes, None);

        let explicit = ProcessLimits {
            max_wall_ms: Some(30_000),
            max_output_bytes: Some(64),
            ..ProcessLimits::default()
        };
        let overridden = table
            .admit(
                &AdmitProcess::new(ProcessKind::BackgroundShell, "shell-override")
                    .with_limits(explicit),
                T0,
            )
            .expect("admit succeeds");
        assert_eq!(
            overridden.limits, explicit,
            "an explicit limit set must win over the class default"
        );
    }

    /// `reconcile` admits through a projection, so the desktop kinds — whose only
    /// adopters project rather than admit — would miss their class limits
    /// entirely if only `AdmitProcess` were seeded.
    #[test]
    fn a_projected_row_also_carries_its_class_limits() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());

        let (record, _outcome) = table
            .reconcile(
                &ProcessProjection::new(
                    ProcessKind::BackgroundShell,
                    "shell-projected",
                    ProcessState::Running,
                ),
                T0,
            )
            .expect("a projection admits the row it describes");

        assert_eq!(
            record.limits.max_output_bytes,
            ProcessKind::BackgroundShell
                .default_limits()
                .max_output_bytes,
            "a projected shell must declare the ceiling its tail truncation enforces"
        );
    }

    #[test]
    fn an_admitted_process_starts_admitted_with_a_self_describing_id() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());

        let record = admit(&table, ProcessKind::ChatTurn, "turn-1");

        assert!(
            record.process_id.starts_with("p-turn-"),
            "id should name its kind: {}",
            record.process_id
        );
        assert_eq!(record.state, ProcessState::Admitted);
        assert_eq!(record.external_id, "turn-1");
        assert!(record.exit.is_none());
        assert!(record.started_at_ms.is_none());
        assert!(record.exited_at_ms.is_none());
        assert_eq!(record.created_at_ms, T0);
        // Seeded from the class, which for a chat turn is the WebView wall budget.
        assert_eq!(record.limits, ProcessKind::ChatTurn.default_limits());
        assert!(record.is_live());
    }

    #[test]
    fn every_kind_and_exit_status_survives_a_round_trip_through_sql() {
        // Guards against adding an enum variant without extending the migration's
        // CHECK constraint — the same enum-vs-storage drift class the red-team
        // mirror was.
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());

        for (index, kind) in ProcessKind::ALL.iter().enumerate() {
            let record = table
                .admit(&AdmitProcess::new(*kind, format!("external-{index}")), T0)
                .unwrap_or_else(|error| {
                    panic!("kind {} rejected by storage: {error}", kind.as_str())
                });
            assert_eq!(record.kind, *kind);
        }

        for (index, status) in [
            ExitStatus::Succeeded,
            ExitStatus::Failed,
            ExitStatus::Cancelled,
            ExitStatus::LimitExceeded,
            ExitStatus::Lost,
            ExitStatus::NeedsReconciliation,
        ]
        .into_iter()
        .enumerate()
        {
            let record = admit(&table, ProcessKind::SideTask, &format!("exit-{index}"));
            let exited = table
                .transition(
                    &record.process_id,
                    ProcessState::Exited,
                    Some(ProcessExit {
                        status,
                        code: Some(3),
                        signal: Some("SIGTERM".to_string()),
                        reason: Some("because".to_string()),
                        breach: None,
                    }),
                    T0 + 1,
                )
                .unwrap_or_else(|error| {
                    panic!("exit status {} rejected: {error}", status.as_str())
                });
            let exit = exited.exit.expect("exit detail round-trips");
            assert_eq!(exit.status, status);
            assert_eq!(exit.code, Some(3));
            assert_eq!(exit.signal.as_deref(), Some("SIGTERM"));
        }
    }

    #[test]
    fn readmitting_the_same_surface_id_is_refused_with_the_id_already_assigned() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let first = admit(&table, ProcessKind::DaemonJob, "job-1");

        let error = table
            .admit(&AdmitProcess::new(ProcessKind::DaemonJob, "job-1"), T0)
            .expect_err("a second admit must not fork the record");

        match error {
            ProcessTableError::DuplicateExternalId {
                existing_process_id,
                ..
            } => assert_eq!(existing_process_id, first.process_id),
            other => panic!("wrong error: {other}"),
        }

        // The same surface id under a different kind is a different process.
        admit(&table, ProcessKind::RemoteRun, "job-1");
    }

    #[test]
    fn the_legal_lifecycle_runs_end_to_end() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::DaemonJob, "job-life");

        let running = table
            .transition(&record.process_id, ProcessState::Running, None, T0 + 10)
            .unwrap();
        assert_eq!(running.started_at_ms, Some(T0 + 10));

        let suspended = table
            .transition(&record.process_id, ProcessState::Suspended, None, T0 + 20)
            .unwrap();
        assert_eq!(suspended.state, ProcessState::Suspended);

        let resumed = table
            .transition(&record.process_id, ProcessState::Running, None, T0 + 30)
            .unwrap();
        assert_eq!(
            resumed.started_at_ms,
            Some(T0 + 10),
            "a resume must not overwrite when the process first started"
        );

        let exited = table
            .transition(
                &record.process_id,
                ProcessState::Exited,
                Some(ProcessExit::succeeded()),
                T0 + 40,
            )
            .unwrap();
        assert_eq!(exited.exited_at_ms, Some(T0 + 40));
        assert!(!exited.is_live());
    }

    #[test]
    fn illegal_transitions_are_refused_rather_than_applied() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());

        // admitted -> suspended: nothing has started, so there is nothing to
        // suspend.
        let fresh = admit(&table, ProcessKind::ChatTurn, "turn-illegal");
        assert!(matches!(
            table.transition(&fresh.process_id, ProcessState::Suspended, None, T0 + 1),
            Err(ProcessTableError::IllegalTransition { .. })
        ));
        assert_eq!(
            table.get(&fresh.process_id).unwrap().unwrap().state,
            ProcessState::Admitted,
            "a refused transition must leave the row untouched"
        );

        // exited is terminal in every direction.
        let done = admit(&table, ProcessKind::ChatTurn, "turn-done");
        table
            .transition(&done.process_id, ProcessState::Running, None, T0 + 1)
            .unwrap();
        table
            .transition(
                &done.process_id,
                ProcessState::Exited,
                Some(ProcessExit::cancelled("stopped")),
                T0 + 2,
            )
            .unwrap();
        for next in [
            ProcessState::Admitted,
            ProcessState::Running,
            ProcessState::Suspended,
        ] {
            assert!(
                matches!(
                    table.transition(&done.process_id, next, None, T0 + 3),
                    Err(ProcessTableError::IllegalTransition { .. })
                ),
                "exited must not move to {}",
                next.as_str()
            );
        }

        // suspended -> admitted is backwards.
        let held = admit(&table, ProcessKind::DaemonJob, "job-held");
        table
            .transition(&held.process_id, ProcessState::Running, None, T0 + 1)
            .unwrap();
        table
            .transition(&held.process_id, ProcessState::Suspended, None, T0 + 2)
            .unwrap();
        assert!(matches!(
            table.transition(&held.process_id, ProcessState::Admitted, None, T0 + 3),
            Err(ProcessTableError::IllegalTransition { .. })
        ));
    }

    #[test]
    fn the_sql_trigger_refuses_what_rust_refuses() {
        // The Rust table and the SQL trigger encode the same rules. A companion
        // store holding this connection can bypass the Rust path entirely, so
        // assert the storage layer stands on its own.
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::WorkflowRun, "wf-trigger");

        let raw = ledger.connection().execute(
            "UPDATE agent_processes SET state = 'suspended' WHERE process_id = ?1",
            params![record.process_id],
        );
        let message = raw.expect_err("the trigger must abort this").to_string();
        assert!(
            message.contains("illegal agent process state transition"),
            "unexpected error: {message}"
        );

        // And the identity guard: kind, surface id, and creation time are fixed
        // once admitted, so a record cannot be quietly repurposed for another
        // process.
        for mutation in [
            "kind = 'daemon_job'",
            "external_id = 'wf-somethingelse'",
            "created_at_ms = 1",
            "process_id = 'p-wf-rewritten'",
        ] {
            let message = ledger
                .connection()
                .execute(
                    &format!("UPDATE agent_processes SET {mutation} WHERE process_id = ?1"),
                    params![record.process_id],
                )
                .expect_err("identity must be immutable")
                .to_string();
            assert!(
                message.contains("agent process identity is immutable"),
                "unexpected error for `{mutation}`: {message}"
            );
        }
    }

    #[test]
    fn exit_detail_and_terminal_state_must_agree() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::BackgroundShell, "sh-1");
        table
            .transition(&record.process_id, ProcessState::Running, None, T0 + 1)
            .unwrap();

        assert!(matches!(
            table.transition(&record.process_id, ProcessState::Exited, None, T0 + 2),
            Err(ProcessTableError::TerminalMismatch { .. })
        ));
        assert!(matches!(
            table.transition(
                &record.process_id,
                ProcessState::Suspended,
                Some(ProcessExit::succeeded()),
                T0 + 2
            ),
            Err(ProcessTableError::TerminalMismatch { .. })
        ));

        // The SQL CHECK holds the same line against a direct write.
        let raw = ledger.connection().execute(
            "UPDATE agent_processes SET exit_status = 'succeeded' WHERE process_id = ?1",
            params![record.process_id],
        );
        assert!(
            raw.is_err(),
            "an exit status on a running row must be refused"
        );
    }

    #[test]
    fn a_parent_must_exist_and_cannot_be_the_process_itself() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let parent = admit(&table, ProcessKind::ChatTurn, "turn-parent");

        let child = table
            .admit(
                &AdmitProcess::new(ProcessKind::Subagent, "sub-1").with_parent(&parent.process_id),
                T0 + 1,
            )
            .unwrap();
        assert_eq!(
            child.parent_process_id.as_deref(),
            Some(parent.process_id.as_str())
        );

        assert!(matches!(
            table.admit(
                &AdmitProcess::new(ProcessKind::Subagent, "sub-orphan").with_parent("p-turn-nope"),
                T0 + 2
            ),
            Err(ProcessTableError::UnknownParent { .. })
        ));

        let self_parent = ledger.connection().execute(
            "UPDATE agent_processes SET parent_process_id = process_id WHERE process_id = ?1",
            params![child.process_id],
        );
        assert!(
            self_parent.is_err(),
            "a process must not become its own parent"
        );
    }

    #[test]
    fn descendants_walks_the_whole_subtree() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let turn = admit(&table, ProcessKind::ChatTurn, "turn-tree");
        let sub = table
            .admit(
                &AdmitProcess::new(ProcessKind::Subagent, "sub-tree").with_parent(&turn.process_id),
                T0 + 1,
            )
            .unwrap();
        let shell = table
            .admit(
                &AdmitProcess::new(ProcessKind::BackgroundShell, "sh-tree")
                    .with_parent(&sub.process_id),
                T0 + 2,
            )
            .unwrap();
        // A sibling tree that must not appear.
        admit(&table, ProcessKind::ChatTurn, "turn-other");

        let mut ids: Vec<String> = table
            .descendants(&turn.process_id)
            .unwrap()
            .into_iter()
            .map(|record| record.process_id)
            .collect();
        ids.sort();
        let mut expected = vec![sub.process_id, shell.process_id];
        expected.sort();
        assert_eq!(ids, expected);
    }

    #[test]
    fn listing_filters_by_kind_liveness_and_workspace_and_is_always_bounded() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());

        let turn = table
            .admit(
                &AdmitProcess::new(ProcessKind::ChatTurn, "turn-a")
                    .with_workspace(Some("/work/one".to_string())),
                T0,
            )
            .unwrap();
        table
            .admit(
                &AdmitProcess::new(ProcessKind::DaemonJob, "job-a")
                    .with_workspace(Some("/work/two".to_string())),
                T0 + 1,
            )
            .unwrap();
        table
            .transition(&turn.process_id, ProcessState::Running, None, T0 + 2)
            .unwrap();
        // Admitted last, so it must sort first.
        let gone = table
            .admit(&AdmitProcess::new(ProcessKind::SideTask, "side-a"), T0 + 5)
            .unwrap();
        table
            .transition(&gone.process_id, ProcessState::Running, None, T0 + 6)
            .unwrap();
        table
            .transition(
                &gone.process_id,
                ProcessState::Exited,
                Some(ProcessExit::succeeded()),
                T0 + 7,
            )
            .unwrap();

        let all = table.list(&ProcessFilter::default()).unwrap();
        assert_eq!(all.len(), 3);
        // Newest first.
        assert_eq!(all[0].external_id, "side-a");

        let live = table
            .list(&ProcessFilter {
                live_only: true,
                ..ProcessFilter::default()
            })
            .unwrap();
        assert_eq!(live.len(), 2);
        assert!(live.iter().all(|record| record.is_live()));

        let turns = table
            .list(&ProcessFilter {
                kinds: vec![ProcessKind::ChatTurn],
                ..ProcessFilter::default()
            })
            .unwrap();
        assert_eq!(turns.len(), 1);

        let in_one = table
            .list(&ProcessFilter {
                workspace: Some("/work/one".to_string()),
                ..ProcessFilter::default()
            })
            .unwrap();
        assert_eq!(
            in_one.len(),
            1,
            "the table can answer what runs in a folder"
        );
        assert_eq!(in_one[0].external_id, "turn-a");

        // A caller cannot ask for an unbounded scan, and cannot ask for zero.
        let clamped = table
            .list(&ProcessFilter {
                limit: Some(u32::MAX),
                ..ProcessFilter::default()
            })
            .unwrap();
        assert_eq!(clamped.len(), 3);
        let single = table
            .list(&ProcessFilter {
                limit: Some(0),
                ..ProcessFilter::default()
            })
            .unwrap();
        assert_eq!(single.len(), 1);
    }

    #[test]
    fn live_counts_group_by_kind() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        admit(&table, ProcessKind::ChatTurn, "t1");
        admit(&table, ProcessKind::ChatTurn, "t2");
        let done = admit(&table, ProcessKind::Subagent, "s1");
        table
            .transition(&done.process_id, ProcessState::Running, None, T0 + 1)
            .unwrap();
        table
            .transition(
                &done.process_id,
                ProcessState::Exited,
                Some(ProcessExit::succeeded()),
                T0 + 2,
            )
            .unwrap();

        let counts = table.live_counts().unwrap();
        assert_eq!(counts, vec![(ProcessKind::ChatTurn, 2)]);
    }

    #[test]
    fn reaping_marks_a_vanished_worker_lost_and_leaves_accounted_ones_alone() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let alive = admit(&table, ProcessKind::ChatTurn, "turn-alive");
        let vanished = admit(&table, ProcessKind::ChatTurn, "turn-vanished");
        for record in [&alive, &vanished] {
            table
                .transition(&record.process_id, ProcessState::Running, None, T0 + 1)
                .unwrap();
        }

        let reaped = table
            .reap_missing(
                &ProcessFilter::default(),
                &[alive.process_id.clone()],
                "worker no longer accounted for",
                T0 + 5,
            )
            .unwrap();

        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].process_id, vanished.process_id);
        let exit = reaped[0].exit.as_ref().unwrap();
        assert_eq!(exit.status, ExitStatus::Lost);
        assert_eq!(
            exit.reason.as_deref(),
            Some("worker no longer accounted for")
        );

        assert_eq!(
            table.get(&alive.process_id).unwrap().unwrap().state,
            ProcessState::Running
        );
        // Reaping twice is a no-op — the reaped row is no longer live.
        assert!(table
            .reap_missing(
                &ProcessFilter::default(),
                &[alive.process_id.clone()],
                "again",
                T0 + 6
            )
            .unwrap()
            .is_empty());
    }

    /// Puts a workflow row in `state` with `pid` as its recorded host.
    fn hosted(
        table: &ProcessTable<'_>,
        kind: ProcessKind,
        external: &str,
        pid: Option<i64>,
        state: ProcessState,
    ) -> ProcessRecord {
        let mut projection = ProcessProjection::new(kind, external, state);
        projection.native_pid = pid;
        if state.is_terminal() {
            projection.exit = Some(ProcessExit {
                status: ExitStatus::Succeeded,
                code: None,
                signal: None,
                reason: None,
                breach: None,
            });
        }
        table.reconcile(&projection, T0).unwrap().0
    }

    #[test]
    fn a_dead_hosts_rows_are_reaped_and_a_live_hosts_are_left_alone() {
        // The gap this closes: a workflow run is executed by whichever process
        // started it, so neither reaper could touch it. `reap_missing` needs a
        // caller able to enumerate its own live work, and the daemon's tick
        // sweeps only `daemon_job`.
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());

        let dead = hosted(
            &table,
            ProcessKind::WorkflowRun,
            "run-dead-host",
            Some(4242),
            ProcessState::Running,
        );
        let live = hosted(
            &table,
            ProcessKind::WorkflowRun,
            "run-live-host",
            Some(777),
            ProcessState::Running,
        );

        let reaped = table
            .reap_dead_hosts(
                &ProcessFilter {
                    kinds: ProcessKind::HOST_RECORDED.to_vec(),
                    ..ProcessFilter::default()
                },
                &|pid| pid == 777,
                "the process hosting this run exited without closing it",
                T0 + 5,
            )
            .unwrap();

        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].process_id, dead.process_id);
        assert_eq!(reaped[0].exit.as_ref().unwrap().status, ExitStatus::Lost);
        assert_eq!(
            table.get(&live.process_id).unwrap().unwrap().state,
            ProcessState::Running,
            "a run still executing in another live process was declared lost"
        );
    }

    #[test]
    fn a_row_with_no_recorded_host_is_never_reaped_by_liveness() {
        // Silence is not death. An adopter that records no host has said nothing
        // about whether its work is running, and reading that as "dead" would
        // close rows for work that is fine — the one error worth engineering
        // against, since the opposite merely leaves a stale row.
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let unknown = hosted(
            &table,
            ProcessKind::WorkflowRun,
            "run-no-host",
            None,
            ProcessState::Running,
        );

        let reaped = table
            .reap_dead_hosts(
                &ProcessFilter {
                    kinds: ProcessKind::HOST_RECORDED.to_vec(),
                    ..ProcessFilter::default()
                },
                // Every pid is dead — the row must survive on the missing pid
                // alone, not on the liveness answer.
                &|_| false,
                "host gone",
                T0 + 5,
            )
            .unwrap();

        assert!(reaped.is_empty());
        assert_eq!(
            table.get(&unknown.process_id).unwrap().unwrap().state,
            ProcessState::Running
        );
    }

    #[test]
    fn host_liveness_reaping_stays_inside_its_scope_and_skips_terminal_rows() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());

        // A daemon job records a pid too, and the daemon's tick owns it. A
        // liveness pass scoped to workflows must not close it.
        let job = hosted(
            &table,
            ProcessKind::DaemonJob,
            "job-1#0",
            Some(4242),
            ProcessState::Running,
        );
        let finished = hosted(
            &table,
            ProcessKind::WorkflowRun,
            "run-finished",
            Some(4242),
            ProcessState::Running,
        );
        table
            .transition(
                &finished.process_id,
                ProcessState::Exited,
                Some(ProcessExit {
                    status: ExitStatus::Succeeded,
                    code: None,
                    signal: None,
                    reason: None,
                    breach: None,
                }),
                T0 + 1,
            )
            .unwrap();
        // Crashing while paused is still a crash: `suspended` is live.
        let paused = hosted(
            &table,
            ProcessKind::WorkflowRun,
            "run-paused",
            Some(4242),
            ProcessState::Running,
        );
        table
            .transition(&paused.process_id, ProcessState::Suspended, None, T0 + 2)
            .unwrap();

        let reaped = table
            .reap_dead_hosts(
                &ProcessFilter {
                    kinds: ProcessKind::HOST_RECORDED.to_vec(),
                    ..ProcessFilter::default()
                },
                &|_| false,
                "host gone",
                T0 + 5,
            )
            .unwrap();

        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].process_id, paused.process_id);
        assert_eq!(
            table.get(&job.process_id).unwrap().unwrap().state,
            ProcessState::Running
        );
        assert_eq!(
            table
                .get(&finished.process_id)
                .unwrap()
                .unwrap()
                .exit
                .unwrap()
                .status,
            ExitStatus::Succeeded,
            "a completed run was overwritten as lost"
        );
    }

    /// Crash injection: a real host process, really gone.
    ///
    /// The unit tests above inject liveness, which pins the rule but not the
    /// syscall. This spawns a process, waits for it to exit, and uses its pid —
    /// so the row is reaped because the OS says that pid is gone, and the live
    /// row survives because this test process really is running.
    #[cfg(unix)]
    #[test]
    fn a_crashed_host_is_detected_through_the_real_liveness_check() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("`true` spawns");
        let dead_pid = i64::from(child.id());
        child.wait().expect("`true` exits");

        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let crashed = hosted(
            &table,
            ProcessKind::WorkflowRun,
            "run-crashed",
            Some(dead_pid),
            ProcessState::Running,
        );
        let ours = hosted(
            &table,
            ProcessKind::WorkflowRun,
            "run-ours",
            Some(i64::from(std::process::id())),
            ProcessState::Running,
        );
        let node = hosted(
            &table,
            ProcessKind::WorkflowNode,
            "run-crashed:node-a",
            Some(dead_pid),
            ProcessState::Running,
        );

        let reaped = reap_processes_whose_host_died(&table, T0 + 5).unwrap();

        let reaped_ids: Vec<&str> = reaped.iter().map(|r| r.process_id.as_str()).collect();
        assert!(reaped_ids.contains(&crashed.process_id.as_str()));
        assert!(
            reaped_ids.contains(&node.process_id.as_str()),
            "a node stranded by the same crash was left running"
        );
        assert_eq!(
            table.get(&ours.process_id).unwrap().unwrap().state,
            ProcessState::Running,
            "the running test process was reported dead"
        );
        // Idempotent: the reaped rows are no longer live.
        assert!(reap_processes_whose_host_died(&table, T0 + 6)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_suspended_desktop_process_is_reaped_as_lost_rather_than_surviving_a_restart() {
        // K2's rule for paused + restart, made explicit rather than left to fall
        // out of `live_only`.
        //
        // Durable *intent* survives a restart; durable *execution* does not. A
        // paused chat turn's loop lives in the WebView, so once the app is gone
        // there is nothing left to resume — and a `suspended` row that outlived
        // its process would offer the user a Resume button for work that cannot
        // come back. That is the exact dishonesty this table exists to remove,
        // so the row is closed out as `lost` like any other abandoned process.
        // Restoring a live process across a restart is K13, not this.
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());

        let turn = admit(&table, ProcessKind::ChatTurn, "turn-paused");
        table
            .transition(&turn.process_id, ProcessState::Running, None, T0 + 1)
            .unwrap();
        table
            .signal(
                &turn.process_id,
                ProcessSignal::Suspend,
                Some("user"),
                T0 + 2,
            )
            .unwrap();
        table
            .transition(&turn.process_id, ProcessState::Suspended, None, T0 + 3)
            .unwrap();

        let reaped = table
            .reap_missing(
                &ProcessFilter {
                    kinds: ProcessKind::DESKTOP_OWNED.to_vec(),
                    ..ProcessFilter::default()
                },
                &[],
                "the app restarted while this process was still running",
                T0 + 9,
            )
            .unwrap();

        assert_eq!(
            reaped.len(),
            1,
            "a suspended row is still live work to reap"
        );
        let closed = table.get(&turn.process_id).unwrap().unwrap();
        assert_eq!(closed.state, ProcessState::Exited);
        assert_eq!(
            closed.exit.as_ref().map(|exit| exit.status),
            Some(ExitStatus::Lost),
            "a paused process that did not survive the restart is lost, not cancelled"
        );
        // The latch is left as it was: it records what was asked for, and the
        // exit records what happened. Rewriting history to hide the pause would
        // lose the reason this process stopped making progress.
        assert!(closed.signal_intent.suspend_requested);
    }

    #[test]
    fn reaping_never_touches_a_kind_the_caller_does_not_own() {
        // The desktop app reaps at startup. The resident daemon outlives it, so
        // a daemon job that is genuinely still running must survive — declaring
        // it lost would be a false terminal state on live work.
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());

        let turn = admit(&table, ProcessKind::ChatTurn, "turn-owned");
        let job = admit(&table, ProcessKind::DaemonJob, "job-not-owned");
        let workflow = admit(&table, ProcessKind::WorkflowRun, "wf-not-owned");
        for record in [&turn, &job, &workflow] {
            table
                .transition(&record.process_id, ProcessState::Running, None, T0 + 1)
                .unwrap();
        }

        let reaped = table
            .reap_missing(
                &ProcessFilter {
                    kinds: ProcessKind::DESKTOP_OWNED.to_vec(),
                    ..ProcessFilter::default()
                },
                &[],
                "app restarted",
                T0 + 5,
            )
            .unwrap();

        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].process_id, turn.process_id);
        assert_eq!(
            table.get(&job.process_id).unwrap().unwrap().state,
            ProcessState::Running,
            "a live daemon job was reaped by the desktop"
        );
        assert_eq!(
            table.get(&workflow.process_id).unwrap().unwrap().state,
            ProcessState::Running,
            "a workflow run that may be daemon-hosted was reaped by the desktop"
        );

        assert!(!ProcessKind::DESKTOP_OWNED.contains(&ProcessKind::DaemonJob));
        assert!(!ProcessKind::DESKTOP_OWNED.contains(&ProcessKind::RemoteRun));
    }

    #[test]
    fn pid_and_run_links_are_recorded_after_the_fact() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::DaemonJob, "job-pid");

        table
            .set_native_pid(&record.process_id, Some(4242), T0 + 1)
            .unwrap();
        assert_eq!(
            table.get(&record.process_id).unwrap().unwrap().native_pid,
            Some(4242)
        );

        assert!(matches!(
            table.set_native_pid("p-job-missing", Some(1), T0 + 1),
            Err(ProcessTableError::NotFound { .. })
        ));
        assert!(matches!(
            table.link_run("p-job-missing", "run-1", T0 + 1),
            Err(ProcessTableError::NotFound { .. })
        ));
    }

    #[test]
    fn limits_round_trip_and_zero_is_refused() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let limits = ProcessLimits {
            max_wall_ms: Some(60_000),
            max_memory_bytes: Some(2 * 1024 * 1024 * 1024),
            max_output_bytes: Some(1_048_576),
            max_child_processes: Some(8),
            max_context_tokens: Some(32_768),
        };
        let record = table
            .admit(
                &AdmitProcess::new(ProcessKind::DaemonJob, "job-limits").with_limits(limits),
                T0,
            )
            .unwrap();
        assert_eq!(record.limits, limits);
        assert!(!record.limits.is_unbounded());

        // A zero limit is a mistake, not "unlimited" — that is what None means.
        let zero = table.admit(
            &AdmitProcess::new(ProcessKind::DaemonJob, "job-zero").with_limits(ProcessLimits {
                max_wall_ms: Some(0),
                ..ProcessLimits::default()
            }),
            T0,
        );
        assert!(zero.is_err());

        // And a zero context budget for the sharper reason V17's `CHECK` names:
        // the chat template alone is tokens, so no request could ever satisfy it
        // — it would refuse every turn while reading like a configured limit.
        let zero_context = table.admit(
            &AdmitProcess::new(ProcessKind::DaemonJob, "job-zero-context").with_limits(
                ProcessLimits {
                    max_context_tokens: Some(0),
                    ..ProcessLimits::default()
                },
            ),
            T0,
        );
        assert!(zero_context.is_err());
    }

    #[test]
    fn an_empty_surface_id_or_a_bad_timestamp_is_refused() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        assert!(matches!(
            table.admit(&AdmitProcess::new(ProcessKind::ChatTurn, "  "), T0),
            Err(ProcessTableError::InvalidField {
                field: "external_id",
                ..
            })
        ));
        assert!(matches!(
            table.admit(&AdmitProcess::new(ProcessKind::ChatTurn, "turn-x"), 0),
            Err(ProcessTableError::InvalidField {
                field: "now_ms",
                ..
            })
        ));
    }

    #[test]
    fn kind_state_and_exit_parsing_reject_unknown_values() {
        assert!(matches!(
            ProcessKind::parse("not_a_kind"),
            Err(ProcessTableError::UnknownKind { .. })
        ));
        assert!(matches!(
            ProcessState::parse("zombie"),
            Err(ProcessTableError::UnknownState { .. })
        ));
        assert!(matches!(
            ExitStatus::parse("exploded"),
            Err(ProcessTableError::UnknownExitStatus { .. })
        ));

        for kind in ProcessKind::ALL {
            assert_eq!(ProcessKind::parse(kind.as_str()).unwrap(), *kind);
            assert!(!kind.tag().is_empty());
        }
    }

    #[test]
    fn reconcile_admits_once_then_moves_the_same_record() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());

        let (first, outcome) = table
            .reconcile(
                &ProcessProjection::new(ProcessKind::WorkflowRun, "wf-1", ProcessState::Running),
                T0,
            )
            .unwrap();
        assert_eq!(outcome, ReconcileOutcome::Admitted);
        assert_eq!(first.state, ProcessState::Running);

        // Same projection again: nothing to do, and no second record.
        let (again, outcome) = table
            .reconcile(
                &ProcessProjection::new(ProcessKind::WorkflowRun, "wf-1", ProcessState::Running),
                T0 + 1,
            )
            .unwrap();
        assert_eq!(outcome, ReconcileOutcome::Unchanged);
        assert_eq!(again.process_id, first.process_id);

        let (done, outcome) = table
            .reconcile(
                &ProcessProjection::exited(
                    ProcessKind::WorkflowRun,
                    "wf-1",
                    ProcessExit::succeeded(),
                ),
                T0 + 2,
            )
            .unwrap();
        assert_eq!(outcome, ReconcileOutcome::Transitioned);
        assert_eq!(done.process_id, first.process_id);
        assert_eq!(done.state, ProcessState::Exited);

        assert_eq!(table.list(&ProcessFilter::default()).unwrap().len(), 1);
    }

    #[test]
    fn reconcile_ignores_a_late_live_projection_instead_of_resurrecting_or_erroring() {
        // An adopter that reconciles on a timer will legitimately arrive after
        // the terminal write. That must not error (it would spam a log for a
        // benign race) and must not move the process back to running.
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());

        table
            .reconcile(
                &ProcessProjection::new(ProcessKind::DaemonJob, "job-late", ProcessState::Running),
                T0,
            )
            .unwrap();
        table
            .reconcile(
                &ProcessProjection::exited(
                    ProcessKind::DaemonJob,
                    "job-late",
                    ProcessExit::cancelled("stopped"),
                ),
                T0 + 1,
            )
            .unwrap();

        let (record, outcome) = table
            .reconcile(
                &ProcessProjection::new(ProcessKind::DaemonJob, "job-late", ProcessState::Running),
                T0 + 2,
            )
            .unwrap();
        assert_eq!(outcome, ReconcileOutcome::AlreadyExited);
        assert_eq!(record.state, ProcessState::Exited);
        assert_eq!(
            record.exit.as_ref().map(|exit| exit.status),
            Some(ExitStatus::Cancelled),
            "the original outcome must survive a late projection"
        );
    }

    #[test]
    fn reconcile_resolves_a_parent_by_surface_id_and_tolerates_a_missing_one() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());

        // Child first: the parent does not exist yet, so the edge is left unset
        // rather than the record being refused.
        let (orphan, _) = table
            .reconcile(
                &ProcessProjection::new(
                    ProcessKind::WorkflowNode,
                    "wf-2:node-a",
                    ProcessState::Admitted,
                )
                .with_parent(ProcessKind::WorkflowRun, "wf-2"),
                T0,
            )
            .unwrap();
        assert!(orphan.parent_process_id.is_none());

        let (parent, _) = table
            .reconcile(
                &ProcessProjection::new(ProcessKind::WorkflowRun, "wf-2", ProcessState::Running),
                T0 + 1,
            )
            .unwrap();

        // A node projected after its run gets the edge.
        let (child, _) = table
            .reconcile(
                &ProcessProjection::new(
                    ProcessKind::WorkflowNode,
                    "wf-2:node-b",
                    ProcessState::Running,
                )
                .with_parent(ProcessKind::WorkflowRun, "wf-2"),
                T0 + 2,
            )
            .unwrap();
        assert_eq!(
            child.parent_process_id.as_deref(),
            Some(parent.process_id.as_str())
        );
        assert_eq!(table.descendants(&parent.process_id).unwrap().len(), 1);
    }

    #[test]
    fn reconcile_refuses_a_terminal_projection_with_no_exit_and_the_reverse() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());

        let mut no_exit =
            ProcessProjection::new(ProcessKind::SideTask, "side-bad", ProcessState::Exited);
        no_exit.exit = None;
        assert!(matches!(
            table.reconcile(&no_exit, T0),
            Err(ProcessTableError::TerminalMismatch { .. })
        ));

        let mut live_with_exit =
            ProcessProjection::new(ProcessKind::SideTask, "side-bad-2", ProcessState::Running);
        live_with_exit.exit = Some(ProcessExit::succeeded());
        assert!(matches!(
            table.reconcile(&live_with_exit, T0),
            Err(ProcessTableError::TerminalMismatch { .. })
        ));
    }

    #[test]
    fn reconcile_fills_in_the_run_link_and_pid_once_they_are_known() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());

        let (record, _) = table
            .reconcile(
                &ProcessProjection::new(
                    ProcessKind::DaemonJob,
                    "job-late-pid",
                    ProcessState::Admitted,
                ),
                T0,
            )
            .unwrap();
        assert!(record.native_pid.is_none());

        let (updated, _) = table
            .reconcile(
                &ProcessProjection::new(
                    ProcessKind::DaemonJob,
                    "job-late-pid",
                    ProcessState::Running,
                )
                .with_native_pid(Some(4321)),
                T0 + 1,
            )
            .unwrap();
        assert_eq!(updated.native_pid, Some(4321));
    }

    /// The unattributed half of the destination ledger, which is the whole of
    /// this slice: a reason accumulates like a process does, its overflow is
    /// counted rather than dropped, and neither attribution can collide with the
    /// other.
    #[test]
    fn unattributed_destinations_accumulate_by_reason_and_never_collide_with_a_process() {
        use crate::run_scope::{Destination, DestinationDrain, Unattributed};

        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::BackgroundShell, "shell-dest");

        let destination = |host: &str| Destination {
            scheme: "https".to_string(),
            host: host.to_string(),
            port: 443,
        };
        let user_action = Unattributed::UserAction.code();

        table
            .add_unattributed_egress_destinations(
                user_action,
                &DestinationDrain {
                    seen: vec![(destination("updates.test"), 2)],
                    overflowed: 0,
                },
                T0,
            )
            .expect("the first flush lands");
        // A second flush is additive, and keeps the first sighting.
        table
            .add_unattributed_egress_destinations(
                user_action,
                &DestinationDrain {
                    seen: vec![(destination("updates.test"), 3)],
                    overflowed: 4,
                },
                T0 + 500,
            )
            .expect("the second flush lands");

        let stored = table
            .unattributed_egress_destinations()
            .expect("the reasons read back");
        let recorded = stored.get(user_action).expect("the reason is present");
        assert_eq!(recorded.destinations.len(), 1, "one row, not two");
        assert_eq!(recorded.destinations[0].requests, 5, "flushes are additive");
        assert_eq!(recorded.destinations[0].first_seen_ms, T0);
        assert_eq!(recorded.destinations[0].last_seen_ms, T0 + 500);
        assert_eq!(
            recorded.dropped, 4,
            "a truncated list that does not say it is truncated reads as a complete one"
        );

        // A different reason is a different list, not a merge.
        table
            .add_unattributed_egress_destinations(
                Unattributed::Startup.code(),
                &DestinationDrain {
                    seen: vec![(destination("updates.test"), 1)],
                    overflowed: 0,
                },
                T0 + 600,
            )
            .expect("a second reason lands");
        let stored = table
            .unattributed_egress_destinations()
            .expect("the reasons read back");
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[user_action].destinations[0].requests, 5);
        assert_eq!(
            stored[Unattributed::Startup.code()].destinations[0].requests,
            1
        );

        // And the same host under a *process* is a third row, untouched by either.
        table
            .add_egress_destinations(
                &record.process_id,
                &DestinationDrain {
                    seen: vec![(destination("updates.test"), 9)],
                    overflowed: 0,
                },
                T0 + 700,
            )
            .expect("the process flush lands");
        let attributed = table
            .egress_destinations_for(&[record.process_id.clone()])
            .expect("the process reads back");
        assert_eq!(attributed[&record.process_id].destinations[0].requests, 9);
        // The unattributed rows are exactly as they were: the `COALESCE` key must
        // separate a process from a reason, or one of these two counts would have
        // absorbed the other.
        let stored = table
            .unattributed_egress_destinations()
            .expect("the reasons read back");
        assert_eq!(stored[user_action].destinations[0].requests, 5);
        assert_eq!(
            stored[Unattributed::Startup.code()].destinations[0].requests,
            1
        );
        // …and the attributed reader still shows only process rows.
        assert_eq!(attributed[&record.process_id].destinations.len(), 1);
    }

    /// A reason that overflowed but named nothing must still be visible. It is the
    /// case a reader would most easily mistake for "reached nowhere".
    #[test]
    fn a_reason_that_only_overflowed_is_still_reported() {
        use crate::run_scope::{DestinationDrain, Unattributed};

        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        table
            .add_unattributed_egress_destinations(
                Unattributed::Scheduled.code(),
                &DestinationDrain {
                    seen: Vec::new(),
                    overflowed: 6,
                },
                T0,
            )
            .expect("an overflow-only flush lands");

        let stored = table
            .unattributed_egress_destinations()
            .expect("the reasons read back");
        let recorded = stored
            .get(Unattributed::Scheduled.code())
            .expect("a reason with only an overflow is still a reason that reached somewhere");
        assert!(recorded.destinations.is_empty());
        assert_eq!(recorded.dropped, 6);
    }

    /// Names the code that creates records for each kind.
    ///
    /// Exhaustive by construction: adding a [`ProcessKind`] variant fails to
    /// compile until its adopter is named here. That is the closest this can get
    /// to the acceptance criterion "a run that exists without a process record
    /// is a bug" — it cannot catch a *new execution path* that reuses an
    /// existing kind and forgets to project, which is stated as a remaining gap
    /// in `docs/agent-os-roadmap.md` rather than pretended away.
    fn adopter_for(kind: ProcessKind) -> &'static str {
        match kind {
            ProcessKind::ChatTurn => "src/lib/agentLoop.ts — runAgentTurn",
            ProcessKind::DaemonJob => {
                "bin/monkey-cli/daemon/engine.rs — sync_process_table, once per tick"
            }
            ProcessKind::Subagent => "src/lib/subagent.ts — runSubagentTask",
            ProcessKind::CrewMember => "src/lib/crewRunner.ts — initializeActorRecorders",
            ProcessKind::WorkflowRun => "m4_services.rs — WorkflowService::append_history",
            ProcessKind::WorkflowNode => "m4_services.rs — WorkflowService::append_history",
            ProcessKind::RemoteRun => "bin/monkey-cli/daemon/mod.rs — project_queue_origin",
            ProcessKind::BackgroundShell => "background_shell.rs — emit_status",
            ProcessKind::ForegroundShell => {
                "workspace_shell.rs — GuardedShell::spawn, for every foreground shell and \
                 verify command on either client"
            }
            ProcessKind::SideTask => "src/lib/sideTaskRunner.ts — runSideTask",
            ProcessKind::BrowserSession => "browser_worker.rs — OwnedBrowser::project",
            ProcessKind::VerifyCommand => {
                "verify.rs — run_command_impl, through bounded_execution::BoundedExecution"
            }
            ProcessKind::HookCommand => {
                "hooks.rs — hook_exec, through bounded_execution::BoundedExecution"
            }
            ProcessKind::SandboxRun => {
                "sandbox.rs — execute_in_sandbox, through bounded_execution::BoundedExecution"
            }
        }
    }

    #[test]
    fn stop_is_the_one_signal_every_kind_honours() {
        // Every kind has a cancellation path, so nothing may refuse `stop`.
        for kind in ProcessKind::ALL {
            assert!(
                kind.signal_support(ProcessSignal::Stop).is_honoured(),
                "{} refuses stop, which no kind may do",
                kind.as_str()
            );
            assert!(kind.honoured_signals().contains(&ProcessSignal::Stop));
        }
    }

    #[test]
    fn kill_is_refused_where_no_os_process_is_owned() {
        // `Kill` promises immediate termination. Silently downgrading it to a
        // cooperative `Stop` would answer a different question than the caller
        // asked.
        for kind in [
            ProcessKind::ChatTurn,
            ProcessKind::Subagent,
            ProcessKind::CrewMember,
            ProcessKind::SideTask,
            ProcessKind::WorkflowRun,
            ProcessKind::WorkflowNode,
        ] {
            let refusal = kind
                .signal_support(ProcessSignal::Kill)
                .refusal()
                .unwrap_or_else(|| panic!("{} must refuse kill", kind.as_str()));
            assert!(refusal.contains("stop it instead"), "{refusal}");
        }

        // `RemoteRun` refuses for a different reason, and says so: it owns no
        // process because it is not one. Its refusal has to point at the child
        // that is, or a caller learns only that the answer is no.
        let refusal = ProcessKind::RemoteRun
            .signal_support(ProcessSignal::Kill)
            .refusal()
            .expect("a remote run owns no process to kill");
        assert!(refusal.contains("daemon job"), "{refusal}");

        for kind in [ProcessKind::DaemonJob, ProcessKind::BackgroundShell] {
            assert!(kind.signal_support(ProcessSignal::Kill).is_honoured());
        }
    }

    #[test]
    fn suspend_is_honoured_only_where_a_pause_mechanism_actually_exists() {
        // The finding this pins: suspend/resume are honoured everywhere a real
        // mechanism exists — real OS suspend (`DaemonJob`, `BackgroundShell`),
        // a cooperative durable latch a loop checks at its own safe point
        // (`ChatTurn`, `Subagent`, `CrewMember`, `SideTask`), or a blocking
        // wait at a coarse boundary (`WorkflowRun`) — and nowhere else.
        //
        // Two holdouts, each pointing at the target that does work.
        // `WorkflowNode`: nothing ever signals a node's own process id.
        // `RemoteRun`: it records the request rather than the work, and
        // regressed into claiming `Honoured` while no delivery path for the
        // kind existed anywhere — not in the daemon's `apply_signal_intent`,
        // which reads only `DaemonJob`, and not in the desktop fan-out.
        for kind in [
            ProcessKind::DaemonJob,
            ProcessKind::SideTask,
            ProcessKind::BackgroundShell,
            ProcessKind::ChatTurn,
            ProcessKind::Subagent,
            ProcessKind::CrewMember,
            ProcessKind::WorkflowRun,
        ] {
            for signal in [ProcessSignal::Suspend, ProcessSignal::Resume] {
                assert!(
                    kind.signal_support(signal).is_honoured(),
                    "{} should honour {}",
                    kind.as_str(),
                    signal.as_str()
                );
            }
        }
        for (kind, target) in [
            (ProcessKind::WorkflowNode, "owning workflow run"),
            (ProcessKind::RemoteRun, "daemon job"),
        ] {
            for signal in [ProcessSignal::Suspend, ProcessSignal::Resume] {
                let refusal = kind.signal_support(signal).refusal().unwrap_or_else(|| {
                    panic!("{} claims to honour {}", kind.as_str(), signal.as_str())
                });
                assert!(
                    !refusal.is_empty() && refusal.len() > 20,
                    "{} gives a useless refusal for {}: {refusal}",
                    kind.as_str(),
                    signal.as_str()
                );
                assert!(
                    refusal.contains(target),
                    "{}'s refusal should name the correct target ({target}), got: {refusal}",
                    kind.as_str()
                );
            }
        }
    }

    #[test]
    fn a_signal_is_recorded_durably_and_survives_being_read_back() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::DaemonJob, "job-signal");
        assert!(record.signal_intent.is_clear());

        let signalled = table
            .signal(
                &record.process_id,
                ProcessSignal::Stop,
                Some("user pressed stop"),
                T0 + 1,
            )
            .unwrap();
        assert!(signalled.signal_intent.stop_requested);
        assert!(!signalled.signal_intent.suspend_requested);
        assert_eq!(
            signalled.signal_reason.as_deref(),
            Some("user pressed stop")
        );
        assert_eq!(signalled.signal_requested_at_ms, Some(T0 + 1));

        // Read back through a fresh view — the whole point is that it is on disk,
        // not in a live handle that dies with the process that made it.
        let reread = ProcessTable::new(ledger.connection())
            .get(&record.process_id)
            .unwrap()
            .unwrap();
        assert!(reread.signal_intent.stop_requested);
    }

    #[test]
    fn resume_clears_a_suspend_but_never_cancels_a_pending_stop() {
        // The race that matters: a process is suspended, then asked to stop, then
        // a stale resume arrives. If resume cleared the stop latch, "stop this"
        // would silently become "keep going".
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::DaemonJob, "job-race");

        table
            .signal(&record.process_id, ProcessSignal::Suspend, None, T0 + 1)
            .unwrap();
        let stopped = table
            .signal(&record.process_id, ProcessSignal::Stop, None, T0 + 2)
            .unwrap();
        assert!(stopped.signal_intent.stop_requested);
        assert!(
            stopped.signal_intent.suspend_requested,
            "a stop must not erase the record that it was suspended"
        );

        let resumed = table
            .signal(&record.process_id, ProcessSignal::Resume, None, T0 + 3)
            .unwrap();
        assert!(!resumed.signal_intent.suspend_requested);
        assert!(
            resumed.signal_intent.stop_requested,
            "resume cancelled a pending stop — the process would keep running"
        );
    }

    #[test]
    fn a_refused_signal_is_an_error_carrying_the_reason_not_a_silent_no_op() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::WorkflowNode, "node-refuse");

        let error = table
            .signal(&record.process_id, ProcessSignal::Suspend, None, T0 + 1)
            .expect_err("a workflow node cannot be suspended independently");
        match error {
            ProcessTableError::SignalRefused {
                kind,
                signal,
                reason,
                ..
            } => {
                assert_eq!(kind, ProcessKind::WorkflowNode);
                assert_eq!(signal, ProcessSignal::Suspend);
                assert!(reason.contains("owning workflow run"), "{reason}");
            }
            other => panic!("wrong error: {other}"),
        }

        // And nothing was written — a refusal must not leave half a request.
        let unchanged = table.get(&record.process_id).unwrap().unwrap();
        assert!(unchanged.signal_intent.is_clear());
        assert!(unchanged.signal_reason.is_none());
    }

    #[test]
    fn signalling_a_process_that_already_exited_is_refused() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::DaemonJob, "job-gone");
        table
            .transition(&record.process_id, ProcessState::Running, None, T0 + 1)
            .unwrap();
        table
            .transition(
                &record.process_id,
                ProcessState::Exited,
                Some(ProcessExit::succeeded()),
                T0 + 2,
            )
            .unwrap();

        assert!(matches!(
            table.signal(&record.process_id, ProcessSignal::Stop, None, T0 + 3),
            Err(ProcessTableError::AlreadyExited { .. })
        ));
    }

    #[test]
    fn pending_signals_finds_work_left_unstopped_by_a_previous_session() {
        // What a supervisor reads after a restart, and what a worker in another
        // process polls at its safe point.
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());

        let quiet = admit(&table, ProcessKind::DaemonJob, "job-quiet");
        let asked = admit(&table, ProcessKind::DaemonJob, "job-asked");
        let other_kind = admit(&table, ProcessKind::SideTask, "side-asked");
        for record in [&quiet, &asked, &other_kind] {
            table
                .transition(&record.process_id, ProcessState::Running, None, T0 + 1)
                .unwrap();
        }
        table
            .signal(
                &asked.process_id,
                ProcessSignal::Stop,
                Some("shutdown"),
                T0 + 2,
            )
            .unwrap();
        table
            .signal(&other_kind.process_id, ProcessSignal::Suspend, None, T0 + 2)
            .unwrap();

        let daemon_pending = table.pending_signals(&[ProcessKind::DaemonJob]).unwrap();
        assert_eq!(daemon_pending.len(), 1);
        assert_eq!(daemon_pending[0].process_id, asked.process_id);

        let all_pending = table.pending_signals(&[]).unwrap();
        assert_eq!(
            all_pending.len(),
            2,
            "an empty kind filter means every kind"
        );

        // An exited process is never pending, however it was signalled.
        table
            .transition(
                &asked.process_id,
                ProcessState::Exited,
                Some(ProcessExit::cancelled("stopped")),
                T0 + 3,
            )
            .unwrap();
        assert_eq!(
            table
                .pending_signals(&[ProcessKind::DaemonJob])
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn kill_is_distinguishable_from_stop_in_the_latch() {
        // The gap this closed: both used to set only `stop_requested`, so once
        // written, a supervisor could not tell "wind down cleanly" from
        // "terminate now" — the difference survived only in a free-text reason.
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());

        let stopped = admit(&table, ProcessKind::DaemonJob, "job-stop");
        let killed = admit(&table, ProcessKind::DaemonJob, "job-kill");

        let stopped = table
            .signal(&stopped.process_id, ProcessSignal::Stop, None, T0 + 1)
            .unwrap();
        let killed = table
            .signal(&killed.process_id, ProcessSignal::Kill, None, T0 + 1)
            .unwrap();

        assert!(stopped.signal_intent.stop_requested);
        assert!(!stopped.signal_intent.kill_requested);
        assert!(!stopped.signal_intent.wants_immediate_termination());

        // A kill sets both: it IS a stop, so every reader that only checks
        // `stop_requested` stays correct without knowing kill exists.
        assert!(killed.signal_intent.stop_requested);
        assert!(killed.signal_intent.kill_requested);
        assert!(killed.signal_intent.wants_immediate_termination());
    }

    #[test]
    fn a_kill_is_never_downgraded_by_a_later_weaker_signal() {
        // Escalation is one-way. A caller who asked for a guaranteed
        // termination does not get un-escalated because something later asked
        // for a polite one, and `resume` must not clear either.
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::DaemonJob, "job-escalated");

        table
            .signal(
                &record.process_id,
                ProcessSignal::Kill,
                Some("hung"),
                T0 + 1,
            )
            .unwrap();
        let after_stop = table
            .signal(
                &record.process_id,
                ProcessSignal::Stop,
                Some("polite"),
                T0 + 2,
            )
            .unwrap();
        assert!(
            after_stop.signal_intent.kill_requested,
            "a stop must not downgrade a kill that was already asked for"
        );

        let after_resume = table
            .signal(&record.process_id, ProcessSignal::Resume, None, T0 + 3)
            .unwrap();
        assert!(after_resume.signal_intent.kill_requested);
        assert!(after_resume.signal_intent.stop_requested);
    }

    #[test]
    fn a_killed_process_is_still_pending_for_a_reader_that_only_knows_stop() {
        // The migration's cheapness rests on this: the pending-signal index and
        // predicate were never rebuilt for `kill_requested`, which is only sound
        // because a kill always carries a stop.
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::DaemonJob, "job-killed-pending");
        table
            .transition(&record.process_id, ProcessState::Running, None, T0 + 1)
            .unwrap();
        table
            .signal(&record.process_id, ProcessSignal::Kill, None, T0 + 2)
            .unwrap();

        let pending = table.pending_signals(&[ProcessKind::DaemonJob]).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].signal_intent.kill_requested);
    }

    #[test]
    fn the_ledger_refuses_a_kill_latch_without_its_stop() {
        // The SQL trigger holds the invariant against a direct write, the same
        // way the state machine's own trigger does — a companion store reaching
        // this connection cannot record a kill that no `stop_requested` reader
        // would ever see.
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let record = admit(&table, ProcessKind::DaemonJob, "job-direct-write");

        let result = ledger.connection().execute(
            "UPDATE agent_processes SET kill_requested = 1 WHERE process_id = ?1",
            [&record.process_id],
        );
        assert!(
            result.is_err(),
            "a kill latch without a stop latch must be refused at the SQL layer"
        );
    }

    #[test]
    fn restart_is_declared_per_kind_and_only_where_a_supervisor_exists() {
        // The honest shape: exactly one kind can be restarted, because exactly
        // one has a supervisor that outlives the process plus a durable
        // description of the work. Every other kind says `Never` for a stated
        // reason rather than by omission.
        assert!(matches!(
            ProcessKind::DaemonJob.restart_policy(),
            RestartPolicy::OnFailure { .. }
        ));
        for kind in [
            ProcessKind::ChatTurn,
            ProcessKind::Subagent,
            ProcessKind::CrewMember,
            ProcessKind::BackgroundShell,
            ProcessKind::SideTask,
            ProcessKind::WorkflowRun,
            ProcessKind::WorkflowNode,
            ProcessKind::RemoteRun,
        ] {
            assert_eq!(
                kind.restart_policy(),
                RestartPolicy::Never,
                "{kind:?} claims a restart policy nothing implements"
            );
        }
    }

    #[test]
    fn a_restart_policy_is_bounded_and_backs_off() {
        let policy = RestartPolicy::OnFailure {
            max_attempts: 3,
            base_backoff_ms: 1_000,
        };
        // `attempt` counts attempts already spent, so 3 permits two retries.
        assert!(policy.permits_retry(0));
        assert!(policy.permits_retry(1));
        assert!(!policy.permits_retry(2), "the ceiling is a ceiling");
        assert!(
            !policy.permits_retry(u32::MAX),
            "no overflow past the bound"
        );

        assert_eq!(policy.backoff_ms(0), 1_000);
        assert_eq!(policy.backoff_ms(1), 2_000);
        assert_eq!(policy.backoff_ms(2), 4_000);
        // Capped, so a late attempt cannot park a job for hours.
        assert_eq!(policy.backoff_ms(30), 60_000);

        assert!(!RestartPolicy::Never.permits_retry(0));
        assert_eq!(RestartPolicy::Never.backoff_ms(5), 0);
    }

    #[test]
    fn a_crash_closes_out_every_desktop_surface_rather_than_leaving_it_running() {
        // Crash injection for the desktop-owned surfaces. The invariant is one
        // sentence: after an abrupt death, nothing still claims to be running.
        // A row stuck at `running` is indistinguishable from live work to every
        // reader — the listing, a future scheduler, and the user deciding
        // whether to start more.
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());

        let mut admitted = Vec::new();
        for (index, kind) in ProcessKind::DESKTOP_OWNED.iter().enumerate() {
            let record = admit(&table, *kind, &format!("external-{index}"));
            table
                .transition(&record.process_id, ProcessState::Running, None, T0 + 1)
                .unwrap();
            admitted.push(record);
        }
        // One of them was mid-pause when the app died, which must not change
        // the answer.
        table
            .transition(
                &admitted[0].process_id,
                ProcessState::Suspended,
                None,
                T0 + 2,
            )
            .unwrap();

        // The crash: nothing this instance owned is accounted for, because its
        // workers died with the process.
        let reaped = table
            .reap_missing(
                &ProcessFilter {
                    kinds: ProcessKind::DESKTOP_OWNED.to_vec(),
                    ..ProcessFilter::default()
                },
                &[],
                "the app restarted while this process was still running",
                T0 + 9,
            )
            .unwrap();

        assert_eq!(reaped.len(), ProcessKind::DESKTOP_OWNED.len());
        for record in &admitted {
            let closed = table.get(&record.process_id).unwrap().unwrap();
            assert_eq!(
                closed.state,
                ProcessState::Exited,
                "{:?} was left claiming to be live after a crash",
                closed.kind
            );
            assert_eq!(
                closed.exit.as_ref().map(|exit| exit.status),
                Some(ExitStatus::Lost),
                "an abandoned process is lost — not succeeded, and not cancelled \
                 as though someone asked for it"
            );
        }

        // And the sweep is idempotent: a second startup finds nothing to do
        // rather than rewriting terminal rows.
        assert!(table
            .reap_missing(
                &ProcessFilter {
                    kinds: ProcessKind::DESKTOP_OWNED.to_vec(),
                    ..ProcessFilter::default()
                },
                &[],
                "second startup",
                T0 + 10,
            )
            .unwrap()
            .is_empty());
    }

    #[test]
    fn signal_parsing_rejects_unknown_values_and_round_trips_every_variant() {
        assert!(matches!(
            ProcessSignal::parse("detonate"),
            Err(ProcessTableError::UnknownSignal { .. })
        ));
        for signal in ProcessSignal::ALL {
            assert_eq!(ProcessSignal::parse(signal.as_str()).unwrap(), *signal);
        }
        assert_eq!(ProcessSignal::ALL.len(), 4);
    }

    #[test]
    fn every_kind_has_a_named_adopter() {
        for kind in ProcessKind::ALL {
            let adopter = adopter_for(*kind);
            assert!(
                !adopter.is_empty(),
                "{} has no adopter named",
                kind.as_str()
            );
        }
        // `ALL` must itself be exhaustive, or the loop above proves nothing. The
        // count is the cheap half; `the_kind_list_and_the_stored_vocabulary_name_
        // the_same_kinds` is the half that says *which* kind is missing.
        assert_eq!(
            ProcessKind::ALL.len(),
            14,
            "ProcessKind::ALL is out of sync with the enum"
        );
    }

    #[test]
    fn the_transition_table_matches_the_documented_state_machine() {
        use ProcessState::*;
        let legal = [
            (Admitted, Running),
            (Admitted, Exited),
            (Running, Suspended),
            (Running, Exited),
            (Suspended, Running),
            (Suspended, Exited),
        ];
        for from in [Admitted, Running, Suspended, Exited] {
            for to in [Admitted, Running, Suspended, Exited] {
                let expected = from == to || legal.contains(&(from, to));
                assert_eq!(
                    from.can_transition_to(to),
                    expected,
                    "{} -> {} should be {}",
                    from.as_str(),
                    to.as_str(),
                    if expected { "legal" } else { "refused" }
                );
            }
        }
        assert!(Exited.is_terminal());
        assert!(!Running.is_terminal());
    }

    fn temp_ledger_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "lm-signal-source-test-{label}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    /// Every (kind, limit) pair answers, and every answer says something.
    ///
    /// The matrix is the whole of K4's declaration contract, so an arm added by
    /// a catch-all — or a reason left empty — would silently reintroduce the
    /// defect: a field recorded with nobody behind it.
    #[test]
    fn every_kind_and_limit_pair_is_answered() {
        for kind in ProcessKind::ALL {
            for limit in ProcessLimitKind::ALL {
                let support = kind.limit_support(*limit);
                assert!(
                    support.detail().len() > 20,
                    "{}/{} answers without saying who or why: {support:?}",
                    kind.as_str(),
                    limit.as_str()
                );
                assert!(
                    !support.detail().contains("unsupported"),
                    "{}/{} says \"unsupported\" instead of naming the missing mechanism",
                    kind.as_str(),
                    limit.as_str()
                );
            }
        }
    }

    /// A class default may not set a limit the matrix says nobody enforces.
    ///
    /// This is the contradiction that would make the matrix a comment rather
    /// than a contract: `default_limits` writes the row, `limit_support`
    /// describes it, and the two disagreeing means one of them is lying to a
    /// reader who cannot tell which.
    #[test]
    fn no_class_default_declares_a_limit_nobody_enforces() {
        for kind in ProcessKind::ALL {
            let class = kind.default_limits();
            let declared: [(ProcessLimitKind, bool); 5] = [
                (ProcessLimitKind::Wall, class.max_wall_ms.is_some()),
                (ProcessLimitKind::Memory, class.max_memory_bytes.is_some()),
                (ProcessLimitKind::Output, class.max_output_bytes.is_some()),
                (
                    ProcessLimitKind::ChildProcesses,
                    class.max_child_processes.is_some(),
                ),
                (
                    ProcessLimitKind::ContextTokens,
                    class.max_context_tokens.is_some(),
                ),
            ];
            for (limit, is_set) in declared {
                if !is_set {
                    continue;
                }
                assert!(
                    !matches!(kind.limit_support(limit), LimitEnforcement::Unavailable(_)),
                    "{} declares {} by class default while the matrix says nobody enforces it",
                    kind.as_str(),
                    limit.as_str()
                );
            }
        }
    }

    /// The one field a caller may still set on a desktop kind, and the one that
    /// made the old behaviour wrong.
    #[test]
    fn only_a_field_its_owner_reads_honours_a_caller_value() {
        // The WebView wall budget: swept off the row, so Settings can raise it.
        assert!(ProcessKind::ChatTurn
            .limit_support(ProcessLimitKind::Wall)
            .honours_caller_value());
        // A memory ceiling on a chat turn was accepted before this and consulted
        // by nothing on any platform.
        assert!(!ProcessKind::ChatTurn
            .limit_support(ProcessLimitKind::Memory)
            .honours_caller_value());
        // The daemon enforces memory for real — but from the job's own recipe,
        // so a caller value would be overwritten on the next projection.
        assert!(!ProcessKind::DaemonJob
            .limit_support(ProcessLimitKind::Memory)
            .honours_caller_value());
        // A per-tree process count is held exactly where a resource controller
        // owns a native tree, and nowhere else. Both halves are asserted: the
        // first is the capability this item added, and the second is what stops
        // it from being claimed for a kind that owns no OS process at all.
        // The kinds routed through a resource controller. That set is now exactly
        // "owns a native process tree": the browser session was the one member of
        // the second group and not the first, bounded by `browser_worker`'s own
        // session quotas, and it is routed. What the quotas kept is what no
        // controller can express — the session clock, the action budget, the disk
        // budget — so the two authorities still hold disjoint resources.
        for kind in ProcessKind::ALL {
            let owns_a_tree = matches!(
                kind,
                ProcessKind::BackgroundShell
                    | ProcessKind::ForegroundShell
                    | ProcessKind::BrowserSession
                    | ProcessKind::VerifyCommand
                    | ProcessKind::HookCommand
                    | ProcessKind::SandboxRun
            );
            assert_eq!(
                kind.limit_support(ProcessLimitKind::ChildProcesses)
                    .honours_caller_value(),
                owns_a_tree,
                "{} disagrees with whether it owns a process tree a count can bound",
                kind.as_str()
            );
            assert_eq!(
                kind.limit_support(ProcessLimitKind::Memory)
                    .honours_caller_value(),
                owns_a_tree,
                "{} disagrees with whether it owns a process tree memory can be summed over",
                kind.as_str()
            );
        }
    }

    #[test]
    fn signal_source_reports_none_for_an_unknown_process() {
        let source = LedgerSignalSource::new(temp_ledger_path("unknown"));
        assert!(source
            .signal_intent(ProcessKind::ChatTurn, "no-such-turn")
            .is_none());
    }

    #[test]
    fn signal_source_reflects_suspend_and_resume_written_through_the_same_ledger() {
        let path = temp_ledger_path("roundtrip");
        let source = LedgerSignalSource::new(&path);

        // Admit and signal through a direct `ProcessTable` handle on the same
        // path, mirroring how the executor (reading) and the desktop/CLI/daemon
        // (writing via `ProcessTable::signal`) are different processes sharing
        // one file.
        let ledger = RunLedger::open(&path).expect("ledger opens at the same path");
        let table = ledger.process_table();
        let record = admit(&table, ProcessKind::WorkflowRun, "wf-run-1");
        assert_eq!(
            source
                .signal_intent(ProcessKind::WorkflowRun, "wf-run-1")
                .expect("row exists once admitted"),
            SignalIntent::default(),
            "a freshly admitted process has no latched intent"
        );

        table
            .signal(&record.process_id, ProcessSignal::Suspend, None, T0)
            .expect("suspend is honoured for a workflow run");
        assert!(
            source
                .signal_intent(ProcessKind::WorkflowRun, "wf-run-1")
                .expect("row still exists")
                .suspend_requested,
            "the read port should see the suspend latch written by another handle"
        );

        table
            .signal(&record.process_id, ProcessSignal::Resume, None, T0)
            .expect("resume is honoured for a workflow run");
        assert!(
            !source
                .signal_intent(ProcessKind::WorkflowRun, "wf-run-1")
                .expect("row still exists")
                .suspend_requested,
            "resume should clear the latch as seen through the read port too"
        );
    }
}
