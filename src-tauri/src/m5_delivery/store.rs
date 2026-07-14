use std::path::{Path, PathBuf};
use std::{fs, io::Write};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use super::{AuditEntry, MutationExecutionRecord, OwnedWorktreeRecord, ReviewReport};

const DATABASE_FILE: &str = "delivery-v1.sqlite3";
const RECONCILIATION_FALLBACK_FILE: &str = "reconciliation-fallback.jsonl";
const MAX_RECONCILIATION_FALLBACK_BYTES: u64 = 4 * 1024 * 1024;
const MIGRATION: &str = r#"
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS delivery_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;
INSERT OR IGNORE INTO delivery_meta(key,value) VALUES('schema_version','1');

CREATE TABLE IF NOT EXISTS owned_worktrees (
    worktree_id TEXT PRIMARY KEY,
    marker_json BLOB NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('active','recovered','archived','cleaned')),
    locked INTEGER NOT NULL DEFAULT 0 CHECK(locked IN (0,1)),
    lock_reason TEXT,
    archive_path TEXT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms > 0)
) STRICT;

CREATE TABLE IF NOT EXISTS mutation_previews (
    digest TEXT PRIMARY KEY,
    action TEXT NOT NULL,
    request_json BLOB NOT NULL,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms > 0),
    expires_at_ms INTEGER NOT NULL CHECK(expires_at_ms > created_at_ms),
    consumed_at_ms INTEGER
) STRICT;

CREATE TABLE IF NOT EXISTS delivery_audit (
    audit_id INTEGER PRIMARY KEY AUTOINCREMENT,
    occurred_at_ms INTEGER NOT NULL CHECK(occurred_at_ms > 0),
    action TEXT NOT NULL,
    target TEXT,
    request_digest TEXT NOT NULL,
    outcome TEXT NOT NULL,
    detail TEXT
) STRICT;

CREATE TABLE IF NOT EXISTS mutation_executions (
    request_digest TEXT PRIMARY KEY,
    action TEXT NOT NULL,
    target TEXT NOT NULL,
    request_json BLOB NOT NULL,
    external INTEGER NOT NULL CHECK(external IN (0,1)),
    state TEXT NOT NULL CHECK(state IN (
      'executing','completed','failed','needs_reconciliation',
      'reconciled_completed','reconciled_not_applied'
    )),
    executor_instance TEXT NOT NULL,
    confirmed_at_ms INTEGER NOT NULL CHECK(confirmed_at_ms > 0),
    started_at_ms INTEGER NOT NULL CHECK(started_at_ms > 0),
    finished_at_ms INTEGER,
    result_json BLOB,
    error TEXT,
    resolution TEXT,
    resolution_note TEXT,
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms > 0)
) STRICT;

CREATE TABLE IF NOT EXISTS review_reports (
    report_id TEXT PRIMARY KEY,
    repository_slug TEXT NOT NULL,
    pr_number INTEGER NOT NULL CHECK(pr_number > 0),
    head_oid TEXT NOT NULL,
    model TEXT NOT NULL,
    report_json BLOB NOT NULL,
    report_digest TEXT NOT NULL,
    published_comment_id INTEGER,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms > 0)
) STRICT;

CREATE TABLE IF NOT EXISTS patch_tasks (
    task_id TEXT PRIMARY KEY,
    repository_slug TEXT NOT NULL,
    pr_number INTEGER NOT NULL CHECK(pr_number > 0),
    comment_id INTEGER NOT NULL CHECK(comment_id > 0),
    recipe_path TEXT NOT NULL,
    run_id TEXT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms > 0)
) STRICT;

CREATE INDEX IF NOT EXISTS delivery_audit_time_idx
    ON delivery_audit(occurred_at_ms DESC,audit_id DESC);
CREATE INDEX IF NOT EXISTS mutation_execution_state_idx
    ON mutation_executions(state,updated_at_ms,request_digest);
CREATE INDEX IF NOT EXISTS review_reports_pr_idx
    ON review_reports(repository_slug,pr_number,created_at_ms DESC);

CREATE TRIGGER IF NOT EXISTS delivery_audit_no_update
BEFORE UPDATE ON delivery_audit BEGIN
  SELECT RAISE(ABORT,'delivery audit is append-only');
END;
CREATE TRIGGER IF NOT EXISTS delivery_audit_no_delete
BEFORE DELETE ON delivery_audit BEGIN
  SELECT RAISE(ABORT,'delivery audit is append-only');
END;
"#;

pub struct DeliveryStore {
    connection: Connection,
    pub root: PathBuf,
}

impl DeliveryStore {
    pub fn open(app_data: &Path) -> Result<Self, String> {
        let root = app_data.join("m5-delivery-v1");
        ensure_private_directory(&root)?;
        let path = root.join(DATABASE_FILE);
        let mut connection = Connection::open(&path).map_err(|error| error.to_string())?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| error.to_string())?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(|error| error.to_string())?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|error| error.to_string())?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(|error| error.to_string())?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        transaction
            .execute_batch(MIGRATION)
            .map_err(|error| error.to_string())?;
        let version: String = transaction
            .query_row(
                "SELECT value FROM delivery_meta WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if version != "1" {
            return Err("Unsupported M5 delivery database schema".to_string());
        }
        transaction.commit().map_err(|error| error.to_string())?;
        restrict_file(&path)?;
        Ok(Self { connection, root })
    }

    #[cfg(test)]
    pub fn open_in_memory(root: PathBuf) -> Result<Self, String> {
        ensure_private_directory(&root)?;
        let connection = Connection::open_in_memory().map_err(|error| error.to_string())?;
        connection
            .execute_batch(MIGRATION)
            .map_err(|error| error.to_string())?;
        Ok(Self { connection, root })
    }

    pub fn save_preview(
        &mut self,
        digest: &str,
        action: &str,
        request: &[u8],
        now_ms: u64,
        expires_at_ms: u64,
    ) -> Result<(), String> {
        self.connection
            .execute(
                "INSERT INTO mutation_previews
                 (digest,action,request_json,created_at_ms,expires_at_ms,consumed_at_ms)
                 VALUES(?1,?2,?3,?4,?5,NULL)
                 ON CONFLICT(digest) DO UPDATE SET
                   action=excluded.action,
                   request_json=excluded.request_json,
                   created_at_ms=excluded.created_at_ms,
                   expires_at_ms=excluded.expires_at_ms,
                   consumed_at_ms=NULL",
                params![
                    digest,
                    action,
                    request,
                    to_i64(now_ms)?,
                    to_i64(expires_at_ms)?
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    #[cfg(test)]
    pub fn consume_preview(
        &mut self,
        digest: &str,
        expected_request: &[u8],
        now_ms: u64,
    ) -> Result<String, String> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let stored = transaction
            .query_row(
                "SELECT action,request_json,expires_at_ms,consumed_at_ms
                 FROM mutation_previews WHERE digest=?1",
                [digest],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "Confirmation preview is missing or expired".to_string())?;
        if stored.1 != expected_request {
            return Err(
                "Confirmation digest does not match the exact mutation request".to_string(),
            );
        }
        if stored.3.is_some() {
            return Err("Confirmation preview was already consumed".to_string());
        }
        if stored.2 < to_i64(now_ms)? {
            return Err("Confirmation preview expired".to_string());
        }
        let changed = transaction
            .execute(
                "UPDATE mutation_previews SET consumed_at_ms=?2
                 WHERE digest=?1 AND consumed_at_ms IS NULL",
                params![digest, to_i64(now_ms)?],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err("Confirmation preview was consumed concurrently".to_string());
        }
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(stored.0)
    }

    /// Atomically consumes the exact confirmation, records the immutable
    /// execution intent, and appends a `pending` audit row. Callers MUST NOT
    /// perform the mutation unless this transaction commits successfully.
    pub fn confirm_and_begin_execution(
        &mut self,
        digest: &str,
        expected_request: &[u8],
        expected_action: &str,
        target: &str,
        external: bool,
        executor_instance: &str,
        now_ms: u64,
    ) -> Result<String, String> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let stored = transaction
            .query_row(
                "SELECT action,request_json,expires_at_ms,consumed_at_ms
                 FROM mutation_previews WHERE digest=?1",
                [digest],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "Confirmation preview is missing or expired".to_string())?;
        if stored.1 != expected_request {
            return Err(
                "Confirmation digest does not match the exact mutation request".to_string(),
            );
        }
        if stored.0 != expected_action {
            return Err("Confirmation action does not match the exact mutation".to_string());
        }
        if stored.3.is_some() {
            return Err("Confirmation preview was already consumed".to_string());
        }
        if stored.2 < to_i64(now_ms)? {
            return Err("Confirmation preview expired".to_string());
        }
        if target.is_empty() || target.len() > 1_024 || executor_instance.is_empty() {
            return Err("Execution target or process instance is invalid".to_string());
        }
        let changed = transaction
            .execute(
                "UPDATE mutation_previews SET consumed_at_ms=?2
                 WHERE digest=?1 AND consumed_at_ms IS NULL",
                params![digest, to_i64(now_ms)?],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err("Confirmation preview was consumed concurrently".to_string());
        }
        transaction
            .execute(
                "INSERT INTO mutation_executions
                 (request_digest,action,target,request_json,external,state,
                  executor_instance,confirmed_at_ms,started_at_ms,finished_at_ms,
                  result_json,error,resolution,resolution_note,updated_at_ms)
                 VALUES(?1,?2,?3,?4,?5,'executing',?6,?7,?7,NULL,NULL,NULL,NULL,NULL,?7)",
                params![
                    digest,
                    stored.0,
                    target,
                    expected_request,
                    external,
                    executor_instance,
                    to_i64(now_ms)?,
                ],
            )
            .map_err(|error| {
                format!("Mutation already has an execution ledger entry or is invalid: {error}")
            })?;
        transaction
            .execute(
                "INSERT INTO delivery_audit
                 (occurred_at_ms,action,target,request_digest,outcome,detail)
                 VALUES(?1,?2,?3,?4,'pending',?5)",
                params![
                    to_i64(now_ms)?,
                    stored.0,
                    target,
                    digest,
                    if external {
                        "Confirmed external mutation; execution started"
                    } else {
                        "Confirmed local mutation; execution started"
                    },
                ],
            )
            .map_err(|error| {
                format!("Could not write the required pre-execution audit: {error}")
            })?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(stored.0)
    }

    pub fn finish_execution(
        &mut self,
        digest: &str,
        result: &Result<serde_json::Value, String>,
        external: bool,
        now_ms: u64,
    ) -> Result<MutationExecutionRecord, String> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let (
            action,
            target,
            stored_external,
            state,
            executor_instance,
            confirmed_at_ms,
            started_at_ms,
        ): (String, String, bool, String, String, i64, i64) = transaction
            .query_row(
                "SELECT action,target,external,state,executor_instance,
                        confirmed_at_ms,started_at_ms
                 FROM mutation_executions WHERE request_digest=?1",
                [digest],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .map_err(|error| error.to_string())?;
        if stored_external != external {
            return Err("Execution ledger external-scope mismatch".to_string());
        }
        if state != "executing" {
            return Err(format!("Mutation is already in '{state}' state"));
        }
        let (next_state, outcome, detail, stored_result, error) = match result {
            Ok(value) => (
                "completed",
                "success",
                "Mutation completed and its result was durably recorded".to_string(),
                Some(value.clone()),
                None,
            ),
            Err(error) if external => (
                "needs_reconciliation",
                "needs_reconciliation",
                format!(
                    "External command returned an ambiguous failure; inspect remote state before resolving: {}",
                    bounded(error, 4_096)
                ),
                None,
                Some(bounded(error, 16 * 1024)),
            ),
            Err(error) => (
                "failed",
                "failed",
                bounded(error, 4_096),
                None,
                Some(bounded(error, 16 * 1024)),
            ),
        };
        let result_json = stored_result
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|error| error.to_string())?;
        let finished_at_ms = to_i64(now_ms)?;
        let changed = transaction
            .execute(
                "UPDATE mutation_executions SET state=?2,finished_at_ms=?3,
                 result_json=?4,error=?5,updated_at_ms=?3
                 WHERE request_digest=?1 AND state='executing'",
                params![
                    digest,
                    next_state,
                    finished_at_ms,
                    result_json.as_deref(),
                    error.as_deref(),
                ],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err("Execution ledger changed concurrently".to_string());
        }
        transaction
            .execute(
                "INSERT INTO delivery_audit
                 (occurred_at_ms,action,target,request_digest,outcome,detail)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![finished_at_ms, action, target, digest, outcome, detail],
            )
            .map_err(|error| format!("Could not write the required completion audit: {error}"))?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(MutationExecutionRecord {
            request_digest: digest.to_string(),
            action,
            target,
            external,
            state: next_state.to_string(),
            executor_instance,
            confirmed_at_ms: from_i64(confirmed_at_ms).map_err(|error| error.to_string())?,
            started_at_ms: from_i64(started_at_ms).map_err(|error| error.to_string())?,
            finished_at_ms: Some(now_ms),
            result: stored_result,
            error,
            resolution: None,
            resolution_note: None,
            updated_at_ms: now_ms,
        })
    }

    /// Best-effort transition used only after the mutation may already have
    /// happened but atomic completion/audit failed. The state update is
    /// intentionally committed before the audit attempt; the fsynced fallback
    /// file covers a database-wide failure.
    pub fn force_needs_reconciliation(
        &mut self,
        digest: &str,
        detail: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        let changed = self
            .connection
            .execute(
                "UPDATE mutation_executions SET state='needs_reconciliation',
                 error=?2,finished_at_ms=?3,updated_at_ms=?3
                 WHERE request_digest=?1 AND state='executing'",
                params![digest, bounded(detail, 16 * 1024), to_i64(now_ms)?],
            )
            .map_err(|error| error.to_string())?;
        if changed == 0 {
            let state: Option<String> = self
                .connection
                .query_row(
                    "SELECT state FROM mutation_executions WHERE request_digest=?1",
                    [digest],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| error.to_string())?;
            if !matches!(state.as_deref(), Some("needs_reconciliation")) {
                return Err("Could not mark the execution for reconciliation".to_string());
            }
        }
        let execution = self
            .execution(digest)?
            .ok_or_else(|| "Execution disappeared while marking reconciliation".to_string())?;
        let _ = self.audit(
            now_ms,
            &execution.action,
            Some(&execution.target),
            digest,
            "needs_reconciliation",
            Some(detail),
        );
        Ok(())
    }

    pub fn execution(&self, digest: &str) -> Result<Option<MutationExecutionRecord>, String> {
        self.connection
            .query_row(
                "SELECT request_digest,action,target,external,state,executor_instance,
                        confirmed_at_ms,started_at_ms,finished_at_ms,result_json,error,
                        resolution,resolution_note,updated_at_ms
                 FROM mutation_executions WHERE request_digest=?1",
                [digest],
                read_execution,
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    pub fn reconciliations(&self) -> Result<Vec<MutationExecutionRecord>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT request_digest,action,target,external,state,executor_instance,
                        confirmed_at_ms,started_at_ms,finished_at_ms,result_json,error,
                        resolution,resolution_note,updated_at_ms
                 FROM mutation_executions WHERE state='needs_reconciliation'
                 ORDER BY updated_at_ms DESC,request_digest ASC",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], read_execution)
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn recover_interrupted_executions(
        &mut self,
        current_instance: &str,
        now_ms: u64,
    ) -> Result<usize, String> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let interrupted = {
            let mut statement = transaction
                .prepare(
                    "SELECT request_digest,action,target FROM mutation_executions
                     WHERE state='executing' AND executor_instance!=?1",
                )
                .map_err(|error| error.to_string())?;
            let rows = statement
                .query_map([current_instance], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(|error| error.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?
        };
        for (digest, action, target) in &interrupted {
            transaction
                .execute(
                    "UPDATE mutation_executions SET state='needs_reconciliation',
                     error='Application exited while the mutation was executing',
                     finished_at_ms=?2,updated_at_ms=?2 WHERE request_digest=?1",
                    params![digest, to_i64(now_ms)?],
                )
                .map_err(|error| error.to_string())?;
            transaction
                .execute(
                    "INSERT INTO delivery_audit
                     (occurred_at_ms,action,target,request_digest,outcome,detail)
                     VALUES(?1,?2,?3,?4,'needs_reconciliation',
                     'Recovered an interrupted execution; no automatic retry is allowed')",
                    params![to_i64(now_ms)?, action, target, digest],
                )
                .map_err(|error| error.to_string())?;
        }
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(interrupted.len())
    }

    pub fn resolve_reconciliation(
        &mut self,
        digest: &str,
        resolution: &str,
        note: &str,
        now_ms: u64,
    ) -> Result<MutationExecutionRecord, String> {
        let next_state = match resolution {
            "completed" => "reconciled_completed",
            "not_applied" => "reconciled_not_applied",
            _ => return Err("Resolution must be 'completed' or 'not_applied'".to_string()),
        };
        if note.trim().is_empty() || note.len() > 4_096 || note.contains('\0') {
            return Err("Reconciliation note must contain 1 to 4096 characters".to_string());
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let (action, target): (String, String) = transaction
            .query_row(
                "SELECT action,target FROM mutation_executions
                 WHERE request_digest=?1 AND state='needs_reconciliation'",
                [digest],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| "Execution is not awaiting reconciliation".to_string())?;
        transaction
            .execute(
                "UPDATE mutation_executions SET state=?2,resolution=?3,resolution_note=?4,
                 updated_at_ms=?5 WHERE request_digest=?1 AND state='needs_reconciliation'",
                params![digest, next_state, resolution, note.trim(), to_i64(now_ms)?],
            )
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT INTO delivery_audit
                 (occurred_at_ms,action,target,request_digest,outcome,detail)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    to_i64(now_ms)?,
                    action,
                    target,
                    digest,
                    next_state,
                    note.trim(),
                ],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        self.execution(digest)?
            .ok_or_else(|| "Resolved execution disappeared".to_string())
    }

    pub fn append_reconciliation_fallback(
        &self,
        digest: &str,
        action: &str,
        target: &str,
        detail: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        let path = self.root.join(RECONCILIATION_FALLBACK_FILE);
        let existing_len = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                    return Err("Reconciliation fallback path is not a regular file".to_string());
                }
                metadata.len()
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error.to_string()),
        };
        let entry = ReconciliationFallback {
            schema_version: 1,
            request_digest: digest.to_string(),
            action: action.to_string(),
            target: target.to_string(),
            detail: bounded(detail, 16 * 1024),
            occurred_at_ms: now_ms,
        };
        let mut bytes = serde_json::to_vec(&entry).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        if existing_len.saturating_add(bytes.len() as u64) > MAX_RECONCILIATION_FALLBACK_BYTES {
            return Err("Reconciliation fallback file would exceed 4 MiB".to_string());
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| format!("Could not open reconciliation fallback: {error}"))?;
        file.write_all(&bytes)
            .map_err(|error| format!("Could not write reconciliation fallback: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("Could not sync reconciliation fallback: {error}"))?;
        restrict_file(&path)
    }

    pub fn import_reconciliation_fallback(&mut self, now_ms: u64) -> Result<usize, String> {
        let path = self.root.join(RECONCILIATION_FALLBACK_FILE);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.to_string()),
        };
        if metadata.file_type().is_symlink()
            || !metadata.file_type().is_file()
            || metadata.len() > MAX_RECONCILIATION_FALLBACK_BYTES
        {
            return Err("Reconciliation fallback is unsafe or exceeds 4 MiB".to_string());
        }
        let bytes = fs::read(&path).map_err(|error| error.to_string())?;
        let mut entries = Vec::new();
        for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let entry: ReconciliationFallback = serde_json::from_slice(line).map_err(|error| {
                format!(
                    "Invalid reconciliation fallback line {}: {error}",
                    index + 1
                )
            })?;
            if entry.schema_version != 1 {
                return Err("Unsupported reconciliation fallback schema".to_string());
            }
            entries.push(entry);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let mut imported = 0usize;
        for entry in &entries {
            let state: Option<String> = transaction
                .query_row(
                    "SELECT state FROM mutation_executions WHERE request_digest=?1",
                    [&entry.request_digest],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| error.to_string())?;
            if !matches!(state.as_deref(), Some("executing" | "needs_reconciliation")) {
                continue;
            }
            transaction
                .execute(
                    "UPDATE mutation_executions SET state='needs_reconciliation',error=?2,
                     finished_at_ms=COALESCE(finished_at_ms,?3),updated_at_ms=?3
                     WHERE request_digest=?1 AND state IN ('executing','needs_reconciliation')",
                    params![
                        entry.request_digest,
                        entry.detail,
                        to_i64(now_ms.max(entry.occurred_at_ms))?,
                    ],
                )
                .map_err(|error| error.to_string())?;
            let already_audited: bool = transaction
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM delivery_audit
                     WHERE request_digest=?1 AND outcome='needs_reconciliation')",
                    [&entry.request_digest],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            if !already_audited {
                transaction
                    .execute(
                        "INSERT INTO delivery_audit
                         (occurred_at_ms,action,target,request_digest,outcome,detail)
                         VALUES(?1,?2,?3,?4,'needs_reconciliation',?5)",
                        params![
                            to_i64(now_ms.max(entry.occurred_at_ms))?,
                            entry.action,
                            entry.target,
                            entry.request_digest,
                            entry.detail,
                        ],
                    )
                    .map_err(|error| error.to_string())?;
            }
            imported += 1;
        }
        transaction.commit().map_err(|error| error.to_string())?;
        let archive = self.root.join("reconciliation-imported");
        ensure_private_directory(&archive)?;
        let archived = archive.join(format!(
            "{}-{}.jsonl",
            now_ms,
            uuid::Uuid::new_v4().simple()
        ));
        fs::rename(&path, &archived).map_err(|error| {
            format!("Could not archive imported reconciliation fallback: {error}")
        })?;
        restrict_file(&archived)?;
        Ok(imported)
    }

    pub fn insert_worktree(&mut self, record: &OwnedWorktreeRecord) -> Result<(), String> {
        let marker = serde_json::to_vec(&record.marker).map_err(|error| error.to_string())?;
        self.connection
            .execute(
                "INSERT INTO owned_worktrees
                 (worktree_id,marker_json,state,locked,lock_reason,archive_path,
                  created_at_ms,updated_at_ms)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(worktree_id) DO NOTHING",
                params![
                    record.marker.worktree_id,
                    marker,
                    record.state,
                    record.locked,
                    record.lock_reason,
                    record.archive_path,
                    to_i64(record.created_at_ms)?,
                    to_i64(record.updated_at_ms)?,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn worktree(&self, worktree_id: &str) -> Result<Option<OwnedWorktreeRecord>, String> {
        self.connection
            .query_row(
                "SELECT marker_json,state,locked,lock_reason,archive_path,
                        created_at_ms,updated_at_ms
                 FROM owned_worktrees WHERE worktree_id=?1",
                [worktree_id],
                read_worktree,
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    pub fn worktrees(&self) -> Result<Vec<OwnedWorktreeRecord>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT marker_json,state,locked,lock_reason,archive_path,
                        created_at_ms,updated_at_ms
                 FROM owned_worktrees ORDER BY created_at_ms DESC,worktree_id ASC",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], read_worktree)
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn update_worktree_state(
        &mut self,
        worktree_id: &str,
        state: &str,
        archive_path: Option<&str>,
        now_ms: u64,
    ) -> Result<(), String> {
        let changed = self
            .connection
            .execute(
                "UPDATE owned_worktrees SET state=?2,archive_path=COALESCE(?3,archive_path),
                        updated_at_ms=?4 WHERE worktree_id=?1",
                params![worktree_id, state, archive_path, to_i64(now_ms)?],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err(format!("Unknown owned worktree '{worktree_id}'"));
        }
        Ok(())
    }

    pub fn update_worktree_lock(
        &mut self,
        worktree_id: &str,
        locked: bool,
        reason: Option<&str>,
        now_ms: u64,
    ) -> Result<(), String> {
        let changed = self
            .connection
            .execute(
                "UPDATE owned_worktrees SET locked=?2,lock_reason=?3,updated_at_ms=?4
                 WHERE worktree_id=?1 AND state!='cleaned'",
                params![worktree_id, locked, reason, to_i64(now_ms)?],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err(format!("Unknown or cleaned worktree '{worktree_id}'"));
        }
        Ok(())
    }

    pub fn audit(
        &mut self,
        occurred_at_ms: u64,
        action: &str,
        target: Option<&str>,
        digest: &str,
        outcome: &str,
        detail: Option<&str>,
    ) -> Result<(), String> {
        self.connection
            .execute(
                "INSERT INTO delivery_audit
                 (occurred_at_ms,action,target,request_digest,outcome,detail)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    to_i64(occurred_at_ms)?,
                    action,
                    target,
                    digest,
                    outcome,
                    detail,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn audit_entries(&self, limit: u32) -> Result<Vec<AuditEntry>, String> {
        let limit = limit.clamp(1, 1_000);
        let mut statement = self
            .connection
            .prepare(
                "SELECT audit_id,occurred_at_ms,action,target,request_digest,outcome,detail
                 FROM delivery_audit ORDER BY audit_id DESC LIMIT ?1",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([i64::from(limit)], |row| {
                Ok(AuditEntry {
                    audit_id: from_i64(row.get(0)?)?,
                    occurred_at_ms: from_i64(row.get(1)?)?,
                    action: row.get(2)?,
                    target: row.get(3)?,
                    request_digest: row.get(4)?,
                    outcome: row.get(5)?,
                    detail: row.get(6)?,
                })
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn save_review(&mut self, report: &ReviewReport) -> Result<(), String> {
        let bytes = serde_json::to_vec(report).map_err(|error| error.to_string())?;
        self.connection
            .execute(
                "INSERT INTO review_reports
                 (report_id,repository_slug,pr_number,head_oid,model,report_json,
                  report_digest,published_comment_id,created_at_ms,updated_at_ms)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                 ON CONFLICT(report_id) DO UPDATE SET
                   report_json=excluded.report_json,
                   report_digest=excluded.report_digest,
                   published_comment_id=COALESCE(excluded.published_comment_id,review_reports.published_comment_id),
                   updated_at_ms=excluded.updated_at_ms",
                params![
                    report.report_id,
                    report.repository_slug,
                    i64::from(report.pr_number),
                    report.head_oid,
                    report.model,
                    bytes,
                    report.report_digest,
                    report.published_comment_id.map(i64::try_from).transpose().map_err(|_| "comment id overflow")?,
                    to_i64(report.created_at_ms)?,
                    to_i64(report.updated_at_ms)?,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn review(&self, report_id: &str) -> Result<Option<ReviewReport>, String> {
        self.connection
            .query_row(
                "SELECT report_json,published_comment_id FROM review_reports WHERE report_id=?1",
                [report_id],
                |row| {
                    let mut report: ReviewReport =
                        serde_json::from_slice(&row.get::<_, Vec<u8>>(0)?).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Blob,
                                Box::new(error),
                            )
                        })?;
                    report.published_comment_id =
                        row.get::<_, Option<i64>>(1)?.map(from_i64).transpose()?;
                    Ok(report)
                },
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    pub fn reviews_for_pr(
        &self,
        repository_slug: &str,
        pr_number: u32,
    ) -> Result<Vec<ReviewReport>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT report_json,published_comment_id FROM review_reports
                 WHERE repository_slug=?1 AND pr_number=?2
                 ORDER BY created_at_ms DESC",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(params![repository_slug, i64::from(pr_number)], |row| {
                let mut report: ReviewReport = serde_json::from_slice(&row.get::<_, Vec<u8>>(0)?)
                    .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Blob,
                        Box::new(error),
                    )
                })?;
                report.published_comment_id =
                    row.get::<_, Option<i64>>(1)?.map(from_i64).transpose()?;
                Ok(report)
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn mark_review_published(
        &mut self,
        report_id: &str,
        comment_id: u64,
        now_ms: u64,
    ) -> Result<(), String> {
        let changed = self
            .connection
            .execute(
                "UPDATE review_reports SET published_comment_id=?2,updated_at_ms=?3
                 WHERE report_id=?1",
                params![report_id, to_i64(comment_id)?, to_i64(now_ms)?],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err(format!("Unknown review report '{report_id}'"));
        }
        Ok(())
    }

    pub fn save_patch_task(
        &mut self,
        task_id: &str,
        repository_slug: &str,
        pr_number: u32,
        comment_id: u64,
        recipe_path: &str,
        run_id: Option<&str>,
        now_ms: u64,
    ) -> Result<(), String> {
        self.connection
            .execute(
                "INSERT INTO patch_tasks
                 (task_id,repository_slug,pr_number,comment_id,recipe_path,run_id,created_at_ms,updated_at_ms)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?7)
                 ON CONFLICT(task_id) DO UPDATE SET
                   run_id=COALESCE(excluded.run_id,patch_tasks.run_id),updated_at_ms=excluded.updated_at_ms",
                params![
                    task_id,
                    repository_slug,
                    i64::from(pr_number),
                    to_i64(comment_id)?,
                    recipe_path,
                    run_id,
                    to_i64(now_ms)?,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

fn read_worktree(row: &rusqlite::Row<'_>) -> rusqlite::Result<OwnedWorktreeRecord> {
    let marker = serde_json::from_slice(&row.get::<_, Vec<u8>>(0)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, Box::new(error))
    })?;
    Ok(OwnedWorktreeRecord {
        marker,
        state: row.get(1)?,
        locked: row.get(2)?,
        lock_reason: row.get(3)?,
        archive_path: row.get(4)?,
        created_at_ms: from_i64(row.get(5)?)?,
        updated_at_ms: from_i64(row.get(6)?)?,
    })
}

fn read_execution(row: &rusqlite::Row<'_>) -> rusqlite::Result<MutationExecutionRecord> {
    let result = row
        .get::<_, Option<Vec<u8>>>(9)?
        .map(|bytes| {
            serde_json::from_slice(&bytes).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    9,
                    rusqlite::types::Type::Blob,
                    Box::new(error),
                )
            })
        })
        .transpose()?;
    Ok(MutationExecutionRecord {
        request_digest: row.get(0)?,
        action: row.get(1)?,
        target: row.get(2)?,
        external: row.get(3)?,
        state: row.get(4)?,
        executor_instance: row.get(5)?,
        confirmed_at_ms: from_i64(row.get(6)?)?,
        started_at_ms: from_i64(row.get(7)?)?,
        finished_at_ms: row.get::<_, Option<i64>>(8)?.map(from_i64).transpose()?,
        result,
        error: row.get(10)?,
        resolution: row.get(11)?,
        resolution_note: row.get(12)?,
        updated_at_ms: from_i64(row.get(13)?)?,
    })
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReconciliationFallback {
    schema_version: u32,
    request_digest: String,
    action: String,
    target: String,
    detail: String,
    occurred_at_ms: u64,
}

pub fn ensure_private_directory(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("Could not create '{}': {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not protect '{}': {error}", path.display()))?;
    }
    Ok(())
}

pub fn restrict_file(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Could not protect '{}': {error}", path.display()))?;
    }
    Ok(())
}

fn to_i64(value: u64) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| "numeric value exceeds SQLite range".to_string())
}

fn from_i64(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
}

fn bounded(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::m5_delivery::ReviewFinding;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "little-monkey-m5-store-{label}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            Self(path)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn confirmation_is_exact_expiring_one_time_and_failures_are_audited() {
        let root = TempRoot::new("preview-audit");
        let mut store = DeliveryStore::open_in_memory(root.0.clone()).unwrap();
        let request = br#"{"kind":"push","payload":{"remote":"origin"}}"#;
        store
            .save_preview("a", "push", request, 1_000, 2_000)
            .unwrap();
        assert!(store.consume_preview("a", b"different", 1_100).is_err());
        assert_eq!(store.consume_preview("a", request, 1_100).unwrap(), "push");
        assert!(store.consume_preview("a", request, 1_200).is_err());
        store
            .save_preview("b", "push", request, 1_000, 2_000)
            .unwrap();
        assert!(store.consume_preview("b", request, 2_001).is_err());
        store
            .audit(
                1_300,
                "create_draft_pr",
                Some("owner/repo:codex/fixture"),
                "a",
                "failed",
                Some("authentication expired"),
            )
            .unwrap();
        let entries = store.audit_entries(10).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].outcome, "failed");
        assert_eq!(entries[0].request_digest, "a");
        assert!(entries[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("authentication"));
    }

    #[test]
    fn review_upsert_preserves_one_report_and_published_comment_identity() {
        let root = TempRoot::new("review-upsert");
        let mut store = DeliveryStore::open_in_memory(root.0.clone()).unwrap();
        let mut report = ReviewReport {
            report_id: "review-fixture".to_string(),
            repository_slug: "owner/repo".to_string(),
            pr_number: 7,
            head_oid: "a".repeat(40),
            model: "fixture".to_string(),
            summary: "First".to_string(),
            findings: vec![ReviewFinding {
                finding_id: "finding-a".to_string(),
                severity: "warning".to_string(),
                path: "src/lib.rs".to_string(),
                line: 3,
                title: "Fixture".to_string(),
                body: "Fix it".to_string(),
            }],
            report_digest: "b".repeat(64),
            published_comment_id: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        store.save_review(&report).unwrap();
        store
            .mark_review_published(&report.report_id, 99, 2)
            .unwrap();
        report.summary = "Updated".to_string();
        report.report_digest = "c".repeat(64);
        report.updated_at_ms = 3;
        store.save_review(&report).unwrap();
        let reports = store.reviews_for_pr("owner/repo", 7).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].summary, "Updated");
        assert_eq!(reports[0].published_comment_id, Some(99));
    }

    #[test]
    fn pending_audit_failure_rolls_back_confirmation_and_prevents_dispatch() {
        let root = TempRoot::new("pending-audit-failure");
        let mut store = DeliveryStore::open_in_memory(root.0.clone()).unwrap();
        let request = br#"{"kind":"push","payload":{"worktreeId":"wt","remote":"origin"}}"#;
        store
            .save_preview("digest-pending", "push", request, 1_000, 10_000)
            .unwrap();
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER fail_pending_audit BEFORE INSERT ON delivery_audit
                 WHEN NEW.outcome='pending' BEGIN
                   SELECT RAISE(ABORT,'fixture pending audit failure');
                 END;",
            )
            .unwrap();
        let began = store.confirm_and_begin_execution(
            "digest-pending",
            request,
            "push",
            "owner/repo:codex/fixture",
            true,
            "instance-a",
            2_000,
        );
        assert!(began.unwrap_err().contains("pre-execution audit"));
        assert!(store.execution("digest-pending").unwrap().is_none());
        let consumed: Option<i64> = store
            .connection
            .query_row(
                "SELECT consumed_at_ms FROM mutation_previews WHERE digest='digest-pending'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(consumed, None);
        assert!(store.audit_entries(10).unwrap().is_empty());
    }

    #[test]
    fn execution_ledger_orders_pending_before_terminal_and_audit_is_append_only() {
        let root = TempRoot::new("execution-ledger");
        let mut store = DeliveryStore::open_in_memory(root.0.clone()).unwrap();
        let local_request = br#"{"kind":"commit"}"#;
        store
            .save_preview("digest-local", "commit", local_request, 1_000, 10_000)
            .unwrap();
        store
            .confirm_and_begin_execution(
                "digest-local",
                local_request,
                "commit",
                "owner/repo:codex/fixture",
                false,
                "instance-a",
                2_000,
            )
            .unwrap();
        let completed = store
            .finish_execution(
                "digest-local",
                &Ok(serde_json::json!({ "oid": "a".repeat(40) })),
                false,
                3_000,
            )
            .unwrap();
        assert_eq!(completed.state, "completed");
        assert_eq!(completed.result.unwrap()["oid"], "a".repeat(40));

        let external_request = br#"{"kind":"push"}"#;
        store
            .save_preview("digest-external", "push", external_request, 4_000, 10_000)
            .unwrap();
        store
            .confirm_and_begin_execution(
                "digest-external",
                external_request,
                "push",
                "owner/repo:codex/fixture",
                true,
                "instance-a",
                5_000,
            )
            .unwrap();
        let ambiguous = store
            .finish_execution(
                "digest-external",
                &Err("connection ended after request dispatch".to_string()),
                true,
                6_000,
            )
            .unwrap();
        assert_eq!(ambiguous.state, "needs_reconciliation");
        assert_eq!(store.reconciliations().unwrap().len(), 1);

        let entries = store.audit_entries(20).unwrap();
        let local = entries
            .iter()
            .filter(|entry| entry.request_digest == "digest-local")
            .map(|entry| entry.outcome.as_str())
            .collect::<Vec<_>>();
        assert_eq!(local, ["success", "pending"]);
        let external = entries
            .iter()
            .filter(|entry| entry.request_digest == "digest-external")
            .map(|entry| entry.outcome.as_str())
            .collect::<Vec<_>>();
        assert_eq!(external, ["needs_reconciliation", "pending"]);
        assert!(store
            .connection
            .execute("UPDATE delivery_audit SET outcome='tampered'", [])
            .is_err());
        assert!(store
            .connection
            .execute("DELETE FROM delivery_audit", [])
            .is_err());
    }

    #[test]
    fn final_audit_failure_is_fsynced_imported_and_never_replayed() {
        let root = TempRoot::new("final-audit-failure");
        let mut store = DeliveryStore::open_in_memory(root.0.clone()).unwrap();
        let request = br#"{"kind":"create_draft_pr"}"#;
        store
            .save_preview("digest-final", "create_draft_pr", request, 1_000, 10_000)
            .unwrap();
        store
            .confirm_and_begin_execution(
                "digest-final",
                request,
                "create_draft_pr",
                "owner/repo:codex/fixture",
                true,
                "instance-a",
                2_000,
            )
            .unwrap();
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER fail_completion_audit BEFORE INSERT ON delivery_audit
                 WHEN NEW.outcome!='pending' BEGIN
                   SELECT RAISE(ABORT,'fixture completion audit failure');
                 END;",
            )
            .unwrap();
        let result = Ok(serde_json::json!({ "number": 17 }));
        assert!(store
            .finish_execution("digest-final", &result, true, 3_000)
            .unwrap_err()
            .contains("completion audit"));
        assert_eq!(
            store.execution("digest-final").unwrap().unwrap().state,
            "executing"
        );
        let detail = "Remote PR may have been created; completion audit failed";
        store
            .force_needs_reconciliation("digest-final", detail, 3_100)
            .unwrap();
        store
            .append_reconciliation_fallback(
                "digest-final",
                "create_draft_pr",
                "owner/repo:codex/fixture",
                detail,
                3_100,
            )
            .unwrap();
        store
            .connection
            .execute_batch("DROP TRIGGER fail_completion_audit;")
            .unwrap();
        assert_eq!(store.import_reconciliation_fallback(3_200).unwrap(), 1);
        let execution = store.execution("digest-final").unwrap().unwrap();
        assert_eq!(execution.state, "needs_reconciliation");
        assert_eq!(store.reconciliations().unwrap().len(), 1);
        assert!(store
            .confirm_and_begin_execution(
                "digest-final",
                request,
                "create_draft_pr",
                "owner/repo:codex/fixture",
                true,
                "instance-a",
                3_300,
            )
            .is_err());
        let needs_audit = store
            .audit_entries(20)
            .unwrap()
            .into_iter()
            .filter(|entry| {
                entry.request_digest == "digest-final" && entry.outcome == "needs_reconciliation"
            })
            .count();
        assert_eq!(needs_audit, 1);
        let resolved = store
            .resolve_reconciliation(
                "digest-final",
                "completed",
                "Verified draft PR #17 exists at the expected head",
                4_000,
            )
            .unwrap();
        assert_eq!(resolved.state, "reconciled_completed");
        assert!(store.reconciliations().unwrap().is_empty());
    }

    #[test]
    fn prior_process_execution_recovers_to_reconciliation_without_retry() {
        let root = TempRoot::new("crash-recovery");
        let app_data = root.0.clone();
        {
            let mut store = DeliveryStore::open(&app_data).unwrap();
            let request = br#"{"kind":"push"}"#;
            store
                .save_preview("digest-crash", "push", request, 1_000, 10_000)
                .unwrap();
            store
                .confirm_and_begin_execution(
                    "digest-crash",
                    request,
                    "push",
                    "owner/repo:codex/fixture",
                    true,
                    "process-before-crash",
                    2_000,
                )
                .unwrap();
        }
        let mut reopened = DeliveryStore::open(&app_data).unwrap();
        assert_eq!(
            reopened
                .recover_interrupted_executions("process-after-restart", 3_000)
                .unwrap(),
            1
        );
        let execution = reopened.execution("digest-crash").unwrap().unwrap();
        assert_eq!(execution.state, "needs_reconciliation");
        assert!(execution.error.unwrap().contains("exited"));
        assert!(reopened
            .audit_entries(20)
            .unwrap()
            .iter()
            .any(|entry| entry.request_digest == "digest-crash"
                && entry.outcome == "needs_reconciliation"));
    }
}
