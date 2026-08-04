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

/// Terminates the group now, with no grace period.
///
/// The delivery a `kill` asks for and a `stop` does not. A cooperative stop
/// sends TERM and gives the child time to wind down; this is what the caller
/// gets when that promise is not good enough — a hung child that ignores TERM
/// is exactly the case `kill` exists for.
#[cfg(unix)]
pub fn kill_process_group(pid: u32) -> Result<(), String> {
    let group = i32::try_from(pid).map_err(|_| format!("Process id {pid} is not a valid pgid"))?;
    if group <= 0 {
        return Err(format!("Refusing to signal process group {group}"));
    }
    // Safe for the same reason as `signal_process_group` — two integers, no
    // memory this owns, and a group id validated as positive above.
    if unsafe { libc::killpg(group, libc::SIGKILL) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(format!("Failed to kill process group {group}: {error}"))
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
    let signal = if stop { libc::SIGSTOP } else { libc::SIGCONT };
    // Safe: `killpg` takes two integers and touches no memory this owns. A
    // negative or zero pgid would be meaningful (0 = "our own group"), so it is
    // rejected before the call rather than signalling this process by accident.
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
    let verb = if stop { "Suspend-Process" } else { "Resume-Process" };
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
