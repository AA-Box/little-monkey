use std::collections::HashMap;
use std::fs::OpenOptions;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use little_monkey_lib::run_protocol::{
    ClientIdentity, ClientKind, PermissionDecision, RepositoryPolicy, RunEvent, RunStatus,
};

use crate::durable_run::{bounded_text, CliRunEventSink, DurableRunRecorder};

use super::ledger::{LeaseToken, SharedLedger};
use super::store::{map_run_status, DaemonConfig, DaemonJob, DaemonPaths, DaemonStore, JobState};
use super::worktree::OwnedWorktree;

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_millis()).ok())
            .unwrap_or(1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonNotification {
    pub run_id: String,
    pub title: String,
    pub body: String,
}

pub trait NotificationAdapter: Send + Sync {
    fn notify(&self, notification: &DaemonNotification) -> Result<(), String>;
}

pub struct OsNotificationAdapter;

impl NotificationAdapter for OsNotificationAdapter {
    fn notify(&self, notification: &DaemonNotification) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        {
            let script = format!(
                "display notification {} with title {}",
                apple_script_string(&notification.body),
                apple_script_string(&notification.title)
            );
            return command_ok("osascript", &["-e", &script]);
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            return command_ok("notify-send", &[&notification.title, &notification.body]);
        }
        #[cfg(windows)]
        {
            let script = format!(
                "$ws=New-Object -ComObject WScript.Shell; $null=$ws.Popup({},5,{},64)",
                powershell_string(&notification.body),
                powershell_string(&notification.title)
            );
            return command_ok(
                "powershell",
                &["-NoProfile", "-NonInteractive", "-Command", &script],
            );
        }
    }
}

#[cfg(target_os = "macos")]
fn apple_script_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(windows)]
fn powershell_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn command_ok(program: &str, args: &[&str]) -> Result<(), String> {
    Command::new(program)
        .args(args)
        .status()
        .map_err(|error| format!("Failed to launch notification adapter: {error}"))
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(format!("Notification adapter exited with {status}"))
            }
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessSignal {
    Pause,
    Resume,
    Terminate,
}

pub trait ManagedProcess: Send {
    fn id(&self) -> u32;
    fn try_wait(&mut self) -> Result<Option<i32>, String>;
    fn signal(&mut self, signal: ProcessSignal) -> Result<(), String>;
    fn memory_bytes(&self) -> Result<Option<u64>, String>;
}

pub trait ProcessAdapter: Send + Sync {
    fn spawn(
        &self,
        job: &DaemonJob,
        paths: &DaemonPaths,
    ) -> Result<Box<dyn ManagedProcess>, String>;
    fn terminate_orphan(&self, process_id: u32) -> Result<(), String>;
}

pub struct RealProcessAdapter {
    executable: std::path::PathBuf,
}

impl RealProcessAdapter {
    pub fn current() -> Result<Self, String> {
        Ok(Self {
            executable: std::env::current_exe()
                .map_err(|error| format!("Could not resolve monkey executable: {error}"))?,
        })
    }
}

impl ProcessAdapter for RealProcessAdapter {
    fn spawn(
        &self,
        job: &DaemonJob,
        paths: &DaemonPaths,
    ) -> Result<Box<dyn ManagedProcess>, String> {
        let log_path = paths.logs.join(format!("{}.log", job.job_id));
        let log = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)
            .map_err(|error| format!("Failed to open daemon run log: {error}"))?;
        super::store::restrict_file(&log_path)?;
        let stderr = log
            .try_clone()
            .map_err(|error| format!("Failed to clone daemon log handle: {error}"))?;
        let mut command = Command::new(&self.executable);
        command
            .arg("--no-rules")
            .arg("task")
            .arg("run")
            .arg(&job.recipe_snapshot)
            .arg("--run-key")
            .arg(format!("daemon:{}", job.job_id))
            .arg("--json")
            .env_remove("LITTLE_MONKEY_TASK_QUEUE_ONLY")
            .env("LITTLE_MONKEY_DAEMON_APPROVAL_WAIT", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
        if let Some(policy) = &job.repository_policy_json {
            command.env("LITTLE_MONKEY_DAEMON_REPOSITORY_POLICY_JSON", policy);
            let allow_external = serde_json::from_str::<RepositoryPolicy>(policy)
                .map(|policy| {
                    policy.allow_push
                        || policy.allow_create_pull_request
                        || policy.allow_review_comment
                })
                .unwrap_or(false);
            if allow_external {
                command.env("LITTLE_MONKEY_DAEMON_ALLOW_EXTERNAL_MUTATIONS", "1");
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command
            .spawn()
            .map_err(|error| format!("Failed to launch daemon task child: {error}"))?;
        Ok(Box::new(RealManagedProcess { child }))
    }

    fn terminate_orphan(&self, process_id: u32) -> Result<(), String> {
        terminate_process_group(process_id)
    }
}

struct RealManagedProcess {
    child: Child,
}

impl ManagedProcess for RealManagedProcess {
    fn id(&self) -> u32 {
        self.child.id()
    }

    fn try_wait(&mut self) -> Result<Option<i32>, String> {
        self.child
            .try_wait()
            .map(|status| status.map(exit_code))
            .map_err(|error| format!("Failed to poll daemon task child: {error}"))
    }

    fn signal(&mut self, signal: ProcessSignal) -> Result<(), String> {
        match signal {
            ProcessSignal::Pause => signal_process(self.child.id(), "STOP"),
            ProcessSignal::Resume => signal_process(self.child.id(), "CONT"),
            ProcessSignal::Terminate => terminate_process_group(self.child.id()),
        }
    }

    fn memory_bytes(&self) -> Result<Option<u64>, String> {
        process_memory_bytes(self.child.id())
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(128)
}

#[cfg(unix)]
fn signal_process(process_id: u32, signal: &str) -> Result<(), String> {
    let group = format!("-{process_id}");
    command_ok("kill", &[&format!("-{signal}"), &group])
}

#[cfg(windows)]
fn signal_process(process_id: u32, signal: &str) -> Result<(), String> {
    let verb = match signal {
        "STOP" => "Suspend-Process",
        "CONT" => "Resume-Process",
        _ => return Err(format!("Unsupported process signal '{signal}'")),
    };
    let script = format!("{verb} -Id {process_id} -ErrorAction Stop");
    command_ok(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", &script],
    )
}

#[cfg(unix)]
fn terminate_process_group(process_id: u32) -> Result<(), String> {
    let group = format!("-{process_id}");
    let _ = command_ok("kill", &["-TERM", &group]);
    for _ in 0..40 {
        if !super::service::process_alive(process_id) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = command_ok("kill", &["-KILL", &group]);
    Ok(())
}

#[cfg(windows)]
fn terminate_process_group(process_id: u32) -> Result<(), String> {
    command_ok("taskkill", &["/PID", &process_id.to_string(), "/T", "/F"])
}

#[cfg(unix)]
fn process_memory_bytes(process_id: u32) -> Result<Option<u64>, String> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &process_id.to_string()])
        .output()
        .map_err(|error| format!("Failed to inspect process memory: {error}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    let kib = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .ok();
    Ok(kib.and_then(|value| value.checked_mul(1024)))
}

#[cfg(windows)]
fn process_memory_bytes(process_id: u32) -> Result<Option<u64>, String> {
    let script = format!("(Get-Process -Id {process_id} -ErrorAction Stop).WorkingSet64");
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .map_err(|error| format!("Failed to inspect process memory: {error}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .ok())
}

struct ActiveProcess {
    process: Box<dyn ManagedProcess>,
    lease: LeaseToken,
}

pub struct DaemonEngine<P, N, C> {
    pub store: DaemonStore,
    pub shared: SharedLedger,
    paths: DaemonPaths,
    config: DaemonConfig,
    process_adapter: P,
    notifier: N,
    clock: C,
    owner_id: String,
    active: HashMap<String, ActiveProcess>,
    last_retention_ms: u64,
}

impl<P: ProcessAdapter, N: NotificationAdapter, C: Clock> DaemonEngine<P, N, C> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: DaemonStore,
        shared: SharedLedger,
        paths: DaemonPaths,
        config: DaemonConfig,
        process_adapter: P,
        notifier: N,
        clock: C,
        owner_id: String,
    ) -> Self {
        Self {
            store,
            shared,
            paths,
            config,
            process_adapter,
            notifier,
            clock,
            owner_id,
            active: HashMap::new(),
            last_retention_ms: 0,
        }
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    pub fn recover(&mut self) -> Result<(), String> {
        let now = self.clock.now_ms();
        for job in self.store.active_jobs()? {
            if let Some(process_id) = job.process_id {
                // A service crash can leave the task child detached. Stop the
                // whole process group before deciding from durable events;
                // never run a second copy alongside an orphan.
                let _ = self.process_adapter.terminate_orphan(process_id);
            }
            self.reconcile_interrupted(&job, now, "daemon restarted while the run was active")?;
        }
        self.store.set_meta("last_recovery_ms", &now.to_string())?;
        Ok(())
    }

    pub fn tick(&mut self) -> Result<(), String> {
        let now = self.clock.now_ms();
        self.store.set_meta("heartbeat_ms", &now.to_string())?;
        self.store
            .set_meta("pid", &std::process::id().to_string())?;
        if self.store.kill_switch()? {
            self.store.request_cancel_all(now)?;
        }

        let ids = self.active.keys().cloned().collect::<Vec<_>>();
        for job_id in ids {
            self.tick_active(&job_id, now)?;
        }

        if !self.store.kill_switch()? {
            let available = usize::try_from(self.config.concurrency)
                .unwrap_or(usize::MAX)
                .saturating_sub(self.active.len());
            if available > 0 {
                for job in self
                    .store
                    .ready_jobs(u32::try_from(available).unwrap_or(u32::MAX))?
                {
                    self.start_job(job, now)?;
                }
            }
        } else {
            self.cancel_queued(now, "global kill switch is engaged")?;
        }

        if now.saturating_sub(self.last_retention_ms) >= 60 * 60 * 1_000 {
            self.apply_retention(now)?;
            self.last_retention_ms = now;
        }
        Ok(())
    }

    fn start_job(&mut self, job: DaemonJob, now: u64) -> Result<(), String> {
        let run_id = job
            .run_id
            .as_deref()
            .ok_or_else(|| format!("Job '{}' has no durable run", job.job_id))?;
        self.validate_worktree(&job)?;
        let Some(lease) = self.shared.acquire_lease(
            run_id,
            &self.owner_id,
            now,
            self.config.lease_duration_ms,
        )?
        else {
            return Ok(());
        };
        match self.process_adapter.spawn(&job, &self.paths) {
            Ok(process) => {
                let process_id = process.id();
                self.store.transition(
                    &job.job_id,
                    JobState::Running,
                    now,
                    Some(process_id),
                    None,
                )?;
                self.active
                    .insert(job.job_id.clone(), ActiveProcess { process, lease });
                self.notify(run_id, "Background run started", &job.job_id);
                Ok(())
            }
            Err(error) => {
                let _ = self.shared.release_lease(&lease);
                if job.attempt.saturating_add(1) < job.max_attempts {
                    self.store
                        .transition(&job.job_id, JobState::Queued, now, None, Some(&error))
                } else {
                    self.fail_run(run_id, "daemon_spawn_failed", &error)?;
                    self.store
                        .transition(&job.job_id, JobState::Failed, now, None, Some(&error))
                }
            }
        }
    }

    fn tick_active(&mut self, job_id: &str, now: u64) -> Result<(), String> {
        let Some(job) = self.store.get_job(job_id)? else {
            return Err(format!("Active daemon job '{job_id}' disappeared"));
        };
        let run_id = job
            .run_id
            .as_deref()
            .ok_or_else(|| format!("Active job '{job_id}' has no run id"))?;
        let Some(lease) = self.active.get(job_id).map(|active| active.lease.clone()) else {
            return Ok(());
        };
        if !self
            .shared
            .heartbeat_lease(&lease, now, self.config.lease_duration_ms)?
        {
            self.active
                .get_mut(job_id)
                .ok_or_else(|| "active process disappeared".to_string())?
                .process
                .signal(ProcessSignal::Terminate)?;
            self.fail_run(
                run_id,
                "daemon_lease_lost",
                "The daemon lost its exclusive execution lease",
            )?;
            self.finish_active(job_id, JobState::Failed, now, Some("execution lease lost"))?;
            return Ok(());
        }
        if let Some(worktree_json) = &job.worktree_json {
            let owned: OwnedWorktree =
                serde_json::from_str(worktree_json).map_err(|error| error.to_string())?;
            let _ = self
                .shared
                .update_worktree_lease(&owned.lease_id, "active", now);
        }

        if job.cancel_requested || self.store.kill_switch()? {
            self.ensure_cancelling(run_id, "Cancellation requested by daemon controller")?;
            self.active
                .get_mut(job_id)
                .ok_or_else(|| "active process disappeared".to_string())?
                .process
                .signal(ProcessSignal::Terminate)?;
            self.cancel_run(run_id, "Cancellation reached the supervised task process")?;
            self.finish_active(job_id, JobState::Cancelled, now, None)?;
            return Ok(());
        }

        if job.pause_requested && job.state != JobState::Paused {
            let process_id = {
                let active = self
                    .active
                    .get_mut(job_id)
                    .ok_or_else(|| "active process disappeared".to_string())?;
                active.process.signal(ProcessSignal::Pause)?;
                active.process.id()
            };
            self.control(run_id)?.emit(RunEvent::Paused {
                reason: Some("Paused by daemon controller".to_string()),
            })?;
            self.store
                .transition(job_id, JobState::Paused, now, Some(process_id), None)?;
            return Ok(());
        }
        if !job.pause_requested && job.state == JobState::Paused {
            let process_id = {
                let active = self
                    .active
                    .get_mut(job_id)
                    .ok_or_else(|| "active process disappeared".to_string())?;
                active.process.signal(ProcessSignal::Resume)?;
                active.process.id()
            };
            self.control(run_id)?.emit(RunEvent::Started {
                engine_id: "monkey-daemon-resume".to_string(),
            })?;
            self.store
                .transition(job_id, JobState::Running, now, Some(process_id), None)?;
        }

        if let Some(started) = job.started_at_ms {
            if now.saturating_sub(started) > job.max_runtime_ms {
                self.cancel_for_budget(job_id, run_id, now, "daemon runtime budget exceeded")?;
                return Ok(());
            }
        }
        if let Some(max_memory) = job.max_memory_bytes {
            if self
                .active
                .get(job_id)
                .ok_or_else(|| "active process disappeared".to_string())?
                .process
                .memory_bytes()?
                .is_some_and(|used| used > max_memory)
            {
                self.cancel_for_budget(job_id, run_id, now, "daemon memory budget exceeded")?;
                return Ok(());
            }
        }
        let log_path = self.paths.logs.join(format!("{}.log", job.job_id));
        if std::fs::metadata(log_path)
            .map(|metadata| metadata.len() > job.max_log_bytes)
            .unwrap_or(false)
        {
            self.cancel_for_budget(job_id, run_id, now, "daemon log budget exceeded")?;
            return Ok(());
        }

        let exit_code = self
            .active
            .get_mut(job_id)
            .ok_or_else(|| "active process disappeared".to_string())?
            .process
            .try_wait()?;
        if let Some(exit_code) = exit_code {
            let stored = self
                .shared
                .load_run(run_id)?
                .ok_or_else(|| format!("Durable run '{run_id}' disappeared"))?;
            if stored.status.is_terminal() {
                let state = map_run_status(stored.status);
                self.finish_active(job_id, state, now, None)?;
                self.notify_terminal(run_id, state);
            } else if stored.status == RunStatus::Queued
                && exit_code != 0
                && job.attempt < job.max_attempts
            {
                // No Started event means the child proved no tool could have
                // executed. This is the only automatic retry boundary.
                self.finish_active(
                    job_id,
                    JobState::Queued,
                    now,
                    Some("child exited before start"),
                )?;
            } else {
                self.reconcile_interrupted(
                    &job,
                    now,
                    &format!("supervised task child exited with code {exit_code}"),
                )?;
                self.active.remove(job_id);
            }
        } else if let Some(stored) = self.shared.load_run(run_id)? {
            let projected = map_run_status(stored.status);
            if projected == JobState::WaitingApproval && job.state != projected {
                let process_id = self.active.get(job_id).map(|active| active.process.id());
                self.store
                    .transition(job_id, projected, now, process_id, None)?;
                self.notify(
                    run_id,
                    "Approval required",
                    "Unsafe background work is waiting",
                );
            }
        }
        Ok(())
    }

    fn cancel_for_budget(
        &mut self,
        job_id: &str,
        run_id: &str,
        now: u64,
        reason: &str,
    ) -> Result<(), String> {
        self.ensure_cancelling(run_id, reason)?;
        if let Some(active) = self.active.get_mut(job_id) {
            active.process.signal(ProcessSignal::Terminate)?;
        }
        self.cancel_run(run_id, reason)?;
        self.finish_active(job_id, JobState::Cancelled, now, Some(reason))
    }

    fn finish_active(
        &mut self,
        job_id: &str,
        state: JobState,
        now: u64,
        error: Option<&str>,
    ) -> Result<(), String> {
        if let Some(active) = self.active.remove(job_id) {
            let _ = self.shared.release_lease(&active.lease);
        }
        self.store.transition(job_id, state, now, None, error)
    }

    fn reconcile_interrupted(
        &mut self,
        job: &DaemonJob,
        now: u64,
        reason: &str,
    ) -> Result<(), String> {
        let Some(run_id) = job.run_id.as_deref() else {
            self.store.transition(
                &job.job_id,
                JobState::Failed,
                now,
                None,
                Some("preparing job has no run id"),
            )?;
            return Ok(());
        };
        let run = self
            .shared
            .load_run(run_id)?
            .ok_or_else(|| format!("Durable run '{run_id}' disappeared"))?;
        if run.status.is_terminal() {
            self.store
                .transition(&job.job_id, map_run_status(run.status), now, None, None)?;
            return Ok(());
        }
        if run.status == RunStatus::Queued {
            self.store
                .transition(&job.job_id, JobState::Queued, now, None, Some(reason))?;
            return Ok(());
        }
        let mutations = self.shared.mutations(run_id)?;
        if let Some(pending) = mutations
            .iter()
            .find(|mutation| mutation.state == "pending")
        {
            self.control(run_id)?.emit(RunEvent::NeedsReconciliation {
                mutation_id: pending.mutation_id.clone(),
                reason: bounded_text(
                    &format!("{reason}; external mutation state is uncertain"),
                    60 * 1024,
                ),
            })?;
            self.store.transition(
                &job.job_id,
                JobState::NeedsReconciliation,
                now,
                None,
                Some(reason),
            )?;
            self.notify(run_id, "Run needs reconciliation", reason);
        } else if run.status == RunStatus::Cancelling || job.cancel_requested {
            self.cancel_run(run_id, reason)?;
            self.store
                .transition(&job.job_id, JobState::Cancelled, now, None, Some(reason))?;
        } else {
            // Confirmed mutations are deliberately not replayed either. A
            // manual retry must explicitly acknowledge their existence.
            let suffix = if mutations
                .iter()
                .any(|mutation| mutation.state == "confirmed")
            {
                "; one or more external mutations were already confirmed and will not be replayed"
            } else {
                ""
            };
            self.fail_run(run_id, "daemon_interrupted", &format!("{reason}{suffix}"))?;
            self.store
                .transition(&job.job_id, JobState::Failed, now, None, Some(reason))?;
        }
        Ok(())
    }

    fn validate_worktree(&self, job: &DaemonJob) -> Result<(), String> {
        match (&job.worktree_json, &job.repository_policy_json) {
            (Some(worktree), Some(policy)) => {
                let worktree: OwnedWorktree =
                    serde_json::from_str(worktree).map_err(|error| error.to_string())?;
                let policy: RepositoryPolicy =
                    serde_json::from_str(policy).map_err(|error| error.to_string())?;
                worktree.validate_live(&self.paths, &policy)
            }
            (None, Some(policy)) => {
                let policy: RepositoryPolicy =
                    serde_json::from_str(policy).map_err(|error| error.to_string())?;
                if policy.owned_worktree_required {
                    Err("Run requires an owned worktree but has no owned lease".to_string())
                } else {
                    Ok(())
                }
            }
            (Some(_), None) => Err("Owned worktree has no repository policy".to_string()),
            (None, None) => Ok(()),
        }
    }

    fn cancel_queued(&mut self, now: u64, reason: &str) -> Result<(), String> {
        for job in self.store.nonterminal_jobs()? {
            if job.state == JobState::Queued || job.state == JobState::Preparing {
                if let Some(run_id) = job.run_id.as_deref() {
                    self.ensure_cancelling(run_id, reason)?;
                    self.cancel_run(run_id, reason)?;
                }
                self.store
                    .transition(&job.job_id, JobState::Cancelled, now, None, Some(reason))?;
            }
        }
        Ok(())
    }

    fn apply_retention(&mut self, now: u64) -> Result<(), String> {
        let retention_ms =
            u64::from(self.config.retention_days).saturating_mul(24 * 60 * 60 * 1_000);
        let before = now.saturating_sub(retention_ms);
        for job in self.store.prune_terminal(before)? {
            remove_if_exists(&job.recipe_snapshot)?;
            remove_if_exists(&self.paths.logs.join(format!("{}.log", job.job_id)))?;
        }
        for job in self.store.terminal_worktree_jobs(before)? {
            let Some(value) = job.worktree_json.as_deref() else {
                continue;
            };
            let owned: OwnedWorktree =
                serde_json::from_str(value).map_err(|error| error.to_string())?;
            match owned.safe_cleanup(&self.paths) {
                Ok(true) => {
                    self.shared
                        .update_worktree_lease(&owned.lease_id, "released", now)?;
                    remove_if_exists(&job.recipe_snapshot)?;
                    remove_if_exists(&self.paths.logs.join(format!("{}.log", job.job_id)))?;
                    self.store.delete_terminal_job(&job.job_id)?;
                }
                Ok(false) => {
                    self.shared.update_worktree_lease(
                        &owned.lease_id,
                        "needs_reconciliation",
                        now,
                    )?;
                }
                Err(error) => {
                    self.shared.update_worktree_lease(
                        &owned.lease_id,
                        "needs_reconciliation",
                        now,
                    )?;
                    eprintln!("daemon retention: {error}");
                }
            }
        }
        Ok(())
    }

    fn control(&self, run_id: &str) -> Result<Arc<DurableRunRecorder>, String> {
        DurableRunRecorder::attach(
            self.shared.run_ledger()?,
            run_id,
            "daemon-controller".to_string(),
            ClientIdentity {
                client_id: "monkey-daemon".to_string(),
                instance_id: self.owner_id.clone(),
                kind: ClientKind::Daemon,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        )
    }

    fn ensure_cancelling(&self, run_id: &str, reason: &str) -> Result<(), String> {
        let run = self
            .shared
            .load_run(run_id)?
            .ok_or_else(|| format!("Unknown run '{run_id}'"))?;
        if run.status != RunStatus::Cancelling {
            let recorder = self.control(run_id)?;
            recorder.emit(RunEvent::CancellationRequested {
                requested_by: recorder.client_identity(),
                reason: Some(bounded_text(reason, 60 * 1024)),
            })?;
            recorder.emit(RunEvent::Cancelling {
                reason: Some(bounded_text(reason, 60 * 1024)),
            })?;
        }
        Ok(())
    }

    fn cancel_run(&self, run_id: &str, reason: &str) -> Result<(), String> {
        let run = self
            .shared
            .load_run(run_id)?
            .ok_or_else(|| format!("Unknown run '{run_id}'"))?;
        if !run.status.is_terminal() {
            self.control(run_id)?.emit(RunEvent::Cancelled {
                reason: Some(bounded_text(reason, 60 * 1024)),
            })?;
        }
        Ok(())
    }

    fn fail_run(&self, run_id: &str, code: &str, message: &str) -> Result<(), String> {
        let run = self
            .shared
            .load_run(run_id)?
            .ok_or_else(|| format!("Unknown run '{run_id}'"))?;
        if !run.status.is_terminal() {
            self.control(run_id)?.emit(RunEvent::Failed {
                code: code.to_string(),
                message: bounded_text(message, 60 * 1024),
                retryable: false,
            })?;
        }
        Ok(())
    }

    pub fn decide_approval(
        &self,
        run_id: &str,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), String> {
        let ledger = self.shared.run_ledger()?;
        let approval = ledger
            .load_approval(run_id, request_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("Unknown approval '{request_id}' for run '{run_id}'"))?;
        if approval.decision.is_some() {
            return Err(format!("Approval '{request_id}' was already decided"));
        }
        let now = self.clock.now_ms();
        let effective = if now >= approval.expires_at_ms {
            PermissionDecision::Expired
        } else {
            decision
        };
        let recorder = self.control(run_id)?;
        recorder.emit(RunEvent::PermissionDecided {
            request_id: request_id.to_string(),
            operation_sha256: approval.operation_sha256,
            decision: effective,
            decided_by: recorder.client_identity(),
        })
    }

    fn notify(&self, run_id: &str, title: &str, body: &str) {
        if !self.config.notifications {
            return;
        }
        let _ = self.notifier.notify(&DaemonNotification {
            run_id: run_id.to_string(),
            title: title.to_string(),
            body: bounded_text(body, 1024),
        });
    }

    fn notify_terminal(&self, run_id: &str, state: JobState) {
        self.notify(
            run_id,
            "Background run finished",
            &format!("Run status: {}", state.token()),
        );
    }
}

fn remove_if_exists(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Failed to remove '{}': {error}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use little_monkey_lib::run_ledger::RunLedger;
    use little_monkey_lib::run_protocol::{
        ClientIdentity, ClientKind, ModelTargetSnapshot, MutationKind,
        PermissionMode as RunPermissionMode, PermissionPolicySnapshot, RootAccess, RootGrant,
        RunBudgets, RunKind, RunSpec, ToolPolicyDecision, WorkspaceContext,
        RUN_PROTOCOL_SCHEMA_VERSION,
    };

    use crate::daemon::store::{NewDaemonJob, DEFAULT_MAX_LOG_BYTES};
    use crate::durable_run::DurableRunRecorder;

    #[derive(Clone)]
    struct FakeClock(Arc<Mutex<u64>>);
    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            *self.0.lock().unwrap()
        }
    }

    #[derive(Default)]
    struct FakeNotifier(Arc<Mutex<Vec<DaemonNotification>>>);
    impl NotificationAdapter for FakeNotifier {
        fn notify(&self, notification: &DaemonNotification) -> Result<(), String> {
            self.0.lock().unwrap().push(notification.clone());
            Ok(())
        }
    }

    struct FakeProcess {
        id: u32,
        exits: Arc<Mutex<VecDeque<Option<i32>>>>,
        signals: Arc<Mutex<Vec<ProcessSignal>>>,
    }
    impl ManagedProcess for FakeProcess {
        fn id(&self) -> u32 {
            self.id
        }
        fn try_wait(&mut self) -> Result<Option<i32>, String> {
            Ok(self.exits.lock().unwrap().pop_front().flatten())
        }
        fn signal(&mut self, signal: ProcessSignal) -> Result<(), String> {
            self.signals.lock().unwrap().push(signal);
            Ok(())
        }
        fn memory_bytes(&self) -> Result<Option<u64>, String> {
            Ok(Some(1024))
        }
    }

    #[derive(Clone)]
    struct FakeProcesses {
        spawns: Arc<Mutex<u32>>,
        exits: Arc<Mutex<VecDeque<Option<i32>>>>,
        signals: Arc<Mutex<Vec<ProcessSignal>>>,
    }
    impl ProcessAdapter for FakeProcesses {
        fn spawn(
            &self,
            _job: &DaemonJob,
            _paths: &DaemonPaths,
        ) -> Result<Box<dyn ManagedProcess>, String> {
            *self.spawns.lock().unwrap() += 1;
            Ok(Box::new(FakeProcess {
                id: 42,
                exits: self.exits.clone(),
                signals: self.signals.clone(),
            }))
        }
        fn terminate_orphan(&self, _process_id: u32) -> Result<(), String> {
            self.signals.lock().unwrap().push(ProcessSignal::Terminate);
            Ok(())
        }
    }

    fn spec(run_id: &str, now: u64) -> RunSpec {
        RunSpec {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            run_id: run_id.into(),
            idempotency_key: format!("idem-{run_id}"),
            created_at_ms: now,
            kind: RunKind::Background,
            submitted_by: ClientIdentity {
                client_id: "daemon-test".into(),
                instance_id: "daemon-fixture".into(),
                kind: ClientKind::Daemon,
                version: "1".into(),
            },
            task: "fixture".into(),
            instructions: None,
            input_artifact_ids: vec![],
            target: ModelTargetSnapshot::Provider {
                target_id: "fixture-target".into(),
                label: "fixture".into(),
                provider_id: "fixture".into(),
                endpoint: "http://127.0.0.1:1/v1".into(),
                model: "fixture".into(),
                credential_ref_id: "credential-none".into(),
                capabilities: crate::task::cli_capabilities(),
            },
            workspace: Some(WorkspaceContext {
                workspace_id: "fixture-workspace".into(),
                primary_root_id: "root-primary".into(),
                roots: vec![RootGrant {
                    root_id: "root-primary".into(),
                    canonical_path: std::env::temp_dir().to_string_lossy().to_string(),
                    access: RootAccess::ReadWrite,
                    allow_symlinks_within_root: false,
                }],
                repository_policy: None,
            }),
            permission_policy: PermissionPolicySnapshot {
                mode: RunPermissionMode::Auto,
                unattended: true,
                approval_timeout_ms: 60_000,
                default_tool_decision: ToolPolicyDecision::Prompt,
                tool_rules: vec![],
                allow_network: false,
                allow_external_mutations: false,
            },
            budgets: RunBudgets {
                wall_time_ms: 60_000,
                max_iterations: 8,
                max_model_calls: 8,
                max_tool_calls: 8,
                max_input_tokens: 10_000,
                max_output_tokens: 10_000,
                max_cost_micros: None,
                max_artifact_bytes: 1024 * 1024,
                max_event_count: 1000,
            },
        }
    }

    fn fixture(
        label: &str,
    ) -> (
        DaemonPaths,
        DaemonStore,
        SharedLedger,
        Arc<DurableRunRecorder>,
        String,
    ) {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-daemon-engine-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let paths = DaemonPaths::under(&root);
        paths.ensure().unwrap();
        let run_id = format!("run-{label}");
        let ledger = RunLedger::open(&paths.ledger_db).unwrap();
        let (recorder, _) =
            DurableRunRecorder::submit(ledger, &spec(&run_id, 1_000), "daemon-fixture".into())
                .unwrap();
        let mut store = DaemonStore::open(&paths).unwrap();
        let snapshot = paths.snapshots.join(format!("job-{label}.json"));
        std::fs::write(&snapshot, b"{}").unwrap();
        store
            .insert_preparing(
                &NewDaemonJob {
                    job_id: format!("job-{label}"),
                    recipe_snapshot: snapshot,
                    priority: 0,
                    max_attempts: 1,
                    created_at_ms: 1_000,
                    max_runtime_ms: 60_000,
                    max_memory_bytes: None,
                    max_log_bytes: DEFAULT_MAX_LOG_BYTES,
                    repository_policy_json: None,
                    worktree_json: None,
                    parent_run_id: None,
                },
                8,
            )
            .unwrap();
        store
            .mark_queued(&format!("job-{label}"), &run_id, 1_000)
            .unwrap();
        let shared = SharedLedger::open(&paths.ledger_db).unwrap();
        (paths, store, shared, recorder, run_id)
    }

    fn fake_adapter() -> FakeProcesses {
        FakeProcesses {
            spawns: Arc::new(Mutex::new(0)),
            exits: Arc::new(Mutex::new(VecDeque::from([None, None]))),
            signals: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[test]
    fn process_signal_enum_distinguishes_pause_resume_and_cancel() {
        assert_ne!(ProcessSignal::Pause, ProcessSignal::Resume);
        assert_ne!(ProcessSignal::Resume, ProcessSignal::Terminate);
    }

    #[test]
    fn cancellation_reaches_the_active_process_and_terminalizes_the_ledger() {
        let (paths, store, shared, _recorder, run_id) = fixture("cancel");
        let adapter = fake_adapter();
        let signals = adapter.signals.clone();
        let clock = FakeClock(Arc::new(Mutex::new(2_000)));
        let mut engine = DaemonEngine::new(
            store,
            shared,
            paths,
            DaemonConfig::default(),
            adapter,
            FakeNotifier::default(),
            clock.clone(),
            "daemon-test-owner".into(),
        );
        engine.tick().unwrap();
        assert_eq!(engine.active_count(), 1);
        engine.store.request_cancel(&run_id, 2_001).unwrap();
        *clock.0.lock().unwrap() = 2_001;
        engine.tick().unwrap();
        assert!(signals.lock().unwrap().contains(&ProcessSignal::Terminate));
        assert_eq!(engine.active_count(), 0);
        assert_eq!(
            engine.shared.load_run(&run_id).unwrap().unwrap().status,
            RunStatus::Cancelled
        );
    }

    #[test]
    fn recovery_never_replays_a_confirmed_mutation() {
        let (paths, mut store, shared, recorder, run_id) = fixture("confirmed");
        recorder
            .emit(RunEvent::Started {
                engine_id: "fixture".into(),
            })
            .unwrap();
        recorder
            .emit(RunEvent::ExternalMutationPrepared {
                mutation_id: "mutation-one".into(),
                tool_call_id: "tool-one".into(),
                kind: MutationKind::Git,
                idempotency_key: Some("fixture-mutation-key".into()),
                summary: "prepare fixture push".into(),
            })
            .unwrap();
        recorder
            .emit(RunEvent::ExternalMutationConfirmed {
                mutation_id: "mutation-one".into(),
                confirmation_ref: Some("abc123".into()),
                summary: "fixture push confirmed".into(),
            })
            .unwrap();
        store
            .transition("job-confirmed", JobState::Running, 2_000, Some(77), None)
            .unwrap();
        let adapter = fake_adapter();
        let spawns = adapter.spawns.clone();
        let signals = adapter.signals.clone();
        let mut engine = DaemonEngine::new(
            store,
            shared,
            paths,
            DaemonConfig::default(),
            adapter,
            FakeNotifier::default(),
            FakeClock(Arc::new(Mutex::new(3_000))),
            "daemon-recovery".into(),
        );
        engine.recover().unwrap();
        engine.tick().unwrap();
        assert_eq!(
            *spawns.lock().unwrap(),
            0,
            "confirmed effects must not replay"
        );
        assert!(signals.lock().unwrap().contains(&ProcessSignal::Terminate));
        assert_eq!(
            engine.shared.load_run(&run_id).unwrap().unwrap().status,
            RunStatus::Failed
        );
        let confirmed = engine
            .shared
            .events(&run_id, 0, 100)
            .unwrap()
            .into_iter()
            .filter(|event| matches!(event.event, RunEvent::ExternalMutationConfirmed { .. }))
            .count();
        assert_eq!(confirmed, 1, "recovery must not duplicate confirmation");
    }

    #[test]
    fn recovery_routes_an_uncertain_prepared_mutation_to_reconciliation() {
        let (paths, mut store, shared, recorder, run_id) = fixture("uncertain");
        recorder
            .emit(RunEvent::Started {
                engine_id: "fixture".into(),
            })
            .unwrap();
        recorder
            .emit(RunEvent::ExternalMutationPrepared {
                mutation_id: "mutation-pending".into(),
                tool_call_id: "tool-pending".into(),
                kind: MutationKind::ExternalService,
                idempotency_key: None,
                summary: "uncertain fixture request".into(),
            })
            .unwrap();
        store
            .transition("job-uncertain", JobState::Running, 2_000, Some(88), None)
            .unwrap();
        let adapter = fake_adapter();
        let mut engine = DaemonEngine::new(
            store,
            shared,
            paths,
            DaemonConfig::default(),
            adapter,
            FakeNotifier::default(),
            FakeClock(Arc::new(Mutex::new(3_000))),
            "daemon-recovery".into(),
        );
        engine.recover().unwrap();
        assert_eq!(
            engine.shared.load_run(&run_id).unwrap().unwrap().status,
            RunStatus::NeedsReconciliation
        );
        assert_eq!(
            engine.store.get_job(&run_id).unwrap().unwrap().state,
            JobState::NeedsReconciliation
        );
    }
}
