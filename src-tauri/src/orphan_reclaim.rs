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
    still_the_recorded_process, ExitStatus, OwnedMember, ProcessExit, ProcessFilter, ProcessKind,
    ProcessRecord, ProcessTable,
};
use crate::process_tree::ProcessIdentity;
use crate::resource_control::ContainmentScope;

/// What a previous session durably recorded about one workload's native
/// processes.
///
/// # Why the reclaim takes this rather than reading it itself
///
/// [`reclaim`] is pure of the database on purpose — the decision table is worth
/// testing against constructed rows rather than against a machine that happens to
/// have a cgroup — so the ownership the reclaim needs arrives as a value. The
/// caller reads it; the decision uses it.
///
/// # `Unreadable`, not an empty set
///
/// A workload whose ownership metadata will not parse is the case a `Vec::new()`
/// would quietly turn into "it owned nothing" — a confident `ConfirmedGone` about
/// processes nobody looked for. It gets its own variant so the only verdict it
/// can reach is [`Reclaim::ContainmentLost`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedOwnership {
    /// The members this workload was observed to own, as they were stored.
    Recorded(Vec<OwnedMember>),
    /// The stored ownership could not be read or validated.
    Unreadable { reason: String },
}

impl RecordedOwnership {
    /// No durable members — which is what a pre-V24 row honestly has, and is not
    /// the same as a row whose members could not be read.
    #[must_use]
    pub fn none() -> Self {
        RecordedOwnership::Recorded(Vec::new())
    }
}

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
pub fn reclaim(record: &ProcessRecord, ownership: &RecordedOwnership) -> Reclaim {
    // The durable half of the answer, before anything else is asked: a workload
    // whose recorded ownership cannot be validated has no safe verdict but
    // uncertainty, whatever its containment says.
    let sticky = match surviving_recorded_members(record, ownership) {
        Ok(sticky) => sticky,
        Err(reason) => return Reclaim::ContainmentLost { reason },
    };

    // A row with no pid never owned an OS process — a WebView kind, a workflow
    // node. There is nothing to reclaim and nothing uncertain about it, unless a
    // supervisor recorded members against it, which only an attached workload
    // can have done.
    let Some(pid) = record.native_pid.and_then(|pid| u32::try_from(pid).ok()) else {
        return match sticky.is_empty() {
            true => Reclaim::ConfirmedGone,
            false => reclaim_supervised(record, None, &sticky),
        };
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
                // The kernel scope is the primary truth here and stays so — it
                // cannot be escaped by an unprivileged descendant, which is why a
                // cgroup-backed workload never needed the member journal. The
                // journal is still *checked* before absence is claimed, because
                // "every persisted identity was looked at" must be true of every
                // arm rather than of most of them.
                ScopeReclaim::Absent | ScopeReclaim::Empty => {
                    reclaim_recorded_survivors(record, &sticky)
                }
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
            reclaim_recorded_survivors(record, &sticky)
        }
        Some(ContainmentScope::ProcessGroup(pgid)) => {
            reclaim_supervised(record, Some(pgid), &sticky)
        }
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
            reclaim_supervised(record, Some(pid), &sticky)
        }
    }
}

/// The recorded members that could still be executing, or why none of them can be
/// judged.
///
/// # The boot marker is the whole of the safety argument on Linux
///
/// A `(pid, start_time)` pair is unambiguous on macOS and Windows because both
/// start times are absolute. Linux counts clock ticks since boot, so the next
/// boot can reissue a pair this app recorded before it — and what a reclaim does
/// with a match is `SIGKILL`. A row that names a different boot than the one
/// running is therefore proof its processes are gone, not a licence to signal
/// their pids; and a row that names a boot this host cannot identify at all is
/// the uncertain case, because "cannot be validated" and "is absent" are the two
/// answers this module exists to keep apart.
fn surviving_recorded_members(
    record: &ProcessRecord,
    ownership: &RecordedOwnership,
) -> Result<Vec<ProcessIdentity>, String> {
    let members = match ownership {
        RecordedOwnership::Unreadable { reason } => {
            return Err(format!(
                "this workload's recorded native ownership could not be read, so nothing can \
                 prove whether the processes it owned are still running: {reason}"
            ))
        }
        RecordedOwnership::Recorded(members) => members,
    };
    if members.is_empty() {
        return Ok(Vec::new());
    }
    match (
        record.native_boot_marker.as_deref(),
        crate::process_tree::boot_marker(),
    ) {
        // Absolute start times: the pair identifies the process on its own, which
        // is also the pre-V24 row's case.
        (None, _) => {}
        (Some(stored), Some(current)) if stored == current => {}
        // A different boot. Every recorded process died with the machine, and the
        // pids they held may now be anybody's — so they are absent, and none of
        // them is signalled.
        (Some(_), Some(_)) => return Ok(Vec::new()),
        (Some(stored), None) => {
            return Err(format!(
                "this workload's native identities were recorded against host boot {stored}, \
                 which this host cannot identify, so none of them can be safely matched"
            ))
        }
    }
    Ok(members
        .iter()
        .map(|member| member.identity)
        // The identity check, not a liveness check: a pid the kernel has since
        // handed to an unrelated process has a different start time and is not
        // ours. This is the one guard between reclaiming a stale workload and
        // killing the user's editor.
        .filter(ProcessIdentity::is_running)
        .collect())
}

/// The answer for an arm whose own containment reported absence: still gone, or
/// gone except for what the journal remembers.
fn reclaim_recorded_survivors(record: &ProcessRecord, sticky: &[ProcessIdentity]) -> Reclaim {
    match sticky.is_empty() {
        true => Reclaim::ConfirmedGone,
        false => reclaim_supervised(record, None, sticky),
    }
}

/// Reclaim a supervised workload from every handle a previous session left.
///
/// # The union, and why each arm is in it
///
/// - **The persisted owned members.** The only thing that can name a descendant
///   which was observed and *then* called `setsid`, changed group and re-parented.
///   Nothing derived from the live process table can attribute it any more.
/// - **The recorded process group.** The cheap, ordinary case: descendants that
///   simply stayed where they were put, reachable while their leader is.
/// - **The recorded session.** A descendant that called `setpgid` left the group
///   but not the session, so the group arm alone would report it absent.
/// - **The root's own identity.** The workload's leader, when it outlived the app.
///
/// An empty process group is therefore *not* sufficient evidence of absence,
/// which is the exact bug this closes: the old version asked the group, found
/// nobody, and closed the row as confidently dead while the escaped descendant
/// went on running.
///
/// # Nothing is signalled that cannot be proven ours
///
/// Two classes of evidence reach this function and they are **not** treated the
/// same, because only one of them proves ownership:
///
/// - A **persisted member** carries its own recorded start time, so re-checking
///   it proves the pid still names the process this workload started. Those are
///   signalled.
/// - A **group or session member** is only as good as the id it was found
///   through, and both ids are pids: a process group is named by its leader's
///   pid and a session by its leader's. Once that leader has been reaped the
///   kernel may hand the number to somebody else, who becomes a leader of their
///   own — at which point the recorded id names a stranger's group. So the group
///   and session are enumerated only as *targets* while the recorded root is
///   still the recorded process; after that they are read as evidence that
///   something may survive, and reported as [`Reclaim::ContainmentLost`] rather
///   than signalled.
///
/// That is a real narrowing of the previous behaviour, which signalled group
/// members by identity after the leader was gone. What made it safe to narrow is
/// the journal: a descendant this app ever observed is now in it, so the case the
/// old sweep was reaching for is covered by the arm that can prove ownership —
/// and the case that is left is one where nothing can. A missed stale process is
/// recoverable; killing an unrelated one is not.
fn reclaim_supervised(
    record: &ProcessRecord,
    pgid: Option<u32>,
    sticky: &[ProcessIdentity],
) -> Reclaim {
    let root_alive = still_the_recorded_process(record);
    let mut targets = std::collections::BTreeMap::<u32, ProcessIdentity>::new();
    for member in sticky {
        targets.insert(member.pid, *member);
    }
    let discover = || -> Vec<u32> {
        pgid.map(crate::process_tree::process_group_members)
            .unwrap_or_default()
            .into_iter()
            .chain(
                record
                    .supervised_session_id
                    .map(crate::process_tree::session_members)
                    .unwrap_or_default(),
            )
            .collect()
    };
    // Found through an id this app recorded, but not provable as this workload's
    // once the leader that named the id is gone.
    let mut unproven: Vec<u32> = Vec::new();
    for pid in discover() {
        let Some(identity) = ProcessIdentity::of(pid) else {
            continue;
        };
        if !identity.is_running() {
            continue;
        }
        if root_alive || targets.contains_key(&pid) {
            targets.insert(pid, identity);
        } else {
            unproven.push(pid);
        }
    }

    if targets.is_empty() && unproven.is_empty() && !root_alive {
        return Reclaim::ConfirmedGone;
    }
    let found = targets.len().max(usize::from(root_alive));

    // The group first, and only through the leader's own identity — the same
    // rule the classification above turns on.
    if root_alive {
        if let Some(pgid) = pgid {
            let _ = crate::os_signal::terminate_process_group(pgid);
        }
    }
    // Then every proven target, whether it stayed in the group or left it a
    // whole process-lifetime ago in supervisor terms.
    for identity in targets.values() {
        crate::os_signal::kill_by_identity(*identity);
    }

    let live_unproven = |unproven: &[u32]| -> Vec<u32> {
        unproven
            .iter()
            .copied()
            .filter(|pid| ProcessIdentity::of(*pid).is_some_and(|identity| identity.is_running()))
            .collect()
    };
    for _ in 0..10 {
        let surviving = targets
            .values()
            .filter(|identity| identity.is_running())
            .count();
        // Only meaningful where the group was provably ours; where it was not,
        // its membership is in `unproven` and is answered below.
        let group_settled = !root_alive
            || pgid
                .map(|pgid| crate::process_tree::process_group_members(pgid).is_empty())
                .unwrap_or(true);
        if surviving == 0 && group_settled && !still_the_recorded_process(record) {
            let leftover = live_unproven(&unproven);
            if leftover.is_empty() {
                return Reclaim::Reclaimed { ended: found };
            }
            return Reclaim::ContainmentLost {
                reason: format!(
                    "{} process(es) remain in this workload's recorded process group or \
                     session, and its root is gone — so nothing can prove they are this \
                     workload's rather than a reuse of the same id, and none of them was \
                     signalled: {leftover:?}",
                    leftover.len()
                ),
            };
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let surviving: Vec<u32> = targets
        .values()
        .filter(|identity| identity.is_running())
        .map(|identity| identity.pid)
        .collect();
    Reclaim::ContainmentLost {
        reason: match surviving.is_empty() {
            // Nothing this app can name is still executing, but the group or the
            // root has not settled — which is not the same as having reclaimed it.
            true => format!(
                "this workload's containment did not empty after the startup reclaim, so work \
                 this session did not start may still be running{}",
                pgid.map(|pgid| format!(" (process group {pgid})"))
                    .unwrap_or_default()
            ),
            false => format!(
                "{} process(es) this workload owns survived the startup reclaim, so it may \
                 still be running: {surviving:?}",
                surviving.len()
            ),
        },
    }
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
        // Read before the reclaim and never after it: the reclaim is what erases
        // the processes these identities name, so a read that came second would
        // be reading its own work. A read that fails is `Unreadable` rather than
        // an empty set, which is what keeps "nothing was recorded" and "the
        // recording could not be read" from collapsing into one verdict.
        let ownership = match table.owned_members(&record.process_id) {
            Ok(members) => RecordedOwnership::Recorded(members),
            Err(error) => RecordedOwnership::Unreadable {
                reason: error.to_string(),
            },
        };
        let verdict = reclaim(&record, &ownership);
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
            supervised_session_id: None,
            native_boot_marker: None,
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
        let verdict = reclaim(&row(Some(4242), None), &RecordedOwnership::none());
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
        assert_eq!(
            reclaim(&row(None, None), &RecordedOwnership::none()),
            Reclaim::ConfirmedGone
        );
        assert_eq!(
            reclaim(&row(None, None), &RecordedOwnership::none())
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
        assert_eq!(
            reclaim(&record, &RecordedOwnership::none()),
            Reclaim::ConfirmedGone
        );
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

        let verdict = reclaim(&record, &RecordedOwnership::none());
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
        let _ = reclaim(&record, &RecordedOwnership::none());
        assert!(crate::os_signal::process_is_alive(pid));
    }

    // --- Durable supervised ownership -----------------------------------------

    /// A child that stays alive long enough to be a signal target.
    ///
    /// The boot-marker tests assert that *nothing was signalled*, and a real
    /// child is the honest subject for that: the test process is not a legitimate
    /// target on any platform, and asking the host whether it is looking at
    /// itself turned out to be unreliable on Windows CI — which made the
    /// assertion about the probe rather than about the reclaim.
    ///
    /// `cmd` has no `sleep`, so Windows gets the same ping stand-in the verify
    /// tests use.
    fn sleeping_child() -> std::process::Child {
        #[cfg(target_os = "windows")]
        let mut command = {
            let mut command = std::process::Command::new("cmd");
            command.args(["/C", "ping -n 31 127.0.0.1"]);
            command
        };
        #[cfg(not(target_os = "windows"))]
        let mut command = {
            let mut command = std::process::Command::new("sleep");
            command.arg("30");
            command
        };
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the child starts")
    }

    /// One recorded member, as a previous session's journal would hold it.
    ///
    /// Not `cfg(unix)`, unlike the process-spawning helper below: the boot-marker
    /// tests are decided by `target_os` rather than by family, so this is reached
    /// on Windows too — and gating it there is how it first failed to compile on
    /// the one platform that cannot be built for locally.
    fn owned(identity: crate::process_tree::ProcessIdentity) -> OwnedMember {
        OwnedMember {
            identity,
            first_seen_at_ms: 1,
            last_seen_at_ms: 2,
        }
    }

    /// The state a crash leaves behind after a descendant has escaped: a dead
    /// root whose recorded process group is empty, and a live process that is in
    /// no group, session or ancestry the row can name.
    ///
    /// Staged from two spawns rather than from `setsid` inside a shell, and that
    /// is deliberate. macOS ships no `setsid(1)`, so a shell-based stager would
    /// silently degrade there into "the child stayed in the group" — which the
    /// *group* arm reclaims, and the test would pass while proving nothing about
    /// the journal. Two process groups model the finished state of the escape
    /// exactly, on every Unix, with no timing to lose.
    #[cfg(unix)]
    fn escaped_child() -> (u32, std::process::Child) {
        use std::os::unix::process::CommandExt;

        // The root: leads its own group, and is gone before the reclaim looks.
        let mut root = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the root starts");
        let root_pid = root.id();
        let _ = root.wait();

        // The descendant, after its escape: alive, in a group of its own, with no
        // link left to the root. Only a durable record can attribute it.
        let escaped = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("the escaped descendant starts");
        (root_pid, escaped)
    }

    /// The regression this whole change exists for.
    ///
    /// A descendant is observed by the supervisor, durably recorded, and *then*
    /// escapes: new session, new process group, re-parented, with the root gone.
    /// The app crashes. A fresh session finds an empty process group and a dead
    /// root — and must still reclaim the descendant, because the ownership was
    /// written down while it was still attributable.
    ///
    /// Against the previous implementation this returns `ConfirmedGone` with the
    /// descendant still running, which is the exact untruth the journal removes.
    #[test]
    #[cfg(unix)]
    fn a_recorded_member_that_escaped_its_group_is_reclaimed_after_a_restart() {
        let (root, mut escaped) = escaped_child();
        let escaped_identity = crate::process_tree::ProcessIdentity::of(escaped.id())
            .expect("the escaped child has an identity");
        assert!(escaped_identity.is_running());

        // The row a previous session left: its root is gone, and the process
        // group it recorded has nobody in it.
        let record = with_scope(
            row(Some(i64::from(root)), Some(1)),
            &format!("pgroup:{root}"),
        );
        assert!(
            crate::process_tree::process_group_members(root).is_empty(),
            "the recorded process group still has members, so this test is not exercising the \
             escape it is about"
        );

        // The control, and the reason this test would be red against the previous
        // implementation: with nothing recorded, every discovery primitive is
        // empty and the reclaim can only conclude absence — while the descendant
        // is demonstrably still running. That verdict is the bug.
        assert_eq!(
            reclaim(&record, &RecordedOwnership::none()),
            Reclaim::ConfirmedGone
        );
        assert!(
            escaped_identity.is_running(),
            "the control must leave the descendant alive, or it proves nothing"
        );

        // What the journal wrote down while the descendant was still ours.
        let ownership = RecordedOwnership::Recorded(vec![owned(escaped_identity)]);
        let verdict = reclaim(&record, &ownership);

        assert!(
            matches!(verdict, Reclaim::Reclaimed { .. }),
            "a durably recorded member that outlived its group must be reclaimed: {verdict:?}"
        );
        assert!(
            !escaped_identity.is_running(),
            "the escaped descendant is still running after the reclaim reported success"
        );
        let _ = escaped.wait();
    }

    /// The same finding stated as the rule it protects: an empty process group is
    /// not evidence of absence while the journal names something alive.
    #[test]
    #[cfg(unix)]
    fn an_empty_process_group_is_not_confirmed_gone_while_a_recorded_member_lives() {
        let (root, mut escaped) = escaped_child();
        let escaped_identity = crate::process_tree::ProcessIdentity::of(escaped.id())
            .expect("the escaped child has an identity");

        let record = with_scope(
            row(Some(i64::from(root)), Some(1)),
            &format!("pgroup:{root}"),
        );
        let verdict = reclaim(
            &record,
            &RecordedOwnership::Recorded(vec![owned(escaped_identity)]),
        );

        assert_ne!(
            verdict,
            Reclaim::ConfirmedGone,
            "a live recorded member makes `confirmed gone` a false statement"
        );
        // And with the member reclaimed, the row may close as `lost` — which is
        // only allowed *because* the absence was established.
        assert_eq!(
            verdict.into_exit("the app restarted").status,
            ExitStatus::Lost
        );
        let _ = escaped.wait();
    }

    /// Several escaped members, each reclaimed — not just the first one found.
    #[test]
    #[cfg(unix)]
    fn every_recorded_member_is_reclaimed_rather_than_the_first() {
        let mut children = Vec::new();
        let mut identities = Vec::new();
        let mut roots = Vec::new();
        for _ in 0..3 {
            let (root, escaped) = escaped_child();
            identities.push(
                crate::process_tree::ProcessIdentity::of(escaped.id())
                    .expect("the escaped child has an identity"),
            );
            roots.push(root);
            children.push(escaped);
        }

        // One row, owning all three: the shape of a workload whose descendants
        // scattered before the crash.
        let record = with_scope(
            row(Some(i64::from(roots[0])), Some(1)),
            &format!("pgroup:{}", roots[0]),
        );
        let verdict = reclaim(
            &record,
            &RecordedOwnership::Recorded(identities.iter().copied().map(owned).collect()),
        );

        assert!(
            matches!(verdict, Reclaim::Reclaimed { ended } if ended >= 3),
            "every recorded member has to be reclaimed, not the first one: {verdict:?}"
        );
        for identity in &identities {
            assert!(
                !identity.is_running(),
                "process {} survived a reclaim that reported success",
                identity.pid
            );
        }
        for mut child in children {
            let _ = child.wait();
        }
    }

    /// PID reuse, which is the failure a member journal would otherwise invite.
    ///
    /// The recorded identity names this test process's pid with a start time
    /// that is not its own — which is precisely what a reused pid looks like from
    /// a row's point of view. The member must be read as absent, and nothing may
    /// be signalled: a reclaim that matched on the number alone would kill the
    /// test runner, and in production the user's editor.
    #[test]
    #[cfg(unix)]
    fn a_recorded_member_whose_start_time_no_longer_matches_is_absent_not_ours() {
        let pid = std::process::id();
        let stale = crate::process_tree::ProcessIdentity { pid, start_time: 1 };
        assert!(
            !stale.is_running(),
            "this test needs a start time that does not match the live process"
        );

        let record = row(Some(4_242_424), Some(1));
        let verdict = reclaim(&record, &RecordedOwnership::Recorded(vec![owned(stale)]));

        assert_eq!(
            verdict,
            Reclaim::ConfirmedGone,
            "a member whose identity no longer matches is gone, not a target"
        );
        assert!(
            crate::os_signal::process_is_alive(pid),
            "the reclaim signalled a pid it could not prove was ours"
        );
    }

    /// A process group whose leader has been reaped names nothing this app can
    /// prove, so its members are reported rather than signalled.
    ///
    /// # Why this is narrower than what it replaces
    ///
    /// The previous version enumerated the recorded group after the leader was
    /// gone and killed each member by identity. Reading the identity from the
    /// live table makes the *signal* precise but says nothing about *ownership*:
    /// a process-group id is its leader's pid, so once that pid is free, any
    /// process that takes it and calls `setpgid` becomes the leader of a group
    /// this row still names. Killing its members is then killing a stranger's
    /// tree, which is the one outcome worth being conservative about.
    ///
    /// The journal is what made narrowing this affordable: a descendant this app
    /// ever observed is now recorded with its own start time and is reclaimed
    /// through the arm that can prove ownership. What is left here is a process
    /// nothing ever attributed, and `containment_lost` is the honest answer for
    /// it.
    #[test]
    #[cfg(unix)]
    fn members_of_a_recorded_group_whose_leader_is_gone_are_reported_not_killed() {
        let (root, mut escaped) = escaped_child();
        let escaped_identity = crate::process_tree::ProcessIdentity::of(escaped.id())
            .expect("the escaped child has an identity");
        // The row names *the escaped child's own* group, which it leads — the
        // shape a reissued process-group id produces, and the reason the leader
        // check is the one that matters rather than the identity read.
        let mut record = with_scope(
            row(Some(i64::from(root)), Some(1)),
            &format!("pgroup:{}", escaped.id()),
        );
        record.native_boot_marker = None;

        let verdict = reclaim(&record, &RecordedOwnership::none());

        assert!(
            matches!(verdict, Reclaim::ContainmentLost { .. }),
            "a group this app cannot prove is its own must be reported, not swept: {verdict:?}"
        );
        assert!(
            escaped_identity.is_running(),
            "a process found only through a reissuable id was signalled"
        );

        let _ = escaped.kill();
        let _ = escaped.wait();
    }

    /// Ownership that cannot be read is uncertainty, never absence.
    ///
    /// The `Vec::new()` this replaces would have made an unreadable journal
    /// indistinguishable from a workload that owned nothing — a confident `lost`
    /// about processes nobody looked for.
    #[test]
    fn ownership_that_cannot_be_read_is_containment_lost_rather_than_gone() {
        let verdict = reclaim(
            &row(Some(4_242_424), Some(1)),
            &RecordedOwnership::Unreadable {
                reason: "the owned-member rows would not parse".to_string(),
            },
        );
        assert!(
            matches!(verdict, Reclaim::ContainmentLost { .. }),
            "{verdict:?}"
        );
        assert_eq!(
            verdict.into_exit("the app restarted").status,
            ExitStatus::ContainmentLost
        );
    }

    /// A row from another boot is proof its processes are gone — and is never a
    /// licence to signal the pids they held.
    ///
    /// Only meaningful where start times are boot-relative, which is Linux; the
    /// other platforms record no marker because their pairs are already
    /// unambiguous.
    #[test]
    #[cfg(target_os = "linux")]
    fn identities_recorded_against_another_boot_are_absent_and_never_signalled() {
        let mut child = sleeping_child();
        let pid = child.id();
        let live = crate::process_tree::ProcessIdentity::of(pid).expect("the child exists");
        let mut record = row(Some(i64::from(pid)), Some(1));
        record.native_boot_marker = Some("linux-btime:1".to_string());

        let verdict = reclaim(&record, &RecordedOwnership::Recorded(vec![owned(live)]));

        assert_eq!(
            verdict,
            Reclaim::ConfirmedGone,
            "identities from a previous boot cannot still exist"
        );
        assert!(
            live.is_running(),
            "a member from another boot was signalled on its pid alone"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    /// A row whose boot cannot be identified at all is the uncertain case.
    #[test]
    #[cfg(not(target_os = "linux"))]
    fn identities_recorded_against_an_unidentifiable_boot_are_uncertain() {
        let mut child = sleeping_child();
        let pid = child.id();
        let live = crate::process_tree::ProcessIdentity::of(pid).expect("the child exists");
        let mut record = row(Some(i64::from(pid)), Some(1));
        record.native_boot_marker = Some("linux-btime:1".to_string());

        let verdict = reclaim(&record, &RecordedOwnership::Recorded(vec![owned(live)]));

        assert!(
            matches!(verdict, Reclaim::ContainmentLost { .. }),
            "a host that cannot identify the recorded boot cannot match its identities: \
             {verdict:?}"
        );
        assert!(
            live.is_running(),
            "a member whose boot could not be identified was signalled anyway"
        );
        let _ = child.kill();
        let _ = child.wait();
    }
}
