//! Kernel-held containment for [`crate::sandbox`] runs on Windows.
//!
//! The Windows half of the isolation work, and deliberately a narrower promise
//! than the other two platforms make. macOS gets a Seatbelt profile and Linux a
//! Landlock ruleset — both *filesystem* boundaries, which is why both report
//! [`crate::sandbox::Isolation::OsSandboxed`]. Windows has no equivalent this
//! module can reach, so what it installs is a **job object**: the kernel bounds
//! the process tree, its resources and its window-station reach, and the
//! filesystem stays wide open.
//!
//! That distinction is the whole reason [`crate::sandbox::Isolation`] and
//! [`crate::sandbox::SandboxEnforcement`] gained a third state rather than
//! reusing `OsSandboxed`. A sandboxed command here still reads and writes the
//! real workspace by absolute path. Reporting it as OS-sandboxed would be the
//! exact claim `sandbox_enforcement`'s doc comment exists to stop making.
//!
//! # What the job object actually enforces
//!
//! * `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` — closing the last handle kills every
//!   process in the job. This is the Windows answer to `process_group(0)` +
//!   `terminate_process_group` on unix: a sandboxed command that spawns a build
//!   and then times out leaves nothing behind. [`JobConfinement`] holds that
//!   handle, so the drop *is* the cleanup.
//! * `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` — a ceiling on live processes in the
//!   tree, so a runaway or hostile command cannot fork-bomb the machine. Set
//!   high enough for a real build ([`MAX_ACTIVE_PROCESSES`]) rather than tuned
//!   to a guess about any one toolchain.
//! * `JOB_OBJECT_LIMIT_JOB_MEMORY` — committed memory across the whole tree,
//!   which is strictly better than the per-process `setrlimit` bound unix gets:
//!   a tree of small processes cannot add up to an unbounded total here.
//! * `JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION` — no interactive
//!   "application error" dialog from a crashing child, which on a headless or
//!   unattended machine is a process that never exits.
//! * `JOB_OBJECT_UILIMIT_HANDLES` — the security-relevant UI restriction, not a
//!   cosmetic one: a process in the job cannot use handles to USER objects
//!   (windows, hooks) created outside it, which is the ordinary route to driving
//!   another process on the same desktop. The clipboard, global atom, desktop
//!   switching, display-settings, `ExitWindows` and system-parameter
//!   restrictions come along in the same call.
//!
//! # What it does not enforce, and what would
//!
//! No filesystem boundary and no network boundary. The mechanisms that give
//! those on Windows are a restricted token (`CreateRestrictedToken` with
//! restricting SIDs) or an AppContainer
//! (`PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`), and both must be supplied
//! *at process creation*: they need `CreateProcessAsUserW` or a
//! `STARTUPINFOEX`, neither of which [`std::process::Command`] can express. So
//! either one means this crate owning its own `CreateProcess` call on Windows —
//! losing `tokio::process`'s child reaping and pipe plumbing — plus granting the
//! container SID on the sandbox root, because a restricting SID that nothing in
//! the DACL grants denies the child its own system DLLs and it never starts.
//! That is a project, not a parameter, and it is the upgrade path rather than
//! something this module half-does.
//!
//! # The assignment race
//!
//! A job can be attached atomically at creation with
//! `PROC_THREAD_ATTRIBUTE_JOB_LIST`, which again needs a `STARTUPINFOEX` that
//! `Command` will not build. So the child is spawned first and assigned
//! immediately after, and there is a window between `CreateProcess` returning
//! and [`JobConfinement::assign`] in which the child is unconfined.
//!
//! In practice nothing escapes through it: the child is `cmd.exe /C …`, and it
//! cannot have spawned a grandchild before it has parsed its own command line.
//! In theory a purpose-built binary could. The window is bounded by two
//! syscalls, it is strictly narrower than the no-job-at-all it replaces, and
//! anything already inside the job when it closes still dies.

use std::io;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicUIRestrictions,
    JobObjectExtendedLimitInformation, SetInformationJobObject, JOBOBJECT_BASIC_UI_RESTRICTIONS,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_UILIMIT_DESKTOP,
    JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
};

/// Live processes allowed in one sandboxed tree.
///
/// Generous on purpose. This is a fork-bomb ceiling, not a build budget: a
/// `cargo build -j16` or an npm install spawns dozens of short-lived processes,
/// and a limit tuned to one toolchain's shape would fail honest work while a
/// hostile command only needs one process to do damage the filesystem boundary
/// is supposed to stop — and there is no filesystem boundary here to help.
const MAX_ACTIVE_PROCESSES: u32 = 512;

/// Committed memory allowed across the whole tree, in bytes.
///
/// Deliberately a tree total rather than a per-process cap. 4 GiB is above what
/// a normal build in a sandbox copy needs and well below exhausting a machine
/// that can run this app at all.
///
/// Does not fit a 32-bit `usize`, so a 32-bit Windows target fails to *compile*
/// on this line rather than silently wrapping to a limit that would kill every
/// build. Release builds only x86_64 and aarch64; a visible compile error is the
/// right outcome if that ever changes.
const MAX_JOB_MEMORY_BYTES: usize = 4 * 1024 * 1024 * 1024;

/// An owned job object. Dropping it kills every process still inside.
///
/// The kill-on-close flag makes the handle's lifetime the containment's
/// lifetime, so this must be held for as long as the child may run — see
/// `execute_in_sandbox`, which binds it beside the child and lets both fall at
/// the end of the same scope.
#[derive(Debug)]
pub struct JobConfinement {
    handle: HANDLE,
}

// The handle is owned outright and only ever passed to job-object syscalls,
// which take it by value and are themselves thread-safe. Needed because the
// value is held across an `await` in `execute_in_sandbox`.
unsafe impl Send for JobConfinement {}
unsafe impl Sync for JobConfinement {}

impl Drop for JobConfinement {
    fn drop(&mut self) {
        // Kills the tree, by way of JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE. No
        // error path: nothing useful remains to be done about a handle that
        // will not close, and this runs on the way out of a scope that may
        // itself be unwinding.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Create and configure the job the sandboxed child will be assigned to.
///
/// Parent-side and before the spawn, so a machine that cannot create or
/// configure a job fails the run instead of producing an unconfined child. That
/// is the same choice `os_limits::install` and `sandbox_linux::confine` make: a
/// tool that will not start is visible, and a tool that quietly lost its
/// boundary is not.
pub fn create_job() -> io::Result<JobConfinement> {
    // An anonymous job (null name): nothing else needs to find it, and a named
    // one could be opened by any process in the session.
    let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // Owned from here on, so every early return below closes it.
    let job = JobConfinement { handle };

    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        JobMemoryLimit: MAX_JOB_MEMORY_BYTES,
        ..unsafe { std::mem::zeroed() }
    };
    limits.BasicLimitInformation.ActiveProcessLimit = MAX_ACTIVE_PROCESSES;
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_JOB_MEMORY
        | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    // Safe: `limits` is a fully initialized struct of exactly the type this
    // information class expects, and the size is taken from that same type.
    let set_limits = unsafe {
        SetInformationJobObject(
            job.handle,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .expect("a Win32 struct size fits in u32"),
        )
    };
    if set_limits == 0 {
        return Err(io::Error::last_os_error());
    }

    let restrictions = JOBOBJECT_BASIC_UI_RESTRICTIONS {
        UIRestrictionsClass: JOB_OBJECT_UILIMIT_HANDLES
            | JOB_OBJECT_UILIMIT_READCLIPBOARD
            | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
            | JOB_OBJECT_UILIMIT_GLOBALATOMS
            | JOB_OBJECT_UILIMIT_DESKTOP
            | JOB_OBJECT_UILIMIT_EXITWINDOWS
            | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
            | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS,
    };
    // Safe: same contract as the call above, for this information class.
    let set_ui = unsafe {
        SetInformationJobObject(
            job.handle,
            JobObjectBasicUIRestrictions,
            (&raw const restrictions).cast(),
            u32::try_from(size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>())
                .expect("a Win32 struct size fits in u32"),
        )
    };
    if set_ui == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

impl JobConfinement {
    /// Put an already-spawned child, and everything it goes on to spawn, in the
    /// job. See "the assignment race" above for why this cannot happen at
    /// creation.
    ///
    /// Works when the app itself is already inside someone else's job, which is
    /// the normal case under a CI runner or a job-wrapping terminal: since
    /// Windows 8 the child ends up in both, with this job nested under the outer
    /// one. What is *not* allowed is putting one process into two unrelated jobs
    /// that cannot form that hierarchy — the kernel answers `ERROR_ACCESS_DENIED`,
    /// which is why the test below pins the error path rather than asserting a
    /// second assignment succeeds.
    pub fn assign(&self, child: &tokio::process::Child) -> io::Result<()> {
        let Some(handle) = child.raw_handle() else {
            // Only `None` once the child has been reaped, which cannot have
            // happened between `spawn` and here.
            return Err(io::Error::other(
                "the sandboxed child exited before it could be confined",
            ));
        };
        // Safe: a live process handle owned by the `Child` that outlives this
        // call, passed to a syscall that only reads it.
        let assigned = unsafe { AssignProcessToJobObject(self.handle, handle as HANDLE) };
        match assigned {
            0 => Err(io::Error::last_os_error()),
            _ => Ok(()),
        }
    }
}

/// Whether this machine can hold a sandboxed run in a job object.
///
/// A probe rather than a `cfg!`, for the same reason
/// [`crate::sandbox::sandbox_enforcement`] is: job objects are creatable on
/// every supported Windows version, but a policy or a container could refuse,
/// and answering from the target triple alone is the claim that function exists
/// to avoid. Creates a job and drops it immediately — the drop kills the
/// processes inside, of which there are none.
pub fn job_objects_are_enforceable() -> bool {
    create_job().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A job must be creatable and fully configurable, because
    /// `execute_in_sandbox` fails the run when it is not.
    #[test]
    fn a_configured_job_is_creatable_on_this_machine() {
        let job = create_job().expect("create and configure a job object");
        assert!(!job.handle.is_null());
        assert!(job_objects_are_enforceable());
    }

    /// The containment is real: a process in the job dies when the job handle
    /// closes, without anyone killing it directly.
    ///
    /// `timeout` rather than a bare sleep is what makes this a test of the job
    /// and not of scheduling — a process that outlives its job is the failure,
    /// so the assertion is that it is gone, and the loop bounds how long we
    /// wait to say so.
    #[tokio::test]
    async fn closing_the_job_kills_the_process_inside_it() {
        use std::process::Stdio;

        let job = create_job().expect("job");
        // Waits ~30s for a nonexistent host, i.e. long enough that its exit
        // could only be the job closing. `ping` is present on every Windows.
        let mut child = tokio::process::Command::new("cmd")
            .args(["/C", "ping -n 30 127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        job.assign(&child).expect("assign the child to the job");

        drop(job);

        let exited = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await;
        assert!(
            exited.is_ok(),
            "the child outlived the job that was supposed to kill it"
        );
    }

    /// A refused assignment must surface as `Err`, not as a silent success.
    ///
    /// `execute_in_sandbox` turns this `Result` into a failed run with `?`, so
    /// the entire safety of "a run that returned was contained" rests on this
    /// function reporting failure. An `Ok` on a refused assignment would produce
    /// exactly the unconfined-child-reported-as-contained case the design exists
    /// to prevent.
    ///
    /// Two unrelated jobs is the reliable way to get a refusal: nesting is only
    /// established when the second job can become a child of the first, and one
    /// process placed directly into two sibling jobs cannot form that hierarchy,
    /// so the kernel answers `ERROR_ACCESS_DENIED`. The production shape — this
    /// app inside a CI runner's job, its child into a fresh one — does nest, and
    /// `closing_the_job_kills_the_process_inside_it` covers it: that test assigns
    /// and kills on a runner that puts its own processes in a job.
    ///
    /// The child sleeps rather than exiting, so a refusal here cannot be confused
    /// with assigning an already-dead process.
    #[tokio::test]
    async fn a_refused_assignment_is_reported_rather_than_swallowed() {
        use std::process::Stdio;

        let first = create_job().expect("first job");
        let sibling = create_job().expect("sibling job");
        let mut child = tokio::process::Command::new("cmd")
            .args(["/C", "ping -n 10 127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        first.assign(&child).expect("the first assignment holds");
        assert!(
            sibling.assign(&child).is_err(),
            "a refused assignment must be an Err, or a run could report containment it never got"
        );

        // `first` kills on close, which is what keeps the test from leaking a
        // `ping` that outlives it.
        drop(sibling);
        drop(first);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await;
    }
}
