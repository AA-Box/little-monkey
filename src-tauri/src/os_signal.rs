//! Real OS suspend/resume of a process group this app owns — SIGSTOP/SIGCONT
//! on unix, `Suspend-Process`/`Resume-Process` on Windows.
//!
//! Extracted from the daemon's job runner (`bin/monkey-cli/daemon/engine.rs`),
//! which needed exactly this to pause a daemon-managed child, so that
//! `background_shell.rs` can deliver the same real suspend/resume to a
//! backgrounded `run_shell` command instead of duplicating the cfg'd
//! shell-out pair.

use std::process::Command;

pub fn suspend_process_group(pid: u32) -> Result<(), String> {
    signal_process_group(pid, true)
}

pub fn resume_process_group(pid: u32) -> Result<(), String> {
    signal_process_group(pid, false)
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
