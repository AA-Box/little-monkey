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
//! than terminating the process. That is a real, kernel-held bound — the tree
//! cannot exceed it — but on its own it produces a workload that dies of a failed
//! allocation, which reads as an ordinary crash. So the supervising loop still
//! samples the job and, when the committed total passes the effective limit,
//! terminates the tree and records `limit_exceeded`. The kernel holds the bound;
//! the supervisor names it.

#![cfg(windows)]

use std::io;

use crate::resource_control::{
    ControllerCapabilities, EffectiveLimits, EnforcementLevel, LimitCapability,
};
use crate::sandbox_windows::{JobConfinement, JobLimits};

pub struct JobObject {
    job: JobConfinement,
    applied: JobLimits,
}

impl JobObject {
    pub fn create(limits: &EffectiveLimits) -> io::Result<Self> {
        let applied = JobLimits::from_effective(limits);
        Ok(JobObject {
            job: crate::sandbox_windows::create_job_with(applied)?,
            applied,
        })
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
                    "JOB_OBJECT_LIMIT_JOB_MEMORY at {} bytes, with the supervisor terminating \
                     and naming the breach so it is not mistaken for a crash",
                    self.applied.memory_bytes
                ),
            },
            child_processes: LimitCapability::Enforced {
                level: EnforcementLevel::Kernel,
                mechanism: format!(
                    "JOB_OBJECT_LIMIT_ACTIVE_PROCESS at {}, counted per job rather than per user",
                    self.applied.active_processes
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
        self.job.terminate()
    }
}
