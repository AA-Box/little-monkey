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

use std::fmt;
use std::future::Future;

/// Why a piece of work legitimately belongs to no run.
///
/// A closed set with stable codes rather than free text, for the reason
/// [`crate::egress::EgressRule`] is: the code is what gets persisted and compared,
/// so it has to outlive both the prose and this enum's spelling. Four variants
/// covering the five cases the audit actually found — a model download and an
/// update check are both `Startup`-class only if they happen at startup, so a
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
}

impl Unattributed {
    /// Every reason, in declaration order.
    pub const ALL: &'static [Unattributed] = &[
        Unattributed::UserAction,
        Unattributed::Scheduled,
        Unattributed::InboundRequest,
        Unattributed::Startup,
    ];

    /// The stable identity that gets persisted. Never reworded.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Unattributed::UserAction => "unattributed.user-action",
            Unattributed::Scheduled => "unattributed.scheduled",
            Unattributed::InboundRequest => "unattributed.inbound-request",
            Unattributed::Startup => "unattributed.startup",
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

tokio::task_local! {
    /// The ambient scope. Absent — not defaulted — outside every [`scoped`] call,
    /// which is what lets [`current`] distinguish "not instrumented" from
    /// "deliberately no run".
    static SCOPE: RunScope;
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
    SCOPE.scope(scope, future).await
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
    SCOPE.sync_scope(scope, body)
}

/// The ambient scope, or `None` at a site no [`scoped`] call encloses.
///
/// `None` means "nobody said", and it is deliberately not the same value as
/// `Some(RunScope::Unattributed(..))` — see this module's doc for why collapsing
/// the two would make the resulting record unreadable.
#[must_use]
pub fn current() -> Option<RunScope> {
    SCOPE.try_with(Clone::clone).ok()
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_scopes_never_observe_each_other() {
        let mut tasks = Vec::new();
        for index in 0..32 {
            let expected = format!("run:{index}");
            tasks.push(tokio::spawn(scoped(RunScope::run(expected.clone()), {
                async move {
                    // Three hand-off opportunities around the read, so a leak has
                    // every chance to show up rather than being scheduled away.
                    tokio::task::yield_now().await;
                    let first = current_run_id();
                    tokio::task::yield_now().await;
                    let second = current_run_id();
                    tokio::task::yield_now().await;
                    (expected, first, second)
                }
            })));
        }

        let mut observed = BTreeSet::new();
        for task in tasks {
            let (expected, first, second) = task.await.expect("scoped task joins");
            assert_eq!(
                first.as_deref(),
                Some(expected.as_str()),
                "a task must read back its own scope across an await"
            );
            assert_eq!(second, first, "and read the same one twice");
            observed.insert(expected);
        }
        assert_eq!(observed.len(), 32, "every task must have been checked");
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
            ]
        );
        assert_eq!(
            codes.iter().collect::<BTreeSet<_>>().len(),
            codes.len(),
            "two reasons sharing a code would be indistinguishable once stored"
        );
    }
}
