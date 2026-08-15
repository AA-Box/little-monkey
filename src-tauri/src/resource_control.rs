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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceSample {
    pub wall_ms: u64,
    pub rss_bytes: Option<u64>,
    pub peak_rss_bytes: Option<u64>,
    pub process_count: Option<u32>,
    pub peak_process_count: Option<u32>,
    pub output_bytes: Option<u64>,
}

/// A measurement as a **row** stores it.
///
/// [`ResourceSample`] minus `wall_ms`, and the difference is the point: elapsed
/// time is derivable from `started_at_ms` and `exited_at_ms`, which every row
/// already carries, so storing it would be a third copy to disagree with the
/// other two. Reusing `ResourceSample` here meant serialising `wallMs: 0` beside
/// four honest `Option`s — an invented zero on the wire, which is the one thing
/// this whole reporting surface refuses to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rss_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_rss_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_process_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

/// The handle a **later session** can re-find a workload by.
///
/// # Why an identity and not a pid
///
/// A pid dies with the record's usefulness: after a restart, `native_pid` plus a
/// start time proves which *root* process a row named, and nothing at all about
/// the tree under it. On Linux that tree is held by a kernel object that outlives
/// this app entirely — a cgroup scope keeps enforcing `memory.max` after the
/// process that wrote it is gone — and the reclaim had no way to name it, so the
/// only thing a restart could do was signal a process group and hope.
///
/// So the containment names itself, durably, in a form the next session can
/// validate before acting on. Validation is the load-bearing half:
/// [`Self::parse`] refuses anything this app could not have created, because the
/// alternative is a stored string deciding which directory gets `cgroup.kill`
/// written to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainmentScope {
    /// A cgroup v2 scope directory, created by this app under a delegated
    /// subtree. Still holding its bound after a crash, and still killable.
    CgroupV2(std::path::PathBuf),
    /// A POSIX process group this workload leads. Proven at attach time by
    /// asking the kernel, never assumed from having requested one.
    ProcessGroup(u32),
    /// A Windows job object. Unnamed, so there is nothing to re-open — recorded
    /// anyway because *which* mechanism held a workload is the fact a restart
    /// needs: a job carries `KILL_ON_JOB_CLOSE`, so the tree died with the app
    /// that held the handle, and that is a proof of absence rather than a gap.
    WindowsJob,
}

/// The directory prefix every scope this app creates carries.
///
/// Duplicated from `resource_control_cgroup`'s `create` rather than shared,
/// because the two uses are adversarial: one mints names and the other decides
/// whether a *stored* name may be acted on. A validator that imported the minting
/// side would follow it if it ever widened.
const CGROUP_SCOPE_PREFIX: &str = "little-monkey-";

/// Where cgroup2 is mounted. A stored path outside this is not ours.
const CGROUP_MOUNT: &str = "/sys/fs/cgroup";

impl ContainmentScope {
    /// The stored form. Schemed, so a reader can tell a cgroup path from a pid
    /// without knowing which host wrote it.
    #[must_use]
    pub fn as_stored(&self) -> String {
        match self {
            ContainmentScope::CgroupV2(path) => format!("cgroup2:{}", path.display()),
            ContainmentScope::ProcessGroup(pgid) => format!("pgroup:{pgid}"),
            ContainmentScope::WindowsJob => "windows-job".to_string(),
        }
    }

    /// Read a stored scope back, refusing anything this app could not have
    /// written.
    ///
    /// A cgroup path is accepted only when it is absolute, lies under the cgroup2
    /// mount, has no `..` component, and its final component carries the prefix
    /// every scope this app mints is named with. Those four together are what
    /// stands between "reclaim the workload a previous session left" and "write
    /// `cgroup.kill` into a directory named by a string in a database".
    #[must_use]
    pub fn parse(stored: &str) -> Option<Self> {
        if stored == "windows-job" {
            return Some(ContainmentScope::WindowsJob);
        }
        if let Some(pgid) = stored.strip_prefix("pgroup:") {
            // 0 is "this process's own group" and 1 is init's; neither is a
            // workload, and both would be catastrophic to signal.
            return pgid
                .parse::<u32>()
                .ok()
                .filter(|pgid| *pgid > 1)
                .map(ContainmentScope::ProcessGroup);
        }
        let path = std::path::PathBuf::from(stored.strip_prefix("cgroup2:")?);
        if !path.is_absolute() || !path.starts_with(CGROUP_MOUNT) {
            return None;
        }
        if path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return None;
        }
        let named_by_us = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(CGROUP_SCOPE_PREFIX));
        named_by_us.then_some(ContainmentScope::CgroupV2(path))
    }
}

/// What actually held one process, recorded on its row.
///
/// # Why this is stored and not recomputed
///
/// "What would this machine use for a new process" and "what enforced *that*
/// process" are different questions, and the reporting surface answered the first
/// while claiming the second. A row written on a Linux laptop with a delegated
/// cgroup, read back on a Mac — or on the same machine after the delegation
/// changed — reported `supervisor · macOS` for a workload the kernel had held.
/// The class default and the effective number were durable; the mechanism that
/// enforced them was not, and it is the part a reader auditing a limit kill most
/// needs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Containment {
    /// The controller's own name, as it was on the host that ran this process.
    pub backend: String,
    pub tree_primitive: String,
    /// [`ContainmentScope::as_stored`], when the backend has a durable handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// The per-limit capability the controller reported at spawn, keyed by
    /// [`ProcessLimitKind::as_str`]. Stored whole rather than as a level per
    /// column: the *mechanism* string is the part a reader can go and look at.
    pub enforcement: std::collections::BTreeMap<String, LimitCapability>,
}

impl Containment {
    #[must_use]
    pub fn for_limit(&self, limit: ProcessLimitKind) -> Option<&LimitCapability> {
        self.enforcement.get(limit.as_str())
    }

    /// The typed scope, if the stored one still validates.
    #[must_use]
    pub fn parsed_scope(&self) -> Option<ContainmentScope> {
        ContainmentScope::parse(self.scope.as_deref()?)
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
    ///
    /// **Ownership is sticky.** Nothing is ever removed. A member that leaves the
    /// group, changes session or re-parents stays owned, because the question a
    /// budget asks is "did this workload start it", and that does not become
    /// false when the workload's descendant rearranges its own bookkeeping.
    owned: BTreeMap<u32, u64>,
    /// The process group and session the workload started in, captured at attach.
    ///
    /// Captured rather than looked up on each sample, and that is the whole
    /// point: both are read off the *root's* row in the process table, so the
    /// moment the root exits they stop being discoverable — which is precisely
    /// when a descendant that stayed in the group becomes unattributable. A
    /// number recorded while the root was alive keeps working after it is not.
    group: Option<u32>,
    session: Option<u32>,
    /// The job handle a managed Windows spawn created the workload into.
    ///
    /// Held for the controller's life because a job dies with its last handle
    /// and `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` takes the tree with it: dropping
    /// this at the end of the spawn would kill the workload the instant it was
    /// contained. Every owner already keeps its controller for as long as the
    /// workload may run, which is what makes this the right place for it.
    #[cfg(windows)]
    spawn_job: Option<crate::sandbox_windows::JobConfinement>,
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
            group: None,
            session: None,
            #[cfg(windows)]
            spawn_job: None,
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

    /// The durable handle a later session could reclaim this workload by.
    ///
    /// `None` until [`Self::attach`] has run for the process-group case, because
    /// a group is only ours if the kernel says the root leads one — `process_group(0)`
    /// is a *request*, and recording an unverified pgid would hand a restart a
    /// number to signal that may belong to somebody else's group.
    #[must_use]
    pub fn scope(&self) -> Option<ContainmentScope> {
        match &self.backend {
            #[cfg(target_os = "linux")]
            Backend::Cgroup(scope) => Some(ContainmentScope::CgroupV2(scope.path().to_path_buf())),
            #[cfg(windows)]
            Backend::Job(_) => Some(ContainmentScope::WindowsJob),
            Backend::Supervisor => self.leading_process_group(),
        }
    }

    /// The root's process group, but only where the root actually leads it.
    #[cfg(unix)]
    fn leading_process_group(&self) -> Option<ContainmentScope> {
        let root = self.root?;
        // The group captured at attach, when it was certainly readable, rather
        // than one queried now — a root that has since exited answers nothing.
        if let Some(group) = self.group.filter(|group| *group == root.pid) {
            return Some(ContainmentScope::ProcessGroup(group));
        }
        let pid = libc::pid_t::try_from(root.pid).ok()?;
        // Safe: a pure query about one pid, with no side effect.
        let pgid = unsafe { libc::getpgid(pid) };
        (pgid == pid).then(|| ContainmentScope::ProcessGroup(root.pid))
    }

    #[cfg(not(unix))]
    fn leading_process_group(&self) -> Option<ContainmentScope> {
        // Windows reaches the supervisor only when the job could not be created,
        // and the parent-link closure it falls back to is not a durable handle:
        // there is nothing a later session could re-open.
        None
    }

    /// Everything a row needs to say what held this process, after the fact.
    #[must_use]
    pub fn containment(&self) -> Containment {
        let capabilities = self.capabilities();
        let enforcement = ProcessLimitKind::ALL
            .iter()
            .map(|limit| {
                (
                    limit.as_str().to_string(),
                    capabilities.for_limit(*limit).clone(),
                )
            })
            .collect();
        Containment {
            backend: capabilities.backend,
            tree_primitive: capabilities.tree_primitive,
            scope: self.scope().map(|scope| scope.as_stored()),
            enforcement,
        }
    }

    /// Install everything that must exist *before* the workload starts.
    ///
    /// The ordering rule K4 turns on: where a platform offers a pre-execution
    /// mechanism, there must be no window in which user code runs outside it.
    /// On Linux the child joins its cgroup between `fork` and `exec`. The
    /// supervisor has no pre-execution mechanism, which is exactly why it reports
    /// itself as supervised.
    ///
    /// **Windows installs nothing here**, and that is the platform's shape rather
    /// than an omission: a job is applied to a process, not carried into one by a
    /// `Command`. A spawn site that calls `CreateProcessW` itself gets the strong
    /// ordering — created suspended, assigned, then resumed, which is what
    /// [`Self::windows_job_for_spawn`] exists for and what every agent shell uses.
    /// A site that spawns through `tokio::process` cannot, so [`Self::attach`]
    /// makes the assignment immediately after creation and
    /// [`Self::adopt`] states the window that leaves.
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

    /// Spawn `command` so that nothing it does happens outside its bound.
    ///
    /// # The one entry point every managed native workload uses
    ///
    /// The two platforms need opposite things and this is where that stops being
    /// each owner's problem. On Unix the containment is installed by the child
    /// between `fork` and `exec`, so this is [`Self::prepare_tokio`] followed by
    /// an ordinary spawn. On Windows a job is applied *to* a process and nothing
    /// a `Command` carries can put it there, so the child is created suspended,
    /// assigned, read back, and only then resumed — see
    /// [`crate::managed_spawn_windows`].
    ///
    /// An owner that spawns through here cannot get the ordering wrong, which is
    /// the point: the owners that did it themselves each left a window in which
    /// the workload's first instructions ran under no job at all.
    ///
    /// [`Self::prepare_tokio`] must still have been called first — it is what
    /// installs the Unix half — and [`Self::attach`] must still be called after,
    /// because recording the identity and refusing an uncontained workload are
    /// separate obligations from establishing the containment.
    pub fn spawn_contained_tokio(
        &mut self,
        command: &mut tokio::process::Command,
    ) -> io::Result<tokio::process::Child> {
        #[cfg(windows)]
        {
            let job = self.windows_job_for_spawn()?;
            let child = crate::managed_spawn_windows::spawn_suspended_tokio(&job, command, 0)?;
            self.spawn_job = Some(job);
            Ok(child)
        }
        #[cfg(not(windows))]
        {
            command.spawn()
        }
    }

    /// [`Self::spawn_contained_tokio`] for an owner that must build a `std`
    /// command.
    pub fn spawn_contained_std(
        &mut self,
        command: &mut std::process::Command,
    ) -> io::Result<std::process::Child> {
        #[cfg(windows)]
        {
            let job = self.windows_job_for_spawn()?;
            let child = crate::managed_spawn_windows::spawn_suspended_std(&job, command, 0)?;
            self.spawn_job = Some(job);
            Ok(child)
        }
        #[cfg(not(windows))]
        {
            command.spawn()
        }
    }

    /// Bring a process that is already running under this containment.
    ///
    /// # When this is the right call, and when it is a weakening
    ///
    /// [`Self::prepare_tokio`]/[`Self::prepare_std`] are the strong form: the
    /// containment exists before the workload's first instruction, so there is no
    /// window at all. Every shell path uses them and must keep using them.
    ///
    /// This is for an owner that cannot build its child's `Command` through those
    /// entry points — the browser session, whose Chromium is launched by a
    /// long-standing spawn site with its own argv, environment and stdio — and,
    /// on Windows, for every owner that spawns through `tokio::process`, because
    /// there is no pre-creation hook there to hang a job assignment on. There the
    /// assignment is made from the parent immediately after `CreateProcess`
    /// returns, which leaves a window measured in microseconds during which a
    /// descendant created by the new process would be outside the job. That is a
    /// narrower guarantee than the shells get on Windows — they go through
    /// `spawn_confined_child`, which assigns the job while the process is still
    /// suspended — and it is stated as such in `docs/limitations.md` rather than
    /// glossed as equivalent.
    ///
    /// [`Self::attach`] calls this itself, so an owner only needs it directly
    /// when the assignment has to happen earlier than the attach.
    ///
    /// On Unix there is no such window: the cgroup migration and the process
    /// group are both installed by `prepare_std` before `exec`, and this call is
    /// the idempotent re-assertion of a membership that already holds.
    ///
    /// Failure is not fatal by itself — [`Self::attach`] is what reads the
    /// membership back and refuses a workload that is not inside its bound.
    pub fn adopt(&self, #[allow(unused_variables)] pid: u32) -> io::Result<()> {
        match &self.backend {
            #[cfg(target_os = "linux")]
            Backend::Cgroup(scope) => scope.adopt(pid),
            #[cfg(windows)]
            Backend::Job(job) => job.adopt(pid),
            // The supervisor's containment is the process group, which only the
            // child can install for itself before `exec`; `prepare_std` did it.
            Backend::Supervisor => Ok(()),
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
        // While the root is certainly alive, which is the only time either is
        // readable — and only where the root *leads* the group or the session.
        //
        // The guard is not a nicety. A pgid or a sid the root merely belongs to
        // is its **parent's**, which is this app: unioning it into the owned set
        // would make a termination sweep every process in this app's own group or
        // login session. Equality with the root's pid is what distinguishes "the
        // primitive this workload was given" from "the primitive it inherited",
        // and every supervised spawn is given one by `prepare_supervised`.
        self.group = process_tree::snapshot()
            .ok()
            .and_then(|nodes| {
                nodes
                    .iter()
                    .find(|node| node.pid == pid)
                    .map(|node| node.process_group_id)
            })
            .filter(|group| *group == pid);
        self.session = process_tree::session_of(pid).filter(|session| *session == pid);

        // Install the containment for the backends that cannot install it before
        // the first instruction, then read it back below.
        //
        // On Unix this is an idempotent re-assertion: `prepare_std`/`prepare_tokio`
        // already joined the cgroup and set the process group from inside the
        // child, before `exec`. On Windows it is the assignment itself, because
        // there is nothing a `Command` can carry into `CreateProcess` — the job is
        // applied either between `CREATE_SUSPENDED` and `ResumeThread` at a spawn
        // site that builds the process itself, or from here.
        //
        // It lives in `attach` rather than at each call site because leaving it to
        // the caller is a bug that only appears on one platform: `prepare_tokio`
        // returns `Ok` on Windows with nothing installed, so an owner that called
        // prepare/spawn/attach — the documented order — got a child in no job at
        // all, and the `confirm_containment` below correctly refused to start it.
        // Every owner that builds its own `Command` was in that state.
        //
        // The error is deliberately dropped: a process already inside this job or
        // scope answers a second assignment with a refusal on some hosts, and the
        // question that decides whether the workload may run is the membership
        // read below, not this write.
        let _ = self.adopt(pid);

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
    ///
    /// # Retention and termination are two bounds, and this is not the second one
    ///
    /// `max_output_bytes` is a **retention** bound in every production path: the
    /// capture buffer is front-truncated as bytes arrive, both pipes keep
    /// draining, and a command that prints past it finishes normally with the
    /// record carrying what it produced alongside what was kept. A verbose build
    /// printing past a model's context cap has done nothing wrong, and ending it
    /// would make the bound something people switch off.
    ///
    /// So nothing in production calls this, and that is a decision rather than an
    /// omission — [`Self::breach`] would terminate the tree on the next tick if it
    /// did. The entry point exists because the *choice* belongs to the owner: a
    /// caller that genuinely wants "stop producing at N bytes" can feed the
    /// running total and get exactly that, and the mechanism should not have to be
    /// invented at that point.
    /// `workspace_shell::a_command_past_its_output_bound_is_truncated_and_still_completes`
    /// pins the policy that ships.
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
        for pid in process_tree::tree_members_of_any_in(
            &nodes,
            &roots,
            &self.group.into_iter().collect::<Vec<_>>(),
            self.session,
        ) {
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
                let usage = supervised_tree_usage(root, &self.owned, self.group, self.session)?;
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

    /// Ask the mechanism whether it refused, and reclaim the tree if it did.
    ///
    /// **This is the call every owner must make before deciding why a workload
    /// ended.** A kernel-held bound does not announce itself by making a
    /// measurement exceed a budget — it announces itself by refusing work, after
    /// which the workload usually dies of an error it cannot explain. An owner
    /// that reads the exit status without asking here records the strongest
    /// enforcement this app has as an anonymous failure.
    ///
    /// Returning `Some` means the tree has already been terminated: a breach that
    /// is reported without reclaiming the workload has reclaimed nothing, and
    /// leaving that to each caller is how one of them forgets.
    pub fn mechanism_breach(&mut self, now_ms: i64) -> io::Result<Option<LimitBreach>> {
        let Some(breach) = self.poll_limit_events(now_ms)? else {
            return Ok(None);
        };
        self.terminate_tree()?;
        Ok(Some(breach))
    }

    /// The whole "is this workload still allowed to run" question, in one call.
    ///
    /// Every owner of a long-running controlled workload asks this on its own
    /// tick — [`run_under`] for a foreground shell, the background shell's exit
    /// watcher, the browser session's watchdog — so the order of the two tests
    /// is decided once here rather than three times at three call sites. The
    /// order is the load-bearing part: the mechanism's own accounting is
    /// consulted *first* and unconditionally, because where a kernel holds the
    /// bound that is the only test that can ever be true (see [`LimitEvent`]).
    ///
    /// A [`ResourceCheck::Breached`] has already torn the owned tree down.
    pub fn check(&mut self, now_ms: i64) -> io::Result<ResourceCheck> {
        if let Some(breach) = self.mechanism_breach(now_ms)? {
            // Taken after the termination rather than before it: what the caller
            // wants recorded is the last measurement that exists, and on a host
            // where the workload is already gone there is honestly none.
            let sample = self.sample().ok().flatten();
            return Ok(ResourceCheck::Breached { breach, sample });
        }
        let Some(sample) = self.sample()? else {
            return Ok(ResourceCheck::Gone);
        };
        if let Some(breach) = self.breach(&sample, now_ms) {
            self.terminate_tree()?;
            return Ok(ResourceCheck::Breached {
                breach,
                sample: Some(sample),
            });
        }
        Ok(ResourceCheck::Running(sample))
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
            Backend::Supervisor => {
                terminate_supervised_tree(self.root, &mut self.owned, self.group, self.session)
            }
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
    group: Option<u32>,
    session: Option<u32>,
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
        &process_tree::tree_members_of_any_in(
            &nodes,
            &roots,
            &group.into_iter().collect::<Vec<_>>(),
            session,
        ),
    ))
}

/// Wall-clock milliseconds, for stamping a breach.
///
/// A clock that will not read is not a reason to refuse to record a limit kill,
/// so this degrades to zero rather than erroring: an unstamped breach still names
/// the limit, the configured value and the measurement, which is what the record
/// is for.
#[must_use]
pub fn now_ms_for_breach() -> i64 {
    now_ms()
}

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

/// What one tick of supervision found.
///
/// The three answers a controller can give about a live workload, as a value, so
/// that every owner acts on the same three and cannot invent a fourth by
/// combining the primitives in its own order. Before this existed the foreground
/// path polled the mechanism and then sampled, and the background path only
/// sampled — which meant a kernel that refused the thirteenth `fork` ended a
/// background command with an unexplained error while the identical foreground
/// command was correctly recorded as `limit_exceeded`.
#[derive(Debug)]
pub enum ResourceCheck {
    /// Inside every bound. Carries the measurement, for the ledger.
    Running(ResourceSample),
    /// A bound fired and the owned tree has already been terminated.
    Breached {
        breach: LimitBreach,
        /// The last measurement, or `None` when the workload was already gone by
        /// the time the mechanism's evidence was read — which is the ordinary
        /// case for a kernel that OOM-killed its member. A sample of zeros would
        /// be a measurement nobody took.
        sample: Option<ResourceSample>,
    },
    /// The workload is gone and nothing fired.
    Gone,
}

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
    run_under_observed(controller, work, |_| {}).await
}

/// [`run_under`] with a look at every measurement as it is taken.
///
/// The observer exists because a sample that only reaches the caller *after* the
/// workload ends can never answer "what is this process holding right now",
/// which is the question a live Processes panel is for. Before this, the final
/// sample was the only one anything outside the loop ever saw, so a build sitting
/// at 6 GiB for ten minutes displayed nothing at all until it finished.
///
/// Deliberately synchronous and infallible: it runs on the sampling tick, and a
/// bookkeeping write that could delay or fail the next resource check would put
/// the ledger in front of the bound. Every implementation is expected to be
/// fail-soft in the way [`crate::bounded_execution::BoundedExecution`] is.
pub async fn run_under_observed<F>(
    controller: &mut ResourceController,
    work: F,
    mut observe: impl FnMut(&ResourceSample),
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
                if let Some(breach) = controller.mechanism_breach(now_ms())? {
                    return Ok(Supervised::Breached(breach, last));
                }
                return Ok(Supervised::Completed(output, last));
            }
            _ = ticker.tick() => {
                match controller.check(now_ms())? {
                    ResourceCheck::Breached { breach, sample } => {
                        return Ok(Supervised::Breached(breach, sample.unwrap_or(last)));
                    }
                    ResourceCheck::Running(sample) => {
                        observe(&sample);
                        last = sample;
                    }
                    // The workload is gone but `work` has not resolved yet —
                    // usually a pipe still draining. Keep waiting for it rather
                    // than reporting a breach against a corpse.
                    ResourceCheck::Gone => continue,
                }
            }
        }
    }
}

/// The limit set to ask a host "what would you hold a workload with".
///
/// **Not `EffectiveLimits::default()`, and that distinction is a real bug this
/// exists to prevent.** A backend installs only the bounds it was asked for: a
/// cgroup scope with nothing to enforce is pure cost, so `CgroupScope::create`
/// correctly declines an empty limit set and the controller falls back to the
/// supervisor. A capability probe built from an empty set therefore reports
/// "supervisor" on a machine whose every real workload runs under a kernel
/// bound — which is the reporting surface saying the opposite of the truth.
///
/// So a probe asks with the bounds a real workload carries. The foreground
/// shell's class defaults are that shape: a memory ceiling and a process ceiling,
/// the two resources every backend here has an answer for.
#[must_use]
pub fn probe_limits() -> EffectiveLimits {
    EffectiveLimits::resolve(&[LimitLayer::new(
        LimitSource::ClassDefault,
        crate::process_table::ProcessKind::ForegroundShell.default_limits(),
    )])
}

/// The backend this host has been *provisioned* to use, when something says so.
///
/// A hosted CI runner is not a systemd user session, so nothing is delegated to
/// it, `CgroupScope::create` correctly declines, and every limit test passes
/// against the supervisor — on the leg whose entire purpose is to prove the
/// kernel path works. That is a green build meaning "the kernel path is still
/// never executed anywhere", which is the failure mode a fallback quietly
/// produces.
///
/// So the CI step that delegates a cgroup also states that it did, and the tests
/// read that statement here and refuse the fallback. A developer machine sets
/// nothing and keeps whichever backend it really has.
#[cfg(test)]
#[must_use]
pub(crate) fn required_backend() -> Option<&'static str> {
    if std::env::var_os("LITTLE_MONKEY_REQUIRE_CGROUP_BACKEND").is_some() {
        return Some("cgroup v2");
    }
    None
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
    group: Option<u32>,
    session: Option<u32>,
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
            expand_owned(owned, group, session);
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
fn expand_owned(owned: &mut BTreeMap<u32, u64>, group: Option<u32>, session: Option<u32>) {
    let roots = survivors(owned);
    if roots.is_empty() {
        return;
    }
    let Ok(nodes) = process_tree::snapshot() else {
        return;
    };
    for pid in process_tree::tree_members_of_any_in(
        &nodes,
        &roots,
        &group.into_iter().collect::<Vec<_>>(),
        session,
    ) {
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
    _group: Option<u32>,
    _session: Option<u32>,
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

    /// A host provisioned for one backend must actually select it.
    ///
    /// The gate behind the Linux CI leg that exists to run the kernel path: with
    /// a delegated hierarchy in place, `ResourceController::new` falling back to
    /// the supervisor is the difference between "cgroup v2 compiles" and "cgroup
    /// v2 works", and every other assertion in this suite passes either way.
    #[test]
    fn a_host_provisioned_for_a_backend_selects_that_backend() {
        let Some(required) = required_backend() else {
            return;
        };
        // Both bounds, because a scope installs only what it was asked for: a
        // memory-only request leaves `pids.max` unwritten and the capability
        // answer correctly says nothing is holding a process ceiling.
        let capabilities = ResourceController::new(probe_limits()).capabilities();
        assert_eq!(
            capabilities.backend, required,
            "this host was provisioned to exercise {required}, so falling back leaves the \
             kernel path unexecuted"
        );
        // The two resources this backend is provisioned for, held by the kernel
        // rather than by the sampling loop — which is the property the whole leg
        // is for.
        for limit in [ProcessLimitKind::Memory, ProcessLimitKind::ChildProcesses] {
            assert_eq!(
                capabilities.for_limit(limit).level(),
                Some(EnforcementLevel::Kernel),
                "{} must be kernel-held under {required}: {:?}",
                limit.as_str(),
                capabilities.for_limit(limit)
            );
        }
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

    // --- K4 on Windows: the job object actually refuses, and says so ---------
    //
    // The Unix suite proves the shells' bounds against real trees through
    // `run_to_output`. Windows could not reuse it: the confined spawn there goes
    // through an AppContainer and a `cmd`-flavoured command line, so a test
    // written in shell would be testing the command language as much as the
    // bound. These drive the controller's own contract instead — prepare, adopt,
    // attach, check — against real child processes, which is the same sequence
    // `spawn_windows` performs and the only part that is platform-specific.
    //
    // Both bounds here are the *invisible* kind: a job refuses rather than
    // letting a measurement pass the cap, so `observed > configured` is false
    // forever and the only evidence is the completion port's message. That is
    // exactly what these assert.

    /// Holds `LITTLE_MONKEY_WINDOWS_IDLE_MS` milliseconds, so a parent can count
    /// it.
    #[cfg(windows)]
    #[test]
    fn windows_idle_child() {
        let Some(ms) = std::env::var("LITTLE_MONKEY_WINDOWS_IDLE_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
        else {
            return;
        };
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }

    /// Waits, then spawns `LITTLE_MONKEY_WINDOWS_FORK_CHILDREN` idle children.
    ///
    /// The wait is what makes the test deterministic rather than a race: the
    /// parent adopts this process into the job immediately after
    /// `CreateProcess` returns, and nothing may be created here before that has
    /// happened, or the descendants would be outside the bound for a reason the
    /// bound is not responsible for.
    #[cfg(windows)]
    #[test]
    fn windows_fork_child() {
        let Some(count) = std::env::var("LITTLE_MONKEY_WINDOWS_FORK_CHILDREN")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
        else {
            return;
        };
        std::thread::sleep(std::time::Duration::from_millis(750));
        let executable = std::env::current_exe().expect("the test binary knows its own path");
        let mut children = Vec::new();
        for _ in 0..count {
            let spawned = std::process::Command::new(&executable)
                .args([
                    "--exact",
                    "resource_control::tests::windows_idle_child",
                    "--test-threads=1",
                ])
                .env("LITTLE_MONKEY_WINDOWS_IDLE_MS", "20000")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            match spawned {
                Ok(child) => children.push(child),
                // The refusal this whole test is about. Stop asking; the parent
                // reads it from the kernel's own notification.
                Err(_) => break,
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(20));
        for mut child in children {
            let _ = child.kill();
        }
    }

    /// Allocates and *touches* `LITTLE_MONKEY_WINDOWS_MEMORY_MIB` mebibytes.
    ///
    /// Touched, not merely reserved: a job's memory limit counts committed
    /// memory, and an untouched `Vec::with_capacity` is not committed on every
    /// allocator path — a hog that only reserved would leave the bound looking
    /// broken.
    #[cfg(windows)]
    #[test]
    fn windows_memory_hog_child() {
        let Some(mib) = std::env::var("LITTLE_MONKEY_WINDOWS_MEMORY_MIB")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
        else {
            return;
        };
        std::thread::sleep(std::time::Duration::from_millis(750));
        let mut held: Vec<Vec<u8>> = Vec::new();
        for _ in 0..mib {
            let mut block = vec![0_u8; 1024 * 1024];
            for page in block.chunks_mut(4096) {
                page[0] = 1;
            }
            held.push(block);
        }
        std::thread::sleep(std::time::Duration::from_secs(20));
        assert_eq!(held.len(), mib);
    }

    /// Spawn `workload` under `controller`, in the ordering `spawn_windows` uses.
    #[cfg(windows)]
    fn windows_child_under(
        controller: &mut ResourceController,
        workload: &str,
        variable: &str,
        value: &str,
    ) -> std::process::Child {
        let executable = std::env::current_exe().expect("the test binary knows its own path");
        let mut command = std::process::Command::new(executable);
        command
            .args([
                "--exact",
                &format!("resource_control::tests::{workload}"),
                "--test-threads=1",
            ])
            .env(variable, value)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        controller
            .prepare_std(&mut command)
            .expect("a Windows job needs nothing before the spawn");
        let child = command.spawn().expect("the workload spawns");
        controller
            .adopt(child.id())
            .expect("the job takes the process it is about to bound");
        controller.attach(child.id()).expect("the job contains it");
        child
    }

    /// A cgroup bound outlives the supervisor that created it.
    ///
    /// The restart question, asked of the mechanism rather than of the process
    /// table: after this app disappears, is a shell tree it left running still
    /// bounded? On Linux the answer is yes and it is the reason startup reclaim
    /// exists — the kernel does not care that the process which wrote `memory.max`
    /// is gone, so the tree keeps its ceiling and loses only its supervisor.
    ///
    /// `mem::forget` is the crash: `CgroupScope::drop` terminates the tree and
    /// removes the directory, which is what an *orderly* exit does and exactly
    /// what a `SIGKILL` does not get to do. Skipped where no delegated hierarchy
    /// exists, since there is then no kernel scope whose survival to ask about.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_cgroup_scope_outlives_the_supervisor_that_created_it() {
        const MEMORY: u64 = 512 * 1024 * 1024;
        let effective = EffectiveLimits::resolve(&[LimitLayer::new(
            LimitSource::UserOverride,
            ProcessLimits {
                max_memory_bytes: Some(MEMORY),
                max_child_processes: Some(16),
                ..ProcessLimits::default()
            },
        )]);
        let mut controller = ResourceController::new(effective);
        if controller.capabilities().backend != "cgroup v2" {
            // A skip, except on the leg whose whole purpose is this path: there a
            // fallback means the kernel backend was never exercised, and a silent
            // return would report that as a pass.
            assert!(
                required_backend().is_none(),
                "this host was provisioned to exercise cgroup v2 and fell back to {}",
                controller.capabilities().backend
            );
            return;
        }

        let mut command = std::process::Command::new("/bin/sh");
        command
            .args(["-c", "sleep 30"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        controller
            .prepare_std(&mut command)
            .expect("the scope joins before exec");
        let mut child = command.spawn().expect("the workload spawns");
        controller
            .attach(child.id())
            .expect("the scope contains it");

        // The scope's own directory, read from the kernel's record of where the
        // child actually is rather than from anything this process remembers.
        let membership = std::fs::read_to_string(format!("/proc/{}/cgroup", child.id()))
            .expect("a running child has a cgroup line");
        let relative = membership
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .expect("cgroup v2 puts the unified hierarchy on the 0:: line")
            .trim()
            .to_string();
        let scope =
            std::path::PathBuf::from("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
        let installed =
            std::fs::read_to_string(scope.join("memory.max")).expect("memory.max reads");
        assert_eq!(
            installed.trim(),
            MEMORY.to_string(),
            "the scope must hold the effective number, not the kernel's default"
        );

        // The supervisor disappears without unwinding.
        std::mem::forget(controller);

        let after = std::fs::read_to_string(scope.join("memory.max"))
            .expect("the scope survives the process that made it");
        assert_eq!(
            after.trim(),
            MEMORY.to_string(),
            "a kernel-held bound is still held once nothing is watching it"
        );
        assert!(
            crate::os_signal::process_is_alive(child.id()),
            "the workload is still running; what it lost is its supervisor, not its ceiling"
        );

        // What startup reclaim would do, done by hand because this test is the
        // session that crashed.
        let _ = child.kill();
        let _ = child.wait();
        for _ in 0..50 {
            if std::fs::remove_dir(&scope).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// A Windows job takes its tree with it when the last handle closes.
    ///
    /// The other half of the restart question, and the opposite answer to
    /// Linux's: `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` means an app that dies —
    /// crash included, because the kernel closes handles a dead process held —
    /// leaves no unmanaged descendants behind at all. There is nothing for a later
    /// session to reclaim, which is a stronger property than "the bound is gone"
    /// and is why it is asserted rather than assumed.
    /// The precondition is read from the job's own accounting rather than from a
    /// liveness probe on the pid. Both answer "is it running", and only one of
    /// them answers the question this test needs — whether the *job* holds a live
    /// member — which is also the reading that survives a runner slow enough for a
    /// freshly spawned test binary not to be schedulable yet. The first version
    /// asserted `process_is_alive` once, immediately after the spawn, and failed
    /// on CI for that reason while every assertion it existed to make went untested.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_windows_job_takes_its_tree_with_it_when_the_supervisor_goes() {
        // A process ceiling and no memory ceiling, deliberately. A job's memory
        // limit is a *commit* charge over the whole job, and the workload here is
        // a second copy of this debug test binary — which is not a workload whose
        // commit anyone chose. The first version of this test set 512 MiB and the
        // child died between the attach and the next statement, which read as the
        // precondition being wrong rather than as the bound doing exactly what it
        // was told. The two tests that keep an idle child alive for twenty seconds
        // on CI state a process ceiling only; these state the same.
        let effective = EffectiveLimits::resolve(&[LimitLayer::new(
            LimitSource::UserOverride,
            ProcessLimits {
                max_child_processes: Some(8),
                ..ProcessLimits::default()
            },
        )]);
        let mut controller = ResourceController::new(effective);
        assert_eq!(
            controller.capabilities().backend,
            "windows job object",
            "every Windows host can create a job; a fallback here is a real failure"
        );
        // Twenty seconds of doing nothing, so anything that ends it inside the
        // wait below is the job closing rather than the workload finishing.
        let mut child = windows_idle_child_under(&mut controller, "20000");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut held = false;
        while std::time::Instant::now() < deadline {
            let sample = controller.sample().expect("the job reports its accounting");
            if sample.is_some_and(|sample| sample.process_count.is_some_and(|count| count >= 1)) {
                held = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            held,
            "the job has to hold a live member before the handle closes, or this proves nothing"
        );

        drop(controller);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut ended = false;
        while std::time::Instant::now() < deadline {
            if child.try_wait().is_ok_and(|status| status.is_some()) {
                ended = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let _ = child.kill().await;
        assert!(
            ended,
            "closing the last job handle must end the tree; a survivor here is a descendant no \
             later session could find"
        );
    }

    /// A contained idle child, spawned the way an owner with its own `Command`
    /// spawns one: prepare, spawn, attach, and no explicit `adopt`.
    #[cfg(windows)]
    fn windows_idle_child_under(
        controller: &mut ResourceController,
        idle_ms: &str,
    ) -> tokio::process::Child {
        let executable = std::env::current_exe().expect("the test binary knows its own path");
        let mut command = tokio::process::Command::new(executable);
        command
            .args([
                "--exact",
                "resource_control::tests::windows_idle_child",
                "--test-threads=1",
            ])
            .env("LITTLE_MONKEY_WINDOWS_IDLE_MS", idle_ms)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        controller
            .prepare_tokio(&mut command)
            .expect("a Windows job needs nothing before the spawn");
        let child = command.spawn().expect("the workload spawns");
        let pid = child.id().expect("a freshly spawned child has a pid");
        controller
            .attach(pid)
            .expect("attaching a running child must contain it, not refuse it");
        child
    }

    /// An owner that only prepares, spawns and attaches gets a contained child.
    ///
    /// The regression this pins is Windows-shaped and was invisible everywhere
    /// else. `prepare_tokio`/`prepare_std` install the containment on Linux and
    /// under the supervisor, and on Windows there is nothing they *can* install —
    /// a job is applied to a process, not carried into one by a `Command`. So an
    /// owner that followed the documented order and did not also call `adopt`
    /// spawned a child in no job at all, `attach` correctly refused to let it run,
    /// and the whole path failed closed with "no kernel bound is holding it".
    /// Every owner with its own `Command` was in that state: the verify runner,
    /// the hook runner, and any added later.
    ///
    /// The helpers above call `adopt` explicitly, which is why the job tests
    /// passed while the production paths did not — so this test deliberately does
    /// not, and spawns through `tokio::process` because that is what those owners
    /// build.
    #[cfg(windows)]
    #[tokio::test]
    async fn an_owner_that_only_prepares_and_attaches_gets_a_contained_child() {
        // A process ceiling and no memory ceiling, deliberately. A job's memory
        // limit is a *commit* charge over the whole job, and the workload here is
        // a second copy of this debug test binary — which is not a workload whose
        // commit anyone chose. The first version of this test set 512 MiB and the
        // child died between the attach and the next statement, which read as the
        // precondition being wrong rather than as the bound doing exactly what it
        // was told. The two tests that keep an idle child alive for twenty seconds
        // on CI state a process ceiling only; these state the same.
        let effective = EffectiveLimits::resolve(&[LimitLayer::new(
            LimitSource::UserOverride,
            ProcessLimits {
                max_child_processes: Some(8),
                ..ProcessLimits::default()
            },
        )]);
        let mut controller = ResourceController::new(effective);
        assert_eq!(
            controller.capabilities().backend,
            "windows job object",
            "every Windows host can create a job; a fallback here is a real failure"
        );

        // No explicit `adopt`, which is the whole point: prepare, spawn, attach.
        let mut child = windows_idle_child_under(&mut controller, "4000");
        // Not merely "attach returned Ok": the sample comes from the job's own
        // accounting, so a non-empty one is the kernel agreeing that the process
        // is inside the bound.
        let sample = controller
            .sample()
            .expect("the job reports its accounting")
            .expect("a running child is not a gone workload");
        assert!(
            sample.process_count.is_some_and(|count| count >= 1),
            "the job should be accounting for the child it contains: {sample:?}"
        );

        controller.terminate_tree().expect("the tree is reclaimed");
        let _ = child.wait().await;
    }

    /// Poll the controller until it reports a breach, or give up.
    #[cfg(windows)]
    fn windows_wait_for_breach(controller: &mut ResourceController) -> LimitBreach {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match controller.check(now_ms()).expect("the controller checks") {
                ResourceCheck::Breached { breach, .. } => return breach,
                ResourceCheck::Running(_) | ResourceCheck::Gone => {}
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the job never reported the limit it was configured with"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// `ActiveProcessLimit`, end to end: the effective number is what is
    /// installed, the kernel refuses the process that would exceed it, and the
    /// refusal arrives as a typed breach rather than as an unexplained failure.
    #[cfg(windows)]
    #[test]
    fn a_windows_job_refuses_the_process_past_its_ceiling_and_names_the_refusal() {
        let effective = EffectiveLimits::resolve(&[LimitLayer::new(
            LimitSource::UserOverride,
            ProcessLimits {
                max_child_processes: Some(3),
                ..ProcessLimits::default()
            },
        )]);
        let mut controller = ResourceController::new(effective);
        let capabilities = controller.capabilities();
        assert_eq!(
            capabilities.backend, "windows job object",
            "every Windows host can create a job; a fallback here is a real failure"
        );
        assert!(
            capabilities
                .child_processes
                .mechanism()
                .is_some_and(|mechanism| mechanism.contains(" at 3,")),
            "the job must be built from the effective limit, not from the fixed guardrail: {:?}",
            capabilities.child_processes
        );

        // Ten wanted against a ceiling of three: the fourth `CreateProcess` is
        // refused and `ActiveProcesses` stays at three.
        let mut child = windows_child_under(
            &mut controller,
            "windows_fork_child",
            "LITTLE_MONKEY_WINDOWS_FORK_CHILDREN",
            "10",
        );
        let breach = windows_wait_for_breach(&mut controller);
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(breach.limit, "max_child_processes");
        assert_eq!(breach.configured, 3);
        assert_eq!(breach.backend, "windows job object");
        assert_eq!(breach.level, "kernel");
        assert!(
            breach.observed <= breach.configured,
            "a refusing kernel holds the count at the cap: {breach:?}"
        );
        let evidence = breach
            .evidence
            .expect("a kernel breach carries its counter");
        assert!(
            evidence.contains("JOB_OBJECT_MSG_ACTIVE_PROCESS_LIMIT"),
            "{evidence}"
        );
    }

    /// `JobMemoryLimit`, end to end, on the same terms.
    #[cfg(windows)]
    #[test]
    fn a_windows_job_refuses_the_commit_past_its_memory_ceiling_and_names_the_refusal() {
        let effective = EffectiveLimits::resolve(&[LimitLayer::new(
            LimitSource::UserOverride,
            ProcessLimits {
                max_memory_bytes: Some(192 * 1024 * 1024),
                ..ProcessLimits::default()
            },
        )]);
        let mut controller = ResourceController::new(effective);
        let capabilities = controller.capabilities();
        assert_eq!(capabilities.backend, "windows job object");
        assert!(
            capabilities
                .memory
                .mechanism()
                .is_some_and(|mechanism| mechanism.contains(&(192 * 1024 * 1024).to_string())),
            "the job's memory ceiling must be the effective limit: {:?}",
            capabilities.memory
        );

        let mut child = windows_child_under(
            &mut controller,
            "windows_memory_hog_child",
            "LITTLE_MONKEY_WINDOWS_MEMORY_MIB",
            "512",
        );
        let breach = windows_wait_for_breach(&mut controller);
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(breach.limit, "max_memory_bytes");
        assert_eq!(breach.configured, 192 * 1024 * 1024);
        assert_eq!(breach.backend, "windows job object");
        assert_eq!(breach.level, "kernel");
        let evidence = breach
            .evidence
            .expect("a kernel breach carries its counter");
        assert!(
            evidence.contains("JOB_OBJECT_MSG_JOB_MEMORY_LIMIT"),
            "{evidence}"
        );
    }

    /// A Windows child that finishes instantly is gone, not uncontained.
    ///
    /// The same distinction the Unix suite pins, on the backend where the
    /// membership read is a different syscall: `IsProcessInJob` against a pid the
    /// kernel has already released cannot say yes, and treating that as an escape
    /// would refuse every short command — a `printf`, an `exec` the confinement
    /// denied — while the record claimed a containment failure that never
    /// happened.
    #[cfg(windows)]
    #[test]
    fn attaching_to_a_finished_windows_child_reports_it_gone_not_uncontained() {
        let mut controller = ResourceController::new(EffectiveLimits::resolve(&[LimitLayer::new(
            LimitSource::UserOverride,
            ProcessLimits {
                max_child_processes: Some(8),
                ..ProcessLimits::default()
            },
        )]));
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("a trivial child spawns");
        let pid = child.id();
        let _ = child.wait();

        match controller.attach(pid) {
            Err(AttachFailure::AlreadyExited) | Ok(()) => {}
            Err(AttachFailure::Containment(error)) => panic!(
                "a command that finished before the check must read as gone, not as a spawn \
                 that escaped its containment: {error}"
            ),
        }
    }

    /// A Windows workload inside every bound must finish, with nothing reported.
    ///
    /// The counter-test for the two above: a job that fired on everything would
    /// satisfy both of them and break every command the app runs.
    #[cfg(windows)]
    #[test]
    fn a_windows_job_inside_its_bounds_reports_no_breach() {
        let effective = EffectiveLimits::resolve(&[LimitLayer::new(
            LimitSource::UserOverride,
            ProcessLimits {
                max_child_processes: Some(16),
                max_memory_bytes: Some(512 * 1024 * 1024),
                ..ProcessLimits::default()
            },
        )]);
        let mut controller = ResourceController::new(effective);
        let mut child = windows_child_under(
            &mut controller,
            "windows_idle_child",
            "LITTLE_MONKEY_WINDOWS_IDLE_MS",
            "1500",
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            match controller.check(now_ms()).expect("the controller checks") {
                ResourceCheck::Breached { breach, .. } => {
                    let _ = child.kill();
                    panic!("a workload inside every bound must not be a breach: {breach:?}")
                }
                ResourceCheck::Gone => break,
                ResourceCheck::Running(_) => {}
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the idle child never finished"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = child.wait();
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
        terminate_supervised_tree(None, &mut owned, None, None)
            .expect("nothing of ours is running");
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

    /// Adversarial process trees: what a descendant can do to get out from under
    /// its budget, and which of those this app actually stops.
    ///
    /// # Why these are real processes
    ///
    /// The escapes under test — `setsid`, re-parenting, both together — are
    /// kernel state changes. A fake process table would assert that this module's
    /// bookkeeping is self-consistent, which is not the question: the question is
    /// whether the kernel's own view, after a descendant has rearranged itself,
    /// still lets a supervisor find and kill what it started.
    ///
    /// # The property, stated once
    ///
    /// **Ownership is sticky from the moment of capture.** A member recorded
    /// while it was still reachable stays owned however it later rearranges its
    /// group, its session or its parent — because "did this workload start it" is
    /// not a question a descendant's own bookkeeping can answer differently
    /// later.
    ///
    /// The escapes are therefore ordered deliberately: each child waits for a
    /// flag file before escaping, so "captured, then escaped" is the sequence
    /// under test rather than a race. The one case where the order is reversed is
    /// [`the_one_escape_no_unprivileged_supervisor_can_follow`], which exists to
    /// draw the boundary rather than to pretend it is not there.
    #[cfg(unix)]
    mod escapes {
        use super::*;

        /// Perl, because it is the one interpreter present on every platform leg
        /// this runs on that can call `setsid(2)` directly. A host without it
        /// skips rather than passing vacuously.
        fn perl_is_available() -> bool {
            std::process::Command::new("perl")
                .arg("-e")
                .arg("exit 0")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        }

        fn scratch(name: &str) -> std::path::PathBuf {
            std::env::temp_dir().join(format!(
                "little_monkey_escape_{}_{}_{name}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ))
        }

        /// A supervisor-backed controller with a real workload attached.
        ///
        /// The limits are deliberately generous: these tests are about ownership,
        /// and a bound that fired during one would end the workload for a reason
        /// the test is not about.
        fn supervised(shell_command: &str) -> (ResourceController, std::process::Child) {
            let mut command = std::process::Command::new("sh");
            command
                .arg("-c")
                .arg(shell_command)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let mut controller =
                ResourceController::new(EffectiveLimits::resolve(&[LimitLayer::new(
                    LimitSource::UserOverride,
                    ProcessLimits {
                        max_memory_bytes: Some(64 * 1024 * 1024 * 1024),
                        max_child_processes: Some(4_096),
                        ..ProcessLimits::default()
                    },
                )]));
            controller
                .prepare_std(&mut command)
                .expect("the containment is installable");
            let child = command.spawn().expect("the workload starts");
            controller
                .attach(child.id())
                .expect("the workload is inside its containment");
            (controller, child)
        }

        /// Wait for a pid to appear in a file the workload writes.
        fn wait_for_pid(path: &std::path::Path) -> u32 {
            for _ in 0..200 {
                if let Ok(text) = std::fs::read_to_string(path) {
                    if let Ok(pid) = text.trim().parse::<u32>() {
                        return pid;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            panic!("the workload never reported its descendant's pid at {path:?}");
        }

        fn wait_until(mut ready: impl FnMut() -> bool, what: &str) {
            for _ in 0..200 {
                if ready() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            panic!("{what} never happened");
        }

        fn owns(controller: &ResourceController, pid: u32) -> bool {
            controller
                .live_owned()
                .iter()
                .any(|identity| identity.pid == pid)
        }

        /// A child that leaves the process group and session entirely.
        ///
        /// `setsid` is the strongest thing an unprivileged descendant can do to a
        /// group-based supervisor: it is in a new session, a new group, and
        /// nothing about either names this workload any more. It is still a
        /// *child*, so the parent link holds — and the point of this test is that
        /// the supervisor does not need the parent link either, because it wrote
        /// the identity down before the escape.
        #[test]
        fn a_captured_child_that_calls_setsid_stays_owned() {
            if !perl_is_available() {
                return;
            }
            let pid_file = scratch("setsid.pid");
            let flag = scratch("setsid.go");
            let (mut controller, mut root) = supervised(&format!(
                "perl -e 'use POSIX qw(setsid); open(my $f, \">\", $ARGV[0]); print $f $$;                  close($f); while (! -e $ARGV[1]) {{ select(undef,undef,undef,0.02) }}                  setsid(); sleep 60' {} {} ; sleep 60",
                pid_file.display(),
                flag.display()
            ));

            let escapee = wait_for_pid(&pid_file);
            // Captured while it is still reachable, which is the precondition the
            // whole property depends on.
            controller
                .sample()
                .expect("the tree samples")
                .expect("it is running");
            assert!(owns(&controller, escapee), "the child was never captured");

            std::fs::write(&flag, "go").expect("the flag is written");
            wait_until(
                || crate::process_tree::session_of(escapee) == Some(escapee),
                "the child never became its own session leader",
            );

            // The escape has happened. Ownership must not have moved with it.
            controller.sample().expect("the tree samples");
            assert!(
                owns(&controller, escapee),
                "a child that left the group and the session dropped out of the owned set"
            );
            controller.terminate_tree().expect("the tree is reclaimed");
            wait_until(
                || !crate::os_signal::process_is_alive(escapee),
                "the escaped child survived the termination that owned it",
            );

            let _ = root.kill();
            let _ = root.wait();
            let _ = std::fs::remove_file(&pid_file);
            let _ = std::fs::remove_file(&flag);
        }

        /// A child whose parent exits, so the kernel re-parents it to init.
        ///
        /// The ancestry a later snapshot could walk is destroyed by this, which is
        /// exactly why ownership is recorded rather than re-derived: no snapshot
        /// taken after the parent is gone can attribute this process to the
        /// workload, and the one taken before can.
        #[test]
        fn a_captured_child_that_reparents_stays_owned() {
            let pid_file = scratch("reparent.pid");
            // The subshell exits immediately, so `sleep` is re-parented while the
            // outer shell — the workload's root — keeps running.
            let (mut controller, mut root) = supervised(&format!(
                "( sleep 60 & echo $! > {} ) ; sleep 60",
                pid_file.display()
            ));

            let escapee = wait_for_pid(&pid_file);
            controller
                .sample()
                .expect("the tree samples")
                .expect("it is running");
            assert!(owns(&controller, escapee), "the child was never captured");

            wait_until(
                || {
                    crate::process_tree::snapshot()
                        .ok()
                        .and_then(|nodes| {
                            nodes
                                .iter()
                                .find(|node| node.pid == escapee)
                                .map(|node| node.parent_pid)
                        })
                        .is_some_and(|parent| parent != root.id())
                },
                "the child never re-parented",
            );

            controller.sample().expect("the tree samples");
            assert!(
                owns(&controller, escapee),
                "a re-parented child dropped out of the owned set"
            );
            controller.terminate_tree().expect("the tree is reclaimed");
            wait_until(
                || !crate::os_signal::process_is_alive(escapee),
                "the re-parented child survived the termination that owned it",
            );

            let _ = root.kill();
            let _ = root.wait();
            let _ = std::fs::remove_file(&pid_file);
        }

        /// Both escapes at once, after capture: no group, no session, no parent.
        ///
        /// Nothing the kernel can be asked, after this, ties the process to the
        /// workload. It stays owned anyway, and it is reclaimed, because the
        /// supervisor wrote the identity down while the answer still existed.
        #[test]
        fn a_captured_child_that_reparents_and_calls_setsid_stays_owned() {
            if !perl_is_available() {
                return;
            }
            let pid_file = scratch("combined.pid");
            let flag = scratch("combined.go");
            let (mut controller, mut root) = supervised(&format!(
                "( perl -e 'use POSIX qw(setsid); open(my $f, \">\", $ARGV[0]); print $f $$;                  close($f); while (! -e $ARGV[1]) {{ select(undef,undef,undef,0.02) }}                  setsid(); sleep 60' {} {} & ) ; sleep 60",
                pid_file.display(),
                flag.display()
            ));

            let escapee = wait_for_pid(&pid_file);
            controller
                .sample()
                .expect("the tree samples")
                .expect("it is running");
            assert!(owns(&controller, escapee), "the child was never captured");

            std::fs::write(&flag, "go").expect("the flag is written");
            wait_until(
                || crate::process_tree::session_of(escapee) == Some(escapee),
                "the child never became its own session leader",
            );

            controller.sample().expect("the tree samples");
            assert!(
                owns(&controller, escapee),
                "a child that escaped every primitive at once dropped out of the owned set"
            );
            controller.terminate_tree().expect("the tree is reclaimed");
            wait_until(
                || !crate::os_signal::process_is_alive(escapee),
                "the doubly-escaped child survived the termination that owned it",
            );

            let _ = root.kill();
            let _ = root.wait();
            let _ = std::fs::remove_file(&pid_file);
            let _ = std::fs::remove_file(&flag);
        }

        /// The boundary, stated rather than papered over.
        ///
        /// A descendant that does **both** escapes *before* the supervisor has
        /// ever recorded it is outside every primitive an unprivileged macOS
        /// process has: it is not in the group, not in the session, and its parent
        /// link is gone, so nothing the kernel can be asked names this workload.
        /// This test asserts that residual honestly — a supervisor that claimed
        /// otherwise would be claiming a guarantee the platform does not offer.
        ///
        /// Note what it is *not*: on Linux the same workload under a cgroup is
        /// fully contained, because membership is inherited and neither `setsid`
        /// nor re-parenting affects it. This is a supervisor limit, and it is why
        /// the backend is reported rather than assumed.
        #[test]
        fn the_one_escape_no_unprivileged_supervisor_can_follow() {
            if !perl_is_available() {
                return;
            }
            let pid_file = scratch("boundary.pid");
            let (mut controller, mut root) = supervised(&format!(
                "( perl -e 'use POSIX qw(setsid); setsid(); open(my $f, \">\", $ARGV[0]);                  print $f $$; close($f); sleep 60' {} & ) ; sleep 60",
                pid_file.display()
            ));
            // Deliberately not sampled before the escape: the pid file is written
            // *after* `setsid`, so by the time this returns the process is already
            // outside everything.
            let escapee = wait_for_pid(&pid_file);
            wait_until(
                || crate::process_tree::session_of(escapee) == Some(escapee),
                "the child never became its own session leader",
            );

            controller.sample().expect("the tree samples");
            if matches!(
                controller.capabilities().backend.as_str(),
                "cgroup v2" | "windows job object"
            ) {
                // A kernel-held containment has no such boundary: membership is
                // inherited and cannot be left. Nothing to assert about a
                // supervisor here.
                controller.terminate_tree().expect("the tree is reclaimed");
                let _ = root.kill();
                let _ = root.wait();
                let _ = std::fs::remove_file(&pid_file);
                return;
            }
            assert!(
                !owns(&controller, escapee),
                "this test documents a residual limit; if the supervisor now finds this                  process, the limit is closed and `docs/limitations.md` has to say so"
            );

            let _ = root.kill();
            let _ = root.wait();
            // Reclaimed by hand, because by construction the supervisor cannot.
            let _ = crate::os_signal::terminate_process_group(escapee);
            let _ = std::fs::remove_file(&pid_file);
        }

        /// An escaped-but-captured child still counts against the budget it
        /// escaped.
        ///
        /// The failure this prevents: a workload could put its allocation behind a
        /// `setsid` and read as a tree holding nothing, so a memory bound would
        /// never fire while the machine filled up. Supervisor-only by nature — a
        /// cgroup member cannot leave its scope, so there is nothing to escape.
        #[test]
        fn an_escaped_child_still_counts_against_the_budget_it_left() {
            if !perl_is_available() {
                return;
            }
            let pid_file = scratch("counts.pid");
            let flag = scratch("counts.go");
            // A quarter of a gigabyte held in one string, against a 64 MiB
            // ceiling: far enough apart that no ordinary process on the host
            // decides the outcome.
            const CEILING: u64 = 64 * 1024 * 1024;
            let mut command = std::process::Command::new("sh");
            command
                .arg("-c")
                .arg(format!(
                    "perl -e 'use POSIX qw(setsid); open(my $f, \">\", $ARGV[0]); print $f $$;                      close($f); while (! -e $ARGV[1]) {{ select(undef,undef,undef,0.02) }}                      setsid(); my $x = \"a\" x (256*1024*1024); sleep 60' {} {} ; sleep 60",
                    pid_file.display(),
                    flag.display()
                ))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let mut controller =
                ResourceController::new(EffectiveLimits::resolve(&[LimitLayer::new(
                    LimitSource::UserOverride,
                    ProcessLimits {
                        max_memory_bytes: Some(CEILING),
                        ..ProcessLimits::default()
                    },
                )]));
            if controller.capabilities().backend != "supervisor" {
                // The kernel backends hold this by construction and have their own
                // tests; this one is about the supervisor's owned set.
                return;
            }
            controller
                .prepare_std(&mut command)
                .expect("the containment is installable");
            let mut root = command.spawn().expect("the workload starts");
            controller.attach(root.id()).expect("it is contained");

            let escapee = wait_for_pid(&pid_file);
            controller
                .sample()
                .expect("the tree samples")
                .expect("it is running");
            assert!(owns(&controller, escapee), "the child was never captured");
            std::fs::write(&flag, "go").expect("the flag is written");

            let mut breach = None;
            for _ in 0..200 {
                match controller.check(now_ms()).expect("the controller checks") {
                    ResourceCheck::Breached { breach: fired, .. } => {
                        breach = Some(fired);
                        break;
                    }
                    ResourceCheck::Running(_) | ResourceCheck::Gone => {}
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let breach = breach.expect(
                "an escaped child's allocation never reached the budget it was supposed to count                  against",
            );
            assert_eq!(breach.limit, ProcessLimitKind::Memory.as_str());
            assert!(breach.observed > CEILING, "{breach:?}");
            wait_until(
                || !crate::os_signal::process_is_alive(escapee),
                "the breach did not reclaim the escaped child that caused it",
            );

            let _ = root.kill();
            let _ = root.wait();
            let _ = std::fs::remove_file(&pid_file);
            let _ = std::fs::remove_file(&flag);
        }
    }
}
