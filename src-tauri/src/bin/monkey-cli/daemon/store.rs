use std::path::{Path, PathBuf};

use little_monkey_lib::run_ledger::RunLedger;
use little_monkey_lib::run_protocol::RunStatus;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

pub const DEFAULT_MAX_QUEUE: u32 = 128;
pub const DEFAULT_CONCURRENCY: u32 = 4;
pub const DEFAULT_RETENTION_DAYS: u32 = 30;
pub const DEFAULT_LEASE_MS: u64 = 15_000;
pub const DEFAULT_POLL_MS: u64 = 250;
pub const DEFAULT_MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct DaemonPaths {
    pub root: PathBuf,
    pub config: PathBuf,
    pub state_db: PathBuf,
    pub ledger_db: PathBuf,
    pub snapshots: PathBuf,
    pub logs: PathBuf,
    pub worktrees: PathBuf,
    pub lock: PathBuf,
}

impl DaemonPaths {
    pub fn resolve() -> Result<Self, String> {
        let app_data = crate::app_data_dir()
            .ok_or_else(|| "Could not resolve the app data directory".to_string())?;
        Ok(Self::under(&app_data))
    }

    pub fn under(app_data: &Path) -> Self {
        let root = app_data.join("daemon");
        Self {
            config: root.join("config.json"),
            state_db: root.join("daemon-v1.sqlite3"),
            ledger_db: app_data.join("profile-v1.sqlite3"),
            snapshots: root.join("snapshots"),
            logs: root.join("logs"),
            worktrees: root.join("worktrees"),
            lock: root.join("daemon.lock"),
            root,
        }
    }

    pub fn ensure(&self) -> Result<(), String> {
        for path in [&self.root, &self.snapshots, &self.logs, &self.worktrees] {
            std::fs::create_dir_all(path)
                .map_err(|error| format!("Failed to create '{}': {error}", path.display()))?;
            restrict_directory(path)?;
        }
        Ok(())
    }
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("Failed to protect '{}': {error}", path.display()))
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
pub fn restrict_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("Failed to protect '{}': {error}", path.display()))
}

#[cfg(not(unix))]
pub fn restrict_file(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    pub schema_version: u32,
    pub max_queue: u32,
    pub concurrency: u32,
    pub retention_days: u32,
    pub poll_interval_ms: u64,
    pub lease_duration_ms: u64,
    pub webhook_port: Option<u16>,
    pub notifications: bool,
    pub max_log_bytes: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            schema_version: 1,
            max_queue: DEFAULT_MAX_QUEUE,
            concurrency: DEFAULT_CONCURRENCY,
            retention_days: DEFAULT_RETENTION_DAYS,
            poll_interval_ms: DEFAULT_POLL_MS,
            lease_duration_ms: DEFAULT_LEASE_MS,
            webhook_port: None,
            notifications: true,
            max_log_bytes: DEFAULT_MAX_LOG_BYTES,
        }
    }
}

impl DaemonConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err(format!(
                "Unsupported daemon config schema {}",
                self.schema_version
            ));
        }
        if !(1..=1024).contains(&self.max_queue) {
            return Err("daemon max_queue must be between 1 and 1024".to_string());
        }
        if !(1..=32).contains(&self.concurrency) {
            return Err("daemon concurrency must be between 1 and 32".to_string());
        }
        if !(1..=3650).contains(&self.retention_days) {
            return Err("daemon retention_days must be between 1 and 3650".to_string());
        }
        if !(100..=1_000).contains(&self.poll_interval_ms) {
            return Err("daemon poll_interval_ms must be between 100 and 1000".to_string());
        }
        if self.lease_duration_ms < self.poll_interval_ms.saturating_mul(3)
            || self.lease_duration_ms > 300_000
        {
            return Err(
                "daemon lease_duration_ms must be at least three poll intervals and at most 300000"
                    .to_string(),
            );
        }
        if self.webhook_port == Some(0) {
            return Err("daemon webhook_port must be non-zero".to_string());
        }
        if !(64 * 1024..=1024 * 1024 * 1024).contains(&self.max_log_bytes) {
            return Err("daemon max_log_bytes must be between 64 KiB and 1 GiB".to_string());
        }
        Ok(())
    }

    pub fn load(paths: &DaemonPaths) -> Result<Self, String> {
        let bytes = std::fs::read(&paths.config).map_err(|error| {
            format!(
                "Daemon is not installed (cannot read '{}': {error})",
                paths.config.display()
            )
        })?;
        let value: Self = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Invalid daemon config: {error}"))?;
        value.validate()?;
        Ok(value)
    }

    pub fn save(&self, paths: &DaemonPaths) -> Result<(), String> {
        self.validate()?;
        paths.ensure()?;
        let tmp = paths.config.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        std::fs::write(&tmp, bytes)
            .map_err(|error| format!("Failed to write daemon config: {error}"))?;
        restrict_file(&tmp)?;
        std::fs::rename(&tmp, &paths.config)
            .map_err(|error| format!("Failed to publish daemon config: {error}"))?;
        restrict_file(&paths.config)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Preparing,
    Queued,
    Running,
    WaitingApproval,
    Paused,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
    NeedsReconciliation,
}

impl JobState {
    pub fn token(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::WaitingApproval => "waiting_approval",
            Self::Paused => "paused",
            Self::Cancelling => "cancelling",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::NeedsReconciliation => "needs_reconciliation",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "preparing" => Ok(Self::Preparing),
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "waiting_approval" => Ok(Self::WaitingApproval),
            "paused" => Ok(Self::Paused),
            "cancelling" => Ok(Self::Cancelling),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "needs_reconciliation" => Ok(Self::NeedsReconciliation),
            other => Err(format!("unknown daemon job state '{other}'")),
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::NeedsReconciliation
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonJob {
    pub job_id: String,
    pub run_id: Option<String>,
    pub recipe_snapshot: PathBuf,
    pub state: JobState,
    pub priority: i32,
    pub attempt: u32,
    pub max_attempts: u32,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub process_id: Option<u32>,
    pub max_runtime_ms: u64,
    pub max_memory_bytes: Option<u64>,
    pub max_log_bytes: u64,
    pub pause_requested: bool,
    pub cancel_requested: bool,
    pub repository_policy_json: Option<String>,
    pub worktree_json: Option<String>,
    pub parent_run_id: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewDaemonJob {
    pub job_id: String,
    pub recipe_snapshot: PathBuf,
    pub priority: i32,
    pub max_attempts: u32,
    pub created_at_ms: u64,
    pub max_runtime_ms: u64,
    pub max_memory_bytes: Option<u64>,
    pub max_log_bytes: u64,
    pub repository_policy_json: Option<String>,
    pub worktree_json: Option<String>,
    pub parent_run_id: Option<String>,
}

pub struct DaemonStore {
    connection: Connection,
}

impl DaemonStore {
    pub fn open(paths: &DaemonPaths) -> Result<Self, String> {
        paths.ensure()?;
        // Opening the authoritative ledger first applies its own migrations;
        // daemon-owned side tables never attempt to recreate shared schema.
        RunLedger::open(&paths.ledger_db).map_err(|error| error.to_string())?;
        let connection = Connection::open(&paths.state_db)
            .map_err(|error| format!("Failed to open daemon state: {error}"))?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| error.to_string())?;
        connection
            .execute_batch(DAEMON_SCHEMA)
            .map_err(|error| format!("Failed to migrate daemon state: {error}"))?;
        restrict_file(&paths.state_db)?;
        Ok(Self { connection })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, String> {
        let connection = Connection::open_in_memory().map_err(|error| error.to_string())?;
        connection
            .execute_batch(DAEMON_SCHEMA)
            .map_err(|error| error.to_string())?;
        Ok(Self { connection })
    }

    pub fn set_meta(&mut self, key: &str, value: &str) -> Result<(), String> {
        self.connection
            .execute(
                "INSERT INTO daemon_meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>, String> {
        self.connection
            .query_row(
                "SELECT value FROM daemon_meta WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    pub fn kill_switch(&self) -> Result<bool, String> {
        Ok(self.get_meta("kill_switch")?.as_deref() == Some("engaged"))
    }

    pub fn set_kill_switch(&mut self, engaged: bool) -> Result<(), String> {
        self.set_meta("kill_switch", if engaged { "engaged" } else { "released" })
    }

    pub fn nonterminal_count(&self) -> Result<u32, String> {
        self.connection
            .query_row(
                "SELECT COUNT(*) FROM daemon_jobs WHERE state NOT IN
                 ('succeeded','failed','cancelled','needs_reconciliation')",
                [],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())
    }

    pub fn insert_preparing(&mut self, job: &NewDaemonJob, max_queue: u32) -> Result<(), String> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let count: u32 = transaction
            .query_row(
                "SELECT COUNT(*) FROM daemon_jobs WHERE state NOT IN
                 ('succeeded','failed','cancelled','needs_reconciliation')",
                [],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if count >= max_queue {
            return Err(format!(
                "Daemon queue is full ({count}/{max_queue}); wait for a run or cancel one"
            ));
        }
        transaction
            .execute(
                "INSERT INTO daemon_jobs (
                    job_id, recipe_snapshot, state, priority, attempt, max_attempts,
                    created_at_ms, updated_at_ms, max_runtime_ms, max_memory_bytes,
                    max_log_bytes, repository_policy_json, worktree_json, parent_run_id
                 ) VALUES (?1, ?2, 'preparing', ?3, 0, ?4, ?5, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    job.job_id,
                    job.recipe_snapshot.to_string_lossy(),
                    job.priority,
                    job.max_attempts,
                    to_i64(job.created_at_ms)?,
                    to_i64(job.max_runtime_ms)?,
                    job.max_memory_bytes.map(to_i64).transpose()?,
                    to_i64(job.max_log_bytes)?,
                    job.repository_policy_json,
                    job.worktree_json,
                    job.parent_run_id,
                ],
            )
            .map_err(|error| format!("Failed to queue daemon job: {error}"))?;
        transaction.commit().map_err(|error| error.to_string())
    }

    pub fn mark_queued(&mut self, job_id: &str, run_id: &str, now_ms: u64) -> Result<(), String> {
        let changed = self
            .connection
            .execute(
                "UPDATE daemon_jobs SET run_id=?2, state='queued', updated_at_ms=?3
                 WHERE job_id=?1 AND state='preparing'",
                params![job_id, run_id, to_i64(now_ms)?],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err(format!("Job '{job_id}' is not preparing"));
        }
        Ok(())
    }

    pub fn get_job(&self, id: &str) -> Result<Option<DaemonJob>, String> {
        self.connection
            .query_row(
                "SELECT job_id, run_id, recipe_snapshot, state, priority, attempt,
                        max_attempts, created_at_ms, updated_at_ms, started_at_ms,
                        finished_at_ms, process_id, max_runtime_ms, max_memory_bytes,
                        max_log_bytes, pause_requested, cancel_requested,
                        repository_policy_json, worktree_json, parent_run_id, last_error
                 FROM daemon_jobs WHERE job_id=?1 OR run_id=?1",
                [id],
                read_job,
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    pub fn ready_jobs(&self, limit: u32) -> Result<Vec<DaemonJob>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT job_id, run_id, recipe_snapshot, state, priority, attempt,
                        max_attempts, created_at_ms, updated_at_ms, started_at_ms,
                        finished_at_ms, process_id, max_runtime_ms, max_memory_bytes,
                        max_log_bytes, pause_requested, cancel_requested,
                        repository_policy_json, worktree_json, parent_run_id, last_error
                 FROM daemon_jobs
                 WHERE state='queued' AND pause_requested=0 AND cancel_requested=0
                 ORDER BY priority DESC, created_at_ms ASC, job_id ASC LIMIT ?1",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([i64::from(limit)], read_job)
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn active_jobs(&self) -> Result<Vec<DaemonJob>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT job_id, run_id, recipe_snapshot, state, priority, attempt,
                        max_attempts, created_at_ms, updated_at_ms, started_at_ms,
                        finished_at_ms, process_id, max_runtime_ms, max_memory_bytes,
                        max_log_bytes, pause_requested, cancel_requested,
                        repository_policy_json, worktree_json, parent_run_id, last_error
                 FROM daemon_jobs WHERE state IN
                 ('running','waiting_approval','paused','cancelling')
                 ORDER BY created_at_ms ASC, job_id ASC",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], read_job)
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn nonterminal_jobs(&self) -> Result<Vec<DaemonJob>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT job_id, run_id, recipe_snapshot, state, priority, attempt,
                        max_attempts, created_at_ms, updated_at_ms, started_at_ms,
                        finished_at_ms, process_id, max_runtime_ms, max_memory_bytes,
                        max_log_bytes, pause_requested, cancel_requested,
                        repository_policy_json, worktree_json, parent_run_id, last_error
                 FROM daemon_jobs WHERE state NOT IN
                 ('succeeded','failed','cancelled','needs_reconciliation')
                 ORDER BY created_at_ms ASC, job_id ASC",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], read_job)
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    /// Returns the most recently updated durable runs owned by this daemon.
    /// The status surface uses a fixed upper bound so frequent desktop polls
    /// cannot grow with unbounded retained history.
    pub fn managed_run_ids(&self, limit: u32) -> Result<Vec<String>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT run_id FROM daemon_jobs
                 WHERE run_id IS NOT NULL
                 ORDER BY updated_at_ms DESC, job_id ASC LIMIT ?1",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([i64::from(limit)], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn request_cancel_all(&mut self, now_ms: u64) -> Result<u32, String> {
        self.connection
            .execute(
                "UPDATE daemon_jobs SET cancel_requested=1, updated_at_ms=?1
                 WHERE state NOT IN ('succeeded','failed','cancelled','needs_reconciliation')",
                [to_i64(now_ms)?],
            )
            .map(|changed| u32::try_from(changed).unwrap_or(u32::MAX))
            .map_err(|error| error.to_string())
    }

    /// Move a job to `state`.
    ///
    /// `attempt` counts **attempts started**, so it moves only on the edge that
    /// starts one: leaving the queue for `running`. It used to increment on every
    /// arrival at `running`, which also caught resuming from `paused` and
    /// returning from `waiting_approval` — neither of which is a new attempt.
    /// That silently spent a job's retry budget: a job with `max_attempts: 3`
    /// paused and resumed twice had no attempts left to fail with, and
    /// `backoff_elapsed` charged it a retry backoff it had never earned.
    pub fn transition(
        &mut self,
        job_id: &str,
        state: JobState,
        now_ms: u64,
        process_id: Option<u32>,
        error: Option<&str>,
    ) -> Result<(), String> {
        let terminal = state.is_terminal();
        let changed = self
            .connection
            .execute(
                "UPDATE daemon_jobs
                 SET state=?2, updated_at_ms=?3,
                     started_at_ms=CASE WHEN ?2='running' AND started_at_ms IS NULL THEN ?3 ELSE started_at_ms END,
                     finished_at_ms=CASE WHEN ?4=1 THEN ?3 ELSE finished_at_ms END,
                     process_id=?5,
                     attempt=CASE
                         WHEN ?2='running' AND state IN ('preparing','queued')
                         THEN attempt + 1 ELSE attempt END,
                     last_error=COALESCE(?6, last_error)
                 WHERE job_id=?1",
                params![
                    job_id,
                    state.token(),
                    to_i64(now_ms)?,
                    terminal,
                    process_id.map(i64::from),
                    error,
                ],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err(format!("Unknown daemon job '{job_id}'"));
        }
        Ok(())
    }

    pub fn request_pause(
        &mut self,
        id: &str,
        value: bool,
        now_ms: u64,
    ) -> Result<DaemonJob, String> {
        let job = self
            .get_job(id)?
            .ok_or_else(|| format!("Unknown daemon run '{id}'"))?;
        if job.state.is_terminal() {
            return Err(format!(
                "Run '{id}' is already terminal ({})",
                job.state.token()
            ));
        }
        self.connection
            .execute(
                "UPDATE daemon_jobs SET pause_requested=?2, updated_at_ms=?3 WHERE job_id=?1",
                params![job.job_id, value, to_i64(now_ms)?],
            )
            .map_err(|error| error.to_string())?;
        self.get_job(&job.job_id)?
            .ok_or_else(|| "job disappeared".to_string())
    }

    pub fn request_cancel(&mut self, id: &str, now_ms: u64) -> Result<DaemonJob, String> {
        let job = self
            .get_job(id)?
            .ok_or_else(|| format!("Unknown daemon run '{id}'"))?;
        if job.state.is_terminal() {
            return Ok(job);
        }
        self.connection
            .execute(
                "UPDATE daemon_jobs SET cancel_requested=1, updated_at_ms=?2 WHERE job_id=?1",
                params![job.job_id, to_i64(now_ms)?],
            )
            .map_err(|error| error.to_string())?;
        self.get_job(&job.job_id)?
            .ok_or_else(|| "job disappeared".to_string())
    }

    pub fn stale_preparing(&self, before_ms: u64) -> Result<Vec<DaemonJob>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT job_id, run_id, recipe_snapshot, state, priority, attempt,
                        max_attempts, created_at_ms, updated_at_ms, started_at_ms,
                        finished_at_ms, process_id, max_runtime_ms, max_memory_bytes,
                        max_log_bytes, pause_requested, cancel_requested,
                        repository_policy_json, worktree_json, parent_run_id, last_error
                 FROM daemon_jobs WHERE state='preparing' AND updated_at_ms < ?1",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([to_i64(before_ms)?], read_job)
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn prune_terminal(&mut self, before_ms: u64) -> Result<Vec<DaemonJob>, String> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let jobs = {
            let mut statement = transaction
                .prepare(
                    "SELECT job_id, run_id, recipe_snapshot, state, priority, attempt,
                            max_attempts, created_at_ms, updated_at_ms, started_at_ms,
                            finished_at_ms, process_id, max_runtime_ms, max_memory_bytes,
                            max_log_bytes, pause_requested, cancel_requested,
                            repository_policy_json, worktree_json, parent_run_id, last_error
                     FROM daemon_jobs WHERE state IN
                     ('succeeded','failed','cancelled','needs_reconciliation')
                     AND finished_at_ms IS NOT NULL AND finished_at_ms < ?1
                     AND worktree_json IS NULL",
                )
                .map_err(|error| error.to_string())?;
            let rows = statement
                .query_map([to_i64(before_ms)?], read_job)
                .map_err(|error| error.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?
        };
        for job in &jobs {
            transaction
                .execute("DELETE FROM daemon_jobs WHERE job_id=?1", [&job.job_id])
                .map_err(|error| error.to_string())?;
        }
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(jobs)
    }

    pub fn terminal_worktree_jobs(&self, before_ms: u64) -> Result<Vec<DaemonJob>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT job_id, run_id, recipe_snapshot, state, priority, attempt,
                        max_attempts, created_at_ms, updated_at_ms, started_at_ms,
                        finished_at_ms, process_id, max_runtime_ms, max_memory_bytes,
                        max_log_bytes, pause_requested, cancel_requested,
                        repository_policy_json, worktree_json, parent_run_id, last_error
                 FROM daemon_jobs WHERE state IN
                 ('succeeded','failed','cancelled','needs_reconciliation')
                 AND finished_at_ms IS NOT NULL AND finished_at_ms < ?1
                 AND worktree_json IS NOT NULL",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([to_i64(before_ms)?], read_job)
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn delete_terminal_job(&mut self, job_id: &str) -> Result<bool, String> {
        self.connection
            .execute(
                "DELETE FROM daemon_jobs WHERE job_id=?1 AND state IN
                 ('succeeded','failed','cancelled','needs_reconciliation')",
                [job_id],
            )
            .map(|changed| changed == 1)
            .map_err(|error| error.to_string())
    }

    /// Reserves a delivery id and nonce atomically. `false` means either was
    /// already seen; callers must not submit another run in that case.
    pub fn reserve_delivery_payload(
        &mut self,
        trigger_id: &str,
        delivery_id: &str,
        nonce: Option<&str>,
        payload: &str,
        now_ms: u64,
    ) -> Result<bool, String> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let exists = transaction
            .query_row(
                "SELECT 1 FROM daemon_delivery_payloads
                 WHERE trigger_id=?1 AND (delivery_id=?2 OR (?3 IS NOT NULL AND nonce=?3))",
                params![trigger_id, delivery_id, nonce],
                |_| Ok(()),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .is_some();
        if exists {
            transaction.commit().map_err(|error| error.to_string())?;
            return Ok(false);
        }
        transaction
            .execute(
                "INSERT INTO daemon_delivery_payloads
                 (trigger_id, delivery_id, nonce, payload_json, status, received_at_ms)
                 VALUES (?1, ?2, ?3, ?4, 'reserved', ?5)",
                params![trigger_id, delivery_id, nonce, payload, to_i64(now_ms)?],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(true)
    }

    pub fn activate_delivery_payload(
        &mut self,
        trigger_id: &str,
        delivery_id: &str,
    ) -> Result<(), String> {
        let changed = self
            .connection
            .execute(
                "UPDATE daemon_delivery_payloads SET status='accepted'
                 WHERE trigger_id=?1 AND delivery_id=?2 AND status='reserved'",
                params![trigger_id, delivery_id],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err(format!(
                "Delivery payload '{trigger_id}/{delivery_id}' is not reserved"
            ));
        }
        Ok(())
    }

    pub fn discard_delivery_payload(
        &mut self,
        trigger_id: &str,
        delivery_id: &str,
    ) -> Result<(), String> {
        self.connection
            .execute(
                "UPDATE daemon_delivery_payloads SET status='rejected'
                 WHERE trigger_id=?1 AND delivery_id=?2 AND status IN ('reserved','accepted')",
                params![trigger_id, delivery_id],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn pending_delivery_payloads(&self, limit: u32) -> Result<Vec<PendingDelivery>, String> {
        self.delivery_payloads_with_status("accepted", limit)
    }

    pub fn reserved_delivery_payloads(&self, limit: u32) -> Result<Vec<PendingDelivery>, String> {
        self.delivery_payloads_with_status("reserved", limit)
    }

    fn delivery_payloads_with_status(
        &self,
        status: &str,
        limit: u32,
    ) -> Result<Vec<PendingDelivery>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT trigger_id, delivery_id, nonce, payload_json, received_at_ms
                 FROM daemon_delivery_payloads WHERE status=?1 AND job_id IS NULL
                 ORDER BY received_at_ms ASC LIMIT ?2",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(params![status, i64::from(limit)], |row| {
                Ok(PendingDelivery {
                    trigger_id: row.get(0)?,
                    delivery_id: row.get(1)?,
                    nonce: row.get(2)?,
                    payload_json: row.get(3)?,
                    received_at_ms: from_i64(row.get(4)?)?,
                })
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn mark_delivery_submitted(
        &mut self,
        trigger_id: &str,
        delivery_id: &str,
        job_id: &str,
    ) -> Result<(), String> {
        self.connection
            .execute(
                "UPDATE daemon_delivery_payloads SET status='submitted', job_id=?3
                 WHERE trigger_id=?1 AND delivery_id=?2 AND status='accepted'",
                params![trigger_id, delivery_id, job_id],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Marks an M4 workflow delivery complete without pretending its run id
    /// belongs to `daemon_jobs`. M4 owns that append-only history, so the
    /// recipe-job foreign key deliberately remains NULL.
    pub fn mark_delivery_submitted_external(
        &mut self,
        trigger_id: &str,
        delivery_id: &str,
    ) -> Result<(), String> {
        let changed = self
            .connection
            .execute(
                "UPDATE daemon_delivery_payloads SET status='submitted', job_id=NULL
                 WHERE trigger_id=?1 AND delivery_id=?2 AND status='accepted'",
                params![trigger_id, delivery_id],
            )
            .map_err(|error| error.to_string())?;
        if changed == 1 {
            return Ok(());
        }
        let existing = self
            .connection
            .query_row(
                "SELECT status,job_id FROM daemon_delivery_payloads
                 WHERE trigger_id=?1 AND delivery_id=?2",
                params![trigger_id, delivery_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if matches!(existing, Some((ref status, None)) if status == "submitted") {
            Ok(())
        } else {
            Err(format!(
                "External delivery payload '{trigger_id}/{delivery_id}' is not pending"
            ))
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingDelivery {
    pub trigger_id: String,
    pub delivery_id: String,
    /// Dedup key already consumed by `reserve_delivery_payload`; kept so the
    /// struct mirrors the full `daemon_delivery_payloads` row, but nothing
    /// reads it after acceptance.
    #[allow(dead_code)]
    pub nonce: Option<String>,
    pub payload_json: String,
    pub received_at_ms: u64,
}

fn read_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<DaemonJob> {
    Ok(DaemonJob {
        job_id: row.get(0)?,
        run_id: row.get(1)?,
        recipe_snapshot: PathBuf::from(row.get::<_, String>(2)?),
        state: JobState::parse(&row.get::<_, String>(3)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
            )
        })?,
        priority: row.get(4)?,
        attempt: from_i64(row.get(5)?)?,
        max_attempts: from_i64(row.get(6)?)?,
        created_at_ms: from_i64(row.get(7)?)?,
        updated_at_ms: from_i64(row.get(8)?)?,
        started_at_ms: row.get::<_, Option<i64>>(9)?.map(from_i64).transpose()?,
        finished_at_ms: row.get::<_, Option<i64>>(10)?.map(from_i64).transpose()?,
        process_id: row
            .get::<_, Option<i64>>(11)?
            .map(|value| {
                u32::try_from(value)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(11, value))
            })
            .transpose()?,
        max_runtime_ms: from_i64(row.get(12)?)?,
        max_memory_bytes: row.get::<_, Option<i64>>(13)?.map(from_i64).transpose()?,
        max_log_bytes: from_i64(row.get(14)?)?,
        pause_requested: row.get::<_, i64>(15)? != 0,
        cancel_requested: row.get::<_, i64>(16)? != 0,
        repository_policy_json: row.get(17)?,
        worktree_json: row.get(18)?,
        parent_run_id: row.get(19)?,
        last_error: row.get(20)?,
    })
}

fn to_i64(value: u64) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| "numeric value exceeds SQLite range".to_string())
}

fn from_i64<T>(value: i64) -> rusqlite::Result<T>
where
    T: TryFrom<i64>,
{
    T::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
}

pub fn map_run_status(status: RunStatus) -> JobState {
    match status {
        RunStatus::Queued => JobState::Queued,
        RunStatus::Running => JobState::Running,
        RunStatus::WaitingForPermission => JobState::WaitingApproval,
        RunStatus::Paused => JobState::Paused,
        RunStatus::Cancelling => JobState::Cancelling,
        RunStatus::Succeeded => JobState::Succeeded,
        RunStatus::Failed => JobState::Failed,
        RunStatus::Cancelled => JobState::Cancelled,
        RunStatus::NeedsReconciliation => JobState::NeedsReconciliation,
    }
}

const DAEMON_SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS daemon_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS daemon_jobs (
    job_id TEXT PRIMARY KEY,
    run_id TEXT UNIQUE,
    recipe_snapshot TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN (
        'preparing','queued','running','waiting_approval','paused','cancelling',
        'succeeded','failed','cancelled','needs_reconciliation'
    )),
    priority INTEGER NOT NULL,
    attempt INTEGER NOT NULL CHECK (attempt >= 0),
    max_attempts INTEGER NOT NULL CHECK (max_attempts BETWEEN 1 AND 100),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0),
    started_at_ms INTEGER,
    finished_at_ms INTEGER,
    process_id INTEGER,
    max_runtime_ms INTEGER NOT NULL CHECK (max_runtime_ms > 0),
    max_memory_bytes INTEGER,
    max_log_bytes INTEGER NOT NULL CHECK (max_log_bytes > 0),
    pause_requested INTEGER NOT NULL DEFAULT 0 CHECK (pause_requested IN (0,1)),
    cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK (cancel_requested IN (0,1)),
    repository_policy_json TEXT,
    worktree_json TEXT,
    parent_run_id TEXT,
    last_error TEXT
) STRICT;

CREATE INDEX IF NOT EXISTS daemon_jobs_ready_idx
    ON daemon_jobs(state, pause_requested, cancel_requested, priority DESC, created_at_ms);
CREATE INDEX IF NOT EXISTS daemon_jobs_run_idx ON daemon_jobs(run_id);

CREATE TABLE IF NOT EXISTS daemon_delivery_payloads (
    trigger_id TEXT NOT NULL,
    delivery_id TEXT NOT NULL,
    nonce TEXT,
    payload_json TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('reserved','accepted','rejected','submitted')),
    received_at_ms INTEGER NOT NULL CHECK (received_at_ms > 0),
    job_id TEXT REFERENCES daemon_jobs(job_id) ON DELETE SET NULL,
    PRIMARY KEY(trigger_id, delivery_id),
    UNIQUE(trigger_id, nonce)
) STRICT;
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn new_job(id: &str, now: u64) -> NewDaemonJob {
        NewDaemonJob {
            job_id: id.to_string(),
            recipe_snapshot: PathBuf::from(format!("/{id}.json")),
            priority: 0,
            max_attempts: 1,
            created_at_ms: now,
            max_runtime_ms: 60_000,
            max_memory_bytes: None,
            max_log_bytes: DEFAULT_MAX_LOG_BYTES,
            repository_policy_json: None,
            worktree_json: None,
            parent_run_id: None,
        }
    }

    #[test]
    fn queue_bound_is_transactional_and_terminal_jobs_do_not_count() {
        let mut store = DaemonStore::open_in_memory().unwrap();
        store.insert_preparing(&new_job("one", 1), 1).unwrap();
        assert!(store.insert_preparing(&new_job("two", 2), 1).is_err());
        store
            .transition("one", JobState::Failed, 3, None, Some("test"))
            .unwrap();
        store.insert_preparing(&new_job("two", 4), 1).unwrap();
    }

    #[test]
    fn ready_queue_orders_priority_then_age() {
        let mut store = DaemonStore::open_in_memory().unwrap();
        for (id, now, priority) in [("old", 1, 0), ("high", 2, 9), ("low", 3, -1)] {
            let mut job = new_job(id, now);
            job.priority = priority;
            store.insert_preparing(&job, 8).unwrap();
            store.mark_queued(id, &format!("run-{id}"), now).unwrap();
        }
        let ids = store
            .ready_jobs(8)
            .unwrap()
            .into_iter()
            .map(|job| job.job_id)
            .collect::<Vec<_>>();
        assert_eq!(ids, ["high", "old", "low"]);
    }

    #[test]
    fn managed_run_ids_are_bounded_and_ordered_by_latest_update() {
        let mut store = DaemonStore::open_in_memory().unwrap();
        for (id, now) in [("old", 1), ("middle", 2), ("new", 3)] {
            store.insert_preparing(&new_job(id, now), 8).unwrap();
            store.mark_queued(id, &format!("run-{id}"), now).unwrap();
        }
        store
            .transition("old", JobState::Running, 4, Some(42), None)
            .unwrap();
        assert_eq!(
            store.managed_run_ids(2).unwrap(),
            ["run-old".to_string(), "run-new".to_string()]
        );
    }

    #[test]
    fn kill_switch_is_durable_in_store() {
        let mut store = DaemonStore::open_in_memory().unwrap();
        assert!(!store.kill_switch().unwrap());
        store.set_kill_switch(true).unwrap();
        assert!(store.kill_switch().unwrap());
        store.set_kill_switch(false).unwrap();
        assert!(!store.kill_switch().unwrap());
    }

    #[test]
    fn external_delivery_marker_is_idempotent_and_has_no_recipe_job_foreign_key() {
        let mut store = DaemonStore::open_in_memory().unwrap();
        assert!(store
            .reserve_delivery_payload("workflow", "delivery", None, "{}", 1)
            .unwrap());
        store
            .activate_delivery_payload("workflow", "delivery")
            .unwrap();
        store
            .mark_delivery_submitted_external("workflow", "delivery")
            .unwrap();
        store
            .mark_delivery_submitted_external("workflow", "delivery")
            .unwrap();
        let stored = store
            .connection
            .query_row(
                "SELECT status,job_id FROM daemon_delivery_payloads
                 WHERE trigger_id='workflow' AND delivery_id='delivery'",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .unwrap();
        assert_eq!(stored, ("submitted".to_string(), None));
        assert!(store.pending_delivery_payloads(1).unwrap().is_empty());
    }
}
