//! Kernel-enforced confinement for [`crate::sandbox`] runs on Windows.
//!
//! Two layers, and they answer different questions. A **job object** bounds the
//! process tree, its resources and its window-station reach. An **AppContainer**
//! is the filesystem and network boundary, the counterpart to the macOS Seatbelt
//! profile and the Linux Landlock ruleset — which is why a run that gets one
//! reports [`crate::sandbox::Isolation::OsSandboxed`] like the other two
//! platforms, and a run that only gets the job reports
//! [`crate::sandbox::Isolation::ProcessContained`].
//!
//! # The filesystem boundary, and why it is a deny-list of nothing
//!
//! An AppContainer process can reach an object only if its DACL grants that
//! container's SID, or grants `ALL APPLICATION PACKAGES`. So the grant is a
//! single ACE on the sandbox root ([`AppContainer::grant_tree_access`]) and
//! nothing else: the workspace copy, `SANDBOX_HOME_DIR` and `SANDBOX_TMP_DIR`
//! all live inside it. Everything the child needs in order to *run* —
//! `System32`, its DLLs, `cmd.exe` — is already granted to
//! `ALL APPLICATION PACKAGES` by Windows itself, which is how any packaged app
//! loads anything.
//!
//! One thing the container takes back rather than grants: `TMP` and `TEMP` are
//! rewritten to the container's own package temp on the way in, whatever the
//! environment block says, so the sandbox-owned temp directory has to be restored
//! from inside the child. See [`APP_CONTAINER_OVERWRITTEN_ENV_KEYS`] — and treat
//! any *other* sandbox-owned variable that stops arriving as the same class of
//! bug, not as a caller mistake.
//!
//! That inverts the shape of the other two platforms and is stronger for it.
//! `build_seatbelt_profile` and `sandbox_linux` have to *enumerate* readable
//! system roots and keep that list correct per distribution; here the user's
//! home, the real workspace, and every other user file are denied because
//! nothing granted them, not because a list remembered to leave them out.
//!
//! One documented narrowing, the filesystem counterpart to the loopback one
//! below: **`cmd`'s `dir` does not work in a sandboxed run.** The tree itself is
//! enumerable — through `FindFirstFile`, through `System.IO`, through any
//! language's directory API — but `DIR` reads the volume first, and the grant is
//! one directory tree with nothing above it, so `C:\` is reachable by nothing the
//! container holds. Widening that would put an ACE on the drive root for every
//! run. See [`AppContainer::grant_tree_access`] for the evidence and the trade.
//!
//! # Network
//!
//! An AppContainer has no network at all unless a capability grants it, so
//! `allow_network: false` is the absence of a capability rather than a filter to
//! get right — closer to Seatbelt's `(deny network*)` than to the Linux seccomp
//! filter, and it needs no syscall list. `allow_network: true` adds
//! `internetClient` and `privateNetworkClientServer`.
//!
//! One documented narrowing: **loopback stays blocked even when network is
//! allowed.** Windows blocks loopback from an AppContainer unless the container
//! SID has a machine-wide loopback exemption, which is an admin-level setting
//! (`CheckNetIsolation`) this app has no business writing. A network-allowed run
//! here can therefore reach a public address but not `127.0.0.1`, where the
//! macOS and Linux paths reach both. It is stricter than those, never looser, so
//! it cannot turn a denied run into an allowed one.
//!
//! # What the job object adds on top
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
//! # Why this module owns `CreateProcessW`
//!
//! The security capabilities travel in a `STARTUPINFOEX` attribute list, and
//! that is unreachable through [`std::process::Command`]: `CommandExt`'s
//! `raw_attribute` is nightly-only, and `creation_flags` cannot supply the list
//! the `EXTENDED_STARTUPINFO_PRESENT` flag refers to. So [`run_confined`] calls
//! `CreateProcessW` itself and rebuilds what `tokio::process` was doing — pipes,
//! the UTF-16 environment block, the wait, the timeout kill. Everything about
//! the spawn is otherwise the same as the `Command` path it replaced: cleared
//! environment, the caller's allowlist, the sandbox working directory, piped
//! stdout/stderr, `NUL` on stdin.
//!
//! Two consequences worth knowing:
//!
//! * The parent's copies of the child's pipe *write* ends are closed the moment
//!   the child holds its own. Skip that and the read end never reaches EOF, so
//!   every run would block until its timeout instead of until the command
//!   finished.
//! * The timeout kills through the job (`TerminateJobObject`), not the process.
//!   It takes the whole tree, and closing the child's pipe ends is exactly what
//!   releases the reader.
//!
//! # The assignment race
//!
//! A job can be attached atomically at creation with
//! `PROC_THREAD_ATTRIBUTE_JOB_LIST`. This module already builds an attribute
//! list, so adding it is now cheap — but the AppContainer is what confines the
//! filesystem, and it *is* applied atomically at creation, so the window below
//! no longer has a filesystem escape in it. The child is spawned and then
//! assigned to the job, leaving a two-syscall window in which it is inside the
//! container but not yet inside the job: it cannot touch the real workspace, only
//! outlive a `TerminateJobObject` that has not happened yet. `cmd.exe /C` cannot
//! spawn a grandchild before parsing its command line, so nothing reaches it in
//! practice.

use std::ffi::{c_void, OsStr};
use std::io::{self, Read};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
use std::path::Path;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, LocalFree, SetHandleInformation, ERROR_SUCCESS, HANDLE, HANDLE_FLAG_INHERIT,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
    GRANT_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile,
};
use windows_sys::Win32::Security::{
    CreateWellKnownSid, FreeSid, WinCapabilityInternetClientSid,
    WinCapabilityPrivateNetworkClientServerSid, ACL, DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SECURITY_MAX_SID_SIZE,
    SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT, WELL_KNOWN_SID_TYPE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ALL_ACCESS, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicUIRestrictions,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
    JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_UILIMIT_DESKTOP,
    JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, UpdateProcThreadAttribute, WaitForSingleObject,
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, INFINITE,
    PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, STARTUPINFOW,
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
        self.assign_raw(handle as HANDLE)
    }

    /// [`JobConfinement::assign`] for a handle this crate created itself.
    ///
    /// `run_confined` calls `CreateProcessW` directly, so it holds a raw process
    /// handle rather than a `tokio::process::Child`. Same syscall, same
    /// fatal-on-failure contract — see the caller.
    pub fn assign_raw(&self, process: HANDLE) -> io::Result<()> {
        // Safe: a live process handle owned by the caller for longer than this
        // call, passed to a syscall that only reads it.
        let assigned = unsafe { AssignProcessToJobObject(self.handle, process) };
        match assigned {
            0 => Err(os_error("AssignProcessToJobObject")),
            _ => Ok(()),
        }
    }

    /// Kill everything in the job now, without giving up the handle.
    ///
    /// The timeout path needs this rather than a `drop`: the job has to stay
    /// alive while the output pipes are drained, and killing the tree is what
    /// closes the child's ends so those reads reach EOF instead of blocking.
    /// Best effort — a kill that fails leaves the `Drop` to try again.
    pub fn terminate(&self) {
        // 1 rather than 0: the exit code is what a killed tree reports, and a
        // zero there would read as a clean exit.
        unsafe {
            let _ = TerminateJobObject(self.handle, 1);
        }
    }
}

/// The last OS error, labelled with the call that produced it.
///
/// Every failure in [`spawn_confined`] is one of six syscalls returning a bare
/// `BOOL`, and `io::Error::last_os_error()` alone cannot say which. A Windows-only
/// path that can only be debugged from a CI log has to name its own failures, or
/// diagnosing one costs a full CI round trip per guess — which is exactly what
/// `ERROR_ENVVAR_NOT_FOUND` out of an unlabelled `CreateProcessW` cost.
fn os_error(call: &str) -> io::Error {
    let error = io::Error::last_os_error();
    io::Error::new(
        error.kind(),
        format!(
            "{call} failed: {error} (os error {:?})",
            error.raw_os_error()
        ),
    )
}

/// A NUL-terminated UTF-16 buffer, which is what every `PCWSTR` here wants.
fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain([0]).collect()
}

/// The same for a path, which may hold non-UTF-8 and must not be lossily
/// converted on the way to a syscall that will act on it.
fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

/// An owned AppContainer profile and its SID.
///
/// The profile is per-run and deleted on drop: it is registry and directory
/// state under the user's profile, so leaking one per sandboxed run would be a
/// slow leak of exactly the kind nobody notices until there are thousands.
pub struct AppContainer {
    name: Vec<u16>,
    sid: PSID,
}

// The SID is an owned allocation only ever passed to syscalls that read it.
// Needed because the container is held across the `await`s in `run_confined`.
unsafe impl Send for AppContainer {}
unsafe impl Sync for AppContainer {}

impl Drop for AppContainer {
    fn drop(&mut self) {
        unsafe {
            FreeSid(self.sid);
            // Best effort: a profile that will not delete is a leak to report,
            // not a reason to fail a run that has already finished.
            let _ = DeleteAppContainerProfile(self.name.as_ptr());
        }
    }
}

/// Create a fresh AppContainer for one run.
///
/// The name has to be unique per run and at most 64 characters, so it is a
/// fixed prefix plus the caller's tag (a run id). Capabilities are attached at
/// *spawn* rather than here — [`SECURITY_CAPABILITIES`] carries them, and
/// keeping them out of the profile means the same profile shape is used whether
/// or not the run is allowed network.
pub fn create_app_container(tag: &str) -> io::Result<AppContainer> {
    // 64 is the documented ceiling for a container name. The tag is a hex run
    // id, so truncating it keeps it unique in practice and cannot produce an
    // invalid character.
    let short: String = tag
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(32)
        .collect();
    let name = wide(&format!("LittleMonkey.Sandbox.{short}"));
    let display = wide("Little Monkey sandboxed run");
    let mut sid: PSID = std::ptr::null_mut();
    // Safe: all three strings are NUL-terminated and outlive the call, and the
    // out-parameter is a valid `PSID` slot.
    let created = unsafe {
        CreateAppContainerProfile(
            name.as_ptr(),
            display.as_ptr(),
            display.as_ptr(),
            std::ptr::null(),
            0,
            &mut sid,
        )
    };
    if created < 0 {
        // Includes HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS) from a previous run
        // that died before its `Drop` ran. Deleting and retrying once is the
        // difference between recovering and never sandboxing again on this
        // machine until the profile is cleaned by hand.
        unsafe {
            let _ = DeleteAppContainerProfile(name.as_ptr());
        }
        let retried = unsafe {
            CreateAppContainerProfile(
                name.as_ptr(),
                display.as_ptr(),
                display.as_ptr(),
                std::ptr::null(),
                0,
                &mut sid,
            )
        };
        if retried < 0 {
            return Err(io::Error::other(format!(
                "could not create an AppContainer profile (HRESULT {retried:#x})"
            )));
        }
    }
    Ok(AppContainer { name, sid })
}

impl AppContainer {
    /// Grant this container full access to one directory tree, inheritably.
    ///
    /// This is the entire filesystem grant. Everything else the child can reach
    /// is what Windows already grants `ALL APPLICATION PACKAGES` — System32 and
    /// friends, read and execute, which is how any packaged app loads its DLLs —
    /// so the user's home, the real workspace and every other user file are
    /// denied by construction rather than by a list this code has to keep
    /// correct. That is a stronger default than the Seatbelt and Landlock roots,
    /// which have to enumerate what is readable.
    ///
    /// # What this grant does *not* reach: `cmd`'s `dir`
    ///
    /// The grant covers enumeration. A confined run lists this tree through
    /// `FindFirstFile` and through `System.IO` — including subdirectories that
    /// existed before the grant ran, which is how `execute_in_sandbox` creates
    /// `SANDBOX_HOME_DIR` and `SANDBOX_TMP_DIR`; `SetNamedSecurityInfoW`
    /// propagates the inheritable ACE onto them and Windows renders it as `(F)`.
    ///
    /// `dir` still answers "Access is denied." — on any path, including
    /// `System32\drivers\etc`, which this app never touches and Windows grants to
    /// `ALL APPLICATION PACKAGES`. The refused object is not a directory: cmd's
    /// `DIR` reads the *volume* before it lists anything, `/b` suppresses the
    /// header rather than the query, and `vol C:` is refused in the same run. This
    /// grant is one directory tree and deliberately nothing above it, so `C:\` is
    /// granted to nothing the container holds.
    ///
    /// Left as it is, and the trade is the point. Making `dir` work means an ACE
    /// on the volume root for every sandboxed run, which is a wider boundary than
    /// this whole module is for — so a sandboxed command that shells out to `dir`
    /// (or `cd /d`, or anything else that touches the drive) fails, while the same
    /// listing through PowerShell, `attrib`, or any language's directory API
    /// works. `a_confined_run_can_enumerate_the_tree_it_was_granted` holds both
    /// halves, and its `vol` assertion is what turns red if the volume ever
    /// becomes reachable.
    ///
    /// Ancestors of `path` are deliberately not touched. Traverse checks on them
    /// are bypassed by `SeChangeNotifyPrivilege`, which an AppContainer token
    /// retains; granting an ACE on each parent would mean writing ACEs onto the
    /// user's `AppData` for every run.
    pub fn grant_tree_access(&self, path: &Path) -> io::Result<()> {
        let object = wide_path(path);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // Safe: `object` is NUL-terminated; every out-parameter is a valid slot,
        // and the two we do not want are passed as null as the API allows.
        let read = unsafe {
            GetNamedSecurityInfoW(
                object.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        if read != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(read as i32));
        }
        // `descriptor` owns the buffer `dacl` points into, so it is freed only
        // after `SetEntriesInAclW` has copied what it needs.
        let entry = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                // `TRUSTEE_IS_SID` means this field is a SID, not a string.
                ptstrName: self.sid.cast(),
            },
        };
        let mut merged: *mut ACL = std::ptr::null_mut();
        let combined = unsafe { SetEntriesInAclW(1, &entry, dacl, &mut merged) };
        if combined != ERROR_SUCCESS {
            unsafe {
                LocalFree(descriptor.cast());
            }
            return Err(io::Error::from_raw_os_error(combined as i32));
        }
        let applied = unsafe {
            SetNamedSecurityInfoW(
                object.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                merged,
                std::ptr::null(),
            )
        };
        unsafe {
            LocalFree(merged.cast());
            LocalFree(descriptor.cast());
        }
        match applied {
            ERROR_SUCCESS => Ok(()),
            error => Err(io::Error::from_raw_os_error(error as i32)),
        }
    }
}

/// A well-known capability SID, in a caller-owned buffer.
///
/// Capabilities are how an AppContainer is granted anything beyond its own
/// ACL'd tree. Only the two network ones are ever requested here, and only when
/// the run opted into network.
fn capability_sid(kind: WELL_KNOWN_SID_TYPE) -> io::Result<Vec<u8>> {
    let mut buffer = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut size = buffer.len() as u32;
    // Safe: the buffer is at least `SECURITY_MAX_SID_SIZE`, which is the
    // documented upper bound for any SID this can produce.
    let created = unsafe {
        CreateWellKnownSid(
            kind,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if created == 0 {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(size as usize);
    Ok(buffer)
}

/// What a confined run produced.
pub struct ConfinedOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

/// One end of an anonymous pipe pair, with the child's end inheritable and the
/// parent's end explicitly not.
///
/// The parent's copy of the *write* end must be closed once the child holds its
/// own, or the read end never reaches EOF and the reader thread blocks until the
/// timeout instead of until the command finishes.
fn piped() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    // Safe: both out-parameters are valid slots and the attributes outlive the
    // call.
    if unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) } == 0 {
        return Err(os_error("CreatePipe"));
    }
    // The parent keeps the read end and must not leak it into the child.
    if unsafe { SetHandleInformation(read, HANDLE_FLAG_INHERIT, 0) } == 0 {
        let error = os_error("SetHandleInformation");
        unsafe {
            let _ = CloseHandle(read);
            let _ = CloseHandle(write);
        }
        return Err(error);
    }
    // Safe: both handles are freshly created, owned here, and not closed again
    // by this function.
    unsafe {
        Ok((
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        ))
    }
}

/// An inheritable handle to `NUL`, so the child's stdin reads EOF rather than
/// inheriting this process's console.
fn null_device() -> io::Result<OwnedHandle> {
    let name = wide("NUL");
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    // Safe: a NUL-terminated device name and a valid attributes struct.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(os_error("CreateFileW(NUL)"));
    }
    // Safe: freshly opened and owned here.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// Variables `CreateProcessW` itself needs in order to *build* an AppContainer
/// process, as opposed to variables the sandboxed command wants.
///
/// An AppContainer's private storage lives under
/// `%LOCALAPPDATA%\Packages\<container>`, and the loader resolves that from the
/// environment block it is handed. Hand it a block without these and process
/// creation fails with `ERROR_ENVVAR_NOT_FOUND` (203) — the error names exactly
/// what happened, but it points at `CreateProcessW` rather than at the missing
/// key, which is a long way from obvious.
///
/// `crate::sandbox`'s allowlist deliberately does not carry them: `BASE_ENV_KEYS`
/// is `PATH` + `SystemRoot`, and `HOME`/`USERPROFILE`/`TMP`/`TEMP` are replaced
/// with sandbox-owned paths. So they are filled in here, from this process, and
/// only when a container is actually being created.
///
/// Not a hole in the sandbox: these are *strings*, and the container has no grant
/// on the directories they name, so it cannot read them. The confinement comes
/// from the DACL, never from the environment.
const APP_CONTAINER_ENV_KEYS: &[&str] = &["LOCALAPPDATA", "SystemRoot", "USERPROFILE"];

/// The variables Windows overwrites on the way into an AppContainer, and which
/// therefore cannot be delivered by the environment block alone.
///
/// A container child's `TMP` and `TEMP` come back as
/// `%LOCALAPPDATA%\Packages\<container>\AC\Temp` — the container's own package
/// temp — no matter what [`environment_block`] put there. That silently undoes
/// the sandbox's promise that temporary files live under the sandbox root: they
/// land outside the tree the caller cleans up, and inside the profile that
/// [`AppContainer::drop`] deletes, so a run's temporary output is destroyed with
/// the container that wrote it. Every `%TMP%` write in
/// `sandbox::tests::app_container_cannot_read_or_write_real_workspace_with_or_without_network`
/// went there, which is why the host kept finding the sandbox's `tmp` empty.
///
/// `USERPROFILE` survives, so it is not in this list; only the temp pair is
/// rewritten. See [`temp_reassignment`] for how they are put back.
const APP_CONTAINER_OVERWRITTEN_ENV_KEYS: &[&str] = &["TMP", "TEMP"];

/// `set` statements that put [`APP_CONTAINER_OVERWRITTEN_ENV_KEYS`] back to what
/// the caller asked for, prefixed to the child's command line.
///
/// The only place left to do it. The environment block is rewritten *during*
/// process creation, so the value has to be restored from inside the child —
/// which for `cmd.exe /C` means `set` before the caller's command, taking effect
/// for it and for everything it spawns. Values come from the caller's own
/// allowlist rather than a second copy of the sandbox layout, so this cannot
/// drift from what `crate::sandbox` decided the sandbox temp directory is.
///
/// Quoted `set "K=V"` rather than bare `set K=V`: it keeps a trailing space out
/// of the value, and it makes a path containing `&` literal instead of a command
/// separator.
///
/// # What this reaches, and the one thing it cannot
///
/// Every process the command starts reads the corrected value, because it is in
/// the environment they inherit — which is what a sandboxed run is for, and what
/// `a_confined_run_gets_the_sandbox_temp_directory_not_the_containers` asserts
/// through `set TMP` rather than `%TMP%`.
///
/// A literal `%TMP%` written *in the caller's own command* is not corrected, and
/// cannot be from here: cmd expands a command line once, when it parses it, so
/// every `%TMP%` on the line was already substituted before this `set` ran.
/// Reaching that too would mean running the caller's command from a batch file,
/// which changes `exit /b`, `%0` and argument handling for every command the app
/// runs — a worse trade than a documented narrowing. So a test that needs the
/// sandbox temp directory should name it in full rather than ask cmd for it.
///
/// Applied whether or not a container is present. A job-only run is not rewritten
/// and does not need this, but re-asserting a value it already has is a no-op —
/// cheaper than a second command-line shape that only one of the two paths ever
/// exercises.
fn temp_reassignment(env: &[(String, String)]) -> String {
    let mut prefix = String::new();
    for key in APP_CONTAINER_OVERWRITTEN_ENV_KEYS {
        if let Some((_, value)) = env
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(key))
        {
            prefix.push_str(&format!("set \"{key}={value}\"&"));
        }
    }
    prefix
}

/// `KEY=VALUE\0…\0\0` in UTF-16, which is what `CREATE_UNICODE_ENVIRONMENT`
/// expects. An empty block would give the child *this* process's environment,
/// so the caller's allowlist is passed even when it is short.
///
/// `extra` is appended only for keys the caller did not already set, so a
/// sandbox-owned `USERPROFILE` always wins over the real one.
fn environment_block(env: &[(String, String)], extra: &[&str]) -> Vec<u16> {
    let mut block = Vec::new();
    for (key, value) in env {
        block.extend(OsStr::new(&format!("{key}={value}")).encode_wide());
        block.push(0);
    }
    for key in extra {
        if env
            .iter()
            .any(|(existing, _)| existing.eq_ignore_ascii_case(key))
        {
            continue;
        }
        if let Some(value) = std::env::var_os(key) {
            block.extend(OsStr::new(key).encode_wide());
            block.push(u16::from(b'='));
            block.extend(value.encode_wide());
            block.push(0);
        }
    }
    block.push(0);
    block
}

/// Run `shell_command` inside `container`, confined by `job`, and collect its
/// output.
///
/// This exists because `std::process::Command` cannot express what an
/// AppContainer needs: the security capabilities travel in a `STARTUPINFOEX`
/// attribute list, which is only reachable through `CreateProcessW` directly
/// (`CommandExt::raw_attribute` is nightly-only). Everything else about the
/// spawn is deliberately the same as the `Command` path it replaces — cleared
/// environment, the caller's allowlist, the sandbox working directory, piped
/// stdout/stderr, `NUL` on stdin.
///
/// The timeout kills through the job rather than the process: `TerminateJobObject`
/// takes the whole tree, so a command that spawned a build does not leave it
/// behind. Killing also closes the child's pipe ends, which is what lets the
/// reader threads finish instead of blocking forever.
/// `container: None` is the degraded path — a machine that could not give us an
/// AppContainer still gets the job object, and the caller reports
/// [`crate::sandbox::Isolation::ProcessContained`] for it. Same spawn either way,
/// so there is one implementation of the pipes, the environment block and the
/// timeout rather than two that can drift.
///
/// Everything raw lives in [`spawn_confined`], which is deliberately synchronous:
/// `STARTUPINFOEXW`, `PROCESS_INFORMATION`, the attribute list and the
/// `SID_AND_ATTRIBUTES` array are all raw pointers and therefore not `Send`, and a
/// `tauri::command`'s future must be. Holding any of them across the `await` below
/// makes the whole command non-`Send`, which is a compile error several call
/// frames away from the cause — so they are confined to a scope that ends before
/// the first `await`, and only `OwnedHandle`/`File`/`usize` cross it.
fn spawn_confined(
    container: Option<&AppContainer>,
    job: &JobConfinement,
    shell_command: &str,
    workspace_dir: &Path,
    env: &[(String, String)],
    allow_network: bool,
) -> io::Result<(OwnedHandle, std::fs::File, std::fs::File)> {
    let (stdout_read, stdout_write) = piped()?;
    let (stderr_read, stderr_write) = piped()?;
    let stdin = null_device()?;

    // Capability SIDs and the `SID_AND_ATTRIBUTES` array that points into them
    // must both outlive the `CreateProcessW` call, so they are bound here rather
    // than built inline in the attribute update below.
    let capability_sids = match (container.is_some(), allow_network) {
        (true, true) => vec![
            capability_sid(WinCapabilityInternetClientSid)?,
            capability_sid(WinCapabilityPrivateNetworkClientServerSid)?,
        ],
        _ => Vec::new(),
    };
    let mut capabilities: Vec<SID_AND_ATTRIBUTES> = capability_sids
        .iter()
        .map(|sid| SID_AND_ATTRIBUTES {
            Sid: sid.as_ptr() as PSID,
            // SE_GROUP_ENABLED. A capability that is present but not enabled
            // grants nothing, which would look exactly like a denied network.
            Attributes: 0x4,
        })
        .collect();
    let security = container.map(|container| SECURITY_CAPABILITIES {
        AppContainerSid: container.sid,
        Capabilities: match capabilities.is_empty() {
            true => std::ptr::null_mut(),
            false => capabilities.as_mut_ptr(),
        },
        CapabilityCount: capabilities.len() as u32,
        Reserved: 0,
    });

    // The attribute list exists only to carry the security capabilities, so
    // without a container there is nothing to build and the plain `STARTUPINFOW`
    // form is used instead.
    let mut attribute_buffer = Vec::new();
    let attribute_list = match security.as_ref() {
        None => std::ptr::null_mut(),
        Some(security) => {
            let mut size: usize = 0;
            // Expected to fail with ERROR_INSUFFICIENT_BUFFER; that is how the
            // size is obtained, so the return value is deliberately ignored.
            unsafe {
                let _ = InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
            }
            attribute_buffer = vec![0u8; size];
            let list = attribute_buffer.as_mut_ptr().cast::<c_void>();
            // Safe: the buffer is exactly the size the call above asked for.
            if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) } == 0 {
                return Err(os_error("InitializeProcThreadAttributeList"));
            }
            // Safe: `security` outlives the spawn below, and the size matches its
            // type.
            let updated = unsafe {
                UpdateProcThreadAttribute(
                    list,
                    0,
                    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                    (security as *const SECURITY_CAPABILITIES).cast(),
                    size_of::<SECURITY_CAPABILITIES>(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            };
            if updated == 0 {
                let error = os_error("UpdateProcThreadAttribute");
                unsafe { DeleteProcThreadAttributeList(list) };
                return Err(error);
            }
            list
        }
    };

    let startup = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            // The EX size only when there is an attribute list to describe;
            // claiming it otherwise makes `CreateProcessW` read past a plain
            // `STARTUPINFOW`.
            cb: match attribute_list.is_null() {
                true => size_of::<STARTUPINFOW>() as u32,
                false => size_of::<STARTUPINFOEXW>() as u32,
            },
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: stdin.as_raw_handle(),
            hStdOutput: stdout_write.as_raw_handle(),
            hStdError: stderr_write.as_raw_handle(),
            ..unsafe { std::mem::zeroed() }
        },
        lpAttributeList: attribute_list,
    };

    // `cmd.exe /C` takes the remainder of the line verbatim, which is what the
    // `Command` path effectively did with `["/C", shell_command]`. Appended
    // rather than quoted on purpose: cmd's own quote handling after `/C` is not
    // MSVC's, and re-quoting here would change commands the user already types
    // successfully elsewhere in the app.
    //
    // The `set` prefix goes before it because there is nowhere else to put it —
    // see [`temp_reassignment`]. It ends in `&`, so the caller's command is still
    // the last thing on the line and still decides the exit code.
    let mut command_line = wide(&format!(
        "cmd.exe /C {}{shell_command}",
        temp_reassignment(env)
    ));
    let application = wide(&format!(
        "{}\\System32\\cmd.exe",
        std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string())
    ));
    let directory = wide_path(workspace_dir);
    // Only a container needs the extra keys, so a job-object-only run keeps the
    // caller's allowlist exactly as given.
    let mut block = environment_block(
        env,
        match container.is_some() {
            true => APP_CONTAINER_ENV_KEYS,
            false => &[],
        },
    );
    let mut process = PROCESS_INFORMATION {
        ..unsafe { std::mem::zeroed() }
    };

    // The EX flag only when there is a list for it to point at: setting it with a
    // null `lpAttributeList` is how you get a confusing ERROR_INVALID_PARAMETER
    // out of an otherwise correct call.
    let mut flags = CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW;
    if !attribute_list.is_null() {
        flags |= EXTENDED_STARTUPINFO_PRESENT;
    }
    // Safe: every pointer is to a live, correctly shaped, NUL-terminated buffer
    // owned by this scope, and `cb`/`flags` agree on which startup-info form this
    // is.
    let spawned = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            flags,
            block.as_mut_ptr().cast(),
            directory.as_ptr(),
            (&raw const startup).cast::<STARTUPINFOW>(),
            &mut process,
        )
    };
    if !attribute_list.is_null() {
        unsafe { DeleteProcThreadAttributeList(attribute_list) };
    }
    if spawned == 0 {
        return Err(os_error("CreateProcessW"));
    }
    // Safe: both handles come from a successful `CreateProcessW` and are owned
    // here from this point.
    let (child, thread) = unsafe {
        (
            OwnedHandle::from_raw_handle(process.hProcess),
            OwnedHandle::from_raw_handle(process.hThread),
        )
    };
    drop(thread);

    // The child now holds its own copies, and the parent's must go or the reads
    // below never see EOF.
    drop(stdout_write);
    drop(stderr_write);
    drop(stdin);

    job.assign_raw(process.hProcess)?;

    // `File` rather than `OwnedHandle` so the reads below are plain `io::Read`,
    // and because both are `Send` and can therefore cross an `await`.
    let stdout_file = unsafe { std::fs::File::from_raw_handle(stdout_read.into_raw_handle()) };
    let stderr_file = unsafe { std::fs::File::from_raw_handle(stderr_read.into_raw_handle()) };
    Ok((child, stdout_file, stderr_file))
}

/// See [`spawn_confined`] for why the raw work is not inlined here.
pub async fn run_confined(
    container: Option<&AppContainer>,
    job: &JobConfinement,
    shell_command: &str,
    workspace_dir: &Path,
    env: &[(String, String)],
    allow_network: bool,
    timeout: Duration,
) -> io::Result<ConfinedOutput> {
    let (child, mut stdout_file, mut stderr_file) = spawn_confined(
        container,
        job,
        shell_command,
        workspace_dir,
        env,
        allow_network,
    )?;

    let reader = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        let mut err = Vec::new();
        // Reported rather than swallowed. A failed read and a command that
        // printed nothing produce the same empty `Vec`, and on a platform that
        // can only be observed through a CI log that ambiguity is expensive: it
        // is what made #290's windows-latest leg unreadable, where a script whose
        // every step was guarded exited 0 — so it reached its trailing `dir /b`,
        // which cannot print nothing — and still came back `stdout="" stderr=""`.
        // Not an error path: a partial read is still the output we have, and the
        // exit code is the run's real result.
        if let Err(error) = stdout_file.read_to_end(&mut out) {
            eprintln!("sandbox: reading the confined child's stdout failed: {error}");
        }
        if let Err(error) = stderr_file.read_to_end(&mut err) {
            eprintln!("sandbox: reading the confined child's stderr failed: {error}");
        }
        (out, err)
    });

    // Moved as a `usize` because `HANDLE` is a raw pointer and so not `Send`,
    // which `spawn_blocking` requires. The handle itself outlives the task:
    // `child` owns it and is held until after the join below.
    let waiter_handle = child.as_raw_handle() as usize;
    let waited = tokio::task::spawn_blocking(move || {
        let handle = waiter_handle as HANDLE;
        unsafe { WaitForSingleObject(handle, INFINITE) };
        let mut code: u32 = 0;
        let read = unsafe { GetExitCodeProcess(handle, &mut code) };
        (read, code)
    });

    match tokio::time::timeout(timeout, waited).await {
        Ok(joined) => {
            let (read, code) = joined.map_err(io::Error::other)?;
            // Reading finishes on its own once the child's ends are closed,
            // which exiting does.
            let (stdout, stderr) = reader.await.map_err(io::Error::other)?;
            drop(child);
            Ok(ConfinedOutput {
                exit_code: (read != 0).then_some(code as i32),
                stdout,
                stderr,
                timed_out: false,
            })
        }
        Err(_) => {
            // The whole tree, not just `cmd.exe`, and closing the child's pipe
            // ends is what unblocks the reader.
            job.terminate();
            let (stdout, stderr) = reader.await.unwrap_or_default();
            drop(child);
            Ok(ConfinedOutput {
                exit_code: None,
                stdout,
                stderr,
                timed_out: true,
            })
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

/// Whether this machine can give a sandboxed run a filesystem boundary.
///
/// Same reasoning as [`job_objects_are_enforceable`], and just as much a probe:
/// AppContainer support is present on every supported Windows version, but
/// creating a profile writes to the user's registry hive and group policy can
/// refuse it. Creates a container and drops it, which deletes the profile again.
///
/// # Why this one is answered once and under a unique name
///
/// The two halves fix the same bug from both ends, and it was a real one:
/// `security_doctor`'s isolation test compares the audit's finding against this
/// probe, and on a parallel test run the two calls disagreed —
/// `isolation.process_contained` against an expected `isolation.os_enforced`.
///
/// The cause is that a profile name is a **machine-global** resource and this
/// probe used a fixed one. [`create_app_container`] recovers from
/// `ERROR_ALREADY_EXISTS` by deleting the existing profile and retrying, which
/// is right for a run whose predecessor died before its `Drop` — and wrong
/// between two live probes, because each one deletes the other's profile and
/// then the loser's `Drop` deletes the winner's. Whoever lost that race
/// reported "no AppContainer" on a machine that has them.
///
/// So: a name unique to this process, so two Little Monkey instances probing at
/// the same moment cannot collide; and a `OnceLock`, so within a process the
/// registry is touched once and every caller afterwards gets the same answer.
/// The memo is also what makes the answer *stable*, which is what the audit and
/// the Sandbox panel are compared against — a probe that can return two values
/// in one process is a disagreement waiting to be rendered.
pub fn app_containers_are_enforceable() -> bool {
    static ENFORCEABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENFORCEABLE.get_or_init(|| {
        // Not a constant: the tag is filtered to alphanumerics and truncated, so
        // the pid keeps this out of the way of another instance's probe and of
        // every real run, whose tag is a run id.
        create_app_container(&format!("probe{}", std::process::id())).is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Concurrent callers must get the same answer.
    ///
    /// The test that would have caught the bug this memo exists for: a profile
    /// name is machine-global, and `create_app_container` recovers from
    /// `ERROR_ALREADY_EXISTS` by deleting and retrying — so two probes under one
    /// fixed name deleted each other's profile and disagreed about whether this
    /// machine has AppContainers at all. `security_doctor`'s isolation test hit
    /// exactly that, comparing its own probe against the audit's.
    ///
    /// Threads rather than a loop, because a loop passes with or without the
    /// memo. This is only a real check while the calls can overlap.
    #[test]
    fn concurrent_appcontainer_probes_agree() {
        let answers: Vec<bool> = (0..8)
            .map(|_| std::thread::spawn(app_containers_are_enforceable))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().expect("the probe does not panic"))
            .collect();
        assert!(
            answers.windows(2).all(|pair| pair[0] == pair[1]),
            "the probe returned {answers:?} — a machine either has AppContainers or does not"
        );
    }

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

    /// The point of the whole module: a confined command can write inside the
    /// sandbox root and cannot touch a file outside it.
    ///
    /// Both halves matter. Only asserting the denial would pass just as well if
    /// the container were so tight that nothing ran at all, which is why the same
    /// run also has to produce the file it is allowed to produce.
    ///
    /// The outside path is under the user's own profile, reachable by this test
    /// process and by any unconfined child — so if the assertion fails, it fails
    /// because the boundary is not there.
    #[tokio::test]
    async fn a_confined_command_can_write_inside_the_sandbox_and_not_outside() {
        let root = std::env::temp_dir().join(format!("lm-ac-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("lm-ac-escape-{}.txt", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&outside);
        std::fs::create_dir_all(&root).expect("sandbox root");

        let container = match create_app_container("boundarytest") {
            Ok(container) => container,
            // A machine where policy forbids a profile is the degraded path the
            // caller already reports honestly; there is no boundary here to test.
            Err(_) => return,
        };
        container
            .grant_tree_access(&root)
            .expect("grant the sandbox root to the container");
        let job = create_job().expect("job");

        let inside_file = root.join("allowed.txt");
        let command = format!(
            "echo confined> \"{}\" & echo escaped> \"{}\"",
            inside_file.display(),
            outside.display()
        );
        let output = run_confined(
            Some(&container),
            &job,
            &command,
            &root,
            &[(
                "SystemRoot".to_string(),
                std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string()),
            )],
            false,
            Duration::from_secs(60),
        )
        .await
        .expect("the confined run itself must work");

        assert!(
            inside_file.is_file(),
            "the granted tree must be writable, or this proves nothing about the denial. \
             stdout={:?} stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !outside.exists(),
            "a confined command wrote outside the sandbox root"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&outside);
    }

    /// A confined run must return what the child printed.
    ///
    /// The assertion this module never made, and the one whose absence left #290
    /// undiagnosable. Every test here reads a file or an exit code, so a run that
    /// loses stdout entirely passes all of them — and that is what
    /// windows-latest was doing: a script guarded step by step exited 0, meaning
    /// it reached its trailing `dir /b` over two directories it had just written
    /// into, and the run still reported `stdout="" stderr=""`.
    ///
    /// Two arms, and the pair is the point. The job-only arm is the control: it
    /// shares every line of pipe plumbing with the container arm and differs only
    /// in the `STARTUPINFOEX` attribute list and the token, so which arm fails
    /// says where to look. Control red → the pipes were never right on either
    /// path. Control green, container red → the AppContainer is what the
    /// inherited write end does not survive, and the next question is the token,
    /// not `CreatePipe`.
    ///
    /// Both streams in one run, so neither can pass by accident of the other:
    /// `1>&2` puts the second `echo` on stderr.
    #[tokio::test]
    async fn a_confined_run_returns_what_the_child_printed() {
        let root = std::env::temp_dir().join(format!("lm-ac-io-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("sandbox root");
        let env = vec![(
            "SystemRoot".to_string(),
            std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string()),
        )];
        let command = "echo on-stdout & echo on-stderr 1>&2";

        let control = run_confined(
            None,
            &create_job().expect("job"),
            command,
            &root,
            &env,
            false,
            Duration::from_secs(60),
        )
        .await
        .expect("the job-only run itself must work");
        assert!(
            String::from_utf8_lossy(&control.stdout).contains("on-stdout")
                && String::from_utf8_lossy(&control.stderr).contains("on-stderr"),
            "a job-only run lost the child's output, so the pipes are wrong before any \
             container is involved: stdout={:?} stderr={:?} exit={:?}",
            String::from_utf8_lossy(&control.stdout),
            String::from_utf8_lossy(&control.stderr),
            control.exit_code
        );

        // A machine without containers has only the control arm to offer, same as
        // the boundary test above — but a skip and a pass are the same colour, and
        // the conclusion drawn from a green run here is precisely "the container
        // arm captured its output", so on CI the skip has to be the failure.
        match create_app_container("iotest") {
            Err(error) => assert!(
                std::env::var_os("CI").is_none(),
                "no AppContainer on this CI runner, so the arm this test exists for \
                 never ran: {error}"
            ),
            Ok(container) => {
                container.grant_tree_access(&root).expect("grant");
                let confined = run_confined(
                    Some(&container),
                    &create_job().expect("job"),
                    command,
                    &root,
                    &env,
                    false,
                    Duration::from_secs(60),
                )
                .await
                .expect("the confined run itself must work");
                assert!(
                    String::from_utf8_lossy(&confined.stdout).contains("on-stdout")
                        && String::from_utf8_lossy(&confined.stderr).contains("on-stderr"),
                    "an AppContainer run lost the child's output while the job-only run \
                     kept it, so the container is what the inherited pipe write end \
                     does not survive: stdout={:?} stderr={:?} exit={:?}",
                    String::from_utf8_lossy(&confined.stdout),
                    String::from_utf8_lossy(&confined.stderr),
                    confined.exit_code
                );
            }
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A confined run must keep working when the caller sets it up the way
    /// `sandbox::execute_in_sandbox` does, not just the way the tests above do.
    ///
    /// The tests above prove `run_confined` in isolation — one granted directory,
    /// a two-key environment, a working directory that *is* the granted root, and
    /// a command with no variables in it. `execute_in_sandbox` differs from that
    /// in four ways at once, and #290's windows-latest failure lives somewhere in
    /// the gap: it reports exit 0 with both streams empty, which for a command
    /// ending in `dir /b` over two directories means the child ran nothing at all
    /// (`dir` is not silent on either stream, even when it fails).
    ///
    /// So this adds those four differences one at a time, and each rung asserts
    /// the same trivially-true thing — the child printed its marker. The first
    /// rung that goes red names the difference that breaks the run, which is
    /// what a single CI run on this platform has to buy.
    #[tokio::test]
    async fn a_confined_run_survives_the_setup_its_real_caller_uses() {
        let root = std::env::temp_dir().join(format!("lm-ac-caller-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // Created before the grant below, exactly as `execute_in_sandbox` creates
        // its home and tmp before asking for the container: an inheritable ACE
        // reaches existing children only if `SetNamedSecurityInfoW` propagates
        // it, and nothing here has ever tested that it does.
        let workspace = root.join("workspace");
        let home = root.join("home");
        let tmp = root.join("tmp");
        for dir in [&workspace, &home, &tmp] {
            std::fs::create_dir_all(dir).expect("sandbox subdirectory");
        }

        let Ok(container) = create_app_container("callertest") else {
            assert!(
                std::env::var_os("CI").is_none(),
                "no AppContainer on this CI runner, so nothing this test exists for ran"
            );
            return;
        };
        container.grant_tree_access(&root).expect("grant");

        let system_root = (
            "SystemRoot".to_string(),
            std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string()),
        );
        // What `sandbox::allowlisted_env` produces, taken from the function itself
        // rather than restated, so a change there cannot leave this rung testing
        // an environment the caller no longer sends.
        let full_env = crate::sandbox::allowlisted_env(&home, &tmp, &[]);

        let rungs: [(&str, std::path::PathBuf, Vec<(String, String)>, &str); 4] = [
            // The baseline the tests above already prove, re-run here only so a
            // red rung below is a difference and not the whole mechanism.
            (
                "granted root as cwd",
                root.clone(),
                vec![system_root.clone()],
                "echo marker",
            ),
            // 1: cwd is a pre-existing subdirectory of the granted tree.
            (
                "subdirectory as cwd",
                workspace.clone(),
                vec![system_root.clone()],
                "echo marker",
            ),
            // 2: the caller's full environment, whose PATH names directories this
            //    container has no grant on, and whose TMP/TEMP point inside it.
            (
                "caller's environment",
                workspace.clone(),
                full_env.clone(),
                "echo marker",
            ),
            // 3: the command reads those variables and writes through them, which
            //    is every remaining difference from #290's script.
            (
                "variables and a write through them",
                workspace.clone(),
                full_env.clone(),
                "echo marker & (echo probe> \"%USERPROFILE%\\probe\") \
                 & if not exist \"%USERPROFILE%\\probe\" exit /b 74 \
                 & dir /b \"%USERPROFILE%\"",
            ),
        ];

        for (label, cwd, env, command) in rungs {
            let output = run_confined(
                Some(&container),
                &create_job().expect("job"),
                command,
                &cwd,
                &env,
                false,
                Duration::from_secs(60),
            )
            .await
            .expect("the confined run itself must work");
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("marker"),
                "a confined run printed nothing once the caller's setup added \
                 \"{label}\", so that is the difference #290 is failing on: \
                 exit={:?} stdout={:?} stderr={:?}",
                output.exit_code,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // The other half of #290's failure, and a separate claim from the one
        // above: the container's own `if not exist` guard passed on the last rung,
        // so the file exists as far as the child is concerned. Whether the host
        // still sees it afterwards is what #290 asserts and what fails there.
        assert!(
            home.join("probe").is_file(),
            "the child's own guard found the probe it wrote under {}, but the host \
             cannot see it afterwards; home contains [{}]",
            home.display(),
            match std::fs::read_dir(&home) {
                Ok(entries) => entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(", "),
                Err(error) => format!("<unreadable: {error}>"),
            }
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A confined run's temporary directory must be the sandbox's own.
    ///
    /// Windows rewrites `TMP` and `TEMP` on the way into an AppContainer to the
    /// container's package temp, `…\Packages\<container>\AC\Temp`, whatever the
    /// environment block says — so this is the one sandbox-owned path that cannot
    /// be delivered by [`environment_block`] and is restored by
    /// [`temp_reassignment`] instead.
    ///
    /// Worth its own test rather than a line in the ladder above, because the
    /// failure it guards is silent in both directions. The child still gets a
    /// writable temp directory, so nothing errors; the files simply land outside
    /// the tree the caller cleans up and inside the profile [`AppContainer::drop`]
    /// deletes, and are gone by the time anyone looks. Asserting the variable is
    /// not enough on its own either — a run could report the right path and still
    /// write somewhere else — so the write is followed from the host side.
    #[tokio::test]
    async fn a_confined_run_gets_the_sandbox_temp_directory_not_the_containers() {
        let root = std::env::temp_dir().join(format!("lm-ac-tmp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let tmp = root.join("tmp");
        std::fs::create_dir_all(&tmp).expect("sandbox tmp");

        let Ok(container) = create_app_container("tmptest") else {
            assert!(
                std::env::var_os("CI").is_none(),
                "no AppContainer on this CI runner, so the rewrite this test exists \
                 for never happened"
            );
            return;
        };
        container.grant_tree_access(&root).expect("grant");

        // Only the temp pair, so a pass cannot come from some other variable
        // happening to name the right directory.
        let env = vec![
            (
                "SystemRoot".to_string(),
                std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string()),
            ),
            ("TMP".to_string(), tmp.to_string_lossy().into_owned()),
            ("TEMP".to_string(), tmp.to_string_lossy().into_owned()),
        ];
        // `set TMP`, not `echo %TMP%`, and a path written out in full rather than
        // through the variable. cmd expands `%VAR%` when it *parses the line*, so
        // every `%TMP%` on this line is replaced before [`temp_reassignment`]'s
        // own `set` at the front of it has run — the probe would report, and the
        // write would use, exactly the value this test exists to prove is gone.
        // `set <prefix>` has no such problem: it prints what the environment holds
        // when it runs. (It also prints `TMPDIR` if the caller set one; harmless,
        // and `TMP=` is not a substring of `TMPDIR=`.)
        let command = format!(
            "set TMP & set TEMP & echo written> \"{}\\probe\"",
            tmp.display()
        );
        let output = run_confined(
            Some(&container),
            &create_job().expect("job"),
            &command,
            &root,
            &env,
            false,
            Duration::from_secs(60),
        )
        .await
        .expect("the confined run itself must work");

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let reported = format!(
            "exit={:?} stdout={stdout:?} stderr={:?}",
            output.exit_code,
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains(&format!("TMP={}", tmp.display()))
                && stdout.contains(&format!("TEMP={}", tmp.display())),
            "the child's temp directory is not the sandbox's — Windows rewrote it \
             and it was not put back, so this run's temporary files are outside the \
             sandbox tree and inside a profile that is about to be deleted: wanted \
             {}; {reported}",
            tmp.display()
        );
        // The claim the variable alone does not make: a write through it lands
        // somewhere the host still has after the container is gone.
        assert!(
            tmp.join("probe").is_file(),
            "the child wrote through its temp directory and the host cannot see the \
             file afterwards: {reported}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A confined run can enumerate the tree it was granted — and cannot reach the
    /// volume, which is why `dir` says otherwise.
    ///
    /// #290 recorded the opposite and could not explain it: its child wrote a
    /// probe into its own home and its own tmp — both writes silent, both files
    /// there for the host afterwards — and then `dir /b` over those same two
    /// directories answered "Access is denied." twice. Listing your own temp
    /// directory is an ordinary thing for a build to do, so that looked like a
    /// hole in the boundary's *allow* half.
    ///
    /// It is not, and this test is the argument. `E3-dotnet` enumerates the
    /// sandbox root through `System.IO` and gets every entry; `E2-attrib`
    /// enumerates a subdirectory through `FindFirstFile`, the same call `dir`
    /// makes. The grant reaches enumeration exactly as it says it does.
    ///
    /// What `dir` cannot reach is the *volume*: `E4-vol` is `vol C:`, and it is
    /// refused. cmd's `DIR` reads the volume before it lists anything — `/b`
    /// suppresses the header, not the query — and
    /// [`AppContainer::grant_tree_access`] grants one directory tree and
    /// deliberately nothing above it, so `C:\` is granted to nothing this
    /// container holds. That is why `dir` fails on *every* path, including
    /// `System32\drivers\etc`, which this app never touches and Windows grants to
    /// `ALL APPLICATION PACKAGES`.
    ///
    /// So `E4-vol` is the assertion worth keeping and the reason the others are
    /// safe. Granting the volume root would make `dir` work and would put an ACE
    /// on `C:\` for every sandboxed run, which is a boundary regression and not a
    /// fix — a red `E4-vol` is that regression, caught at the one place it is
    /// visible. `E5-dir` is carried in the transcript rather than asserted: it is
    /// a `cmd` behaviour, not a boundary claim, and a Windows that changed it
    /// should not turn this red.
    ///
    /// Ruled out along the way, none of it worth re-testing. Not propagation —
    /// `before` exists when the grant runs and carries the inherited ACE anyway,
    /// and `after`, which inherits at creation, behaves identically. Not the mask
    /// — Windows renders the grant as `(F)`. Not the token — `whoami /priv` shows
    /// `SeChangeNotifyPrivilege` enabled, which is what the module docs rest on,
    /// and `icacls` run from *inside* prints the root's ACL, so the child can open
    /// that directory and read its security descriptor.
    ///
    /// No `if`, no `(…)`, no `exit /b`, and each step labelled with its stderr
    /// merged into stdout before any other redirection — for the reasons #290
    /// wrote down the hard way. The verdict is the transcript, not the exit code.
    #[tokio::test]
    async fn a_confined_run_can_enumerate_the_tree_it_was_granted() {
        let root = std::env::temp_dir().join(format!("lm-ac-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // Created before the grant, exactly as `execute_in_sandbox` creates its
        // home and tmp before it has a container to grant them to. The ACE reaches
        // it by propagation rather than by inheritance at creation, and the
        // listing below is what says so.
        let before = root.join("before");
        std::fs::create_dir_all(&before).expect("pre-grant subdirectory");
        std::fs::write(before.join("before-marker"), "x").expect("pre-grant marker");

        let Ok(container) = create_app_container("listtest") else {
            assert!(
                std::env::var_os("CI").is_none(),
                "no AppContainer on this CI runner, so nothing this test exists for ran"
            );
            return;
        };
        container.grant_tree_access(&root).expect("grant");

        std::fs::write(root.join("root-marker"), "x").expect("root marker");

        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
        let written = root.join("by-container");
        let command = format!(
            "echo E1-write & echo written-by-container 2>&1> \"{written}\" \
             & echo E2-attrib & attrib \"{before}\\*\" 2>&1 \
             & echo E3-dotnet & powershell -NoProfile -NonInteractive -Command \
               \"[IO.Directory]::GetFileSystemEntries('{root}') -join ','\" 2>&1 \
             & echo E4-vol & vol C: 2>&1 \
             & echo E5-dir & dir /b \"{root}\" 2>&1 \
             & echo LIST-END",
            root = root.display(),
            before = before.display(),
            written = written.display(),
        );

        let output = run_confined(
            Some(&container),
            &create_job().expect("job"),
            &command,
            &root,
            &[
                ("SystemRoot".to_string(), system_root.clone()),
                (
                    "PATH".to_string(),
                    format!(
                        "{system_root}\\System32;{system_root}\\System32\\WindowsPowerShell\\v1.0"
                    ),
                ),
            ],
            false,
            Duration::from_secs(60),
        )
        .await
        .expect("the confined run itself must work");

        // What the host sees of the same ACLs. A listing the container is refused
        // while the host reads a container ACE on the directory is a different
        // finding from one where the ACE is not there, and one CI run on this
        // platform has to be able to tell them apart.
        let host_acl = |path: &Path| match std::process::Command::new("icacls").arg(path).output() {
            Ok(output) => String::from_utf8_lossy(&output.stdout).replace("\r\n", " | "),
            Err(error) => format!("<icacls failed: {error}>"),
        };
        let report = format!(
            "exit={:?} stdout={:?} stderr={:?}\nhost sees by-container: {}\n\
             host icacls root: {}\nhost icacls before: {}",
            output.exit_code,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            written.is_file(),
            host_acl(&root),
            host_acl(&before),
        );
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            stdout.contains("E1-write") && stdout.contains("LIST-END"),
            "the confined child did not run its script from start to finish, so no rung \
             below is a statement about enumeration: {report}"
        );
        // Each rung is read inside its own slice of the transcript, between its
        // label and the next one. Searching the whole of stdout would let one green
        // rung answer for another that looks for the same marker.
        let section = |rung: &str, next: &str| {
            stdout
                .split_once(rung)
                .and_then(|(_, rest)| rest.split_once(next))
                .map(|(section, _)| section.to_string())
                .unwrap_or_default()
        };
        // The write half, and the baseline the listing rungs are measured against:
        // a run that cannot write in its own tree has nothing to say about whether
        // it can list it. Checked from the host, because a file only the container
        // can see is a file the run has lost.
        assert!(
            written.is_file(),
            "the confined run could not create a file in the tree it was granted, so \
             this run says nothing about listing versus writing: {report}"
        );
        for (rung, next, marker) in [
            ("E2-attrib", "E3-dotnet", "before-marker"),
            ("E3-dotnet", "E4-vol", "root-marker"),
        ] {
            assert!(
                section(rung, next).contains(marker),
                "{rung}: a confined run could not enumerate a directory inside the tree \
                 it was granted, while it could create a file in that same tree — so the \
                 grant does not reach enumeration and a sandboxed command cannot list \
                 its own temp directory: {report}"
            );
        }
        // The deny half, and the reason `E5-dir` is refused. An ACE on `C:\` would
        // make `dir` work and would be on the volume root for every sandboxed run.
        assert!(
            section("E4-vol", "E5-dir").contains("Access is denied."),
            "a confined run can read the volume, so something has granted this \
             container the drive root rather than one tree under it — which is the \
             whole boundary, not a widening of it: {report}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Network is denied unless the run asked for it, which for an AppContainer is
    /// the absence of a capability rather than a filter.
    ///
    /// Asserted through the exit status of a real request rather than by
    /// inspecting the token: what matters is that the connection does not happen.
    /// Loopback is deliberately not used here — it stays blocked even when
    /// network is allowed (see the module docs), so it cannot tell the two cases
    /// apart.
    #[tokio::test]
    async fn a_confined_run_without_network_cannot_resolve_or_connect() {
        let root = std::env::temp_dir().join(format!("lm-ac-net-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("sandbox root");
        let Ok(container) = create_app_container("nettest") else {
            return;
        };
        container.grant_tree_access(&root).expect("grant");
        let job = create_job().expect("job");
        let env = vec![(
            "SystemRoot".to_string(),
            std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string()),
        )];

        // `-n 1` so a denied lookup fails fast rather than retrying.
        let denied = run_confined(
            Some(&container),
            &job,
            "ping -n 1 example.com",
            &root,
            &env,
            false,
            Duration::from_secs(60),
        )
        .await
        .expect("the run itself must work");
        assert_ne!(
            denied.exit_code,
            Some(0),
            "a network-denied run reached the network: {}",
            String::from_utf8_lossy(&denied.stdout)
        );

        let _ = std::fs::remove_dir_all(&root);
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
