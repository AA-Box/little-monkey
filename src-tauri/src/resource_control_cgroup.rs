//! Kernel-held limits on Linux, through a delegated cgroup v2 scope.
//!
//! The strongest production-safe mechanism a Linux host offers without root. A
//! cgroup's `memory.max` and `pids.max` are held by the kernel: the bound applies
//! to every process in the group including ones this app never sees, it survives
//! the supervisor dying, and `memory.current`/`pids.current` are the kernel's own
//! accounting rather than a sum this app computed.
//!
//! # Why delegation, and why not root
//!
//! Creating a cgroup means creating a directory under the cgroup2 mount. An
//! unprivileged process may do that only inside a subtree that has been
//! *delegated* to it, which on a systemd host is what `Delegate=yes` on
//! `user@<uid>.service` provides — the ordinary desktop case. Where that subtree
//! exists this module uses it. Where it does not, [`CgroupScope::create`] returns
//! `Ok(None)` and the caller falls back to the supervisor, which enforces the
//! same resources at a lower level and says so. Asking the user to run this app
//! as root to get a memory limit would trade a much larger authority for a
//! smaller one.
//!
//! # The "no internal processes" rule decides the layout
//!
//! A non-root cgroup may contain processes *or* enable controllers for its
//! children, never both. So a scope cannot simply be a child of whatever cgroup
//! this app happens to be in — that cgroup contains this app. The search walks
//! *upward* from our own cgroup looking for the nearest ancestor that will both
//! accept a new child directory and hand it the `memory` and `pids` controllers,
//! which on a delegated systemd session is the delegation root itself.
//!
//! # Nothing runs before the scope owns it
//!
//! `cgroup.procs` is opened before `fork` and written between `fork` and `exec`,
//! so the target program has already been migrated when it starts and every
//! process it goes on to create is created inside the scope. There is no window,
//! which is the property that separates this from attaching a supervisor after
//! the fact.

#![cfg(target_os = "linux")]

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::resource_control::{ControllerCapabilities, EffectiveLimits, LimitCapability};

/// Where cgroup2 is mounted on every mainstream distribution. Verified against
/// `/proc/self/cgroup` reporting a v2 (`0::`) line rather than assumed.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

pub struct CgroupScope {
    path: PathBuf,
    /// Held open from before the fork so `pre_exec` has nothing to do but write.
    /// Opening a file is not async-signal-safe; writing to an open descriptor is.
    procs_fd: OwnedFd,
    memory_enforced: bool,
    pids_enforced: bool,
    /// Why a controller is missing, when one is, so the capability answer names
    /// the specific thing this host did not delegate.
    missing_reason: Option<String>,
}

impl CgroupScope {
    /// Create a scope for these limits, or `Ok(None)` where the host offers no
    /// usable delegated hierarchy.
    ///
    /// `Ok(None)` rather than an error because "this host has no delegated
    /// cgroup" is a normal configuration, not a failure — a container without
    /// `cgroupns`, a non-systemd init, a distribution still on v1.
    pub fn create(limits: &EffectiveLimits) -> io::Result<Option<Self>> {
        // A scope with nothing to enforce is pure cost: a directory, a
        // controller write and a teardown, holding no bound.
        if limits.memory_bytes.is_none() && limits.child_processes.is_none() {
            return Ok(None);
        }
        let Some(own) = own_cgroup_path()? else {
            return Ok(None);
        };

        let name = format!("little-monkey-{}", uuid::Uuid::new_v4().simple());
        // Upward from our own cgroup: the nearest ancestor that accepts a child
        // *and* can hand it the controllers. Bounded rather than "until /" so a
        // surprising mount layout cannot walk out of the hierarchy.
        for candidate in own.ancestors().take(8) {
            if candidate == Path::new(CGROUP_ROOT) || !candidate.starts_with(CGROUP_ROOT) {
                break;
            }
            match Self::try_create_under(candidate, &name, limits) {
                Ok(Some(scope)) => return Ok(Some(scope)),
                Ok(None) => continue,
                Err(_) => continue,
            }
        }
        Ok(None)
    }

    fn try_create_under(
        parent: &Path,
        name: &str,
        limits: &EffectiveLimits,
    ) -> io::Result<Option<Self>> {
        let path = parent.join(name);
        if fs::create_dir(&path).is_err() {
            return Ok(None);
        }
        // From here on every early return must remove the directory, or a failed
        // attempt leaves an empty cgroup behind on every spawn.
        let scope = Self::configure(path.clone(), parent, limits);
        match scope {
            Ok(Some(scope)) => Ok(Some(scope)),
            other => {
                let _ = fs::remove_dir(&path);
                other
            }
        }
    }

    fn configure(
        path: PathBuf,
        parent: &Path,
        limits: &EffectiveLimits,
    ) -> io::Result<Option<Self>> {
        let mut available = read_controllers(&path.join("cgroup.controllers"));
        let wanted: Vec<&str> = ["memory", "pids"]
            .into_iter()
            .filter(|controller| !available.iter().any(|held| held == controller))
            .collect();
        if !wanted.is_empty() {
            // Delegating a controller downward is the parent's decision to
            // record, and it may legitimately refuse.
            let request: String = wanted
                .iter()
                .map(|controller| format!("+{controller} "))
                .collect();
            let _ = fs::write(parent.join("cgroup.subtree_control"), request.trim_end());
            available = read_controllers(&path.join("cgroup.controllers"));
        }
        let has = |controller: &str| available.iter().any(|held| held == controller);

        let mut missing = Vec::new();
        let mut memory_enforced = false;
        if let Some(memory) = limits.memory_bytes {
            if has("memory") {
                fs::write(path.join("memory.max"), memory.value.to_string())?;
                // Refuse swap headroom too, or a memory-capped workload simply
                // swaps instead of being bounded. A host without swap accounting
                // rejects this, which is not a reason to fail the scope.
                let _ = fs::write(path.join("memory.swap.max"), "0");
                memory_enforced = true;
            } else {
                missing.push("the `memory` controller is not delegated to this user's subtree");
            }
        }
        let mut pids_enforced = false;
        if let Some(children) = limits.child_processes {
            if has("pids") {
                fs::write(path.join("pids.max"), children.value.to_string())?;
                pids_enforced = true;
            } else {
                missing.push("the `pids` controller is not delegated to this user's subtree");
            }
        }
        if !memory_enforced && !pids_enforced {
            // Nothing was actually installed, so this scope would advertise a
            // kernel bound it does not hold.
            return Ok(None);
        }

        let procs_fd = fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(path.join("cgroup.procs"))?
            .into();

        Ok(Some(CgroupScope {
            path,
            procs_fd,
            memory_enforced,
            pids_enforced,
            missing_reason: (!missing.is_empty()).then(|| missing.join("; ")),
        }))
    }

    /// The scope directory, so a row can record the one handle that outlives
    /// this app: the kernel keeps enforcing this cgroup after the process which
    /// created it is gone, and a restart needs to be able to name it.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn capabilities(&self) -> ControllerCapabilities {
        let missing = || {
            self.missing_reason
                .clone()
                .unwrap_or_else(|| "this scope was created without that controller".to_string())
        };
        ControllerCapabilities {
            backend: "cgroup v2".to_string(),
            tree_primitive: format!(
                "cgroup v2 scope at {} — membership is inherited by every descendant and is not \
                 escapable by `setsid` or re-parenting",
                self.path.display()
            ),
            // The kernel holds memory and pids; wall clock has no cgroup control,
            // so it stays with the supervising loop and is reported as such.
            wall: LimitCapability::Enforced {
                level: crate::resource_control::EnforcementLevel::Supervised,
                mechanism: "the sampling loop compares elapsed time against the effective wall \
                            limit; cgroup v2 has no wall-clock control"
                    .to_string(),
            },
            memory: if self.memory_enforced {
                LimitCapability::Enforced {
                    level: crate::resource_control::EnforcementLevel::Kernel,
                    mechanism: "cgroup v2 `memory.max`, with `memory.swap.max` at zero so the \
                                bound cannot be evaded by swapping"
                        .to_string(),
                }
            } else {
                LimitCapability::Unavailable { reason: missing() }
            },
            child_processes: if self.pids_enforced {
                LimitCapability::Enforced {
                    level: crate::resource_control::EnforcementLevel::Kernel,
                    mechanism: "cgroup v2 `pids.max`, counted per scope rather than per real uid"
                        .to_string(),
                }
            } else {
                LimitCapability::Unavailable { reason: missing() }
            },
            output: LimitCapability::Enforced {
                level: crate::resource_control::EnforcementLevel::Supervised,
                mechanism: "the capture buffer is bounded as bytes arrive; a pipe is not a \
                            cgroup resource"
                    .to_string(),
            },
            context_tokens: LimitCapability::NotApplicable {
                reason: "a cgroup bounds an OS process; a context budget is enforced at the \
                         model request by the runtime that can count exactly"
                    .to_string(),
            },
        }
    }

    /// Read the scope's membership back and require the workload to be in it.
    ///
    /// The migration write happens between `fork` and `exec`, where a failure has
    /// no channel back to this process: `pre_exec` returning an error aborts the
    /// spawn on some paths and is simply lost on others, and a partially applied
    /// delegation can leave the write refused with `EPERM`. Reading `cgroup.procs`
    /// afterwards is the only way to know the kernel bound is actually holding the
    /// process the record says it is holding.
    pub fn confirm_membership(&self, pid: u32) -> io::Result<()> {
        let members = fs::read_to_string(self.path.join("cgroup.procs"))?;
        if members
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .any(|member| member == pid)
        {
            return Ok(());
        }
        Err(io::Error::other(format!(
            "process {pid} started without joining its cgroup scope at {}, so `memory.max` and \
             `pids.max` are not holding it",
            self.path.display()
        )))
    }

    /// Move an already-running process into this scope.
    ///
    /// The counterpart to the `pre_exec` join for a caller that could not use it.
    /// Writing a pid into `cgroup.procs` migrates that thread group; everything it
    /// forks afterwards is a member by kernel rule, exactly as if it had joined
    /// before `exec`. Idempotent — a process already in the scope is written again
    /// and stays there — which is what lets a caller do both.
    pub fn adopt(&self, pid: u32) -> io::Result<()> {
        fs::write(self.path.join("cgroup.procs"), pid.to_string())
    }

    pub fn prepare_tokio(&self, command: &mut tokio::process::Command) -> io::Result<()> {
        let fd = self.procs_fd.as_raw_fd();
        // Safe: the closure writes two bytes to an already-open descriptor and
        // allocates nothing, which is the whole async-signal-safety requirement.
        unsafe { command.pre_exec(move || join_cgroup(fd)) };
        Ok(())
    }

    pub fn prepare_std(&self, command: &mut std::process::Command) -> io::Result<()> {
        use std::os::unix::process::CommandExt;
        let fd = self.procs_fd.as_raw_fd();
        // Safe: as above.
        unsafe { command.pre_exec(move || join_cgroup(fd)) };
        Ok(())
    }

    /// `memory.current` and `pids.current` — the kernel's own accounting, not a
    /// sum this app computed by walking a process table.
    pub fn sample(&self) -> io::Result<Option<(Option<u64>, Option<u32>)>> {
        let count = read_number(&self.path.join("pids.current"));
        // An empty scope is an exited workload, and must read as "gone" rather
        // than as a tree of zero processes holding zero bytes.
        if count == Some(0) {
            return Ok(None);
        }
        Ok(Some((
            read_number(&self.path.join("memory.current")),
            count.map(|value| u32::try_from(value).unwrap_or(u32::MAX)),
        )))
    }

    /// The kernel's own record that one of these bounds refused something.
    ///
    /// # Why the counters and not the comparison
    ///
    /// `pids.max` is the clear case. A scope capped at twelve refuses the
    /// thirteenth `fork` with `EAGAIN` and leaves `pids.current` at twelve, so
    /// `observed > configured` is false at the moment the limit fires and false at
    /// every sample afterwards. The workload sees a shell reporting "cannot fork"
    /// and dies of it, which without this reads as an ordinary command failure —
    /// the enforcement worked and the app could not say so.
    ///
    /// `memory.max` behaves the same way for the same reason: the kernel reclaims
    /// and then OOM-kills *inside the scope* rather than letting `memory.current`
    /// pass the cap.
    ///
    /// So both are read from `*.events`, which are monotonic counters the kernel
    /// increments at the refusal. A fresh scope starts at zero and no other
    /// workload can write to it, so any non-zero value is this workload's.
    ///
    /// `memory.events`' `max` counter is deliberately **not** treated as a breach
    /// on its own: it counts reclaim under pressure, which a workload right at its
    /// ceiling can do repeatedly while making progress, and killing it for that
    /// would turn a working budget into a flaky one. `oom_kill` is the
    /// unambiguous one — the kernel gave up and killed a member.
    pub fn poll_limit_events(&self) -> io::Result<Option<crate::resource_control::LimitEvent>> {
        use crate::process_table::ProcessLimitKind;
        use crate::resource_control::LimitEvent;

        if self.pids_enforced {
            let refused = read_event_counter(&self.path.join("pids.events"), "max");
            if refused.is_some_and(|count| count > 0) {
                return Ok(Some(LimitEvent {
                    limit: ProcessLimitKind::ChildProcesses,
                    // The kernel held the scope *at* the cap, which is what
                    // refusing the next fork means.
                    observed: read_number(&self.path.join("pids.current")).unwrap_or(0),
                    evidence: "cgroup v2 `pids.events` max, the kernel refusing a fork at the cap"
                        .to_string(),
                }));
            }
        }
        if self.memory_enforced {
            let killed = read_event_counter(&self.path.join("memory.events"), "oom_kill");
            if killed.is_some_and(|count| count > 0) {
                return Ok(Some(LimitEvent {
                    limit: ProcessLimitKind::Memory,
                    observed: read_number(&self.path.join("memory.peak"))
                        .or_else(|| read_number(&self.path.join("memory.current")))
                        .unwrap_or(0),
                    evidence: "cgroup v2 `memory.events` oom_kill, the kernel killing a member \
                               inside the scope rather than letting it pass `memory.max`"
                        .to_string(),
                }));
            }
        }
        Ok(None)
    }

    /// `cgroup.kill`, which SIGKILLs every member atomically — including members
    /// created while the kill is in flight, which a walk-and-signal loop cannot
    /// promise. Kernels before 5.14 have no such file, so the fallback signals
    /// the membership list until it stops changing.
    pub fn terminate_tree(&self) -> io::Result<()> {
        if fs::write(self.path.join("cgroup.kill"), "1").is_ok() {
            self.wait_for_the_scope_to_empty();
            return Ok(());
        }
        for _ in 0..8 {
            let members = fs::read_to_string(self.path.join("cgroup.procs")).unwrap_or_default();
            let mut signalled = 0;
            for pid in members
                .lines()
                .filter_map(|line| line.trim().parse::<i32>().ok())
            {
                // Safe: signals one pid inside a cgroup this process created, so
                // it cannot name a process outside the owned workload.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                signalled += 1;
            }
            if signalled == 0 {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Wait until nothing in the scope is still executing.
    ///
    /// # Why a kill is not finished when the write returns
    ///
    /// `cgroup.kill` queues SIGKILL for every member; the kernel decides when
    /// each one actually dies, and the write returns long before that. Every
    /// caller of `terminate_tree` reads it as "the workload is reclaimed" — the
    /// breach path records the row immediately afterwards, and the tests assert
    /// the tree is gone — so a backend that returns early makes the strongest
    /// enforcement this app has *look* like the flakiest. The supervisor has
    /// always verified across bounded passes with a settle between them; this is
    /// the same promise from the kernel backend, and the reason it is the same
    /// function's job rather than each caller's.
    ///
    /// A **zombie counts as gone**, which is the distinction the rest of this
    /// module already draws: an exited-but-unreaped member holds no memory, runs
    /// no code and cannot fork, and it stays listed in `cgroup.procs` until its
    /// parent reaps it — which for the shell leader is the caller, after this
    /// returns. Waiting for it would be waiting for ourselves.
    ///
    /// Bounded, because "wait until it is empty" against a member the kernel will
    /// not kill is an unbounded loop, and a supervisor that will not return is
    /// worse than one that reports a survivor.
    fn wait_for_the_scope_to_empty(&self) {
        const PASSES: usize = 10;
        const SETTLE: std::time::Duration = std::time::Duration::from_millis(20);
        for _ in 0..PASSES {
            let members = fs::read_to_string(self.path.join("cgroup.procs")).unwrap_or_default();
            let still_running = members
                .lines()
                .filter_map(|line| line.trim().parse::<u32>().ok())
                .any(|pid| {
                    crate::process_tree::ProcessIdentity::of(pid)
                        .is_some_and(|identity| identity.is_running())
                });
            if !still_running {
                return;
            }
            std::thread::sleep(SETTLE);
        }
    }
}

impl Drop for CgroupScope {
    fn drop(&mut self) {
        // A cgroup with members cannot be removed, so the kill has to happen
        // first — otherwise every bounded command leaks a directory that only a
        // reboot collects.
        let _ = self.terminate_tree();
        let _ = fs::remove_dir(&self.path);
    }
}

/// Migrate the calling process into the scope. Runs between `fork` and `exec`.
///
/// Writing `0` names the calling process, so nothing has to format a pid — which
/// matters because formatting allocates and this closure may not.
fn join_cgroup(fd: std::os::fd::RawFd) -> io::Result<()> {
    const SELF: &[u8; 2] = b"0\n";
    // Safe: writes two bytes from a static buffer to a descriptor opened before
    // the fork. No allocation, no lock, no shared state.
    let written = unsafe { libc::write(fd, SELF.as_ptr().cast(), SELF.len()) };
    if written < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// This process's cgroup v2 directory, or `None` where the host is not v2.
fn own_cgroup_path() -> io::Result<Option<PathBuf>> {
    let text = match fs::read_to_string("/proc/self/cgroup") {
        Ok(text) => text,
        Err(_) => return Ok(None),
    };
    Ok(parse_own_cgroup(&text)
        .map(|relative| PathBuf::from(CGROUP_ROOT).join(relative.trim_start_matches('/'))))
}

/// The `0::<path>` line, which is cgroup v2's unified hierarchy.
///
/// A host still on v1 has only numbered controller lines and no `0::` line at
/// all, so this returning `None` is the v1 answer rather than a parse failure.
fn parse_own_cgroup(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::to_string)
}

fn read_controllers(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// A cgroup counter, where `max` is the kernel's spelling of "no limit" and is
/// deliberately not read as a number.
fn read_number(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// One `<key> <count>` line out of a cgroup v2 `*.events` file.
///
/// `None` for a missing file or a missing key rather than zero: a kernel too old
/// to publish `pids.events` has *not* told us the limit did not fire, and reading
/// that as "no breach" is the difference between an honest gap and a wrong
/// answer.
fn read_event_counter(path: &Path, key: &str) -> Option<u64> {
    parse_event_counter(&fs::read_to_string(path).ok()?, key)
}

fn parse_event_counter(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next()? == key).then(|| fields.next()?.parse().ok())?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unified_hierarchy_line_is_the_one_that_is_read() {
        let v2 = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/x.scope\n";
        assert_eq!(
            parse_own_cgroup(v2).as_deref(),
            Some("/user.slice/user-1000.slice/user@1000.service/app.slice/x.scope")
        );
    }

    /// A v1 host has no `0::` line. Reading one of its numbered controller lines
    /// as a v2 path would build a directory under a hierarchy that does not work
    /// this way at all.
    #[test]
    fn a_v1_only_host_reports_no_unified_path_rather_than_a_wrong_one() {
        let v1 = "12:memory:/user.slice\n11:pids:/user.slice\n";
        assert!(parse_own_cgroup(v1).is_none());
    }

    #[test]
    fn a_hybrid_host_still_finds_its_unified_line() {
        let hybrid = "12:memory:/user.slice\n0::/user.slice/user-1000.slice\n";
        assert_eq!(
            parse_own_cgroup(hybrid).as_deref(),
            Some("/user.slice/user-1000.slice")
        );
    }

    /// `max` is not a number, and parsing it as one would either panic or read as
    /// zero — a memory budget of zero bytes fires on the first sample, forever.
    #[test]
    fn the_kernels_word_for_unlimited_is_not_read_as_a_number() {
        let file =
            std::env::temp_dir().join(format!("lm-cgroup-max-{}", uuid::Uuid::new_v4().simple()));
        fs::write(&file, "max\n").expect("write");
        assert_eq!(read_number(&file), None);
        let _ = fs::remove_file(&file);
    }

    /// The counter that proves a `pids.max` fired. `pids.current` never passes
    /// the cap, so this file is the only thing that can say the limit worked.
    #[test]
    fn a_refusal_counter_is_read_by_key_rather_than_by_line_position() {
        let pids = "max 3\n";
        assert_eq!(parse_event_counter(pids, "max"), Some(3));
        // `memory.events` has six keys in an order the kernel is free to change.
        let memory = "low 0\nhigh 0\nmax 41\noom 2\noom_kill 1\noom_group_kill 0\n";
        assert_eq!(parse_event_counter(memory, "oom_kill"), Some(1));
        assert_eq!(parse_event_counter(memory, "max"), Some(41));
    }

    /// A kernel too old to publish the file has not said the limit did not fire.
    /// Reading a missing key as zero would turn an unanswerable question into a
    /// confident "no breach".
    #[test]
    fn a_missing_counter_is_unknown_rather_than_zero() {
        assert_eq!(parse_event_counter("low 0\nhigh 0\n", "oom_kill"), None);
        assert_eq!(parse_event_counter("", "max"), None);
    }

    /// Nothing to enforce means no scope, so an unbounded command does not pay
    /// for a directory, two controller writes and a teardown.
    #[test]
    fn limits_with_nothing_a_cgroup_can_hold_do_not_create_a_scope() {
        let limits = EffectiveLimits::default();
        assert!(CgroupScope::create(&limits).expect("no error").is_none());
    }
}
