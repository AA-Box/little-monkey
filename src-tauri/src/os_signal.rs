//! Real OS suspend/resume of a process group this app owns — SIGSTOP/SIGCONT
//! on unix, `Suspend-Process`/`Resume-Process` on Windows.
//!
//! Extracted from the daemon's job runner (`bin/monkey-cli/daemon/engine.rs`),
//! which needed exactly this to pause a daemon-managed child, so that
//! `background_shell.rs` can deliver the same real suspend/resume to a
//! backgrounded `run_shell` command instead of duplicating the cfg'd
//! shell-out pair.

// Only the Windows path shells out now that unix signals through `killpg`;
// the unix tests still spawn a real child to signal.
#[cfg(any(windows, test))]
use std::process::Command;

/// Whether `pid` names a process that still exists.
///
/// Used to decide whether a process row's *host* is gone, which is the one
/// question the process table could not answer: a workflow run is executed by
/// whichever process started it, and after a crash nothing distinguished "still
/// running over there" from "died with its host". See
/// [`crate::process_table::ProcessTable::reap_dead_hosts`].
///
/// **Pid reuse is a real limit and the failure direction is deliberate.** The OS
/// may hand a dead host's pid to an unrelated process, and this then reports the
/// host as alive — so a stale row survives longer than it should. The inverse
/// error, declaring a live host dead, would close a row for work that is still
/// running and is the one outcome worth engineering against; nothing here can
/// produce it. Narrowing reuse further needs the host's start time, which has no
/// portable source across the platforms this ships on.
#[cfg(unix)]
pub fn process_is_alive(pid: u32) -> bool {
    let Ok(target) = i32::try_from(pid) else {
        return false;
    };
    if target <= 0 {
        // 0 means "our own process group" and negatives name a group, so neither
        // is a question about one process. Refuse rather than answer wrongly.
        return false;
    }
    // Safe: two integers, no memory this owns. Signal 0 performs the permission
    // and existence checks without delivering anything.
    if unsafe { libc::kill(target, 0) } == 0 {
        return true;
    }
    // EPERM means the process exists and belongs to another user — which is
    // still "alive", and the answer this function is asked for.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// See the unix version for the pid-reuse caveat, which applies identically.
#[cfg(windows)]
pub fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    if pid == 0 {
        return false;
    }
    // Safe: opens a handle by id and touches no memory this owns. A null handle
    // means the process could not be opened, which for a nonexistent pid is the
    // answer we want.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    // `WaitForSingleObject` with no timeout rather than `GetExitCodeProcess`:
    // a process is free to exit with code 259, which is indistinguishable from
    // `STILL_ACTIVE`. A timeout here means the handle is not signalled, so the
    // process has genuinely not exited.
    let alive = unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
    unsafe {
        CloseHandle(handle);
    }
    alive
}

pub fn suspend_process_group(pid: u32) -> Result<(), String> {
    signal_process_group(pid, true)
}

pub fn resume_process_group(pid: u32) -> Result<(), String> {
    signal_process_group(pid, false)
}

/// How long a process tree gets to wind down after TERM before it is killed.
///
/// Short, and deliberately not a poll-until-gone loop. The obvious shape — TERM,
/// then poll liveness and return as soon as the group is gone — does not work
/// here: the group leader is a child of *this* process that the caller has not
/// reaped yet, so it lingers as a zombie, and a zombie still exists as far as
/// `kill(pid, 0)` is concerned. The poll therefore never fires early and every
/// terminate pays the whole grace period. That was measurable: an early version
/// of this took 2s per call, on a tokio worker thread, at a timeout boundary.
///
/// So the grace is a flat wait sized for the thing it protects — a build flushing
/// output and removing temp files takes milliseconds, not seconds — and anything
/// still alive after it was ignoring TERM anyway.
const TERMINATE_GRACE: std::time::Duration = std::time::Duration::from_millis(250);

/// Ends a whole process tree: TERM, a grace period, then KILL.
///
/// The shape a **timeout** wants, as opposed to [`kill_process_group`]'s
/// immediate KILL: a build or a test run gets the chance to flush its output and
/// remove its temp files, and only a process that ignores TERM is killed outright.
///
/// **This exists because `kill_on_drop` is not a limit.** Tokio's `kill_on_drop`
/// SIGKILLs the one pid it spawned, so a timeout on `sh -c "cargo build"` reaped
/// the shell and left the compiler running — consuming the machine long after the
/// turn reported "timed out". A wall-clock bound that leaves the work running is
/// not a bound.
///
/// Platform mapping, since "process group" is a unix concept:
/// - **unix**: signals the group led by `pid`, so the caller must have spawned the
///   child with `process_group(0)`. Signalling the pid alone would reproduce the
///   exact bug this fixes.
/// - **Windows**: `taskkill /T` walks the child tree by parent, so `pid` is just
///   the child's own id and no group is needed. TERM/KILL is not a distinction
///   Windows offers here, so it is one forced termination.
/// SIGKILL one process, and only if it is still the process that was recorded.
///
/// The identity check is not a nicety: a pid the kernel has since handed to an
/// unrelated process fails it and is skipped, because killing the user's editor
/// because a compiler exited and its pid was reused is a far worse outcome than
/// leaving one process alive.
#[cfg(unix)]
pub fn kill_by_identity(identity: crate::process_tree::ProcessIdentity) {
    if !identity.is_still_alive() {
        return;
    }
    let Ok(target) = libc::pid_t::try_from(identity.pid) else {
        return;
    };
    // 0 is "every process in our own group" and negatives name a group, so
    // neither is a question about one process — and one of them would signal this
    // app. 1 is init.
    if target <= 1 {
        return;
    }
    // Safe: signals one pid whose identity was checked on the line above. A
    // process that exited in between answers ESRCH, which is the wanted outcome.
    unsafe { libc::kill(target, libc::SIGKILL) };
}

#[cfg(not(unix))]
pub fn kill_by_identity(identity: crate::process_tree::ProcessIdentity) {
    if !identity.is_still_alive() {
        return;
    }
    let _ = terminate_process_group(identity.pid);
}

pub fn terminate_process_group(pid: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        signal_group(pid, libc::SIGTERM)?;
        std::thread::sleep(TERMINATE_GRACE);
        // Ignored rather than propagated: an already-gone group is `ESRCH`, which
        // `signal_group` reports as success anyway — the caller wanted it not
        // running, and it is not.
        let _ = kill_process_group(pid);
        Ok(())
    }
    #[cfg(windows)]
    {
        command_ok("taskkill", &["/PID", &pid.to_string(), "/T", "/F"])
    }
}

/// Terminates the group now, with no grace period.
///
/// The delivery a `kill` asks for and a `stop` does not. A cooperative stop
/// sends TERM and gives the child time to wind down; this is what the caller
/// gets when that promise is not good enough — a hung child that ignores TERM
/// is exactly the case `kill` exists for.
#[cfg(unix)]
pub fn kill_process_group(pid: u32) -> Result<(), String> {
    signal_group(pid, libc::SIGKILL)
}

/// Signals the process group led by `pid`.
///
/// A direct `killpg(2)` rather than shelling out to `kill`. The subprocess form
/// looked simpler and was not: `kill -STOP -1234` means "signal process group
/// 1234" to a POSIX/BSD `kill`, but procps-ng's `kill` (the Linux default) reads
/// a leading `-` argument as an option, so the same command is not portable
/// across the platforms this ships on. It also depended on `PATH` and paid a
/// fork+exec per signal. `killpg` has none of those properties and is the call
/// the shell-out was standing in for.
#[cfg(unix)]
fn signal_process_group(pid: u32, stop: bool) -> Result<(), String> {
    signal_group(pid, if stop { libc::SIGSTOP } else { libc::SIGCONT })
}

/// The one `killpg` call site, so the pgid validation and the "already gone is
/// success" rule cannot drift between the signals that share them.
///
/// Safe: `killpg` takes two integers and touches no memory this owns. A negative
/// or zero pgid *is* meaningful to it (0 = "our own group"), which would signal
/// this app, so it is rejected before the call rather than trusted.
#[cfg(unix)]
fn signal_group(pid: u32, signal: i32) -> Result<(), String> {
    let group = i32::try_from(pid).map_err(|_| format!("Process id {pid} is not a valid pgid"))?;
    if group <= 0 {
        return Err(format!("Refusing to signal process group {group}"));
    }
    if unsafe { libc::killpg(group, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    // The group is already gone — it exited between the caller's read and this
    // call. Not a failure: the caller wanted it not running, and it is not.
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(format!("Failed to signal process group {group}: {error}"))
}

#[cfg(windows)]
fn signal_process_group(pid: u32, stop: bool) -> Result<(), String> {
    let verb = if stop {
        "Suspend-Process"
    } else {
        "Resume-Process"
    };
    let script = format!("{verb} -Id {pid} -ErrorAction Stop");
    command_ok(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", &script],
    )
}

/// Windows only — unix signals through `killpg` above. Windows has no
/// process-group stop, so PowerShell's `Suspend-Process` is the actual
/// mechanism rather than a stand-in for a syscall.
#[cfg(windows)]
fn command_ok(program: &str, args: &[&str]) -> Result<(), String> {
    Command::new(program)
        .args(args)
        .status()
        .map_err(|error| format!("Failed to run {program}: {error}"))
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(format!("{program} exited with {status}"))
            }
        })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::thread::sleep;
    use std::time::Duration;

    fn process_state(pid: u32) -> String {
        let output = Command::new("ps")
            .args(["-o", "state=", "-p", &pid.to_string()])
            .output()
            .expect("ps runs");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// The bug this whole change exists for: a grandchild surviving its parent's
    /// termination.
    ///
    /// `kill_on_drop` SIGKILLs one pid, so a timeout on `sh -c "sleep 30"` reaped
    /// the shell and left `sleep` running. Asserted on a *grandchild* rather than
    /// the direct child, because killing the direct child was never the part that
    /// was broken — and a test that only checked the child would have passed
    /// against the old code.
    #[tokio::test]
    async fn terminating_a_group_reaps_the_grandchild_a_direct_kill_would_leave() {
        // The shell prints its child's pid and then waits, so the test learns a pid
        // that `kill_on_drop` would never have touched.
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 30 & echo $! ; wait")
            .stdin(Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .expect("shell spawns");
        let pgid = child.id().expect("child has a pid");

        let mut stdout = child.stdout.take().expect("stdout piped");
        let grandchild = {
            use tokio::io::AsyncReadExt;
            let mut buffer = [0u8; 32];
            let read = stdout.read(&mut buffer).await.expect("read grandchild pid");
            String::from_utf8_lossy(&buffer[..read])
                .trim()
                .parse::<u32>()
                .expect("grandchild pid parses")
        };
        assert!(process_is_alive(grandchild), "grandchild did not start");

        terminate_process_group(pgid).expect("terminate succeeds");

        assert!(
            !process_is_alive(grandchild),
            "the grandchild survived its group being terminated — the exact orphan \
             `kill_on_drop` leaves"
        );
        // The leader is gone too — but only observable after reaping it. Until the
        // caller waits, a terminated child is a zombie, which still *exists*, so
        // `process_is_alive` correctly reports it alive. This is the same
        // distinction `liveness_distinguishes_a_running_child_from_a_reaped_one`
        // pins, and it is why the grace above is a flat wait rather than a poll.
        let status = child.wait().await.expect("leader is reaped");
        assert!(
            !status.success(),
            "a terminated shell must not report success"
        );
        assert!(!process_is_alive(pgid));
    }

    #[test]
    fn terminating_refuses_a_pid_that_is_not_a_group_leader_id() {
        // 0 means "our own process group" to `killpg`, so a caller that lost track
        // of its pgid must not be able to terminate this app.
        assert!(terminate_process_group(0).is_err());
    }

    #[test]
    fn liveness_distinguishes_a_running_child_from_a_reaped_one() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("sleep spawns");
        let pid = child.id();
        sleep(Duration::from_millis(50));
        assert!(process_is_alive(pid), "a running child reported as gone");

        child.kill().expect("kill succeeds");
        // The wait matters: between `kill` and the parent reaping it, the child
        // is a zombie — which still *exists*, so `kill(pid, 0)` succeeds and this
        // correctly reports it alive. Only after the wait is the pid free.
        child.wait().expect("child is reaped");
        assert!(!process_is_alive(pid), "a reaped child reported as alive");
    }

    #[test]
    fn liveness_reports_this_process_and_refuses_a_non_process_id() {
        assert!(process_is_alive(std::process::id()));
        // 0 means "our own process group" to `kill(2)`, so it is not a question
        // about one process and must not answer as though it were.
        assert!(!process_is_alive(0));
    }

    #[test]
    fn suspend_and_resume_actually_change_the_childs_os_state() {
        use std::os::unix::process::CommandExt;
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Real callers always spawn with `process_group(0)` so the group
            // id equals the child's own pid, which is what `-<pid>` targets
            // below — without it the signal would hit whatever group this
            // test process itself belongs to.
            .process_group(0)
            .spawn()
            .expect("sleep spawns");
        let pid = child.id();
        // Give the OS a moment to schedule it before checking state.
        sleep(Duration::from_millis(50));

        suspend_process_group(pid).expect("suspend succeeds");
        sleep(Duration::from_millis(50));
        assert!(
            process_state(pid).starts_with('T'),
            "expected a stopped ('T') state after suspend, got {:?}",
            process_state(pid)
        );

        resume_process_group(pid).expect("resume succeeds");
        sleep(Duration::from_millis(50));
        assert!(
            !process_state(pid).starts_with('T'),
            "expected a running state after resume, got {:?}",
            process_state(pid)
        );

        let _ = child.kill();
        let _ = child.wait();
    }
}
