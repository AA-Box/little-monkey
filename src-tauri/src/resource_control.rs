//! The one resource-control contract every native child-process owner uses.
//!
//! Before this module, "how is this process bounded" had one answer per owner:
//! the daemon forked `ps` and compared RSS to a recipe field, the Windows shell
//! built a job object with fixed constants, `tools.rs` dropped a future and
//! terminated a process group, and `ProcessLimits` recorded numbers that in
//! several cases nobody read. Those are four resource architectures, and a
//! caller could not ask any of them what was actually holding a workload.
//!
//! [`ResourceController`] is that question made answerable. It owns the whole
//! lifetime of a bound — prepare the containment, attach the workload, sample it,
//! terminate the tree — and it can state, from code, what a reader previously had
//! to infer from documentation:
//!
//! - the requested limit and the effective one ([`EffectiveLimits`]),
//! - which mechanism holds each ([`ControllerCapabilities`]),
//! - whether that mechanism is kernel-held or supervised ([`EnforcementLevel`]),
//! - the measured usage where it is measurable ([`ResourceSample`]),
//! - the exact reason a limit cannot be enforced ([`LimitCapability::Unavailable`]),
//! - and the tree primitive this process is contained by.
//!
//! # An enum, not a `dyn` trait
//!
//! There are exactly three backends and they are selected by `cfg`, never at
//! runtime by policy: a host is Linux or Windows or neither. A trait object would
//! add a vtable, an allocation and a lifetime parameter to express a choice the
//! compiler already made. The five operations are the same five either way, which
//! is the part that matters — one contract, one place to add a backend.
//!
//! # The supervisor is not a consolation prize
//!
//! [`Backend::Supervisor`] measures a real process tree through
//! [`crate::process_tree`] and terminates it through
//! [`crate::os_signal::terminate_process_group`]. It genuinely enforces memory,
//! process count and wall time, and it says so as
//! [`EnforcementLevel::Supervised`] rather than borrowing the word "kernel". The
//! distinction is load-bearing: a kernel-held bound survives this app dying, and
//! a supervised one does not.

use std::collections::BTreeMap;
use std::io;
use std::time::Instant;

use crate::process_table::{ProcessLimitKind, ProcessLimits};
use crate::process_tree::{self, ProcessIdentity};

/// Who is holding a bound.
///
/// Kept separate from "is it enforced at all" because a caller auditing what this
/// app promised needs to know whether the promise survives the app. Calling a
/// supervised bound kernel-enforced is the specific dishonesty this type exists
/// to make impossible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnforcementLevel {
    /// The kernel holds it and will keep holding it if this app disappears.
    Kernel,
    /// A supervisor in this app measures and acts. Dies with the supervisor.
    Supervised,
    /// A real bound whose number comes from the owner (a recipe, a workflow
    /// definition), not from this process record.
    OwnerSourced,
}

impl EnforcementLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            EnforcementLevel::Kernel => "kernel",
            EnforcementLevel::Supervised => "supervised",
            EnforcementLevel::OwnerSourced => "owner-sourced",
        }
    }
}

/// What this backend can do about one resource.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum LimitCapability {
    Enforced {
        level: EnforcementLevel,
        /// The named primitive, e.g. "cgroup v2 memory.max". Never a category.
        mechanism: String,
    },
    /// The resource does not exist for this workload — a context-token budget on
    /// a process that never calls a model. Distinct from `Unavailable`, which is
    /// a missing mechanism rather than a missing question.
    NotApplicable { reason: String },
    /// The mechanism is missing on this host, and the reason names what is
    /// missing rather than saying "unsupported".
    Unavailable { reason: String },
}

impl LimitCapability {
    fn supervised(mechanism: &str) -> Self {
        LimitCapability::Enforced {
            level: EnforcementLevel::Supervised,
            mechanism: mechanism.to_string(),
        }
    }

    #[must_use]
    pub fn is_enforced(&self) -> bool {
        matches!(self, LimitCapability::Enforced { .. })
    }

    #[must_use]
    pub fn level(&self) -> Option<EnforcementLevel> {
        match self {
            LimitCapability::Enforced { level, .. } => Some(*level),
            _ => None,
        }
    }

    #[must_use]
    pub fn mechanism(&self) -> Option<&str> {
        match self {
            LimitCapability::Enforced { mechanism, .. } => Some(mechanism),
            _ => None,
        }
    }

    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            LimitCapability::NotApplicable { reason } | LimitCapability::Unavailable { reason } => {
                Some(reason)
            }
            LimitCapability::Enforced { .. } => None,
        }
    }
}

/// Everything a caller can ask about what will hold this workload.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControllerCapabilities {
    /// The backend's own name, for the UI and `monkey processes show`.
    pub backend: String,
    /// The lifetime primitive that owns the tree: a cgroup, a job object, a
    /// process group. Stated rather than inferred, because a process group is
    /// not a container and the difference decides whether a deliberate escape is
    /// possible.
    pub tree_primitive: String,
    pub wall: LimitCapability,
    pub memory: LimitCapability,
    pub child_processes: LimitCapability,
    pub output: LimitCapability,
    pub context_tokens: LimitCapability,
}

impl ControllerCapabilities {
    #[must_use]
    pub fn for_limit(&self, limit: ProcessLimitKind) -> &LimitCapability {
        match limit {
            ProcessLimitKind::Wall => &self.wall,
            ProcessLimitKind::Memory => &self.memory,
            ProcessLimitKind::ChildProcesses => &self.child_processes,
            ProcessLimitKind::Output => &self.output,
            ProcessLimitKind::ContextTokens => &self.context_tokens,
        }
    }
}

/// Where an effective limit's number came from.
///
/// Recorded alongside the value so the UI can answer "why is my job capped at
/// 4 GiB" without the reader having to know the resolution order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitSource {
    PlatformGuardrail,
    ClassDefault,
    Recipe,
    Workflow,
    UserOverride,
}

impl LimitSource {
    pub fn as_str(self) -> &'static str {
        match self {
            LimitSource::PlatformGuardrail => "platform_guardrail",
            LimitSource::ClassDefault => "class_default",
            LimitSource::Recipe => "recipe",
            LimitSource::Workflow => "workflow",
            LimitSource::UserOverride => "user_override",
        }
    }
}

/// One resolved bound: the number that will be installed, and who supplied it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedLimit {
    pub value: u64,
    pub source: LimitSource,
}

/// The limits a process will actually run under, after every layer has had its
/// say.
///
/// The resolution lives in [`Self::resolve`] and nowhere else. Four owners each
/// merging class defaults with caller overrides in their own way is how the same
/// field came to mean different things in different subsystems.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveLimits {
    pub wall_ms: Option<ResolvedLimit>,
    pub memory_bytes: Option<ResolvedLimit>,
    pub output_bytes: Option<ResolvedLimit>,
    pub child_processes: Option<ResolvedLimit>,
    pub context_tokens: Option<ResolvedLimit>,
}

/// One layer's contribution to the merge, in precedence order from weakest
/// authority to strongest *identity* — note that authority here decides only
/// whose name is recorded, never whose number wins.
#[derive(Debug, Clone, Copy, Default)]
pub struct LimitLayer {
    pub limits: ProcessLimits,
    pub source: LimitSource,
}

impl Default for LimitSource {
    fn default() -> Self {
        LimitSource::ClassDefault
    }
}

impl LimitLayer {
    #[must_use]
    pub fn new(source: LimitSource, limits: ProcessLimits) -> Self {
        LimitLayer { limits, source }
    }
}

impl EffectiveLimits {
    /// Intersect every layer: **the strongest bound wins**, whoever stated it.
    ///
    /// These fields are maxima, so intersecting them means taking the minimum.
    /// A caller cannot widen a class default or a platform guardrail by passing a
    /// larger number — which is the property that makes a guardrail a guardrail —
    /// and a class default cannot override a tighter recipe value either. Order
    /// in the slice therefore does not change the number; it only breaks ties for
    /// which source is *recorded*, and it breaks them toward the earlier layer so
    /// a guardrail listed first is credited when it is doing the work.
    #[must_use]
    pub fn resolve(layers: &[LimitLayer]) -> Self {
        fn tightest<T: Into<u64> + Copy>(
            layers: &[LimitLayer],
            field: impl Fn(&ProcessLimits) -> Option<T>,
        ) -> Option<ResolvedLimit> {
            layers
                .iter()
                .filter_map(|layer| {
                    field(&layer.limits).map(|value| ResolvedLimit {
                        value: value.into(),
                        source: layer.source,
                    })
                })
                // `min_by_key` keeps the first of equal keys, which is why the
                // tie goes to the earlier layer.
                .min_by_key(|resolved| resolved.value)
        }

        EffectiveLimits {
            wall_ms: tightest(layers, |limits| limits.max_wall_ms),
            memory_bytes: tightest(layers, |limits| limits.max_memory_bytes),
            output_bytes: tightest(layers, |limits| limits.max_output_bytes),
            child_processes: tightest(layers, |limits| limits.max_child_processes),
            context_tokens: tightest(layers, |limits| limits.max_context_tokens),
        }
    }

    #[must_use]
    pub fn value_for(&self, limit: ProcessLimitKind) -> Option<ResolvedLimit> {
        match limit {
            ProcessLimitKind::Wall => self.wall_ms,
            ProcessLimitKind::Memory => self.memory_bytes,
            ProcessLimitKind::Output => self.output_bytes,
            ProcessLimitKind::ChildProcesses => self.child_processes,
            ProcessLimitKind::ContextTokens => self.context_tokens,
        }
    }

    /// The flat record stored on the process row.
    #[must_use]
    pub fn to_process_limits(self) -> ProcessLimits {
        ProcessLimits {
            max_wall_ms: self.wall_ms.map(|resolved| resolved.value),
            max_memory_bytes: self.memory_bytes.map(|resolved| resolved.value),
            max_output_bytes: self.output_bytes.map(|resolved| resolved.value),
            max_child_processes: self
                .child_processes
                .map(|resolved| u32::try_from(resolved.value).unwrap_or(u32::MAX)),
            max_context_tokens: self.context_tokens.map(|resolved| resolved.value),
        }
    }
}

/// What the workload is holding right now.
///
/// `None` on a field means this backend does not measure it — never zero. A
/// watchdog comparing `Some(0)` against a budget would be satisfied forever.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceSample {
    pub wall_ms: u64,
    pub rss_bytes: Option<u64>,
    pub peak_rss_bytes: Option<u64>,
    pub process_count: Option<u32>,
    pub peak_process_count: Option<u32>,
    pub output_bytes: Option<u64>,
}

/// Evidence, from the mechanism itself, that a configured bound fired.
///
/// # Why `observed > configured` cannot be the only test
///
/// A *supervised* bound is discovered by comparison: the workload exceeds the
/// number, and the supervisor notices shortly afterwards. That is the right test
/// for a supervisor, and it is the wrong test for a kernel, because a kernel
/// bound's entire purpose is that the workload never exceeds the number.
///
/// A cgroup with `pids.max = 12` refuses the thirteenth `fork` and leaves
/// `pids.current` at 12. A Windows job with an active-process limit fails the
/// thirteenth `CreateProcess` and leaves `ActiveProcesses` at 12. In both cases
/// `observed > configured` is false at every sample and stays false forever — so
/// a controller that tested only the comparison would watch the limit work
/// perfectly and then record the workload as having failed for no stated reason.
/// "The bound held" and "the app can say which bound held" are two obligations,
/// and this type is the second one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitEvent {
    pub limit: ProcessLimitKind,
    /// What the mechanism was holding when it refused. Usually *equal* to the
    /// configured value rather than above it, which is the whole point.
    pub observed: u64,
    /// The counter or notification that carried the evidence, named exactly —
    /// `pids.events max`, `JOB_OBJECT_MSG_ACTIVE_PROCESS_LIMIT` — so a reader can
    /// go and look at the same thing.
    pub evidence: String,
}

/// A limit that fired, with everything a ledger event and a UI row need.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitBreach {
    /// The `ProcessLimits` field name, so the record names the thing that was set.
    pub limit: String,
    pub configured: u64,
    pub observed: u64,
    /// Which backend noticed. `"windows job object"`, `"cgroup v2"`, `"supervisor"`.
    pub backend: String,
    pub level: String,
    pub observed_at_ms: i64,
    /// The kernel counter or notification that carried the evidence, when the
    /// breach came from the mechanism's own accounting rather than from the
    /// supervisor's comparison.
    ///
    /// `None` for a supervised bound, which has no accounting of its own and
    /// whose evidence *is* the two numbers beside it. Optional rather than
    /// mandatory precisely so the supervisor is not made to invent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

impl LimitBreach {
    /// The human sentence shown as the exit reason.
    ///
    /// Both numbers, always: "memory limit exceeded" tells a reader the budget
    /// fired, and "held 4.11 GiB against a 4.00 GiB budget" tells them whether
    /// the budget was wrong or the job was.
    ///
    /// Where a kernel refused rather than allowed an overshoot, the two numbers
    /// are *equal* and would read as a limit that did not fire, so the evidence
    /// is appended to say which counter proved it did.
    #[must_use]
    pub fn describe(&self) -> String {
        let base = format!(
            "{} exceeded: observed {} against a configured {} ({} · {})",
            self.limit, self.observed, self.configured, self.backend, self.level
        );
        match &self.evidence {
            Some(evidence) => format!("{base}, reported by {evidence}"),
            None => base,
        }
    }
}

/// Why a workload could not be brought under containment.
///
/// The two cases are not degrees of the same failure and must not share a
/// branch: one is a workload that finished, and the other is a workload running
/// outside the bound its record would otherwise claim.
#[derive(Debug)]
pub enum AttachFailure {
    /// The process finished before it could be attached.
    ///
    /// Nothing escaped: there is no tree left to bound, and the exit status is
    /// the answer the caller wanted. Normal for a command that fails to `exec`
    /// or prints one line.
    AlreadyExited,
    /// The process exists and containment could not be established or verified —
    /// job membership refused, the cgroup did not take it, or the host would not
    /// report an identity that a later sample or signal could be checked against.
    ///
    /// Fatal by construction: continuing would run agent-controlled code with a
    /// process record asserting a bound nothing holds.
    Containment(io::Error),
}

impl std::fmt::Display for AttachFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttachFailure::AlreadyExited => {
                write!(formatter, "the process exited before it could be attached")
            }
            AttachFailure::Containment(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for AttachFailure {}

/// The controller: one workload's whole resource story.
pub struct ResourceController {
    limits: EffectiveLimits,
    backend: Backend,
    root: Option<ProcessIdentity>,
    started_at: Instant,
    peak_rss_bytes: Option<u64>,
    peak_process_count: Option<u32>,
    /// Bytes the owner has captured. Fed by whoever drains the pipes, because
    /// no backend can see a pipe.
    output_bytes: Option<u64>,
    /// Every process this workload has ever been observed to own, by identity.
    ///
    /// Accumulated rather than re-derived, because ancestry is only readable
    /// while the ancestors are alive: a descendant that re-parents after its
    /// parent is killed cannot be attributed to this workload by any later
    /// snapshot. What can be attributed is what was recorded *before* the link
    /// was destroyed, which is what this is.
    owned: BTreeMap<u32, u64>,
}

enum Backend {
    #[cfg(target_os = "linux")]
    Cgroup(crate::resource_control_cgroup::CgroupScope),
    #[cfg(windows)]
    Job(crate::resource_control_job::JobObject),
    /// Measures a real tree and terminates a real process group. Available on
    /// every host, and the only backend on macOS.
    Supervisor,
}

impl ResourceController {
    /// Build the strongest controller this host offers for these limits.
    ///
    /// Falls back rather than failing: a Linux host with no delegated cgroup
    /// hierarchy gets the supervisor, which enforces the same resources at a
    /// lower level and says so. What it never does is report a bound it is not
    /// holding.
    #[must_use]
    pub fn new(limits: EffectiveLimits) -> Self {
        #[cfg(target_os = "linux")]
        let backend = match crate::resource_control_cgroup::CgroupScope::create(&limits) {
            Ok(Some(scope)) => Backend::Cgroup(scope),
            // Both a refusal and an absent hierarchy land here. The reason is
            // carried into `capabilities()` by the supervisor's own answer, so
            // nothing is silently downgraded without a caller being able to see
            // it.
            Ok(None) | Err(_) => Backend::Supervisor,
        };
        #[cfg(windows)]
        let backend = match crate::resource_control_job::JobObject::create(&limits) {
            Ok(job) => Backend::Job(job),
            Err(_) => Backend::Supervisor,
        };
        #[cfg(not(any(target_os = "linux", windows)))]
        let backend = Backend::Supervisor;

        ResourceController {
            limits,
            backend,
            root: None,
            started_at: Instant::now(),
            peak_rss_bytes: None,
            peak_process_count: None,
            output_bytes: None,
            owned: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn limits(&self) -> &EffectiveLimits {
        &self.limits
    }

    #[must_use]
    pub fn root(&self) -> Option<ProcessIdentity> {
        self.root
    }

    /// What is holding this workload, and what is not.
    #[must_use]
    pub fn capabilities(&self) -> ControllerCapabilities {
        match &self.backend {
            #[cfg(target_os = "linux")]
            Backend::Cgroup(scope) => scope.capabilities(),
            #[cfg(windows)]
            Backend::Job(job) => job.capabilities(),
            Backend::Supervisor => supervisor_capabilities(),
        }
    }

    /// Install everything that must exist *before* the workload starts.
    ///
    /// The ordering rule K4 turns on: where a platform offers a pre-execution
    /// mechanism, there must be no window in which user code runs outside it.
    /// On Linux the child joins its cgroup between `fork` and `exec`; on Windows
    /// the process is created suspended and assigned before it is resumed. The
    /// supervisor has no pre-execution mechanism, which is exactly why it reports
    /// itself as supervised.
    pub fn prepare_tokio(&self, command: &mut tokio::process::Command) -> io::Result<()> {
        match &self.backend {
            #[cfg(target_os = "linux")]
            Backend::Cgroup(scope) => scope.prepare_tokio(command),
            #[cfg(windows)]
            Backend::Job(job) => job.prepare_tokio(command),
            Backend::Supervisor => Ok(prepare_supervised(command)),
        }
    }

    /// [`Self::prepare_tokio`] for the sites that must build a `std` command —
    /// a background shell is deliberately not `kill_on_drop`, and `std` and
    /// `tokio` share no `pre_exec` trait.
    ///
    /// Split as an entry point rather than duplicated policy, on the same terms
    /// as `os_limits::apply_std`: a site that cannot use tokio's builder must not
    /// be a site with weaker containment.
    pub fn prepare_std(&self, command: &mut std::process::Command) -> io::Result<()> {
        match &self.backend {
            #[cfg(target_os = "linux")]
            Backend::Cgroup(scope) => scope.prepare_std(command),
            #[cfg(windows)]
            Backend::Job(job) => job.prepare_std(command),
            Backend::Supervisor => Ok(prepare_supervised_std(command)),
        }
    }

    /// Hand the spawn site the job this workload must be created into.
    ///
    /// Windows containment is the job, and the job has to exist before
    /// `CreateProcessW` so the process can be created suspended, assigned, and
    /// only then resumed. That ordering is why this is a separate step rather
    /// than part of `prepare_*`: on Unix the containment is installed by the
    /// child between `fork` and `exec`, and on Windows it is installed by the
    /// parent around the creation.
    #[cfg(windows)]
    pub fn windows_job_for_spawn(&self) -> io::Result<crate::sandbox_windows::JobConfinement> {
        match &self.backend {
            Backend::Job(job) => job.duplicate_for_spawn(),
            // The job could not be created, so this workload gets the fixed
            // containment ceiling and the supervisor's own bounds on top. It is
            // weaker than the caller asked for, which is exactly what
            // `capabilities()` will report.
            Backend::Supervisor => crate::sandbox_windows::create_job(),
        }
    }

    /// Record the workload's root once it exists, and verify it is contained.
    ///
    /// Stores an identity rather than a pid: everything after this — sampling,
    /// termination — re-checks that the pid is still the process we attached to,
    /// so a reused pid cannot be sampled as ours or, far worse, killed as ours.
    ///
    /// # This is containment verification, not bookkeeping
    ///
    /// The `Err` half used to be discarded at both call sites on the reading that
    /// a process which exits instantly is normal — which is true, and is only one
    /// of the two things that reach here. The other is a process that is
    /// **running** and is not inside the containment the record is about to claim
    /// for it: a Windows process that reached `CreateProcess` without being
    /// assigned to its job, a cgroup that did not take the migration write, a host
    /// that will not report an identity later checks depend on. Continuing from
    /// those is the exact failure K4 exists to prevent, so they are
    /// [`AttachFailure::Containment`] and the caller must fail the spawn.
    pub fn attach(&mut self, pid: u32) -> Result<(), AttachFailure> {
        let Some(identity) = ProcessIdentity::of(pid) else {
            // No identity, so either the process is gone or this host will not
            // say. Only the first is safe to carry on from, and the difference is
            // whether the pid is still there at all.
            //
            // `kill(pid, 0)` alone answers that wrong for a corpse: macOS refuses
            // `proc_pidinfo` for a zombie, so a child that exited a microsecond
            // ago has no start time *and* still answers a signal probe — which
            // read as "running and unidentifiable" and refused the spawn. Asking
            // whether it is our own unreaped child settles it, and the pid here is
            // always one this process just spawned.
            if crate::os_signal::process_is_alive(pid) && !process_tree::child_exited_unreaped(pid)
            {
                return Err(AttachFailure::Containment(io::Error::other(format!(
                    "process {pid} is running and the host would not report its start time, so \
                     nothing can prove which process a later sample or signal would reach"
                ))));
            }
            return Err(AttachFailure::AlreadyExited);
        };
        self.root = Some(identity);
        self.started_at = Instant::now();
        self.owned.insert(pid, identity.start_time);

        if let Err(error) = self.confirm_containment(pid) {
            // A process that exited between the spawn and this check cannot be a
            // member of anything, so re-ask the question that separates the two
            // cases rather than reporting a containment failure against a corpse.
            //
            // `is_running`, not `is_still_alive`: a child that has exited but not
            // yet been reaped is a zombie, and a zombie keeps its `/proc/<pid>`
            // entry — so `is_still_alive` answers true for it and this refused a
            // spawn that had already finished. `printf` is fast enough to hit
            // that window essentially always, which is how the counter-test
            // caught it: on Linux under a real cgroup, an ordinary short command
            // failed with "started without joining its cgroup scope" while every
            // long-running one passed.
            //
            // Same reasoning the survivor set already applies: an unreaped
            // process holds nothing and cannot fork, so there is no containment
            // question left to answer about it.
            if !identity.is_running() {
                return Err(AttachFailure::AlreadyExited);
            }
            return Err(AttachFailure::Containment(error));
        }
        Ok(())
    }

    /// Ask the backend whether this process is inside the containment it created.
    fn confirm_containment(&self, #[allow(unused_variables)] pid: u32) -> io::Result<()> {
        match &self.backend {
            #[cfg(target_os = "linux")]
            Backend::Cgroup(scope) => scope.confirm_membership(pid),
            #[cfg(windows)]
            Backend::Job(job) => job.confirm_assignment(pid),
            // The supervisor's containment *is* the identity recorded above and
            // the process group installed before the fork; there is no separate
            // membership to read back.
            Backend::Supervisor => Ok(()),
        }
    }

    /// Feed the controller the byte count only the pipe reader can know.
    pub fn record_output_bytes(&mut self, bytes: u64) {
        self.output_bytes = Some(bytes);
    }

    /// Fold everything currently attributable to this workload into the owned set.
    ///
    /// Called on every sample, and once more immediately before a termination,
    /// because **ancestry is only readable while the ancestors are alive**. Kill
    /// the group first and a descendant that had left it is re-parented to init;
    /// no later snapshot can then prove it was ever ours, and it survives. The
    /// order this function's callers keep — record, then terminate — is the fix
    /// for that race.
    fn record_ownership(&mut self) {
        let Some(root) = self.root else {
            return;
        };
        let Ok(nodes) = process_tree::snapshot() else {
            return;
        };
        // Expand from the root *and* from every member already recorded as ours:
        // once the root is gone the closure from it is empty, while a member
        // captured earlier may still have live children of its own.
        let mut roots: Vec<u32> = vec![root.pid];
        roots.extend(
            self.owned
                .iter()
                .filter(|(pid, start_time)| {
                    ProcessIdentity {
                        pid: **pid,
                        start_time: **start_time,
                    }
                    .is_running()
                })
                .map(|(pid, _)| *pid),
        );
        for pid in process_tree::tree_members_of_any(&nodes, &roots) {
            if let Some(identity) = ProcessIdentity::of(pid) {
                self.owned.entry(pid).or_insert(identity.start_time);
            }
        }
    }

    /// Every process still running that this workload owns.
    #[must_use]
    pub fn live_owned(&self) -> Vec<ProcessIdentity> {
        self.owned
            .iter()
            .map(|(pid, start_time)| ProcessIdentity {
                pid: *pid,
                start_time: *start_time,
            })
            .filter(ProcessIdentity::is_running)
            .collect()
    }

    /// Measure now, folding peaks into the controller.
    ///
    /// `Ok(None)` means the workload is gone — which is what an exited process
    /// is, and deliberately not a sample of zeros.
    pub fn sample(&mut self) -> io::Result<Option<ResourceSample>> {
        let wall_ms = u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let Some(root) = self.root else {
            // Nothing attached yet: wall time is still real and measured from
            // construction, and nothing else can be.
            return Ok(Some(ResourceSample {
                wall_ms,
                output_bytes: self.output_bytes,
                ..ResourceSample::default()
            }));
        };

        // Whatever the backend measures, ownership is recorded on every tick:
        // the set has to be complete *before* a termination destroys the links
        // that prove membership.
        self.record_ownership();

        let measured = match &self.backend {
            #[cfg(target_os = "linux")]
            Backend::Cgroup(scope) => scope.sample()?,
            #[cfg(windows)]
            Backend::Job(job) => job.sample()?,
            Backend::Supervisor => {
                let usage = supervised_tree_usage(root, &self.owned)?;
                usage.map(|usage| (usage.rss_bytes, Some(usage.process_count)))
            }
        };
        let Some((rss_bytes, process_count)) = measured else {
            return Ok(None);
        };

        if let Some(bytes) = rss_bytes {
            self.peak_rss_bytes = Some(self.peak_rss_bytes.unwrap_or(0).max(bytes));
        }
        if let Some(count) = process_count {
            self.peak_process_count = Some(self.peak_process_count.unwrap_or(0).max(count));
        }
        Ok(Some(ResourceSample {
            wall_ms,
            rss_bytes,
            peak_rss_bytes: self.peak_rss_bytes,
            process_count,
            peak_process_count: self.peak_process_count,
            output_bytes: self.output_bytes,
        }))
    }

    /// Ask the backend whether its own accounting says a bound fired.
    ///
    /// Consulted *before* [`Self::breach`] on every tick, because where a kernel
    /// mechanism holds the bound this is the only test that can be true — see
    /// [`LimitEvent`] for why the comparison never becomes true against a limit
    /// that is doing its job.
    ///
    /// The supervisor answers `None`: it has no accounting of its own, and its
    /// bound genuinely *is* the comparison.
    pub fn poll_limit_events(&mut self, now_ms: i64) -> io::Result<Option<LimitBreach>> {
        let event: Option<LimitEvent> = match &mut self.backend {
            #[cfg(target_os = "linux")]
            Backend::Cgroup(scope) => scope.poll_limit_events()?,
            #[cfg(windows)]
            Backend::Job(job) => job.poll_limit_events()?,
            Backend::Supervisor => None,
        };
        let Some(event) = event else {
            return Ok(None);
        };
        // Only a limit this controller was actually asked to hold. A cgroup
        // inherits nothing here, but a job object's fixed guardrail can fire
        // without the caller having stated a bound, and attributing that to a
        // caller's policy would name a number nobody set.
        let Some(configured) = self.limits.value_for(event.limit) else {
            return Ok(None);
        };
        let capabilities = self.capabilities();
        let Some(level) = capabilities.for_limit(event.limit).level() else {
            return Ok(None);
        };
        Ok(Some(LimitBreach {
            limit: event.limit.as_str().to_string(),
            configured: configured.value,
            observed: event.observed,
            backend: capabilities.backend,
            level: level.as_str().to_string(),
            observed_at_ms: now_ms,
            evidence: Some(event.evidence),
        }))
    }

    /// The first limit this sample violates, if any.
    ///
    /// Wall is checked first because it is the one bound that is always
    /// measurable, so a workload past its deadline is reported as such even on a
    /// host that can measure nothing else.
    #[must_use]
    pub fn breach(&self, sample: &ResourceSample, now_ms: i64) -> Option<LimitBreach> {
        let capabilities = self.capabilities();
        let backend = capabilities.backend.clone();
        let check = |limit: ProcessLimitKind, observed: Option<u64>| -> Option<LimitBreach> {
            let configured = self.limits.value_for(limit)?.value;
            let observed = observed?;
            if observed <= configured {
                return None;
            }
            // A limit nothing enforces cannot be breached *by this controller*:
            // reporting one would attribute a kill to a mechanism that did not
            // make it.
            let capability = capabilities.for_limit(limit);
            let level = capability.level()?;
            Some(LimitBreach {
                limit: limit.as_str().to_string(),
                configured,
                observed,
                backend: backend.clone(),
                level: level.as_str().to_string(),
                observed_at_ms: now_ms,
                // The supervisor's evidence is the pair of numbers above it.
                evidence: None,
            })
        };

        check(ProcessLimitKind::Wall, Some(sample.wall_ms))
            .or_else(|| check(ProcessLimitKind::Memory, sample.rss_bytes))
            .or_else(|| {
                check(
                    ProcessLimitKind::ChildProcesses,
                    sample.process_count.map(u64::from),
                )
            })
            .or_else(|| check(ProcessLimitKind::Output, sample.output_bytes))
    }

    /// Stop the entire owned workload.
    ///
    /// Idempotent, and safe against a root that has already exited: every signal
    /// is preceded by an identity check, so a pid that has been reused is never
    /// signalled.
    ///
    /// `&mut` rather than `&self` because reclaiming a tree is not a read: the
    /// membership recorded before the first signal is what the later passes
    /// verify against, and deriving it after the fact is the race this exists to
    /// close.
    pub fn terminate_tree(&mut self) -> io::Result<()> {
        // Before anything is signalled, and unconditionally — including when the
        // root is already gone, which is precisely the case where an escaped
        // descendant is all that is left.
        self.record_ownership();
        match &self.backend {
            #[cfg(target_os = "linux")]
            Backend::Cgroup(scope) => scope.terminate_tree(),
            #[cfg(windows)]
            Backend::Job(job) => job.terminate_tree(),
            Backend::Supervisor => terminate_supervised_tree(self.root, &mut self.owned),
        }
    }
}

/// What the supervisor measures: the closure from the root, unioned with every
/// member recorded as ours that is still running.
///
/// The second half is what makes an escaped descendant count against the budget
/// it escaped. Without it a workload could put its allocation behind a `setsid`
/// and a re-parent and read as a tree holding nothing.
fn supervised_tree_usage(
    root: ProcessIdentity,
    owned: &BTreeMap<u32, u64>,
) -> io::Result<Option<process_tree::TreeUsage>> {
    let mut roots: Vec<u32> = vec![root.pid];
    roots.extend(
        owned
            .iter()
            .filter(|(pid, start_time)| {
                ProcessIdentity {
                    pid: **pid,
                    start_time: **start_time,
                }
                .is_running()
            })
            .map(|(pid, _)| *pid),
    );
    if !root.is_running() && roots.len() == 1 {
        // Nothing owned is executing: the workload is gone, which is not the same
        // as a tree of zero processes holding zero bytes.
        return Ok(None);
    }
    let nodes = process_tree::snapshot()?;
    Ok(process_tree::measure_members(
        &process_tree::tree_members_of_any(&nodes, &roots),
    ))
}

/// Wall-clock milliseconds, for stamping a breach.
///
/// A clock that will not read is not a reason to refuse to record a limit kill,
/// so this degrades to zero rather than erroring: an unstamped breach still names
/// the limit, the configured value and the measurement, which is what the record
/// is for.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_millis()).ok())
        .unwrap_or(0)
}

/// How often the supervising loop measures.
///
/// # Why half a second, and what it costs
///
/// This is the granularity of every *supervised* bound, and it is a real
/// limitation rather than a tuning preference: a workload can allocate several
/// gigabytes inside one interval, so a supervised memory limit is "terminated
/// shortly after exceeding" and not "cannot exceed". A kernel-held bound —
/// cgroup `memory.max`, a job object's `JobMemoryLimit` — has no such window,
/// which is exactly why those backends are preferred where a host offers them
/// and why [`EnforcementLevel`] is reported rather than assumed.
///
/// Half a second rather than the daemon watchdog's thirty: that watchdog governs
/// jobs measured in minutes, while an agent shell's whole lifetime is often under
/// a second, and a bound first checked after thirty seconds would never fire for
/// most of the processes this supervises. The cost is one process-table read per
/// interval per supervised workload, which is a few milliseconds.
pub const SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// The outcome of running work under a controller.
#[derive(Debug)]
pub enum Supervised<T> {
    /// The work finished on its own terms. Carries the final sample so a caller
    /// can record peak usage even when nothing was breached.
    Completed(T, ResourceSample),
    /// A limit fired. The owned tree has already been terminated.
    Breached(LimitBreach, ResourceSample),
}

/// Run `work` while sampling the controller, terminating the whole owned tree
/// the moment a limit is exceeded.
///
/// The termination happens *here*, before returning, rather than being left to
/// the caller. A bound that fires and returns without tearing down the workload
/// is the failure this codebase has already had twice: the browser action quota
/// latched `cancelled` without killing Chromium, and a workflow past its wall
/// budget was left claiming to be running. Both reported success while leaking
/// the thing they existed to reclaim.
///
/// `work` is dropped on a breach, which for a `tokio` child with `kill_on_drop`
/// reaps the direct child — but that is not what contains the tree, and it must
/// not be mistaken for it. [`ResourceController::terminate_tree`] is what reaches
/// the grandchild holding the memory.
pub async fn run_under<F>(
    controller: &mut ResourceController,
    work: F,
) -> io::Result<Supervised<F::Output>>
where
    F: std::future::Future,
{
    let mut work = std::pin::pin!(work);
    let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
    // The first tick of a tokio interval completes immediately, which is wanted:
    // a workload already over its wall limit at attach should not get a free
    // interval before anyone looks.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last = ResourceSample::default();
    loop {
        tokio::select! {
            // Biased so a completed workload is never reported as breached on the
            // same poll: a command that exits at its deadline finished, and
            // relabelling that would make the limit look responsible for an
            // outcome it did not cause.
            biased;
            output = &mut work => {
                // A kernel bound normally ends the workload rather than being
                // observed by a sample: the cgroup OOM-kills the member that
                // asked for too much, the job fails its allocation. The work
                // future then resolves with an ordinary non-zero exit, and the
                // only record of *why* is the backend's own counter. Ask before
                // believing the exit — otherwise the strongest enforcement this
                // app has is also the enforcement it can never name.
                if let Some(breach) = controller.poll_limit_events(now_ms())? {
                    controller.terminate_tree()?;
                    return Ok(Supervised::Breached(breach, last));
                }
                return Ok(Supervised::Completed(output, last));
            }
            _ = ticker.tick() => {
                // The mechanism's own accounting first, and unconditionally —
                // including when the workload is already gone. A cgroup that
                // OOM-killed its member, or a job that refused the thirteenth
                // process, has an exited workload and a limit that fired, and
                // reading the sample first would call that an ordinary crash.
                if let Some(breach) = controller.poll_limit_events(now_ms())? {
                    controller.terminate_tree()?;
                    return Ok(Supervised::Breached(breach, last));
                }
                let Some(sample) = controller.sample()? else {
                    // The workload is gone but `work` has not resolved yet —
                    // usually a pipe still draining. Keep waiting for it rather
                    // than reporting a breach against a corpse.
                    continue;
                };
                last = sample;
                if let Some(breach) = controller.breach(&sample, now_ms()) {
                    controller.terminate_tree()?;
                    return Ok(Supervised::Breached(breach, sample));
                }
            }
        }
    }
}

/// What a supervisor can and cannot promise.
///
/// Written out per resource rather than as one summary, because the four answers
/// genuinely differ: three are real supervised bounds and the fourth belongs to
/// whoever owns the pipe.
fn supervisor_capabilities() -> ControllerCapabilities {
    ControllerCapabilities {
        backend: "supervisor".to_string(),
        tree_primitive: if cfg!(windows) {
            "parent-link closure over the host process table".to_string()
        } else {
            "POSIX process group, unioned with the parent-link closure".to_string()
        },
        wall: LimitCapability::supervised(
            "the sampling loop compares elapsed time against the effective wall limit",
        ),
        memory: LimitCapability::supervised(
            "summed resident size over the tree, read from the kernel's own process table",
        ),
        child_processes: LimitCapability::supervised(
            "live members of the owned tree, counted per tree rather than per uid",
        ),
        output: LimitCapability::supervised(
            "the capture buffer is bounded as bytes arrive, before they reach the heap",
        ),
        context_tokens: LimitCapability::NotApplicable {
            reason: "a resource controller bounds an OS process; a context budget is enforced \
                     at the model request by the runtime that can count exactly"
                .to_string(),
        },
    }
}

#[cfg(unix)]
fn prepare_supervised(command: &mut tokio::process::Command) {
    // The process group is the supervisor's tree primitive, so it has to exist
    // before the child does. `process_group(0)` makes the child a group leader,
    // which is what lets a terminate reach descendants that the parent link alone
    // would miss.
    command.process_group(0);
}

#[cfg(unix)]
fn prepare_supervised_std(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn prepare_supervised(_command: &mut tokio::process::Command) {}

#[cfg(not(unix))]
fn prepare_supervised_std(_command: &mut std::process::Command) {}

/// How many terminate-and-verify passes before the supervisor reports that it
/// could not reclaim the workload.
///
/// A pass is needed at all because a member may `fork` between the snapshot and
/// the signal, and the child is owned too. Bounded because "kill until nothing is
/// left" against a process actively forking is an unbounded loop, and a
/// supervisor that will not return is worse than one that reports a survivor.
const MAX_TERMINATION_PASSES: usize = 6;

/// How long a pass waits before deciding whether a signal took effect.
///
/// SIGKILL is not synchronous with its delivery: the target is dead when the
/// kernel says so, not when `kill` returns, and a re-check with no pause reads
/// the process as still running and burns a pass.
#[cfg(unix)]
const TERMINATION_SETTLE: std::time::Duration = std::time::Duration::from_millis(20);

/// Reclaim every process this workload owns, and prove it.
///
/// # The ordering, and why it is the whole algorithm
///
/// The previous version killed the process group and *then* looked for
/// descendants outside it. That is a race with a guaranteed loser: the group kill
/// re-parents every out-of-group descendant to init, so by the time the snapshot
/// is taken the escaped process has neither the group nor a parent link, and
/// nothing can attribute it to the workload. It survives, and the supervisor
/// reports success.
///
/// So membership is captured **before** anything is signalled — that is what
/// [`ResourceController::record_ownership`] does on every sample and once more
/// immediately above this call — and the captured identities are what the later
/// passes signal and verify against.
///
/// # Identity, at every signal
///
/// Each captured member is re-checked against its recorded start time
/// immediately before it is signalled. A pid the kernel has since handed to an
/// unrelated process fails that check and is skipped: killing the user's editor
/// because a compiler exited and its pid was reused is a far worse outcome than
/// leaving one process alive.
#[cfg(unix)]
fn terminate_supervised_tree(
    root: Option<ProcessIdentity>,
    owned: &mut BTreeMap<u32, u64>,
) -> io::Result<()> {
    if owned.is_empty() {
        return Ok(());
    }
    let mut group_error = None;
    for pass in 0..MAX_TERMINATION_PASSES {
        if pass > 0 {
            // Re-derive from what is still running: a member that forked after
            // the previous pass is owned as well, and its ancestry is readable
            // right now because its parent is one of ours.
            expand_owned(owned);
        }

        // The group: one signal for every member that stayed in it, which is the
        // ordinary case and the cheap one. Only while the leader is still there —
        // a process-group id *is* the leader's pid, so signalling it after the
        // leader has been reaped can reach a group the kernel has since given to
        // somebody else.
        if let Some(root) = root {
            if root.is_still_alive() {
                if let Err(error) = crate::os_signal::terminate_process_group(root.pid) {
                    group_error = Some(error);
                }
            }
        }

        // Then every captured member, identity-validated. The root goes last so
        // that while the rest are being signalled its zombie still reserves the
        // process-group id above.
        let root_pid = root.map(|root| root.pid);
        for (pid, start_time) in owned.iter() {
            if Some(*pid) == root_pid {
                continue;
            }
            kill_if_identity_matches(*pid, *start_time);
        }
        if let Some(root) = root {
            kill_if_identity_matches(root.pid, root.start_time);
        }

        if survivors(owned).is_empty() {
            return Ok(());
        }
        std::thread::sleep(TERMINATION_SETTLE);
        if survivors(owned).is_empty() {
            return Ok(());
        }
    }

    let remaining = survivors(owned);
    if !remaining.is_empty() {
        // Reported, never swallowed: a caller that believes a tree was reclaimed
        // will release its reservation and record a clean exit for a workload
        // still consuming the machine.
        return Err(io::Error::other(format!(
            "{} process(es) owned by this workload survived {MAX_TERMINATION_PASSES} termination \
             passes: {remaining:?}",
            remaining.len()
        )));
    }
    match group_error {
        Some(error) => Err(io::Error::other(error)),
        None => Ok(()),
    }
}

/// Add anything reachable from a still-running owned member.
#[cfg(unix)]
fn expand_owned(owned: &mut BTreeMap<u32, u64>) {
    let roots = survivors(owned);
    if roots.is_empty() {
        return;
    }
    let Ok(nodes) = process_tree::snapshot() else {
        return;
    };
    for pid in process_tree::tree_members_of_any(&nodes, &roots) {
        if let Some(identity) = ProcessIdentity::of(pid) {
            owned.entry(pid).or_insert(identity.start_time);
        }
    }
}

/// Owned members that are still executing — zombies excluded, because a process
/// awaiting reaping holds nothing and cannot fork, and counting it would make
/// every termination burn its whole pass budget and then report a failure.
#[cfg(unix)]
fn survivors(owned: &BTreeMap<u32, u64>) -> Vec<u32> {
    owned
        .iter()
        .filter(|(pid, start_time)| {
            ProcessIdentity {
                pid: **pid,
                start_time: **start_time,
            }
            .is_running()
        })
        .map(|(pid, _)| *pid)
        .collect()
}

/// SIGKILL one pid, and only if it is still the process that was recorded.
#[cfg(unix)]
fn kill_if_identity_matches(pid: u32, start_time: u64) {
    let identity = ProcessIdentity { pid, start_time };
    if !identity.is_still_alive() {
        return;
    }
    let Ok(target) = libc::pid_t::try_from(pid) else {
        return;
    };
    // 0 is "every process in our own group" and negatives name a group, so
    // neither is a question about one process — and one of them would signal this
    // app. 1 is init.
    if target <= 1 {
        return;
    }
    // Safe: sends a signal to one pid whose identity was checked on the line
    // above. A process that exited in between answers ESRCH, which is the
    // outcome this wanted.
    unsafe { libc::kill(target, libc::SIGKILL) };
}

/// Windows reaches here only when the job object could not be created, so the
/// tree primitive is the parent-link closure and `taskkill /T` walks it.
#[cfg(not(unix))]
fn terminate_supervised_tree(
    root: Option<ProcessIdentity>,
    owned: &mut BTreeMap<u32, u64>,
) -> io::Result<()> {
    let mut last_error = None;
    for (pid, start_time) in owned.iter() {
        let identity = ProcessIdentity {
            pid: *pid,
            start_time: *start_time,
        };
        // Same rule as the unix path: never signal a pid the kernel may have
        // handed to somebody else since it was recorded.
        if !identity.is_running() {
            continue;
        }
        if let Err(error) = crate::os_signal::terminate_process_group(*pid) {
            last_error = Some(error);
        }
    }
    let _ = root;
    match last_error {
        Some(error) => Err(io::Error::other(error)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(wall: Option<u64>, memory: Option<u64>) -> ProcessLimits {
        ProcessLimits {
            max_wall_ms: wall,
            max_memory_bytes: memory,
            ..ProcessLimits::default()
        }
    }

    #[test]
    fn the_tightest_bound_wins_regardless_of_which_layer_stated_it() {
        let resolved = EffectiveLimits::resolve(&[
            LimitLayer::new(LimitSource::PlatformGuardrail, limits(Some(60_000), None)),
            LimitLayer::new(LimitSource::ClassDefault, limits(Some(30_000), None)),
            LimitLayer::new(LimitSource::UserOverride, limits(Some(90_000), None)),
        ]);
        let wall = resolved.wall_ms.expect("a wall bound was stated");
        assert_eq!(wall.value, 30_000);
        assert_eq!(
            wall.source,
            LimitSource::ClassDefault,
            "the source recorded is whoever supplied the winning number"
        );
    }

    /// The property that makes a guardrail a guardrail: a caller passing a bigger
    /// number does not get a bigger bound.
    #[test]
    fn a_caller_cannot_widen_a_platform_guardrail() {
        let resolved = EffectiveLimits::resolve(&[
            LimitLayer::new(
                LimitSource::PlatformGuardrail,
                limits(None, Some(4 * 1024 * 1024 * 1024)),
            ),
            LimitLayer::new(LimitSource::UserOverride, limits(None, Some(u64::MAX))),
        ]);
        assert_eq!(
            resolved.memory_bytes.expect("bounded").value,
            4 * 1024 * 1024 * 1024
        );
    }

    #[test]
    fn a_caller_may_still_tighten_below_every_other_layer() {
        let resolved = EffectiveLimits::resolve(&[
            LimitLayer::new(LimitSource::ClassDefault, limits(Some(30_000), None)),
            LimitLayer::new(LimitSource::UserOverride, limits(Some(5_000), None)),
        ]);
        let wall = resolved.wall_ms.expect("bounded");
        assert_eq!(wall.value, 5_000);
        assert_eq!(wall.source, LimitSource::UserOverride);
    }

    #[test]
    fn a_field_no_layer_states_stays_absent_rather_than_becoming_zero() {
        let resolved = EffectiveLimits::resolve(&[LimitLayer::new(
            LimitSource::ClassDefault,
            limits(Some(1), None),
        )]);
        assert!(resolved.memory_bytes.is_none());
        assert!(resolved.to_process_limits().max_memory_bytes.is_none());
    }

    #[test]
    fn an_equal_bound_is_credited_to_the_earlier_layer() {
        let resolved = EffectiveLimits::resolve(&[
            LimitLayer::new(LimitSource::PlatformGuardrail, limits(Some(10), None)),
            LimitLayer::new(LimitSource::UserOverride, limits(Some(10), None)),
        ]);
        assert_eq!(
            resolved.wall_ms.expect("bounded").source,
            LimitSource::PlatformGuardrail
        );
    }

    #[test]
    fn every_backend_names_a_mechanism_rather_than_a_category() {
        let capabilities = ResourceController::new(EffectiveLimits::default()).capabilities();
        for limit in ProcessLimitKind::ALL {
            let capability = capabilities.for_limit(*limit);
            let text = capability
                .mechanism()
                .or_else(|| capability.reason())
                .expect("every capability says something");
            assert!(
                text.len() > 20 && !text.contains("unsupported"),
                "{limit:?} answered with a non-answer: {text}"
            );
        }
        assert!(
            !capabilities.tree_primitive.is_empty(),
            "the lifetime primitive must be stated, not inferred"
        );
    }

    /// A breach can only be attributed to a mechanism that exists. Reporting one
    /// otherwise would credit a kill to a backend that never made it.
    #[test]
    fn a_limit_no_backend_enforces_is_never_reported_as_breached() {
        let controller = ResourceController::new(EffectiveLimits::resolve(&[LimitLayer::new(
            LimitSource::UserOverride,
            ProcessLimits {
                max_context_tokens: Some(10),
                ..ProcessLimits::default()
            },
        )]));
        let sample = ResourceSample {
            wall_ms: 0,
            ..ResourceSample::default()
        };
        assert!(controller.breach(&sample, 1).is_none());
    }

    #[test]
    fn a_wall_breach_names_both_numbers_and_the_backend() {
        let mut controller = ResourceController::new(EffectiveLimits::resolve(&[LimitLayer::new(
            LimitSource::UserOverride,
            limits(Some(10), None),
        )]));
        controller.record_output_bytes(0);
        let sample = ResourceSample {
            wall_ms: 5_000,
            ..ResourceSample::default()
        };
        let breach = controller.breach(&sample, 42).expect("past its deadline");
        assert_eq!(breach.limit, "max_wall_ms");
        assert_eq!(breach.configured, 10);
        assert_eq!(breach.observed, 5_000);
        assert_eq!(breach.observed_at_ms, 42);
        let described = breach.describe();
        assert!(
            described.contains("5000") && described.contains("10"),
            "the reason must let a reader tell a wrong budget from a wrong job: {described}"
        );
    }

    #[test]
    fn a_sample_inside_every_bound_is_not_a_breach() {
        let controller = ResourceController::new(EffectiveLimits::resolve(&[LimitLayer::new(
            LimitSource::UserOverride,
            limits(Some(10_000), Some(1_024)),
        )]));
        let sample = ResourceSample {
            wall_ms: 9_999,
            rss_bytes: Some(1_024),
            ..ResourceSample::default()
        };
        assert!(
            controller.breach(&sample, 1).is_none(),
            "a limit is a maximum, so equalling it is inside it"
        );
    }

    /// The Windows job's numbers must come from the effective limits, and the
    /// fixed guardrail must survive as a ceiling rather than as the policy.
    ///
    /// Compiled and run only on Windows, because the type it asserts on is the
    /// job's own. The *rule* it encodes — intersect, never replace — is the same
    /// one `a_caller_cannot_widen_a_platform_guardrail` holds for every host.
    #[cfg(windows)]
    #[test]
    fn a_windows_job_takes_the_tighter_of_the_guardrail_and_the_effective_limit() {
        use crate::sandbox_windows::JobLimits;

        let guardrail = JobLimits::guardrail();

        // Tighter than the guardrail: the caller's number is installed.
        let tightened = JobLimits::from_effective(&EffectiveLimits::resolve(&[LimitLayer::new(
            LimitSource::UserOverride,
            ProcessLimits {
                max_memory_bytes: Some(512 * 1024 * 1024),
                max_child_processes: Some(8),
                ..ProcessLimits::default()
            },
        )]));
        assert_eq!(tightened.memory_bytes, 512 * 1024 * 1024);
        assert_eq!(tightened.active_processes, 8);

        // Looser than the guardrail: the guardrail holds. This is the assertion
        // that stops a job object from advertising a bound larger than the fixed
        // containment ceiling it is also meant to provide.
        let widened = JobLimits::from_effective(&EffectiveLimits::resolve(&[LimitLayer::new(
            LimitSource::UserOverride,
            ProcessLimits {
                max_memory_bytes: Some(u64::MAX),
                max_child_processes: Some(u32::MAX),
                ..ProcessLimits::default()
            },
        )]));
        assert_eq!(widened, guardrail);

        // Nothing stated: the guardrail, unchanged, which is what every spawn got
        // before effective limits existed.
        assert_eq!(
            JobLimits::from_effective(&EffectiveLimits::default()),
            guardrail
        );
    }

    /// A child that finishes before anyone can look at it is not a containment
    /// failure.
    ///
    /// The containment check reads the backend's membership back — the only way
    /// to know a `pre_exec` migration actually took, since a failure there has no
    /// channel home. A process that has already exited is in no cgroup and no
    /// job, so the check has to separate "gone" from "running and uncontained".
    ///
    /// It separated them with `is_still_alive`, which a **zombie** satisfies: an
    /// exited-but-unreaped child keeps its `/proc/<pid>` entry. So on Linux under
    /// a real cgroup, every short command was refused with "started without
    /// joining its cgroup scope" while long-running ones passed — caught by the
    /// counter-test, on `printf`, which is fast enough to hit that window
    /// essentially always.
    ///
    /// Asserted here rather than only on Linux because the distinction is the
    /// controller's, not the backend's: the supervisor reaches the same branch,
    /// and pinning it on every Unix host is what stops it regressing on the two
    /// this cannot run.
    #[cfg(unix)]
    #[test]
    fn attaching_to_a_child_that_already_exited_reports_it_gone_not_uncontained() {
        use std::process::{Command, Stdio};

        let mut child = Command::new("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("`true` spawns");
        let pid = child.id();
        // Deliberately not reaped: this is the zombie window a fast command
        // spends between exiting and its parent calling `wait`.
        for _ in 0..200 {
            if !ProcessIdentity::of(pid).is_some_and(|identity| identity.is_running()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let mut controller = ResourceController::new(EffectiveLimits::resolve(&[LimitLayer::new(
            LimitSource::UserOverride,
            ProcessLimits {
                max_memory_bytes: Some(64 * 1024 * 1024),
                max_child_processes: Some(8),
                ..ProcessLimits::default()
            },
        )]));
        match controller.attach(pid) {
            Err(AttachFailure::AlreadyExited) => {}
            other => panic!(
                "a command that finished before the check must read as gone, not as a spawn \
                 that escaped its containment: {other:?}"
            ),
        }
        let _ = child.wait();
    }

    #[test]
    fn terminating_before_anything_is_attached_is_not_an_error() {
        let mut controller = ResourceController::new(EffectiveLimits::default());
        controller.terminate_tree().expect("nothing to terminate");
    }

    /// A pid that has been reused is the one thing a supervisor must never
    /// signal. Deterministic because it never involves a second process: an
    /// identity whose start time does not match what the pid reports now is by
    /// definition not the process that was recorded, and this asserts the
    /// supervisor treats that as "not ours" rather than as "still running".
    #[cfg(unix)]
    #[test]
    fn a_recorded_member_whose_identity_no_longer_matches_is_not_signalled() {
        let mut owned = BTreeMap::new();
        // This test process, under a start time it does not have.
        let me = std::process::id();
        owned.insert(me, u64::MAX);
        assert!(
            survivors(&owned).is_empty(),
            "a stale identity must not read as a live owned member"
        );
        // And the signal path agrees: were it not identity-checked, this would
        // SIGKILL the test binary and the run would simply disappear.
        kill_if_identity_matches(me, u64::MAX);
        terminate_supervised_tree(None, &mut owned).expect("nothing of ours is running");
    }

    /// Init is not a member of anything, and 0 means "our own process group".
    #[cfg(unix)]
    #[test]
    fn the_supervisor_refuses_to_signal_init_or_its_own_group() {
        // Neither call may deliver a signal. Reaching the `libc::kill` below
        // either guard would terminate this test binary or the whole session.
        kill_if_identity_matches(0, 0);
        kill_if_identity_matches(1, 0);
    }
}
