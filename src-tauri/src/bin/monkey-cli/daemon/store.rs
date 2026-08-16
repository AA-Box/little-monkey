use std::path::{Path, PathBuf};

use little_monkey_lib::run_ledger::RunLedger;
use little_monkey_lib::run_protocol::RunStatus;
use little_monkey_lib::runtime_adapter::AcceleratorKind;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use super::admission::DeviceClaim;

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
    /// The profile data directory these paths sit under — `root`'s parent.
    ///
    /// Several subsystems the daemon reaches into (the companion's voice
    /// configuration, the shared artifact store, the extension registry) live
    /// beside `daemon/`, not inside it. Handing them `root` produces a second,
    /// empty copy of that state under `daemon/` that nothing else ever reads,
    /// which is a silent wrong answer rather than an error, so the resolution
    /// lives here once instead of at each call site.
    pub fn app_data(&self) -> Result<&Path, String> {
        self.root
            .parent()
            .ok_or_else(|| "Daemon root has no app-data parent".to_string())
    }

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
    /// Visible to the rest of `daemon` so subsystem-specific storage (see
    /// `daemon::channel_store`) can add `impl DaemonStore` blocks in their own
    /// file instead of growing this one without bound.
    pub(super) connection: Connection,
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

    /// Jobs whose state was touched at or after `since_ms`, oldest first.
    ///
    /// The notification watcher's read: it needs the *terminal* transitions,
    /// which `active_jobs` by definition cannot show it, and it needs them
    /// without scanning the whole table on every tick.
    pub fn jobs_updated_since(&self, since_ms: u64, limit: u32) -> Result<Vec<DaemonJob>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT job_id, run_id, recipe_snapshot, state, priority, attempt,
                        max_attempts, created_at_ms, updated_at_ms, started_at_ms,
                        finished_at_ms, process_id, max_runtime_ms, max_memory_bytes,
                        max_log_bytes, pause_requested, cancel_requested,
                        repository_policy_json, worktree_json, parent_run_id, last_error, hold_reason
                 FROM daemon_jobs WHERE updated_at_ms >= ?1
                 ORDER BY updated_at_ms ASC, job_id ASC LIMIT ?2",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(params![to_i64(since_ms)?, i64::from(limit)], read_job)
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
        devices: &[DeviceClaim],
    ) -> Result<(), String> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "UPDATE daemon_jobs
                 SET reservation_model_key=?2, reservation_ram_bytes=?3,
                     reservation_vram_bytes=?4, hold_reason=NULL
                 WHERE job_id=?1",
                params![job_id, model_key, to_i64(ram_bytes)?, to_i64(vram_bytes)?],
            )
            .map_err(|error| error.to_string())?;
        // Replaced rather than merged: a re-record is a *restatement* of what
        // this job holds (a resume re-books the same claim), and merging would
        // double it every time a suspended job came back.
        transaction
            .execute(
                "DELETE FROM daemon_job_device_reservations WHERE job_id=?1",
                [job_id],
            )
            .map_err(|error| error.to_string())?;
        for claim in devices {
            transaction
                .execute(
                    "INSERT INTO daemon_job_device_reservations
                         (job_id, accelerator, device_index, bytes)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        job_id,
                        accelerator_token(claim.device.kind),
                        // -1 is "no device enumerated" — see `DAEMON_V4_SQL`.
                        claim.device.index.map(i64::from).unwrap_or(-1),
                        to_i64(claim.bytes)?,
                    ],
                )
                .map_err(|error| error.to_string())?;
        }
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Give the claim back. Idempotent, because more than one exit path reaches
    /// it for the same job.
    pub fn release_reservation(&mut self, job_id: &str) -> Result<(), String> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "UPDATE daemon_jobs
                 SET reservation_model_key=NULL, reservation_ram_bytes=NULL,
                     reservation_vram_bytes=NULL
                 WHERE job_id=?1",
                [job_id],
            )
            .map_err(|error| error.to_string())?;
        // The device rows go back on the *same* paths and in the same
        // transaction, so a release cannot leave a card booked by a job that is
        // gone. Both `finish_active` and `reconcile_interrupted` reach here —
        // the clean exit and the crash funnel — which is what makes that true of
        // every exit rather than of the tidy ones.
        transaction
            .execute(
                "DELETE FROM daemon_job_device_reservations WHERE job_id=?1",
                [job_id],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(())
    }

    /// `job_id -> device claims`, for every job currently holding one (K15).
    ///
    /// Per job rather than per device, which is the opposite of
    /// [`Self::committed_device_reservations`] and deliberately so, for exactly
    /// the reason [`Self::job_reservations`] gives: the committed total asks "how
    /// much is held on this card", and preemption asks "who is holding it".
    pub fn job_device_reservations(
        &self,
    ) -> Result<std::collections::HashMap<String, Vec<DeviceClaim>>, String> {
        let mut statement = self
            .connection
            .prepare(&format!(
                "SELECT d.job_id, d.accelerator, d.device_index, d.bytes
                   FROM daemon_job_device_reservations d
                   JOIN daemon_jobs j ON j.job_id = d.job_id
                  WHERE j.reservation_model_key IS NOT NULL
                    AND j.state IN {DAEMON_ACTIVE_STATES}"
            ))
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    from_i64(row.get(3)?)?,
                ))
            })
            .map_err(|error| error.to_string())?;
        let mut claims: std::collections::HashMap<String, Vec<DeviceClaim>> =
            std::collections::HashMap::new();
        for row in rows {
            let (job_id, token, index, bytes) = row.map_err(|error| error.to_string())?;
            let Some(kind) = accelerator_from_token(&token) else {
                continue;
            };
            let index = if index < 0 {
                None
            } else {
                u32::try_from(index).ok()
            };
            claims.entry(job_id).or_default().push(DeviceClaim {
                device: super::admission::DeviceId { kind, index },
                bytes,
            });
        }
        Ok(claims)
    }

    /// What each accelerator device holds right now, deduplicated by resident
    /// model exactly as [`Self::committed_reservations`] is (K15).
    ///
    /// `(kind, device_index, bytes)`, with `None` for a machine that enumerated
    /// no device. The two-level grouping is the point: `MAX` within a model —
    /// two holders of one model recorded marginally different claims and the
    /// larger is the conservative one — then `SUM` across *distinct* models,
    /// which is what a card actually has to hold.
    pub fn committed_device_reservations(
        &self,
    ) -> Result<Vec<(AcceleratorKind, Option<u32>, u64)>, String> {
        let mut statement = self
            .connection
            .prepare(&format!(
                "SELECT accelerator, device_index, SUM(bytes) FROM (
                     SELECT d.accelerator AS accelerator,
                            d.device_index AS device_index,
                            MAX(d.bytes) AS bytes
                       FROM daemon_job_device_reservations d
                       JOIN daemon_jobs j ON j.job_id = d.job_id
                      WHERE j.reservation_model_key IS NOT NULL
                        AND j.state IN {DAEMON_ACTIVE_STATES}
                      GROUP BY j.reservation_model_key, d.accelerator, d.device_index
                 )
                 GROUP BY accelerator, device_index"
            ))
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    from_i64(row.get(2)?)?,
                ))
            })
            .map_err(|error| error.to_string())?;
        let mut committed = Vec::new();
        for row in rows {
            let (token, index, bytes) = row.map_err(|error| error.to_string())?;
            // An unreadable accelerator token is skipped rather than guessed at:
            // charging bytes to the wrong card is worse than not charging them.
            let Some(kind) = accelerator_from_token(&token) else {
                continue;
            };
            let index = if index < 0 {
                None
            } else {
                u32::try_from(index).ok()
            };
            committed.push((kind, index, bytes));
        }
        Ok(committed)
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
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        // Device rows first, because the `UPDATE` below is what makes the jobs
        // stop looking stale — doing it after would leave nothing to select on.
        transaction
            .execute(
                &format!(
                    "DELETE FROM daemon_job_device_reservations
                      WHERE job_id IN (
                          SELECT job_id FROM daemon_jobs
                           WHERE reservation_model_key IS NOT NULL
                             AND state NOT IN {DAEMON_ACTIVE_STATES}
                      )"
                ),
                [],
            )
            .map_err(|error| error.to_string())?;
        let swept = transaction
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
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(swept)
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

/// The stored token for an accelerator kind.
///
/// Its own function rather than serde, because this string is a *database* value
/// and must never change when a serde attribute is reworded. Round-tripped by
/// [`accelerator_from_token`], and pinned by a test.
fn accelerator_token(kind: AcceleratorKind) -> &'static str {
    match kind {
        AcceleratorKind::Cpu => "cpu",
        AcceleratorKind::Metal => "metal",
        AcceleratorKind::Cuda => "cuda",
        AcceleratorKind::Rocm => "rocm",
        AcceleratorKind::Vulkan => "vulkan",
        AcceleratorKind::DirectMl => "directml",
        AcceleratorKind::AppleNeuralEngine => "apple-neural-engine",
    }
}

/// The inverse. `None` for a token this build does not know, which is a row
/// written by a newer daemon — skipped rather than guessed at, because charging
/// bytes to the wrong card is worse than not charging them.
fn accelerator_from_token(token: &str) -> Option<AcceleratorKind> {
    Some(match token {
        "cpu" => AcceleratorKind::Cpu,
        "metal" => AcceleratorKind::Metal,
        "cuda" => AcceleratorKind::Cuda,
        "rocm" => AcceleratorKind::Rocm,
        "vulkan" => AcceleratorKind::Vulkan,
        "directml" => AcceleratorKind::DirectMl,
        "apple-neural-engine" => AcceleratorKind::AppleNeuralEngine,
        _ => return None,
    })
}

const DAEMON_V4: i64 = 4;
const DAEMON_V4_CHECKSUM: &str = "daemon-jobs-v4-device-reservations";

/// Each accelerator device as a thing the scheduler reserves against (K15).
///
/// `reservation_vram_bytes` is one number per job, so two 24 GB cards read as one
/// 48 GB pool and a second job was admitted against capacity the first had
/// already exhausted on one card. That figure stays — it is the pooled total a
/// caller with nothing to say about devices still reads — and this table is what
/// nothing may sum: one row per (job, device).
///
/// # Why a table rather than three more columns
///
/// A column can hold one device. A split model holds bytes on several, and the
/// count is a property of the machine rather than of the schema, so columns would
/// have to be widened by whoever first plugs in a fourth card.
///
/// `device_index` is the *runtime's* ordinal — what `--main-gpu` and a position
/// in `--tensor-split` mean — and `-1` stands for "this machine advertised
/// accelerator memory but enumerated no device". A sentinel rather than NULL
/// because it is half of the primary key, and SQLite permits NULLs in the columns
/// of a non-`INTEGER` primary key, which would stop it deduplicating.
///
/// `ON DELETE CASCADE` on the job, unlike the rest of this schema: these rows are
/// not a record in their own right, they are a property of the reservation, and
/// a job that is ever pruned should take them with it.
const DAEMON_V4_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS daemon_job_device_reservations (
    job_id TEXT NOT NULL REFERENCES daemon_jobs(job_id) ON DELETE CASCADE,
    accelerator TEXT NOT NULL CHECK (length(accelerator) > 0),
    device_index INTEGER NOT NULL CHECK (device_index >= -1),
    bytes INTEGER NOT NULL CHECK (bytes >= 0),
    PRIMARY KEY(job_id, accelerator, device_index)
) STRICT;
"#;

const DAEMON_V5: i64 = 5;
const DAEMON_V5_CHECKSUM: &str = "daemon-jobs-v5-channels";

/// The messaging channel subsystem's durable state.
///
/// It lives in the daemon store rather than the run ledger because every writer
/// is the daemon: an account is polled by a daemon worker, an inbound event is
/// deduplicated before it becomes a job, and an outbox row is retried by the
/// same loop that retries jobs. The run ledger records what a *run* did; these
/// tables record what the outside world said and what we said back.
///
/// # Secrets
///
/// No table here has a column for a token, and none may grow one.
/// `credential_ref` is a keychain account name — the same shape `connectors`
/// uses — so the database is safe to copy into a support bundle and a leaked
/// file grants nothing.
///
/// # Deduplication
///
/// `channel_events` is the durable dedupe authority for everything inbound:
/// `UNIQUE(source, account_id, direction, provider_event_id)` means a
/// redelivered webhook, a replayed polling window, and a provider echo of our
/// own message all collapse onto the row that is already there. The daemon
/// queue's `deterministic_job_id` is the second line of defense, not the first.
///
/// # Outbox
///
/// `needs_reconciliation` is a state rather than a flavor of `failed` because
/// its meaning is the opposite: the send may have *succeeded*, so retrying
/// risks duplicating an external effect. Nothing retries that state
/// automatically. `UNIQUE(account_id, idempotency_key)` is what makes a
/// crash between "row queued" and "row sent" recoverable at all.
///
/// # Cursors
///
/// Transport resume state (a Telegram update offset, a Slack cursor, a Matrix
/// sync token) is bounded and per-account. `CHECK (length(cursor_value) <=
/// 4096)` keeps a provider from turning a resume token into unbounded storage.
const DAEMON_V5_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS channel_accounts (
    account_id TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK (length(kind) > 0),
    label TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
    non_secret_config_json TEXT NOT NULL,
    credential_ref TEXT,
    access_policy_json TEXT NOT NULL,
    health TEXT NOT NULL CHECK (health IN (
        'unconfigured','disconnected','connecting','connected','degraded','unsupported','error'
    )),
    health_detail TEXT,
    last_error TEXT,
    last_probe_at_ms INTEGER,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0)
) STRICT;

CREATE INDEX IF NOT EXISTS channel_accounts_kind_idx ON channel_accounts(kind, enabled);

CREATE TABLE IF NOT EXISTS channel_sender_authorizations (
    account_id TEXT NOT NULL REFERENCES channel_accounts(account_id) ON DELETE CASCADE,
    sender_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending','approved','blocked')),
    pairing_code_digest TEXT,
    requested_at_ms INTEGER NOT NULL CHECK (requested_at_ms > 0),
    expires_at_ms INTEGER,
    approved_at_ms INTEGER,
    blocked_at_ms INTEGER,
    display_label TEXT,
    metadata_json TEXT NOT NULL,
    PRIMARY KEY(account_id, sender_id)
) STRICT;

CREATE INDEX IF NOT EXISTS channel_sender_pending_idx
    ON channel_sender_authorizations(account_id, state, requested_at_ms);

CREATE TABLE IF NOT EXISTS channel_routes (
    route_id TEXT PRIMARY KEY,
    scope_json TEXT NOT NULL UNIQUE,
    target_json TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0)
) STRICT;

CREATE TABLE IF NOT EXISTS channel_session_map (
    session_key TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES channel_accounts(account_id) ON DELETE CASCADE,
    conversation_id TEXT NOT NULL,
    thread_id TEXT,
    session_id TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    last_used_at_ms INTEGER NOT NULL CHECK (last_used_at_ms > 0)
) STRICT;

CREATE INDEX IF NOT EXISTS channel_session_account_idx
    ON channel_session_map(account_id, conversation_id);

CREATE TABLE IF NOT EXISTS channel_events (
    event_id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES channel_accounts(account_id) ON DELETE CASCADE,
    source TEXT NOT NULL CHECK (source IN (
        'desktop','mobile','messaging_channel','peer','voice','telephone'
    )),
    direction TEXT NOT NULL CHECK (direction IN ('inbound','outbound')),
    provider_event_id TEXT NOT NULL CHECK (length(provider_event_id) > 0),
    conversation_id TEXT NOT NULL,
    thread_id TEXT,
    sender_id TEXT,
    envelope_json TEXT NOT NULL,
    disposition TEXT NOT NULL CHECK (disposition IN (
        'accepted','challenged','ignored','duplicate','failed'
    )),
    ignore_reason TEXT,
    job_id TEXT,
    received_at_ms INTEGER NOT NULL CHECK (received_at_ms > 0),
    UNIQUE(source, account_id, direction, provider_event_id)
) STRICT;

CREATE INDEX IF NOT EXISTS channel_events_recent_idx
    ON channel_events(account_id, received_at_ms DESC);

CREATE TABLE IF NOT EXISTS channel_outbox (
    outbox_id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES channel_accounts(account_id) ON DELETE CASCADE,
    conversation_id TEXT NOT NULL,
    thread_id TEXT,
    reply_to_provider_id TEXT,
    state TEXT NOT NULL CHECK (state IN (
        'queued','sending','sent','failed','needs_reconciliation','cancelled'
    )),
    payload_json TEXT NOT NULL,
    payload_digest TEXT NOT NULL CHECK (length(payload_digest) > 0),
    idempotency_key TEXT NOT NULL CHECK (length(idempotency_key) > 0),
    provider_message_id TEXT,
    attempt INTEGER NOT NULL DEFAULT 0 CHECK (attempt >= 0),
    max_attempts INTEGER NOT NULL CHECK (max_attempts BETWEEN 1 AND 100),
    next_attempt_at_ms INTEGER,
    last_error TEXT,
    job_id TEXT,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0),
    sent_at_ms INTEGER,
    UNIQUE(account_id, idempotency_key)
) STRICT;

CREATE INDEX IF NOT EXISTS channel_outbox_ready_idx
    ON channel_outbox(state, next_attempt_at_ms, created_at_ms);

CREATE TABLE IF NOT EXISTS channel_cursors (
    account_id TEXT NOT NULL REFERENCES channel_accounts(account_id) ON DELETE CASCADE,
    cursor_key TEXT NOT NULL CHECK (length(cursor_key) > 0),
    cursor_value TEXT NOT NULL CHECK (length(cursor_value) <= 4096),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0),
    PRIMARY KEY(account_id, cursor_key)
) STRICT;
"#;

const DAEMON_V6: i64 = 6;
const DAEMON_V6_CHECKSUM: &str = "daemon-jobs-v6-telephony";

/// Telephony state: the carrier accounts an operator owns and the calls that
/// went through them.
///
/// SMS deliberately has no tables here. An inbound text is a `ChannelEnvelope`
/// and lives in `channel_events` like every other message, which is the whole
/// point of routing SMS through the messaging subsystem rather than beside it.
/// What telephony adds that messaging has no concept of is a *call*.
///
/// # Two separate powers
///
/// `inbound_policy` and `outbound_approval` are separate columns because they
/// are separate decisions. An operator who lets Little Monkey answer the phone
/// has not agreed to let it dial out, and a schema that stored one flag would
/// make that distinction impossible to express.
///
/// # Money
///
/// Every row here can cost the operator money at their carrier, which is why
/// `telecom_calls` keeps `needs_reconciliation` as a state of its own: a call
/// that may already have been placed is never retried automatically, and the
/// row stays for a human to settle.
const DAEMON_V6_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS telecom_accounts (
    account_id TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('twilio','telnyx','plivo','mock')),
    label TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0,1)),
    carrier_account_id TEXT NOT NULL,
    from_number TEXT NOT NULL CHECK (length(from_number) > 0),
    credential_ref TEXT,
    public_base_url TEXT,
    non_secret_config_json TEXT NOT NULL,
    inbound_policy TEXT NOT NULL CHECK (inbound_policy IN ('reject','voicemail','answer')),
    outbound_approval TEXT NOT NULL CHECK (outbound_approval IN ('never','approval','allow')),
    health TEXT NOT NULL CHECK (health IN (
        'unconfigured','disconnected','connecting','connected','degraded','unsupported','error'
    )),
    health_detail TEXT,
    last_error TEXT,
    last_probe_at_ms INTEGER,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0)
) STRICT;

CREATE TABLE IF NOT EXISTS telecom_calls (
    call_id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES telecom_accounts(account_id) ON DELETE CASCADE,
    provider_call_id TEXT,
    direction TEXT NOT NULL CHECK (direction IN ('inbound','outbound')),
    peer_number TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN (
        'queued','ringing','in_progress','completed','failed','needs_reconciliation'
    )),
    session_key TEXT,
    job_id TEXT,
    idempotency_key TEXT NOT NULL,
    last_error TEXT,
    started_at_ms INTEGER,
    ended_at_ms INTEGER,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0),
    UNIQUE(account_id, idempotency_key)
) STRICT;

CREATE INDEX IF NOT EXISTS telecom_calls_live_idx
    ON telecom_calls(account_id, state, created_at_ms DESC);
CREATE INDEX IF NOT EXISTS telecom_calls_provider_idx
    ON telecom_calls(account_id, provider_call_id);

CREATE TABLE IF NOT EXISTS telecom_events (
    event_id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES telecom_accounts(account_id) ON DELETE CASCADE,
    provider_event_id TEXT NOT NULL CHECK (length(provider_event_id) > 0),
    kind TEXT NOT NULL,
    call_id TEXT,
    payload_digest TEXT NOT NULL,
    received_at_ms INTEGER NOT NULL CHECK (received_at_ms > 0),
    UNIQUE(account_id, provider_event_id)
) STRICT;
"#;

const DAEMON_V7: i64 = 7;
const DAEMON_V7_CHECKSUM: &str = "daemon-jobs-v7-call-limits";

/// Per-account call limits, and the deadlines a live call is held to.
///
/// The defaults are the cautious ones: one call at a time, a ring given up on
/// after a minute, a call cut at half an hour, and no recording. Every one of
/// them bounds something that costs the operator money or records a person who
/// did not ask to be recorded, so the safe value is the one an operator gets
/// without choosing anything.
///
/// `ALTER TABLE ... ADD COLUMN` rather than a rebuild: the defaults are
/// constants, so an existing row gets the safe limit without a data migration.
const DAEMON_V7_SQL: &str = r#"
ALTER TABLE telecom_accounts ADD COLUMN max_concurrent_calls INTEGER NOT NULL DEFAULT 1;
ALTER TABLE telecom_accounts ADD COLUMN ring_timeout_s INTEGER NOT NULL DEFAULT 60;
ALTER TABLE telecom_accounts ADD COLUMN max_duration_s INTEGER NOT NULL DEFAULT 1800;
ALTER TABLE telecom_accounts ADD COLUMN recording_enabled INTEGER NOT NULL DEFAULT 0;
"#;

const DAEMON_V8: i64 = 8;
const DAEMON_V8_CHECKSUM: &str = "daemon-jobs-v8-ingress-turns";

/// Accepted conversation turns, whatever origin they arrived on.
///
/// This is the one table that spans the messaging channels, the phone, a paired
/// device, a peer node and the voice stack, because "a turn was accepted and
/// must run exactly once" is the same fact for all of them. Each origin keeps
/// its own event log — `channel_events`, `telecom_events` — for what the
/// provider said; this records what Little Monkey decided to do about it.
///
/// # The window this closes
///
/// Recording an inbound event makes it deduplicated, not durable: a process
/// that dies between recording the event and enqueuing the run leaves a message
/// that the provider will never redeliver (it was acknowledged) and that the
/// event log will refuse as a duplicate if it does. A row here is written in
/// the same breath as the accept decision and cleared only once the queue has
/// the job, so recovery has both the fact and the payload it needs to finish.
///
/// # Exactly once
///
/// `dedupe_key` is UNIQUE and carries the origin identity
/// (`source:account:event_id`), so redelivery collapses. `job_id` is the
/// queue's deterministic id, so a recovery pass that races the original
/// submission produces one job rather than two.
const DAEMON_V8_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS ingress_turns (
    ingress_id TEXT PRIMARY KEY,
    dedupe_key TEXT NOT NULL UNIQUE,
    source TEXT NOT NULL CHECK (source IN (
        'desktop','mobile','messaging_channel','peer','voice','telephone'
    )),
    source_account_id TEXT NOT NULL,
    source_event_id TEXT NOT NULL,
    session_key TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('accepted','queued','failed')),
    ingress_json TEXT NOT NULL,
    params_json TEXT NOT NULL,
    job_id TEXT,
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    last_error TEXT,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0)
) STRICT;

CREATE INDEX IF NOT EXISTS ingress_turns_pending_idx
    ON ingress_turns(state, created_at_ms);
CREATE INDEX IF NOT EXISTS ingress_turns_recent_idx
    ON ingress_turns(created_at_ms DESC);
CREATE INDEX IF NOT EXISTS ingress_turns_job_idx ON ingress_turns(job_id);
"#;

const DAEMON_V9: i64 = 9;
const DAEMON_V9_CHECKSUM: &str = "daemon-jobs-v9-call-opening-line";

/// What is said when a call connects.
///
/// A call that opens with silence sounds broken to whoever picked up, so both
/// directions have an opening line: the number's greeting on an inbound call,
/// and on an outbound one the sentence the agent was approved to say. It lives
/// on the call rather than on the account because the outbound line is
/// per-call, and the approval the operator gave was for those words.
const DAEMON_V9_SQL: &str = r#"
ALTER TABLE telecom_calls ADD COLUMN opening_line TEXT;
"#;

const DAEMON_V10: i64 = 10;
const DAEMON_V10_CHECKSUM: &str = "daemon-jobs-v10-peer-threads";

/// What two paired installations have said to each other.
///
/// The pairing itself lives where every pairing lives — `remote-v1.sqlite3`,
/// with the device identity, the secret generation and the capability grant.
/// These tables hold only the traffic, because traffic is daemon state: it is
/// deduplicated by the daemon, it becomes daemon jobs, and it has to survive a
/// restart in step with the queue those jobs are in.
///
/// # Deduplication
///
/// `UNIQUE(sender_instance_id, message_id, direction)` is the durable half of
/// at-most-once. A peer that retries a delivery — the client retries three
/// times on a lost connection by design — lands on the row already here, and a
/// rejected message keeps its row too, so a redelivery cannot re-run a
/// decision that already went against it.
///
/// # Results
///
/// A result is a row like any other, with `direction='outbound'` and a message
/// id derived from the job it reports on, so materializing the same finished
/// run twice writes one row. Nothing is pushed to the peer: the sender polls
/// its own thread, which is what keeps this side free of an outbound
/// connection it would otherwise have to keep alive.
const DAEMON_V10_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS peer_threads (
    thread_id TEXT PRIMARY KEY,
    peer_device_id TEXT NOT NULL,
    peer_instance_id TEXT NOT NULL,
    session_key TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    last_activity_at_ms INTEGER NOT NULL CHECK (last_activity_at_ms > 0)
) STRICT;

CREATE INDEX IF NOT EXISTS peer_threads_recent_idx
    ON peer_threads(peer_device_id, last_activity_at_ms DESC);

CREATE TABLE IF NOT EXISTS peer_messages (
    row_id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES peer_threads(thread_id) ON DELETE CASCADE,
    peer_device_id TEXT NOT NULL,
    sender_instance_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    direction TEXT NOT NULL CHECK (direction IN ('inbound','outbound')),
    kind TEXT NOT NULL CHECK (kind IN ('message','task_request','artifact','result')),
    correlation_id TEXT,
    disposition TEXT NOT NULL CHECK (disposition IN ('accepted','rejected','delivered')),
    rejection TEXT,
    envelope_json TEXT NOT NULL,
    ingress_id TEXT,
    job_id TEXT,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    UNIQUE(sender_instance_id, message_id, direction)
) STRICT;

CREATE INDEX IF NOT EXISTS peer_messages_thread_idx
    ON peer_messages(thread_id, created_at_ms);
CREATE INDEX IF NOT EXISTS peer_messages_job_idx ON peer_messages(job_id);
"#;

const DAEMON_V11: i64 = 11;
const DAEMON_V11_CHECKSUM: &str = "daemon-jobs-v11-ingress-execution-snapshot";

/// The frozen execution context's identity, alongside the turn it belongs to.
///
/// The context itself lives inside `ingress_json`, where it is what the turn
/// executes. These two columns are the *observability* half: an operator asking
/// "which configuration was this accepted under?" gets an answer without the
/// listing having to parse a recipe out of a JSON blob, and two turns that
/// disagree about their digest are visible side by side.
///
/// Nullable because turns accepted by an earlier build have no frozen context.
/// Those rows keep working — they resolve their recipe at execution time, the
/// behavior they were accepted with — and nothing backfills a digest for a
/// configuration nobody recorded.
const DAEMON_V11_SQL: &str = r#"
ALTER TABLE ingress_turns ADD COLUMN execution_version INTEGER;
ALTER TABLE ingress_turns ADD COLUMN execution_digest TEXT;
"#;

const DAEMON_V12: i64 = 12;
const DAEMON_V12_CHECKSUM: &str = "daemon-jobs-v12-ingress-mutation-contract";

/// The workspace-mutation contract and the continuation lineage.
///
/// Both are already inside `ingress_json` — the contract is a field on the
/// accepted turn, and a continuation carries its parent's id — so these columns
/// exist because the *policy* has to find rows by them. "Which accepted turns
/// promised a file would change and have not been settled yet" is the query that
/// runs on every daemon tick, and it cannot be a scan of every stored turn's
/// JSON.
///
/// `mutation_state` is NULL until the run is terminal and its outcome has been
/// read; from then on it is one of `satisfied`, `corrected` (a continuation was
/// submitted), `unmet` (reported), or `interrupted` (the run stopped before it
/// could say, so nothing is replayed).
///
/// Every column is nullable or defaulted, so turns accepted by an earlier build
/// keep working: no contract, no lineage, nothing to settle.
const DAEMON_V12_SQL: &str = r#"
ALTER TABLE ingress_turns ADD COLUMN mutation_required INTEGER NOT NULL DEFAULT 0;
ALTER TABLE ingress_turns ADD COLUMN mutation_state TEXT;
ALTER TABLE ingress_turns ADD COLUMN mutation_detail TEXT;
ALTER TABLE ingress_turns ADD COLUMN parent_ingress_id TEXT;
ALTER TABLE ingress_turns ADD COLUMN continuation_kind TEXT;
ALTER TABLE ingress_turns ADD COLUMN continuation_attempt INTEGER NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS ingress_turns_contract_idx
    ON ingress_turns(mutation_required, mutation_state, created_at_ms);
CREATE INDEX IF NOT EXISTS ingress_turns_parent_idx
    ON ingress_turns(parent_ingress_id);
"#;

const DAEMON_V13: i64 = 13;
const DAEMON_V13_CHECKSUM: &str = "daemon-jobs-v13-outbox-invocation-identity";

/// One durable tool invocation, at most one outbound intent.
///
/// `channel_outbox` has always been unique on `(account_id, idempotency_key)`,
/// which asks "has this account already been told this?" — the right question
/// for a reply keyed to a provider event, and the wrong one for an agent's
/// `send_message`. There the identity is the tool invocation itself, so the
/// same job and tool-call id replayed against a *different* account cleared
/// the account-scoped constraint and queued a second message to a second
/// person. The account is part of what the invocation asked to send, not part
/// of which invocation asked.
///
/// `invocation_id` is that identity — job plus tool-call id — and the index
/// makes it unique across every account. Partial on NOT NULL because only
/// agent sends have an invocation behind them: an inbound auto-reply is keyed
/// to the event it answers and keeps the account-scoped constraint it has
/// always had, so no historical row is reinterpreted and no existing key can
/// collide into the new index.
const DAEMON_V13_SQL: &str = r#"
ALTER TABLE channel_outbox ADD COLUMN invocation_id TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS channel_outbox_invocation_idx
    ON channel_outbox(invocation_id)
    WHERE invocation_id IS NOT NULL;
"#;

const DAEMON_V14: i64 = 14;
const DAEMON_V14_CHECKSUM: &str = "daemon-jobs-v14-channel-event-ingress-link";

/// The durable relation between a provider event and the turn it became.
///
/// Before this column the two facts were only *correlated* — an event and a
/// turn that happened to share an account and an event id — and they were
/// committed by two separate transactions, so a crash between them left an
/// event row that permanently suppressed the provider's redelivery with no
/// accepted turn behind it. The column is what lets the acceptance be one
/// transaction and what lets an operator ask, from SQLite alone, which turn
/// owns an event and whether one was ever created.
///
/// # Existing rows
///
/// The backfill links every inbound accepted event that already has a turn, by
/// the dedupe key that turn was stored under — `source:account:event_id`, the
/// same three columns the event carries. What it deliberately cannot do is
/// invent a turn for an event that never got one: those rows stay NULL, which
/// is the same shape a message a webhook provider has been acknowledged for
/// rests in, and the channel worker decides them again from the envelope they
/// still carry rather than pretending they completed.
const DAEMON_V14_SQL: &str = r#"
ALTER TABLE channel_events ADD COLUMN ingress_id TEXT;

UPDATE channel_events
   SET ingress_id = (
       SELECT ingress_turns.ingress_id FROM ingress_turns
        WHERE ingress_turns.dedupe_key =
              channel_events.source || ':' || channel_events.account_id || ':' ||
              channel_events.provider_event_id
   )
 WHERE direction = 'inbound' AND disposition = 'accepted' AND ingress_id IS NULL;

CREATE INDEX IF NOT EXISTS channel_events_ingress_idx
    ON channel_events(ingress_id);
CREATE INDEX IF NOT EXISTS channel_events_orphan_idx
    ON channel_events(direction, disposition, ingress_id, received_at_ms);
"#;

const DAEMON_V15: i64 = 15;
const DAEMON_V15_CHECKSUM: &str = "daemon-jobs-v15-channel-conversation-refs";

/// Where a provider wants a reply sent, kept durably per conversation.
///
/// Some providers do not accept a conversation id alone. The Bot Framework
/// addresses a Teams reply by the `serviceUrl` its inbound activity carried,
/// and that value is per conversation and per region — without it there is no
/// endpoint to POST to at all. Holding it in memory made a reply survive only
/// as long as the process that received the activity, so a durable turn that
/// outlived a restart had a queued answer and nowhere to send it.
///
/// # What may live here
///
/// Addressing, never authorization. `reference_json` is written only from an
/// input the provider's own adapter has already authenticated and validated —
/// for Teams that means an activity whose Bot Framework JWT verified and a
/// `serviceUrl` on a Microsoft-owned host — so an unauthenticated request can
/// never plant an outbound destination.
///
/// A **credential** may never live here, and nothing that expires does either.
/// The bot access tokens both Teams and Google Chat acquire stay in memory with
/// their expiry, and the operator's own long-lived tokens stay in the keychain.
/// LINE's `replyToken` is deliberately not here: it authorizes answering as the
/// bot, is valid for seconds, and belongs to one event rather than to the
/// conversation this table is keyed by — so that adapter pushes instead, and
/// stores nothing. Teams is currently the only writer. Nothing reads this table
/// but the adapters; no status API projects it.
///
/// The size check keeps a provider from turning a reply address into unbounded
/// storage.
///
/// A row is per `(account_id, conversation_id)` and is overwritten as newer
/// activities arrive, because the newest authenticated address is the one a
/// provider wants used.
const DAEMON_V15_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS channel_conversation_refs (
    account_id TEXT NOT NULL REFERENCES channel_accounts(account_id) ON DELETE CASCADE,
    conversation_id TEXT NOT NULL CHECK (length(conversation_id) > 0),
    reference_json TEXT NOT NULL CHECK (length(reference_json) <= 8192),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0),
    PRIMARY KEY(account_id, conversation_id)
) STRICT;
"#;

const DAEMON_V16: i64 = 16;
const DAEMON_V16_CHECKSUM: &str = "daemon-jobs-v16-authenticated-peer-identity";

/// Scope peer conversation identity to the authenticated pairing.
///
/// The v10 tables trusted two envelope fields too much: `thread_id` was global
/// across every peer, and `sender_instance_id` participated in dedupe even
/// though the signed device credential — not the envelope — is identity. The
/// replacement keys make both guarantees depend on `peer_device_id`. Rows
/// whose historical message owner disagrees with their thread owner are
/// intentionally not copied; they were produced only by the old cross-peer
/// collision and cannot be attributed safely.
///
/// Shape/expiry/loop refusals happen before a thread exists. Their separate,
/// bounded table gives Security Doctor evidence without retaining peer text or
/// an invalid unbounded identifier.
const DAEMON_V16_SQL: &str = r#"
DROP INDEX IF EXISTS peer_messages_job_idx;
DROP INDEX IF EXISTS peer_messages_thread_idx;
DROP INDEX IF EXISTS peer_threads_recent_idx;

ALTER TABLE peer_messages RENAME TO peer_messages_v10;
ALTER TABLE peer_threads RENAME TO peer_threads_v10;

CREATE TABLE peer_threads (
    peer_device_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    peer_instance_id TEXT NOT NULL,
    session_key TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    last_activity_at_ms INTEGER NOT NULL CHECK (last_activity_at_ms > 0),
    PRIMARY KEY(peer_device_id, thread_id)
) STRICT;

INSERT INTO peer_threads (
    peer_device_id, thread_id, peer_instance_id, session_key,
    created_at_ms, last_activity_at_ms
)
SELECT peer_device_id, thread_id, peer_instance_id, session_key,
       created_at_ms, last_activity_at_ms
  FROM peer_threads_v10;

CREATE TABLE peer_messages (
    row_id TEXT PRIMARY KEY,
    peer_device_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    sender_instance_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    direction TEXT NOT NULL CHECK (direction IN ('inbound','outbound')),
    kind TEXT NOT NULL CHECK (kind IN ('message','task_request','artifact','result')),
    correlation_id TEXT,
    disposition TEXT NOT NULL CHECK (disposition IN ('accepted','rejected','delivered')),
    rejection TEXT,
    envelope_json TEXT NOT NULL,
    ingress_id TEXT,
    job_id TEXT,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    FOREIGN KEY(peer_device_id, thread_id)
        REFERENCES peer_threads(peer_device_id, thread_id) ON DELETE CASCADE,
    UNIQUE(peer_device_id, message_id, direction)
) STRICT;

INSERT INTO peer_messages (
    row_id, peer_device_id, thread_id, sender_instance_id, message_id,
    direction, kind, correlation_id, disposition, rejection, envelope_json,
    ingress_id, job_id, created_at_ms
)
SELECT message.row_id, message.peer_device_id, message.thread_id,
       message.sender_instance_id, message.message_id, message.direction,
       message.kind, message.correlation_id, message.disposition,
       message.rejection, message.envelope_json, message.ingress_id,
       message.job_id, message.created_at_ms
  FROM peer_messages_v10 AS message
  JOIN peer_threads_v10 AS thread
    ON thread.thread_id = message.thread_id
   AND thread.peer_device_id = message.peer_device_id;

DROP TABLE peer_messages_v10;
DROP TABLE peer_threads_v10;

CREATE INDEX peer_threads_recent_idx
    ON peer_threads(peer_device_id, last_activity_at_ms DESC);
CREATE INDEX peer_messages_thread_idx
    ON peer_messages(peer_device_id, thread_id, created_at_ms);
CREATE INDEX peer_messages_job_idx ON peer_messages(job_id);

CREATE TABLE peer_rejection_events (
    event_id TEXT PRIMARY KEY CHECK (length(event_id) BETWEEN 1 AND 64),
    peer_device_id TEXT NOT NULL CHECK (length(peer_device_id) BETWEEN 1 AND 128),
    message_id TEXT CHECK (message_id IS NULL OR length(message_id) BETWEEN 1 AND 128),
    thread_id TEXT CHECK (thread_id IS NULL OR length(thread_id) BETWEEN 1 AND 128),
    reason TEXT NOT NULL CHECK (length(reason) BETWEEN 1 AND 64),
    occurred_at_ms INTEGER NOT NULL CHECK (occurred_at_ms > 0)
) STRICT;

CREATE INDEX peer_rejection_events_recent_idx
    ON peer_rejection_events(occurred_at_ms DESC, event_id DESC);
"#;

const DAEMON_V17: i64 = 17;
const DAEMON_V17_CHECKSUM: &str = "daemon-jobs-v17-sms-delivery-and-callback-rejections";

/// What became of a message after the carrier accepted it, and whether a
/// carrier's callbacks are being refused at the door.
///
/// # Delivery is not sending
///
/// `channel_outbox.state` answers "did the provider take it?" — `sent` is the
/// end of that story. A carrier answers a second question minutes later: did it
/// reach the handset? On SMS that answer is routine and routinely negative
/// (a wrong number, a landline, a carrier block), and until now it arrived,
/// verified, and was dropped. These columns are where it lands, so an operator
/// can see that the text they were told was sent was never delivered.
///
/// `delivery_state` is deliberately separate from `state` rather than a new
/// value in it: a delivery receipt must never move a row back into the retry
/// machinery, and a schema where "delivered" and "sent" are the same column
/// invites exactly that.
///
/// # Rejected callbacks
///
/// A signature that does not verify earns no durable row — the body is
/// attacker-supplied and recording it would be storage anyone can write. But
/// the *fact* that this account's callbacks are being rejected is the single
/// most useful thing Security Doctor can tell an operator whose public URL no
/// longer matches what they configured, so a bounded counter and the reason
/// code (never the body, never a header) live on the account itself.
const DAEMON_V17_SQL: &str = r#"
ALTER TABLE channel_outbox ADD COLUMN delivery_state TEXT;
ALTER TABLE channel_outbox ADD COLUMN delivery_error TEXT;
ALTER TABLE channel_outbox ADD COLUMN delivered_at_ms INTEGER;

CREATE INDEX IF NOT EXISTS channel_outbox_provider_message_idx
    ON channel_outbox(account_id, provider_message_id);

ALTER TABLE telecom_accounts ADD COLUMN rejected_callbacks INTEGER NOT NULL DEFAULT 0;
ALTER TABLE telecom_accounts ADD COLUMN last_rejection TEXT;
ALTER TABLE telecom_accounts ADD COLUMN last_rejection_at_ms INTEGER;
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
    (DAEMON_V4, DAEMON_V4_CHECKSUM, DAEMON_V4_SQL),
    (DAEMON_V5, DAEMON_V5_CHECKSUM, DAEMON_V5_SQL),
    (DAEMON_V6, DAEMON_V6_CHECKSUM, DAEMON_V6_SQL),
    (DAEMON_V7, DAEMON_V7_CHECKSUM, DAEMON_V7_SQL),
    (DAEMON_V8, DAEMON_V8_CHECKSUM, DAEMON_V8_SQL),
    (DAEMON_V9, DAEMON_V9_CHECKSUM, DAEMON_V9_SQL),
    (DAEMON_V10, DAEMON_V10_CHECKSUM, DAEMON_V10_SQL),
    (DAEMON_V11, DAEMON_V11_CHECKSUM, DAEMON_V11_SQL),
    (DAEMON_V12, DAEMON_V12_CHECKSUM, DAEMON_V12_SQL),
    (DAEMON_V13, DAEMON_V13_CHECKSUM, DAEMON_V13_SQL),
    (DAEMON_V14, DAEMON_V14_CHECKSUM, DAEMON_V14_SQL),
    (DAEMON_V15, DAEMON_V15_CHECKSUM, DAEMON_V15_SQL),
    (DAEMON_V16, DAEMON_V16_CHECKSUM, DAEMON_V16_SQL),
    (DAEMON_V17, DAEMON_V17_CHECKSUM, DAEMON_V17_SQL),
];

/// Latest version this build understands. The forward-only guard compares
/// against this rather than a specific version, so adding V4 needs no edit
/// there.
const DAEMON_LATEST: i64 = DAEMON_V17;

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

    /// The invocation index arrives on a database that already has outbox
    /// rows, and those rows were keyed per account.
    ///
    /// Two accounts holding the same idempotency key was legal before V13 and
    /// stays legal: a unique index over the key itself would have refused to
    /// build here and left the installation unable to open its own state. The
    /// index is over `invocation_id`, which no historical row has, so there is
    /// nothing for it to collide with and nothing to backfill — an old row
    /// keeps the account-scoped identity it was written with.
    #[test]
    fn outbox_rows_written_before_the_invocation_index_upgrade_in_place() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE daemon_migrations (
                    version INTEGER PRIMARY KEY,
                    checksum TEXT NOT NULL,
                    applied_at_ms INTEGER NOT NULL
                 ) STRICT;",
            )
            .unwrap();
        for &(version, checksum, sql) in DAEMON_MIGRATIONS {
            if version > DAEMON_V12 {
                break;
            }
            connection.execute_batch(sql).unwrap();
            connection
                .execute(
                    "INSERT INTO daemon_migrations(version, checksum, applied_at_ms)
                     VALUES (?1, ?2, 1)",
                    rusqlite::params![version, checksum],
                )
                .unwrap();
        }
        connection
            .execute_batch(
                "INSERT INTO channel_accounts (
                    account_id, kind, label, enabled, non_secret_config_json, credential_ref,
                    access_policy_json, health, created_at_ms, updated_at_ms
                 ) VALUES
                    ('acct-1','telegram','One',1,'{}',NULL,'{}','connected',1,1),
                    ('acct-2','telegram','Two',1,'{}',NULL,'{}','connected',1,1);
                 INSERT INTO channel_outbox (
                    outbox_id, account_id, conversation_id, state, payload_json,
                    payload_digest, idempotency_key, attempt, max_attempts, created_at_ms,
                    updated_at_ms
                 ) VALUES
                    ('out-1','acct-1','c1','queued','{}','d1','reply-job-1-1',0,3,1,1),
                    ('out-2','acct-2','c1','queued','{}','d2','reply-job-1-1',0,3,1,1);",
            )
            .unwrap();

        apply_daemon_migrations(&connection).unwrap();

        let rows: i64 = connection
            .query_row("SELECT COUNT(*) FROM channel_outbox", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 2, "both historical rows survive");
        let unset: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM channel_outbox WHERE invocation_id IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unset, 2, "nothing is backfilled into the new identity");
    }

    /// A turn accepted before execution contexts were frozen has to survive the
    /// upgrade, and read back as a turn with no frozen context — not as one
    /// with today's configuration stamped onto it.
    #[test]
    fn an_ingress_row_written_before_the_execution_columns_upgrades_in_place() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE daemon_migrations (
                    version INTEGER PRIMARY KEY,
                    checksum TEXT NOT NULL,
                    applied_at_ms INTEGER NOT NULL
                 ) STRICT;",
            )
            .unwrap();
        for &(version, checksum, sql) in DAEMON_MIGRATIONS {
            if version > DAEMON_V10 {
                break;
            }
            connection.execute_batch(sql).unwrap();
            connection
                .execute(
                    "INSERT INTO daemon_migrations(version, checksum, applied_at_ms)
                     VALUES (?1, ?2, 1)",
                    rusqlite::params![version, checksum],
                )
                .unwrap();
        }
        connection
            .execute_batch(
                "INSERT INTO ingress_turns (
                    ingress_id, dedupe_key, source, source_account_id, source_event_id,
                    session_key, state, ingress_json, params_json, attempts,
                    created_at_ms, updated_at_ms
                 ) VALUES ('ingr-old', 'peer:node-1:handover-1', 'peer', 'node-1',
                           'handover-1', 'peer:node-1', 'accepted', '{}', '[]', 0, 1, 1);",
            )
            .unwrap();

        apply_daemon_migrations(&connection).unwrap();

        let store = DaemonStore { connection };
        let turn = &store.recent_ingress_turns(10).unwrap()[0];
        assert_eq!(turn.ingress_id, "ingr-old");
        assert_eq!(
            turn.state,
            crate::daemon::ingress_store::IngressState::Accepted
        );
        assert_eq!(turn.execution_version, None);
        assert_eq!(turn.execution_digest, None);
        // V12's columns default rather than backfill: a turn accepted before the
        // workspace-mutation contract existed promised nothing and continues
        // nothing, which is exactly what it meant when it was written.
        assert!(!turn.mutation_required);
        assert!(turn.mutation_state.is_none());
        assert!(turn.mutation_detail.is_none());
        assert!(turn.parent_ingress_id.is_none());
        assert!(turn.continuation_kind.is_none());
        assert_eq!(turn.continuation_attempt, 0);
        // And it is not work the contract policy will pick up, because there is
        // no contract on it to settle.
        assert!(store.unsettled_mutation_contracts(10).unwrap().is_empty());
    }

    /// The V11 file is the one most installations are upgrading from, so the
    /// contract columns have to land on a database that already has the
    /// execution snapshot without disturbing it.
    #[test]
    fn an_ingress_row_written_before_the_contract_columns_keeps_its_frozen_snapshot() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE daemon_migrations (
                    version INTEGER PRIMARY KEY,
                    checksum TEXT NOT NULL,
                    applied_at_ms INTEGER NOT NULL
                 ) STRICT;",
            )
            .unwrap();
        for &(version, checksum, sql) in DAEMON_MIGRATIONS {
            if version > DAEMON_V11 {
                break;
            }
            connection.execute_batch(sql).unwrap();
            connection
                .execute(
                    "INSERT INTO daemon_migrations(version, checksum, applied_at_ms)
                     VALUES (?1, ?2, 1)",
                    rusqlite::params![version, checksum],
                )
                .unwrap();
        }
        connection
            .execute_batch(
                "INSERT INTO ingress_turns (
                    ingress_id, dedupe_key, source, source_account_id, source_event_id,
                    session_key, state, ingress_json, params_json, job_id, attempts,
                    execution_version, execution_digest, created_at_ms, updated_at_ms
                 ) VALUES ('ingr-v11', 'desktop:session-1:turn-1', 'desktop', 'session-1',
                           'turn-1', 'desktop:session-1', 'queued', '{}', '[]',
                           'ingress-abc', 1, 1, 'deadbeef', 1, 1);",
            )
            .unwrap();

        apply_daemon_migrations(&connection).unwrap();

        let store = DaemonStore { connection };
        let turn = &store.recent_ingress_turns(10).unwrap()[0];
        assert_eq!(turn.execution_version, Some(1));
        assert_eq!(turn.execution_digest.as_deref(), Some("deadbeef"));
        assert!(!turn.mutation_required);
        assert!(turn.mutation_state.is_none());
    }

    /// V13 links a provider event to the turn it became. An installation
    /// upgrading into it carries two kinds of accepted event: ones whose turn
    /// was created (the pair is recoverable, and the link is derivable from the
    /// dedupe key they already share) and ones whose turn never was — the crash
    /// window this whole change exists to close. The first must be linked; the
    /// second must stay visible as unfinished rather than be linked to
    /// something, or quietly counted as complete.
    #[test]
    fn events_written_before_the_ingress_link_are_paired_or_left_visible() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE daemon_migrations (
                    version INTEGER PRIMARY KEY,
                    checksum TEXT NOT NULL,
                    applied_at_ms INTEGER NOT NULL
                 ) STRICT;",
            )
            .unwrap();
        for &(version, checksum, sql) in DAEMON_MIGRATIONS {
            if version > DAEMON_V12 {
                break;
            }
            connection.execute_batch(sql).unwrap();
            connection
                .execute(
                    "INSERT INTO daemon_migrations(version, checksum, applied_at_ms)
                     VALUES (?1, ?2, 1)",
                    rusqlite::params![version, checksum],
                )
                .unwrap();
        }
        connection
            .execute_batch(
                "INSERT INTO channel_accounts (
                    account_id, kind, label, enabled, non_secret_config_json,
                    credential_ref, access_policy_json, health, created_at_ms, updated_at_ms
                 ) VALUES ('acct-1', 'telegram', 'Ops bot', 1, '{}', NULL, '{}',
                           'connected', 1, 1);

                 INSERT INTO ingress_turns (
                    ingress_id, dedupe_key, source, source_account_id, source_event_id,
                    session_key, state, ingress_json, params_json, attempts,
                    created_at_ms, updated_at_ms
                 ) VALUES ('ingr-paired', 'messaging_channel:acct-1:evt-1',
                           'messaging_channel', 'acct-1', 'evt-1', 'tg:chat-7',
                           'queued', '{}', '[]', 0, 1, 1);

                 INSERT INTO channel_events (
                    event_id, account_id, source, direction, provider_event_id,
                    conversation_id, thread_id, sender_id, envelope_json, disposition,
                    ignore_reason, job_id, received_at_ms
                 ) VALUES
                    ('evt-paired', 'acct-1', 'messaging_channel', 'inbound', 'evt-1',
                     'chat-7', NULL, 'user-3', '{}', 'accepted', NULL, NULL, 1),
                    ('evt-orphan', 'acct-1', 'messaging_channel', 'inbound', 'evt-2',
                     'chat-7', NULL, 'user-3', '{}', 'accepted', NULL, NULL, 2);",
            )
            .unwrap();

        apply_daemon_migrations(&connection).unwrap();

        let store = DaemonStore { connection };
        let events = store.recent_channel_events("acct-1", 10).unwrap();
        let paired = events
            .iter()
            .find(|event| event.event_id == "evt-paired")
            .expect("the paired event");
        assert_eq!(paired.ingress_id.as_deref(), Some("ingr-paired"));
        let orphan = events
            .iter()
            .find(|event| event.event_id == "evt-orphan")
            .expect("the orphaned event");
        assert_eq!(orphan.ingress_id, None);
        let unfinished = store.accepted_events_awaiting_processing(10).unwrap();
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].event_id, "evt-orphan");
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
                .record_reservation(id, "shared-model", 1_024, 512, &[])
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

    /// Per-device reservations, and the two properties that make them safe: they
    /// are grouped by *resident model* like the pooled figures beside them, and
    /// every release path takes them with it.
    #[test]
    fn device_reservations_group_by_model_and_come_back_on_every_release_path() {
        use super::super::admission::{DeviceClaim, DeviceId};

        let mut store = DaemonStore::open_in_memory().unwrap();
        let card0 = DeviceId::device(AcceleratorKind::Cuda, 0);
        let card1 = DeviceId::device(AcceleratorKind::Cuda, 1);

        for id in ["job-a", "job-b", "job-c"] {
            store.insert_preparing(&new_job(id, 1), 8).unwrap();
            store.mark_queued(id, &format!("run-{id}"), 1).unwrap();
            store
                .transition(id, JobState::Running, 2, Some(7), None)
                .unwrap();
        }

        // Two jobs against ONE resident model, and a third against another. The
        // model is loaded once, so the card must be charged once for it.
        for id in ["job-a", "job-b"] {
            store
                .record_reservation(
                    id,
                    "model-shared",
                    1_024,
                    4_096,
                    &[DeviceClaim {
                        device: card0.clone(),
                        bytes: 4_096,
                    }],
                )
                .unwrap();
        }
        store
            .record_reservation(
                "job-c",
                "model-other",
                1_024,
                3_000,
                &[
                    DeviceClaim {
                        device: card0.clone(),
                        bytes: 1_000,
                    },
                    DeviceClaim {
                        device: card1.clone(),
                        bytes: 2_000,
                    },
                ],
            )
            .unwrap();

        let committed = |store: &DaemonStore| {
            let mut rows = store.committed_device_reservations().unwrap();
            rows.sort();
            rows
        };
        assert_eq!(
            committed(&store),
            vec![
                (AcceleratorKind::Cuda, Some(0), 5_096),
                (AcceleratorKind::Cuda, Some(1), 2_000),
            ],
            "card 0 holds one copy of the shared model plus the other model, not two copies"
        );

        // Per job, for preemption: this is who is holding what.
        let per_job = store.job_device_reservations().unwrap();
        assert_eq!(per_job["job-a"].len(), 1);
        assert_eq!(per_job["job-c"].len(), 2);

        // The clean exit path. One of two holders leaving frees nothing: the
        // model is still resident for the other.
        store.release_reservation("job-a").unwrap();
        store
            .transition("job-a", JobState::Succeeded, 3, None, None)
            .unwrap();
        assert_eq!(
            committed(&store),
            vec![
                (AcceleratorKind::Cuda, Some(0), 5_096),
                (AcceleratorKind::Cuda, Some(1), 2_000),
            ],
            "the bytes come back when the LAST holder exits, not the first"
        );

        // The last holder of the shared model leaves, and its card 0 bytes go.
        store.release_reservation("job-b").unwrap();
        store
            .transition("job-b", JobState::Succeeded, 4, None, None)
            .unwrap();
        assert_eq!(
            committed(&store),
            vec![
                (AcceleratorKind::Cuda, Some(0), 1_000),
                (AcceleratorKind::Cuda, Some(1), 2_000),
            ],
        );

        // The crash funnel: a job that reached a terminal state without ever
        // releasing. The sweep has to take its device rows too, or a card stays
        // booked by a job that is gone.
        store
            .transition("job-c", JobState::Failed, 5, None, Some("crashed"))
            .unwrap();
        assert!(store.sweep_stale_reservations().unwrap() >= 1);
        assert!(
            committed(&store).is_empty(),
            "a swept reservation must leave no device rows behind"
        );
        assert!(store.job_device_reservations().unwrap().is_empty());
    }

    /// A re-record restates what a job holds rather than adding to it — a resume
    /// re-books the same claim, and merging would double it every time.
    #[test]
    fn re_recording_a_reservation_replaces_its_device_rows() {
        use super::super::admission::{DeviceClaim, DeviceId};

        let mut store = DaemonStore::open_in_memory().unwrap();
        store
            .insert_preparing(&new_job("job-resume", 1), 8)
            .unwrap();
        store.mark_queued("job-resume", "run-resume", 1).unwrap();
        store
            .transition("job-resume", JobState::Running, 2, Some(7), None)
            .unwrap();

        let claim = [DeviceClaim {
            device: DeviceId::device(AcceleratorKind::Cuda, 0),
            bytes: 2_048,
        }];
        for _ in 0..3 {
            store
                .record_reservation("job-resume", "model", 1_024, 2_048, &claim)
                .unwrap();
        }
        assert_eq!(
            store.committed_device_reservations().unwrap(),
            vec![(AcceleratorKind::Cuda, Some(0), 2_048)]
        );
    }

    /// The stored accelerator tokens are database values and must round-trip
    /// exactly. A reworded serde attribute must not silently re-key every row.
    #[test]
    fn accelerator_tokens_round_trip_and_are_stable() {
        for kind in [
            AcceleratorKind::Cpu,
            AcceleratorKind::Metal,
            AcceleratorKind::Cuda,
            AcceleratorKind::Rocm,
            AcceleratorKind::Vulkan,
            AcceleratorKind::DirectMl,
            AcceleratorKind::AppleNeuralEngine,
        ] {
            assert_eq!(accelerator_from_token(accelerator_token(kind)), Some(kind));
        }
        assert_eq!(accelerator_token(AcceleratorKind::Cuda), "cuda");
        // A token from a newer daemon is skipped, never guessed at.
        assert_eq!(accelerator_from_token("tensor-thing"), None);
    }
}
