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
    /// Why admission last passed this job over, or `None` when it never has.
    ///
    /// Separate from `last_error` on purpose: that column is sticky
    /// (`transition` writes it with `COALESCE`), so a hold left in it would
    /// still be there when the job later succeeded and would be read back as
    /// that success's exit reason.
    pub hold_reason: Option<String>,
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
            .execute_batch(DAEMON_PRAGMAS)
            .map_err(|error| format!("Failed to configure daemon state: {error}"))?;
        apply_daemon_migrations(&connection)?;
        restrict_file(&paths.state_db)?;
        Ok(Self { connection })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, String> {
        let connection = Connection::open_in_memory().map_err(|error| error.to_string())?;
        connection
            .execute_batch(DAEMON_PRAGMAS)
            .map_err(|error| error.to_string())?;
        apply_daemon_migrations(&connection)?;
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
                        repository_policy_json, worktree_json, parent_run_id, last_error, hold_reason
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
                        repository_policy_json, worktree_json, parent_run_id, last_error, hold_reason
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
                        repository_policy_json, worktree_json, parent_run_id, last_error, hold_reason
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
                        repository_policy_json, worktree_json, parent_run_id, last_error, hold_reason
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

    /// Claim memory for an admitted job, durably (K7).
    ///
    /// Also clears `hold_reason`: whatever admission was waiting for, it is not
    /// waiting for it any more.
    pub fn record_reservation(
        &mut self,
        job_id: &str,
        model_key: &str,
        ram_bytes: u64,
        vram_bytes: u64,
    ) -> Result<(), String> {
        self.connection
            .execute(
                "UPDATE daemon_jobs
                 SET reservation_model_key=?2, reservation_ram_bytes=?3,
                     reservation_vram_bytes=?4, hold_reason=NULL
                 WHERE job_id=?1",
                params![job_id, model_key, to_i64(ram_bytes)?, to_i64(vram_bytes)?],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Give the claim back. Idempotent, because more than one exit path reaches
    /// it for the same job.
    pub fn release_reservation(&mut self, job_id: &str) -> Result<(), String> {
        self.connection
            .execute(
                "UPDATE daemon_jobs
                 SET reservation_model_key=NULL, reservation_ram_bytes=NULL,
                     reservation_vram_bytes=NULL
                 WHERE job_id=?1",
                [job_id],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// One row per *resident model*, not per job: `(model_key, ram, vram)`.
    ///
    /// The `GROUP BY` is the whole point. Two queued turns against one local
    /// model make it resident once, so they must be charged once — and the
    /// release rule falls out of the same grouping rather than needing its own
    /// bookkeeping: the model's row disappears when the last job holding that
    /// key leaves an active state, not when the first one does.
    ///
    /// `MAX` rather than any single row's value because two holders of one model
    /// were admitted against different hardware snapshots and may have recorded
    /// marginally different claims; the larger is the conservative one.
    pub fn committed_reservations(&self) -> Result<Vec<(String, u64, u64)>, String> {
        let mut statement = self
            .connection
            .prepare(&format!(
                "SELECT reservation_model_key,
                        MAX(COALESCE(reservation_ram_bytes, 0)),
                        MAX(COALESCE(reservation_vram_bytes, 0))
                 FROM daemon_jobs
                 WHERE reservation_model_key IS NOT NULL
                   AND state IN {DAEMON_ACTIVE_STATES}
                 GROUP BY reservation_model_key"
            ))
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    from_i64(row.get(1)?)?,
                    from_i64(row.get(2)?)?,
                ))
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    /// Drop reservations left behind by a daemon that died holding them.
    ///
    /// `committed_reservations` already ignores a row whose job is no longer
    /// active, so this is not what makes the accounting correct — it is what
    /// keeps the columns honest for anything else that reads them, and it is the
    /// only thing that would notice a row whose job reached a terminal state by
    /// some path that never released it. Returns how many were swept.
    pub fn sweep_stale_reservations(&mut self) -> Result<usize, String> {
        self.connection
            .execute(
                &format!(
                    "UPDATE daemon_jobs
                     SET reservation_model_key=NULL, reservation_ram_bytes=NULL,
                         reservation_vram_bytes=NULL
                     WHERE reservation_model_key IS NOT NULL
                       AND state NOT IN {DAEMON_ACTIVE_STATES}"
                ),
                [],
            )
            .map_err(|error| error.to_string())
    }

    /// `(job_id, model_key, ram, vram)` for every job currently holding a claim.
    ///
    /// Per job rather than per resident model, which is the opposite of
    /// [`Self::committed_reservations`] and deliberately so: the committed total
    /// asks "how much is held", and preemption asks "who is holding it", which
    /// needs the holders spelled out. It is also how the engine learns that a
    /// model has more than one holder — and therefore that suspending any one of
    /// them frees nothing.
    pub fn job_reservations(&self) -> Result<Vec<(String, String, u64, u64)>, String> {
        let mut statement = self
            .connection
            .prepare(&format!(
                "SELECT job_id, reservation_model_key,
                        COALESCE(reservation_ram_bytes, 0),
                        COALESCE(reservation_vram_bytes, 0)
                 FROM daemon_jobs
                 WHERE reservation_model_key IS NOT NULL
                   AND state IN {DAEMON_ACTIVE_STATES}"
            ))
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    from_i64(row.get(2)?)?,
                    from_i64(row.get(3)?)?,
                ))
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    /// Jobs waiting to start: the backlog the backpressure signal compares
    /// `held_count` against, and the only counter that can tell "the queue is
    /// deep" from "everything in it is stuck".
    pub fn queued_count(&self) -> Result<u32, String> {
        self.connection
            .query_row(
                "SELECT COUNT(*) FROM daemon_jobs WHERE state IN ('preparing','queued')",
                [],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())
    }

    /// How many queued jobs admission is currently refusing for resources.
    ///
    /// The backpressure signal's "the machine is full, not the queue" case, and
    /// the only counter that can tell those two apart.
    pub fn held_count(&self) -> Result<u32, String> {
        self.connection
            .query_row(
                "SELECT COUNT(*) FROM daemon_jobs
                 WHERE state='queued' AND hold_reason IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())
    }

    /// Record why admission passed a queued job over, so "held for memory" is
    /// distinguishable from "not looked at yet" without inventing a job state.
    pub fn record_hold(&mut self, job_id: &str, reason: Option<&str>) -> Result<(), String> {
        self.connection
            .execute(
                "UPDATE daemon_jobs SET hold_reason=?2 WHERE job_id=?1",
                params![job_id, reason],
            )
            .map_err(|error| error.to_string())?;
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
                        repository_policy_json, worktree_json, parent_run_id, last_error, hold_reason
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
                            repository_policy_json, worktree_json, parent_run_id, last_error, hold_reason
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
                        repository_policy_json, worktree_json, parent_run_id, last_error, hold_reason
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

/// Outcome tokens for [`SchedulerDecision::outcome`]. Spelled once here because
/// the migration's `CHECK` constraint spells them too.
pub const DECISION_ADMITTED: &str = "admitted";
pub const DECISION_HELD: &str = "held";
pub const DECISION_PREEMPTED: &str = "preempted";
pub const DECISION_RESUMED: &str = "resumed";
pub const DECISION_REJECTED: &str = "rejected";

/// How many decisions are retained. Older ones are deleted on insert.
///
/// A decision log has to be bounded or it becomes the largest table in the
/// database: at four ticks a second with anything queued, an unbounded log grows
/// by tens of thousands of rows an hour. 512 is a few minutes of a busy queue,
/// which is the window in which anyone actually asks "why did that run go
/// first".
pub const MAX_SCHEDULER_DECISIONS: i64 = 512;

/// One arbitration decision, after the fact.
///
/// The point of the row is the last three fields. "Job A was admitted" is not
/// inspectable; "job A was admitted over B and C because available RAM read
/// 9.2 GiB at 15:04:02" is. `measurement` names *which* number decided it and
/// `measured_at_ms` is that number's own observation time, not the time this row
/// was written — a re-derived guess with a fresh timestamp is exactly the thing
/// this column exists to rule out.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SchedulerDecision {
    pub decided_at_ms: u64,
    pub job_id: String,
    pub outcome: String,
    /// The class the run's frozen kind declares.
    pub process_class: String,
    /// That class after aging promotion, which is what the ranking actually used.
    pub effective_class: String,
    pub workspace: Option<String>,
    /// What this job was chosen over, most-nearly-chosen first. Bounded when
    /// written — a decision row must not grow with the queue.
    pub passed_over: Vec<String>,
    pub detail: String,
    pub measurement: String,
    pub measured_value: Option<u64>,
    pub measured_at_ms: Option<u64>,
}

impl DaemonStore {
    /// Append a decision and drop anything past [`MAX_SCHEDULER_DECISIONS`].
    pub fn record_decision(&mut self, decision: &SchedulerDecision) -> Result<(), String> {
        let passed_over =
            serde_json::to_string(&decision.passed_over).map_err(|error| error.to_string())?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT INTO daemon_scheduler_decisions (
                    decided_at_ms, job_id, outcome, process_class, effective_class,
                    workspace, passed_over_json, detail, measurement, measured_value,
                    measured_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    to_i64(decision.decided_at_ms)?,
                    decision.job_id,
                    decision.outcome,
                    decision.process_class,
                    decision.effective_class,
                    decision.workspace,
                    passed_over,
                    decision.detail,
                    decision.measurement,
                    decision.measured_value.map(to_i64).transpose()?,
                    decision.measured_at_ms.map(to_i64).transpose()?,
                ],
            )
            .map_err(|error| format!("Failed to record scheduling decision: {error}"))?;
        transaction
            .execute(
                "DELETE FROM daemon_scheduler_decisions WHERE decision_id <=
                 (SELECT MAX(decision_id) FROM daemon_scheduler_decisions) - ?1",
                [MAX_SCHEDULER_DECISIONS],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())
    }

    /// The newest decisions first.
    pub fn recent_decisions(&self, limit: u32) -> Result<Vec<SchedulerDecision>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT decided_at_ms, job_id, outcome, process_class, effective_class,
                        workspace, passed_over_json, detail, measurement, measured_value,
                        measured_at_ms
                 FROM daemon_scheduler_decisions
                 ORDER BY decision_id DESC LIMIT ?1",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([i64::from(limit.clamp(1, 512))], |row| {
                Ok(SchedulerDecision {
                    decided_at_ms: from_i64(row.get(0)?)?,
                    job_id: row.get(1)?,
                    outcome: row.get(2)?,
                    process_class: row.get(3)?,
                    effective_class: row.get(4)?,
                    workspace: row.get(5)?,
                    passed_over: serde_json::from_str(&row.get::<_, String>(6)?)
                        .unwrap_or_default(),
                    detail: row.get(7)?,
                    measurement: row.get(8)?,
                    measured_value: row.get::<_, Option<i64>>(9)?.map(from_i64).transpose()?,
                    measured_at_ms: row.get::<_, Option<i64>>(10)?.map(from_i64).transpose()?,
                })
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
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
        hold_reason: row.get(21)?,
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

const DAEMON_PRAGMAS: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
"#;

const DAEMON_V1: i64 = 1;
const DAEMON_V1_CHECKSUM: &str = "daemon-jobs-v1";

const DAEMON_V1_SQL: &str = r#"
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

const DAEMON_V2: i64 = 2;
const DAEMON_V2_CHECKSUM: &str = "daemon-jobs-v2-reservations";

/// Resource-aware admission (K7) needs two facts to outlive the daemon process:
/// what an admitted job holds, and why a queued one is still queued.
///
/// `reservation_model_key` is the identity of the *resident model*, not of the
/// job, which is what makes the committed total a `GROUP BY` rather than a `SUM`
/// — see `committed_reservations`.
///
/// Added by `ALTER` rather than by rebuilding the table, which costs the ability
/// to express "all three reservation columns are set together" as a SQL `CHECK`
/// (SQLite cannot add a constraint to an existing table). No loss worth
/// rebuilding a live queue for: `record_reservation` is the only writer and sets
/// all three in one statement.
const DAEMON_V2_SQL: &str = r#"
ALTER TABLE daemon_jobs ADD COLUMN reservation_model_key TEXT;
ALTER TABLE daemon_jobs ADD COLUMN reservation_ram_bytes INTEGER;
ALTER TABLE daemon_jobs ADD COLUMN reservation_vram_bytes INTEGER;
ALTER TABLE daemon_jobs ADD COLUMN hold_reason TEXT;
"#;

const DAEMON_V3: i64 = 3;
const DAEMON_V3_CHECKSUM: &str = "daemon-jobs-v3-scheduler-decisions";

/// The scheduler's decision log (K8): which job was chosen, what it was chosen
/// over, and which measurement decided it.
///
/// Its own table rather than more columns on `daemon_jobs`, because a job has
/// many decisions over its life — held on nine ticks, preempted, resumed,
/// admitted — and a column can only hold the last one. `hold_reason` is the
/// degenerate single-slot version of this and stays where it is: the ready-queue
/// gate reads it in SQL, and it answers "why is this job still queued right now",
/// which is a live question rather than a historical one.
///
/// `decision_id` is an `INTEGER PRIMARY KEY` and therefore the rowid, which is
/// what makes the retention delete in `record_decision` a single indexed range
/// scan rather than an ordered scan of the whole table.
const DAEMON_V3_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS daemon_scheduler_decisions (
    decision_id INTEGER PRIMARY KEY,
    decided_at_ms INTEGER NOT NULL CHECK (decided_at_ms > 0),
    job_id TEXT NOT NULL,
    outcome TEXT NOT NULL CHECK (outcome IN (
        'admitted','held','preempted','resumed','rejected'
    )),
    process_class TEXT NOT NULL CHECK (process_class IN (
        'interactive','batch','background','maintenance'
    )),
    effective_class TEXT NOT NULL CHECK (effective_class IN (
        'interactive','batch','background','maintenance'
    )),
    workspace TEXT,
    passed_over_json TEXT NOT NULL,
    detail TEXT NOT NULL,
    measurement TEXT NOT NULL,
    measured_value INTEGER,
    measured_at_ms INTEGER
) STRICT;

CREATE INDEX IF NOT EXISTS daemon_scheduler_decisions_job_idx
    ON daemon_scheduler_decisions(job_id, decision_id DESC);
"#;

/// Every migration in order, so applying them is a loop rather than a stanza per
/// version. Mirrors the shape `denial_sink` and the run ledger already use, and
/// pays off the debt `DaemonEngine::recover`'s comment flagged: before this,
/// `daemon-v1.sqlite3` was one `CREATE TABLE IF NOT EXISTS` with no version key,
/// so neither a new state nor a new column could be added at all.
///
/// V1 is the pre-existing schema. Recording it is safe on a database that
/// already has those tables because every statement in it is
/// `CREATE ... IF NOT EXISTS`, so the first run against an old file writes the
/// version row and changes nothing else.
const DAEMON_MIGRATIONS: &[(i64, &str, &str)] = &[
    (DAEMON_V1, DAEMON_V1_CHECKSUM, DAEMON_V1_SQL),
    (DAEMON_V2, DAEMON_V2_CHECKSUM, DAEMON_V2_SQL),
    (DAEMON_V3, DAEMON_V3_CHECKSUM, DAEMON_V3_SQL),
];

/// Latest version this build understands. The forward-only guard compares
/// against this rather than a specific version, so adding V4 needs no edit
/// there.
const DAEMON_LATEST: i64 = DAEMON_V3;

/// Active states, spelled once. A reservation is held for exactly as long as the
/// job is in one of them, which is what releases it on any exit path — clean,
/// crashed, or reconciled — without each of those paths having to remember to.
const DAEMON_ACTIVE_STATES: &str = "('running','waiting_approval','paused','cancelling')";

fn apply_daemon_migrations(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS daemon_migrations (
                version INTEGER PRIMARY KEY,
                checksum TEXT NOT NULL,
                applied_at_ms INTEGER NOT NULL
             ) STRICT;",
        )
        .map_err(|error| format!("Failed to open daemon migrations: {error}"))?;

    if let Some(version) = connection
        .query_row("SELECT MAX(version) FROM daemon_migrations", [], |row| {
            row.get::<_, Option<i64>>(0)
        })
        .map_err(|error| error.to_string())?
    {
        // Forward-only. A rolled-back build meeting a newer queue refuses to
        // open it rather than reading columns it does not know are there.
        if version > DAEMON_LATEST {
            return Err(format!(
                "Daemon state was written by a newer build (schema v{version}); upgrade monkey or remove the daemon state file"
            ));
        }
    }

    for &(version, checksum, sql) in DAEMON_MIGRATIONS {
        if let Some(recorded) = connection
            .query_row(
                "SELECT checksum FROM daemon_migrations WHERE version = ?1",
                [version],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
        {
            // Already applied. Still checked rather than skipped: a schema
            // edited in place instead of added as a new version is the mistake
            // worth failing on, and only the recorded checksum can see it.
            if recorded != checksum {
                return Err(format!(
                    "Daemon schema v{version} was edited in place; add a new version instead"
                ));
            }
            continue;
        }
        connection
            .execute_batch(sql)
            .map_err(|error| format!("Failed to migrate daemon state to v{version}: {error}"))?;
        connection
            .execute(
                "INSERT INTO daemon_migrations (version, checksum, applied_at_ms)
                 VALUES (?1, ?2, ?3)",
                params![version, checksum, 1_i64],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A queue written by a build that predates the migration table must upgrade
    /// in place and keep its rows. This is the case the daemon store had no way
    /// to handle at all before K7.
    #[test]
    fn a_pre_migration_database_upgrades_in_place_and_keeps_its_jobs() {
        let connection = Connection::open_in_memory().unwrap();
        // Exactly what an older build left behind: V1's tables and no version key.
        connection.execute_batch(DAEMON_V1_SQL).unwrap();
        connection
            .execute_batch(
                "INSERT INTO daemon_jobs (
                    job_id, recipe_snapshot, state, priority, attempt, max_attempts,
                    created_at_ms, updated_at_ms, max_runtime_ms, max_log_bytes
                 ) VALUES ('old', '/old.json', 'running', 0, 1, 1, 1, 1, 60000, 1024);",
            )
            .unwrap();

        apply_daemon_migrations(&connection).unwrap();

        let store = DaemonStore { connection };
        let job = store.get_job("old").unwrap().unwrap();
        assert_eq!(job.state, JobState::Running, "the row survives the upgrade");
        assert_eq!(job.hold_reason, None, "the new column reads as unset");
        assert!(store.committed_reservations().unwrap().is_empty());
    }

    /// Re-running the loop is a no-op, and a checksum that no longer matches its
    /// recorded version is the mistake worth failing on.
    #[test]
    fn migrations_are_idempotent_and_refuse_a_schema_edited_in_place() {
        let connection = Connection::open_in_memory().unwrap();
        apply_daemon_migrations(&connection).unwrap();
        apply_daemon_migrations(&connection).unwrap();

        connection
            .execute(
                "UPDATE daemon_migrations SET checksum='tampered' WHERE version=?1",
                [DAEMON_V2],
            )
            .unwrap();
        let error = apply_daemon_migrations(&connection).unwrap_err();
        assert!(
            error.contains("edited in place"),
            "expected an in-place-edit refusal, got {error:?}"
        );
    }

    #[test]
    fn a_reservation_is_charged_once_per_resident_model_and_swept_when_idle() {
        let mut store = DaemonStore::open_in_memory().unwrap();
        for id in ["one", "two"] {
            store.insert_preparing(&new_job(id, 1), 8).unwrap();
            store.mark_queued(id, &format!("run-{id}"), 1).unwrap();
            store
                .transition(id, JobState::Running, 2, Some(7), None)
                .unwrap();
            store
                .record_reservation(id, "shared-model", 1_024, 512)
                .unwrap();
        }
        assert_eq!(
            store.committed_reservations().unwrap(),
            vec![("shared-model".to_string(), 1_024, 512)],
            "two jobs, one resident model, one charge"
        );

        // The first holder leaving does not free the model.
        store.release_reservation("one").unwrap();
        store
            .transition("one", JobState::Succeeded, 3, None, None)
            .unwrap();
        assert_eq!(store.committed_reservations().unwrap().len(), 1);

        // A terminal job that somehow kept its columns is swept.
        store
            .transition("two", JobState::Failed, 4, None, Some("crash"))
            .unwrap();
        assert_eq!(store.sweep_stale_reservations().unwrap(), 1);
        assert!(store.committed_reservations().unwrap().is_empty());
    }

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
    fn attempt_counts_starts_not_arrivals_at_running() {
        let mut store = DaemonStore::open_in_memory().unwrap();
        let mut job = new_job("counted", 1);
        job.max_attempts = 5;
        store.insert_preparing(&job, 8).unwrap();
        store.mark_queued("counted", "run-counted", 1).unwrap();
        let attempt = |store: &DaemonStore| store.get_job("counted").unwrap().unwrap().attempt;

        assert_eq!(attempt(&store), 0, "a queued job has started nothing yet");

        // Leaving the queue is the only edge that starts an attempt.
        store
            .transition("counted", JobState::Running, 2, Some(10), None)
            .unwrap();
        assert_eq!(attempt(&store), 1);

        // Every other arrival at `running` is the same attempt resuming. This
        // used to increment, which spent the job's retry budget without it ever
        // failing, and charged it a `backoff_elapsed` wait it never earned.
        for interrupted in [JobState::Paused, JobState::WaitingApproval] {
            store
                .transition("counted", interrupted, 3, Some(10), None)
                .unwrap();
            store
                .transition("counted", JobState::Running, 4, Some(10), None)
                .unwrap();
            assert_eq!(
                attempt(&store),
                1,
                "returning from {interrupted:?} is not a new attempt"
            );
        }

        // A real retry does count: back to the queue, then out of it again.
        store
            .transition("counted", JobState::Queued, 5, None, Some("boom"))
            .unwrap();
        assert_eq!(attempt(&store), 1, "requeueing is not itself a start");
        store
            .transition("counted", JobState::Running, 6, Some(11), None)
            .unwrap();
        assert_eq!(attempt(&store), 2);
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

    /// The log has to stay bounded, and it has to keep the *newest* rows —
    /// dropping the newest would leave a log that answers questions nobody has.
    #[test]
    fn the_decision_log_is_bounded_and_retains_the_newest() {
        let mut store = DaemonStore::open_in_memory().unwrap();
        let total = u64::try_from(MAX_SCHEDULER_DECISIONS).unwrap() + 40;
        for index in 1..=total {
            store
                .record_decision(&SchedulerDecision {
                    decided_at_ms: index,
                    job_id: format!("job-{index}"),
                    outcome: DECISION_ADMITTED.to_string(),
                    process_class: "batch".to_string(),
                    effective_class: "batch".to_string(),
                    workspace: Some("/work".to_string()),
                    passed_over: vec!["job-other".to_string()],
                    detail: "fixture".to_string(),
                    measurement: "available_ram_bytes".to_string(),
                    measured_value: Some(1_024),
                    measured_at_ms: Some(index),
                })
                .unwrap();
        }
        let retained = store.recent_decisions(512).unwrap();
        assert_eq!(
            i64::try_from(retained.len()).unwrap(),
            MAX_SCHEDULER_DECISIONS
        );
        assert_eq!(retained[0].job_id, format!("job-{total}"), "newest first");
        assert_eq!(retained[0].passed_over, ["job-other"]);
        assert_eq!(retained[0].measured_at_ms, Some(total));

        // The wire spelling `--json` prints, which the desktop mirror
        // (`DesktopSchedulerDecision` in the library's `daemon_commands.rs`)
        // decodes: camelCase throughout, and no `decision_id` — the column exists
        // but `recent_decisions` deliberately does not select it.
        let wire = serde_json::to_value(&retained[0]).unwrap();
        assert!(wire.get("passedOver").is_some() && wire.get("measuredAtMs").is_some());
        assert!(wire.get("measured_at_ms").is_none() && wire.get("decisionId").is_none());
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
