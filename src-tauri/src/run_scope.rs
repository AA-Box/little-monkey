//! The run a piece of work belongs to, carried implicitly instead of threaded.
//!
//! # The problem this solves
//!
//! Two roadmap items — a per-run egress allowlist and a per-process resource
//! ledger — both stalled on the same missing mechanism, which is what makes it its
//! own thing rather than a footnote inside either. There was no ambient notion of
//! "the run this work belongs to": 30 files construct an outbound HTTP client at 65
//! sites, and a run id could only reach any of them as an explicit parameter
//! through signatures with no other reason to carry one. `denial_sink`'s own doc
//! says it outright — "there are zero `task_local!` declarations in this crate to
//! carry one implicitly" — and that is the sentence this module exists to retire.
//!
//! # A run id was not enough on its own
//!
//! The ledger half of that pair charges bytes to a **process**, not to a run: a
//! run has many process rows and `agent_processes` is what carries the resource
//! columns. So the ambient value is a run id *and* an optional
//! [`ProcessScope`] — see that type for why the byte counter travels with the
//! identity rather than living in a global map keyed by it. The two halves are
//! independent on purpose: a run with no process id is a legitimate state, and
//! the honest record for work in it is "unattributed" rather than a nearby
//! process that did not do it.
//!
//! # Why a task-local and not a thread-local
//!
//! This is the decisive technical point, not a style preference. Tokio moves a task
//! between worker threads at every `.await`, so a `thread_local!` set at a command
//! boundary is *not* the value read after the first await — it is whatever the
//! thread that happens to resume the task last stored. With concurrent runs that is
//! not merely lossy, it is a correctness bug that hands one run another run's
//! identity, which for an allowlist means enforcing the wrong policy. A
//! `tokio::task_local!` follows the task rather than the thread, so it is the only
//! shape of ambient state that is safe here at all.
//!
//! # Some work has no run, and that is a first-class answer
//!
//! Timer-driven knowledge refresh, connector verification in Settings, model
//! downloads, update checks and every inbound request `server.rs` serves are not
//! runs and never will be. A mechanism that assumes a run is always present would
//! either refuse that work or invent an identity for it, and both are worse than
//! the current honesty. So [`RunScope`] has two arms, and the distinction it draws
//! is the one that matters:
//!
//! - [`RunScope::Unattributed`] — "this deliberately has no run", with the reason
//!   named.
//! - [`current`] returning `None` — "nobody has told us", i.e. a site not yet
//!   instrumented.
//!
//! Collapsing those two into one `Option` is exactly the mistake that makes an
//! audit trail untrustworthy later: a blank field that might mean "background
//! work" or might mean "we lost it" cannot be read either way.
//!
//! # What this does not do
//!
//! `tokio::spawn` does **not** inherit a task-local — a spawned task starts outside
//! every scope. That is deliberate on tokio's part (the spawned task may outlive
//! the spawner) and it is not worked around here, because silently copying a scope
//! into a detached task would attribute work to a run that may already have
//! finished. Work continuing in a spawned task must re-enter the scope itself. A
//! test below pins this so that nobody has to discover it from a blank column.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// Why a piece of work legitimately belongs to no run.
///
/// A closed set with stable codes rather than free text, for the reason
/// [`crate::egress::EgressRule`] is: the code is what gets persisted and compared,
/// so it has to outlive both the prose and this enum's spelling. The first four
/// cover the five cases the audit actually found — a model download and an update
/// check are both `Startup`-class only if they happen at startup, so a
/// user-initiated download is [`UserAction`](Self::UserAction).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Unattributed {
    /// Something the user asked for directly, outside any run: verifying a
    /// connector in Settings, starting a model download, clicking "check for
    /// updates".
    UserAction,
    /// Timer-driven background work, such as knowledge refresh.
    Scheduled,
    /// Serving an inbound request. `server.rs`'s callers are HTTP clients, not
    /// runs, and no amount of plumbing will make them one.
    InboundRequest,
    /// Work done while the process comes up, before any run can exist.
    Startup,
    /// A transport deliberately shared by every run that uses it, so its own
    /// traffic belongs to the *connection* rather than to any one run.
    ///
    /// Unlike the four above, this one is a **decision** and not an observation,
    /// so it is worth writing down why the decision went this way. The case is
    /// `mcp.rs`: one connection per configured MCP server, reused by every run.
    /// The alternative — one transport per run per server — was rejected on three
    /// counts:
    ///
    /// - A stdio MCP server is a *child process*. Five parallel runs against four
    ///   configured servers would mean twenty processes instead of four; that is a
    ///   resource regression a user feels, in exchange for a label.
    /// - Per-run connections multiply OAuth token refreshes by the concurrency,
    ///   which is a good way to meet a provider's rate limit.
    /// - What per-run connections would actually make attributable is the
    ///   transport's *own* traffic — the SSE notification stream and its
    ///   reconnects — and that traffic genuinely belongs to the connection. No run
    ///   asked for it and it outlives any single run.
    ///
    /// So paying a felt resource cost to attach a run id to a stream that is not
    /// one run's is backwards, and this variant is the honest label for what that
    /// stream is instead.
    SharedTransport,
}

impl Unattributed {
    /// Every reason, in declaration order.
    pub const ALL: &'static [Unattributed] = &[
        Unattributed::UserAction,
        Unattributed::Scheduled,
        Unattributed::InboundRequest,
        Unattributed::Startup,
        Unattributed::SharedTransport,
    ];

    /// The stable identity that gets persisted. Never reworded.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Unattributed::UserAction => "unattributed.user-action",
            Unattributed::Scheduled => "unattributed.scheduled",
            Unattributed::InboundRequest => "unattributed.inbound-request",
            Unattributed::Startup => "unattributed.startup",
            Unattributed::SharedTransport => "unattributed.shared-transport",
        }
    }
}

impl fmt::Display for Unattributed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

/// What the work in scope belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunScope {
    /// Work performed on behalf of this run.
    Run(String),
    /// Work that has no run, and says why.
    Unattributed(Unattributed),
}

impl RunScope {
    /// Convenience for the common case of a `&str` run id.
    #[must_use]
    pub fn run(id: impl Into<String>) -> Self {
        RunScope::Run(id.into())
    }

    /// The run id, or `None` when this scope is deliberately unattributed.
    #[must_use]
    pub fn run_id(&self) -> Option<&str> {
        match self {
            RunScope::Run(id) => Some(id),
            RunScope::Unattributed(_) => None,
        }
    }

    /// The reason there is no run, or `None` when there is one.
    #[must_use]
    pub fn unattributed(&self) -> Option<Unattributed> {
        match self {
            RunScope::Run(_) => None,
            RunScope::Unattributed(reason) => Some(*reason),
        }
    }
}

/// The process-table row the ambient work belongs to, plus the drop-box its
/// egress is charged into.
///
/// # Why the counter lives in the scope
///
/// The resource ledger needs bytes per **process**, and the only place that knows
/// which process an outbound request belongs to is the scope it was made under.
/// Two other shapes were considered and rejected:
///
/// - A global `Mutex<HashMap<process_id, u64>>` charged per request. That buys
///   attribution with a lock on the hot path of every HTTP body frame, which is
///   the one thing per-byte accounting must not cost.
/// - A process id alone, with the counting done elsewhere. There is no
///   "elsewhere" that does not end up being the map above.
///
/// So the counter is an `Arc<AtomicU64>` handed out with the identity: charging is
/// one relaxed `fetch_add` with no lock and no allocation, and whoever entered the
/// scope holds the other handle and drains it into
/// `ProcessTable::add_egress_bytes` on its own schedule. Nothing global, nothing to
/// clean up if the task that was charging dies.
///
/// `Clone` is cheap and shares the counter — two clones are two views of one
/// tally, which is what lets the scope owner drain what the scoped work charged.
#[derive(Debug, Clone)]
pub struct ProcessScope {
    process_id: Arc<str>,
    egress_bytes: Arc<AtomicU64>,
    destinations: Arc<Mutex<DestinationLog>>,
    context_reuse: Arc<ContextReuseTally>,
    /// The row's `max_context_tokens`, read once when the scope was entered.
    ///
    /// Carried here rather than re-read per request for the reason this whole
    /// module exists: the request path is Tauri-free and has no ledger handle, and
    /// the alternative is threading a budget through signatures that have no other
    /// reason to carry one. `None` is "no budget", which is every process today.
    max_context_tokens: Option<u64>,
    /// The scheduler class this process's run belongs to, read once when the
    /// scope was entered. Decides what happens when the context fills — see
    /// [`crate::context_cache::context_policy`]. `None` when the process has no
    /// run to derive a class from, which is a state, not a default.
    class: Option<crate::run_protocol::ProcessClass>,
}

/// Prompt tokens a runtime told us it reused from its cache, and prompt tokens
/// it told us it evaluated, summed across this process's completions.
///
/// Two counters rather than a ratio because a ratio cannot be accumulated: the
/// hit rate over a process is `reused / (reused + evaluated)` over the whole
/// process, not the mean of each completion's own rate, which would weigh a
/// ten-token turn the same as a ten-thousand-token one.
#[derive(Debug, Default)]
struct ContextReuseTally {
    reused: AtomicU64,
    evaluated: AtomicU64,
}

/// A drained [`ContextReuseTally`]. `reused` is the measured tokens saved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextReuse {
    pub reused_tokens: u64,
    pub evaluated_tokens: u64,
}

impl ContextReuse {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.reused_tokens == 0 && self.evaluated_tokens == 0
    }
}

impl ProcessScope {
    /// A scope for a row that already exists in `agent_processes`.
    ///
    /// The id is the table's own `process_id`, not an external id — this is what
    /// `add_egress_bytes` takes, and resolving an external id to it is the
    /// caller's job because only the caller knows the kind.
    #[must_use]
    pub fn new(process_id: impl Into<String>) -> Self {
        ProcessScope {
            process_id: Arc::from(process_id.into()),
            egress_bytes: Arc::new(AtomicU64::new(0)),
            destinations: Arc::new(Mutex::new(DestinationLog::default())),
            context_reuse: Arc::new(ContextReuseTally::default()),
            max_context_tokens: None,
            class: None,
        }
    }

    /// The row's context budget, for a caller that has just read the row.
    ///
    /// A builder rather than a `new` parameter so the many scopes that have no
    /// budget — which is all of them until one is configured — stay one call.
    #[must_use]
    pub fn with_context_budget(mut self, max_context_tokens: Option<u64>) -> Self {
        self.max_context_tokens = max_context_tokens;
        self
    }

    /// The prompt-token ceiling for one request, or `None` for no budget.
    #[must_use]
    pub fn max_context_tokens(&self) -> Option<u64> {
        self.max_context_tokens
    }

    /// The scheduler class of this process's run, for a caller that has just
    /// resolved it. A builder for [`Self::with_context_budget`]'s reason.
    #[must_use]
    pub fn with_class(mut self, class: Option<crate::run_protocol::ProcessClass>) -> Self {
        self.class = class;
        self
    }

    #[must_use]
    pub fn class(&self) -> Option<crate::run_protocol::ProcessClass> {
        self.class
    }

    #[must_use]
    pub fn process_id(&self) -> &str {
        &self.process_id
    }

    /// Charges `bytes` to this process. Lock-free and safe to call from any task
    /// or thread holding a clone.
    ///
    /// `Relaxed` is sufficient: the only reader is [`Self::take_egress`], which
    /// needs the total to be correct rather than ordered against anything else,
    /// and `fetch_add` is atomic regardless of ordering.
    pub fn charge_egress(&self, bytes: u64) {
        self.egress_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Takes everything charged since the last call, leaving zero behind.
    ///
    /// A drain rather than a read because the ledger's `add_egress_bytes` is
    /// additive: reading without resetting and adding the result would count
    /// every earlier byte again on every flush.
    #[must_use]
    pub fn take_egress(&self) -> u64 {
        self.egress_bytes.swap(0, Ordering::Relaxed)
    }

    /// Notes that this process sent one request to `scheme://host:port`.
    ///
    /// Called once per request rather than once per frame, which is what makes a
    /// lock affordable here where [`Self::charge_egress`] could not afford one:
    /// a body delivers thousands of frames, but it is one request.
    ///
    /// `host` is expected lowercased by the caller, the same convention
    /// [`crate::egress::allowlist_host_matches`] uses, so nothing folds twice.
    pub fn note_destination(&self, scheme: &str, host: &str, port: u16) {
        let mut log = self
            .destinations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let key = Destination {
            scheme: scheme.to_string(),
            host: host.to_string(),
            port,
        };
        if let Some(requests) = log.seen.get_mut(&key) {
            *requests += 1;
            return;
        }
        if log.seen.len() >= MAX_DESTINATIONS {
            // Counted, never silently dropped: a reader that sees a truncated
            // list must be able to tell it is truncated, and by how much.
            log.overflowed += 1;
            return;
        }
        log.seen.insert(key, 1);
    }

    /// Takes everything noted since the last call, leaving the log empty.
    ///
    /// A drain for [`Self::take_egress`]'s reason: the writer is additive, so a
    /// read that left the counts behind would write them again next flush.
    #[must_use]
    pub fn take_destinations(&self) -> DestinationDrain {
        let mut log = self
            .destinations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        DestinationDrain {
            seen: std::mem::take(&mut log.seen).into_iter().collect(),
            overflowed: std::mem::replace(&mut log.overflowed, 0),
        }
    }

    /// Puts a drain back, for a caller whose write of it failed.
    ///
    /// Additive like [`Self::note_destination`], so a flush that fails delays
    /// its counts to the next drain instead of destroying them. A destination
    /// that arrived while the write was in flight keeps its count; the two are
    /// summed rather than one replacing the other.
    ///
    /// The cap is re-applied, so returning a full drain cannot grow the log past
    /// [`MAX_DESTINATIONS`] — the excess becomes overflow, which is what it would
    /// have been had the write never been attempted.
    pub fn return_destinations(&self, drain: DestinationDrain) {
        let mut log = self
            .destinations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        log.overflowed += drain.overflowed;
        for (destination, requests) in drain.seen {
            if let Some(existing) = log.seen.get_mut(&destination) {
                *existing += requests;
            } else if log.seen.len() < MAX_DESTINATIONS {
                log.seen.insert(destination, requests);
            } else {
                log.overflowed += requests;
            }
        }
    }

    /// Records one completion's measured prompt-cache split.
    ///
    /// Only ever called with figures a runtime actually reported. A runtime that
    /// reports no reuse figure must not reach here with `reused = 0` — that would
    /// make "this runtime does not measure reuse" indistinguishable from "this
    /// runtime measured no reuse", and the ledger cannot tell them apart
    /// afterwards.
    pub fn note_context_reuse(&self, reused: u64, evaluated: u64) {
        self.context_reuse
            .reused
            .fetch_add(reused, Ordering::Relaxed);
        self.context_reuse
            .evaluated
            .fetch_add(evaluated, Ordering::Relaxed);
    }

    /// Takes everything noted since the last call, leaving zero behind — a drain
    /// for [`Self::take_egress`]'s reason.
    #[must_use]
    pub fn take_context_reuse(&self) -> ContextReuse {
        ContextReuse {
            reused_tokens: self.context_reuse.reused.swap(0, Ordering::Relaxed),
            evaluated_tokens: self.context_reuse.evaluated.swap(0, Ordering::Relaxed),
        }
    }

    /// Puts a drain back, for a caller whose write of it failed.
    pub fn return_context_reuse(&self, reuse: ContextReuse) {
        self.note_context_reuse(reuse.reused_tokens, reuse.evaluated_tokens);
    }
}

/// The most distinct destinations one process's egress record will hold.
///
/// A ceiling is needed because the count is not purely the app's own: a run that
/// declares no allowlist can be walked across arbitrarily many hosts by the
/// content it fetches, so "one row per distinct destination" is unbounded in the
/// case that matters. 128 is far above what any ordinary run reaches — a chat
/// turn talks to one provider — and requests past it are still counted, just not
/// individually named.
pub const MAX_DESTINATIONS: usize = 128;

/// One host a process sent a request to.
///
/// Port is `port_or_known_default`'s answer rather than the url's literal port,
/// so `https://example.com` and `https://example.com:443` are one destination
/// rather than two.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Destination {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

/// What a scope accumulates between flushes.
#[derive(Debug, Default)]
struct DestinationLog {
    /// Ordered so a drain is deterministic, which is what lets a test assert on
    /// it without sorting first.
    seen: BTreeMap<Destination, u64>,
    overflowed: u64,
}

/// A flush's worth of destinations.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DestinationDrain {
    /// Each destination and how many requests went to it, in [`Destination`]
    /// order.
    pub seen: Vec<(Destination, u64)>,
    /// Requests to destinations past [`MAX_DESTINATIONS`], which are counted but
    /// not named.
    pub overflowed: u64,
}

impl DestinationDrain {
    /// Whether there is nothing here to write down.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty() && self.overflowed == 0
    }
}

/// What the task-local actually holds.
///
/// A struct rather than a third [`RunScope`] arm so that `RunScope::Run(id)` —
/// constructed at call sites this module does not own — keeps compiling and
/// keeps meaning what it meant. A run with no process id is a legitimate state:
/// it means the work belongs to a run whose process row nobody has resolved
/// here, which the egress accounting records as unattributed rather than
/// guessing at.
#[derive(Debug, Clone)]
struct Ambient {
    scope: RunScope,
    process: Option<ProcessScope>,
}

tokio::task_local! {
    /// The ambient scope. Absent — not defaulted — outside every [`scoped`] call,
    /// which is what lets [`current`] distinguish "not instrumented" from
    /// "deliberately no run".
    static SCOPE: Ambient;
}

/// Runs `future` with `scope` as the ambient scope for everything it awaits.
///
/// Nests: an inner call shadows the outer scope for the duration of its future and
/// the outer one is intact afterwards. That falls out of `task_local`'s own
/// semantics rather than being implemented here, and the test below pins it,
/// because a run that spawns sub-work under a different scope must not corrupt its
/// own.
pub async fn scoped<F>(scope: RunScope, future: F) -> F::Output
where
    F: Future,
{
    SCOPE
        .scope(
            Ambient {
                scope,
                process: None,
            },
            future,
        )
        .await
}

/// [`scoped`], with the process-table row the work belongs to.
///
/// The caller keeps its own clone of `process` and drains it into
/// `ProcessTable::add_egress_bytes` — see [`ProcessScope`] for why the counter
/// travels with the identity instead of living in a global map.
pub async fn scoped_with_process<F>(scope: RunScope, process: ProcessScope, future: F) -> F::Output
where
    F: Future,
{
    SCOPE
        .scope(
            Ambient {
                scope,
                process: Some(process),
            },
            future,
        )
        .await
}

/// [`scoped`] for code that is not a future.
///
/// # Why this is needed at all
///
/// A task-local is not inherited across `tokio::spawn` **or**
/// `tokio::task::spawn_blocking`, and a good deal of this app's egress happens under
/// the latter: `browser_worker.rs` runs every browser action through
/// `spawn_blocking`, and its per-subresource decisions are plain synchronous functions
/// several frames below it. For those, [`scoped`] is unusable — there is no future to
/// wrap — so without this the only options were to thread a parameter through every
/// intervening signature or to record a blank.
///
/// Wraps `tokio::task::LocalKey::sync_scope`, which exists for exactly this case.
/// Costs nothing where it is not needed and is not a substitute for [`scoped`] in
/// async code: a scope entered here ends when `body` returns, so an `async` block
/// created inside it does not carry the scope to wherever it is later awaited.
pub fn scoped_sync<F, R>(scope: RunScope, body: F) -> R
where
    F: FnOnce() -> R,
{
    SCOPE.sync_scope(
        Ambient {
            scope,
            process: None,
        },
        body,
    )
}

/// The ambient scope, or `None` at a site no [`scoped`] call encloses.
///
/// `None` means "nobody said", and it is deliberately not the same value as
/// `Some(RunScope::Unattributed(..))` — see this module's doc for why collapsing
/// the two would make the resulting record unreadable.
#[must_use]
pub fn current() -> Option<RunScope> {
    SCOPE.try_with(|ambient| ambient.scope.clone()).ok()
}

/// The process the ambient work belongs to, if the scope was entered with one.
///
/// `None` covers both "no scope at all" and "a scope with no process id"; a
/// caller that has to tell those apart asks [`current`] as well. Nothing here
/// invents an identity for either case.
#[must_use]
pub fn current_process() -> Option<ProcessScope> {
    SCOPE
        .try_with(|ambient| ambient.process.clone())
        .ok()
        .flatten()
}

/// The ambient run id, if the current scope has one.
///
/// The narrow accessor most callers want: a site recording an event wants the id or
/// nothing, and should not have to match on a scope to say so.
#[must_use]
pub fn current_run_id() -> Option<String> {
    current().and_then(|scope| scope.run_id().map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The acceptance clause, in the smallest honest form: an identity set at a
    /// boundary is visible from a site several frames down that was never given it.
    ///
    /// The intermediate functions take no scope parameter on purpose — that is the
    /// whole claim. If they had to, this mechanism would be the parameter threading
    /// it exists to replace.
    #[tokio::test]
    async fn a_scope_set_at_a_boundary_is_visible_frames_below_it() {
        async fn innermost() -> Option<String> {
            current_run_id()
        }
        async fn middle() -> Option<String> {
            innermost().await
        }
        async fn outermost() -> Option<String> {
            middle().await
        }

        let seen = scoped(RunScope::run("run:alpha"), outermost()).await;
        assert_eq!(seen.as_deref(), Some("run:alpha"));
    }

    /// Outside every scope the answer is `None` — not a default, not an invented
    /// identity. Guards the half of the contract that says an uninstrumented site
    /// is distinguishable from a deliberately unattributed one.
    #[tokio::test]
    async fn outside_every_scope_there_is_no_identity_to_report() {
        assert_eq!(current(), None);
        assert_eq!(current_run_id(), None);
    }

    /// Deliberately-no-run is a value, and it is not the same value as "nobody
    /// said". Both report no run id, and only one of them reports a reason.
    #[tokio::test]
    async fn unattributed_work_is_a_statement_and_not_an_absence() {
        let scope = scoped(RunScope::Unattributed(Unattributed::Scheduled), async {
            current()
        })
        .await;

        assert_eq!(
            scope,
            Some(RunScope::Unattributed(Unattributed::Scheduled)),
            "the reason must survive to the reader"
        );
        assert_eq!(
            scope.as_ref().and_then(RunScope::run_id),
            None,
            "unattributed work has no run id"
        );
        assert_eq!(
            scope.and_then(|scope| scope.unattributed()),
            Some(Unattributed::Scheduled)
        );
        // And the contrast that makes the distinction load-bearing.
        assert_eq!(current(), None, "outside a scope there is no reason either");
    }

    /// The second acceptance clause: concurrent runs never observe each other's
    /// identity.
    ///
    /// Written with an await *inside* each scope, because that is the only version
    /// of this test that can fail. Tokio moves a task between worker threads at an
    /// await point, so a thread-local implementation passes a version of this test
    /// with no awaits and then hands one run another's id the moment there is real
    /// work between the set and the read. `yield_now` forces at least one such
    /// hand-off per task.
    ///
    /// Extended to carry a process id alongside the run id, because the ledger's
    /// per-process egress attribution rests on exactly the same property: a
    /// process id that survives the run id's awaits but not its own would charge
    /// one process's bytes to another.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_scopes_never_observe_each_other() {
        let mut tasks = Vec::new();
        for index in 0..32u64 {
            let expected = format!("run:{index}");
            let expected_process = format!("p-turn-{index}");
            tasks.push(tokio::spawn(scoped_with_process(
                RunScope::run(expected.clone()),
                ProcessScope::new(expected_process.clone()),
                {
                    async move {
                        // Three hand-off opportunities around the read, so a leak has
                        // every chance to show up rather than being scheduled away.
                        tokio::task::yield_now().await;
                        let first = current_run_id();
                        let first_process = current_process();
                        tokio::task::yield_now().await;
                        let second = current_run_id();
                        tokio::task::yield_now().await;
                        // Charged from inside the scope, read back by the owner
                        // below: the whole point of the counter travelling with
                        // the identity.
                        if let Some(process) = current_process() {
                            process.charge_egress(index + 1);
                        }
                        (
                            expected,
                            first,
                            second,
                            expected_process,
                            first_process,
                            current_process(),
                        )
                    }
                },
            )));
        }

        let mut observed = BTreeSet::new();
        for task in tasks {
            let (expected, first, second, expected_process, first_process, last_process) =
                task.await.expect("scoped task joins");
            assert_eq!(
                first.as_deref(),
                Some(expected.as_str()),
                "a task must read back its own scope across an await"
            );
            assert_eq!(second, first, "and read the same one twice");
            assert_eq!(
                first_process.as_ref().map(ProcessScope::process_id),
                Some(expected_process.as_str()),
                "a task must read back its own process id across an await"
            );
            assert_eq!(
                last_process.as_ref().map(ProcessScope::process_id),
                Some(expected_process.as_str()),
                "and the same one after two more hand-offs"
            );
            assert_eq!(
                last_process.expect("a process is in scope").take_egress(),
                observed.len() as u64 + 1,
                "each task's bytes must land in its own tally"
            );
            observed.insert(expected);
        }
        assert_eq!(observed.len(), 32, "every task must have been checked");
    }

    /// A run with no process id is a state, not a failure: the run id is still
    /// there and the process side reports nothing rather than something invented.
    #[tokio::test]
    async fn a_scope_may_carry_a_run_without_a_process() {
        let (run, process) = scoped(RunScope::run("run:alpha"), async {
            (current_run_id(), current_process())
        })
        .await;
        assert_eq!(run.as_deref(), Some("run:alpha"));
        assert!(
            process.is_none(),
            "no process row was named, so none may be reported"
        );
        // And outside every scope, neither half exists.
        assert!(current_process().is_none());
    }

    /// The counter is shared by clones and drains to zero, which is what makes it
    /// safe to feed the ledger's additive `add_egress_bytes`.
    #[test]
    fn a_process_scope_tally_is_shared_by_clones_and_drains_once() {
        let owner = ProcessScope::new("p-turn-1");
        let charged = owner.clone();
        charged.charge_egress(1_024);
        charged.charge_egress(512);

        assert_eq!(owner.take_egress(), 1_536, "a clone charges the same tally");
        assert_eq!(
            owner.take_egress(),
            0,
            "a drained tally must not report the same bytes twice"
        );
        assert_eq!(charged.take_egress(), 0, "and the drain is shared too");
    }

    /// Two completions' measured splits sum into one tally, and the hit rate that
    /// falls out is the whole process's — not the mean of the two rates, which
    /// would weigh a 10-token turn the same as a 1000-token one.
    #[test]
    fn context_reuse_accumulates_over_completions_and_drains_once() {
        let owner = ProcessScope::new("p-reuse");
        let charged = owner.clone();
        // A cold turn, then a warm one over the same prefix.
        charged.note_context_reuse(0, 1_000);
        charged.note_context_reuse(9, 1);

        let drained = owner.take_context_reuse();
        assert_eq!(drained.reused_tokens, 9);
        assert_eq!(drained.evaluated_tokens, 1_001);
        // The mean of the two turns' rates would be 45%; the process's own rate is
        // 9/1010, which is the figure that says what the cache actually saved.
        assert!(
            owner.take_context_reuse().is_empty(),
            "a drain is once only"
        );

        owner.return_context_reuse(drained);
        assert_eq!(
            owner.take_context_reuse(),
            drained,
            "a failed write puts the measurement back rather than losing it"
        );
    }

    /// Repeat requests raise a count rather than adding a row, and the cap turns
    /// the rest into a number instead of dropping them.
    #[test]
    fn destinations_dedupe_and_the_overflow_past_the_cap_is_counted() {
        let scope = ProcessScope::new("p-turn-2");
        scope.note_destination("https", "api.example.com", 443);
        scope.note_destination("https", "api.example.com", 443);
        // Same host, different port: a different destination, not the same one.
        scope.note_destination("https", "api.example.com", 8443);

        // Two slots are already taken, so the last two of these do not fit — and
        // then one more on top, for three requests past the cap in total.
        for index in 0..MAX_DESTINATIONS {
            scope.note_destination("https", &format!("host-{index}.example.com"), 443);
        }
        scope.note_destination("https", "one-too-many.example.com", 443);

        let drain = scope.take_destinations();
        assert_eq!(drain.seen.len(), MAX_DESTINATIONS, "the cap is the ceiling");
        assert_eq!(
            drain
                .seen
                .iter()
                .find(|(destination, _)| destination.host == "api.example.com"
                    && destination.port == 443)
                .map(|(_, requests)| *requests),
            Some(2),
            "a repeat request raises the count on the destination it already has"
        );
        assert_eq!(
            drain.overflowed, 3,
            "requests past the cap are counted, never silently dropped"
        );
        assert!(
            scope.take_destinations().is_empty(),
            "a drained log must not report the same requests twice"
        );
    }

    /// A failed write can hand the drain back without losing or double-counting.
    #[test]
    fn returning_a_drain_sums_with_what_arrived_meanwhile() {
        let scope = ProcessScope::new("p-turn-3");
        scope.note_destination("https", "api.example.com", 443);
        let drain = scope.take_destinations();

        // The request that arrived while the (failed) write was in flight.
        scope.note_destination("https", "api.example.com", 443);
        scope.return_destinations(drain);

        let recovered = scope.take_destinations();
        assert_eq!(
            recovered.seen,
            vec![(
                Destination {
                    scheme: "https".to_string(),
                    host: "api.example.com".to_string(),
                    port: 443,
                },
                2,
            )],
            "the returned count and the new one are summed, not one replacing the other"
        );
    }

    /// `tokio::spawn` does not inherit the scope, and this pins it rather than
    /// wishing otherwise.
    ///
    /// Documented as a test because the failure it prevents is silent: somebody
    /// assumes a detached task carries the run, sees a blank column, and concludes
    /// the mechanism is broken. It is not — a spawned task may outlive the run that
    /// spawned it, so inheriting would attribute work to a finished run. Work that
    /// continues in a spawned task re-enters the scope itself, exactly as the test
    /// above does.
    #[tokio::test]
    async fn a_spawned_task_does_not_inherit_the_scope() {
        let inherited = scoped(RunScope::run("run:parent"), async {
            tokio::spawn(async { current() })
                .await
                .expect("spawned task joins")
        })
        .await;

        assert_eq!(
            inherited, None,
            "a detached task starts outside every scope; re-enter it deliberately"
        );
    }

    /// Nesting shadows and then restores, so sub-work under a different scope
    /// cannot corrupt the scope it was launched from.
    #[tokio::test]
    async fn a_nested_scope_shadows_and_then_restores() {
        let (inner, after) = scoped(RunScope::run("run:outer"), async {
            let inner = scoped(RunScope::run("run:inner"), async { current_run_id() }).await;
            (inner, current_run_id())
        })
        .await;

        assert_eq!(inner.as_deref(), Some("run:inner"));
        assert_eq!(
            after.as_deref(),
            Some("run:outer"),
            "the outer scope must survive the inner one"
        );
    }

    /// Codes are the persisted identity, so they are pinned here against a written
    /// list: renaming one orphans every row already recorded under the old
    /// spelling. Same reason `EgressRule`'s codes are pinned.
    #[test]
    fn every_unattributed_reason_has_a_stable_unique_code() {
        let codes: Vec<&str> = Unattributed::ALL
            .iter()
            .map(|reason| reason.code())
            .collect();
        assert_eq!(
            codes,
            vec![
                "unattributed.user-action",
                "unattributed.scheduled",
                "unattributed.inbound-request",
                "unattributed.startup",
                "unattributed.shared-transport",
            ]
        );
        assert_eq!(
            codes.iter().collect::<BTreeSet<_>>().len(),
            codes.len(),
            "two reasons sharing a code would be indistinguishable once stored"
        );
    }
}
