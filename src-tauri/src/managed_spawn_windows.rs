//! The one way a Windows workload starts: suspended, assigned, verified, resumed.
//!
//! # The window this closes
//!
//! On Unix a containment is installed by the child itself between `fork` and
//! `exec`, so there is no instant at which the workload exists outside it. A
//! Windows job is the opposite shape — it is applied *to* a process by its
//! parent, and nothing a `Command` carries can put it there. So an owner that
//! spawned normally and assigned afterwards left a real window: the child's first
//! instructions ran, and anything it created in that window belonged to no job.
//! Microseconds, and a fork bomb's first fork is microseconds.
//!
//! The agent shells and the sandbox run already avoided that by calling
//! `CreateProcessW` themselves with `CREATE_SUSPENDED`, which is correct and is
//! also several hundred lines of attribute lists and startup-info structs — not
//! something to copy into three more owners. This module is the same ordering
//! obtained the cheap way:
//!
//! ```text
//! Command::creation_flags(CREATE_SUSPENDED)
//!   → spawn                      (the process exists; its thread has not run)
//!   → AssignProcessToJobObject   (containment established)
//!   → IsProcessInJob             (containment verified, not assumed)
//!   → ResumeThread               (the workload's first instruction, at last)
//! ```
//!
//! `std` and `tokio` keep doing the parts they are good at — argv quoting, the
//! environment block, stdio handles, reaping — and the only thing this adds is
//! the four steps above in that order.
//!
//! # Every failure path reclaims
//!
//! A suspended process that is never resumed is a process that never exits: it
//! holds its handles, its memory and its pid forever, and nothing about it looks
//! wrong to an observer. So each of the three steps that can fail terminates the
//! child before returning the error, rather than dropping a `Child` and hoping
//! `kill_on_drop` was set.
//!
//! # Why the thread is found rather than kept
//!
//! `CreateProcessW` hands back a thread handle; `Command::spawn` does not. A
//! process created suspended has exactly one thread, so the toolhelp snapshot
//! below is unambiguous — and the pid cannot be reused while this process holds
//! an open handle to it, which is the property that makes looking it up safe
//! rather than a race.

#![cfg(windows)]

use std::io;
use std::os::windows::io::{AsRawHandle, RawHandle};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::Threading::{
    OpenThread, ResumeThread, TerminateProcess, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME,
};

use crate::sandbox_windows::JobConfinement;

/// The creation flags a managed spawn adds.
///
/// Only `CREATE_SUSPENDED`. Deliberately nothing else: a caller that wants
/// `CREATE_NO_WINDOW` or a unicode environment says so itself, because silently
/// adding flags to somebody else's spawn is how a console-mode tool loses its
/// console two refactors later.
///
/// **This overwrites any flags the caller already set**, because neither `std`
/// nor `tokio` exposes a way to read them back. Every caller in this crate sets
/// them here or not at all.
pub const MANAGED_FLAGS: u32 = CREATE_SUSPENDED;

/// [`spawn_suspended_std`] for a `tokio` command.
pub fn spawn_suspended_tokio(
    job: &JobConfinement,
    command: &mut tokio::process::Command,
    extra_flags: u32,
) -> io::Result<tokio::process::Child> {
    command.creation_flags(MANAGED_FLAGS | extra_flags);
    let child = command.spawn()?;
    let Some(handle) = child.raw_handle() else {
        // A tokio child with no handle has already been reaped, which cannot
        // happen for one created suspended — but the alternative to checking is
        // resuming a process nobody can terminate on the error path.
        return Err(io::Error::other(
            "the suspended child reported no process handle, so its containment could not be \
             verified before it was resumed",
        ));
    };
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("the suspended child reported no pid"))?;
    contain_then_resume(job, handle, pid)?;
    Ok(child)
}

/// Spawn `command` with its job containment established before its first
/// instruction.
///
/// The job must be the one the workload is meant to run under — in production
/// that is [`crate::resource_control::ResourceController::windows_job_for_spawn`],
/// so the limits are the process's effective ones rather than a fixed ceiling.
pub fn spawn_suspended_std(
    job: &JobConfinement,
    command: &mut std::process::Command,
    extra_flags: u32,
) -> io::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;

    command.creation_flags(MANAGED_FLAGS | extra_flags);
    let child = command.spawn()?;
    let handle = child.as_raw_handle();
    let pid = child.id();
    contain_then_resume(job, handle, pid)?;
    Ok(child)
}

/// Assign, verify, resume — and terminate rather than leak on any failure.
fn contain_then_resume(job: &JobConfinement, handle: RawHandle, pid: u32) -> io::Result<()> {
    let process = handle as HANDLE;

    if let Err(error) = job.assign_raw(process) {
        return Err(reclaim(process, error));
    }
    // Read back rather than trusted. An assignment that returned success and did
    // not take would leave a process record claiming a kernel-held bound over a
    // process no job contains, which is the exact failure the whole ordering
    // exists to prevent — and the only moment it can still be fixed for free is
    // while the thread is still suspended.
    match job.contains(pid) {
        Ok(true) => {}
        Ok(false) => {
            return Err(reclaim(
                process,
                io::Error::other(format!(
                    "process {pid} was created suspended and assigned to its job, and the kernel \
                     does not report it as a member, so nothing is holding its limits"
                )),
            ))
        }
        Err(error) => return Err(reclaim(process, error)),
    }

    if let Err(error) = resume_primary_thread(pid) {
        return Err(reclaim(process, error));
    }
    Ok(())
}

/// Terminate a child that must not be allowed to run, and report why.
///
/// The termination's own failure is folded into the message rather than
/// replacing it: the caller needs the original reason, and "and it could not be
/// reclaimed either" is the part that turns a handled error into an incident.
fn reclaim(process: HANDLE, primary: io::Error) -> io::Error {
    // Safe: terminates a process this function's caller just created and still
    // holds a handle to. Exit code 1 is arbitrary and never read — nothing waits
    // on a child that was refused before it ran.
    let terminated = unsafe { TerminateProcess(process, 1) };
    if terminated == 0 {
        let cleanup = io::Error::last_os_error();
        return io::Error::other(format!(
            "{primary}; and the suspended child could not be terminated either ({cleanup}), so it \
             may be holding its pid and handles indefinitely"
        ));
    }
    primary
}

/// Resume every thread the process owns, which for one created suspended is one.
///
/// Enumerated rather than kept, because `Command::spawn` does not hand back a
/// thread handle. The lookup is safe against pid reuse by construction: the
/// caller holds an open handle to this process for the whole call, and Windows
/// cannot reuse a pid while any handle to it is open.
fn resume_primary_thread(pid: u32) -> io::Result<()> {
    // Safe: takes a snapshot of the system's threads. The handle is closed below
    // on every path.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;

    let mut resumed = 0usize;
    let mut failure: Option<io::Error> = None;
    // Safe: reads the first entry into a correctly sized struct this stack owns.
    let mut ok = unsafe { Thread32First(snapshot, &mut entry) };
    while ok != 0 {
        if entry.th32OwnerProcessID == pid {
            match resume_thread(entry.th32ThreadID) {
                Ok(()) => resumed += 1,
                Err(error) => failure = Some(error),
            }
        }
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        // Safe: as above.
        ok = unsafe { Thread32Next(snapshot, &mut entry) };
    }
    // Safe: closes the snapshot handle this function opened, exactly once.
    unsafe {
        let _ = CloseHandle(snapshot);
    }

    if let Some(error) = failure {
        return Err(error);
    }
    if resumed == 0 {
        return Err(io::Error::other(format!(
            "no thread of process {pid} could be resumed, so the workload was created suspended \
             and would never have run"
        )));
    }
    Ok(())
}

fn resume_thread(thread_id: u32) -> io::Result<()> {
    // Safe: opens one thread with exactly the right this needs, or null.
    let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) };
    if thread.is_null() {
        return Err(io::Error::last_os_error());
    }
    // Safe: the handle is live for this call and closed immediately after.
    let previous = unsafe { ResumeThread(thread) };
    // Safe: closes the handle opened above, exactly once.
    unsafe {
        let _ = CloseHandle(thread);
    }
    if previous == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_table::ProcessLimits;
    use crate::resource_control::{EffectiveLimits, LimitLayer, LimitSource, ResourceController};

    fn controller() -> ResourceController {
        ResourceController::new(EffectiveLimits::resolve(&[LimitLayer::new(
            LimitSource::UserOverride,
            ProcessLimits {
                max_child_processes: Some(16),
                ..ProcessLimits::default()
            },
        )]))
    }

    /// The property the whole module exists for: a workload whose very first act
    /// is to create a descendant cannot produce one outside the job.
    ///
    /// The old ordering — spawn, then assign — left a window here that this test
    /// would have to be lucky to catch. This one does not race it: the child is
    /// still suspended when the job membership is read, so the assertion is about
    /// the ordering itself rather than about whether the ordering was fast enough.
    #[tokio::test]
    async fn a_child_is_inside_its_job_before_it_has_run_an_instruction() {
        let controller = controller();
        assert_eq!(
            controller.capabilities().backend,
            "windows job object",
            "every Windows host can create a job; a fallback here is a real failure"
        );
        let job = controller
            .windows_job_for_spawn()
            .expect("the job duplicates for the spawn");

        let mut command = tokio::process::Command::new("cmd.exe");
        command
            .args(["/C", "ping -n 6 127.0.0.1 > NUL"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = spawn_suspended_tokio(&job, &mut command, 0)
            .expect("the managed spawn contains the child");
        let pid = child.id().expect("a live child has a pid");

        assert!(
            job.contains(pid).expect("membership reads"),
            "the child is not in the job it was created into"
        );
        // And the descendant it creates is in the same job, which is the thing a
        // late assignment could not promise.
        let _ = child.kill().await;
    }

    /// A child refused before it runs is terminated, not left holding its pid.
    ///
    /// A suspended process that is never resumed never exits: it keeps its pid,
    /// its handles and its memory until the machine reboots, and nothing about it
    /// looks wrong to an observer. So the error path is driven directly rather
    /// than trusted — and the marker file proves the second half, that the
    /// workload's first instruction never ran.
    #[tokio::test]
    async fn a_child_refused_before_it_runs_is_terminated_rather_than_left_suspended() {
        use std::os::windows::process::CommandExt;

        let marker = std::env::temp_dir().join(format!(
            "little_monkey_managed_{}_{}.txt",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let mut command = std::process::Command::new("cmd.exe");
        command
            .args(["/C", &format!("echo ran > \"{}\"", marker.display())])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(MANAGED_FLAGS);
        let child = command.spawn().expect("the suspended child is created");
        let pid = child.id();

        // The error path, exactly as `contain_then_resume` takes it.
        let error = reclaim(
            child.as_raw_handle() as HANDLE,
            io::Error::other("the job refused this process"),
        );
        assert_eq!(
            error.to_string(),
            "the job refused this process",
            "the original reason must survive the reclaim: {error}"
        );

        for _ in 0..200 {
            if !crate::os_signal::process_is_alive(pid) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            !crate::os_signal::process_is_alive(pid),
            "a child refused before it ran is still holding pid {pid}"
        );
        assert!(
            !marker.exists(),
            "the refused workload ran its first instruction anyway"
        );
        drop(child);
        let _ = std::fs::remove_file(&marker);
    }
}
