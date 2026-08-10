//! Arbitration: process classes, fair-share, aging, preemption and
//! backpressure (K8).
//!
//! K7 answered "does this fit". This answers "of the things that fit, which one,
//! and what did it beat". Those are different questions and the second one is
//! where the queue stopped being a queue: `ready_jobs` ordered by
//! `priority DESC, created_at_ms ASC` and took the first N, which is a priority
//! queue with no notion of who is waiting on the answer, no notion of one
//! workspace hogging the machine, and no bound at all on how long the tail
//! waits.
//!
//! Pure on purpose, exactly as `admission.rs` is pure: the engine owns *when* to
//! ask, where the numbers come from and what to do with the answer; this owns
//! *what the answer is*. A scheduler whose logic only exists inside a tokio tick
//! is a scheduler nobody can test.

use super::admission::{DeviceClaim, Resource};
use little_monkey_lib::run_protocol::RunKind;
// `ProcessClass` and `classify` live in the lib beside `RunKind`, which is what
// decides a class — the desktop and the ledger need the same answer this
// scheduler uses, and two copies of a four-arm enum drift.
pub use little_monkey_lib::run_protocol::{classify, ProcessClass};

/// One aging step per minute a job spends queued. See [`starvation_bound_ms`].
pub const AGING_INTERVAL_MS: u64 = 60_000;

/// How many named classes there are. Referenced by the starvation bound rather
/// than spelled as `3` there, so the bound and the enum cannot drift apart.
pub const CLASS_COUNT: u32 = 4;

/// Fair-share charges are compared in buckets this wide, not exactly.
///
/// Comparing raw millisecond totals would make the order flip on any difference
/// at all, so a workspace one CPU-millisecond ahead would lose to one that is
/// otherwise identical and lower priority. A quantum makes the comparison a
/// deficit counter: workspaces within the same bucket are considered even, and
/// declared priority decides between them.
pub const FAIR_SHARE_QUANTUM_MS: u64 = 30_000;

/// How many of a workspace's most recent processes its fair-share charge is
/// summed over.
///
/// A bound rather than all of history for the obvious reason and one less
/// obvious one: a workspace that ran something enormous last month must not be
/// punished for it forever, and the process table's own listing is bounded too,
/// so an unbounded charge was never actually available.
pub const FAIR_SHARE_WINDOW_ROWS: u32 = 64;

/// How many aging steps a job that has been queued `queued_ms` has earned.
pub const fn aging_steps(queued_ms: u64) -> u32 {
    let steps = queued_ms / AGING_INTERVAL_MS;
    if steps > u32::MAX as u64 {
        u32::MAX
    } else {
        steps as u32
    }
}

/// **The starvation bound.**
///
/// A queued job accrues one aging step per [`AGING_INTERVAL_MS`], and each step
/// promotes its effective class one level toward `Interactive`. With
/// `M = CLASS_COUNT` classes, the lowest class reaches the highest after at most
/// `M - 1` steps, so
///
/// ```text
///     T_head = (CLASS_COUNT - 1) × AGING_INTERVAL_MS = 3 × 60_000 ms = 3 minutes
/// ```
///
/// after which the job is at the head of the ranking and stays there: rank key 2
/// is aging steps descending, so a fully-aged job outranks every job that shares
/// its effective class but has not waited as long, and no later arrival can
/// overtake it however it was submitted. Concretely, a `maintenance` job at the
/// tail of a queue being flooded with `interactive` work is ranked first after
/// three minutes.
///
/// The bound is on *reaching the head*, not on dispatch, and the difference is
/// not a hedge — it is the only honest statement available. Dispatch also needs a
/// free slot, and the wait for one is bounded by the running jobs' own
/// `max_runtime_ms` (the watchdog cancels at that ceiling, so it is finite) which
/// this module cannot shorten. What the bound does guarantee is that the wait
/// stops depending on what arrives *after* the job did, which is precisely what
/// "no starvation" means:
///
/// ```text
///     delay_until_dispatch ≤ T_head + max(max_runtime_ms of the running jobs)
/// ```
pub const fn starvation_bound_ms() -> u64 {
    (CLASS_COUNT as u64 - 1) * AGING_INTERVAL_MS
}

/// A queued job as the arbitration sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub job_id: String,
    pub class: ProcessClass,
    /// The share key: the run's primary workspace root. `None` for a run with no
    /// workspace snapshot at all (a model-only chat), which is its own share
    /// group rather than being lumped in with whichever workspace sorted first.
    pub workspace: Option<String>,
    pub priority: i32,
    pub created_at_ms: u64,
    /// Measured device time already spent by this candidate's workspace — summed
    /// `cpu_time_ms` over its most recent [`FAIR_SHARE_WINDOW_ROWS`] processes.
    /// See [`rank`] for what this does and does not capture.
    pub charged_ms: u64,
}

impl Candidate {
    pub fn effective_class(&self, now_ms: u64) -> ProcessClass {
        self.class.promoted(self.aging_steps(now_ms))
    }

    pub fn aging_steps(&self, now_ms: u64) -> u32 {
        aging_steps(now_ms.saturating_sub(self.created_at_ms))
    }

    fn charge_bucket(&self) -> u64 {
        self.charged_ms / FAIR_SHARE_QUANTUM_MS
    }
}

/// **The arbitration rule**, in one paragraph because a rule that cannot be
/// stated in one is not a rule.
///
/// Candidates are ordered by six keys, in this order. (1) **Effective class**:
/// `interactive`, then `batch`, then `background`, then `maintenance`, where the
/// effective class is the declared class promoted one level per
/// [`AGING_INTERVAL_MS`] the job has spent queued. (2) **Aging steps**,
/// most-aged first, so a promoted job outranks an un-aged job that merely shares
/// its effective class — this is the key that makes [`starvation_bound_ms`] a
/// bound rather than a hope. (3) **Fair-share deficit**, least-charged workspace
/// first, where the charge is the workspace's measured `cpu_time_ms` over its
/// most recent [`FAIR_SHARE_WINDOW_ROWS`] processes, compared in
/// [`FAIR_SHARE_QUANTUM_MS`] buckets. (4) **Declared priority**, descending.
/// (5) **Queue age**, oldest first. (6) **Job id**, so the order is total and
/// two ticks never disagree about it. The engine then walks that order and
/// admits what fits, *holding* rather than stopping at what does not — so the
/// ranking decides who gets first refusal, never who is allowed to be
/// considered.
///
/// What fair-share measures is real device time, not queue turns: a workspace
/// running one six-hour job accrues six hours of charge and is outranked by one
/// running seven hundred thirty-second turns, which is the case an even queue
/// rotation gets exactly backwards.
///
/// ponytail: the charge is `cpu_time_ms`, so it measures CPU occupancy and
/// nothing else. A job blocked on a remote provider's HTTP response is charged
/// almost nothing while holding a slot, and a job saturating the GPU is charged
/// only for the CPU that fed it — `gpu_device_ms` exists in the ledger but no
/// runtime here ever reports it, so there is nothing to add. The upgrade path is
/// entirely additive: when a runtime reports GPU device time, add it to the
/// charge here and nothing else changes.
pub fn rank(candidates: &mut [Candidate], now_ms: u64) {
    candidates.sort_by(|left, right| {
        left.effective_class(now_ms)
            .rank()
            .cmp(&right.effective_class(now_ms).rank())
            .then(right.aging_steps(now_ms).cmp(&left.aging_steps(now_ms)))
            .then(left.charge_bucket().cmp(&right.charge_bucket()))
            .then(right.priority.cmp(&left.priority))
            .then(left.created_at_ms.cmp(&right.created_at_ms))
            .then(left.job_id.cmp(&right.job_id))
    });
}

/// Ranking-key tokens, so a decision log can name which key decided and a reader
/// can match on it.
pub const KEY_SOLE_CANDIDATE: &str = "sole_candidate";
pub const KEY_EFFECTIVE_CLASS: &str = "effective_class";
pub const KEY_AGING_STEPS: &str = "aging_steps";
pub const KEY_FAIR_SHARE: &str = "cpu_time_ms";
pub const KEY_PRIORITY: &str = "priority";
pub const KEY_QUEUE_AGE: &str = "created_at_ms";
pub const KEY_JOB_ID: &str = "job_id";

/// Which of [`rank`]'s six keys actually put `chosen` ahead of `runner_up`, and
/// the value of that key for `chosen`.
///
/// The first key on which the two differ, walked in the same order `rank` uses —
/// which is what makes this an explanation of the decision rather than a second,
/// separately-maintained opinion about it. If `rank` gains a key this must gain
/// the same one in the same position, and the round-trip test below is what
/// fails if it does not.
pub fn deciding_key(
    chosen: &Candidate,
    runner_up: Option<&Candidate>,
    now_ms: u64,
) -> (&'static str, Option<u64>) {
    let Some(other) = runner_up else {
        return (KEY_SOLE_CANDIDATE, None);
    };
    if chosen.effective_class(now_ms) != other.effective_class(now_ms) {
        return (
            KEY_EFFECTIVE_CLASS,
            Some(u64::from(chosen.effective_class(now_ms).rank())),
        );
    }
    if chosen.aging_steps(now_ms) != other.aging_steps(now_ms) {
        return (KEY_AGING_STEPS, Some(u64::from(chosen.aging_steps(now_ms))));
    }
    if chosen.charge_bucket() != other.charge_bucket() {
        // The reported value is the raw measured charge, not the bucket: the
        // bucket is this module's comparison device and the milliseconds are the
        // measurement.
        return (KEY_FAIR_SHARE, Some(chosen.charged_ms));
    }
    if chosen.priority != other.priority {
        // Priority is signed, and a decision log field that has to represent
        // `-1` as a `u64` would represent it as something enormous. The token
        // says which key decided; `detail` carries the number.
        return (KEY_PRIORITY, None);
    }
    if chosen.created_at_ms != other.created_at_ms {
        return (KEY_QUEUE_AGE, Some(chosen.created_at_ms));
    }
    (KEY_JOB_ID, None)
}

/// A running job as preemption sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    pub job_id: String,
    pub class: ProcessClass,
    pub ram_bytes: u64,
    pub vram_bytes: u64,
    /// What this job holds on each accelerator device (K15).
    ///
    /// Beside `vram_bytes` rather than replacing it: that figure is the pooled
    /// total and is still what a RAM-versus-accelerator comparison reads. This is
    /// what a *device* shortfall is answered from, and the distinction is the
    /// whole point — suspending a job that holds 8 GB on card 0 frees nothing at
    /// all when card 1 is the one that is full.
    pub device_bytes: Vec<DeviceClaim>,
    /// Already suspended by the scheduler, so it has nothing left to give.
    pub preempted: bool,
    pub started_at_ms: u64,
}

impl Running {
    fn claim(&self, resource: &Resource) -> u64 {
        match resource {
            Resource::Ram => self.ram_bytes,
            // Only the bytes on the device that actually fell short. Falling back
            // to `vram_bytes` here would re-introduce the aggregate through the
            // preemption path: a victim would look like it could free memory on a
            // card it never touched, the set would pass the covers-the-shortfall
            // guard, and real work would be parked for nothing.
            Resource::Accelerator(device) => self
                .device_bytes
                .iter()
                .find(|claim| claim.device == *device)
                .map(|claim| claim.bytes)
                .unwrap_or(0),
        }
    }
}

/// Everyone who should step aside so `claimant` can start, if anyone should.
///
/// The cascading form the single-victim rule could not express: two `background`
/// jobs that together free enough used to free nothing, so a claimant was held
/// behind a pair it could have displaced.
///
/// # Why parking a set is not riskier than parking one
///
/// The objection to cascading was that it needs a cost model for "how much work
/// is being parked", which nothing here measures. It does not, because of the
/// guard: victims are accumulated in preference order and the set is returned
/// **only if it actually covers the shortfall**. A set that would fall short is
/// discarded whole and nobody is suspended, so the failure this was worried
/// about — parking real work and still not admitting the claimant — cannot
/// happen. What remains is the same judgement the single-victim rule already
/// makes, applied more than once.
///
/// Greedy in [`preference`] order, so it takes the lowest class first, then the
/// largest claim — which is also what keeps the set small, since the biggest
/// contributors are consumed first. The remaining judgement, "is parking three
/// background jobs worse than making one interactive turn wait", is the same one
/// answered for a single victim.
///
/// Returns empty when no set covers the shortfall, when nothing is eligible, or
/// when `shortfall_bytes` is zero — the last because a claimant that needs
/// nothing must not suspend anyone.
pub fn preemption_victims<'a>(
    claimant: ProcessClass,
    resource: &Resource,
    shortfall_bytes: u64,
    running: &'a [Running],
) -> Vec<&'a Running> {
    if shortfall_bytes == 0 {
        return Vec::new();
    }
    let mut ordered: Vec<&Running> = eligible(claimant, running).collect();
    ordered.sort_by(|left, right| preference(left, right, resource));

    let mut freed = 0u64;
    let mut chosen = Vec::new();
    for victim in ordered {
        // Saturating because these are measured claims summed across jobs; a
        // total that overflowed would wrap to a small number and silently stop
        // covering the shortfall it had already exceeded.
        freed = freed.saturating_add(victim.claim(resource));
        chosen.push(victim);
        if freed >= shortfall_bytes {
            return chosen;
        }
    }
    // Everything eligible together is still not enough. Suspending any of it
    // would park work for nothing, so nobody is chosen.
    Vec::new()
}

/// Who may be preempted at all: a strictly lower class that has not already
/// been parked.
///
/// Only a **strictly lower** class is ever preempted, for the reason the callers
/// above give: equal classes preempting each other is livelock.
fn eligible(claimant: ProcessClass, running: &[Running]) -> impl Iterator<Item = &Running> {
    running
        .iter()
        .filter(move |victim| !victim.preempted && victim.class.rank() > claimant.rank())
}

/// Which of two eligible jobs should be preempted first.
///
/// Lowest class, then largest claim, then the *most recently started* — the
/// long-running job keeps its progress, which is the difference between
/// preemption and just undoing work. `job_id` last, so the order is total and a
/// tick's decision is reproducible.
fn preference(left: &Running, right: &Running, resource: &Resource) -> std::cmp::Ordering {
    right
        .class
        .rank()
        .cmp(&left.class.rank())
        .then(right.claim(resource).cmp(&left.claim(resource)))
        .then(right.started_at_ms.cmp(&left.started_at_ms))
        .then(left.job_id.cmp(&right.job_id))
}

/// How long a preempted job must stay suspended before the scheduler will
/// resume it.
///
/// Without a floor, one interactive job arriving and leaving every poll interval
/// would suspend and resume the same background job four times a second, which
/// costs more than it saves and looks like a bug from the outside.
pub const PREEMPTION_MIN_SUSPENDED_MS: u64 = 5_000;

/// What a producer is told about whether to send more work.
///
/// Three states rather than a boolean because the useful middle case exists: the
/// queue is not full, so refusing would be wrong, but it is deep enough that a
/// producer with a choice should wait. A boolean forces that case into one of the
/// two wrong answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackpressureState {
    /// Send work.
    Accepting,
    /// Work is still accepted, but a producer that can wait should.
    Slow,
    /// Refused. `enqueue` will return an error, so a producer that ignores this
    /// gets the error instead of the signal.
    Closed,
}

impl BackpressureState {
    pub const fn token(self) -> &'static str {
        match self {
            Self::Accepting => "accepting",
            Self::Slow => "slow",
            Self::Closed => "closed",
        }
    }
}

/// The backpressure signal, as it appears on `monkey daemon status --json`.
///
/// Every field is a number or a token a producer can act on without parsing
/// prose; `detail` is the sentence for a human and is never the thing to branch
/// on.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Backpressure {
    pub state: BackpressureState,
    /// Convenience mirror of `state != Closed`, so the common check is one field
    /// and a producer that has never heard of `slow` still behaves correctly.
    pub accepting: bool,
    /// Stable machine token, or `None` when accepting freely.
    pub reason: Option<&'static str>,
    pub detail: Option<String>,
    /// Roughly how long to wait before trying again. Advisory: derived from the
    /// poll interval and the queue depth, not from a prediction of when a job
    /// will finish, because nothing here knows that.
    pub retry_after_ms: Option<u64>,
    /// Every non-terminal job, which is what the queue capacity bounds.
    pub queue_depth: u32,
    pub queue_capacity: u32,
    /// Jobs waiting to start — the subset of `queue_depth` that is not already
    /// running.
    pub queued: u32,
    /// Queued jobs admission is currently refusing for resources. `held == queued`
    /// is the shape that means "the machine, not the queue, is full", which is a
    /// different sentence and a different fix.
    pub held: u32,
}

impl Backpressure {
    /// The refusal to return from `enqueue`, or `None` to accept.
    ///
    /// The daemon-side honouring of its own signal. It lives here so the sentence
    /// a producer reads and the state it can branch on cannot disagree, and so a
    /// producer that ignores `state: "closed"` gets this error rather than a
    /// queue that quietly overfills.
    pub fn refusal(&self) -> Option<String> {
        if self.state != BackpressureState::Closed {
            return None;
        }
        let detail = self
            .detail
            .clone()
            .unwrap_or_else(|| "the daemon is not accepting work".to_string());
        Some(match self.retry_after_ms {
            Some(retry) => format!("{detail} (retry after about {retry} ms)"),
            None => detail,
        })
    }
}

/// Reason tokens. Public so a producer can match on them without copying string
/// literals out of this file.
pub const BACKPRESSURE_KILL_SWITCH: &str = "kill_switch";
pub const BACKPRESSURE_QUEUE_FULL: &str = "queue_full";
pub const BACKPRESSURE_QUEUE_DEEP: &str = "queue_deep";
pub const BACKPRESSURE_MEMORY_SATURATED: &str = "memory_saturated";

/// Fraction of the queue that counts as deep enough to ask producers to slow.
const SLOW_AT_PERCENT: u32 = 80;

/// The signal, from counts the daemon already has.
///
/// Ordered by severity: a closed reason always wins over a slow one, and among
/// closed reasons the kill switch wins because it is the one an operator set
/// deliberately and the one they will look for first.
pub fn backpressure(
    kill_switch: bool,
    queue_depth: u32,
    queue_capacity: u32,
    queued: u32,
    held: u32,
    poll_interval_ms: u64,
) -> Backpressure {
    let capacity = queue_capacity.max(1);
    let signal = |state: BackpressureState,
                  reason: Option<&'static str>,
                  detail: Option<String>,
                  retry_after_ms: Option<u64>| Backpressure {
        state,
        accepting: state != BackpressureState::Closed,
        reason,
        detail,
        retry_after_ms,
        queue_depth,
        queue_capacity: capacity,
        queued,
        held,
    };
    if kill_switch {
        return signal(
            BackpressureState::Closed,
            Some(BACKPRESSURE_KILL_SWITCH),
            Some("the global kill switch is engaged; release it before queueing work".to_string()),
            None,
        );
    }
    if queue_depth >= capacity {
        return signal(
            BackpressureState::Closed,
            Some(BACKPRESSURE_QUEUE_FULL),
            Some(format!(
                "{queue_depth} of {capacity} queue slots are in use; wait for a run or cancel one"
            )),
            // One poll interval per queued job is a crude estimate and is meant
            // to be: it scales with the backlog and never claims to know when a
            // job will finish.
            Some(poll_interval_ms.saturating_mul(u64::from(queue_depth.max(1)))),
        );
    }
    // Every job waiting to start is held for memory: the queue has room and the
    // machine does not.
    if held > 0 && held >= queued {
        return signal(
            BackpressureState::Slow,
            Some(BACKPRESSURE_MEMORY_SATURATED),
            Some(format!(
                "all {held} queued runs are waiting on memory; more work will queue but not start"
            )),
            Some(poll_interval_ms.saturating_mul(u64::from(held.max(1)))),
        );
    }
    if u64::from(queue_depth) * 100 >= u64::from(capacity) * u64::from(SLOW_AT_PERCENT) {
        return signal(
            BackpressureState::Slow,
            Some(BACKPRESSURE_QUEUE_DEEP),
            Some(format!(
                "{queue_depth} of {capacity} queue slots are in use; slow down"
            )),
            Some(poll_interval_ms.saturating_mul(u64::from(queue_depth.max(1)))),
        );
    }
    signal(BackpressureState::Accepting, None, None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(job_id: &str, class: ProcessClass, created_at_ms: u64) -> Candidate {
        Candidate {
            job_id: job_id.to_string(),
            class,
            workspace: Some("/work".to_string()),
            priority: 0,
            created_at_ms,
            charged_ms: 0,
        }
    }

    fn order(candidates: &[Candidate], now_ms: u64) -> Vec<String> {
        let mut owned = candidates.to_vec();
        rank(&mut owned, now_ms);
        owned.into_iter().map(|entry| entry.job_id).collect()
    }

    /// The class has to come from the run's frozen kind, because that is the one
    /// thing a later caller cannot re-assert. A desktop turn is interactive; the
    /// same recipe queued from the CLI is not.
    #[test]
    fn a_desktop_turn_is_interactive_and_a_queued_recipe_is_not() {
        assert_eq!(
            classify(&RunKind::Interactive, 0),
            ProcessClass::Interactive
        );
        assert_eq!(classify(&RunKind::Workflow, 0), ProcessClass::Batch);
        assert_eq!(classify(&RunKind::Scheduled, 0), ProcessClass::Maintenance);
        assert_eq!(classify(&RunKind::Background, 0), ProcessClass::Background);
    }

    /// Priority may demote and must not promote — otherwise `--priority 9` is a
    /// self-service interactive badge.
    #[test]
    fn priority_can_only_demote_a_class() {
        assert_eq!(
            classify(&RunKind::Workflow, 9),
            ProcessClass::Batch,
            "a high priority must not buy a better class"
        );
        assert_eq!(
            classify(&RunKind::Interactive, -1),
            ProcessClass::Background,
            "an explicit deprioritization is the one thing a caller may say"
        );
        assert_eq!(
            classify(&RunKind::Scheduled, -1),
            ProcessClass::Maintenance,
            "demotion never moves a job below the class it was already in"
        );
    }

    #[test]
    fn classes_are_served_best_first() {
        let now = 1_000;
        let candidates = vec![
            candidate("maint", ProcessClass::Maintenance, now),
            candidate("bg", ProcessClass::Background, now),
            candidate("inter", ProcessClass::Interactive, now),
            candidate("batch", ProcessClass::Batch, now),
        ];
        assert_eq!(order(&candidates, now), ["inter", "batch", "bg", "maint"]);
    }

    /// The bound, demonstrated: a `maintenance` job at the tail of a queue that
    /// keeps receiving fresh `interactive` work is ranked first once
    /// `starvation_bound_ms` has elapsed, and not before.
    #[test]
    fn the_starvation_bound_holds_against_a_flood_of_interactive_arrivals() {
        let enqueued_at = 1_000;
        let starving = candidate("starving", ProcessClass::Maintenance, enqueued_at);

        // Just before the bound, three interactive arrivals still outrank it:
        // two aging steps only lift maintenance to batch.
        let almost = enqueued_at + starvation_bound_ms() - 1;
        let mut queue = vec![
            starving.clone(),
            candidate("flood-a", ProcessClass::Interactive, almost),
            candidate("flood-b", ProcessClass::Interactive, almost),
            candidate("flood-c", ProcessClass::Interactive, almost),
        ];
        rank(&mut queue, almost);
        assert_ne!(
            queue[0].job_id, "starving",
            "the bound must not be reached early"
        );

        // At the bound it is at the head, and it stays there however much fresh
        // interactive work arrives afterwards.
        let at_bound = enqueued_at + starvation_bound_ms();
        let mut queue = vec![starving.clone()];
        for index in 0..32 {
            queue.push(candidate(
                &format!("flood-{index:02}"),
                ProcessClass::Interactive,
                at_bound,
            ));
        }
        rank(&mut queue, at_bound);
        assert_eq!(
            queue[0].job_id,
            "starving",
            "a maintenance job must reach the head after {} ms",
            starvation_bound_ms()
        );

        // And later arrivals cannot undo it.
        let later = at_bound + 10 * AGING_INTERVAL_MS;
        queue.push(candidate("late", ProcessClass::Interactive, later));
        rank(&mut queue, later);
        assert_eq!(queue[0].job_id, "starving");
    }

    /// Aging steps outrank equal effective classes, which is the key that makes
    /// the bound stick: without it a promoted job would merely *join* the
    /// interactive class and be reordered by fair-share and priority forever.
    #[test]
    fn an_aged_job_outranks_an_un_aged_job_of_the_same_effective_class() {
        let now = 1_000 + starvation_bound_ms();
        let mut queue = vec![
            Candidate {
                priority: 9,
                charged_ms: 0,
                ..candidate("fresh", ProcessClass::Interactive, now)
            },
            Candidate {
                priority: -0,
                charged_ms: 10 * FAIR_SHARE_QUANTUM_MS,
                ..candidate("aged", ProcessClass::Maintenance, 1_000)
            },
        ];
        rank(&mut queue, now);
        assert_eq!(
            queue[0].job_id, "aged",
            "neither priority nor a large charge may overtake an aged job"
        );
    }

    /// One workspace must not monopolize the device. The charge is measured
    /// device time, so a workspace that has already had the machine loses to one
    /// that has not — regardless of how much work it has queued.
    #[test]
    fn a_workspace_that_has_had_the_device_loses_to_one_that_has_not() {
        let now = 1_000;
        let hog = |id: &str| Candidate {
            workspace: Some("/hog".to_string()),
            charged_ms: 20 * FAIR_SHARE_QUANTUM_MS,
            ..candidate(id, ProcessClass::Batch, now)
        };
        let quiet = Candidate {
            workspace: Some("/quiet".to_string()),
            charged_ms: 0,
            ..candidate("quiet-1", ProcessClass::Batch, now)
        };
        // The hog queued first and queued more of it.
        let candidates = vec![hog("hog-1"), hog("hog-2"), hog("hog-3"), quiet];
        assert_eq!(
            order(&candidates, now)[0],
            "quiet-1",
            "a queue-order scheduler would have run all three hog jobs first"
        );
    }

    /// The case an even rotation gets backwards: one long job is not equal to
    /// many short ones, and the charge is what tells them apart.
    #[test]
    fn one_long_job_charges_more_than_many_short_ones() {
        let now = 1_000;
        let long = Candidate {
            workspace: Some("/long".to_string()),
            // Six hours of measured CPU from a single prior job.
            charged_ms: 6 * 60 * 60 * 1_000,
            ..candidate("long-next", ProcessClass::Batch, now)
        };
        let short = Candidate {
            workspace: Some("/short".to_string()),
            // 720 thirty-second turns is six hours of wall time but this
            // workspace only ever held the CPU for a fraction of it.
            charged_ms: 20 * 60 * 1_000,
            ..candidate("short-next", ProcessClass::Batch, now)
        };
        assert_eq!(order(&[long, short], now)[0], "short-next");
    }

    /// Within a fair-share bucket the declared priority is what decides, so the
    /// number a producer picks still means something.
    #[test]
    fn priority_decides_inside_a_fair_share_bucket() {
        let now = 1_000;
        let low = Candidate {
            priority: 0,
            ..candidate("low", ProcessClass::Batch, now)
        };
        let high = Candidate {
            priority: 5,
            // Same bucket: a difference under the quantum is not a difference.
            charged_ms: FAIR_SHARE_QUANTUM_MS - 1,
            ..candidate("high", ProcessClass::Batch, now)
        };
        assert_eq!(order(&[low, high], now)[0], "high");
    }

    #[test]
    fn the_order_is_total_so_two_ticks_never_disagree() {
        let now = 1_000;
        let one = candidate("a", ProcessClass::Batch, now);
        let two = candidate("b", ProcessClass::Batch, now);
        assert_eq!(order(&[one.clone(), two.clone()], now), ["a", "b"]);
        assert_eq!(order(&[two, one], now), ["a", "b"]);
    }

    /// The explanation must agree with the decision: for every pair, the key
    /// `deciding_key` names is one the two candidates really differ on, and
    /// every earlier key is one they really agree on.
    #[test]
    fn the_deciding_key_is_the_first_key_the_ranking_actually_used() {
        let base = candidate("base", ProcessClass::Batch, 1_000);
        // Each case pairs `base` against one candidate that differs on exactly
        // one key, with the clock the difference needs.
        let cases: Vec<(Candidate, u64, &'static str)> = vec![
            (
                candidate("class", ProcessClass::Interactive, 1_000),
                1_000,
                KEY_EFFECTIVE_CLASS,
            ),
            (
                // One interval on, `base` has aged from batch to interactive and
                // this arrival is interactive already: same effective class,
                // fewer aging steps.
                candidate(
                    "arrival",
                    ProcessClass::Interactive,
                    1_000 + AGING_INTERVAL_MS,
                ),
                1_000 + AGING_INTERVAL_MS,
                KEY_AGING_STEPS,
            ),
            (
                Candidate {
                    charged_ms: 5 * FAIR_SHARE_QUANTUM_MS,
                    ..candidate("charged", ProcessClass::Batch, 1_000)
                },
                1_000,
                KEY_FAIR_SHARE,
            ),
            (
                Candidate {
                    priority: 3,
                    ..candidate("priority", ProcessClass::Batch, 1_000)
                },
                1_000,
                KEY_PRIORITY,
            ),
            (
                candidate("younger", ProcessClass::Batch, 1_001),
                1_001,
                KEY_QUEUE_AGE,
            ),
            (
                candidate("zzz", ProcessClass::Batch, 1_000),
                1_000,
                KEY_JOB_ID,
            ),
        ];
        for (other, now, expected) in cases {
            let mut pair = vec![base.clone(), other.clone()];
            rank(&mut pair, now);
            let (key, _) = deciding_key(&pair[0], Some(&pair[1]), now);
            assert_eq!(
                key, expected,
                "ranking {:?} against {:?} was decided by {key}",
                base.job_id, other.job_id
            );
        }
        assert_eq!(deciding_key(&base, None, 1_000).0, KEY_SOLE_CANDIDATE);
    }

    fn running(job_id: &str, class: ProcessClass, ram_bytes: u64) -> Running {
        Running {
            device_bytes: Vec::new(),
            job_id: job_id.to_string(),
            class,
            ram_bytes,
            vram_bytes: 0,
            preempted: false,
            started_at_ms: 1_000,
        }
    }

    #[test]
    fn only_a_strictly_lower_class_is_preempted() {
        let running = vec![
            running("peer", ProcessClass::Interactive, 8_000),
            running("lower", ProcessClass::Background, 8_000),
        ];
        let victims =
            preemption_victims(ProcessClass::Interactive, &Resource::Ram, 4_000, &running);
        assert_eq!(
            victims
                .iter()
                .map(|entry| entry.job_id.as_str())
                .collect::<Vec<_>>(),
            vec!["lower"]
        );

        assert!(
            preemption_victims(ProcessClass::Background, &Resource::Ram, 4_000, &running)
                .is_empty(),
            "a background claimant may not preempt an interactive peer"
        );
    }

    /// Equal classes must never preempt each other, or two interactive turns
    /// livelock and neither finishes.
    #[test]
    fn an_equal_class_is_never_a_victim() {
        let running = vec![running("peer", ProcessClass::Batch, 16_000)];
        assert!(
            preemption_victims(ProcessClass::Batch, &Resource::Ram, 1_000, &running).is_empty()
        );
    }

    #[test]
    fn a_victim_must_cover_the_shortfall_by_itself_and_the_newest_goes_first() {
        let running = vec![
            running("too-small", ProcessClass::Maintenance, 1_000),
            Running {
                started_at_ms: 5_000,
                ..running("newest", ProcessClass::Maintenance, 9_000)
            },
            Running {
                started_at_ms: 2_000,
                ..running("oldest", ProcessClass::Maintenance, 9_000)
            },
        ];
        let victims = preemption_victims(ProcessClass::Batch, &Resource::Ram, 8_000, &running);
        assert_eq!(
            victims
                .iter()
                .map(|v| v.job_id.as_str())
                .collect::<Vec<_>>(),
            vec!["newest"],
            "one job covers it alone, so the long-running one keeps its progress"
        );

        assert!(
            preemption_victims(ProcessClass::Batch, &Resource::Ram, 20_000, &running).is_empty(),
            "19_000 across everything eligible still does not cover 20_000"
        );
    }

    /// The case the single-victim rule could not express: a pair that together
    /// covers the shortfall used to free nothing.
    #[test]
    fn a_set_of_victims_is_taken_in_preference_order_and_stops_once_it_covers() {
        let running = vec![
            Running {
                started_at_ms: 5_000,
                ..running("newest", ProcessClass::Maintenance, 9_000)
            },
            Running {
                started_at_ms: 2_000,
                ..running("oldest", ProcessClass::Maintenance, 9_000)
            },
            running("small", ProcessClass::Maintenance, 1_000),
        ];

        // 9_000 alone is short; 9_000 + 9_000 is not.
        let victims: Vec<&str> =
            preemption_victims(ProcessClass::Batch, &Resource::Ram, 15_000, &running)
                .iter()
                .map(|victim| victim.job_id.as_str())
                .collect();
        assert_eq!(
            victims,
            vec!["newest", "oldest"],
            "largest claims first, newest before oldest, and it stops as soon as it covers"
        );

        assert_eq!(
            preemption_victims(ProcessClass::Batch, &Resource::Ram, 8_000, &running)
                .iter()
                .map(|victim| victim.job_id.as_str())
                .collect::<Vec<_>>(),
            vec!["newest"],
            "one job that covers it on its own is still one job"
        );
    }

    /// The guard that makes cascading safe without a cost model: a set that
    /// would fall short parks nobody.
    #[test]
    fn victims_are_not_parked_when_even_all_of_them_would_not_cover_the_shortfall() {
        let running = vec![
            running("a", ProcessClass::Background, 4_000),
            running("b", ProcessClass::Background, 4_000),
        ];
        assert!(
            preemption_victims(ProcessClass::Interactive, &Resource::Ram, 20_000, &running)
                .is_empty(),
            "suspending work that still would not admit the claimant is pure loss"
        );
        // And the same rule for a claimant that needs nothing at all.
        assert!(
            preemption_victims(ProcessClass::Interactive, &Resource::Ram, 0, &running).is_empty(),
            "a claimant with no shortfall must not suspend anyone"
        );
        // Equal class is never eligible, cascading or not.
        assert!(
            preemption_victims(ProcessClass::Background, &Resource::Ram, 4_000, &running)
                .is_empty(),
            "equal classes preempting each other is the livelock this rules out"
        );
    }

    #[test]
    fn an_already_preempted_job_has_nothing_left_to_give() {
        let running = vec![Running {
            preempted: true,
            ..running("parked", ProcessClass::Background, 9_000)
        }];
        assert!(
            preemption_victims(ProcessClass::Interactive, &Resource::Ram, 1_000, &running)
                .is_empty()
        );
    }

    #[test]
    fn backpressure_closes_on_the_kill_switch_and_on_a_full_queue() {
        let engaged = backpressure(true, 0, 128, 0, 0, 250);
        assert_eq!(engaged.state, BackpressureState::Closed);
        assert!(!engaged.accepting);
        assert_eq!(engaged.reason, Some(BACKPRESSURE_KILL_SWITCH));

        let full = backpressure(false, 128, 128, 4, 0, 250);
        assert_eq!(full.state, BackpressureState::Closed);
        assert_eq!(full.reason, Some(BACKPRESSURE_QUEUE_FULL));
        assert_eq!(full.retry_after_ms, Some(250 * 128));
        assert!(full
            .refusal()
            .is_some_and(|text| text.contains("retry after")));

        // The kill switch wins: it is the one an operator set deliberately.
        assert_eq!(
            backpressure(true, 128, 128, 4, 0, 250).reason,
            Some(BACKPRESSURE_KILL_SWITCH)
        );
    }

    /// The queue has room and the machine does not, which producers need told
    /// apart from a full queue: waiting helps, and cancelling does not.
    #[test]
    fn a_queue_of_held_jobs_reports_the_machine_rather_than_the_queue() {
        let saturated = backpressure(false, 8, 128, 4, 4, 250);
        assert_eq!(saturated.state, BackpressureState::Slow);
        assert!(saturated.accepting, "a slow signal still accepts work");
        assert_eq!(saturated.reason, Some(BACKPRESSURE_MEMORY_SATURATED));
        assert_eq!(saturated.held, 4);
        assert!(saturated.refusal().is_none());

        // One of two waiting jobs held is congestion, not saturation.
        assert_eq!(
            backpressure(false, 8, 128, 4, 1, 250).state,
            BackpressureState::Accepting
        );
    }

    #[test]
    fn a_deep_queue_asks_producers_to_slow_without_refusing() {
        let deep = backpressure(false, 110, 128, 110, 0, 250);
        assert_eq!(deep.state, BackpressureState::Slow);
        assert_eq!(deep.reason, Some(BACKPRESSURE_QUEUE_DEEP));
        assert!(deep.refusal().is_none());

        let calm = backpressure(false, 10, 128, 10, 0, 250);
        assert_eq!(calm.state, BackpressureState::Accepting);
        assert_eq!(calm.reason, None);
        assert_eq!(calm.retry_after_ms, None);
    }

    #[test]
    fn promotion_saturates_at_interactive() {
        assert_eq!(
            ProcessClass::Maintenance.promoted(CLASS_COUNT - 1),
            ProcessClass::Interactive
        );
        assert_eq!(
            ProcessClass::Maintenance.promoted(1_000),
            ProcessClass::Interactive
        );
        assert_eq!(
            ProcessClass::Maintenance.promoted(0),
            ProcessClass::Maintenance
        );
    }

    /// Preemption has to free memory on the card that is actually full.
    ///
    /// The failure this guards is subtle and expensive: with a pooled figure, a
    /// job holding 8 GiB on card 0 looks like it can relieve a shortfall on card
    /// 1. The victim set passes the covers-the-shortfall guard, real work is
    /// suspended, and the claimant is *still* held — the exact "park work for
    /// nothing" outcome the set-based rule was built to rule out.
    #[test]
    fn a_device_shortfall_only_counts_victims_holding_that_device() {
        use super::super::admission::{DeviceClaim, DeviceId};
        use little_monkey_lib::runtime_adapter::AcceleratorKind;

        let card0 = DeviceId::device(AcceleratorKind::Cuda, 0);
        let card1 = DeviceId::device(AcceleratorKind::Cuda, 1);

        let holder = |job_id: &str, device: &DeviceId, bytes: u64| Running {
            job_id: job_id.to_string(),
            class: ProcessClass::Background,
            ram_bytes: 0,
            vram_bytes: bytes,
            device_bytes: vec![DeviceClaim {
                device: device.clone(),
                bytes,
            }],
            preempted: false,
            started_at_ms: 1,
        };

        let running = vec![
            holder("on-card-0", &card0, 8_000),
            holder("on-card-1", &card1, 8_000),
        ];

        let victims = preemption_victims(
            ProcessClass::Interactive,
            &Resource::Accelerator(card1.clone()),
            4_000,
            &running,
        );
        assert_eq!(
            victims
                .iter()
                .map(|v| v.job_id.as_str())
                .collect::<Vec<_>>(),
            vec!["on-card-1"],
            "only the job holding the full card can relieve it"
        );

        // Nobody holds card 0 enough to cover this, and the job on card 1 must
        // not be counted toward it — so nobody is suspended at all.
        let none = preemption_victims(
            ProcessClass::Interactive,
            &Resource::Accelerator(card0.clone()),
            20_000,
            &running,
        );
        assert!(
            none.is_empty(),
            "a set that cannot cover the shortfall must park nobody, got {:?}",
            none.iter().map(|v| &v.job_id).collect::<Vec<_>>()
        );

        // And the RAM leg is unaffected by any of this.
        let ram_holder = Running {
            job_id: "ram".to_string(),
            class: ProcessClass::Background,
            ram_bytes: 9_000,
            vram_bytes: 0,
            device_bytes: Vec::new(),
            preempted: false,
            started_at_ms: 1,
        };
        let ram_running = vec![ram_holder];
        let victims = preemption_victims(
            ProcessClass::Interactive,
            &Resource::Ram,
            4_000,
            &ram_running,
        );
        assert_eq!(victims.len(), 1);
    }
}
