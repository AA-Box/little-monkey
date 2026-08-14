//! Kernel-held limits on Windows, through the job object the shell already owns.
//!
//! This is deliberately **not** a second job-object implementation.
//! `sandbox_windows` already creates a job, assigns a suspended process to it and
//! only then resumes the thread — the ordering K4 requires, built for the shell
//! boundary. What was missing is that its numbers were fixed constants, so a job
//! bounded a tree at 4 GiB whatever the process's `ProcessLimits` said, and
//! nothing read the job's accounting back.
//!
//! So this module owns two things and delegates the rest:
//!
//! 1. **Deriving the job's numbers from [`EffectiveLimits`]** — see
//!    [`crate::sandbox_windows::job_limits_from`], which intersects the effective
//!    limit with the fixed guardrail. The guardrail survives as an independent
//!    ceiling; it stops standing in for the caller's policy.
//! 2. **Reading the job back** — `ActiveProcesses` and `PeakJobMemoryUsed` are
//!    the kernel's own accounting for the whole tree, which is strictly better
//!    than the parent-link walk the supervisor has to do.
//!
//! # What a job memory limit does, and what that means for the exit
//!
//! `JOB_OBJECT_LIMIT_JOB_MEMORY` makes *allocation fail* inside the job rather
//! than terminating the process, and `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` makes
//! `CreateProcess` fail rather than killing anything. Both are real kernel-held
//! bounds — the tree cannot exceed either — and both are invisible to a
//! supervisor comparing a measurement against a budget, because the kernel's
//! whole job is to stop the measurement ever passing the budget. A tree capped at
//! twelve processes reports twelve, forever, while the thirteenth `CreateProcess`
//! fails and the workload dies of an error it cannot explain.
//!
//! So the job is associated with an **I/O completion port** at creation, and the
//! kernel posts `JOB_OBJECT_MSG_ACTIVE_PROCESS_LIMIT` and
//! `JOB_OBJECT_MSG_JOB_MEMORY_LIMIT` to it at the moment it refuses. That is the
//! evidence [`crate::resource_control::LimitEvent`] exists to carry: the kernel
//! holds the bound, and this is how the app can name which bound it held instead
//! of recording an unexplained crash.

#![cfg(windows)]

use std::io;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::JobObjects::{
    JobObjectAssociateCompletionPortInformation, SetInformationJobObject,
    JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
};
use windows_sys::Win32::System::SystemServices::{
    JOB_OBJECT_MSG_ACTIVE_PROCESS_LIMIT, JOB_OBJECT_MSG_JOB_MEMORY_LIMIT,
    JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT,
};
use windows_sys::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED,
};

use crate::process_table::ProcessLimitKind;
use crate::resource_control::{
    ControllerCapabilities, EffectiveLimits, EnforcementLevel, LimitCapability, LimitEvent,
};
use crate::sandbox_windows::{JobConfinement, JobLimits};

/// The port the kernel posts this job's limit notifications to.
///
/// Owned rather than borrowed because the notification is only useful while the
/// workload runs, and a job may be associated with exactly one completion port
/// for its whole lifetime — so this is created with the job and closed with it.
struct LimitNotifications {
    port: HANDLE,
}

// The handle is owned outright and only passed to completion-port syscalls,
// which take it by value and are thread-safe. Needed for the same reason
// `JobConfinement` needs it: the controller is held across awaits.
unsafe impl Send for LimitNotifications {}
unsafe impl Sync for LimitNotifications {}

impl Drop for LimitNotifications {
    fn drop(&mut self) {
        // Safe: closes the handle this type owns, exactly once. Nothing useful
        // remains to be done about a handle that will not close.
        unsafe {
            let _ = CloseHandle(self.port);
        }
    }
}

impl LimitNotifications {
    /// Create a port and tell the job to post its limit messages to it.
    fn associate(job: &JobConfinement) -> io::Result<Self> {
        // A fresh, unattached port: `INVALID_HANDLE_VALUE` for the file means
        // "create one rather than associate a handle with an existing one", and
        // one concurrent thread is all a non-blocking drain needs.
        // Safe: creates a kernel object and returns its handle or null.
        let port =
            unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
        if port.is_null() {
            return Err(io::Error::last_os_error());
        }
        let notifications = LimitNotifications { port };
        let associate = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
            // Nothing distinguishes one job from another here — this controller
            // owns exactly one — so the key carries no meaning and is zero.
            CompletionKey: std::ptr::null_mut(),
            CompletionPort: port,
        };
        // Safe: a fully initialised struct of exactly the type this information
        // class expects, sized from that same type.
        let ok = unsafe {
            SetInformationJobObject(
                job.raw_handle(),
                JobObjectAssociateCompletionPortInformation,
                (&raw const associate).cast(),
                u32::try_from(std::mem::size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>())
                    .expect("a Win32 struct size fits in u32"),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(notifications)
    }

    /// Take every limit message the kernel has posted since the last drain.
    ///
    /// Non-blocking: a zero timeout returns what is queued and nothing more, so
    /// this rides the supervisor's existing sampling tick rather than needing a
    /// thread of its own. Messages persist in the port until drained, so a
    /// refusal that happened between two ticks is still reported.
    fn drain(&self) -> Option<ProcessLimitKind> {
        let mut fired = None;
        loop {
            let mut message: u32 = 0;
            let mut key: usize = 0;
            let mut overlapped: *mut OVERLAPPED = std::ptr::null_mut();
            // Safe: writes three outputs this stack owns. A zero timeout cannot
            // block, and a false return with an empty queue is the normal exit.
            let ok = unsafe {
                GetQueuedCompletionStatus(self.port, &mut message, &mut key, &mut overlapped, 0)
            };
            if ok == 0 {
                return fired;
            }
            // For job notifications the "bytes transferred" field carries the
            // message id and the overlapped pointer carries the process id;
            // neither is a real transfer or a real OVERLAPPED.
            match message {
                JOB_OBJECT_MSG_ACTIVE_PROCESS_LIMIT => {
                    // The first refusal wins, and a process-count refusal is
                    // reported ahead of a memory one because it is the cause a
                    // memory message would then be a consequence of.
                    return Some(ProcessLimitKind::ChildProcesses);
                }
                JOB_OBJECT_MSG_JOB_MEMORY_LIMIT | JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT => {
                    fired = fired.or(Some(ProcessLimitKind::Memory));
                }
                // Process start/exit and end-of-job-time messages are not limits
                // this controller declared; draining them is what keeps the queue
                // from filling with them.
                _ => {}
            }
        }
    }
}

pub struct JobObject {
    job: JobConfinement,
    applied: JobLimits,
    /// `None` when the port could not be created or associated. The kernel bound
    /// still holds — this is the *naming* of it that is missing — so the
    /// controller degrades to the supervisor's comparison rather than failing the
    /// spawn over a diagnostic.
    notifications: Option<LimitNotifications>,
}

impl JobObject {
    pub fn create(limits: &EffectiveLimits) -> io::Result<Self> {
        let applied = JobLimits::from_effective(limits);
        let job = crate::sandbox_windows::create_job_with(applied)?;
        let notifications = LimitNotifications::associate(&job).ok();
        Ok(JobObject {
            job,
            applied,
            notifications,
        })
    }

    /// What the kernel says it refused, if anything.
    ///
    /// See this module's header for why the comparison in
    /// [`crate::resource_control::ResourceController::breach`] cannot see either
    /// of these.
    pub fn poll_limit_events(&self) -> io::Result<Option<LimitEvent>> {
        let Some(notifications) = &self.notifications else {
            return Ok(None);
        };
        let Some(limit) = notifications.drain() else {
            return Ok(None);
        };
        let accounting = self.job.accounting()?;
        Ok(Some(match limit {
            ProcessLimitKind::ChildProcesses => LimitEvent {
                limit,
                // The job was held *at* the ceiling, which is what refusing the
                // next `CreateProcess` means.
                observed: u64::from(accounting.active_processes),
                evidence: "JOB_OBJECT_MSG_ACTIVE_PROCESS_LIMIT, the kernel refusing a process \
                           creation at the job's active-process ceiling"
                    .to_string(),
            },
            _ => LimitEvent {
                limit: ProcessLimitKind::Memory,
                observed: accounting.job_memory_bytes,
                evidence: "JOB_OBJECT_MSG_JOB_MEMORY_LIMIT, the kernel refusing a commit at the \
                           job's memory ceiling"
                    .to_string(),
            },
        }))
    }

    /// Hand the job to a spawn site that will assign the suspended process to it.
    ///
    /// The controller keeps a duplicate handle so it can still sample and
    /// terminate: a job stays alive while *any* handle is open, and
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` fires when the last one closes, so
    /// duplicating extends containment rather than weakening it.
    pub fn duplicate_for_spawn(&self) -> io::Result<JobConfinement> {
        self.job.duplicate()
    }

    /// Assign a process this controller did not create into the job.
    ///
    /// See [`crate::resource_control::ResourceController::adopt`]: this is the
    /// weaker ordering, for an owner with no pre-creation hook, and
    /// [`Self::confirm_assignment`] is still what decides whether the workload
    /// runs.
    pub fn adopt(&self, pid: u32) -> io::Result<()> {
        self.job.assign_pid(pid)
    }

    /// A process that reached the spawn site but never got assigned would run
    /// outside the bound while the record claimed it was inside one.
    pub fn confirm_assignment(&self, pid: u32) -> io::Result<()> {
        if self.job.contains(pid)? {
            return Ok(());
        }
        Err(io::Error::other(format!(
            "process {pid} started without being assigned to its job object, so no kernel bound \
             is holding it"
        )))
    }

    /// How this job reports a refusal, stated rather than assumed: a host where
    /// the completion port could not be associated still has the kernel bound and
    /// has lost the ability to name it, and a capability answer that did not say
    /// so would be claiming a diagnostic it does not have.
    fn notification_mechanism(&self) -> &'static str {
        match self.notifications {
            Some(_) => "the job's completion port reporting the refusal",
            None => {
                "no completion port available on this host, so the refusal is named only \
                     where the supervisor's own measurement catches it"
            }
        }
    }

    pub fn capabilities(&self) -> ControllerCapabilities {
        ControllerCapabilities {
            backend: "windows job object".to_string(),
            tree_primitive: "Windows job object with JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE — every \
                             descendant is a member by kernel rule and cannot detach"
                .to_string(),
            // No job limit expresses wall clock. `JOB_OBJECT_LIMIT_JOB_TIME` is
            // CPU time, which accumulates per core and is a different policy, so
            // claiming it as a wall bound would be wrong in the direction that
            // kills healthy parallel builds.
            wall: LimitCapability::Enforced {
                level: EnforcementLevel::Supervised,
                mechanism: "the sampling loop compares elapsed time against the effective wall \
                            limit; a job object has no wall-clock limit, only CPU time"
                    .to_string(),
            },
            memory: LimitCapability::Enforced {
                level: EnforcementLevel::Kernel,
                mechanism: format!(
                    "JOB_OBJECT_LIMIT_JOB_MEMORY at {} bytes, with {} so a refused commit is \
                     recorded as the limit it was and not as a crash",
                    self.applied.memory_bytes,
                    self.notification_mechanism()
                ),
            },
            child_processes: LimitCapability::Enforced {
                level: EnforcementLevel::Kernel,
                mechanism: format!(
                    "JOB_OBJECT_LIMIT_ACTIVE_PROCESS at {}, counted per job rather than per user, \
                     with {}",
                    self.applied.active_processes,
                    self.notification_mechanism()
                ),
            },
            output: LimitCapability::Enforced {
                level: EnforcementLevel::Supervised,
                mechanism: "the capture buffer is bounded as bytes arrive; a pipe is not a job \
                            resource"
                    .to_string(),
            },
            context_tokens: LimitCapability::NotApplicable {
                reason: "a job object bounds an OS process tree; a context budget is enforced at \
                         the model request by the runtime that can count exactly"
                    .to_string(),
            },
        }
    }

    /// Nothing to install at `pre_exec` time: on Windows the containment is the
    /// job, and the job is applied by the spawn site between `CREATE_SUSPENDED`
    /// and `ResumeThread`.
    pub fn prepare_tokio(&self, _command: &mut tokio::process::Command) -> io::Result<()> {
        Ok(())
    }

    pub fn prepare_std(&self, _command: &mut std::process::Command) -> io::Result<()> {
        Ok(())
    }

    pub fn sample(&self) -> io::Result<Option<(Option<u64>, Option<u32>)>> {
        let accounting = self.job.accounting()?;
        // An empty job is a workload that exited, not a tree of zero processes
        // holding zero bytes.
        if accounting.active_processes == 0 {
            return Ok(None);
        }
        Ok(Some((
            Some(accounting.job_memory_bytes),
            Some(accounting.active_processes),
        )))
    }

    pub fn terminate_tree(&self) -> io::Result<()> {
        self.job.terminate_result()
    }
}
