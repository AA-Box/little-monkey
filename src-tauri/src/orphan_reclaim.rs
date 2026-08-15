//! What a new app session may conclude about work an old one left behind.
//!
//! # The rule this module exists to hold
//!
//! Startup reconciliation used to do two things in sequence: try to kill a
//! process group, and then close every row it had looked at as
//! [`ExitStatus::Lost`] regardless of how that went. `Lost` asserts a fact — the
//! worker went away — and the sequence asserted it whether or not anything had
//! been established. A descendant that had left the group, a cgroup whose members
//! would not die, a row too old to carry an identity: all three closed as
//! confidently dead, and the machine kept running them.
//!
//! So a reclaim now produces a *verdict* ([`Reclaim`]) rather than a side effect,
//! and only two of its three values are allowed to become `Lost`. The third is
//! [`ExitStatus::ContainmentLost`], which says the honest thing: this app stopped
//! tracking a workload it could not prove had ended.
//!
//! # Evidence, per backend
//!
//! The evidence available differs by what held the workload, which is exactly why
//! the row records that ([`Containment`]) rather than leaving a restart to guess:
//!
//! - **cgroup v2.** The strongest case, and the one where doing nothing was worst:
//!   the kernel keeps enforcing the scope after this app dies, so the tree is
//!   still running, still bounded, and unwatched. The scope is named on the row,
//!   validated on the way back in, killed, and read back until it is empty.
//! - **A process group.** The root's identity is checked before anything is
//!   signalled, and the group's live membership is enumerated from the host
//!   process table rather than assumed from the pid.
//! - **A Windows job.** `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` means the kernel
//!   ended the tree when the handle-holding process died, so absence is the
//!   expected finding — but it is *checked* rather than assumed, because a
//!   process still alive at the recorded identity would mean the job did not do
//!   what the row claims it did.
//!
//! # What this deliberately does not do
//!
//! It never signals a pid it cannot tie to a recorded start time, and never
//! writes `cgroup.kill` into a path it did not validate. Failing to reclaim one
//! stale process is recoverable; killing an unrelated one is not.

use crate::process_table::{
    still_the_recorded_process, ExitStatus, ProcessExit, ProcessFilter, ProcessKind, ProcessRecord,
    ProcessTable,
};
use crate::resource_control::ContainmentScope;

/// What a restart could establish about one row's workload.
#[derive(Debug, PartialEq, Eq)]
pub enum Reclaim {
    /// Nothing owned by this row is executing, and that was checked rather than
    /// assumed.
    ConfirmedGone,
    /// Work was found and ended, and its absence verified afterwards.
    Reclaimed { ended: usize },
    /// Work may still be executing and nothing available here can show
    /// otherwise. The reason is what a person reads on the row.
    ContainmentLost { reason: String },
}

impl Reclaim {
    /// The exit this verdict earns.
    ///
    /// The whole point of the type: only a checked absence may be recorded as
    /// `Lost`, and uncertainty gets a status of its own rather than borrowing
    /// one that means something stronger.
    #[must_use]
    pub fn into_exit(self, reaped_reason: &str) -> ProcessExit {
        match self {
            Reclaim::ConfirmedGone => ProcessExit {
                status: ExitStatus::Lost,
                code: None,
                signal: None,
                reason: Some(reaped_reason.to_string()),
                breach: None,
            },
            Reclaim::Reclaimed { ended } => ProcessExit {
                status: ExitStatus::Lost,
                code: None,
                signal: None,
                reason: Some(format!(
                    "{reaped_reason}; {ended} process(es) it still owned were reclaimed at startup"
                )),
                breach: None,
            },
            Reclaim::ContainmentLost { reason } => ProcessExit {
                status: ExitStatus::ContainmentLost,
                code: None,
                signal: None,
                reason: Some(reason),
                breach: None,
            },
        }
    }
}

/// The kinds whose rows name a native tree this module can reason about.
///
/// A browser session is excluded because it has a reclaim of its own that also
/// collects a profile directory; everything else here either owns no OS process
/// or is somebody else's to sweep.
const NATIVE_TREE_KINDS: &[ProcessKind] = &[
    ProcessKind::ForegroundShell,
    ProcessKind::BackgroundShell,
    ProcessKind::VerifyCommand,
    ProcessKind::HookCommand,
    ProcessKind::SandboxRun,
];

/// Decide what happened to one row's workload, and reclaim it where possible.
///
/// Pure of the database and of the clock, so the decision table can be tested
/// against constructed rows rather than against a machine that happens to have a
/// cgroup.
#[must_use]
pub fn reclaim(record: &ProcessRecord) -> Reclaim {
    // A row with no pid never owned an OS process — a WebView kind, a workflow
    // node. There is nothing to reclaim and nothing uncertain about it.
    let Some(pid) = record.native_pid.and_then(|pid| u32::try_from(pid).ok()) else {
        return Reclaim::ConfirmedGone;
    };

    let scope = record
        .containment
        .as_ref()
        .and_then(|containment| containment.parsed_scope());

    match scope {
        #[cfg(target_os = "linux")]
        Some(ContainmentScope::CgroupV2(path)) => {
            use crate::resource_control_cgroup::ScopeReclaim;
            match crate::resource_control_cgroup::reclaim_scope(&path) {
                ScopeReclaim::Absent | ScopeReclaim::Empty => Reclaim::ConfirmedGone,
                ScopeReclaim::Reclaimed(ended) => Reclaim::Reclaimed { ended },
                ScopeReclaim::Survivors(survivors) => Reclaim::ContainmentLost {
                    reason: format!(
                        "{} process(es) in this workload's cgroup scope at {} survived the \
                         startup reclaim, so it may still be running: {survivors:?}",
                        survivors.len(),
                        path.display()
                    ),
                },
            }
        }
        // A cgroup path on a host that is not Linux is a row written elsewhere.
        // Nothing here can inspect it, and saying so is the honest answer.
        #[cfg(not(target_os = "linux"))]
        Some(ContainmentScope::CgroupV2(path)) => Reclaim::ContainmentLost {
            reason: format!(
                "this row was written on a Linux host and names a cgroup scope at {}, which \
                 this host cannot inspect",
                path.display()
            ),
        },
        Some(ContainmentScope::WindowsJob) => {
            // The job carried `KILL_ON_JOB_CLOSE`, so the kernel ended the tree
            // when the process holding the handle died. Absence is the expected
            // finding and is checked: a process still alive at the recorded
            // identity would mean the job did not do what the row claims.
            if still_the_recorded_process(record) {
                return Reclaim::ContainmentLost {
                    reason: format!(
                        "process {pid} is still running under an identity this row recorded \
                         inside a job object that should have killed it when the previous \
                         session ended"
                    ),
                };
            }
            Reclaim::ConfirmedGone
        }
        Some(ContainmentScope::ProcessGroup(pgid)) => reclaim_process_group(record, pgid),
        None => {
            // A pid with no containment and no identity is the case the old code
            // closed as `lost` on no evidence at all: nothing can be checked,
            // because nothing can prove the pid still names this workload.
            if record.native_start_time.is_none() {
                return Reclaim::ContainmentLost {
                    reason: format!(
                        "this row recorded pid {pid} without a start time or a containment \
                         handle, so nothing can prove whether its work is still running"
                    ),
                };
            }
            // An identity but no recorded containment: a pre-V23 row. The root
            // is still checkable, which is weaker than a scope and is not
            // nothing.
            reclaim_process_group(record, pid)
        }
    }
}

/// Reclaim through a process group, and verify.
#[cfg(unix)]
fn reclaim_process_group(record: &ProcessRecord, pgid: u32) -> Reclaim {
    let members = crate::process_tree::process_group_members(pgid);
    let root_alive = still_the_recorded_process(record);
    if members.is_empty() && !root_alive {
        return Reclaim::ConfirmedGone;
    }
    let found = members.len().max(usize::from(root_alive));
    // Only through the leader's own identity: a process-group id *is* a pid, and
    // signalling one whose leader has been reaped can reach a group the kernel
    // has since handed to somebody else.
    if root_alive {
        let _ = crate::os_signal::terminate_process_group(pgid);
    } else {
        // The leader is gone but members remain, which is the ordinary shape of
        // an escaped descendant: signal each one by identity instead.
        for member in &members {
            if let Some(identity) = crate::process_tree::ProcessIdentity::of(*member) {
                if identity.is_running() {
                    crate::os_signal::kill_by_identity(identity);
                }
            }
        }
    }
    for _ in 0..10 {
        if crate::process_tree::process_group_members(pgid).is_empty()
            && !still_the_recorded_process(record)
        {
            return Reclaim::Reclaimed { ended: found };
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Reclaim::ContainmentLost {
        reason: format!(
            "process group {pgid} still has members after the startup reclaim, so work this \
             session did not start may still be running"
        ),
    }
}

/// Windows reaches here only for a row whose job could not be created, where the
/// fallback tree primitive was the parent-link closure — and that closure died
/// with the process that held it.
#[cfg(not(unix))]
fn reclaim_process_group(record: &ProcessRecord, pgid: u32) -> Reclaim {
    if still_the_recorded_process(record) {
        let _ = crate::os_signal::terminate_process_group(pgid);
        for _ in 0..10 {
            if !still_the_recorded_process(record) {
                return Reclaim::Reclaimed { ended: 1 };
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        return Reclaim::ContainmentLost {
            reason: format!(
                "process {pgid} is still running under the identity this row recorded and did \
                 not respond to the startup reclaim"
            ),
        };
    }
    Reclaim::ConfirmedGone
}

/// Close out every native-tree row a previous session left live, each with the
/// verdict its own evidence earns.
///
/// Returns the rows that could **not** be shown to be gone, so a caller can say
/// so rather than logging a count that hides them.
pub fn reclaim_orphaned_native_trees(
    table: &ProcessTable<'_>,
    reason: &str,
    now_ms: i64,
) -> Result<Vec<ProcessRecord>, String> {
    let live = table
        .list(&ProcessFilter {
            kinds: NATIVE_TREE_KINDS.to_vec(),
            live_only: true,
            ..ProcessFilter::default()
        })
        .map_err(|error| error.to_string())?;

    let mut uncertain = Vec::new();
    for record in live {
        let verdict = reclaim(&record);
        let uncertain_row = matches!(verdict, Reclaim::ContainmentLost { .. });
        let exit = verdict.into_exit(reason);
        match table.transition(
            &record.process_id,
            crate::process_table::ProcessState::Exited,
            Some(exit),
            now_ms,
        ) {
            Ok(closed) => {
                if uncertain_row {
                    uncertain.push(closed);
                }
            }
            Err(error) => {
                eprintln!(
                    "process table: could not close {} after its startup reclaim: {error}",
                    record.process_id
                );
            }
        }
    }
    Ok(uncertain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_table::{ProcessLimits, ProcessState, SignalIntent};

    fn row(native_pid: Option<i64>, native_start_time: Option<i64>) -> ProcessRecord {
        ProcessRecord {
            process_id: "fgsh-orphan".to_string(),
            parent_process_id: None,
            kind: ProcessKind::ForegroundShell,
            external_id: "ext".to_string(),
            state: ProcessState::Running,
            run_id: None,
            workspace: None,
            profile: None,
            native_pid,
            native_start_time,
            limits: ProcessLimits::default(),
            containment: None,
            usage: None,
            usage_sampled_at_ms: None,
            signal_intent: SignalIntent::default(),
            signal_reason: None,
            signal_requested_at_ms: None,
            exit: None,
            created_at_ms: 1,
            updated_at_ms: 1,
            started_at_ms: Some(1),
            exited_at_ms: None,
        }
    }

    fn with_scope(mut record: ProcessRecord, scope: &str) -> ProcessRecord {
        record.containment = Some(crate::resource_control::Containment {
            backend: "test".to_string(),
            tree_primitive: "test".to_string(),
            scope: Some(scope.to_string()),
            enforcement: Default::default(),
        });
        record
    }

    /// The defect this module exists for: a row nothing can check must not be
    /// recorded as confidently dead.
    #[test]
    fn a_row_with_a_pid_and_no_identity_is_uncertain_rather_than_lost() {
        let verdict = reclaim(&row(Some(4242), None));
        assert!(
            matches!(verdict, Reclaim::ContainmentLost { .. }),
            "{verdict:?}"
        );
        let exit = verdict.into_exit("the app restarted");
        assert_eq!(exit.status, ExitStatus::ContainmentLost);
        assert!(exit
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("nothing can prove")));
    }

    /// A row that never owned a process is not uncertain about anything.
    #[test]
    fn a_row_with_no_pid_is_confirmed_gone() {
        assert_eq!(reclaim(&row(None, None)), Reclaim::ConfirmedGone);
        assert_eq!(
            reclaim(&row(None, None))
                .into_exit("the app restarted")
                .status,
            ExitStatus::Lost
        );
    }

    /// A stored scope is a string from a database, and what happens to a cgroup
    /// path is `cgroup.kill`. Anything this app could not have written must not
    /// parse, which is the one guard between a reclaim and a hostile row.
    #[test]
    fn a_scope_this_app_could_not_have_written_never_parses() {
        for hostile in [
            // The mount point itself, and a sibling directory whose name merely
            // starts the same way — the reason the prefix carries its separator.
            "cgroup2:/sys/fs/cgroup",
            "cgroup2:/sys/fs/cgrouped/little-monkey-x",
            "cgroup2:/sys/fs/cgroup/system.slice",
            "cgroup2:/sys/fs/cgroup/../../etc/little-monkey-x",
            "cgroup2:/etc/little-monkey-x",
            "cgroup2:little-monkey-relative",
            "pgroup:0",
            "pgroup:1",
            "pgroup:not-a-number",
            "some-other-scheme:/tmp",
        ] {
            assert!(
                ContainmentScope::parse(hostile).is_none(),
                "{hostile} was accepted"
            );
        }
        assert_eq!(
            ContainmentScope::parse("cgroup2:/sys/fs/cgroup/user.slice/little-monkey-abc"),
            Some(ContainmentScope::CgroupV2(
                "/sys/fs/cgroup/user.slice/little-monkey-abc".into()
            ))
        );
        assert_eq!(
            ContainmentScope::parse("pgroup:4242"),
            Some(ContainmentScope::ProcessGroup(4242))
        );
        assert_eq!(
            ContainmentScope::parse("windows-job"),
            Some(ContainmentScope::WindowsJob)
        );
    }

    /// A Windows job's kill-on-close means absence is expected — and it is still
    /// checked, because a process that outlived it would mean the row's claim was
    /// wrong.
    #[test]
    fn a_windows_job_row_whose_process_is_gone_is_confirmed_gone() {
        // A pid that cannot be running: the identity check fails on the start
        // time whatever the host does with the number.
        let record = with_scope(row(Some(4_242_424), Some(1)), "windows-job");
        assert_eq!(reclaim(&record), Reclaim::ConfirmedGone);
    }

    /// The whole restart story, against a real tree that a real crash left
    /// behind.
    ///
    /// # What "crash" means here
    ///
    /// `std::mem::forget` on the controller, which is the closest a test can get
    /// to the app being killed: no `Drop` runs, so the cgroup is not killed, the
    /// job handle is not closed, and nothing signals the process group. What is
    /// left is exactly what a new session finds — a running tree and a row.
    ///
    /// The workload is a shell with a backgrounded grandchild, because the
    /// grandchild is the process a pid-based reclaim misses: it is not the
    /// recorded pid, and after its parent is gone it is not reachable from one
    /// either. Only the containment handle finds it.
    ///
    /// Whichever backend this host provides is the one under test — a cgroup on a
    /// delegated Linux session, the process group everywhere else — because the
    /// production path chose it, which is the only way a test of a fallback is a
    /// test of anything.
    #[test]
    #[cfg(unix)]
    fn a_crashed_session_s_tree_is_reclaimed_through_its_recorded_containment() {
        use crate::resource_control::{
            EffectiveLimits, LimitLayer, LimitSource, ResourceController,
        };

        let marker = std::env::temp_dir().join(format!(
            "little_monkey_orphan_{}_{}.pid",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg(format!(
                "sleep 30 & echo $! > {}; sleep 30",
                marker.to_string_lossy()
            ))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let mut controller = ResourceController::new(EffectiveLimits::resolve(&[LimitLayer::new(
            LimitSource::ClassDefault,
            ProcessKind::ForegroundShell.default_limits(),
        )]));
        controller
            .prepare_std(&mut command)
            .expect("the containment is installable");
        let child = command.spawn().expect("the shell starts");
        controller
            .attach(child.id())
            .expect("the shell is inside its containment");

        // The row a previous session would have written.
        let mut record = row(
            Some(i64::from(child.id())),
            controller
                .root()
                .and_then(|identity| i64::try_from(identity.start_time).ok()),
        );
        record.containment = Some(controller.containment());

        // Wait for the grandchild to exist and report itself, because that is the
        // process the reclaim has to reach.
        let mut grandchild = None;
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(&marker) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    grandchild = Some(pid);
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let grandchild = grandchild.expect("the workload reported its backgrounded grandchild");
        assert!(crate::os_signal::process_is_alive(grandchild));

        // The crash: nothing runs, nothing is signalled, the containment stays as
        // the kernel left it.
        std::mem::forget(controller);
        std::mem::forget(child);

        let verdict = reclaim(&record);
        assert!(
            matches!(verdict, Reclaim::Reclaimed { .. }),
            "a running tree with a recorded containment must be reclaimed, not guessed at: \
             {verdict:?}"
        );
        // The exit is only allowed to be `lost` *because* the verdict established
        // the absence — which is the property the whole module is for.
        assert_eq!(
            verdict.into_exit("the app restarted").status,
            ExitStatus::Lost
        );

        for _ in 0..100 {
            if !crate::os_signal::process_is_alive(grandchild) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            !crate::os_signal::process_is_alive(grandchild),
            "the grandchild a pid-based reclaim would have missed is still running"
        );
        let _ = std::fs::remove_file(&marker);
    }

    /// This process is the one case a test can be certain about: it is running,
    /// its identity matches, and it leads no group of this app's making — so the
    /// verdict has to be one that does not signal it.
    #[test]
    #[cfg(unix)]
    fn a_reclaim_never_signals_a_process_whose_identity_does_not_match() {
        let pid = std::process::id();
        // A start time that is not this process's, which is what a reused pid
        // looks like from a row's point of view.
        let record = with_scope(row(Some(i64::from(pid)), Some(1)), &format!("pgroup:{pid}"));
        // The assertion is that this returns at all and that this process is
        // still here afterwards: a reclaim that signalled on a pid alone would
        // have killed the test runner.
        let _ = reclaim(&record);
        assert!(crate::os_signal::process_is_alive(pid));
    }
}
