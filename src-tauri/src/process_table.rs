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

use std::fmt;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

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
    /// A side task running beside the main conversation.
    SideTask,
}

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
            ProcessKind::SideTask => "side_task",
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
            ProcessKind::SideTask => "side",
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
            "side_task" => ProcessKind::SideTask,
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
    /// trigger — even though a desktop-started one does die with the app. That
    /// costs a stale row on the rarer path and never a wrong reap on the
    /// dangerous one.
    pub const DESKTOP_OWNED: &'static [ProcessKind] = &[
        ProcessKind::ChatTurn,
        ProcessKind::Subagent,
        ProcessKind::CrewMember,
        ProcessKind::BackgroundShell,
        ProcessKind::SideTask,
    ];

    /// Whether this kind honours `signal`, and if not, why.
    ///
    /// This is a statement about what the code does *today*, not an aspiration.
    /// The shape of it is the finding: `suspend`/`resume` exist in exactly two
    /// places — the daemon (real OS suspend of a child it owns) and side tasks
    /// (a cooperative `paused` status its loop already checks) — and nowhere
    /// else. `kill` is only meaningful where this app owns an OS process.
    ///
    /// Each refusal names the mechanism that is missing rather than saying
    /// "unsupported", because the caller's next question is always "why not",
    /// and for several of these the answer is a design boundary rather than an
    /// unwritten feature: a chat turn's loop lives in the WebView, so suspending
    /// it and surviving a restart are different problems (see K13 in
    /// `docs/agent-os-roadmap.md`).
    pub fn signal_support(self, signal: ProcessSignal) -> SignalSupport {
        use ProcessKind as K;
        use ProcessSignal as S;
        match (self, signal) {
            // Stop is universal: every kind has a cancellation path.
            (_, S::Stop) => SignalSupport::Honoured,

            // Kill needs an OS process this app owns.
            (K::DaemonJob | K::BackgroundShell, S::Kill) => SignalSupport::Honoured,
            (K::RemoteRun, S::Kill) => SignalSupport::Honoured,
            (_, S::Kill) => SignalSupport::Refused(
                "this kind owns no OS process to terminate; stop it instead, which winds it \
                 down at its next safe point",
            ),

            // Suspend/resume: real where we own a child, cooperative where the
            // loop already checks a paused state.
            (K::DaemonJob | K::RemoteRun, S::Suspend | S::Resume) => SignalSupport::Honoured,
            (K::SideTask, S::Suspend | S::Resume) => SignalSupport::Honoured,
            (K::BackgroundShell, S::Suspend | S::Resume) => SignalSupport::Refused(
                "the child process is owned but not yet suspended by signal; killing it is the \
                 only stop available today",
            ),
            (K::ChatTurn | K::Subagent | K::CrewMember, S::Suspend | S::Resume) => {
                SignalSupport::Refused(
                    "this loop has no pause point yet: it would have to yield at a round \
                     boundary, and a paused loop cannot survive an app restart because it lives \
                     in the WebView",
                )
            }
            (K::WorkflowRun | K::WorkflowNode, S::Suspend | S::Resume) => SignalSupport::Refused(
                "the workflow executor observes cancellation at level boundaries but has no \
                 pause state; resuming would mean replaying from the last completed level",
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
        ProcessKind::SideTask,
    ];
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
            other => {
                return Err(ProcessTableError::UnknownExitStatus {
                    status: other.to_string(),
                })
            }
        })
    }
}

/// The limit set attached to a process.
///
/// `None` means "not bounded by this process record" — honest, and different
/// from zero. Nothing in this module enforces these; they are the declaration a
/// scheduler and the platform enforcement in K4 read. Recording them here does
/// not make them enforced, and the field docs say so rather than implying a
/// guarantee that does not exist yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessLimits {
    /// Wall-clock budget. The daemon already enforces its own equivalent
    /// (`max_runtime_ms`); no other kind enforces anything.
    pub max_wall_ms: Option<u64>,
    /// Resident memory ceiling. Declared only; no platform mechanism reads it
    /// yet (see K4 — there is no `setrlimit`, cgroup or job object anywhere in
    /// this app today).
    pub max_memory_bytes: Option<u64>,
    /// Captured output ceiling.
    pub max_output_bytes: Option<u64>,
    /// Child-process ceiling.
    pub max_child_processes: Option<u32>,
}

impl ProcessLimits {
    pub fn is_unbounded(&self) -> bool {
        *self == ProcessLimits::default()
    }
}

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
}

impl ProcessExit {
    pub fn succeeded() -> Self {
        ProcessExit {
            status: ExitStatus::Succeeded,
            code: None,
            signal: None,
            reason: None,
        }
    }

    pub fn failed(reason: impl Into<String>) -> Self {
        ProcessExit {
            status: ExitStatus::Failed,
            code: None,
            signal: None,
            reason: Some(reason.into()),
        }
    }

    pub fn cancelled(reason: impl Into<String>) -> Self {
        ProcessExit {
            status: ExitStatus::Cancelled,
            code: None,
            signal: None,
            reason: Some(reason.into()),
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
    pub limits: ProcessLimits,
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
            limits: ProcessLimits::default(),
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

/// A durable request for a signal, recorded on the process record.
///
/// Two independent latches rather than one "requested signal" field: a stop and
/// a suspend are not alternatives. A process can be suspended and then asked to
/// stop, and the stop must win without the suspend intent being lost from the
/// audit trail. `Resume` clears the suspend latch; `Kill` sets the stop latch —
/// the distinction between them lives in how the kind delivers it, not in what
/// is recorded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalIntent {
    pub stop_requested: bool,
    pub suspend_requested: bool,
}

impl SignalIntent {
    pub fn is_clear(&self) -> bool {
        !self.stop_requested && !self.suspend_requested
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
    pub limits: ProcessLimits,
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
            limits: ProcessLimits::default(),
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

    pub fn with_limits(mut self, limits: ProcessLimits) -> Self {
        self.limits = limits;
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

/// A sink for [`ProcessProjection`]s.
///
/// A port, not a ledger handle. Services that must not depend on storage —
/// `WorkflowService` keeps its history in a JSON file store and is deliberately
/// database-agnostic — depend on this instead, so their unit tests use a
/// recording fake rather than standing up SQLite, and every caller of theirs
/// (desktop, CLI, daemon-triggered) gets the projection from one place.
///
/// Implementations must be fail-soft at their own boundary if the caller cannot
/// tolerate an error; `project` returns one so a caller that *can* report it
/// has the option.
pub trait ProcessProjector: Send + Sync {
    fn project(&self, projection: &ProcessProjection) -> Result<(), String>;
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
    format!(
        "p-{}-{}",
        kind.tag(),
        uuid::Uuid::new_v4().simple()
    )
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
                exit_status, exit_code, exit_signal, exit_reason,
                created_at_ms, updated_at_ms, started_at_ms, exited_at_ms
             ) VALUES (
                ?1, ?2, ?3, ?4, 'admitted', ?5,
                ?6, ?7, NULL,
                ?8, ?9, ?10, ?11,
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
            ],
        )?;

        self.get(&process_id)?
            .ok_or_else(|| ProcessTableError::NotFound {
                process_id: process_id.clone(),
            })
    }

    /// Move a process to `next`. Refuses an illegal transition rather than
    /// applying it, and requires exit detail exactly when moving to `Exited`.
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

        self.connection.execute(
            "UPDATE agent_processes
                SET state = ?2,
                    exit_status = ?3,
                    exit_code = ?4,
                    exit_signal = ?5,
                    exit_reason = ?6,
                    updated_at_ms = ?7,
                    started_at_ms = ?8,
                    exited_at_ms = ?9
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
            ],
        )?;

        self.get(process_id)?
            .ok_or_else(|| ProcessTableError::NotFound {
                process_id: process_id.to_string(),
            })
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

        let (stop, suspend) = match signal {
            // Kill and Stop both record "stop wanted". They differ in delivery
            // urgency, which is the kind's concern, not this table's.
            ProcessSignal::Stop | ProcessSignal::Kill => (true, record.signal_intent.suspend_requested),
            ProcessSignal::Suspend => (record.signal_intent.stop_requested, true),
            ProcessSignal::Resume => (record.signal_intent.stop_requested, false),
        };

        self.connection.execute(
            "UPDATE agent_processes
                SET stop_requested = ?2,
                    suspend_requested = ?3,
                    signal_reason = ?4,
                    signal_requested_at_ms = ?5,
                    updated_at_ms = ?5
              WHERE process_id = ?1",
            params![
                process_id,
                i64::from(stop),
                i64::from(suspend),
                reason,
                now_ms,
            ],
        )?;

        self.get(process_id)?
            .ok_or_else(|| ProcessTableError::NotFound {
                process_id: process_id.to_string(),
            })
    }

    /// Live processes with a signal waiting to be delivered.
    ///
    /// What a worker polls at its safe point, and what a supervisor reads after a
    /// restart to find work that was asked to stop before the app died.
    pub fn pending_signals(
        &self,
        kinds: &[ProcessKind],
    ) -> ProcessTableResult<Vec<ProcessRecord>> {
        Ok(self
            .list(&ProcessFilter {
                kinds: kinds.to_vec(),
                live_only: true,
                limit: Some(MAX_LIST_LIMIT),
                ..ProcessFilter::default()
            })?
            .into_iter()
            .filter(|record| !record.signal_intent.is_clear())
            .collect())
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
        let updated = self.connection.execute(
            "UPDATE agent_processes SET native_pid = ?2, updated_at_ms = ?3 WHERE process_id = ?1",
            params![process_id, native_pid, now_ms],
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
    pub fn link_run(
        &self,
        process_id: &str,
        run_id: &str,
        now_ms: i64,
    ) -> ProcessTableResult<()> {
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
        if projection.native_pid.is_some() && record.native_pid != projection.native_pid {
            self.set_native_pid(&record.process_id, projection.native_pid, now_ms)?;
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
                }),
                now_ms,
            )?);
        }
        Ok(reaped)
    }
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

impl ProcessProjector for LedgerProcessProjector {
    fn project(&self, projection: &ProcessProjection) -> Result<(), String> {
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
                crate::run_ledger::RunLedger::open(&self.path).map_err(|error| error.to_string())?,
            );
        }
        let ledger = slot.as_ref().expect("ledger initialized above");
        ledger
            .process_table()
            .reconcile(projection, now_ms)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

const SELECT_COLUMNS: &str = "SELECT process_id, parent_process_id, kind, external_id, state, \
     run_id, workspace, profile, native_pid, max_wall_ms, max_memory_bytes, max_output_bytes, \
     max_child_processes, exit_status, exit_code, exit_signal, exit_reason, created_at_ms, \
     updated_at_ms, started_at_ms, exited_at_ms, stop_requested, suspend_requested, \
     signal_reason, signal_requested_at_ms FROM agent_processes";

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
            }),
            Err(error) => return Ok(Err(error)),
        },
        None => None,
    };

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
        limits: ProcessLimits {
            max_wall_ms: row.get::<_, Option<i64>>(9)?.map(|v| v as u64),
            max_memory_bytes: row.get::<_, Option<i64>>(10)?.map(|v| v as u64),
            max_output_bytes: row.get::<_, Option<i64>>(11)?.map(|v| v as u64),
            max_child_processes: row.get::<_, Option<i64>>(12)?.map(|v| v as u32),
        },
        exit,
        signal_intent: SignalIntent {
            stop_requested: row.get::<_, i64>(21)? != 0,
            suspend_requested: row.get::<_, i64>(22)? != 0,
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
        assert!(record.limits.is_unbounded());
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
                existing_process_id, ..
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
        assert!(raw.is_err(), "an exit status on a running row must be refused");
    }

    #[test]
    fn a_parent_must_exist_and_cannot_be_the_process_itself() {
        let ledger = ledger();
        let table = ProcessTable::new(ledger.connection());
        let parent = admit(&table, ProcessKind::ChatTurn, "turn-parent");

        let child = table
            .admit(
                &AdmitProcess::new(ProcessKind::Subagent, "sub-1")
                    .with_parent(&parent.process_id),
                T0 + 1,
            )
            .unwrap();
        assert_eq!(child.parent_process_id.as_deref(), Some(parent.process_id.as_str()));

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
        assert!(self_parent.is_err(), "a process must not become its own parent");
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
        assert_eq!(in_one.len(), 1, "the table can answer what runs in a folder");
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
                &ProcessProjection::new(ProcessKind::DaemonJob, "job-late-pid", ProcessState::Admitted),
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
            ProcessKind::SideTask => "src/lib/sideTaskRunner.ts — runSideTask",
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
        for kind in [
            ProcessKind::DaemonJob,
            ProcessKind::BackgroundShell,
            ProcessKind::RemoteRun,
        ] {
            assert!(kind.signal_support(ProcessSignal::Kill).is_honoured());
        }
    }

    #[test]
    fn suspend_is_honoured_only_where_a_pause_mechanism_actually_exists() {
        // The finding this pins: suspend/resume exist in exactly two places —
        // the daemon (real OS suspend of a child it owns, inherited by remote
        // runs, which are daemon jobs) and side tasks (a cooperative `paused`
        // status their loop already checks). Every other refusal names the
        // missing mechanism so it reads as a boundary, not a TODO.
        for kind in [
            ProcessKind::DaemonJob,
            ProcessKind::RemoteRun,
            ProcessKind::SideTask,
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
        for kind in [
            ProcessKind::ChatTurn,
            ProcessKind::Subagent,
            ProcessKind::CrewMember,
            ProcessKind::WorkflowRun,
            ProcessKind::WorkflowNode,
            ProcessKind::BackgroundShell,
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
        assert_eq!(signalled.signal_reason.as_deref(), Some("user pressed stop"));
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
        let record = admit(&table, ProcessKind::ChatTurn, "turn-refuse");

        let error = table
            .signal(&record.process_id, ProcessSignal::Suspend, None, T0 + 1)
            .expect_err("a chat turn cannot be suspended today");
        match error {
            ProcessTableError::SignalRefused {
                kind, signal, reason, ..
            } => {
                assert_eq!(kind, ProcessKind::ChatTurn);
                assert_eq!(signal, ProcessSignal::Suspend);
                assert!(reason.contains("round boundary"), "{reason}");
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
            .signal(&asked.process_id, ProcessSignal::Stop, Some("shutdown"), T0 + 2)
            .unwrap();
        table
            .signal(&other_kind.process_id, ProcessSignal::Suspend, None, T0 + 2)
            .unwrap();

        let daemon_pending = table.pending_signals(&[ProcessKind::DaemonJob]).unwrap();
        assert_eq!(daemon_pending.len(), 1);
        assert_eq!(daemon_pending[0].process_id, asked.process_id);

        let all_pending = table.pending_signals(&[]).unwrap();
        assert_eq!(all_pending.len(), 2, "an empty kind filter means every kind");

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
            table.pending_signals(&[ProcessKind::DaemonJob]).unwrap().len(),
            0
        );
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
        // `ALL` must itself be exhaustive, or the loop above proves nothing.
        assert_eq!(
            ProcessKind::ALL.len(),
            9,
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
}
