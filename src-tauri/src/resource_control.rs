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
}

impl LimitBreach {
    /// The human sentence shown as the exit reason.
    ///
    /// Both numbers, always: "memory limit exceeded" tells a reader the budget
    /// fired, and "held 4.11 GiB against a 4.00 GiB budget" tells them whether
    /// the budget was wrong or the job was.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "{} exceeded: observed {} against a configured {} ({} · {})",
            self.limit, self.observed, self.configured, self.backend, self.level
        )
    }
}

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

    /// Record the workload's root once it exists.
    ///
    /// Stores an identity rather than a pid: everything after this — sampling,
    /// termination — re-checks that the pid is still the process we attached to,
    /// so a reused pid cannot be sampled as ours or, far worse, killed as ours.
    pub fn attach(&mut self, pid: u32) -> io::Result<()> {
        let identity = ProcessIdentity::of(pid).ok_or_else(|| {
            io::Error::other(format!(
                "process {pid} was gone before it could be attached"
            ))
        })?;
        self.root = Some(identity);
        self.started_at = Instant::now();
        #[cfg(windows)]
        if let Backend::Job(job) = &self.backend {
            job.confirm_assignment(pid)?;
        }
        Ok(())
    }

    /// Feed the controller the byte count only the pipe reader can know.
    pub fn record_output_bytes(&mut self, bytes: u64) {
        self.output_bytes = Some(bytes);
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

        let measured = match &self.backend {
            #[cfg(target_os = "linux")]
            Backend::Cgroup(scope) => scope.sample()?,
            #[cfg(windows)]
            Backend::Job(job) => job.sample()?,
            Backend::Supervisor => {
                if !root.is_still_alive() {
                    None
                } else {
                    process_tree::measure_tree(root.pid)?
                        .map(|usage| (usage.rss_bytes, Some(usage.process_count)))
                }
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
    /// Idempotent, and safe against a root that has already exited: the identity
    /// check happens first, so a pid that has been reused is never signalled.
    pub fn terminate_tree(&self) -> io::Result<()> {
        match &self.backend {
            #[cfg(target_os = "linux")]
            Backend::Cgroup(scope) => scope.terminate_tree(),
            #[cfg(windows)]
            Backend::Job(job) => job.terminate_tree(),
            Backend::Supervisor => {
                let Some(root) = self.root else {
                    return Ok(());
                };
                if !root.is_still_alive() {
                    return Ok(());
                }
                terminate_supervised_tree(root)
            }
        }
    }
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
            output = &mut work => return Ok(Supervised::Completed(output, last)),
            _ = ticker.tick() => {
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

#[cfg(unix)]
fn terminate_supervised_tree(root: ProcessIdentity) -> io::Result<()> {
    // The group first: one signal reaches every member that stayed in it, which
    // is the ordinary case and the cheap one.
    let group_result =
        crate::os_signal::terminate_process_group(root.pid).map_err(io::Error::other);
    // Then anything that left the group but is still a descendant. This is the
    // half a process group cannot do, and the reason the supervisor measures by
    // parent link as well: a child that called `setsid` is outside the group and
    // still inside the workload.
    if let Ok(nodes) = process_tree::snapshot() {
        for pid in process_tree::tree_members(&nodes, root.pid) {
            if pid == root.pid {
                continue;
            }
            // Safe: sends a signal to one pid. A pid that has already exited
            // answers ESRCH, which is the success case here.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
    }
    group_result
}

#[cfg(not(unix))]
fn terminate_supervised_tree(root: ProcessIdentity) -> io::Result<()> {
    crate::os_signal::terminate_process_group(root.pid).map_err(io::Error::other)
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

    #[test]
    fn terminating_before_anything_is_attached_is_not_an_error() {
        let controller = ResourceController::new(EffectiveLimits::default());
        controller.terminate_tree().expect("nothing to terminate");
    }
}
