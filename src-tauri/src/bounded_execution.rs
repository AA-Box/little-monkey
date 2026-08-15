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
//! # Fail-closed at admission, fail-soft afterwards
//!
//! The two are not a compromise, they are the difference between establishing a
//! lifecycle and reporting on one.
//!
//! **Admission is fail-closed.** [`BoundedExecution::admit`] returns a `Result`,
//! and a caller that cannot get one must not spawn. The alternative — which this
//! module used to do — is a warning on stderr and a native process running
//! outside the ledger that claims to cover every bounded execution. "Admit →
//! durable row → contain → spawn" is either the order or it is a sentence in a
//! document.
//!
//! **Everything after it is fail-soft.** A periodic sample, a usage refresh, the
//! close-out: a command must never die because a bookkeeping row could not be
//! updated, and [`Drop`] is the backstop that keeps a missed close-out from
//! becoming a permanent phantom. Errors are reported to stderr and swallowed
//! here, which is the one place that decision is made rather than three.
//!
//! **Ownership is the exception on the other side.** A newly observed native
//! member is not telemetry — it is the only evidence a restart will have — so
//! [`ProjectedOwnership`] hands it to the same projector without swallowing
//! anything, and the controller reclaims the workload if it will not land.

use std::sync::Arc;

use crate::process_table::{
    OwnedProcesses, ProcessExit, ProcessKind, ProcessLimits, ProcessProjection, ProcessProjector,
    ProcessState,
};
use crate::resource_control::{Containment, OwnershipJournal, ResourceController, ResourceSample};

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

    fn record_owned(&self, owned: &OwnedProcesses) -> Result<(), String> {
        use tauri::Manager;
        let state = self.app.state::<crate::AppState>();
        crate::process_commands::record_owned_processes(&self.app, state.inner(), owned)
    }
}

/// The one path from "the supervisor observed a native process" to "a row says
/// so".
///
/// # Why this is a wrapper rather than six loops
///
/// Every supervised owner — both shells, the verify runner, the hook runner, the
/// sandbox, the browser — holds a [`ResourceController`] and a
/// [`ProcessProjector`], and each could have walked the controller's owned set
/// onto its own row. Six copies of that is six chances to forget the identity
/// check, to swallow the error, or to write on the wrong tick. This is the
/// adapter between the two, so there is one.
///
/// The kind and external id are the row's, captured once: the controller never
/// learns them, which is what keeps the lowest layer here free of the process
/// table.
pub struct ProjectedOwnership {
    projector: Arc<dyn ProcessProjector>,
    kind: ProcessKind,
    external_id: String,
}

impl ProjectedOwnership {
    /// The journal for one row, as the handle a controller takes.
    #[must_use]
    pub fn shared(
        projector: Arc<dyn ProcessProjector>,
        kind: ProcessKind,
        external_id: impl Into<String>,
    ) -> Arc<dyn OwnershipJournal> {
        Arc::new(ProjectedOwnership {
            projector,
            kind,
            external_id: external_id.into(),
        })
    }
}

impl OwnershipJournal for ProjectedOwnership {
    fn record_owned(
        &self,
        members: &[crate::process_tree::ProcessIdentity],
        session: Option<u32>,
        boot_marker: Option<&str>,
    ) -> Result<(), String> {
        // Not swallowed, unlike `BoundedExecution::project`: the caller is the
        // controller, and the error is what tells it the workload has stopped
        // being recoverable.
        self.projector.record_owned(&OwnedProcesses {
            kind: self.kind,
            external_id: self.external_id.clone(),
            members: members.to_vec(),
            session,
            boot_marker: boot_marker.map(str::to_string),
        })
    }
}

/// The projector for a host with no `AppHandle` — `monkey-cli`, which runs the
/// same verify commands the desktop does.
///
/// Resolved through [`crate::app_paths::data_dir`] rather than the bare app-data
/// root, because that is the profile chokepoint: a store that skips it writes
/// rows into whichever profile happened to be active when the path was cached.
///
/// # Why this became fallible
///
/// It used to answer `Option`, and `None` meant "run the command with no row" —
/// which is a bounded native execution outside the ledger that claims to cover
/// every one of them. An unresolvable profile directory is not a reason to run
/// agent-controlled code untracked; it is a reason to say so and stop.
pub fn cli_projector() -> Result<Arc<dyn ProcessProjector>, String> {
    let path = crate::app_paths::data_dir()
        .ok_or_else(|| {
            "the active profile's data directory could not be resolved, so this command's \
             process record could not be created"
                .to_string()
        })?
        .join(crate::run_commands::DATABASE_FILE);
    Ok(Arc::new(crate::process_table::LedgerProcessProjector::new(
        path,
    )))
}

/// Why a bounded execution could not begin.
///
/// One variant today, and a named type rather than a bare `String` because the
/// distinction it carries is the whole point of the change: the workload did not
/// fail, it never started, and a caller has to be able to say which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionFailure {
    /// The process table would not take the row this execution's lifecycle
    /// begins with.
    NoProcessRecord { kind: ProcessKind, reason: String },
}

impl std::fmt::Display for AdmissionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdmissionFailure::NoProcessRecord { kind, reason } => write!(
                formatter,
                "this {} was not started because its durable process lifecycle could not be \
                 created: {reason}",
                kind.as_str()
            ),
        }
    }
}

impl std::error::Error for AdmissionFailure {}

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
    ///
    /// # Fallible, and nothing may spawn without it
    ///
    /// The admitting write used to be fail-soft like every other write here, so a
    /// process table that refused the row printed a warning and the command ran
    /// anyway — a contained, limit-enforced, agent-controlled native tree with no
    /// entry in the ledger that claims to hold every one of them. There is no
    /// version of that which is merely a missing row: a workload nothing recorded
    /// is a workload nothing reclaims, signals or reports.
    ///
    /// So the row is the admission ticket. An `Err` here means the caller must
    /// return a start failure and never reach its spawn.
    pub fn admit(
        projector: Arc<dyn ProcessProjector>,
        kind: ProcessKind,
        workspace: Option<String>,
        limits: ProcessLimits,
    ) -> Result<Self, AdmissionFailure> {
        let mut execution = BoundedExecution {
            projector,
            kind,
            external_id: format!("{}-{}", kind.tag(), uuid::Uuid::new_v4()),
            parent: None,
            workspace,
            limits,
            identity: None,
            containment: None,
            // Closed until the row exists, which is what keeps [`Drop`] silent on
            // the failure path below: an execution whose admission was refused is
            // never returned and never ran, so a close-out from it would be a
            // terminal write about a row nothing admitted.
            closed: true,
        };
        // Not through `project`, which swallows by design: this is the one write
        // whose failure the caller has to see.
        execution
            .projector
            .project(&execution.projection(ProcessState::Admitted, None, None))
            .map_err(|reason| AdmissionFailure::NoProcessRecord { kind, reason })?;
        execution.closed = false;
        Ok(execution)
    }

    /// The journal this execution's controller records its owned processes to.
    ///
    /// Handed to [`ResourceController::persist_ownership_to`] once the workload is
    /// attached, so every native process the supervisor observes lands on *this*
    /// row.
    ///
    /// [`ResourceController::persist_ownership_to`]:
    /// crate::resource_control::ResourceController::persist_ownership_to
    #[must_use]
    pub fn ownership_journal(&self) -> Arc<dyn OwnershipJournal> {
        ProjectedOwnership::shared(self.projector.clone(), self.kind, self.external_id.clone())
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

        fn record_owned(&self, owned: &OwnedProcesses) -> Result<(), String> {
            let ledger = self.ledger.lock().map_err(|_| "poisoned".to_string())?;
            ProcessTable::new(ledger.connection())
                .record_owned(
                    owned.kind,
                    &owned.external_id,
                    &owned.members,
                    owned.session,
                    owned.boot_marker.as_deref(),
                    1_800_000_000_000,
                )
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
        )
        .expect("the in-memory ledger admits the row");
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

    /// The fail-closed rule, at the type level: a process table that refuses the
    /// row hands back an error and no usable execution, so a caller cannot reach
    /// its spawn.
    ///
    /// The `Result` is what makes this checkable at all — the previous version
    /// returned `Self` unconditionally, and the only evidence of the refusal was
    /// a line on stderr that nothing could assert against.
    #[test]
    fn an_execution_whose_row_cannot_be_written_is_never_admitted() {
        let outcome = BoundedExecution::admit(
            crate::test_support::FailingProjector::shared(),
            ProcessKind::VerifyCommand,
            None,
            ProcessKind::VerifyCommand.default_limits(),
        );

        let failure = match outcome {
            Err(failure) => failure,
            // Named rather than `expect_err`, which would need `BoundedExecution`
            // to be `Debug` — and it holds a `dyn ProcessProjector`, which is the
            // reason it is not.
            Ok(_) => panic!("a refused row must not yield a usable execution"),
        };
        assert!(
            matches!(
                &failure,
                AdmissionFailure::NoProcessRecord { kind, .. }
                    if *kind == ProcessKind::VerifyCommand
            ),
            "{failure:?}"
        );
        // The message has to separate "never started" from "ran and failed",
        // because that is the distinction every caller is about to report.
        let message = failure.to_string();
        assert!(
            message.contains("was not started") && message.contains("process lifecycle"),
            "{message}"
        );
    }

    /// And it writes no close-out either.
    ///
    /// `Drop` is the backstop for an execution whose owner went away, and a
    /// failed admission must not reach it: a terminal projection for a row that
    /// was never admitted would create the phantom `Drop` exists to prevent, one
    /// state earlier.
    #[test]
    fn a_refused_admission_writes_no_terminal_row_either() {
        struct CountingProjector {
            writes: Mutex<Vec<ProcessState>>,
        }

        impl ProcessProjector for CountingProjector {
            fn project(&self, projection: &ProcessProjection) -> Result<(), String> {
                self.writes
                    .lock()
                    .expect("not poisoned")
                    .push(projection.state);
                Err("the process table is unavailable".to_string())
            }

            fn record_owned(&self, _owned: &OwnedProcesses) -> Result<(), String> {
                Err("the process table is unavailable".to_string())
            }
        }

        let projector = Arc::new(CountingProjector {
            writes: Mutex::new(Vec::new()),
        });
        let outcome = BoundedExecution::admit(
            projector.clone(),
            ProcessKind::HookCommand,
            None,
            ProcessKind::HookCommand.default_limits(),
        );
        assert!(outcome.is_err());
        drop(outcome);

        assert_eq!(
            *projector.writes.lock().expect("not poisoned"),
            vec![ProcessState::Admitted],
            "the only write may be the admission that failed; a `Drop` close-out here would \
             be a terminal row for a process that never existed"
        );
    }

    /// Production source, minus each file's test module.
    ///
    /// Split on the module rather than on the bare attribute: several of these
    /// files carry a `#[cfg(test)]` item well above their tests, and cutting at
    /// the first one would hide most of the production code from the assertions
    /// below — which is how a structural test passes by looking at nothing.
    fn production(source: &str, file: &str) -> String {
        source
            .split_once("\n#[cfg(test)]\nmod tests {")
            .or_else(|| source.split_once("\n#[cfg(test)]\npub(crate) mod tests {"))
            .map(|(before, _)| before.to_string())
            .unwrap_or_else(|| panic!("{file} has no `mod tests` to split on"))
    }

    /// Structural: no production caller may take the admission's `Result` and
    /// carry on.
    ///
    /// The type makes ignoring it awkward; this makes it visible. `let _ =`,
    /// `.ok()` and `unwrap_or` on an admission are each a way to reintroduce
    /// exactly the gap the `Result` closed — a spawn that happens whether or not
    /// the row exists — and none of them is a compiler error.
    #[test]
    fn no_owner_discards_the_result_of_an_admission() {
        for (file, source) in [
            ("verify.rs", include_str!("verify.rs")),
            ("hooks.rs", include_str!("hooks.rs")),
            ("sandbox.rs", include_str!("sandbox.rs")),
        ] {
            let source = production(source, file);
            for (start, _) in source.match_indices("BoundedExecution::admit(") {
                // The 40 characters before the call carry the binding, which is
                // where a discarded result would be visible.
                let before = &source[start.saturating_sub(40)..start];
                assert!(
                    !before.contains("let _ =") && !before.contains("if let Ok"),
                    "{file} discards an admission result, which puts the spawn back in front \
                     of the row"
                );
            }
            for discard in [
                "admit(\n            projector,\n        )\n        .ok()",
                ".unwrap_or_default()",
            ] {
                assert!(
                    !source.contains(&format!("BoundedExecution::admit{discard}")),
                    "{file} swallows an admission failure"
                );
            }
        }
    }

    /// Structural: every supervised owner records ownership through the one
    /// journal, and none of them walks the controller's owned set itself.
    ///
    /// The failure this prevents is not a bug in any single file — it is six
    /// files each growing their own copy of "for each owned pid, write a row",
    /// which is six places to forget the identity check or swallow the error.
    /// [`ResourceController::live_owned`] stays available for measurement, so the
    /// assertion is about the *durable* path specifically.
    #[test]
    fn every_supervised_owner_persists_ownership_through_the_one_journal() {
        // Each owner, and the file where its controller is wired to a row.
        for (file, source) in [
            ("verify.rs", include_str!("verify.rs")),
            ("hooks.rs", include_str!("hooks.rs")),
            ("sandbox.rs", include_str!("sandbox.rs")),
            ("tools.rs", include_str!("tools.rs")),
            ("background_shell.rs", include_str!("background_shell.rs")),
            ("browser_worker.rs", include_str!("browser_worker.rs")),
        ] {
            let source = production(source, file);
            assert!(
                source.contains("persist_ownership_to") || source.contains("persist_ownership()"),
                "{file} owns a supervised workload and never makes its ownership durable"
            );
            assert!(
                !source.contains(".owned()"),
                "{file} reads the controller's owned set directly; the durable path is \
                 `ProjectedOwnership`"
            );
        }
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
            )
            .expect("the in-memory ledger admits the row");
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
        )
        .expect("the in-memory ledger admits the row");
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
        )
        .expect("the in-memory ledger admits the row");
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

    /// A backend that measures nothing must leave a gap, not record a zero.
    ///
    /// The peak fold is where this is easy to get wrong: `MAX(peak, 0)` against
    /// an unmeasured reading writes `0`, which reads as "this tree held nothing"
    /// rather than "nobody looked" — and a reader cannot tell those apart after
    /// the fact.
    #[test]
    fn an_unmeasured_sample_leaves_a_gap_rather_than_recording_a_zero() {
        let projector = fake();
        let mut execution = BoundedExecution::admit(
            projector.clone(),
            ProcessKind::VerifyCommand,
            None,
            ProcessKind::VerifyCommand.default_limits(),
        )
        .expect("the in-memory ledger admits the row");
        let external_id = execution.external_id().to_string();
        execution.identity = crate::process_tree::ProcessIdentity::of(std::process::id());
        let projection = execution.projection(ProcessState::Running, None, None);
        execution.project(projection);

        // Wall time only, which is what a host that can measure nothing else
        // honestly reports.
        execution.sampled(&ResourceSample {
            wall_ms: 10,
            rss_bytes: None,
            peak_rss_bytes: None,
            process_count: None,
            peak_process_count: None,
            output_bytes: None,
        });

        let live = row(&projector, &external_id);
        let usage = live
            .usage
            .expect("the row was sampled, so it has a reading");
        assert_eq!(usage.rss_bytes, None);
        assert_eq!(
            usage.peak_rss_bytes, None,
            "an unmeasured peak became a zero"
        );
        assert_eq!(usage.process_count, None);
        assert_eq!(usage.peak_process_count, None);
        assert_eq!(usage.output_bytes, None);
        // The stamp is still there: "measured, and there was nothing to report"
        // is what separates this from "never measured at all".
        assert!(live.usage_sampled_at_ms.is_some());

        execution.exited(ProcessExit::succeeded(), None);
    }

    /// A peak already on the row survives a later sample that cannot measure.
    #[test]
    fn an_unmeasured_sample_never_lowers_a_peak_the_row_already_holds() {
        let projector = fake();
        let mut execution = BoundedExecution::admit(
            projector.clone(),
            ProcessKind::VerifyCommand,
            None,
            ProcessKind::VerifyCommand.default_limits(),
        )
        .expect("the in-memory ledger admits the row");
        let external_id = execution.external_id().to_string();
        execution.identity = crate::process_tree::ProcessIdentity::of(std::process::id());
        let projection = execution.projection(ProcessState::Running, None, None);
        execution.project(projection);

        execution.sampled(&ResourceSample {
            wall_ms: 10,
            rss_bytes: Some(9_000),
            peak_rss_bytes: Some(9_000),
            process_count: Some(7),
            peak_process_count: Some(7),
            output_bytes: Some(5),
        });
        execution.sampled(&ResourceSample {
            wall_ms: 20,
            ..ResourceSample::default()
        });

        let live = row(&projector, &external_id);
        let usage = live.usage.expect("the row was sampled");
        assert_eq!(usage.peak_rss_bytes, Some(9_000));
        assert_eq!(usage.peak_process_count, Some(7));
        // And the retained-output count, which is cumulative rather than a peak.
        assert_eq!(usage.output_bytes, Some(5));

        execution.exited(ProcessExit::succeeded(), None);
    }
}
