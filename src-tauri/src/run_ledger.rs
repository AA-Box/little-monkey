//! Durable, append-only run ledger shared by every execution surface.
//!
//! The ledger is intentionally synchronous and independent of Tauri. A
//! daemon can own it directly, while desktop/CLI tests can use the same API
//! through an embedded host. SQLite serializes writers; `BEGIN IMMEDIATE`
//! plus database triggers make event sequence assignment race-safe across
//! processes and preserve terminal-state invariants even if a future caller
//! bypasses the high-level checks in this module.

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::limits::Limit;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;

use crate::run_protocol::{
    ArtifactKind, CheckpointKind, ClientIdentity, MutationKind, PermissionDecision, RiskLevel,
    RunEvent, RunEventEnvelope, RunSpec, RunStatus,
};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_SQLITE_VALUE_BYTES: i32 = 8 * 1024 * 1024;
const MAX_SQL_TEXT_BYTES: i32 = 1024 * 1024;
const MAX_LIST_LIMIT: usize = 1_000;
const MIGRATION_V1: i64 = 1;
const MIGRATION_V1_CHECKSUM: &str = "run-ledger-v1-2026-07-13";
const MIGRATION_V2: i64 = 2;
const MIGRATION_V2_CHECKSUM: &str = "profile-store-v2-2026-07-13";
const MIGRATION_V3: i64 = 3;
const MIGRATION_V3_CHECKSUM: &str = "run-archive-v3-2026-07-14";
const MIGRATION_V4: i64 = 4;
const MIGRATION_V4_CHECKSUM: &str = "approval-chains-v4-2026-07-16";
const MIGRATION_V5: i64 = 5;
const MIGRATION_V5_CHECKSUM: &str = "agent-process-table-v5-2026-08-02";

#[derive(Debug)]
pub enum LedgerError {
    Sqlite(rusqlite::Error),
    Serialization(serde_json::Error),
    Protocol(String),
    NotFound {
        entity: &'static str,
        id: String,
    },
    IdempotencyConflict {
        key: String,
        existing_run_id: String,
        requested_run_id: String,
    },
    RunIdConflict {
        run_id: String,
    },
    DuplicateEvent {
        event_id: String,
    },
    SequenceMismatch {
        run_id: String,
        expected: u64,
        actual: u64,
    },
    TerminalRun {
        run_id: String,
        terminal_sequence: u64,
    },
    ApprovalDigestMismatch {
        request_id: String,
    },
    ApprovalExpiryMismatch {
        request_id: String,
    },
    ApprovalDecisionTiming {
        request_id: String,
        message: &'static str,
    },
    ApprovalAlreadyDecided {
        request_id: String,
    },
    InvalidTransition(String),
    Corrupt(String),
    NumericOverflow(&'static str),
    MigrationConflict {
        version: i64,
    },
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(f, "SQLite error: {error}"),
            Self::Serialization(error) => write!(f, "ledger serialization error: {error}"),
            Self::Protocol(error) => write!(f, "invalid run protocol value: {error}"),
            Self::NotFound { entity, id } => write!(f, "{entity} '{id}' was not found"),
            Self::IdempotencyConflict {
                key,
                existing_run_id,
                requested_run_id,
            } => write!(
                f,
                "idempotency key '{key}' belongs to run '{existing_run_id}', but the submitted spec for run '{requested_run_id}' differs"
            ),
            Self::RunIdConflict { run_id } => {
                write!(f, "run id '{run_id}' already exists with a different spec")
            }
            Self::DuplicateEvent { event_id } => {
                write!(f, "event id '{event_id}' already exists")
            }
            Self::SequenceMismatch {
                run_id,
                expected,
                actual,
            } => write!(
                f,
                "run '{run_id}' expected event sequence {expected}, received {actual}"
            ),
            Self::TerminalRun {
                run_id,
                terminal_sequence,
            } => write!(
                f,
                "run '{run_id}' already terminated at sequence {terminal_sequence}"
            ),
            Self::ApprovalDigestMismatch { request_id } => write!(
                f,
                "approval '{request_id}' does not match the requested operation digest"
            ),
            Self::ApprovalExpiryMismatch { request_id } => write!(
                f,
                "approval '{request_id}' does not match the requested expiry"
            ),
            Self::ApprovalDecisionTiming {
                request_id,
                message,
            } => write!(f, "approval '{request_id}' has invalid timing: {message}"),
            Self::ApprovalAlreadyDecided { request_id } => {
                write!(f, "approval '{request_id}' already has a decision")
            }
            Self::InvalidTransition(message) => f.write_str(message),
            Self::Corrupt(message) => write!(f, "ledger is corrupt: {message}"),
            Self::NumericOverflow(field) => {
                write!(f, "{field} exceeds SQLite's signed integer range")
            }
            Self::MigrationConflict { version } => write!(
                f,
                "schema migration {version} has an unexpected checksum or is newer than this binary"
            ),
        }
    }
}

impl Error for LedgerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Serialization(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for LedgerError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<serde_json::Error> for LedgerError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serialization(value)
    }
}

pub type LedgerResult<T> = Result<T, LedgerError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRun {
    pub spec: RunSpec,
    pub status: RunStatus,
    pub last_sequence: u64,
    pub terminal_sequence: Option<u64>,
    pub updated_at_ms: u64,
    pub archived_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitRunOutcome {
    pub run: StoredRun,
    pub inserted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendEventOutcome {
    pub run_id: String,
    pub sequence: u64,
    pub status: RunStatus,
    pub terminal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredApproval {
    pub run_id: String,
    pub request_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub operation_sha256: String,
    pub requested_sequence: u64,
    pub awaiting_sequence: Option<u64>,
    pub expires_at_ms: u64,
    pub decision: Option<PermissionDecision>,
    pub decided_sequence: Option<u64>,
    pub decided_by: Option<ClientIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IntegrityReport {
    pub violations: Vec<String>,
}

impl IntegrityReport {
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.violations.is_empty()
    }
}

pub struct RunLedger {
    connection: Connection,
}

impl RunLedger {
    pub fn open(path: impl AsRef<Path>) -> LedgerResult<Self> {
        let mut connection = Connection::open(path)?;
        configure_connection(&connection)?;
        apply_migrations(&mut connection)?;
        Ok(Self { connection })
    }

    pub fn open_in_memory() -> LedgerResult<Self> {
        let mut connection = Connection::open_in_memory()?;
        configure_connection(&connection)?;
        apply_migrations(&mut connection)?;
        Ok(Self { connection })
    }

    /// Narrow crate-internal escape hatch for transactional companion stores
    /// that share this database. Keeping the connection private outside the
    /// crate prevents execution surfaces from bypassing ledger invariants.
    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Mutable counterpart used by companion stores to open one SQLite
    /// transaction spanning all of their normalized rows.
    pub(crate) fn connection_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }

    /// A typed view of the unified agent process table on this connection.
    ///
    /// Public where [`Self::connection`] is not: `monkey processes` and the
    /// daemon are separate binaries that need the process table, and handing
    /// them a `ProcessTable` keeps the raw connection — and every invariant it
    /// could bypass — crate-private.
    pub fn process_table(&self) -> crate::process_table::ProcessTable<'_> {
        crate::process_table::ProcessTable::new(&self.connection)
    }

    /// Submit an immutable run spec. Reusing an idempotency key succeeds only
    /// when the serialized spec bytes are identical to the stored submission.
    pub fn submit_run(&mut self, spec: &RunSpec) -> LedgerResult<SubmitRunOutcome> {
        spec.validate()
            .map_err(|error| LedgerError::Protocol(error.to_string()))?;
        let spec_json = serde_json::to_vec(spec)?;
        let created_at_ms = to_sql_i64(spec.created_at_ms, "created_at_ms")?;
        let max_event_count = to_sql_i64(spec.budgets.max_event_count, "max_event_count")?;

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some((existing_run_id, existing_spec)) = transaction
            .query_row(
                "SELECT run_id, spec_json FROM runs WHERE idempotency_key = ?1",
                [&spec.idempotency_key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?
        {
            if existing_spec != spec_json {
                return Err(LedgerError::IdempotencyConflict {
                    key: spec.idempotency_key.clone(),
                    existing_run_id,
                    requested_run_id: spec.run_id.clone(),
                });
            }
            let run = load_run_from(&transaction, &spec.run_id)?.ok_or_else(|| {
                LedgerError::Corrupt(format!(
                    "idempotency key '{}' points to missing run '{}'",
                    spec.idempotency_key, spec.run_id
                ))
            })?;
            transaction.commit()?;
            return Ok(SubmitRunOutcome {
                run,
                inserted: false,
            });
        }

        if transaction
            .query_row(
                "SELECT 1 FROM runs WHERE run_id = ?1",
                [&spec.run_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            return Err(LedgerError::RunIdConflict {
                run_id: spec.run_id.clone(),
            });
        }

        transaction.execute(
            "INSERT INTO runs (
                run_id, idempotency_key, spec_json, created_at_ms, updated_at_ms,
                status, last_sequence, terminal_sequence, max_event_count
             ) VALUES (?1, ?2, ?3, ?4, ?4, 'queued', 0, NULL, ?5)",
            params![
                spec.run_id,
                spec.idempotency_key,
                spec_json,
                created_at_ms,
                max_event_count
            ],
        )?;

        let run = load_run_from(&transaction, &spec.run_id)?.ok_or_else(|| {
            LedgerError::Corrupt(format!("newly inserted run '{}' disappeared", spec.run_id))
        })?;
        transaction.commit()?;
        Ok(SubmitRunOutcome {
            run,
            inserted: true,
        })
    }

    /// Append exactly one event and update every derived projection in the
    /// same transaction. Sequence zero, gaps, duplicates, and post-terminal
    /// events are rejected before any projection can become visible.
    pub fn append_event(
        &mut self,
        envelope: &RunEventEnvelope,
    ) -> LedgerResult<AppendEventOutcome> {
        envelope
            .validate()
            .map_err(|error| LedgerError::Protocol(error.to_string()))?;
        let sequence = to_sql_i64(envelope.sequence, "sequence")?;
        let occurred_at_ms = to_sql_i64(envelope.occurred_at_ms, "occurred_at_ms")?;
        let envelope_json = serde_json::to_vec(envelope)?;
        let emitter_json = serde_json::to_vec(&envelope.emitter)?;
        let effects = derive_event_effects(&envelope.event);

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = transaction
            .query_row(
                "SELECT status, last_sequence, terminal_sequence, max_event_count
                 FROM runs WHERE run_id = ?1",
                [&envelope.run_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| LedgerError::NotFound {
                entity: "run",
                id: envelope.run_id.clone(),
            })?;

        let current_status = parse_run_status(&state.0)?;
        let last_sequence = from_sql_u64(state.1, "last_sequence")?;
        if let Some(terminal_sequence) = state.2 {
            return Err(LedgerError::TerminalRun {
                run_id: envelope.run_id.clone(),
                terminal_sequence: from_sql_u64(terminal_sequence, "terminal_sequence")?,
            });
        }

        let expected = last_sequence
            .checked_add(1)
            .ok_or(LedgerError::NumericOverflow("sequence"))?;
        if envelope.sequence != expected {
            return Err(LedgerError::SequenceMismatch {
                run_id: envelope.run_id.clone(),
                expected,
                actual: envelope.sequence,
            });
        }
        if sequence > state.3 {
            return Err(LedgerError::InvalidTransition(format!(
                "run '{}' exceeded its max_event_count of {}",
                envelope.run_id, state.3
            )));
        }
        if transaction
            .query_row(
                "SELECT 1 FROM run_events WHERE event_id = ?1",
                [&envelope.event_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            return Err(LedgerError::DuplicateEvent {
                event_id: envelope.event_id.clone(),
            });
        }

        validate_status_transition(current_status, effects.status)?;

        let derived_status = effects.status.map(run_status_token);
        transaction.execute(
            "INSERT INTO run_events (
                event_id, run_id, sequence, occurred_at_ms, actor_id,
                emitter_json, event_type, envelope_json, derived_status, is_terminal
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                envelope.event_id,
                envelope.run_id,
                sequence,
                occurred_at_ms,
                envelope.actor_id,
                emitter_json,
                effects.event_type,
                envelope_json,
                derived_status,
                i64::from(effects.terminal)
            ],
        )?;

        apply_projection(&transaction, envelope, &effects.projection)?;
        let resulting_status = effects.status.unwrap_or(current_status);
        transaction.commit()?;

        Ok(AppendEventOutcome {
            run_id: envelope.run_id.clone(),
            sequence: envelope.sequence,
            status: resulting_status,
            terminal: effects.terminal,
        })
    }

    pub fn load_run(&self, run_id: &str) -> LedgerResult<Option<StoredRun>> {
        load_run_from(&self.connection, run_id)
    }

    pub fn load_run_by_idempotency_key(
        &self,
        idempotency_key: &str,
    ) -> LedgerResult<Option<StoredRun>> {
        let run_id = self
            .connection
            .query_row(
                "SELECT run_id FROM runs WHERE idempotency_key = ?1",
                [idempotency_key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        run_id
            .map(|run_id| load_run_from(&self.connection, &run_id))
            .transpose()
            .map(Option::flatten)
    }

    pub fn list_runs(&self, limit: usize, include_archived: bool) -> LedgerResult<Vec<StoredRun>> {
        let limit = bounded_limit(limit)?;
        let sql = if include_archived {
            "SELECT run_id FROM runs ORDER BY created_at_ms DESC, run_id DESC LIMIT ?1"
        } else {
            "SELECT run_id FROM runs WHERE archived_at_ms IS NULL
             ORDER BY created_at_ms DESC, run_id DESC LIMIT ?1"
        };
        let mut statement = self.connection.prepare(sql)?;
        let ids = statement
            .query_map([limit], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|run_id| {
                load_run_from(&self.connection, &run_id)?.ok_or_else(|| {
                    LedgerError::Corrupt(format!("listed run '{run_id}' disappeared"))
                })
            })
            .collect()
    }

    /// Hides a terminal run from the default `list_runs` result without
    /// touching its event history — the ledger's append-only guarantee stays
    /// intact (see `MIGRATION_V3_SQL`'s doc comment for why hard-delete isn't
    /// an option). Archiving an active run makes no sense (there'd be
    /// nothing stopping it from producing more events while hidden), so it's
    /// rejected the same way other illegal state transitions are.
    pub fn archive_run(&mut self, run_id: &str, archived_at_ms: u64) -> LedgerResult<StoredRun> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = load_run_from(&transaction, run_id)?.ok_or_else(|| LedgerError::NotFound {
            entity: "run",
            id: run_id.to_string(),
        })?;
        if !run.status.is_terminal() {
            return Err(LedgerError::InvalidTransition(format!(
                "run '{run_id}' cannot be archived while status is '{}'",
                run_status_token(run.status)
            )));
        }
        transaction.execute(
            "UPDATE runs SET archived_at_ms = ?2 WHERE run_id = ?1",
            params![run_id, to_sql_i64(archived_at_ms, "archived_at_ms")?],
        )?;
        let archived = load_run_from(&transaction, run_id)?.ok_or_else(|| {
            LedgerError::Corrupt(format!("archived run '{run_id}' disappeared"))
        })?;
        transaction.commit()?;
        Ok(archived)
    }

    /// Reverses `archive_run`. Always legal — an archived run's status never
    /// changes, so there's no transition to validate.
    pub fn unarchive_run(&mut self, run_id: &str) -> LedgerResult<StoredRun> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if load_run_from(&transaction, run_id)?.is_none() {
            return Err(LedgerError::NotFound {
                entity: "run",
                id: run_id.to_string(),
            });
        }
        transaction.execute(
            "UPDATE runs SET archived_at_ms = NULL WHERE run_id = ?1",
            [run_id],
        )?;
        let run = load_run_from(&transaction, run_id)?.ok_or_else(|| {
            LedgerError::Corrupt(format!("unarchived run '{run_id}' disappeared"))
        })?;
        transaction.commit()?;
        Ok(run)
    }

    pub fn load_events(
        &self,
        run_id: &str,
        after_sequence: u64,
        limit: usize,
    ) -> LedgerResult<Vec<RunEventEnvelope>> {
        let after_sequence = to_sql_i64(after_sequence, "after_sequence")?;
        let limit = bounded_limit(limit)?;
        let mut statement = self.connection.prepare(
            "SELECT sequence, envelope_json FROM run_events
             WHERE run_id = ?1 AND sequence > ?2
             ORDER BY sequence ASC LIMIT ?3",
        )?;
        let rows = statement
            .query_map(params![run_id, after_sequence, limit], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(|(stored_sequence, bytes)| {
                let envelope: RunEventEnvelope = serde_json::from_slice(&bytes)?;
                envelope.validate().map_err(|error| {
                    LedgerError::Corrupt(format!(
                        "stored event '{}' fails protocol validation: {error}",
                        envelope.event_id
                    ))
                })?;
                if envelope.run_id != run_id
                    || to_sql_i64(envelope.sequence, "sequence")? != stored_sequence
                {
                    return Err(LedgerError::Corrupt(format!(
                        "event '{}' metadata does not match its row",
                        envelope.event_id
                    )));
                }
                Ok(envelope)
            })
            .collect()
    }

    pub fn load_approval(
        &self,
        run_id: &str,
        request_id: &str,
    ) -> LedgerResult<Option<StoredApproval>> {
        self.connection
            .query_row(
                "SELECT tool_call_id, tool_name, operation_sha256,
                        requested_sequence, awaiting_sequence, expires_at_ms,
                        decision, decided_sequence, decided_by_json
                 FROM approvals WHERE run_id = ?1 AND request_id = ?2",
                params![run_id, request_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, Option<i64>>(7)?,
                        row.get::<_, Option<Vec<u8>>>(8)?,
                    ))
                },
            )
            .optional()?
            .map(|row| {
                let decision = row
                    .6
                    .as_deref()
                    .map(parse_permission_decision)
                    .transpose()?;
                let decided_by = row
                    .8
                    .map(|bytes| serde_json::from_slice::<ClientIdentity>(&bytes))
                    .transpose()?;
                Ok(StoredApproval {
                    run_id: run_id.to_string(),
                    request_id: request_id.to_string(),
                    tool_call_id: row.0,
                    tool_name: row.1,
                    operation_sha256: row.2,
                    requested_sequence: from_sql_u64(row.3, "requested_sequence")?,
                    awaiting_sequence: row
                        .4
                        .map(|value| from_sql_u64(value, "awaiting_sequence"))
                        .transpose()?,
                    expires_at_ms: from_sql_u64(row.5, "expires_at_ms")?,
                    decision,
                    decided_sequence: row
                        .7
                        .map(|value| from_sql_u64(value, "decided_sequence"))
                        .transpose()?,
                    decided_by,
                })
            })
            .transpose()
    }

    pub fn applied_migrations(&self) -> LedgerResult<Vec<i64>> {
        let mut statement = self
            .connection
            .prepare("SELECT version FROM schema_migrations ORDER BY version")?;
        let versions = statement
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(versions)
    }

    pub fn has_fts5(&self) -> LedgerResult<bool> {
        Ok(self.connection.query_row(
            "SELECT enabled FROM ledger_capabilities WHERE name = 'fts5'",
            [],
            |row| row.get::<_, i64>(0),
        )? == 1)
    }

    pub fn integrity_check(&self) -> LedgerResult<IntegrityReport> {
        let mut report = IntegrityReport::default();

        let mut statement = self.connection.prepare("PRAGMA integrity_check")?;
        for result in statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
        {
            if result != "ok" {
                report.violations.push(format!("SQLite: {result}"));
            }
        }

        let mut foreign_keys = self.connection.prepare("PRAGMA foreign_key_check")?;
        for violation in foreign_keys
            .query_map([], |row| {
                Ok(format!(
                    "foreign key: table={}, rowid={:?}, parent={}, fk={}",
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        {
            report.violations.push(violation);
        }

        collect_named_violations(
            &self.connection,
            "SELECT run_id FROM runs r
             WHERE last_sequence != COALESCE(
                (SELECT MAX(sequence) FROM run_events e WHERE e.run_id = r.run_id), 0
             )",
            "last_sequence mismatch",
            &mut report,
        )?;
        collect_named_violations(
            &self.connection,
            "SELECT run_id FROM run_events GROUP BY run_id
             HAVING MIN(sequence) != 1 OR COUNT(*) != MAX(sequence)",
            "event sequence gap",
            &mut report,
        )?;
        collect_named_violations(
            &self.connection,
            "SELECT run_id FROM run_events GROUP BY run_id
             HAVING SUM(is_terminal) > 1 OR
                    MAX(sequence) > COALESCE(MIN(CASE WHEN is_terminal = 1 THEN sequence END), MAX(sequence))",
            "terminal event invariant",
            &mut report,
        )?;
        collect_named_violations(
            &self.connection,
            "SELECT run_id FROM runs
             WHERE (terminal_sequence IS NULL AND status IN
                    ('succeeded','failed','cancelled','needs_reconciliation'))
                OR (terminal_sequence IS NOT NULL AND status NOT IN
                    ('succeeded','failed','cancelled','needs_reconciliation'))",
            "terminal status mismatch",
            &mut report,
        )?;

        Ok(report)
    }
}

fn configure_connection(connection: &Connection) -> LedgerResult<()> {
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_SQLITE_VALUE_BYTES)?;
    connection.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, MAX_SQL_TEXT_BYTES)?;
    connection.set_limit(Limit::SQLITE_LIMIT_COLUMN, 256)?;
    connection.set_limit(Limit::SQLITE_LIMIT_EXPR_DEPTH, 100)?;
    connection.set_limit(Limit::SQLITE_LIMIT_COMPOUND_SELECT, 32)?;
    connection.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    connection.set_limit(Limit::SQLITE_LIMIT_LIKE_PATTERN_LENGTH, 4_096)?;
    connection.set_limit(Limit::SQLITE_LIMIT_VARIABLE_NUMBER, 512)?;
    connection.set_limit(Limit::SQLITE_LIMIT_TRIGGER_DEPTH, 32)?;
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;
         PRAGMA foreign_keys = ON;
         PRAGMA trusted_schema = OFF;
         PRAGMA recursive_triggers = ON;
         PRAGMA wal_autocheckpoint = 1000;
         PRAGMA journal_size_limit = 67108864;",
    )?;
    Ok(())
}

fn apply_migrations(connection: &mut Connection) -> LedgerResult<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            checksum TEXT NOT NULL,
            applied_at_ms INTEGER NOT NULL CHECK (applied_at_ms > 0)
         ) STRICT;",
    )?;

    if let Some(version) =
        connection.query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get::<_, Option<i64>>(0)
        })?
    {
        if version > MIGRATION_V5 {
            return Err(LedgerError::MigrationConflict { version });
        }
    }

    for (version, expected) in [
        (MIGRATION_V1, MIGRATION_V1_CHECKSUM),
        (MIGRATION_V2, MIGRATION_V2_CHECKSUM),
        (MIGRATION_V3, MIGRATION_V3_CHECKSUM),
        (MIGRATION_V4, MIGRATION_V4_CHECKSUM),
        (MIGRATION_V5, MIGRATION_V5_CHECKSUM),
    ] {
        if let Some(checksum) = connection
            .query_row(
                "SELECT checksum FROM schema_migrations WHERE version = ?1",
                [version],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            if checksum != expected {
                return Err(LedgerError::MigrationConflict { version });
            }
        }
    }

    let has_v1_before = connection
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V1],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    let has_v2_before = connection
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V2],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if has_v2_before && !has_v1_before {
        return Err(LedgerError::MigrationConflict {
            version: MIGRATION_V2,
        });
    }
    let has_v3_before = connection
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V3],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if has_v3_before && !has_v2_before {
        return Err(LedgerError::MigrationConflict {
            version: MIGRATION_V3,
        });
    }
    let has_v4_before = connection
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V4],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if has_v4_before && !has_v3_before {
        return Err(LedgerError::MigrationConflict {
            version: MIGRATION_V4,
        });
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let fts5 = transaction.query_row(
        "SELECT sqlite_compileoption_used('ENABLE_FTS5')",
        [],
        |row| row.get::<_, i64>(0),
    )? == 1;

    let has_v1 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V1],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v1 {
        transaction.execute_batch(MIGRATION_V1_SQL)?;
        if fts5 {
            transaction.execute_batch(MIGRATION_V1_FTS5_SQL)?;
        }
        transaction.execute(
            "INSERT INTO ledger_capabilities (name, enabled) VALUES ('fts5', ?1)",
            [i64::from(fts5)],
        )?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms)
             VALUES (?1, ?2, ?3)",
            params![MIGRATION_V1, MIGRATION_V1_CHECKSUM, now_ms_i64()?],
        )?;
    }

    let has_v2 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V2],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v2 {
        transaction.execute_batch(MIGRATION_V2_SQL)?;
        if fts5 {
            transaction.execute_batch(MIGRATION_V2_FTS5_SQL)?;
        }
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms)
             VALUES (?1, ?2, ?3)",
            params![MIGRATION_V2, MIGRATION_V2_CHECKSUM, now_ms_i64()?],
        )?;
    }

    let has_v3 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V3],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v3 {
        transaction.execute_batch(MIGRATION_V3_SQL)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms)
             VALUES (?1, ?2, ?3)",
            params![MIGRATION_V3, MIGRATION_V3_CHECKSUM, now_ms_i64()?],
        )?;
    }

    let has_v4 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V4],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v4 {
        transaction.execute_batch(MIGRATION_V4_SQL)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms)
             VALUES (?1, ?2, ?3)",
            params![MIGRATION_V4, MIGRATION_V4_CHECKSUM, now_ms_i64()?],
        )?;
    }

    let has_v5 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V5],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v5 {
        transaction.execute_batch(MIGRATION_V5_SQL)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms)
             VALUES (?1, ?2, ?3)",
            params![MIGRATION_V5, MIGRATION_V5_CHECKSUM, now_ms_i64()?],
        )?;
    }

    transaction.execute_batch("PRAGMA user_version = 5;")?;
    transaction.commit()?;
    Ok(())
}

const MIGRATION_V1_SQL: &str = r#"
CREATE TABLE ledger_capabilities (
    name TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1))
) STRICT;

CREATE TABLE runs (
    run_id TEXT PRIMARY KEY,
    idempotency_key TEXT NOT NULL UNIQUE,
    spec_json BLOB NOT NULL,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0),
    status TEXT NOT NULL CHECK (status IN (
        'queued', 'running', 'waiting_for_permission', 'paused', 'cancelling',
        'succeeded', 'failed', 'cancelled', 'needs_reconciliation'
    )),
    last_sequence INTEGER NOT NULL DEFAULT 0 CHECK (last_sequence >= 0),
    terminal_sequence INTEGER,
    max_event_count INTEGER NOT NULL CHECK (max_event_count > 0),
    CHECK (terminal_sequence IS NULL OR terminal_sequence = last_sequence),
    CHECK (
        (terminal_sequence IS NULL AND status NOT IN
            ('succeeded', 'failed', 'cancelled', 'needs_reconciliation'))
        OR
        (terminal_sequence IS NOT NULL AND status IN
            ('succeeded', 'failed', 'cancelled', 'needs_reconciliation'))
    )
) STRICT;

CREATE INDEX runs_created_idx ON runs(created_at_ms DESC, run_id DESC);
CREATE INDEX runs_status_idx ON runs(status, updated_at_ms DESC);

CREATE TABLE run_events (
    event_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(run_id) ON DELETE RESTRICT,
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    occurred_at_ms INTEGER NOT NULL CHECK (occurred_at_ms > 0),
    actor_id TEXT,
    emitter_json BLOB NOT NULL,
    event_type TEXT NOT NULL,
    envelope_json BLOB NOT NULL,
    derived_status TEXT CHECK (derived_status IS NULL OR derived_status IN (
        'queued', 'running', 'waiting_for_permission', 'paused', 'cancelling',
        'succeeded', 'failed', 'cancelled', 'needs_reconciliation'
    )),
    is_terminal INTEGER NOT NULL CHECK (is_terminal IN (0, 1)),
    UNIQUE(run_id, sequence),
    CHECK (
        (is_terminal = 1 AND derived_status IN
            ('succeeded', 'failed', 'cancelled', 'needs_reconciliation'))
        OR
        (is_terminal = 0 AND (derived_status IS NULL OR derived_status IN
            ('queued', 'running', 'waiting_for_permission', 'paused', 'cancelling')))
    )
) STRICT;

CREATE INDEX run_events_run_time_idx
    ON run_events(run_id, occurred_at_ms, sequence);
CREATE INDEX run_events_actor_idx
    ON run_events(actor_id, occurred_at_ms) WHERE actor_id IS NOT NULL;

CREATE TRIGGER run_events_validate_insert
BEFORE INSERT ON run_events
BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM runs WHERE run_id = NEW.run_id
    ) THEN RAISE(ABORT, 'run not found') END;
    SELECT CASE WHEN (
        SELECT terminal_sequence FROM runs WHERE run_id = NEW.run_id
    ) IS NOT NULL THEN RAISE(ABORT, 'events after terminal are forbidden') END;
    SELECT CASE WHEN NEW.sequence != (
        SELECT last_sequence + 1 FROM runs WHERE run_id = NEW.run_id
    ) THEN RAISE(ABORT, 'run event sequence gap') END;
    SELECT CASE WHEN NEW.sequence > (
        SELECT max_event_count FROM runs WHERE run_id = NEW.run_id
    ) THEN RAISE(ABORT, 'run event budget exceeded') END;
END;

CREATE TRIGGER run_events_project_run
AFTER INSERT ON run_events
BEGIN
    UPDATE runs
       SET last_sequence = NEW.sequence,
           terminal_sequence = CASE
               WHEN NEW.is_terminal = 1 THEN NEW.sequence
               ELSE terminal_sequence
           END,
           status = COALESCE(NEW.derived_status, status),
           updated_at_ms = MAX(updated_at_ms, NEW.occurred_at_ms)
     WHERE run_id = NEW.run_id;
END;

CREATE TRIGGER run_events_forbid_update
BEFORE UPDATE ON run_events
BEGIN
    SELECT RAISE(ABORT, 'run events are append-only');
END;

CREATE TRIGGER run_events_forbid_delete
BEFORE DELETE ON run_events
BEGIN
    SELECT RAISE(ABORT, 'run events are append-only');
END;

CREATE TABLE approvals (
    run_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    tool_call_id TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    operation_sha256 TEXT NOT NULL CHECK (length(operation_sha256) = 64),
    requested_sequence INTEGER NOT NULL,
    awaiting_sequence INTEGER,
    expires_at_ms INTEGER NOT NULL CHECK (expires_at_ms > 0),
    detail TEXT NOT NULL,
    risk_level TEXT,
    decision TEXT CHECK (decision IS NULL OR decision IN
        ('allow_once', 'allow_for_run', 'deny', 'expired')),
    decided_sequence INTEGER,
    decided_by_json BLOB,
    PRIMARY KEY(run_id, request_id),
    FOREIGN KEY(run_id, requested_sequence)
        REFERENCES run_events(run_id, sequence) ON DELETE RESTRICT,
    FOREIGN KEY(run_id, awaiting_sequence)
        REFERENCES run_events(run_id, sequence) ON DELETE RESTRICT,
    FOREIGN KEY(run_id, decided_sequence)
        REFERENCES run_events(run_id, sequence) ON DELETE RESTRICT,
    CHECK ((decision IS NULL AND decided_sequence IS NULL AND decided_by_json IS NULL)
        OR (decision IS NOT NULL AND decided_sequence IS NOT NULL AND decided_by_json IS NOT NULL))
) STRICT;

CREATE INDEX approvals_pending_idx
    ON approvals(expires_at_ms, run_id) WHERE decision IS NULL;

CREATE TABLE artifacts (
    artifact_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    event_sequence INTEGER NOT NULL,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    media_type TEXT NOT NULL,
    content_sha256 TEXT NOT NULL CHECK (length(content_sha256) = 64),
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    storage_path TEXT,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    FOREIGN KEY(run_id, event_sequence)
        REFERENCES run_events(run_id, sequence) ON DELETE RESTRICT
) STRICT;

CREATE INDEX artifacts_run_idx ON artifacts(run_id, event_sequence);
CREATE INDEX artifacts_content_idx ON artifacts(content_sha256);

CREATE TABLE checkpoints (
    checkpoint_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    event_sequence INTEGER NOT NULL,
    kind TEXT NOT NULL,
    label TEXT NOT NULL,
    content_sha256 TEXT CHECK (content_sha256 IS NULL OR length(content_sha256) = 64),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    FOREIGN KEY(run_id, event_sequence)
        REFERENCES run_events(run_id, sequence) ON DELETE RESTRICT
) STRICT;

CREATE INDEX checkpoints_run_idx ON checkpoints(run_id, event_sequence);

CREATE TABLE external_mutations (
    run_id TEXT NOT NULL,
    mutation_id TEXT NOT NULL,
    tool_call_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'confirmed', 'needs_reconciliation')),
    idempotency_key TEXT,
    summary TEXT NOT NULL,
    prepared_sequence INTEGER NOT NULL,
    confirmed_sequence INTEGER,
    confirmation_ref TEXT,
    reconciliation_reason TEXT,
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0),
    PRIMARY KEY(run_id, mutation_id),
    FOREIGN KEY(run_id, prepared_sequence)
        REFERENCES run_events(run_id, sequence) ON DELETE RESTRICT,
    FOREIGN KEY(run_id, confirmed_sequence)
        REFERENCES run_events(run_id, sequence) ON DELETE RESTRICT,
    CHECK ((state = 'pending' AND confirmed_sequence IS NULL)
        OR (state != 'pending'))
) STRICT;

CREATE UNIQUE INDEX external_mutations_idempotency_idx
    ON external_mutations(run_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

CREATE TABLE run_leases (
    run_id TEXT PRIMARY KEY REFERENCES runs(run_id) ON DELETE RESTRICT,
    owner_id TEXT NOT NULL,
    lease_token_sha256 TEXT NOT NULL CHECK (length(lease_token_sha256) = 64),
    generation INTEGER NOT NULL CHECK (generation > 0),
    acquired_at_ms INTEGER NOT NULL CHECK (acquired_at_ms > 0),
    heartbeat_at_ms INTEGER NOT NULL CHECK (heartbeat_at_ms > 0),
    expires_at_ms INTEGER NOT NULL CHECK (expires_at_ms > heartbeat_at_ms)
) STRICT;

CREATE INDEX run_leases_expiry_idx ON run_leases(expires_at_ms);

CREATE TABLE worktree_leases (
    lease_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(run_id) ON DELETE RESTRICT,
    repository_id TEXT NOT NULL,
    common_git_dir TEXT NOT NULL,
    canonical_path TEXT NOT NULL UNIQUE,
    branch TEXT NOT NULL,
    base_oid TEXT NOT NULL,
    expected_head TEXT,
    state TEXT NOT NULL CHECK (state IN
        ('creating', 'active', 'archived', 'cleanup_pending', 'released', 'needs_reconciliation')),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    heartbeat_at_ms INTEGER NOT NULL CHECK (heartbeat_at_ms > 0),
    released_at_ms INTEGER
) STRICT;

CREATE INDEX worktree_leases_run_idx ON worktree_leases(run_id, state);
CREATE INDEX worktree_leases_repo_idx ON worktree_leases(repository_id, state);

CREATE TABLE triggers (
    trigger_id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    config_json BLOB NOT NULL,
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0),
    next_fire_at_ms INTEGER,
    last_delivery_at_ms INTEGER
) STRICT;

CREATE TABLE trigger_deliveries (
    trigger_id TEXT NOT NULL REFERENCES triggers(trigger_id) ON DELETE RESTRICT,
    delivery_id TEXT NOT NULL,
    payload_sha256 TEXT NOT NULL CHECK (length(payload_sha256) = 64),
    received_at_ms INTEGER NOT NULL CHECK (received_at_ms > 0),
    status TEXT NOT NULL CHECK (status IN
        ('received', 'accepted', 'duplicate', 'rejected', 'submitted')),
    run_id TEXT REFERENCES runs(run_id) ON DELETE SET NULL,
    PRIMARY KEY(trigger_id, delivery_id)
) STRICT;

CREATE TABLE paired_clients (
    client_id TEXT PRIMARY KEY,
    public_key BLOB NOT NULL,
    key_generation INTEGER NOT NULL CHECK (key_generation > 0),
    capabilities_json BLOB NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active', 'rotated', 'revoked')),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    last_seen_at_ms INTEGER,
    revoked_at_ms INTEGER
) STRICT;

CREATE TABLE session_groups (
    group_id TEXT PRIMARY KEY,
    parent_group_id TEXT REFERENCES session_groups(group_id) ON DELETE RESTRICT,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    name TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0)
) STRICT;

CREATE UNIQUE INDEX session_groups_ordinal_idx
    ON session_groups(COALESCE(parent_group_id, ''), ordinal);

CREATE TABLE sessions (
    session_id TEXT PRIMARY KEY,
    group_id TEXT REFERENCES session_groups(group_id) ON DELETE SET NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    title TEXT NOT NULL,
    active_run_id TEXT REFERENCES runs(run_id) ON DELETE SET NULL,
    pinned INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
    archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0)
) STRICT;

CREATE UNIQUE INDEX sessions_ordinal_idx
    ON sessions(COALESCE(group_id, ''), ordinal);

CREATE TABLE messages (
    message_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    run_id TEXT REFERENCES runs(run_id) ON DELETE SET NULL,
    actor_id TEXT,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    metadata_json BLOB,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0),
    UNIQUE(session_id, ordinal)
) STRICT;

CREATE INDEX messages_run_idx ON messages(run_id, ordinal) WHERE run_id IS NOT NULL;
CREATE INDEX messages_actor_idx ON messages(actor_id, created_at_ms) WHERE actor_id IS NOT NULL;

CREATE TABLE message_translations (
    message_id TEXT NOT NULL REFERENCES messages(message_id) ON DELETE RESTRICT,
    locale TEXT NOT NULL,
    content TEXT NOT NULL,
    source_sha256 TEXT NOT NULL CHECK (length(source_sha256) = 64),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0),
    PRIMARY KEY(message_id, locale)
) STRICT;

CREATE TABLE attachments (
    attachment_id TEXT PRIMARY KEY,
    content_sha256 TEXT NOT NULL CHECK (length(content_sha256) = 64),
    kind TEXT NOT NULL,
    media_type TEXT NOT NULL,
    byte_size INTEGER NOT NULL CHECK (byte_size >= 0),
    storage_path TEXT NOT NULL,
    metadata_json BLOB,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0)
) STRICT;

CREATE INDEX attachments_content_idx ON attachments(content_sha256);

CREATE TABLE message_attachments (
    message_id TEXT NOT NULL REFERENCES messages(message_id) ON DELETE RESTRICT,
    attachment_id TEXT NOT NULL REFERENCES attachments(attachment_id) ON DELETE RESTRICT,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    purpose TEXT,
    PRIMARY KEY(message_id, attachment_id),
    UNIQUE(message_id, ordinal)
) STRICT;

CREATE TABLE actor_transcripts (
    transcript_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    actor_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    run_id TEXT REFERENCES runs(run_id) ON DELETE SET NULL,
    message_id TEXT REFERENCES messages(message_id) ON DELETE SET NULL,
    content TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    UNIQUE(session_id, actor_id, ordinal)
) STRICT;
"#;

const MIGRATION_V1_FTS5_SQL: &str = r#"
CREATE VIRTUAL TABLE messages_fts USING fts5(
    content,
    role,
    session_id UNINDEXED,
    message_id UNINDEXED,
    content='messages',
    content_rowid='rowid'
);

CREATE TRIGGER messages_fts_insert AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, content, role, session_id, message_id)
    VALUES (new.rowid, new.content, new.role, new.session_id, new.message_id);
END;

CREATE TRIGGER messages_fts_delete AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content, role, session_id, message_id)
    VALUES ('delete', old.rowid, old.content, old.role, old.session_id, old.message_id);
END;

CREATE TRIGGER messages_fts_update AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content, role, session_id, message_id)
    VALUES ('delete', old.rowid, old.content, old.role, old.session_id, old.message_id);
    INSERT INTO messages_fts(rowid, content, role, session_id, message_id)
    VALUES (new.rowid, new.content, new.role, new.session_id, new.message_id);
END;
"#;

// Profile/search additions intentionally live in their own migration. The v1
// checksum is a durable compatibility promise and must never be changed after
// databases have shipped with it.
const MIGRATION_V2_SQL: &str = r#"
ALTER TABLE session_groups ADD COLUMN kind TEXT NOT NULL DEFAULT 'folder'
    CHECK (kind IN ('folder', 'comparison'));
ALTER TABLE session_groups ADD COLUMN metadata_json BLOB;

ALTER TABLE sessions ADD COLUMN unread INTEGER NOT NULL DEFAULT 0
    CHECK (unread IN (0, 1));
ALTER TABLE sessions ADD COLUMN model_key TEXT;
ALTER TABLE sessions ADD COLUMN persona_id TEXT;
ALTER TABLE sessions ADD COLUMN workspace_path TEXT;
ALTER TABLE sessions ADD COLUMN metadata_json BLOB;

ALTER TABLE actor_transcripts ADD COLUMN kind TEXT NOT NULL DEFAULT 'model'
    CHECK (kind IN ('model', 'tool_request', 'tool_result', 'notice', 'subagent'));
ALTER TABLE actor_transcripts ADD COLUMN model_key TEXT;
ALTER TABLE actor_transcripts ADD COLUMN persona_id TEXT;
ALTER TABLE actor_transcripts ADD COLUMN workspace_path TEXT;
ALTER TABLE actor_transcripts ADD COLUMN metadata_json BLOB;

CREATE TABLE profile_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    source_path TEXT,
    source_sha256 TEXT CHECK (source_sha256 IS NULL OR length(source_sha256) = 64),
    recovery_path TEXT,
    migrated_at_ms INTEGER CHECK (migrated_at_ms IS NULL OR migrated_at_ms > 0),
    payload_sha256 TEXT NOT NULL CHECK (length(payload_sha256) = 64),
    active_session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    root_metadata_json BLOB,
    saved_at_ms INTEGER NOT NULL CHECK (saved_at_ms > 0),
    last_indexed_run_event_rowid INTEGER NOT NULL DEFAULT 0
        CHECK (last_indexed_run_event_rowid >= 0)
) STRICT;

CREATE TABLE profile_crews (
    crew_id TEXT PRIMARY KEY,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    name TEXT NOT NULL,
    metadata_json BLOB NOT NULL,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0)
) STRICT;

CREATE UNIQUE INDEX profile_crews_ordinal_idx ON profile_crews(ordinal);

CREATE TABLE profile_run_search_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    last_indexed_run_event_rowid INTEGER NOT NULL DEFAULT 0
        CHECK (last_indexed_run_event_rowid >= 0)
) STRICT;

INSERT INTO profile_run_search_state(singleton, last_indexed_run_event_rowid)
VALUES (1, 0);

-- v1's message_attachments primary key cannot represent the same exact image
-- twice in one message. This occurrence-oriented link preserves every ordinal
-- while attachments/blobs remain content-addressed and deduplicated.
CREATE TABLE profile_message_attachment_links (
    message_id TEXT NOT NULL REFERENCES messages(message_id) ON DELETE RESTRICT,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    attachment_id TEXT NOT NULL REFERENCES attachments(attachment_id) ON DELETE RESTRICT,
    purpose TEXT NOT NULL,
    PRIMARY KEY(message_id, ordinal)
) STRICT;

CREATE INDEX profile_message_attachment_content_idx
    ON profile_message_attachment_links(attachment_id, message_id);

CREATE TABLE profile_search_documents (
    document_id TEXT PRIMARY KEY,
    source_kind TEXT NOT NULL CHECK (source_kind IN
        ('message', 'actor_transcript', 'run_event')),
    source_id TEXT NOT NULL,
    session_id TEXT REFERENCES sessions(session_id) ON DELETE RESTRICT,
    run_id TEXT REFERENCES runs(run_id) ON DELETE RESTRICT,
    title TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    occurred_at_ms INTEGER NOT NULL CHECK (occurred_at_ms > 0),
    model_key TEXT,
    persona_id TEXT,
    workspace_path TEXT,
    archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
    metadata_json BLOB,
    UNIQUE(source_kind, source_id)
) STRICT;

CREATE INDEX profile_search_documents_time_idx
    ON profile_search_documents(occurred_at_ms DESC, document_id);
CREATE INDEX profile_search_documents_session_idx
    ON profile_search_documents(session_id, occurred_at_ms DESC)
    WHERE session_id IS NOT NULL;
CREATE INDEX profile_search_documents_run_idx
    ON profile_search_documents(run_id, occurred_at_ms, document_id)
    WHERE run_id IS NOT NULL;
CREATE INDEX profile_search_documents_filters_idx
    ON profile_search_documents(archived, model_key, persona_id, workspace_path);
"#;

const MIGRATION_V2_FTS5_SQL: &str = r#"
CREATE VIRTUAL TABLE profile_search_fts USING fts5(
    content,
    title,
    role,
    source_kind,
    content='profile_search_documents',
    content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 2'
);

CREATE TRIGGER profile_search_fts_insert
AFTER INSERT ON profile_search_documents BEGIN
    INSERT INTO profile_search_fts(rowid, content, title, role, source_kind)
    VALUES (new.rowid, new.content, new.title, new.role, new.source_kind);
END;

CREATE TRIGGER profile_search_fts_delete
AFTER DELETE ON profile_search_documents BEGIN
    INSERT INTO profile_search_fts(
        profile_search_fts, rowid, content, title, role, source_kind
    ) VALUES (
        'delete', old.rowid, old.content, old.title, old.role, old.source_kind
    );
END;

CREATE TRIGGER profile_search_fts_update
AFTER UPDATE ON profile_search_documents BEGIN
    INSERT INTO profile_search_fts(
        profile_search_fts, rowid, content, title, role, source_kind
    ) VALUES (
        'delete', old.rowid, old.content, old.title, old.role, old.source_kind
    );
    INSERT INTO profile_search_fts(rowid, content, title, role, source_kind)
    VALUES (new.rowid, new.content, new.title, new.role, new.source_kind);
END;
"#;

// Run Center has no way to remove a run from view: `run_events` is
// deliberately append-only (see `run_events_forbid_delete` below) so the
// ledger stays a tamper-evident audit trail, and every child table's FK is
// `ON DELETE RESTRICT` for the same reason. Hard-deleting a run would fight
// that invariant. Archiving just hides it from the default `list_runs`
// result — the row and its full event history are untouched, so
// `integrity_check` and the audit trail stay exactly as trustworthy as
// before. Reversible via `unarchive_run`.
const MIGRATION_V3_SQL: &str = r#"
ALTER TABLE runs ADD COLUMN archived_at_ms INTEGER
    CHECK (archived_at_ms IS NULL OR archived_at_ms > 0);

CREATE INDEX runs_archived_idx ON runs(archived_at_ms) WHERE archived_at_ms IS NOT NULL;
"#;

// Human Approval Chains (ROADMAP.md, Phase 3): standalone sibling tables for
// `approval_chains.rs`'s multi-stage approval state machine — deliberately
// NOT threaded through `runs`/`run_events` (a chain stage isn't a step of any
// one immutable run; a chain can gate an arbitrary future action that has no
// run yet, or none at all). `approval_chains.rs` reads/writes these directly
// through `RunLedger::connection()`/`connection_mut()`, the same
// "companion store sharing this database" pattern `profile_store.rs` already
// uses for its own tables — see those methods' doc comments.
/// The unified agent process table — see `process_table.rs` for the record it
/// stores and why the five execution surfaces needed one.
///
/// Lives here, as a companion store sharing this database, for the same reason
/// `approval_chain_runs` does: `DaemonStore::open` opens `RunLedger` first
/// precisely so shared migrations apply once, which means the daemon gets this
/// table without a second migration path of its own.
///
/// The two triggers are not belt-and-braces. `process_table.rs` validates the
/// same rules in Rust, but companion stores reach this connection directly, and
/// the whole point of this table is that a transition can no longer be applied
/// by whoever happens to hold a handle — `DaemonStore::transition` is an
/// unguarded `UPDATE … WHERE job_id = ?` with no from-state precondition, and
/// that is the mistake being designed out.
const MIGRATION_V5_SQL: &str = r#"
CREATE TABLE agent_processes (
    process_id TEXT PRIMARY KEY,
    parent_process_id TEXT REFERENCES agent_processes(process_id) ON DELETE RESTRICT,
    kind TEXT NOT NULL CHECK (kind IN (
        'chat_turn', 'daemon_job', 'subagent', 'crew_member', 'workflow_run',
        'workflow_node', 'remote_run', 'background_shell', 'side_task'
    )),
    external_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('admitted', 'running', 'suspended', 'exited')),
    run_id TEXT REFERENCES runs(run_id) ON DELETE RESTRICT,
    workspace TEXT,
    profile TEXT,
    native_pid INTEGER,
    max_wall_ms INTEGER CHECK (max_wall_ms IS NULL OR max_wall_ms > 0),
    max_memory_bytes INTEGER CHECK (max_memory_bytes IS NULL OR max_memory_bytes > 0),
    max_output_bytes INTEGER CHECK (max_output_bytes IS NULL OR max_output_bytes > 0),
    max_child_processes INTEGER CHECK (max_child_processes IS NULL OR max_child_processes > 0),
    exit_status TEXT CHECK (exit_status IS NULL OR exit_status IN (
        'succeeded', 'failed', 'cancelled', 'limit_exceeded', 'lost', 'needs_reconciliation'
    )),
    exit_code INTEGER,
    exit_signal TEXT,
    exit_reason TEXT,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0),
    started_at_ms INTEGER CHECK (started_at_ms IS NULL OR started_at_ms > 0),
    exited_at_ms INTEGER CHECK (exited_at_ms IS NULL OR exited_at_ms > 0),
    CHECK ((state = 'exited') = (exit_status IS NOT NULL)),
    CHECK (parent_process_id IS NULL OR parent_process_id <> process_id),
    UNIQUE(kind, external_id)
) STRICT;

CREATE INDEX agent_processes_live_idx ON agent_processes(created_at_ms DESC)
    WHERE state <> 'exited';
CREATE INDEX agent_processes_kind_idx ON agent_processes(kind, created_at_ms DESC);
CREATE INDEX agent_processes_parent_idx ON agent_processes(parent_process_id)
    WHERE parent_process_id IS NOT NULL;
CREATE INDEX agent_processes_run_idx ON agent_processes(run_id)
    WHERE run_id IS NOT NULL;
CREATE INDEX agent_processes_workspace_idx ON agent_processes(workspace, created_at_ms DESC)
    WHERE workspace IS NOT NULL;

CREATE TRIGGER agent_processes_validate_transition
BEFORE UPDATE OF state ON agent_processes
WHEN OLD.state <> NEW.state AND NOT (
       (OLD.state = 'admitted'  AND NEW.state IN ('running', 'exited'))
    OR (OLD.state = 'running'   AND NEW.state IN ('suspended', 'exited'))
    OR (OLD.state = 'suspended' AND NEW.state IN ('running', 'exited'))
)
BEGIN
    SELECT RAISE(ABORT, 'illegal agent process state transition');
END;

CREATE TRIGGER agent_processes_forbid_identity_update
BEFORE UPDATE ON agent_processes
WHEN OLD.process_id <> NEW.process_id
  OR OLD.kind <> NEW.kind
  OR OLD.external_id <> NEW.external_id
  OR OLD.created_at_ms <> NEW.created_at_ms
BEGIN
    SELECT RAISE(ABORT, 'agent process identity is immutable');
END;
"#;

const MIGRATION_V4_SQL: &str = r#"
CREATE TABLE approval_chain_runs (
    chain_id TEXT PRIMARY KEY,
    template_id TEXT NOT NULL,
    operation_sha256 TEXT NOT NULL CHECK (length(operation_sha256) = 64),
    detail TEXT NOT NULL,
    total_stages INTEGER NOT NULL CHECK (total_stages > 0),
    current_stage INTEGER NOT NULL DEFAULT 0 CHECK (current_stage >= 0),
    status TEXT NOT NULL CHECK (status IN ('pending', 'approved', 'rejected', 'expired')),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0)
) STRICT;

CREATE INDEX approval_chain_runs_created_idx ON approval_chain_runs(created_at_ms);
CREATE INDEX approval_chain_runs_status_idx ON approval_chain_runs(status, created_at_ms)
    WHERE status = 'pending';

CREATE TABLE approval_chain_stage_decisions (
    chain_id TEXT NOT NULL REFERENCES approval_chain_runs(chain_id) ON DELETE RESTRICT,
    stage_index INTEGER NOT NULL CHECK (stage_index >= 0),
    stage_label TEXT NOT NULL,
    decision TEXT NOT NULL CHECK (decision IN ('allow', 'deny', 'expired')),
    escalated INTEGER NOT NULL DEFAULT 0 CHECK (escalated IN (0, 1)),
    decided_at_ms INTEGER NOT NULL CHECK (decided_at_ms > 0),
    decided_by_json BLOB,
    PRIMARY KEY(chain_id, stage_index)
) STRICT;

CREATE INDEX approval_chain_stage_decisions_chain_idx
    ON approval_chain_stage_decisions(chain_id, stage_index);
"#;

struct EventEffects<'a> {
    event_type: &'static str,
    status: Option<RunStatus>,
    terminal: bool,
    projection: Projection<'a>,
}

enum Projection<'a> {
    None,
    ApprovalRequested {
        request_id: &'a str,
        tool_call_id: &'a str,
        tool_name: &'a str,
        operation_sha256: &'a str,
        expires_at_ms: u64,
        detail: &'a str,
        risk_level: Option<&'a RiskLevel>,
    },
    ApprovalAwaiting {
        request_id: &'a str,
        operation_sha256: &'a str,
        expires_at_ms: u64,
    },
    ApprovalDecided {
        request_id: &'a str,
        operation_sha256: &'a str,
        decision: &'a PermissionDecision,
        decided_by: &'a ClientIdentity,
    },
    Artifact {
        artifact_id: &'a str,
        kind: &'a ArtifactKind,
        name: &'a str,
        media_type: &'a str,
        content_sha256: &'a str,
        size_bytes: u64,
    },
    Checkpoint {
        checkpoint_id: &'a str,
        kind: &'a CheckpointKind,
        label: &'a str,
        content_sha256: Option<&'a str>,
    },
    ExternalPrepared {
        mutation_id: &'a str,
        tool_call_id: &'a str,
        kind: &'a MutationKind,
        idempotency_key: Option<&'a str>,
        summary: &'a str,
    },
    ExternalConfirmed {
        mutation_id: &'a str,
        confirmation_ref: Option<&'a str>,
        summary: &'a str,
    },
    ExternalNeedsReconciliation {
        mutation_id: &'a str,
        reason: &'a str,
    },
}

/// The only match over `RunEvent` in the persistence layer. Protocol variant
/// additions therefore cause one compile-time exhaustiveness failure rather
/// than scattered projection drift.
fn derive_event_effects(event: &RunEvent) -> EventEffects<'_> {
    let (event_type, status, projection) = match event {
        RunEvent::Queued { .. } => ("queued", Some(RunStatus::Queued), Projection::None),
        RunEvent::Started { .. } => ("started", Some(RunStatus::Running), Projection::None),
        RunEvent::ModelDelta { .. } => ("model_delta", None, Projection::None),
        RunEvent::ToolProposed { .. } => ("tool_proposed", None, Projection::None),
        RunEvent::PermissionRequested {
            request_id,
            tool_call_id,
            tool_name,
            operation_sha256,
            expires_at_ms,
            detail,
            risk_level,
            ..
        } => (
            "permission_requested",
            None,
            Projection::ApprovalRequested {
                request_id,
                tool_call_id,
                tool_name,
                operation_sha256,
                expires_at_ms: *expires_at_ms,
                detail,
                risk_level: risk_level.as_ref(),
            },
        ),
        RunEvent::PermissionDecided {
            request_id,
            operation_sha256,
            decision,
            decided_by,
        } => (
            "permission_decided",
            Some(RunStatus::Running),
            Projection::ApprovalDecided {
                request_id,
                operation_sha256,
                decision,
                decided_by,
            },
        ),
        RunEvent::ToolStarted { .. } => ("tool_started", None, Projection::None),
        RunEvent::ToolFinished { .. } => ("tool_finished", None, Projection::None),
        RunEvent::ArtifactAdded {
            artifact_id,
            kind,
            name,
            media_type,
            content_sha256,
            size_bytes,
        } => (
            "artifact_added",
            None,
            Projection::Artifact {
                artifact_id,
                kind,
                name,
                media_type,
                content_sha256,
                size_bytes: *size_bytes,
            },
        ),
        RunEvent::CheckpointLinked {
            checkpoint_id,
            kind,
            label,
            content_sha256,
        } => (
            "checkpoint_linked",
            None,
            Projection::Checkpoint {
                checkpoint_id,
                kind,
                label,
                content_sha256: content_sha256.as_deref(),
            },
        ),
        RunEvent::VerificationFinished { .. } => ("verification_finished", None, Projection::None),
        RunEvent::UsageRecorded { .. } => ("usage_recorded", None, Projection::None),
        RunEvent::CancellationRequested { .. } => (
            "cancellation_requested",
            Some(RunStatus::Cancelling),
            Projection::None,
        ),
        RunEvent::ExternalMutationPrepared {
            mutation_id,
            tool_call_id,
            kind,
            idempotency_key,
            summary,
        } => (
            "external_mutation_prepared",
            None,
            Projection::ExternalPrepared {
                mutation_id,
                tool_call_id,
                kind,
                idempotency_key: idempotency_key.as_deref(),
                summary,
            },
        ),
        RunEvent::ExternalMutationConfirmed {
            mutation_id,
            confirmation_ref,
            summary,
        } => (
            "external_mutation_confirmed",
            None,
            Projection::ExternalConfirmed {
                mutation_id,
                confirmation_ref: confirmation_ref.as_deref(),
                summary,
            },
        ),
        RunEvent::AwaitingApproval {
            request_id,
            operation_sha256,
            expires_at_ms,
            ..
        } => (
            "awaiting_approval",
            Some(RunStatus::WaitingForPermission),
            Projection::ApprovalAwaiting {
                request_id,
                operation_sha256,
                expires_at_ms: *expires_at_ms,
            },
        ),
        RunEvent::Paused { .. } => ("paused", Some(RunStatus::Paused), Projection::None),
        RunEvent::Cancelling { .. } => {
            ("cancelling", Some(RunStatus::Cancelling), Projection::None)
        }
        RunEvent::Completed { .. } => ("completed", Some(RunStatus::Succeeded), Projection::None),
        RunEvent::Failed { .. } => ("failed", Some(RunStatus::Failed), Projection::None),
        RunEvent::Cancelled { .. } => ("cancelled", Some(RunStatus::Cancelled), Projection::None),
        RunEvent::NeedsReconciliation {
            mutation_id,
            reason,
        } => (
            "needs_reconciliation",
            Some(RunStatus::NeedsReconciliation),
            Projection::ExternalNeedsReconciliation {
                mutation_id,
                reason,
            },
        ),
    };
    EventEffects {
        event_type,
        status,
        terminal: event.is_terminal(),
        projection,
    }
}

fn apply_projection(
    transaction: &Transaction<'_>,
    envelope: &RunEventEnvelope,
    projection: &Projection<'_>,
) -> LedgerResult<()> {
    let sequence = to_sql_i64(envelope.sequence, "sequence")?;
    let occurred_at_ms = to_sql_i64(envelope.occurred_at_ms, "occurred_at_ms")?;
    match projection {
        Projection::None => {}
        Projection::ApprovalRequested {
            request_id,
            tool_call_id,
            tool_name,
            operation_sha256,
            expires_at_ms,
            detail,
            risk_level,
        } => {
            transaction.execute(
                "INSERT INTO approvals (
                    run_id, request_id, tool_call_id, tool_name, operation_sha256,
                    requested_sequence, expires_at_ms, detail, risk_level
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    envelope.run_id,
                    request_id,
                    tool_call_id,
                    tool_name,
                    operation_sha256,
                    sequence,
                    to_sql_i64(*expires_at_ms, "expires_at_ms")?,
                    detail,
                    risk_level.map(enum_token).transpose()?
                ],
            )?;
        }
        Projection::ApprovalAwaiting {
            request_id,
            operation_sha256,
            expires_at_ms,
        } => {
            let stored = transaction
                .query_row(
                    "SELECT operation_sha256, expires_at_ms, decision FROM approvals
                     WHERE run_id = ?1 AND request_id = ?2",
                    params![envelope.run_id, request_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .optional()?
                .ok_or_else(|| LedgerError::NotFound {
                    entity: "approval",
                    id: (*request_id).to_string(),
                })?;
            if stored.0 != *operation_sha256 {
                return Err(LedgerError::ApprovalDigestMismatch {
                    request_id: (*request_id).to_string(),
                });
            }
            if from_sql_u64(stored.1, "expires_at_ms")? != *expires_at_ms {
                return Err(LedgerError::ApprovalExpiryMismatch {
                    request_id: (*request_id).to_string(),
                });
            }
            if stored.2.is_some() {
                return Err(LedgerError::ApprovalAlreadyDecided {
                    request_id: (*request_id).to_string(),
                });
            }
            transaction.execute(
                "UPDATE approvals
                 SET awaiting_sequence = ?3
                 WHERE run_id = ?1 AND request_id = ?2",
                params![envelope.run_id, request_id, sequence],
            )?;
        }
        Projection::ApprovalDecided {
            request_id,
            operation_sha256,
            decision,
            decided_by,
        } => {
            let existing = transaction
                .query_row(
                    "SELECT operation_sha256, decision, expires_at_ms FROM approvals
                     WHERE run_id = ?1 AND request_id = ?2",
                    params![envelope.run_id, request_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .optional()?
                .ok_or_else(|| LedgerError::NotFound {
                    entity: "approval",
                    id: (*request_id).to_string(),
                })?;
            if existing.0 != *operation_sha256 {
                return Err(LedgerError::ApprovalDigestMismatch {
                    request_id: (*request_id).to_string(),
                });
            }
            if existing.1.is_some() {
                return Err(LedgerError::ApprovalAlreadyDecided {
                    request_id: (*request_id).to_string(),
                });
            }
            let expires_at_ms = from_sql_u64(existing.2, "expires_at_ms")?;
            match decision {
                PermissionDecision::Expired if envelope.occurred_at_ms < expires_at_ms => {
                    return Err(LedgerError::ApprovalDecisionTiming {
                        request_id: (*request_id).to_string(),
                        message: "expired decisions are valid only at or after expiry",
                    });
                }
                PermissionDecision::AllowOnce
                | PermissionDecision::AllowForRun
                | PermissionDecision::Deny
                    if envelope.occurred_at_ms >= expires_at_ms =>
                {
                    return Err(LedgerError::ApprovalDecisionTiming {
                        request_id: (*request_id).to_string(),
                        message: "allow or deny decisions must occur before expiry",
                    });
                }
                _ => {}
            }
            transaction.execute(
                "UPDATE approvals
                 SET decision = ?3, decided_sequence = ?4, decided_by_json = ?5
                 WHERE run_id = ?1 AND request_id = ?2",
                params![
                    envelope.run_id,
                    request_id,
                    enum_token(*decision)?,
                    sequence,
                    serde_json::to_vec(decided_by)?
                ],
            )?;
        }
        Projection::Artifact {
            artifact_id,
            kind,
            name,
            media_type,
            content_sha256,
            size_bytes,
        } => {
            transaction.execute(
                "INSERT INTO artifacts (
                    artifact_id, run_id, event_sequence, kind, name, media_type,
                    content_sha256, size_bytes, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    artifact_id,
                    envelope.run_id,
                    sequence,
                    enum_token(*kind)?,
                    name,
                    media_type,
                    content_sha256,
                    to_sql_i64(*size_bytes, "size_bytes")?,
                    occurred_at_ms
                ],
            )?;
        }
        Projection::Checkpoint {
            checkpoint_id,
            kind,
            label,
            content_sha256,
        } => {
            transaction.execute(
                "INSERT INTO checkpoints (
                    checkpoint_id, run_id, event_sequence, kind, label,
                    content_sha256, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    checkpoint_id,
                    envelope.run_id,
                    sequence,
                    enum_token(*kind)?,
                    label,
                    content_sha256,
                    occurred_at_ms
                ],
            )?;
        }
        Projection::ExternalPrepared {
            mutation_id,
            tool_call_id,
            kind,
            idempotency_key,
            summary,
        } => {
            transaction.execute(
                "INSERT INTO external_mutations (
                    run_id, mutation_id, tool_call_id, kind, state,
                    idempotency_key, summary, prepared_sequence, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7, ?8)",
                params![
                    envelope.run_id,
                    mutation_id,
                    tool_call_id,
                    enum_token(*kind)?,
                    idempotency_key,
                    summary,
                    sequence,
                    occurred_at_ms
                ],
            )?;
        }
        Projection::ExternalConfirmed {
            mutation_id,
            confirmation_ref,
            summary,
        } => {
            let changed = transaction.execute(
                "UPDATE external_mutations
                 SET state = 'confirmed', confirmed_sequence = ?3,
                     confirmation_ref = ?4, summary = ?5, updated_at_ms = ?6
                 WHERE run_id = ?1 AND mutation_id = ?2 AND state = 'pending'",
                params![
                    envelope.run_id,
                    mutation_id,
                    sequence,
                    confirmation_ref,
                    summary,
                    occurred_at_ms
                ],
            )?;
            if changed != 1 {
                return Err(LedgerError::InvalidTransition(format!(
                    "external mutation '{}' is missing or is not pending",
                    mutation_id
                )));
            }
        }
        Projection::ExternalNeedsReconciliation {
            mutation_id,
            reason,
        } => {
            let changed = transaction.execute(
                "UPDATE external_mutations
                 SET state = 'needs_reconciliation', reconciliation_reason = ?3,
                     updated_at_ms = ?4
                 WHERE run_id = ?1 AND mutation_id = ?2 AND state = 'pending'",
                params![envelope.run_id, mutation_id, reason, occurred_at_ms],
            )?;
            if changed != 1 {
                return Err(LedgerError::InvalidTransition(format!(
                    "external mutation '{}' is missing or is not pending",
                    mutation_id
                )));
            }
        }
    }
    Ok(())
}

fn load_run_from(connection: &Connection, run_id: &str) -> LedgerResult<Option<StoredRun>> {
    connection
        .query_row(
            "SELECT spec_json, status, last_sequence, terminal_sequence, updated_at_ms,
                    archived_at_ms
             FROM runs WHERE run_id = ?1",
            [run_id],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                ))
            },
        )
        .optional()?
        .map(|row| {
            Ok(StoredRun {
                spec: serde_json::from_slice(&row.0)?,
                status: parse_run_status(&row.1)?,
                last_sequence: from_sql_u64(row.2, "last_sequence")?,
                terminal_sequence: row
                    .3
                    .map(|value| from_sql_u64(value, "terminal_sequence"))
                    .transpose()?,
                updated_at_ms: from_sql_u64(row.4, "updated_at_ms")?,
                archived_at_ms: row
                    .5
                    .map(|value| from_sql_u64(value, "archived_at_ms"))
                    .transpose()?,
            })
        })
        .transpose()
}

fn collect_named_violations(
    connection: &Connection,
    sql: &str,
    label: &str,
    report: &mut IntegrityReport,
) -> LedgerResult<()> {
    let mut statement = connection.prepare(sql)?;
    for run_id in statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?
    {
        report.violations.push(format!("{label}: run {run_id}"));
    }
    Ok(())
}

fn bounded_limit(limit: usize) -> LedgerResult<i64> {
    if limit == 0 || limit > MAX_LIST_LIMIT {
        return Err(LedgerError::InvalidTransition(format!(
            "list limit must be between 1 and {MAX_LIST_LIMIT}"
        )));
    }
    i64::try_from(limit).map_err(|_| LedgerError::NumericOverflow("limit"))
}

fn to_sql_i64(value: u64, field: &'static str) -> LedgerResult<i64> {
    i64::try_from(value).map_err(|_| LedgerError::NumericOverflow(field))
}

fn from_sql_u64(value: i64, field: &'static str) -> LedgerResult<u64> {
    u64::try_from(value).map_err(|_| LedgerError::Corrupt(format!("{field} is negative")))
}

fn now_ms_i64() -> LedgerResult<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            LedgerError::Corrupt(format!("system clock precedes Unix epoch: {error}"))
        })?;
    let millis = u64::try_from(duration.as_millis())
        .map_err(|_| LedgerError::NumericOverflow("current timestamp"))?;
    to_sql_i64(millis, "current timestamp")
}

fn enum_token<T: Serialize>(value: &T) -> LedgerResult<String> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(value) => Ok(value),
        _ => Err(LedgerError::Corrupt(
            "protocol enum did not serialize as a string".to_string(),
        )),
    }
}

fn run_status_token(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Queued => "queued",
        RunStatus::Running => "running",
        RunStatus::WaitingForPermission => "waiting_for_permission",
        RunStatus::Paused => "paused",
        RunStatus::Cancelling => "cancelling",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
        RunStatus::NeedsReconciliation => "needs_reconciliation",
    }
}

fn validate_status_transition(current: RunStatus, next: Option<RunStatus>) -> LedgerResult<()> {
    if current == RunStatus::Cancelling
        && matches!(
            next,
            Some(
                RunStatus::Queued
                    | RunStatus::Running
                    | RunStatus::WaitingForPermission
                    | RunStatus::Paused
            )
        )
    {
        return Err(LedgerError::InvalidTransition(
            "a cancelling run cannot return to an active state".to_string(),
        ));
    }
    Ok(())
}

fn parse_run_status(value: &str) -> LedgerResult<RunStatus> {
    match value {
        "queued" => Ok(RunStatus::Queued),
        "running" => Ok(RunStatus::Running),
        "waiting_for_permission" => Ok(RunStatus::WaitingForPermission),
        "paused" => Ok(RunStatus::Paused),
        "cancelling" => Ok(RunStatus::Cancelling),
        "succeeded" => Ok(RunStatus::Succeeded),
        "failed" => Ok(RunStatus::Failed),
        "cancelled" => Ok(RunStatus::Cancelled),
        "needs_reconciliation" => Ok(RunStatus::NeedsReconciliation),
        other => Err(LedgerError::Corrupt(format!(
            "unknown run status '{other}'"
        ))),
    }
}

fn parse_permission_decision(value: &str) -> LedgerResult<PermissionDecision> {
    match value {
        "allow_once" => Ok(PermissionDecision::AllowOnce),
        "allow_for_run" => Ok(PermissionDecision::AllowForRun),
        "deny" => Ok(PermissionDecision::Deny),
        "expired" => Ok(PermissionDecision::Expired),
        other => Err(LedgerError::Corrupt(format!(
            "unknown permission decision '{other}'"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;
    use crate::run_protocol::{
        CapabilityAssessment, CapabilityState, ClientKind, ModelCapabilitiesSnapshot,
        ModelTargetSnapshot, PermissionMode, PermissionPolicySnapshot, RunBudgets, RunKind,
        ToolPolicyDecision, UsageSnapshot, RUN_PROTOCOL_SCHEMA_VERSION,
    };

    struct TempDb {
        path: PathBuf,
    }

    impl TempDb {
        fn new(label: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            Self {
                path: std::env::temp_dir().join(format!(
                    "little-monkey-ledger-{label}-{}-{counter}-{nanos}.db",
                    std::process::id()
                )),
            }
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            for path in [
                self.path.clone(),
                PathBuf::from(format!("{}-wal", self.path.display())),
                PathBuf::from(format!("{}-shm", self.path.display())),
            ] {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    fn client() -> ClientIdentity {
        ClientIdentity {
            client_id: "ledger-test".to_string(),
            instance_id: "instance-01".to_string(),
            kind: ClientKind::Test,
            version: "1.0.0-test".to_string(),
        }
    }

    fn capability() -> CapabilityAssessment {
        CapabilityAssessment {
            state: CapabilityState::Supported,
            evidence: "test fixture".to_string(),
        }
    }

    fn capabilities() -> ModelCapabilitiesSnapshot {
        ModelCapabilitiesSnapshot {
            tool_calling: capability(),
            vision: capability(),
            embeddings: capability(),
            structured_output: capability(),
            image_generation: capability(),
            audio: capability(),
            runtime_lifecycle: capability(),
            fim: capability(),
            code_completion: capability(),
            inline_edit: capability(),
            fim_metadata: None,
        }
    }

    fn spec(run_id: &str, idempotency_key: &str) -> RunSpec {
        RunSpec {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            run_id: run_id.to_string(),
            idempotency_key: idempotency_key.to_string(),
            created_at_ms: 1_000,
            kind: RunKind::Background,
            submitted_by: client(),
            task: "exercise the durable ledger".to_string(),
            instructions: None,
            input_artifact_ids: Vec::new(),
            target: ModelTargetSnapshot::Ollama {
                target_id: "ollama-test".to_string(),
                label: "Ollama test".to_string(),
                base_url: "http://127.0.0.1:11434".to_string(),
                model: "qwen-test".to_string(),
                is_cloud: false,
                capabilities: capabilities(),
                estimated_memory_bytes: Some(1),
            },
            workspace: None,
            permission_policy: PermissionPolicySnapshot {
                mode: PermissionMode::Manual,
                unattended: false,
                approval_timeout_ms: 60_000,
                default_tool_decision: ToolPolicyDecision::Prompt,
                tool_rules: Vec::new(),
                allow_network: false,
                allow_external_mutations: false,
            },
            budgets: RunBudgets {
                wall_time_ms: 60_000,
                max_iterations: 10,
                max_model_calls: 10,
                max_tool_calls: 10,
                max_input_tokens: 10_000,
                max_output_tokens: 10_000,
                max_cost_micros: None,
                max_artifact_bytes: 1_000_000,
                max_event_count: 1_000,
            },
        }
    }

    fn envelope(run_id: &str, sequence: u64, event_id: &str, event: RunEvent) -> RunEventEnvelope {
        RunEventEnvelope {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            event_id: event_id.to_string(),
            run_id: run_id.to_string(),
            sequence,
            occurred_at_ms: 2_000 + sequence,
            actor_id: None,
            emitter: client(),
            event,
        }
    }

    fn queued(run_id: &str, sequence: u64) -> RunEventEnvelope {
        envelope(
            run_id,
            sequence,
            &format!("event-{sequence}"),
            RunEvent::Queued { queue: None },
        )
    }

    fn started(run_id: &str, sequence: u64, event_id: &str) -> RunEventEnvelope {
        envelope(
            run_id,
            sequence,
            event_id,
            RunEvent::Started {
                engine_id: "engine-01".to_string(),
            },
        )
    }

    fn completed(run_id: &str, sequence: u64, event_id: &str) -> RunEventEnvelope {
        envelope(
            run_id,
            sequence,
            event_id,
            RunEvent::Completed {
                summary: Some("done".to_string()),
                result_artifact_ids: Vec::new(),
                usage: UsageSnapshot {
                    input_tokens: 1,
                    output_tokens: 1,
                    cached_input_tokens: 0,
                    model_calls: 1,
                    tool_calls: 0,
                    cost_micros: None,
                },
            },
        )
    }

    #[test]
    fn submit_is_idempotent_only_for_byte_identical_specs() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        let original = spec("run-idempotent", "submit/idempotent");

        let first = ledger.submit_run(&original).unwrap();
        assert!(first.inserted);
        let second = ledger.submit_run(&original).unwrap();
        assert!(!second.inserted);
        assert_eq!(second.run.spec, original);

        let mut changed = original.clone();
        changed.task = "different task".to_string();
        assert!(matches!(
            ledger.submit_run(&changed),
            Err(LedgerError::IdempotencyConflict { .. })
        ));
    }

    #[test]
    fn event_replay_requires_contiguous_order_and_loads_in_sequence() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        ledger
            .submit_run(&spec("run-order", "submit/order"))
            .unwrap();

        assert!(matches!(
            ledger.append_event(&started("run-order", 2, "started-too-early")),
            Err(LedgerError::SequenceMismatch {
                expected: 1,
                actual: 2,
                ..
            })
        ));
        ledger.append_event(&queued("run-order", 1)).unwrap();
        assert!(matches!(
            ledger.append_event(&started("run-order", 3, "gap")),
            Err(LedgerError::SequenceMismatch {
                expected: 2,
                actual: 3,
                ..
            })
        ));
        ledger
            .append_event(&started("run-order", 2, "started"))
            .unwrap();

        let events = ledger.load_events("run-order", 0, 10).unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(
            ledger.load_run("run-order").unwrap().unwrap().last_sequence,
            2
        );
    }

    #[test]
    fn terminal_event_is_unique_and_forbids_later_events() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        ledger
            .submit_run(&spec("run-terminal", "submit/terminal"))
            .unwrap();
        ledger.append_event(&queued("run-terminal", 1)).unwrap();
        ledger
            .append_event(&completed("run-terminal", 2, "completed"))
            .unwrap();

        let error = ledger
            .append_event(&envelope(
                "run-terminal",
                3,
                "cancelled-too-late",
                RunEvent::Cancelled { reason: None },
            ))
            .unwrap_err();
        assert!(matches!(
            error,
            LedgerError::TerminalRun {
                terminal_sequence: 2,
                ..
            }
        ));

        let run = ledger.load_run("run-terminal").unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Succeeded);
        assert_eq!(run.terminal_sequence, Some(2));
        assert_eq!(ledger.load_events("run-terminal", 0, 10).unwrap().len(), 2);
    }

    #[test]
    fn approval_decision_must_match_the_requested_operation_digest() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        ledger
            .submit_run(&spec("run-approval", "submit/approval"))
            .unwrap();
        ledger.append_event(&queued("run-approval", 1)).unwrap();
        let correct_digest = "a".repeat(64);
        let wrong_digest = "b".repeat(64);
        ledger
            .append_event(&envelope(
                "run-approval",
                2,
                "permission-requested",
                RunEvent::PermissionRequested {
                    request_id: "approval-01".to_string(),
                    tool_call_id: "tool-call-01".to_string(),
                    tool_name: "run_shell".to_string(),
                    operation_sha256: correct_digest.clone(),
                    expires_at_ms: 50_000,
                    detail: "run a command".to_string(),
                    risk_level: Some(RiskLevel::High),
                    risk_reason: Some("shell mutation".to_string()),
                },
            ))
            .unwrap();
        ledger
            .append_event(&envelope(
                "run-approval",
                3,
                "awaiting-approval",
                RunEvent::AwaitingApproval {
                    request_id: "approval-01".to_string(),
                    operation_sha256: correct_digest.clone(),
                    expires_at_ms: 50_000,
                    reason: None,
                },
            ))
            .unwrap();

        let wrong = envelope(
            "run-approval",
            4,
            "wrong-decision",
            RunEvent::PermissionDecided {
                request_id: "approval-01".to_string(),
                operation_sha256: wrong_digest,
                decision: PermissionDecision::AllowOnce,
                decided_by: client(),
            },
        );
        assert!(matches!(
            ledger.append_event(&wrong),
            Err(LedgerError::ApprovalDigestMismatch { .. })
        ));
        assert_eq!(
            ledger
                .load_run("run-approval")
                .unwrap()
                .unwrap()
                .last_sequence,
            3
        );

        ledger
            .append_event(&envelope(
                "run-approval",
                4,
                "correct-decision",
                RunEvent::PermissionDecided {
                    request_id: "approval-01".to_string(),
                    operation_sha256: correct_digest.clone(),
                    decision: PermissionDecision::AllowOnce,
                    decided_by: client(),
                },
            ))
            .unwrap();
        let approval = ledger
            .load_approval("run-approval", "approval-01")
            .unwrap()
            .unwrap();
        assert_eq!(approval.operation_sha256, correct_digest);
        assert_eq!(approval.decision, Some(PermissionDecision::AllowOnce));
        assert_eq!(approval.decided_sequence, Some(4));
    }

    #[test]
    fn approval_expiry_is_immutable_and_decision_timing_rolls_back() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        ledger
            .submit_run(&spec("run-expiry", "submit/expiry"))
            .unwrap();
        ledger.append_event(&queued("run-expiry", 1)).unwrap();
        let digest = "e".repeat(64);
        ledger
            .append_event(&envelope(
                "run-expiry",
                2,
                "expiry-request",
                RunEvent::PermissionRequested {
                    request_id: "approval-expiry".to_string(),
                    tool_call_id: "tool-call-expiry".to_string(),
                    tool_name: "run_shell".to_string(),
                    operation_sha256: digest.clone(),
                    expires_at_ms: 5_000,
                    detail: "timed approval".to_string(),
                    risk_level: None,
                    risk_reason: None,
                },
            ))
            .unwrap();

        assert!(matches!(
            ledger.append_event(&envelope(
                "run-expiry",
                3,
                "changed-expiry",
                RunEvent::AwaitingApproval {
                    request_id: "approval-expiry".to_string(),
                    operation_sha256: digest.clone(),
                    expires_at_ms: 6_000,
                    reason: None,
                },
            )),
            Err(LedgerError::ApprovalExpiryMismatch { .. })
        ));
        let approval = ledger
            .load_approval("run-expiry", "approval-expiry")
            .unwrap()
            .unwrap();
        assert_eq!(approval.expires_at_ms, 5_000);
        assert_eq!(approval.awaiting_sequence, None);
        assert_eq!(
            ledger
                .load_run("run-expiry")
                .unwrap()
                .unwrap()
                .last_sequence,
            2
        );

        ledger
            .append_event(&envelope(
                "run-expiry",
                3,
                "correct-expiry",
                RunEvent::AwaitingApproval {
                    request_id: "approval-expiry".to_string(),
                    operation_sha256: digest.clone(),
                    expires_at_ms: 5_000,
                    reason: None,
                },
            ))
            .unwrap();

        let mut late_allow = envelope(
            "run-expiry",
            4,
            "late-allow",
            RunEvent::PermissionDecided {
                request_id: "approval-expiry".to_string(),
                operation_sha256: digest.clone(),
                decision: PermissionDecision::AllowOnce,
                decided_by: client(),
            },
        );
        late_allow.occurred_at_ms = 5_000;
        assert!(matches!(
            ledger.append_event(&late_allow),
            Err(LedgerError::ApprovalDecisionTiming { .. })
        ));

        let mut early_expired = envelope(
            "run-expiry",
            4,
            "early-expired",
            RunEvent::PermissionDecided {
                request_id: "approval-expiry".to_string(),
                operation_sha256: digest.clone(),
                decision: PermissionDecision::Expired,
                decided_by: client(),
            },
        );
        early_expired.occurred_at_ms = 4_999;
        assert!(matches!(
            ledger.append_event(&early_expired),
            Err(LedgerError::ApprovalDecisionTiming { .. })
        ));
        assert_eq!(
            ledger
                .load_run("run-expiry")
                .unwrap()
                .unwrap()
                .last_sequence,
            3
        );
        assert_eq!(
            ledger
                .load_approval("run-expiry", "approval-expiry")
                .unwrap()
                .unwrap()
                .decision,
            None
        );

        let mut expired = early_expired;
        expired.event_id = "expired-at-deadline".to_string();
        expired.occurred_at_ms = 5_000;
        ledger.append_event(&expired).unwrap();
        assert_eq!(
            ledger
                .load_approval("run-expiry", "approval-expiry")
                .unwrap()
                .unwrap()
                .decision,
            Some(PermissionDecision::Expired)
        );
    }

    #[test]
    fn cancelling_run_cannot_return_to_active_state_and_rolls_back_sequence() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        ledger
            .submit_run(&spec("run-cancelling", "submit/cancelling"))
            .unwrap();
        ledger.append_event(&queued("run-cancelling", 1)).unwrap();
        ledger
            .append_event(&envelope(
                "run-cancelling",
                2,
                "cancelling",
                RunEvent::Cancelling { reason: None },
            ))
            .unwrap();

        assert!(matches!(
            ledger.append_event(&started("run-cancelling", 3, "restart-invalid")),
            Err(LedgerError::InvalidTransition(_))
        ));
        let run = ledger.load_run("run-cancelling").unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Cancelling);
        assert_eq!(run.last_sequence, 2);

        ledger
            .append_event(&envelope(
                "run-cancelling",
                3,
                "cancelled",
                RunEvent::Cancelled { reason: None },
            ))
            .unwrap();
    }

    #[test]
    fn load_events_revalidates_stored_protocol_data() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        ledger
            .submit_run(&spec("run-tampered", "submit/tampered"))
            .unwrap();
        let valid = queued("run-tampered", 1);
        ledger.append_event(&valid).unwrap();

        let mut tampered = valid;
        tampered.event = RunEvent::Queued {
            queue: Some("invalid queue id".to_string()),
        };
        ledger
            .connection
            .execute_batch("DROP TRIGGER run_events_forbid_update")
            .unwrap();
        ledger
            .connection
            .execute(
                "UPDATE run_events SET envelope_json = ?1 WHERE event_id = 'event-1'",
                [serde_json::to_vec(&tampered).unwrap()],
            )
            .unwrap();

        assert!(matches!(
            ledger.load_events("run-tampered", 0, 10),
            Err(LedgerError::Corrupt(_))
        ));
    }

    #[test]
    fn artifact_checkpoint_and_external_mutation_projections_commit_with_events() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        ledger
            .submit_run(&spec("run-projections", "submit/projections"))
            .unwrap();
        ledger.append_event(&queued("run-projections", 1)).unwrap();
        ledger
            .append_event(&envelope(
                "run-projections",
                2,
                "artifact-added",
                RunEvent::ArtifactAdded {
                    artifact_id: "artifact-01".to_string(),
                    kind: ArtifactKind::Report,
                    name: "report.md".to_string(),
                    media_type: "text/markdown".to_string(),
                    content_sha256: "c".repeat(64),
                    size_bytes: 42,
                },
            ))
            .unwrap();
        ledger
            .append_event(&envelope(
                "run-projections",
                3,
                "checkpoint-linked",
                RunEvent::CheckpointLinked {
                    checkpoint_id: "checkpoint-01".to_string(),
                    kind: CheckpointKind::Workspace,
                    label: "Before edits".to_string(),
                    content_sha256: Some("d".repeat(64)),
                },
            ))
            .unwrap();
        ledger
            .append_event(&envelope(
                "run-projections",
                4,
                "mutation-prepared",
                RunEvent::ExternalMutationPrepared {
                    mutation_id: "mutation-01".to_string(),
                    tool_call_id: "tool-call-01".to_string(),
                    kind: MutationKind::Git,
                    idempotency_key: Some("github/pr-01".to_string()),
                    summary: "create draft PR".to_string(),
                },
            ))
            .unwrap();
        ledger
            .append_event(&envelope(
                "run-projections",
                5,
                "mutation-confirmed",
                RunEvent::ExternalMutationConfirmed {
                    mutation_id: "mutation-01".to_string(),
                    confirmation_ref: Some("pr-123".to_string()),
                    summary: "draft PR created".to_string(),
                },
            ))
            .unwrap();

        assert_eq!(
            ledger
                .connection
                .query_row(
                    "SELECT size_bytes FROM artifacts WHERE artifact_id = 'artifact-01'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            42
        );
        assert_eq!(
            ledger
                .connection
                .query_row(
                    "SELECT event_sequence FROM checkpoints WHERE checkpoint_id = 'checkpoint-01'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            3
        );
        let mutation = ledger
            .connection
            .query_row(
                "SELECT state, confirmed_sequence, confirmation_ref
                 FROM external_mutations
                 WHERE run_id = 'run-projections' AND mutation_id = 'mutation-01'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(mutation, ("confirmed".to_string(), 5, "pr-123".to_string()));
        assert_eq!(
            ledger
                .load_run("run-projections")
                .unwrap()
                .unwrap()
                .last_sequence,
            5
        );
        assert!(ledger.integrity_check().unwrap().is_ok());
    }

    #[test]
    fn committed_wal_state_survives_drop_and_reopen() {
        let database = TempDb::new("reopen");
        {
            let mut ledger = RunLedger::open(&database.path).unwrap();
            ledger
                .submit_run(&spec("run-reopen", "submit/reopen"))
                .unwrap();
            ledger.append_event(&queued("run-reopen", 1)).unwrap();
        }

        let ledger = RunLedger::open(&database.path).unwrap();
        let run = ledger.load_run("run-reopen").unwrap().unwrap();
        assert_eq!(run.last_sequence, 1);
        assert_eq!(ledger.load_events("run-reopen", 0, 10).unwrap().len(), 1);
        assert!(ledger.integrity_check().unwrap().is_ok());
    }

    #[test]
    fn concurrent_append_race_allows_exactly_one_writer_for_a_sequence() {
        let database = TempDb::new("concurrent");
        {
            let mut ledger = RunLedger::open(&database.path).unwrap();
            ledger
                .submit_run(&spec("run-concurrent", "submit/concurrent"))
                .unwrap();
            ledger.append_event(&queued("run-concurrent", 1)).unwrap();
        }

        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for suffix in ["a", "b"] {
            let path = database.path.clone();
            let barrier = Arc::clone(&barrier);
            let event_id = format!("concurrent-{suffix}");
            handles.push(thread::spawn(move || {
                let mut ledger = RunLedger::open(path).unwrap();
                barrier.wait();
                ledger.append_event(&started("run-concurrent", 2, &event_id))
            }));
        }
        barrier.wait();
        let results = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(LedgerError::SequenceMismatch { .. })))
                .count(),
            1
        );

        let ledger = RunLedger::open(&database.path).unwrap();
        assert_eq!(
            ledger.load_events("run-concurrent", 0, 10).unwrap().len(),
            2
        );
        assert!(ledger.integrity_check().unwrap().is_ok());
    }

    #[test]
    fn migration_is_safe_to_rerun_and_installs_the_shared_profile_schema() {
        let database = TempDb::new("migration");
        {
            let ledger = RunLedger::open(&database.path).unwrap();
            assert_eq!(ledger.applied_migrations().unwrap(), vec![1, 2, 3, 4, 5]);
        }
        let ledger = RunLedger::open(&database.path).unwrap();
        assert_eq!(ledger.applied_migrations().unwrap(), vec![1, 2, 3, 4, 5]);

        let journal_mode = ledger
            .connection
            .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
            .unwrap();
        assert_eq!(journal_mode, "wal");
        assert_eq!(
            ledger
                .connection
                .query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(
            ledger.connection.limit(Limit::SQLITE_LIMIT_LENGTH).unwrap() <= MAX_SQLITE_VALUE_BYTES
        );
        assert_eq!(
            ledger
                .connection
                .limit(Limit::SQLITE_LIMIT_ATTACHED)
                .unwrap(),
            0
        );

        for table in [
            "runs",
            "run_events",
            "approvals",
            "artifacts",
            "checkpoints",
            "external_mutations",
            "run_leases",
            "worktree_leases",
            "triggers",
            "trigger_deliveries",
            "paired_clients",
            "session_groups",
            "sessions",
            "messages",
            "message_translations",
            "attachments",
            "message_attachments",
            "actor_transcripts",
            "profile_state",
            "profile_crews",
            "profile_run_search_state",
            "profile_message_attachment_links",
            "profile_search_documents",
            "approval_chain_runs",
            "approval_chain_stage_decisions",
        ] {
            let exists = ledger
                .connection
                .query_row(
                    "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                    [table],
                    |_| Ok(()),
                )
                .optional()
                .unwrap()
                .is_some();
            assert!(exists, "missing shared ledger/profile table {table}");
        }

        let fts_table_exists = ledger
            .connection
            .query_row(
                "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'messages_fts'",
                [],
                |_| Ok(()),
            )
            .optional()
            .unwrap()
            .is_some();
        assert_eq!(fts_table_exists, ledger.has_fts5().unwrap());
        let profile_fts_table_exists = ledger
            .connection
            .query_row(
                "SELECT 1 FROM sqlite_schema
                  WHERE type = 'table' AND name = 'profile_search_fts'",
                [],
                |_| Ok(()),
            )
            .optional()
            .unwrap()
            .is_some();
        assert_eq!(profile_fts_table_exists, ledger.has_fts5().unwrap());
        if ledger.has_fts5().unwrap() {
            ledger
                .connection
                .execute(
                    "INSERT INTO sessions (
                        session_id, ordinal, title, created_at_ms, updated_at_ms
                     ) VALUES ('session-fts', 0, 'FTS test', 1, 1)",
                    [],
                )
                .unwrap();
            ledger
                .connection
                .execute(
                    "INSERT INTO messages (
                        message_id, session_id, ordinal, role, content,
                        created_at_ms, updated_at_ms
                     ) VALUES (
                        'message-fts', 'session-fts', 0, 'assistant',
                        'durable searchable transcript', 1, 1
                     )",
                    [],
                )
                .unwrap();
            let matches = ledger
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM messages_fts
                     WHERE messages_fts MATCH 'searchable'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap();
            assert_eq!(matches, 1);
        }
        assert!(ledger.integrity_check().unwrap().is_ok());
    }

    #[test]
    fn archive_run_hides_it_from_the_default_list_but_keeps_its_events() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        ledger
            .submit_run(&spec("run-archive", "submit/archive"))
            .unwrap();
        ledger.append_event(&queued("run-archive", 1)).unwrap();
        ledger
            .append_event(&completed("run-archive", 2, "completed"))
            .unwrap();

        let archived = ledger.archive_run("run-archive", 5_000).unwrap();
        assert_eq!(archived.archived_at_ms, Some(5_000));
        // Archiving is a view concern only — the event history is untouched.
        assert_eq!(
            ledger.load_events("run-archive", 0, 10).unwrap().len(),
            2
        );
        assert!(ledger.integrity_check().unwrap().is_ok());

        assert!(ledger
            .list_runs(100, false)
            .unwrap()
            .iter()
            .all(|run| run.spec.run_id != "run-archive"));
        assert!(ledger
            .list_runs(100, true)
            .unwrap()
            .iter()
            .any(|run| run.spec.run_id == "run-archive"));

        let unarchived = ledger.unarchive_run("run-archive").unwrap();
        assert_eq!(unarchived.archived_at_ms, None);
        assert!(ledger
            .list_runs(100, false)
            .unwrap()
            .iter()
            .any(|run| run.spec.run_id == "run-archive"));
    }

    #[test]
    fn archive_run_rejects_a_run_that_is_still_active() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        ledger
            .submit_run(&spec("run-active", "submit/active"))
            .unwrap();
        ledger.append_event(&queued("run-active", 1)).unwrap();

        assert!(matches!(
            ledger.archive_run("run-active", 5_000),
            Err(LedgerError::InvalidTransition(_))
        ));
        assert!(ledger
            .list_runs(100, false)
            .unwrap()
            .iter()
            .any(|run| run.spec.run_id == "run-active"));
    }
}
