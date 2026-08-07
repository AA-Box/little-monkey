use std::path::Path;
use std::time::Duration;

use little_monkey_lib::run_ledger::{RunLedger, StoredApproval, StoredRun};
use little_monkey_lib::run_protocol::RunEventEnvelope;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseToken {
    pub run_id: String,
    pub owner_id: String,
    pub token_sha256: String,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalMutationState {
    pub mutation_id: String,
    pub state: String,
    pub summary: String,
}

#[derive(Debug, Clone)]
pub struct StoredTrigger {
    pub trigger_id: String,
    pub kind: String,
    pub config_json: Vec<u8>,
    pub enabled: bool,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub next_fire_at_ms: Option<u64>,
    pub last_delivery_at_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct TriggerReplacement {
    pub trigger_id: String,
    pub kind: String,
    pub config_json: Vec<u8>,
    pub next_fire_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryDisposition {
    Accepted,
    Duplicate,
    ConflictingDuplicate,
}

pub struct SharedLedger {
    path: std::path::PathBuf,
    connection: Connection,
}

impl SharedLedger {
    /// The unified agent process table on this connection.
    ///
    /// The daemon reads and writes it through the same typed view the desktop
    /// and `monkey processes` use, so the state machine has one implementation
    /// rather than a daemon-shaped copy.
    pub fn process_table(&self) -> little_monkey_lib::process_table::ProcessTable<'_> {
        little_monkey_lib::process_table::ProcessTable::new(&self.connection)
    }

    pub fn open(path: &Path) -> Result<Self, String> {
        RunLedger::open(path).map_err(|error| error.to_string())?;
        let connection = Connection::open(path).map_err(|error| error.to_string())?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|error| error.to_string())?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")
            .map_err(|error| error.to_string())?;
        Ok(Self {
            path: path.to_path_buf(),
            connection,
        })
    }

    pub fn run_ledger(&self) -> Result<RunLedger, String> {
        RunLedger::open(&self.path).map_err(|error| error.to_string())
    }

    pub fn load_run(&self, run_id: &str) -> Result<Option<StoredRun>, String> {
        self.run_ledger()?
            .load_run(run_id)
            .map_err(|error| error.to_string())
    }

    pub fn events(
        &self,
        run_id: &str,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<RunEventEnvelope>, String> {
        self.run_ledger()?
            .load_events(run_id, after_sequence, limit)
            .map_err(|error| error.to_string())
    }

    pub fn acquire_lease(
        &mut self,
        run_id: &str,
        owner_id: &str,
        now_ms: u64,
        duration_ms: u64,
    ) -> Result<Option<LeaseToken>, String> {
        let expires_at_ms = now_ms
            .checked_add(duration_ms)
            .ok_or_else(|| "lease expiry overflow".to_string())?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let current = transaction
            .query_row(
                "SELECT owner_id, generation, expires_at_ms FROM run_leases WHERE run_id=?1",
                [run_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if let Some((current_owner, _generation, expiry)) = &current {
            if *expiry > to_i64(now_ms)? && current_owner != owner_id {
                transaction.commit().map_err(|error| error.to_string())?;
                return Ok(None);
            }
        }
        let generation = current
            .map(|(_, value, _)| value)
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| "lease generation overflow".to_string())?;
        let token = uuid::Uuid::new_v4().to_string();
        let token_sha256 = sha256_hex(token.as_bytes());
        transaction
            .execute(
                "INSERT INTO run_leases (
                    run_id, owner_id, lease_token_sha256, generation,
                    acquired_at_ms, heartbeat_at_ms, expires_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)
                 ON CONFLICT(run_id) DO UPDATE SET
                    owner_id=excluded.owner_id,
                    lease_token_sha256=excluded.lease_token_sha256,
                    generation=excluded.generation,
                    acquired_at_ms=excluded.acquired_at_ms,
                    heartbeat_at_ms=excluded.heartbeat_at_ms,
                    expires_at_ms=excluded.expires_at_ms",
                params![
                    run_id,
                    owner_id,
                    token_sha256,
                    generation,
                    to_i64(now_ms)?,
                    to_i64(expires_at_ms)?,
                ],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(Some(LeaseToken {
            run_id: run_id.to_string(),
            owner_id: owner_id.to_string(),
            token_sha256,
            generation: u64::try_from(generation)
                .map_err(|_| "lease generation is invalid".to_string())?,
        }))
    }

    pub fn heartbeat_lease(
        &mut self,
        lease: &LeaseToken,
        now_ms: u64,
        duration_ms: u64,
    ) -> Result<bool, String> {
        let expiry = now_ms
            .checked_add(duration_ms)
            .ok_or_else(|| "lease expiry overflow".to_string())?;
        let changed = self
            .connection
            .execute(
                "UPDATE run_leases SET heartbeat_at_ms=?5, expires_at_ms=?6
                 WHERE run_id=?1 AND owner_id=?2 AND lease_token_sha256=?3 AND generation=?4",
                params![
                    lease.run_id,
                    lease.owner_id,
                    lease.token_sha256,
                    to_i64(lease.generation)?,
                    to_i64(now_ms)?,
                    to_i64(expiry)?,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(changed == 1)
    }

    pub fn release_lease(&mut self, lease: &LeaseToken) -> Result<bool, String> {
        let changed = self
            .connection
            .execute(
                "DELETE FROM run_leases
                 WHERE run_id=?1 AND owner_id=?2 AND lease_token_sha256=?3 AND generation=?4",
                params![
                    lease.run_id,
                    lease.owner_id,
                    lease.token_sha256,
                    to_i64(lease.generation)?,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(changed == 1)
    }

    pub fn mutations(&self, run_id: &str) -> Result<Vec<ExternalMutationState>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT mutation_id, state, summary FROM external_mutations
                 WHERE run_id=?1 ORDER BY prepared_sequence ASC",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([run_id], |row| {
                Ok(ExternalMutationState {
                    mutation_id: row.get(0)?,
                    state: row.get(1)?,
                    summary: row.get(2)?,
                })
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn pending_approvals(&self, run_id: &str) -> Result<Vec<StoredApproval>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT request_id FROM approvals
                 WHERE run_id=?1 AND decision IS NULL ORDER BY requested_sequence ASC",
            )
            .map_err(|error| error.to_string())?;
        let ids = statement
            .query_map([run_id], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        let ledger = self.run_ledger()?;
        ids.into_iter()
            .map(|id| {
                ledger
                    .load_approval(run_id, &id)
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| format!("Approval '{id}' disappeared"))
            })
            .collect()
    }

    pub fn upsert_trigger(
        &mut self,
        trigger_id: &str,
        kind: &str,
        config_json: &[u8],
        now_ms: u64,
        next_fire_at_ms: Option<u64>,
    ) -> Result<(), String> {
        self.connection
            .execute(
                "INSERT INTO triggers (
                    trigger_id, kind, config_json, enabled, created_at_ms,
                    updated_at_ms, next_fire_at_ms, last_delivery_at_ms
                 ) VALUES (?1, ?2, ?3, 1, ?4, ?4, ?5, NULL)
                 ON CONFLICT(trigger_id) DO UPDATE SET
                    kind=excluded.kind,
                    config_json=excluded.config_json,
                    enabled=1,
                    updated_at_ms=excluded.updated_at_ms,
                    next_fire_at_ms=excluded.next_fire_at_ms",
                params![
                    trigger_id,
                    kind,
                    config_json,
                    to_i64(now_ms)?,
                    next_fire_at_ms.map(to_i64).transpose()?,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Atomically disables the previous M4-owned trigger set and enables the
    /// complete replacement. Rows are retained (rather than deleted) because
    /// accepted delivery history intentionally has restrictive foreign keys.
    pub fn replace_trigger_batch(
        &mut self,
        previous_trigger_ids: &[String],
        replacements: &[TriggerReplacement],
        now_ms: u64,
    ) -> Result<(), String> {
        let mut seen = std::collections::BTreeSet::new();
        for replacement in replacements {
            if replacement.trigger_id.is_empty()
                || replacement.trigger_id.len() > 128
                || !replacement
                    .trigger_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            {
                return Err("replacement trigger id is invalid".to_string());
            }
            if replacement.kind.is_empty() || replacement.kind.len() > 64 {
                return Err("replacement trigger kind is invalid".to_string());
            }
            if !seen.insert(replacement.trigger_id.as_str()) {
                return Err(format!(
                    "duplicate replacement trigger '{}'",
                    replacement.trigger_id
                ));
            }
        }
        let now = to_i64(now_ms)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        for trigger_id in previous_trigger_ids {
            transaction
                .execute(
                    "UPDATE triggers SET enabled=0, updated_at_ms=?2 WHERE trigger_id=?1",
                    params![trigger_id, now],
                )
                .map_err(|error| error.to_string())?;
        }
        for replacement in replacements {
            transaction
                .execute(
                    "INSERT INTO triggers (
                        trigger_id, kind, config_json, enabled, created_at_ms,
                        updated_at_ms, next_fire_at_ms, last_delivery_at_ms
                     ) VALUES (?1, ?2, ?3, 1, ?4, ?4, ?5, NULL)
                     ON CONFLICT(trigger_id) DO UPDATE SET
                        kind=excluded.kind,
                        config_json=excluded.config_json,
                        enabled=1,
                        updated_at_ms=excluded.updated_at_ms,
                        next_fire_at_ms=excluded.next_fire_at_ms",
                    params![
                        replacement.trigger_id,
                        replacement.kind,
                        replacement.config_json,
                        now,
                        replacement.next_fire_at_ms.map(to_i64).transpose()?,
                    ],
                )
                .map_err(|error| error.to_string())?;
        }
        transaction.commit().map_err(|error| error.to_string())
    }

    pub fn list_triggers(&self) -> Result<Vec<StoredTrigger>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT trigger_id, kind, config_json, enabled, created_at_ms,
                        updated_at_ms, next_fire_at_ms, last_delivery_at_ms
                 FROM triggers ORDER BY trigger_id ASC",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok(StoredTrigger {
                    trigger_id: row.get(0)?,
                    kind: row.get(1)?,
                    config_json: row.get(2)?,
                    enabled: row.get::<_, i64>(3)? != 0,
                    created_at_ms: from_i64(row.get(4)?)?,
                    updated_at_ms: from_i64(row.get(5)?)?,
                    next_fire_at_ms: row.get::<_, Option<i64>>(6)?.map(from_i64).transpose()?,
                    last_delivery_at_ms: row.get::<_, Option<i64>>(7)?.map(from_i64).transpose()?,
                })
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn trigger(&self, trigger_id: &str) -> Result<Option<StoredTrigger>, String> {
        self.connection
            .query_row(
                "SELECT trigger_id, kind, config_json, enabled, created_at_ms,
                        updated_at_ms, next_fire_at_ms, last_delivery_at_ms
                 FROM triggers WHERE trigger_id=?1",
                [trigger_id],
                |row| {
                    Ok(StoredTrigger {
                        trigger_id: row.get(0)?,
                        kind: row.get(1)?,
                        config_json: row.get(2)?,
                        enabled: row.get::<_, i64>(3)? != 0,
                        created_at_ms: from_i64(row.get(4)?)?,
                        updated_at_ms: from_i64(row.get(5)?)?,
                        next_fire_at_ms: row.get::<_, Option<i64>>(6)?.map(from_i64).transpose()?,
                        last_delivery_at_ms: row
                            .get::<_, Option<i64>>(7)?
                            .map(from_i64)
                            .transpose()?,
                    })
                },
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    pub fn disable_trigger(&mut self, trigger_id: &str, now_ms: u64) -> Result<bool, String> {
        self.connection
            .execute(
                "UPDATE triggers SET enabled=0, updated_at_ms=?2 WHERE trigger_id=?1",
                params![trigger_id, to_i64(now_ms)?],
            )
            .map(|changed| changed == 1)
            .map_err(|error| error.to_string())
    }

    pub fn update_trigger_schedule(
        &mut self,
        trigger_id: &str,
        next_fire_at_ms: Option<u64>,
        last_delivery_at_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<(), String> {
        self.connection
            .execute(
                "UPDATE triggers SET next_fire_at_ms=?2,
                    last_delivery_at_ms=COALESCE(?3,last_delivery_at_ms), updated_at_ms=?4
                 WHERE trigger_id=?1",
                params![
                    trigger_id,
                    next_fire_at_ms.map(to_i64).transpose()?,
                    last_delivery_at_ms.map(to_i64).transpose()?,
                    to_i64(now_ms)?,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn accept_delivery(
        &mut self,
        trigger_id: &str,
        delivery_id: &str,
        payload_sha256: &str,
        now_ms: u64,
    ) -> Result<DeliveryDisposition, String> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        if let Some(existing) = transaction
            .query_row(
                "SELECT payload_sha256 FROM trigger_deliveries
                 WHERE trigger_id=?1 AND delivery_id=?2",
                params![trigger_id, delivery_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
        {
            transaction.commit().map_err(|error| error.to_string())?;
            return Ok(if existing == payload_sha256 {
                DeliveryDisposition::Duplicate
            } else {
                DeliveryDisposition::ConflictingDuplicate
            });
        }
        transaction
            .execute(
                "INSERT INTO trigger_deliveries (
                    trigger_id, delivery_id, payload_sha256, received_at_ms, status, run_id
                 ) VALUES (?1, ?2, ?3, ?4, 'accepted', NULL)",
                params![trigger_id, delivery_id, payload_sha256, to_i64(now_ms)?],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(DeliveryDisposition::Accepted)
    }

    pub fn delivery(
        &self,
        trigger_id: &str,
        delivery_id: &str,
    ) -> Result<Option<(String, String, Option<String>)>, String> {
        self.connection
            .query_row(
                "SELECT status, payload_sha256, run_id FROM trigger_deliveries
                 WHERE trigger_id=?1 AND delivery_id=?2",
                params![trigger_id, delivery_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    pub fn reject_delivery(
        &mut self,
        trigger_id: &str,
        delivery_id: &str,
        payload_sha256: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        self.connection
            .execute(
                "INSERT INTO trigger_deliveries (
                    trigger_id, delivery_id, payload_sha256, received_at_ms, status, run_id
                 ) VALUES (?1, ?2, ?3, ?4, 'rejected', NULL)
                 ON CONFLICT(trigger_id, delivery_id) DO NOTHING",
                params![trigger_id, delivery_id, payload_sha256, to_i64(now_ms)?],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn mark_delivery_submitted(
        &mut self,
        trigger_id: &str,
        delivery_id: &str,
        run_id: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        let changed = self
            .connection
            .execute(
                "UPDATE trigger_deliveries SET status='submitted', run_id=?3
                 WHERE trigger_id=?1 AND delivery_id=?2 AND status='accepted'",
                params![trigger_id, delivery_id, run_id],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            let existing = self.delivery(trigger_id, delivery_id)?;
            if !matches!(
                existing,
                Some((ref status, _, Some(ref existing_run)))
                    if status == "submitted" && existing_run == run_id
            ) {
                return Err(format!(
                    "Trigger delivery '{trigger_id}/{delivery_id}' is not pending"
                ));
            }
        }
        self.connection
            .execute(
                "UPDATE triggers SET last_delivery_at_ms=?2, updated_at_ms=?2
                 WHERE trigger_id=?1",
                params![trigger_id, to_i64(now_ms)?],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Marks a delivery handed to the M4 workflow ledger. Workflow run ids
    /// live in M4's append-only history rather than the shared recipe `runs`
    /// table, so the nullable foreign-key column deliberately remains NULL.
    pub fn mark_delivery_submitted_external(
        &mut self,
        trigger_id: &str,
        delivery_id: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        let changed = self
            .connection
            .execute(
                "UPDATE trigger_deliveries SET status='submitted', run_id=NULL
                 WHERE trigger_id=?1 AND delivery_id=?2 AND status='accepted'",
                params![trigger_id, delivery_id],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            let existing = self.delivery(trigger_id, delivery_id)?;
            if !matches!(existing, Some((ref status, _, None)) if status == "submitted") {
                return Err(format!(
                    "Trigger delivery '{trigger_id}/{delivery_id}' is not pending"
                ));
            }
        }
        self.connection
            .execute(
                "UPDATE triggers SET last_delivery_at_ms=?2, updated_at_ms=?2
                 WHERE trigger_id=?1",
                params![trigger_id, to_i64(now_ms)?],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_worktree_lease(
        &mut self,
        lease_id: &str,
        run_id: &str,
        repository_id: &str,
        common_git_dir: &str,
        canonical_path: &str,
        branch: &str,
        base_oid: &str,
        expected_head: Option<&str>,
        state: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        self.connection
            .execute(
                "INSERT INTO worktree_leases (
                    lease_id, run_id, repository_id, common_git_dir, canonical_path,
                    branch, base_oid, expected_head, state, created_at_ms,
                    heartbeat_at_ms, released_at_ms
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10,NULL)
                 ON CONFLICT(lease_id) DO UPDATE SET
                    heartbeat_at_ms=excluded.heartbeat_at_ms,
                    expected_head=excluded.expected_head,
                    state=excluded.state",
                params![
                    lease_id,
                    run_id,
                    repository_id,
                    common_git_dir,
                    canonical_path,
                    branch,
                    base_oid,
                    expected_head,
                    state,
                    to_i64(now_ms)?,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn update_worktree_lease(
        &mut self,
        lease_id: &str,
        state: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        self.connection
            .execute(
                "UPDATE worktree_leases SET state=?2, heartbeat_at_ms=?3,
                    released_at_ms=CASE WHEN ?2='released' THEN ?3 ELSE released_at_ms END
                 WHERE lease_id=?1",
                params![lease_id, state, to_i64(now_ms)?],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn to_i64(value: u64) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| "numeric value exceeds SQLite range".to_string())
}

fn from_i64(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::run_protocol::{
        ClientIdentity, ClientKind, ModelTargetSnapshot, PermissionMode as RunPermissionMode,
        PermissionPolicySnapshot, RootAccess, RootGrant, RunBudgets, RunKind, RunSpec,
        ToolPolicyDecision, WorkspaceContext, RUN_PROTOCOL_SCHEMA_VERSION,
    };

    fn spec(id: &str) -> RunSpec {
        RunSpec {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            run_id: id.to_string(),
            idempotency_key: format!("idem-{id}"),
            created_at_ms: 1,
            kind: RunKind::Workflow,
            submitted_by: ClientIdentity {
                client_id: "daemon-test".into(),
                instance_id: "instance".into(),
                kind: ClientKind::Daemon,
                version: "1".into(),
            },
            task: "test".into(),
            instructions: None,
            input_artifact_ids: vec![],
            target: ModelTargetSnapshot::Provider {
                target_id: "target".into(),
                label: "test".into(),
                provider_id: "test".into(),
                endpoint: "http://127.0.0.1:1/v1".into(),
                model: "test".into(),
                credential_ref_id: "credential-none".into(),
                capabilities: crate::task::cli_capabilities(),
            },
            workspace: Some(WorkspaceContext {
                workspace_id: "workspace".into(),
                primary_root_id: "root".into(),
                roots: vec![RootGrant {
                    root_id: "root".into(),
                    canonical_path: "/tmp".into(),
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
                egress_allowlist: None,
            },
            budgets: RunBudgets {
                wall_time_ms: 60_000,
                max_iterations: 4,
                max_model_calls: 4,
                max_tool_calls: 4,
                max_input_tokens: 1000,
                max_output_tokens: 1000,
                max_cost_micros: None,
                max_artifact_bytes: 1024,
                max_event_count: 100,
            },
        }
    }

    #[test]
    fn lease_cannot_be_stolen_before_expiry_and_generation_advances_after() {
        let dir = std::env::temp_dir().join(format!("lm-daemon-ledger-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("profile.sqlite3");
        RunLedger::open(&path)
            .unwrap()
            .submit_run(&spec("run-one"))
            .unwrap();
        let mut ledger = SharedLedger::open(&path).unwrap();
        let first = ledger
            .acquire_lease("run-one", "a", 10, 100)
            .unwrap()
            .unwrap();
        assert!(ledger
            .acquire_lease("run-one", "b", 20, 100)
            .unwrap()
            .is_none());
        let second = ledger
            .acquire_lease("run-one", "b", 111, 100)
            .unwrap()
            .unwrap();
        assert!(second.generation > first.generation);
        assert!(!ledger.heartbeat_lease(&first, 120, 100).unwrap());
        assert!(ledger.heartbeat_lease(&second, 120, 100).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn external_workflow_delivery_is_idempotent_without_recipe_run_foreign_key() {
        let dir = std::env::temp_dir().join(format!("lm-daemon-ledger-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("profile.sqlite3");
        let mut ledger = SharedLedger::open(&path).unwrap();
        ledger
            .upsert_trigger("workflow-hook", "signed_webhook", b"{}", 10, None)
            .unwrap();
        assert_eq!(
            ledger
                .accept_delivery("workflow-hook", "delivery-one", &"a".repeat(64), 11)
                .unwrap(),
            DeliveryDisposition::Accepted
        );
        ledger
            .mark_delivery_submitted_external("workflow-hook", "delivery-one", 12)
            .unwrap();
        ledger
            .mark_delivery_submitted_external("workflow-hook", "delivery-one", 13)
            .unwrap();
        assert!(matches!(
            ledger.delivery("workflow-hook", "delivery-one").unwrap(),
            Some((ref status, _, None)) if status == "submitted"
        ));
        let _ = std::fs::remove_dir_all(dir);
    }
}
