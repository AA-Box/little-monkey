//! One process-table lifecycle for every bounded agent-controlled execution.
//!
//! # The gap this closes
//!
//! Three executions — a verify command, a lifecycle hook, a disposable-copy
//! sandbox run — already ran under the same [`ResourceController`] as an agent
//! shell. The bound was installed before the first instruction, the tree was
//! measured, and a breach reclaimed the whole thing. None of it was *visible*: a
//! limit that fired on a verify command was reported in that command's own
//! result string and nowhere the processes ledger could show it, so "what is this
//! machine doing on the agent's behalf, and what is holding it" had three blind
//! spots.
//!
//! The missing half was never enforcement. It was the row.
//!
//! # Why a type rather than three copies of the same five calls
//!
//! The lifecycle is admit → attach → sample → close, and every step has a rule
//! that is easy to get subtly wrong once per call site: the row must exist before
//! the workload starts (or a command that dies immediately has no record at all),
//! the containment must be recorded from the controller rather than assumed, a
//! breach must reach the row as typed fields rather than as prose, and **the row
//! must close even on a path nobody thought about**. The last one is why [`Drop`]
//! is implemented: an owner that returns early through a `?` leaves a row saying
//! `running` forever, and a process table with permanent phantoms is worse than
//! one with gaps.
//!
//! # Fail-soft, at every write
//!
//! A command must never fail because a bookkeeping row could not be written —
//! the same contract every other adopter of [`ProcessProjector`] keeps. Errors
//! are reported to stderr and swallowed here, which is the one place the decision
//! is made rather than three.

use std::sync::Arc;

use crate::process_table::{
    ProcessExit, ProcessKind, ProcessLimits, ProcessProjection, ProcessProjector, ProcessState,
};
use crate::resource_control::{Containment, ResourceController, ResourceSample};

/// A [`ProcessProjector`] over the app's own pooled ledger connection.
///
/// Deliberately not a second [`crate::process_table::LedgerProcessProjector`]:
/// that one opens a connection of its own, which is right for a service with no
/// app handle and wrong for a desktop command that is already holding one. The
/// handle is all this needs — `AppState` is reachable from it — which is what
/// lets an owner deep in `verify.rs` project without threading two parameters
/// through every frame.
pub struct AppProcessProjector<R: tauri::Runtime> {
    app: tauri::AppHandle<R>,
}

impl<R: tauri::Runtime> AppProcessProjector<R> {
    pub fn new(app: tauri::AppHandle<R>) -> Self {
        AppProcessProjector { app }
    }

    /// The projector as the shared handle a [`BoundedExecution`] takes.
    pub fn shared(app: tauri::AppHandle<R>) -> Arc<dyn ProcessProjector> {
        Arc::new(Self::new(app))
    }
}

impl<R: tauri::Runtime> ProcessProjector for AppProcessProjector<R> {
    fn project(&self, projection: &ProcessProjection) -> Result<(), String> {
        use tauri::Manager;
        let state = self.app.state::<crate::AppState>();
        crate::process_commands::project_process_record(&self.app, state.inner(), projection)
    }
}

/// The projector for a host with no `AppHandle` — `monkey-cli`, which runs the
/// same verify commands the desktop does.
///
/// Resolved through [`crate::app_paths::data_dir`] rather than the bare app-data
/// root, because that is the profile chokepoint: a store that skips it writes
/// rows into whichever profile happened to be active when the path was cached.
///
/// `None` where the active profile cannot be resolved at all, which is the one
/// case with nowhere to write; a run is never refused over it.
#[must_use]
pub fn cli_projector() -> Option<Arc<dyn ProcessProjector>> {
    let path = crate::app_paths::data_dir()?.join(crate::run_commands::DATABASE_FILE);
    Some(Arc::new(crate::process_table::LedgerProcessProjector::new(
        path,
    )))
}

/// One bounded execution's row, from admission to close-out.
pub struct BoundedExecution {
    projector: Arc<dyn ProcessProjector>,
    kind: ProcessKind,
    external_id: String,
    parent: Option<(ProcessKind, String)>,
    workspace: Option<String>,
    /// The *effective* limits, not what the caller asked for: what a reader sees
    /// must be the number installed on the tree.
    limits: ProcessLimits,
    identity: Option<crate::process_tree::ProcessIdentity>,
    containment: Option<Containment>,
    /// Whether a terminal row has been written. Read by [`Drop`], which is the
    /// backstop for every path that returns without closing.
    closed: bool,
}

impl BoundedExecution {
    /// Admit a row before the workload starts.
    ///
    /// Before, and not after: a command that fails to `exec`, or prints one line
    /// and exits, is exactly the case a reader wants a record of, and a row
    /// written after the wait would miss all of them.
    ///
    /// The external id is minted here rather than taken, because none of these
    /// three executions has a durable identifier of its own — a verify command's
    /// `command_id` names the *configured* command and repeats on every run, so
    /// using it would collide on the table's `UNIQUE(kind, external_id)` the
    /// second time anyone pressed the button.
    #[must_use]
    pub fn admit(
        projector: Arc<dyn ProcessProjector>,
        kind: ProcessKind,
        workspace: Option<String>,
        limits: ProcessLimits,
    ) -> Self {
        let execution = BoundedExecution {
            projector,
            kind,
            external_id: format!("{}-{}", kind.tag(), uuid::Uuid::new_v4()),
            parent: None,
            workspace,
            limits,
            identity: None,
            containment: None,
            closed: false,
        };
        execution.project(execution.projection(ProcessState::Admitted, None, None));
        execution
    }

    /// Name the turn or run that asked for this work, so
    /// `monkey processes list --parent` can answer "what did this turn run".
    ///
    /// A setter rather than a consuming builder because the common caller holds
    /// the execution in an `Option` it is still going to use, and threading a
    /// move through that is how the borrow checker gets argued with instead of
    /// listened to.
    pub fn set_parent(&mut self, kind: ProcessKind, external_id: impl Into<String>) {
        self.parent = Some((kind, external_id.into()));
    }

    /// This execution's process id, for a caller that wants to name it in its own
    /// result.
    #[must_use]
    pub fn external_id(&self) -> &str {
        &self.external_id
    }

    /// The workload is attached and running: record its identity and what holds
    /// it.
    ///
    /// Called after [`ResourceController::attach`] rather than after the spawn,
    /// because the controller only knows both facts once the attach has verified
    /// the containment — a row that claimed a cgroup before the membership was
    /// read back would be asserting the thing the attach exists to check.
    pub fn running(&mut self, controller: &ResourceController) {
        self.identity = controller.root();
        self.containment = Some(controller.containment());
        let projection = self.projection(ProcessState::Running, None, None);
        self.project(projection);
    }

    /// Record what the controller is measuring right now.
    ///
    /// Called on the sampling loop's own tick, so the panel shows a live process
    /// holding what it is actually holding rather than a number that only appears
    /// once the process is dead.
    pub fn sampled(&self, sample: &ResourceSample) {
        let projection = self.projection(ProcessState::Running, None, Some(*sample));
        self.project(projection);
    }

    /// Close the row with the outcome, and the last measurement anything took.
    pub fn exited(mut self, exit: ProcessExit, sample: Option<ResourceSample>) {
        self.closed = true;
        let projection = self.projection(ProcessState::Exited, Some(exit), sample);
        self.project(projection);
    }

    fn projection(
        &self,
        state: ProcessState,
        exit: Option<ProcessExit>,
        usage: Option<ResourceSample>,
    ) -> ProcessProjection {
        let mut projection = ProcessProjection::new(self.kind, &self.external_id, state)
            .with_workspace(self.workspace.clone())
            .with_native_identity(self.identity)
            .with_limits(self.limits)
            .with_containment(self.containment.clone())
            .with_usage(usage);
        projection.exit = exit;
        if let Some((parent_kind, parent_external)) = &self.parent {
            projection = projection.with_parent(*parent_kind, parent_external.clone());
        }
        projection
    }

    fn project(&self, projection: ProcessProjection) {
        if let Err(error) = self.projector.project(&projection) {
            eprintln!(
                "{}: could not record {} in the process table: {error}",
                self.kind.as_str(),
                projection.external_id
            );
        }
    }
}

/// The backstop for every path that returns without closing the row.
///
/// A `?` on an unrelated failure, a panic unwinding through the owner, a branch
/// added later that forgets: all of them used to be able to leave a row saying
/// `running` for a process that ended minutes ago, and nothing would ever
/// correct it — these kinds are swept at startup, so the phantom would survive
/// until the next app launch.
///
/// [`ExitStatus::Lost`] rather than a manufactured success, and with a reason
/// that says what happened rather than inventing an outcome. This is the honest
/// answer: the work ended and its owner did not report how.
///
/// [`ExitStatus::Lost`]: crate::process_table::ExitStatus::Lost
impl Drop for BoundedExecution {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        let mut projection = self.projection(ProcessState::Exited, None, None);
        projection.exit = Some(ProcessExit {
            status: crate::process_table::ExitStatus::Lost,
            code: None,
            signal: None,
            reason: Some(
                "this execution's owner went away without recording an outcome".to_string(),
            ),
            breach: None,
        });
        self.project(projection);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_table::{ExitStatus, ProcessTable};
    use crate::run_ledger::RunLedger;
    use std::sync::Mutex;

    /// A projector over an in-memory ledger, so these tests exercise the real
    /// reconcile and the real SQL rather than a recording fake that would only
    /// prove this module calls itself.
    struct LedgerFake {
        ledger: Mutex<RunLedger>,
    }

    impl ProcessProjector for LedgerFake {
        fn project(&self, projection: &ProcessProjection) -> Result<(), String> {
            let ledger = self.ledger.lock().map_err(|_| "poisoned".to_string())?;
            ProcessTable::new(ledger.connection())
                .reconcile(projection, 1_800_000_000_000)
                .map(|_| ())
                .map_err(|error| error.to_string())
        }
    }

    fn fake() -> Arc<LedgerFake> {
        Arc::new(LedgerFake {
            ledger: Mutex::new(RunLedger::open_in_memory().expect("in-memory ledger opens")),
        })
    }

    fn row(fake: &LedgerFake, external_id: &str) -> crate::process_table::ProcessRecord {
        let ledger = fake.ledger.lock().expect("not poisoned");
        ProcessTable::new(ledger.connection())
            .find_by_external_id(ProcessKind::VerifyCommand, external_id)
            .expect("the lookup runs")
            .expect("the row exists")
    }

    /// The whole point of the type: a row exists from before the workload starts
    /// until after it ends, and carries the effective limits throughout.
    #[test]
    fn a_bounded_execution_has_a_row_before_it_runs_and_after_it_ends() {
        let projector = fake();
        let limits = ProcessKind::VerifyCommand.default_limits();
        let execution = BoundedExecution::admit(
            projector.clone(),
            ProcessKind::VerifyCommand,
            Some("/tmp/workspace".to_string()),
            limits,
        );
        let external_id = execution.external_id().to_string();

        let admitted = row(&projector, &external_id);
        assert_eq!(admitted.state, ProcessState::Admitted);
        assert_eq!(admitted.limits, limits);
        assert_eq!(admitted.workspace.as_deref(), Some("/tmp/workspace"));

        execution.exited(ProcessExit::succeeded(), None);
        let exited = row(&projector, &external_id);
        assert_eq!(exited.state, ProcessState::Exited);
        assert_eq!(
            exited.exit.expect("an exited row carries its exit").status,
            ExitStatus::Succeeded
        );
    }

    /// The failure `Drop` exists for: an owner that returns early must not leave
    /// a row that says `running` for the rest of the machine's life.
    #[test]
    fn an_owner_that_drops_without_closing_still_closes_the_row() {
        let projector = fake();
        let external_id = {
            let execution = BoundedExecution::admit(
                projector.clone(),
                ProcessKind::VerifyCommand,
                None,
                ProcessKind::VerifyCommand.default_limits(),
            );
            execution.external_id().to_string()
        };

        let closed = row(&projector, &external_id);
        assert_eq!(closed.state, ProcessState::Exited);
        let exit = closed
            .exit
            .expect("a dropped execution still records an exit");
        assert_eq!(exit.status, ExitStatus::Lost);
        assert!(
            exit.reason
                .as_deref()
                .is_some_and(|reason| reason.contains("without recording an outcome")),
            "the reason must say what happened rather than invent an outcome"
        );
    }

    /// A limit kill has to reach the row as typed fields — which limit, what was
    /// configured, what was observed, which backend and at what level — because
    /// prose is not something a UI or a query can act on.
    #[test]
    fn a_limit_kill_reaches_the_row_as_typed_fields() {
        let projector = fake();
        let execution = BoundedExecution::admit(
            projector.clone(),
            ProcessKind::VerifyCommand,
            None,
            ProcessKind::VerifyCommand.default_limits(),
        );
        let external_id = execution.external_id().to_string();
        let breach = crate::resource_control::LimitBreach {
            limit: crate::process_table::ProcessLimitKind::Memory
                .as_str()
                .to_string(),
            configured: 8 * 1024 * 1024 * 1024,
            observed: 9 * 1024 * 1024 * 1024,
            backend: "cgroup v2".to_string(),
            level: "kernel".to_string(),
            observed_at_ms: 1_800_000_000_000,
            evidence: Some("cgroup v2 `memory.events` oom_kill".to_string()),
        };
        execution.exited(ProcessExit::limit_exceeded(breach.clone()), None);

        let closed = row(&projector, &external_id);
        let exit = closed.exit.expect("an exited row carries its exit");
        assert_eq!(exit.status, ExitStatus::LimitExceeded);
        assert_eq!(exit.breach, Some(breach));
    }

    /// Peaks are the row's, not the sample's: a later sample that measures less
    /// must not lower the highest value anything ever saw.
    #[test]
    fn a_later_sample_updates_the_current_value_and_never_lowers_a_peak() {
        let projector = fake();
        let mut execution = BoundedExecution::admit(
            projector.clone(),
            ProcessKind::VerifyCommand,
            None,
            ProcessKind::VerifyCommand.default_limits(),
        );
        let external_id = execution.external_id().to_string();
        // Attaching is what moves the row to `running`, and a sample before that
        // would have nowhere to land.
        execution.identity = crate::process_tree::ProcessIdentity::of(std::process::id());
        let projection = execution.projection(ProcessState::Running, None, None);
        execution.project(projection);

        execution.sampled(&ResourceSample {
            wall_ms: 10,
            rss_bytes: Some(4_000),
            peak_rss_bytes: Some(4_000),
            process_count: Some(3),
            peak_process_count: Some(3),
            output_bytes: Some(11),
        });
        execution.sampled(&ResourceSample {
            wall_ms: 20,
            rss_bytes: Some(1_000),
            peak_rss_bytes: Some(1_000),
            process_count: Some(1),
            peak_process_count: Some(1),
            output_bytes: Some(22),
        });

        let live = row(&projector, &external_id);
        let usage = live.usage.expect("a sampled row carries its measurement");
        assert_eq!(usage.rss_bytes, Some(1_000), "current follows the sample");
        assert_eq!(
            usage.peak_rss_bytes,
            Some(4_000),
            "a peak is the highest anything saw, not the latest controller's own"
        );
        assert_eq!(usage.process_count, Some(1));
        assert_eq!(usage.peak_process_count, Some(3));
        assert_eq!(usage.output_bytes, Some(22));
        assert!(live.usage_sampled_at_ms.is_some());

        execution.exited(ProcessExit::succeeded(), None);
    }
}
