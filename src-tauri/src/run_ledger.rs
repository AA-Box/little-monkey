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
use serde::{Deserialize, Serialize};

use crate::run_protocol::{
    ArtifactKind, CheckpointKind, ClientIdentity, MutationKind, PermissionDecision, RiskLevel,
    RunEvent, RunEventEnvelope, RunSpec, RunStatus,
};

/// Domain separator, so a chain hash can never be mistaken for — or collide
/// with — one of the per-payload `*_sha256` digests the ledger already stores.
const CHAIN_HASH_DOMAIN: &[u8] = b"little-monkey/run-event-chain/v1";

/// Domain separator for the subsystem stream's chain. Distinct from
/// [`CHAIN_HASH_DOMAIN`] so a row cannot be lifted from one chain into the other
/// and still verify.
const SUBSYSTEM_CHAIN_HASH_DOMAIN: &[u8] = b"little-monkey/subsystem-event-chain/v1";

/// One link of the subsystem event chain.
///
/// Same construction as [`event_chain_hash`] and for the same reasons —
/// length-prefixed fields, a presence tag on every optional — with one
/// difference worth stating: there is no "appended only when present" escape
/// here. That trick exists in [`event_chain_hash`] to keep rows written before a
/// column existed verifiable, and this table has no such rows. Every field is
/// unconditional, so a future column added here will need the same treatment
/// V10 needed there.
#[allow(clippy::too_many_arguments)]
fn subsystem_chain_hash(
    previous: Option<&str>,
    event_id: &str,
    subsystem: &str,
    action: &str,
    occurred_at_ms: i64,
    run_id: Option<&str>,
    attribution: &str,
    process_id: Option<&str>,
    permission_request_id: Option<&str>,
    outcome: &str,
    detail_json: Option<&[u8]>,
) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    let mut field = |bytes: &[u8]| {
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    };
    field(SUBSYSTEM_CHAIN_HASH_DOMAIN);
    field(previous.unwrap_or_default().as_bytes());
    field(&[u8::from(previous.is_some())]);
    field(event_id.as_bytes());
    field(subsystem.as_bytes());
    field(action.as_bytes());
    field(&occurred_at_ms.to_be_bytes());
    field(run_id.unwrap_or_default().as_bytes());
    field(&[u8::from(run_id.is_some())]);
    field(attribution.as_bytes());
    field(process_id.unwrap_or_default().as_bytes());
    field(&[u8::from(process_id.is_some())]);
    field(permission_request_id.unwrap_or_default().as_bytes());
    field(&[u8::from(permission_request_id.is_some())]);
    field(outcome.as_bytes());
    field(detail_json.unwrap_or_default());
    field(&[u8::from(detail_json.is_some())]);

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push(HEX[(byte >> 4) as usize] as char);
        hex.push(HEX[(byte & 0x0f) as usize] as char);
    }
    hex
}

/// One link of the run event chain: SHA-256 over every column of the row, bound
/// to the previous row's hash.
///
/// Fields are **length-prefixed**, not concatenated. Concatenation alone is
/// ambiguous — `("ab", "c")` and `("a", "bc")` produce identical bytes — so a
/// naive join would let one event be rewritten as a different event with the
/// same hash. Each field contributes its big-endian `u64` length followed by its
/// bytes, which no two distinct field lists can produce.
///
/// `previous` is `None` only at the start of a chain: either the run's first
/// event, or the first event appended after V9 to a run whose earlier events
/// predate chaining. Both cases are reported by
/// [`RunLedger::verify_run_chain`] rather than papered over.
///
/// **`process_id` contributes nothing at all when it is `None`, and that is what
/// keeps V9-era rows verifiable.** A column added after the chain shipped is
/// otherwise a dilemma: fold it in unconditionally and every row written before
/// it existed fails to verify; leave it out and it is the one column an attacker
/// may rewrite for free. Skipping the field entirely for `None` gives both — a
/// row with no process hashes exactly as it did under V9, while setting,
/// changing, or clearing a process id all change the digest and are caught.
#[allow(clippy::too_many_arguments)]
fn event_chain_hash(
    previous: Option<&str>,
    event_id: &str,
    run_id: &str,
    sequence: u64,
    occurred_at_ms: i64,
    actor_id: Option<&str>,
    event_type: &str,
    emitter_json: &[u8],
    envelope_json: &[u8],
    derived_status: Option<&str>,
    is_terminal: bool,
    process_id: Option<&str>,
) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    let mut field = |bytes: &[u8]| {
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    };
    field(CHAIN_HASH_DOMAIN);
    // An absent optional field and an empty one must not hash alike, so each
    // carries a one-byte presence tag ahead of its value.
    field(previous.unwrap_or_default().as_bytes());
    field(&[u8::from(previous.is_some())]);
    field(event_id.as_bytes());
    field(run_id.as_bytes());
    field(&sequence.to_be_bytes());
    field(&occurred_at_ms.to_be_bytes());
    field(actor_id.unwrap_or_default().as_bytes());
    field(&[u8::from(actor_id.is_some())]);
    field(event_type.as_bytes());
    field(emitter_json);
    field(envelope_json);
    field(derived_status.unwrap_or_default().as_bytes());
    field(&[u8::from(derived_status.is_some())]);
    field(&[u8::from(is_terminal)]);
    // Appended only when present — see this function's doc for why that is what
    // lets a column added after V9 be covered without invalidating V9's rows.
    if let Some(process_id) = process_id {
        field(process_id.as_bytes());
    }

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest.iter() {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

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
const MIGRATION_V6: i64 = 6;
const MIGRATION_V6_CHECKSUM: &str = "process-signal-intent-v6-2026-08-02";
const MIGRATION_V7: i64 = 7;
const MIGRATION_V7_CHECKSUM: &str = "process-kill-intent-v7-2026-08-03";
const MIGRATION_V8: i64 = 8;
const MIGRATION_V8_CHECKSUM: &str = "process-resource-ledger-v8-2026-08-06";
const MIGRATION_V9: i64 = 9;
const MIGRATION_V9_CHECKSUM: &str = "run-event-hash-chain-v9-2026-08-07";
const MIGRATION_V10: i64 = 10;
const MIGRATION_V10_CHECKSUM: &str = "run-event-process-identity-v10-2026-08-07";
const MIGRATION_V11: i64 = 11;
const MIGRATION_V11_CHECKSUM: &str = "permission-decisions-v11-2026-08-07";
const MIGRATION_V12: i64 = 12;
const MIGRATION_V12_CHECKSUM: &str = "subsystem-events-v12-2026-08-07";
const MIGRATION_V13: i64 = 13;
const MIGRATION_V13_CHECKSUM: &str = "compatible-migrations-v13-2026-08-08";
const MIGRATION_V14: i64 = 14;
const MIGRATION_V14_CHECKSUM: &str = "egress-destinations-v14-2026-08-08";
const MIGRATION_V15: i64 = 15;
const MIGRATION_V15_CHECKSUM: &str = "tool-call-origin-v15-2026-08-08";
const MIGRATION_V16: i64 = 16;
const MIGRATION_V16_CHECKSUM: &str = "context-reuse-v16-2026-08-08";
const MIGRATION_V17: i64 = 17;
const MIGRATION_V17_CHECKSUM: &str = "context-budget-v17-2026-08-08";
const MIGRATION_V18: i64 = 18;
const MIGRATION_V18_CHECKSUM: &str = "browser-session-kind-v18-2026-08-09";
const MIGRATION_V19: i64 = 19;
const MIGRATION_V19_CHECKSUM: &str = "unattributed-egress-destinations-v19-2026-08-09";
const MIGRATION_V20: i64 = 20;
const MIGRATION_V20_CHECKSUM: &str = "subsystem-worktree-v20-2026-08-10";
const MIGRATION_V21: i64 = 21;
const MIGRATION_V21_CHECKSUM: &str = "foreground-shell-and-limit-breach-v21-2026-08-14";
const MIGRATION_V22: i64 = 22;
const MIGRATION_V22_CHECKSUM: &str = "native-process-identity-v22-2026-08-14";

/// The newest schema this binary knows how to write.
const SCHEMA_VERSION: i64 = MIGRATION_V22;

/// Whether a migration keeps older binaries able to open the database.
///
/// This is the fact the forward-only guard was missing, and the reason
/// `denial_sink.rs` is a separate database file at all — see its module doc,
/// which spells out that a schema bump is "a one-way door for the whole ledger"
/// because a user who rolls back to the previous build gets a binary that cannot
/// open its own run history. That was true, and it pushed an observability table
/// out into a second file to avoid it.
///
/// It does not have to be true. Most migrations here only *add* — a table an
/// older binary never queries, a nullable column its inserts omit. Those are
/// invisible to it. What genuinely breaks an older binary is a migration that
/// makes the database **reject writes it used to accept**, which in practice
/// means a trigger or constraint over a table that binary already writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compatibility {
    /// An older binary can still open and write this database. The floor is
    /// inherited from the last breaking migration.
    Additive,
    /// This migration rejects writes an older binary would make, so that binary
    /// must not open the database at all.
    RequiresThisVersion,
}

/// The result of recomputing a run's event chain.
///
/// A tagged union rather than a `bool` plus fields, so a caller cannot read
/// `covered_from` off a broken chain and present it as a verified range — the
/// same reason the resource ledger's panel renders a tagged union.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum ChainVerification {
    Intact {
        /// First sequence the chain vouches for. `None` when no event carries a
        /// hash at all, which is every run written before migration V9 —
        /// distinct from "verified and empty".
        covered_from: Option<u64>,
        covered_through: Option<u64>,
        events_seen: u64,
        /// How many of `events_seen` name a K1 process. Reported rather than
        /// assumed complete: an event appended outside a process scope names
        /// none, and the gap between the two numbers is the honest measure of how
        /// far per-event attribution actually reaches today.
        events_naming_a_process: u64,
    },
    Broken {
        sequence: u64,
        detail: String,
    },
}

/// The origin's side of a K18 handover: the chain tip the target must name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationDeparture {
    pub run_id: String,
    pub sequence: u64,
    pub event_hash: String,
    pub target_node_id: String,
    pub payload_sha256: String,
    pub checkpoint_id: String,
}

/// The target's side of a K18 handover, read back from its first event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationArrival {
    pub run_id: String,
    pub origin_node_id: String,
    pub origin_last_sequence: u64,
    pub origin_last_event_hash: String,
    pub payload_sha256: String,
    pub event_hash: String,
}

/// Whether two nodes' halves of one run are the same chain (roadmap K18).
///
/// A tagged union for [`ChainVerification`]'s reason: "the halves join" and
/// "here is why they do not" must not be readable off the same value, or a
/// caller shows a migration as audited when the link is broken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum MigrationChainJoin {
    Joined {
        run_id: String,
        origin_node_id: String,
        target_node_id: String,
        /// Last sequence the origin vouches for; the target's chain restarts at
        /// 1 because its `runs` row is new, so the two numbers do not continue
        /// one another and are reported separately rather than summed.
        origin_last_sequence: u64,
        payload_sha256: String,
    },
    Broken {
        detail: String,
    },
}

/// Joins the two halves of a migrated run, or says exactly which fact disagrees.
///
/// Pure, and takes both halves as values, because the whole point of K18's
/// "single ledger event chain across both nodes" is that no one database holds
/// them: an auditor gathers a departure from one machine and an arrival from
/// the other and checks them here. Each half is separately verifiable by
/// [`RunLedger::verify_run_chain`] on its own node; this is the seam between.
#[must_use]
pub fn join_migration_chain(
    departure: &MigrationDeparture,
    arrival: &MigrationArrival,
) -> MigrationChainJoin {
    let broken = |detail: String| MigrationChainJoin::Broken { detail };
    if departure.run_id != arrival.run_id {
        return broken(format!(
            "the origin departed run '{}' but the target admitted run '{}'",
            departure.run_id, arrival.run_id
        ));
    }
    if arrival.origin_last_event_hash != departure.event_hash {
        return broken(
            "the target's arrival names a different origin event hash than the origin's departure"
                .to_string(),
        );
    }
    if arrival.origin_last_sequence != departure.sequence {
        return broken(format!(
            "the target's arrival names origin sequence {} but the departure is sequence {}",
            arrival.origin_last_sequence, departure.sequence
        ));
    }
    if arrival.payload_sha256 != departure.payload_sha256 {
        return broken(
            "the two nodes recorded different payload digests for the same move".to_string(),
        );
    }
    MigrationChainJoin::Joined {
        run_id: departure.run_id.clone(),
        origin_node_id: arrival.origin_node_id.clone(),
        target_node_id: departure.target_node_id.clone(),
        origin_last_sequence: departure.sequence,
        payload_sha256: departure.payload_sha256.clone(),
    }
}

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

/// Whether a decision's `tool_call_id` names a real tool call.
///
/// The same distinction [`PermissionAttribution`] draws for the run, drawn for
/// the tool call, and for the same reason. `tool_call_id` is `NOT NULL`, so a
/// caller with no tool call in hand — deleting a model from Settings, running a
/// local app definition over HTTP — had one **invented** for it. That id is
/// shaped exactly like a real one and joins to nothing, so the acceptance's own
/// question ("produce the decision that authorized this tool call") silently
/// returned nothing for a reason the log did not record: not "no decision", but
/// "this decision was never about a tool call at all".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolCallOrigin {
    /// The caller supplied the id, so it joins to a real tool call.
    Caller,
    /// There was no tool call and the id was generated to fill the column.
    /// Nothing joins to it, and that is the fact worth recording.
    Synthesized,
    /// Written before this column existed. Kept distinct from the two above for
    /// [`PermissionAttribution::Unknown`]'s reason: "we never recorded it" must
    /// not read as "we recorded that it was real".
    Unknown,
}

impl ToolCallOrigin {
    /// The stable identity that gets persisted — what the CHECK constraint
    /// lists. Never reworded.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            ToolCallOrigin::Caller => "caller",
            ToolCallOrigin::Synthesized => "synthesized",
            ToolCallOrigin::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> LedgerResult<Self> {
        match value {
            "caller" => Ok(ToolCallOrigin::Caller),
            "synthesized" => Ok(ToolCallOrigin::Synthesized),
            "unknown" => Ok(ToolCallOrigin::Unknown),
            other => Err(LedgerError::Corrupt(format!(
                "unknown tool call origin '{other}'"
            ))),
        }
    }
}

/// What a permission decision belongs to.
///
/// The two arms of [`crate::run_scope::RunScope`] plus the two states a scope
/// cannot express. Keeping them apart is the whole point: a blank attribution
/// that might mean "background work" or might mean "we lost it" cannot be read
/// either way, and an audit trail you cannot read is not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionAttribution {
    /// Raised inside a run that the ledger holds, so `run_id` joins to `runs`
    /// and the matching `PermissionRequested` event exists too.
    LedgerRun,
    /// Raised inside a run whose id is real but was never registered in the
    /// ledger — a chat turn running without durable runs enabled. Before this
    /// table, this case wrote nothing at all.
    UnregisteredRun,
    /// Deliberately outside any run, with the reason named.
    Unattributed(crate::run_scope::Unattributed),
    /// Nobody told us. A call site not yet carrying a [`crate::run_scope`], kept
    /// distinct from `Unattributed` so "not instrumented" never reads as
    /// "background work".
    Unknown,
}

impl PermissionAttribution {
    /// The stable identity that gets persisted. Never reworded — it is what the
    /// `attribution` CHECK constraint lists.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            PermissionAttribution::LedgerRun => "ledger-run",
            PermissionAttribution::UnregisteredRun => "unregistered-run",
            PermissionAttribution::Unknown => "unknown",
            PermissionAttribution::Unattributed(reason) => reason.code(),
        }
    }

    fn parse(value: &str) -> LedgerResult<Self> {
        match value {
            "ledger-run" => Ok(PermissionAttribution::LedgerRun),
            "unregistered-run" => Ok(PermissionAttribution::UnregisteredRun),
            "unknown" => Ok(PermissionAttribution::Unknown),
            other => crate::run_scope::Unattributed::ALL
                .iter()
                .find(|reason| reason.code() == other)
                .map(|reason| PermissionAttribution::Unattributed(*reason))
                .ok_or_else(|| {
                    LedgerError::Corrupt(format!("unknown permission attribution '{other}'"))
                }),
        }
    }

    /// True when this attribution requires a run id, which is what the table's
    /// CHECK enforces. Kept next to [`code`](Self::code) so the two cannot drift.
    #[must_use]
    fn names_a_run(self) -> bool {
        matches!(
            self,
            PermissionAttribution::LedgerRun | PermissionAttribution::UnregisteredRun
        )
    }
}

/// Serialized as its [`code`](PermissionAttribution::code), so a machine-readable
/// trail carries the same token the database does.
impl Serialize for PermissionAttribution {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.code())
    }
}

/// A permission request, at the moment it was raised.
///
/// Everything here is fixed by the act of asking, which is why the table's
/// update trigger refuses to let any of it change afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequestRecord {
    pub request_id: String,
    pub run_id: Option<String>,
    pub attribution: PermissionAttribution,
    pub process_id: Option<String>,
    pub tool_name: String,
    pub tool_call_id: String,
    /// Whether [`Self::tool_call_id`] joins to anything. Read it before
    /// concluding a tool call had no decision — see [`ToolCallOrigin`].
    pub tool_call_origin: ToolCallOrigin,
    pub operation_sha256: String,
    /// The permission mode in force, after any turn-scoped override. Recorded
    /// because "why did this not prompt" is unanswerable without it.
    pub mode: String,
    pub risk_level: Option<RiskLevel>,
    /// Whether the risk level came from the deterministic path floor rather than
    /// a classifier — the difference between a fact and an advisory opinion.
    pub risk_floored: bool,
    pub requested_at_ms: u64,
    pub expires_at_ms: u64,
}

/// One tool call in a run with no permission decision behind it (roadmap K12).
///
/// `mutation` is `None` when the run recorded a `ToolStarted` with no matching
/// `ToolProposed` — the log does not say whether that call could change
/// anything, and defaulting it to "read-only" is how an ungated write would slip
/// past the check that exists to catch it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionGap {
    pub tool_call_id: String,
    pub tool_name: Option<String>,
    pub mutation: Option<bool>,
}

impl PermissionGap {
    /// Whether this gap is the bug the acceptance names, as opposed to an
    /// ordinary ungated read. Unknown counts as a bug: see `mutation`.
    #[must_use]
    pub fn is_unauthorized_mutation(&self) -> bool {
        self.mutation.unwrap_or(true)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredPermissionDecision {
    pub request: PermissionRequestRecord,
    /// `None` while the request is still open. Every terminal path in
    /// `permissions.rs` fills this, including expiry and "no window to ask".
    pub decision: Option<PermissionDecision>,
    /// Who decided, as the same `engine:`/`user:` identity string the run events
    /// carry.
    pub decided_by: Option<String>,
    pub decided_at_ms: Option<u64>,
}

/// Which subsystem produced an event. A closed set with stable codes, for the
/// reason [`crate::run_scope::Unattributed`] is one: the code is what gets
/// persisted and compared, so it has to outlive this enum's spelling.
///
/// The list is the acceptance's own list minus the two that already write to
/// `run_events` — desktop and daemon both go through `RunLedger::append_event`
/// and have a run to hang off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subsystem {
    Http,
    Mcp,
    Browser,
    Acp,
    Remote,
    /// Agent worktree lifecycle (`agent_worktrees.rs`): create/remove/apply
    /// are run-less filesystem+git mutations, exactly the class of action
    /// this stream exists for.
    Worktree,
}

impl Subsystem {
    pub const ALL: &'static [Subsystem] = &[
        Subsystem::Http,
        Subsystem::Mcp,
        Subsystem::Browser,
        Subsystem::Acp,
        Subsystem::Remote,
        Subsystem::Worktree,
    ];

    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Subsystem::Http => "http",
            Subsystem::Mcp => "mcp",
            Subsystem::Browser => "browser",
            Subsystem::Acp => "acp",
            Subsystem::Remote => "remote",
            Subsystem::Worktree => "worktree",
        }
    }

    fn parse(value: &str) -> LedgerResult<Self> {
        Subsystem::ALL
            .iter()
            .find(|subsystem| subsystem.code() == value)
            .copied()
            .ok_or_else(|| LedgerError::Corrupt(format!("unknown subsystem '{value}'")))
    }
}

impl Serialize for Subsystem {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.code())
    }
}

/// How a subsystem action ended.
///
/// `Denied` is kept apart from `Failed` deliberately: a call the permission gate
/// refused and a call that ran and errored are different findings, and a reader
/// counting failures should not be counting refusals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubsystemOutcome {
    Succeeded,
    Failed,
    Denied,
    Cancelled,
}

impl SubsystemOutcome {
    pub const ALL: &'static [SubsystemOutcome] = &[
        SubsystemOutcome::Succeeded,
        SubsystemOutcome::Failed,
        SubsystemOutcome::Denied,
        SubsystemOutcome::Cancelled,
    ];

    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            SubsystemOutcome::Succeeded => "succeeded",
            SubsystemOutcome::Failed => "failed",
            SubsystemOutcome::Denied => "denied",
            SubsystemOutcome::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> LedgerResult<Self> {
        SubsystemOutcome::ALL
            .iter()
            .find(|outcome| outcome.code() == value)
            .copied()
            .ok_or_else(|| LedgerError::Corrupt(format!("unknown subsystem outcome '{value}'")))
    }
}

impl Serialize for SubsystemOutcome {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.code())
    }
}

/// One thing a run-less subsystem did.
///
/// Written once, when the action has an outcome. There is deliberately no
/// "started" row: the action's authorization is already recorded in
/// `permission_decisions` *before* it runs, so a second row would restate what
/// the permission row already proves. An action that never finishes therefore
/// leaves an open permission and no event, which reads correctly as "authorized,
/// never completed".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubsystemEvent {
    pub event_id: String,
    pub subsystem: Subsystem,
    /// What was done, in the subsystem's own vocabulary — an MCP tool name, an
    /// HTTP route, a browser action.
    pub action: String,
    pub occurred_at_ms: u64,
    pub run_id: Option<String>,
    pub attribution: PermissionAttribution,
    pub process_id: Option<String>,
    /// The `permission_decisions` row that authorized this. `None` means nothing
    /// gated it — a finding, not a blank.
    pub permission_request_id: Option<String>,
    pub outcome: SubsystemOutcome,
    /// Subsystem-specific detail. Whatever goes here is covered by the chain, so
    /// it cannot be edited after the fact.
    pub detail_json: Option<Vec<u8>>,
}

/// A stored subsystem event with the sequence the stream assigned it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredSubsystemEvent {
    pub sequence: u64,
    pub event_id: String,
    pub subsystem: Subsystem,
    pub action: String,
    pub occurred_at_ms: u64,
    pub run_id: Option<String>,
    pub attribution: PermissionAttribution,
    pub process_id: Option<String>,
    pub permission_request_id: Option<String>,
    pub outcome: SubsystemOutcome,
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

    /// The process kinds the stored `CHECK` constraint will accept.
    ///
    /// Read out of the table's own DDL rather than restated, so it is genuinely
    /// the database's answer and not a second copy of the Rust list. Exists for
    /// the invariant that `ProcessKind::ALL` and the storage vocabulary name the
    /// same set — a kind in one and not the other is either unstorable or
    /// silently exempt from every per-kind check.
    pub fn stored_process_kinds(&self) -> Result<std::collections::BTreeSet<String>, LedgerError> {
        let ddl: String = self.connection.query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'agent_processes'",
            [],
            |row| row.get(0),
        )?;
        // The vocabulary is the quoted list inside `kind TEXT NOT NULL CHECK
        // (kind IN ( … ))`. Scanning for quoted words after that marker is enough
        // and stays right if the list is re-wrapped, which a rebuild migration
        // does every time.
        let start = ddl
            .find("kind IN (")
            .ok_or_else(|| LedgerError::Corrupt("agent_processes has no kind CHECK".into()))?;
        let end = ddl[start..]
            .find(')')
            .map(|offset| start + offset)
            .ok_or_else(|| LedgerError::Corrupt("agent_processes kind CHECK is unclosed".into()))?;
        Ok(ddl[start..end]
            .split('\'')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect())
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
        // Read inside the same transaction that just pinned `sequence` to
        // `last_sequence + 1`, so the row this links to cannot change underneath
        // it. `None` means the predecessor predates V9 (or there is none), which
        // starts a new covered range rather than being an error.
        let previous_hash = transaction
            .query_row(
                "SELECT event_hash FROM run_events WHERE run_id = ?1 AND sequence = ?2",
                params![envelope.run_id, sequence - 1],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        // Read from the ambient scope rather than threaded through a parameter:
        // every one of the 46 production call sites funnels through here, so this
        // is the single place that can name the process without touching any of
        // them. `None` — no scope, or a scope with no process — is recorded as
        // NULL, which honestly means "this event names no process" rather than
        // guessing one from the run. It cannot be recovered by joining later:
        // `agent_processes.run_id` is not unique, because a run legitimately owns
        // many processes.
        let process_id =
            crate::run_scope::current_process().map(|process| process.process_id().to_string());
        let event_hash = event_chain_hash(
            previous_hash.as_deref(),
            &envelope.event_id,
            &envelope.run_id,
            envelope.sequence,
            occurred_at_ms,
            envelope.actor_id.as_deref(),
            effects.event_type,
            &emitter_json,
            &envelope_json,
            derived_status,
            effects.terminal,
            process_id.as_deref(),
        );
        transaction.execute(
            "INSERT INTO run_events (
                event_id, run_id, sequence, occurred_at_ms, actor_id,
                emitter_json, event_type, envelope_json, derived_status, is_terminal,
                event_hash, prev_event_hash, process_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
                i64::from(effects.terminal),
                event_hash,
                previous_hash,
                process_id
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

    /// Recomputes one run's event chain and reports what it can and cannot
    /// vouch for.
    ///
    /// Detects, within the covered range: any edited column of any event, a
    /// deleted interior event, and a reordering. Detects a **truncated tail**
    /// too, but by a second route — `runs.last_sequence` is maintained by the
    /// `run_events_project_run` trigger, so removing the newest events leaves it
    /// claiming more events than exist, and hiding that requires a second,
    /// separate edit to a different table.
    ///
    /// Cannot detect: removal of the entire covered range, since a per-run chain
    /// has no anchor outside the database it lives in. Saying so is the point —
    /// an integrity claim that overstates itself is worse than none.
    /// Every tool call in `run_id` whose authorizing permission decision cannot
    /// be produced from the log (roadmap K12).
    ///
    /// # Why this exists when `permission_decisions_for_tool_call` already does
    ///
    /// The acceptance's sentence is "a tool call whose authorizing decision
    /// cannot be produced from the log is a bug". Answering it one
    /// `tool_call_id` at a time can only confirm a call somebody already
    /// suspected; a bug nobody suspected is exactly the one that stays. This
    /// asks the question of every call in a run at once, which is what turns
    /// the sentence into something a check can fail on.
    ///
    /// # Mutation is the line, and it is read from the log rather than assumed
    ///
    /// Not every tool call is gated, and that is correct: reading a file is not
    /// an authorization event. So an ungated read-only call is reported and not
    /// counted against the run, while an ungated **mutating** call is the bug —
    /// and `mutation` comes from the run's own `ToolProposed` event rather than
    /// from a tool-name list here, which would be a second opinion about a fact
    /// the log already records.
    ///
    /// A call with a `ToolStarted` and no `ToolProposed` is reported as
    /// mutation-unknown rather than assumed harmless: the log genuinely does not
    /// say, and guessing "read-only" is how an ungated write gets skipped.
    pub fn permission_gaps(&self, run_id: &str) -> LedgerResult<Vec<PermissionGap>> {
        if load_run_from(&self.connection, run_id)?.is_none() {
            return Err(LedgerError::NotFound {
                entity: "run",
                id: run_id.to_string(),
            });
        }
        let mut statement = self.connection.prepare(
            "SELECT envelope_json FROM run_events WHERE run_id = ?1 ORDER BY sequence ASC",
        )?;
        let mut rows = statement.query([run_id])?;

        // Insertion-ordered so the report follows the run rather than a hash.
        let mut order: Vec<String> = Vec::new();
        let mut proposed: std::collections::HashMap<String, (String, bool)> =
            std::collections::HashMap::new();
        let mut started: Vec<String> = Vec::new();
        while let Some(row) = rows.next()? {
            let bytes: Vec<u8> = row.get(0)?;
            let Ok(envelope) = serde_json::from_slice::<RunEventEnvelope>(&bytes) else {
                // A row this binary cannot parse is a finding for
                // `verify_run_chain`, not for this pass — reporting it here as a
                // permission gap would blame the wrong thing.
                continue;
            };
            match envelope.event {
                RunEvent::ToolProposed {
                    tool_call_id,
                    tool_name,
                    mutation,
                    ..
                } => {
                    if !proposed.contains_key(&tool_call_id) {
                        order.push(tool_call_id.clone());
                    }
                    proposed.insert(tool_call_id, (tool_name, mutation));
                }
                RunEvent::ToolStarted { tool_call_id } => started.push(tool_call_id),
                _ => {}
            }
        }
        for tool_call_id in started {
            if !proposed.contains_key(&tool_call_id) && !order.contains(&tool_call_id) {
                order.push(tool_call_id);
            }
        }

        let mut gaps = Vec::new();
        for tool_call_id in order {
            if !self
                .permission_decisions_for_tool_call(&tool_call_id)?
                .is_empty()
            {
                continue;
            }
            let (tool_name, mutation) = match proposed.get(&tool_call_id) {
                Some((name, mutation)) => (Some(name.clone()), Some(*mutation)),
                None => (None, None),
            };
            gaps.push(PermissionGap {
                tool_call_id,
                tool_name,
                mutation,
            });
        }
        Ok(gaps)
    }

    pub fn verify_run_chain(&self, run_id: &str) -> LedgerResult<ChainVerification> {
        let claimed_last_sequence = self
            .connection
            .query_row(
                "SELECT last_sequence FROM runs WHERE run_id = ?1",
                [run_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .ok_or_else(|| LedgerError::NotFound {
                entity: "run",
                id: run_id.to_string(),
            })?;
        let claimed_last_sequence = from_sql_u64(claimed_last_sequence, "last_sequence")?;

        let mut statement = self.connection.prepare(
            "SELECT event_id, sequence, occurred_at_ms, actor_id, emitter_json, event_type,
                    envelope_json, derived_status, is_terminal, event_hash, prev_event_hash,
                    process_id
             FROM run_events WHERE run_id = ?1 ORDER BY sequence ASC",
        )?;
        let mut rows = statement.query([run_id])?;

        let mut covered_from = None;
        let mut covered_through = None;
        let mut events_seen = 0u64;
        let mut events_naming_a_process = 0u64;
        let mut expected_previous: Option<String> = None;
        while let Some(row) = rows.next()? {
            events_seen += 1;
            if row.get::<_, Option<String>>(11)?.is_some() {
                events_naming_a_process += 1;
            }
            let sequence = from_sql_u64(row.get::<_, i64>(1)?, "sequence")?;
            let stored_hash = row.get::<_, Option<String>>(9)?;
            let stored_previous = row.get::<_, Option<String>>(10)?;
            let Some(stored_hash) = stored_hash else {
                // Predates V9. Anything after it starts a fresh covered range;
                // an unchained row appearing *inside* one is a broken link,
                // which the `expected_previous` check below catches.
                if covered_from.is_some() {
                    return Ok(ChainVerification::Broken {
                        sequence,
                        detail: "an event inside the covered range carries no hash".to_string(),
                    });
                }
                continue;
            };
            if let Some(expected) = &expected_previous {
                if stored_previous.as_deref() != Some(expected.as_str()) {
                    return Ok(ChainVerification::Broken {
                        sequence,
                        detail: "this event does not link to the previous event's hash".to_string(),
                    });
                }
            }
            let recomputed = event_chain_hash(
                stored_previous.as_deref(),
                &row.get::<_, String>(0)?,
                run_id,
                sequence,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?.as_deref(),
                &row.get::<_, String>(5)?,
                &row.get::<_, Vec<u8>>(4)?,
                &row.get::<_, Vec<u8>>(6)?,
                row.get::<_, Option<String>>(7)?.as_deref(),
                row.get::<_, i64>(8)? != 0,
                row.get::<_, Option<String>>(11)?.as_deref(),
            );
            if recomputed != stored_hash {
                return Ok(ChainVerification::Broken {
                    sequence,
                    detail: "this event's contents do not match its recorded hash".to_string(),
                });
            }
            covered_from.get_or_insert(sequence);
            covered_through = Some(sequence);
            expected_previous = Some(stored_hash);
        }

        if events_seen < claimed_last_sequence {
            return Ok(ChainVerification::Broken {
                sequence: claimed_last_sequence,
                detail: format!(
                    "the run projects {claimed_last_sequence} events but only {events_seen} are \
                     stored, so events were removed"
                ),
            });
        }

        Ok(ChainVerification::Intact {
            covered_from,
            covered_through,
            events_seen,
            events_naming_a_process,
        })
    }

    pub fn load_run(&self, run_id: &str) -> LedgerResult<Option<StoredRun>> {
        load_run_from(&self.connection, run_id)
    }

    /// The origin's chain tip for a run whose newest event is a departure
    /// (roadmap K18), or `None` when this run never left.
    ///
    /// Reads the *newest* event rather than searching for any departure on
    /// purpose: a run that departed, was refused, and carried on locally has a
    /// departure in its history that no longer describes where it is. The tip
    /// is the only departure a join may be built from, because the tip's hash is
    /// the only one the far side could have named.
    pub fn migration_departure(&self, run_id: &str) -> LedgerResult<Option<MigrationDeparture>> {
        let row = self
            .connection
            .query_row(
                "SELECT sequence, envelope_json, event_hash FROM run_events
                 WHERE run_id = ?1 ORDER BY sequence DESC LIMIT 1",
                [run_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((sequence, envelope_json, event_hash)) = row else {
            return Ok(None);
        };
        let envelope: RunEventEnvelope = serde_json::from_slice(&envelope_json)?;
        let RunEvent::MigrationDeparted {
            target_node_id,
            payload_sha256,
            checkpoint_id,
        } = envelope.event
        else {
            return Ok(None);
        };
        // An unchained departure cannot anchor a join: there would be nothing
        // for the arrival to name. Reported as absent rather than as a
        // departure with an empty hash, which a caller could pass on as if it
        // linked something.
        let Some(event_hash) = event_hash else {
            return Ok(None);
        };
        Ok(Some(MigrationDeparture {
            run_id: run_id.to_string(),
            sequence: from_sql_u64(sequence, "sequence")?,
            event_hash,
            target_node_id,
            payload_sha256,
            checkpoint_id,
        }))
    }

    /// The target's half of a migrated run, read from its first event.
    ///
    /// Sequence 1 and nowhere else: an arrival that is not the first event of
    /// the local chain would mean this node was already running the run before
    /// it was handed over, which is a different (and unsound) history.
    pub fn migration_arrival(&self, run_id: &str) -> LedgerResult<Option<MigrationArrival>> {
        let row = self
            .connection
            .query_row(
                "SELECT envelope_json, event_hash FROM run_events
                 WHERE run_id = ?1 AND sequence = 1",
                [run_id],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        let Some((envelope_json, event_hash)) = row else {
            return Ok(None);
        };
        let envelope: RunEventEnvelope = serde_json::from_slice(&envelope_json)?;
        let RunEvent::MigrationArrived {
            origin_node_id,
            origin_last_sequence,
            origin_last_event_hash,
            payload_sha256,
        } = envelope.event
        else {
            return Ok(None);
        };
        let Some(event_hash) = event_hash else {
            return Ok(None);
        };
        Ok(Some(MigrationArrival {
            run_id: run_id.to_string(),
            origin_node_id,
            origin_last_sequence,
            origin_last_event_hash,
            payload_sha256,
            event_hash,
        }))
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
        let archived = load_run_from(&transaction, run_id)?
            .ok_or_else(|| LedgerError::Corrupt(format!("archived run '{run_id}' disappeared")))?;
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

    /// Record a permission request, whatever it belongs to.
    ///
    /// Unlike the `approvals` table this has no run precondition, which is the
    /// point — see [`MIGRATION_V11_SQL`]. Calling it twice for one `request_id`
    /// is a caller bug and fails on the primary key rather than overwriting what
    /// was asked.
    pub fn record_permission_request(&self, record: &PermissionRequestRecord) -> LedgerResult<()> {
        if record.attribution.names_a_run() != record.run_id.is_some() {
            return Err(LedgerError::Corrupt(format!(
                "attribution '{}' and run id disagree about whether this permission has a run",
                record.attribution.code()
            )));
        }
        self.connection.execute(
            "INSERT INTO permission_decisions (
                request_id, run_id, attribution, process_id, tool_name, tool_call_id,
                tool_call_origin, operation_sha256, mode, risk_level, risk_floored,
                requested_at_ms, expires_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                record.request_id,
                record.run_id,
                record.attribution.code(),
                record.process_id,
                record.tool_name,
                record.tool_call_id,
                record.tool_call_origin.code(),
                record.operation_sha256,
                record.mode,
                record.risk_level.as_ref().map(enum_token).transpose()?,
                i64::from(record.risk_floored),
                to_sql_i64(record.requested_at_ms, "requested_at_ms")?,
                to_sql_i64(record.expires_at_ms, "expires_at_ms")?,
            ],
        )?;
        Ok(())
    }

    /// Close out a recorded request. The `permission_decisions_decide_once`
    /// trigger rejects a second decision, so this reports one rather than
    /// quietly winning.
    pub fn record_permission_decision(
        &self,
        request_id: &str,
        decision: PermissionDecision,
        decided_by: &str,
        decided_at_ms: u64,
    ) -> LedgerResult<()> {
        let changed = self.connection.execute(
            "UPDATE permission_decisions
             SET decision = ?2, decided_by = ?3, decided_at_ms = ?4
             WHERE request_id = ?1",
            params![
                request_id,
                enum_token(&decision)?,
                decided_by,
                to_sql_i64(decided_at_ms, "decided_at_ms")?,
            ],
        )?;
        if changed == 0 {
            return Err(LedgerError::NotFound {
                entity: "permission request",
                id: request_id.to_string(),
            });
        }
        Ok(())
    }

    pub fn load_permission_decision(
        &self,
        request_id: &str,
    ) -> LedgerResult<Option<StoredPermissionDecision>> {
        let mut statement = self.connection.prepare(&format!(
            "{PERMISSION_DECISION_SELECT} WHERE request_id = ?1"
        ))?;
        let row = statement
            .query_map([request_id], permission_decision_columns)?
            .next()
            .transpose()?;
        row.map(decode_permission_decision).transpose()
    }

    /// Every permission decision recorded for a tool call, oldest first.
    ///
    /// This is the query the acceptance asks for: given a tool call, produce the
    /// decision that authorized it. An empty answer means nothing gated the
    /// call, which is a finding rather than an absence.
    pub fn permission_decisions_for_tool_call(
        &self,
        tool_call_id: &str,
    ) -> LedgerResult<Vec<StoredPermissionDecision>> {
        let mut statement = self.connection.prepare(&format!(
            "{PERMISSION_DECISION_SELECT} WHERE tool_call_id = ?1 ORDER BY requested_at_ms, request_id"
        ))?;
        let rows = statement
            .query_map([tool_call_id], permission_decision_columns)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(decode_permission_decision).collect()
    }

    /// Append one subsystem event and return the sequence the stream gave it.
    ///
    /// The tail read and the insert share one transaction, so two concurrent
    /// appends cannot both chain off the same predecessor — the second waits
    /// and links to the first. Without that, the chain would fork and one branch
    /// would be silently lost to the `UNIQUE` sequence.
    pub fn append_subsystem_event(&mut self, event: &SubsystemEvent) -> LedgerResult<u64> {
        if event.attribution.names_a_run() != event.run_id.is_some() {
            return Err(LedgerError::Corrupt(format!(
                "attribution '{}' and run id disagree about whether this event has a run",
                event.attribution.code()
            )));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<String> = transaction
            .query_row(
                "SELECT event_hash FROM subsystem_events ORDER BY sequence DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let occurred_at_ms = to_sql_i64(event.occurred_at_ms, "occurred_at_ms")?;
        let hash = subsystem_chain_hash(
            previous.as_deref(),
            &event.event_id,
            event.subsystem.code(),
            &event.action,
            occurred_at_ms,
            event.run_id.as_deref(),
            event.attribution.code(),
            event.process_id.as_deref(),
            event.permission_request_id.as_deref(),
            event.outcome.code(),
            event.detail_json.as_deref(),
        );
        transaction.execute(
            "INSERT INTO subsystem_events (
                event_id, subsystem, action, occurred_at_ms, run_id, attribution,
                process_id, permission_request_id, outcome, detail_json,
                event_hash, prev_event_hash
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                event.event_id,
                event.subsystem.code(),
                event.action,
                occurred_at_ms,
                event.run_id,
                event.attribution.code(),
                event.process_id,
                event.permission_request_id,
                event.outcome.code(),
                event.detail_json,
                hash,
                previous,
            ],
        )?;
        let sequence = transaction.last_insert_rowid();
        transaction.commit()?;
        from_sql_u64(sequence, "sequence")
    }

    /// Recompute the subsystem chain and report whether it is intact.
    ///
    /// Unlike [`verify_run_chain`](Self::verify_run_chain) there is no unchained
    /// era to skip: this table was born chained, so a row without a hash is a
    /// corruption rather than history. Tail truncation is the one thing a chain
    /// cannot see on its own, and here — unlike `run_events`, where
    /// `runs.last_sequence` is a second witness — there is no counter to
    /// contradict it. That limit is stated rather than glossed; an integrity
    /// claim that overstates itself is worse than none.
    pub fn verify_subsystem_chain(&self) -> LedgerResult<ChainVerification> {
        let mut statement = self.connection.prepare(
            "SELECT sequence, event_id, subsystem, action, occurred_at_ms, run_id, attribution,
                    process_id, permission_request_id, outcome, detail_json,
                    event_hash, prev_event_hash
             FROM subsystem_events ORDER BY sequence ASC",
        )?;
        let mut rows = statement.query([])?;

        let mut covered_from = None;
        let mut covered_through = None;
        let mut events_seen = 0u64;
        let mut events_naming_a_process = 0u64;
        let mut expected_previous: Option<String> = None;
        while let Some(row) = rows.next()? {
            events_seen += 1;
            let sequence = from_sql_u64(row.get::<_, i64>(0)?, "sequence")?;
            let process_id = row.get::<_, Option<String>>(7)?;
            if process_id.is_some() {
                events_naming_a_process += 1;
            }
            let stored_hash = row.get::<_, String>(11)?;
            let stored_previous = row.get::<_, Option<String>>(12)?;
            if stored_previous.as_deref() != expected_previous.as_deref() {
                return Ok(ChainVerification::Broken {
                    sequence,
                    detail: "this event does not link to the previous event's hash".to_string(),
                });
            }
            let recomputed = subsystem_chain_hash(
                stored_previous.as_deref(),
                &row.get::<_, String>(1)?,
                &row.get::<_, String>(2)?,
                &row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<String>>(5)?.as_deref(),
                &row.get::<_, String>(6)?,
                process_id.as_deref(),
                row.get::<_, Option<String>>(8)?.as_deref(),
                &row.get::<_, String>(9)?,
                row.get::<_, Option<Vec<u8>>>(10)?.as_deref(),
            );
            if recomputed != stored_hash {
                return Ok(ChainVerification::Broken {
                    sequence,
                    detail: "this event's contents do not match its recorded hash".to_string(),
                });
            }
            covered_from.get_or_insert(sequence);
            covered_through = Some(sequence);
            expected_previous = Some(stored_hash);
        }

        Ok(ChainVerification::Intact {
            covered_from,
            covered_through,
            events_seen,
            events_naming_a_process,
        })
    }

    /// Subsystem events in stream order, newest first.
    pub fn recent_subsystem_events(
        &self,
        subsystem: Option<Subsystem>,
        limit: u32,
    ) -> LedgerResult<Vec<StoredSubsystemEvent>> {
        let mut statement = self.connection.prepare(
            "SELECT sequence, event_id, subsystem, action, occurred_at_ms, run_id, attribution,
                    process_id, permission_request_id, outcome
             FROM subsystem_events
             WHERE ?1 IS NULL OR subsystem = ?1
             ORDER BY sequence DESC LIMIT ?2",
        )?;
        let rows = statement
            .query_map(
                params![subsystem.map(Subsystem::code), limit],
                subsystem_event_columns,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(decode_subsystem_event).collect()
    }

    /// The newest link in the subsystem chain, hashes included.
    ///
    /// `recent_subsystem_events` deliberately does not return hashes — the
    /// panel that reads it shows what happened, not what vouches for it. The
    /// K21 conformance suite needs the opposite: only the linkage, never the
    /// contents, because a hash is safe to hand a caller and a `detail_json`
    /// holding the user's own text is not.
    pub fn subsystem_chain_head(&self) -> LedgerResult<Option<ChainLink>> {
        self.connection
            .query_row(
                "SELECT sequence, event_hash, prev_event_hash
                 FROM subsystem_events ORDER BY sequence DESC LIMIT 1",
                [],
                chain_link_columns,
            )
            .optional()?
            .map(decode_chain_link)
            .transpose()
    }

    /// Chain links after `after_sequence`, oldest first.
    ///
    /// A caller that saw an earlier head can ask for what followed it and
    /// recompute the linkage itself: the first returned link's
    /// `previous_hash` must equal the head it remembers, or the log was
    /// rewritten between the two reads.
    pub fn subsystem_chain_links(
        &self,
        after_sequence: u64,
        limit: u32,
    ) -> LedgerResult<Vec<ChainLink>> {
        let after = to_sql_i64(after_sequence, "after_sequence")?;
        let mut statement = self.connection.prepare(
            "SELECT sequence, event_hash, prev_event_hash
             FROM subsystem_events WHERE sequence > ?1
             ORDER BY sequence ASC LIMIT ?2",
        )?;
        let rows = statement
            .query_map(params![after, limit], chain_link_columns)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(decode_chain_link).collect()
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

/// Every migration, with the checksum that pins its SQL and whether it keeps
/// older binaries able to open the database.
///
/// **V1–V8 are marked breaking without re-deriving each one, and that is
/// deliberate.** They already shipped under the old blanket rule, so calling
/// them breaking preserves exactly today's behaviour and claims nothing new.
/// Claiming compatibility wrongly is far worse than claiming it too little: it
/// would hand an older binary a database it corrupts. Compatibility is asserted
/// only where it has been checked.
///
/// V9 is genuinely breaking. `run_events_chain_must_not_stop` aborts an insert
/// whose `event_hash` is NULL into a run that is already chained — which is
/// every insert a pre-V9 binary makes.
///
/// V10–V12 are additive, and each was checked:
///
/// - **V10** adds a nullable `run_events.process_id` and a partial index. A V9
///   binary's inserts omit it, and V9's hash deliberately contributes nothing
///   for an absent process id, so rows it writes still verify.
/// - **V11** adds `permission_decisions`, a table no older binary queries, and
///   triggers that fire only on it.
/// - **V12** adds `subsystem_events` on the same terms.
///
/// V13 must require itself: a V12 binary applies the *old* blanket guard, so it
/// refuses a V13 database no matter what this column says. The fix cannot reach
/// backwards — it stops the bleeding from here on.
const MIGRATION_LADDER: &[(i64, &str, Compatibility)] = &[
    (
        MIGRATION_V1,
        MIGRATION_V1_CHECKSUM,
        Compatibility::RequiresThisVersion,
    ),
    (
        MIGRATION_V2,
        MIGRATION_V2_CHECKSUM,
        Compatibility::RequiresThisVersion,
    ),
    (
        MIGRATION_V3,
        MIGRATION_V3_CHECKSUM,
        Compatibility::RequiresThisVersion,
    ),
    (
        MIGRATION_V4,
        MIGRATION_V4_CHECKSUM,
        Compatibility::RequiresThisVersion,
    ),
    (
        MIGRATION_V5,
        MIGRATION_V5_CHECKSUM,
        Compatibility::RequiresThisVersion,
    ),
    (
        MIGRATION_V6,
        MIGRATION_V6_CHECKSUM,
        Compatibility::RequiresThisVersion,
    ),
    (
        MIGRATION_V7,
        MIGRATION_V7_CHECKSUM,
        Compatibility::RequiresThisVersion,
    ),
    (
        MIGRATION_V8,
        MIGRATION_V8_CHECKSUM,
        Compatibility::RequiresThisVersion,
    ),
    (
        MIGRATION_V9,
        MIGRATION_V9_CHECKSUM,
        Compatibility::RequiresThisVersion,
    ),
    (
        MIGRATION_V10,
        MIGRATION_V10_CHECKSUM,
        Compatibility::Additive,
    ),
    (
        MIGRATION_V11,
        MIGRATION_V11_CHECKSUM,
        Compatibility::Additive,
    ),
    (
        MIGRATION_V12,
        MIGRATION_V12_CHECKSUM,
        Compatibility::Additive,
    ),
    (
        MIGRATION_V13,
        MIGRATION_V13_CHECKSUM,
        Compatibility::RequiresThisVersion,
    ),
    // Additive, and the first migration to actually collect on what V13 bought:
    // a new table plus a nullable column, both of which a V13 binary simply never
    // looks at. Its writes stay correct — `add_egress_bytes` touches neither.
    (
        MIGRATION_V14,
        MIGRATION_V14_CHECKSUM,
        Compatibility::Additive,
    ),
    // Additive: one nullable-in-effect column with a default. A V14 binary
    // never selects it, and its inserts leave it at the `'unknown'` default —
    // which is exactly what those rows are, since that binary did not record
    // where the id came from.
    (
        MIGRATION_V15,
        MIGRATION_V15_CHECKSUM,
        Compatibility::Additive,
    ),
    // Additive: two nullable measurement columns, in the same family as V8's and
    // read by nothing older. A V15 binary keeps writing every other column
    // correctly and simply leaves these NULL, which is what they mean.
    (
        MIGRATION_V16,
        MIGRATION_V16_CHECKSUM,
        Compatibility::Additive,
    ),
    // Additive: one nullable limit column beside the four `max_*` columns V5
    // installed. A V16 binary never selects it and never sets it, which reads
    // correctly as "that binary enforced no context budget".
    (
        MIGRATION_V17,
        MIGRATION_V17_CHECKSUM,
        Compatibility::Additive,
    ),
    // Breaking, and this is the one migration where that is the *point* rather
    // than a cost. It widens `agent_processes.kind` to admit `browser_session`,
    // and a V17 binary's `ProcessKind::parse` rejects any kind it does not know
    // — so a V17 binary reading a database that now contains browser rows does
    // not degrade gracefully, it errors out of `list`/`get` for every caller.
    // Raising the floor is what stops that; an `Additive` claim here would be a
    // promise the older binary cannot keep.
    (
        MIGRATION_V18,
        MIGRATION_V18_CHECKSUM,
        Compatibility::RequiresThisVersion,
    ),
    // Additive in effect, and it inherits V18's floor rather than raising it. It
    // rebuilds `egress_destinations` to *relax* a constraint — `process_id`
    // becomes nullable beside a new `unattributed_reason` — so a V18 binary's
    // reads (`egress_destinations_for`, which filters `WHERE process_id IN (…)`)
    // return exactly the attributed rows they always did, and its writes still
    // name a process, which the widened `CHECK` still accepts. The unattributed
    // rows are simply invisible to it, which is what they were before this
    // migration existed.
    (
        MIGRATION_V19,
        MIGRATION_V19_CHECKSUM,
        Compatibility::Additive,
    ),
    // Breaking for exactly V18's reason, one table over: it widens
    // `subsystem_events.subsystem` to admit `worktree` (agent-worktree
    // create/remove/apply — see `agent_worktrees.rs`), and an older binary's
    // `Subsystem::parse` errors on any code it does not know, so a database
    // containing worktree rows would fail that binary's every chain read.
    (
        MIGRATION_V20,
        MIGRATION_V20_CHECKSUM,
        Compatibility::RequiresThisVersion,
    ),
    // Breaking for V18's reason again: it widens `agent_processes.kind` to admit
    // `foreground_shell`, and a V20 binary's `ProcessKind::parse` rejects a kind
    // it does not know — so every `list`/`get` on a database containing an agent
    // shell's rows would error rather than degrade. The typed limit-breach
    // columns it adds beside that are additive on their own; the kind is what
    // raises the floor.
    (
        MIGRATION_V21,
        MIGRATION_V21_CHECKSUM,
        Compatibility::RequiresThisVersion,
    ),
    // One nullable column beside `native_pid`, so a row names a *process* rather
    // than a slot in the pid space. Additive in the strict sense an older binary
    // cares about: it adds no vocabulary and widens no `CHECK`, so a V21 reader
    // opens the database and simply never selects the column.
    (
        MIGRATION_V22,
        MIGRATION_V22_CHECKSUM,
        Compatibility::Additive,
    ),
];

/// The oldest binary that may open a database with `version` applied.
///
/// An additive migration inherits the floor from the last breaking one before
/// it; a breaking migration raises the floor to itself.
fn min_reader_version_for(version: i64) -> i64 {
    let mut floor = MIGRATION_V1;
    for (candidate, _, compatibility) in MIGRATION_LADDER {
        if *candidate > version {
            break;
        }
        if matches!(compatibility, Compatibility::RequiresThisVersion) {
            floor = *candidate;
        }
    }
    floor
}

/// What the database itself says the oldest safe reader is.
///
/// `None` for a database with no migrations yet. A pre-V13 database has no
/// column to answer with, so its `MAX(version)` is the answer — which is exactly
/// the old rule, and the honest one for rows written before the floor existed.
fn required_reader_version(connection: &Connection) -> LedgerResult<Option<i64>> {
    let has_column = connection
        .prepare("SELECT min_reader_version FROM schema_migrations LIMIT 0")
        .is_ok();
    let column = if has_column {
        "MAX(min_reader_version)"
    } else {
        "MAX(version)"
    };
    Ok(connection.query_row(
        &format!("SELECT {column} FROM schema_migrations"),
        [],
        |row| row.get::<_, Option<i64>>(0),
    )?)
}

fn apply_migrations(connection: &mut Connection) -> LedgerResult<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            checksum TEXT NOT NULL,
            applied_at_ms INTEGER NOT NULL CHECK (applied_at_ms > 0)
         ) STRICT;",
    )?;

    // Forward-only, but only as far as compatibility actually demands.
    //
    // The old rule was `MAX(version) > this binary's version -> refuse`, which
    // treated every schema bump as a one-way door: roll back one build and the
    // run history is unopenable. `MIGRATION_LADDER` records which migrations
    // genuinely reject an older binary's writes, so the floor is the newest
    // *breaking* version rather than the newest version, and an additive
    // migration costs a rollback nothing.
    //
    // Read from the database rather than recomputed here on purpose: a database
    // written by a future build carries its own floor, and that is the only
    // number that can say whether this binary may touch it.
    if let Some(required) = required_reader_version(connection)? {
        if required > SCHEMA_VERSION {
            return Err(LedgerError::MigrationConflict { version: required });
        }
    }

    for (version, expected, _) in MIGRATION_LADDER {
        if let Some(checksum) = connection
            .query_row(
                "SELECT checksum FROM schema_migrations WHERE version = ?1",
                [*version],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            if checksum != *expected {
                return Err(LedgerError::MigrationConflict { version: *version });
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

    let has_v6 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V6],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v6 {
        transaction.execute_batch(MIGRATION_V6_SQL)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms)
             VALUES (?1, ?2, ?3)",
            params![MIGRATION_V6, MIGRATION_V6_CHECKSUM, now_ms_i64()?],
        )?;
    }

    let has_v7 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V7],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v7 {
        transaction.execute_batch(MIGRATION_V7_SQL)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms)
             VALUES (?1, ?2, ?3)",
            params![MIGRATION_V7, MIGRATION_V7_CHECKSUM, now_ms_i64()?],
        )?;
    }

    let has_v8 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V8],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v8 {
        transaction.execute_batch(MIGRATION_V8_SQL)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms)
             VALUES (?1, ?2, ?3)",
            params![MIGRATION_V8, MIGRATION_V8_CHECKSUM, now_ms_i64()?],
        )?;
    }

    let has_v9 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V9],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v9 {
        transaction.execute_batch(MIGRATION_V9_SQL)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms)
             VALUES (?1, ?2, ?3)",
            params![MIGRATION_V9, MIGRATION_V9_CHECKSUM, now_ms_i64()?],
        )?;
    }

    let has_v10 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V10],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v10 {
        transaction.execute_batch(MIGRATION_V10_SQL)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms)
             VALUES (?1, ?2, ?3)",
            params![MIGRATION_V10, MIGRATION_V10_CHECKSUM, now_ms_i64()?],
        )?;
    }

    let has_v11 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V11],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v11 {
        transaction.execute_batch(MIGRATION_V11_SQL)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms)
             VALUES (?1, ?2, ?3)",
            params![MIGRATION_V11, MIGRATION_V11_CHECKSUM, now_ms_i64()?],
        )?;
    }

    let has_v12 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V12],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v12 {
        transaction.execute_batch(MIGRATION_V12_SQL)?;
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms)
             VALUES (?1, ?2, ?3)",
            params![MIGRATION_V12, MIGRATION_V12_CHECKSUM, now_ms_i64()?],
        )?;
    }

    let has_v13 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V13],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v13 {
        // SQLite has no `ADD COLUMN IF NOT EXISTS`, and the column can outlive
        // its migration row if a database is ever repaired by hand — so ask
        // rather than assume, instead of failing an open on a duplicate column.
        let has_column = transaction
            .prepare("SELECT min_reader_version FROM schema_migrations LIMIT 0")
            .is_ok();
        if !has_column {
            transaction.execute_batch(MIGRATION_V13_SQL)?;
        }
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms, min_reader_version)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                MIGRATION_V13,
                MIGRATION_V13_CHECKSUM,
                now_ms_i64()?,
                min_reader_version_for(MIGRATION_V13)
            ],
        )?;
    }

    let has_v14 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V14],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v14 {
        // Probed separately, for V13's reason and one more of its own: SQLite
        // cannot `ALTER TABLE ... DROP COLUMN` a column that carries a `CHECK`,
        // so a database wound back to an earlier version — which is exactly what
        // the upgrade tests below construct — can genuinely have the column
        // without the table, or the table without the column.
        let has_table = transaction
            .prepare("SELECT 1 FROM egress_destinations LIMIT 0")
            .is_ok();
        if !has_table {
            transaction.execute_batch(MIGRATION_V14_SQL)?;
        }
        let has_column = transaction
            .prepare("SELECT egress_destinations_dropped FROM agent_processes LIMIT 0")
            .is_ok();
        if !has_column {
            transaction.execute_batch(MIGRATION_V14_COLUMN_SQL)?;
        }
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms, min_reader_version)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                MIGRATION_V14,
                MIGRATION_V14_CHECKSUM,
                now_ms_i64()?,
                min_reader_version_for(MIGRATION_V14)
            ],
        )?;
    }

    let has_v15 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V15],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v15 {
        // Probed like V13's and V14's, and for the same reason: SQLite cannot
        // drop a column carrying a `CHECK`, so a database wound back to an
        // earlier version keeps it.
        let has_column = transaction
            .prepare("SELECT tool_call_origin FROM permission_decisions LIMIT 0")
            .is_ok();
        if !has_column {
            transaction.execute_batch(MIGRATION_V15_SQL)?;
        }
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms, min_reader_version)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                MIGRATION_V15,
                MIGRATION_V15_CHECKSUM,
                now_ms_i64()?,
                min_reader_version_for(MIGRATION_V15)
            ],
        )?;
    }

    let has_v16 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V16],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v16 {
        // Probed like V14's and V15's: the `CHECK` on each column is what stops
        // SQLite from dropping it, so a database wound back below V16 keeps them.
        // Probed one at a time because a wind-back can leave either alone.
        let has_reused = transaction
            .prepare("SELECT context_tokens_reused FROM agent_processes LIMIT 0")
            .is_ok();
        if !has_reused {
            transaction.execute_batch(MIGRATION_V16_REUSED_SQL)?;
        }
        let has_evaluated = transaction
            .prepare("SELECT context_tokens_evaluated FROM agent_processes LIMIT 0")
            .is_ok();
        if !has_evaluated {
            transaction.execute_batch(MIGRATION_V16_EVALUATED_SQL)?;
        }
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms, min_reader_version)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                MIGRATION_V16,
                MIGRATION_V16_CHECKSUM,
                now_ms_i64()?,
                min_reader_version_for(MIGRATION_V16)
            ],
        )?;
    }

    let has_v17 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V17],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v17 {
        // Probed like V16's: the `CHECK` is what stops SQLite dropping it, so a
        // database wound back below V17 keeps the column.
        let has_column = transaction
            .prepare("SELECT max_context_tokens FROM agent_processes LIMIT 0")
            .is_ok();
        if !has_column {
            transaction.execute_batch(MIGRATION_V17_SQL)?;
        }
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms, min_reader_version)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                MIGRATION_V17,
                MIGRATION_V17_CHECKSUM,
                now_ms_i64()?,
                min_reader_version_for(MIGRATION_V17)
            ],
        )?;
    }

    let has_v18 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V18],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v18 {
        // Probed by asking the table itself whether it would accept the new
        // kind, rather than by a column check like V16's and V17's — this
        // migration adds no column, it widens a `CHECK`, so there is nothing for
        // `SELECT … LIMIT 0` to find. The probe runs the rebuild only when the
        // constraint is still the narrow one, which keeps it idempotent for a
        // database that already has V18's schema but lost its ledger row.
        let already_wide = transaction
            .query_row(
                "SELECT 1 FROM sqlite_master
                 WHERE type = 'table' AND name = 'agent_processes'
                   AND sql LIKE '%browser_session%'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !already_wide {
            transaction.execute_batch(MIGRATION_V18_SQL)?;
        }
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms, min_reader_version)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                MIGRATION_V18,
                MIGRATION_V18_CHECKSUM,
                now_ms_i64()?,
                min_reader_version_for(MIGRATION_V18)
            ],
        )?;
    }

    let has_v19 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V19],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v19 {
        // Probed on the new column rather than on the migration row, like V16's
        // and V17's, so a database that already carries the rebuilt shape is not
        // rebuilt a second time.
        let already_widened = transaction
            .prepare("SELECT unattributed_reason FROM egress_destinations LIMIT 0")
            .is_ok();
        if !already_widened {
            transaction.execute_batch(MIGRATION_V19_SQL)?;
        }
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms, min_reader_version)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                MIGRATION_V19,
                MIGRATION_V19_CHECKSUM,
                now_ms_i64()?,
                min_reader_version_for(MIGRATION_V19)
            ],
        )?;
    }

    let has_v20 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V20],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v20 {
        // Same probe idea as V18's: this migration widens a CHECK rather than
        // adding a column, so the table's own DDL is the only thing that can
        // say whether the rebuild already happened.
        let already_wide = transaction
            .query_row(
                "SELECT 1 FROM sqlite_master
                 WHERE type = 'table' AND name = 'subsystem_events'
                   AND sql LIKE '%worktree%'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !already_wide {
            transaction.execute_batch(MIGRATION_V20_SQL)?;
        }
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms, min_reader_version)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                MIGRATION_V20,
                MIGRATION_V20_CHECKSUM,
                now_ms_i64()?,
                min_reader_version_for(MIGRATION_V20)
            ],
        )?;
    }

    let has_v21 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V21],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v21 {
        // Same probe as V18's and V20's: this widens a `CHECK` as well as adding
        // columns, so the table's own DDL is the only thing that can say whether
        // the rebuild already ran.
        let already_wide = transaction
            .query_row(
                "SELECT 1 FROM sqlite_master
                 WHERE type = 'table' AND name = 'agent_processes'
                   AND sql LIKE '%foreground_shell%'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !already_wide {
            transaction.execute_batch(MIGRATION_V21_SQL)?;
        }
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms, min_reader_version)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                MIGRATION_V21,
                MIGRATION_V21_CHECKSUM,
                now_ms_i64()?,
                min_reader_version_for(MIGRATION_V21)
            ],
        )?;
    }

    let has_v22 = transaction
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [MIGRATION_V22],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_v22 {
        // A plain `ADD COLUMN`, so no rebuild and no probe of the table's DDL:
        // this changes no `CHECK` and no vocabulary, unlike V18, V20 and V21.
        // Guarded anyway, because a database that already carries the column
        // from a half-applied run would otherwise fail the whole ladder.
        let already_present = transaction
            .query_row(
                "SELECT 1 FROM sqlite_master
                 WHERE type = 'table' AND name = 'agent_processes'
                   AND sql LIKE '%native_start_time%'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !already_present {
            transaction.execute_batch(MIGRATION_V22_SQL)?;
        }
        transaction.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at_ms, min_reader_version)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                MIGRATION_V22,
                MIGRATION_V22_CHECKSUM,
                now_ms_i64()?,
                min_reader_version_for(MIGRATION_V22)
            ],
        )?;
    }

    // Every row's floor is (re)stated from the ladder, including the rows that
    // predate the column. Rewriting them all rather than only the new one keeps
    // the database's answer and the binary's answer the same by construction —
    // a row left at the `DEFAULT 1` would claim a compatibility nobody checked.
    for (version, _, _) in MIGRATION_LADDER {
        transaction.execute(
            "UPDATE schema_migrations SET min_reader_version = ?2 WHERE version = ?1",
            params![*version, min_reader_version_for(*version)],
        )?;
    }

    // Derived rather than written out, because a literal here is a second place
    // the ladder head lives and the two drifted apart silently: `PRAGMA` takes no
    // bound parameter, which is the only reason this is a `format!`.
    transaction.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
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
/// Durable signal intent on a process — see `process_table.rs`'s `ProcessSignal`.
///
/// Intent is a column rather than a live handle because only the daemon's cancel
/// survived a restart. Every other kind's stop was an in-memory
/// `AbortController` or `CancellationToken`: kill the app mid-turn and the
/// *request* to stop was gone along with the thing being stopped, and an
/// out-of-process run could not be signalled at all — `m4_workflows_cancel`
/// returns `false` when the run is absent from its in-memory map, so a
/// daemon-triggered workflow was simply uncancellable from the desktop.
///
/// Recording intent separately from delivery is also what lets a signal be
/// *refused with a reason*: a kind that cannot honour `suspend` says so, instead
/// of a command that appears to succeed and silently does nothing.
/// `kill` stops being indistinguishable from `stop` in the latch.
///
/// Both used to set `stop_requested` alone, so a reader could not tell which was
/// asked for — only the free-text `signal_reason` survived. That was honest
/// while the only kinds honouring `kill` delivered it identically to `stop`, but
/// it means a UI offering two buttons would imply a difference the schema could
/// not carry, and a supervisor could not tell "wind down cleanly" from
/// "terminate now" after a restart.
///
/// `kill_requested` never appears without `stop_requested` — a kill IS a stop
/// with a stronger delivery promise — and the trigger below enforces that rather
/// than trusting every writer to remember it. That invariant is what keeps this
/// migration cheap: every reader already checking `stop_requested` keeps working
/// untouched, no existing query changes meaning, and the
/// `agent_processes_pending_signal_idx` partial index still covers a killed row
/// without being rebuilt, because such a row always has `stop_requested = 1`.
/// The per-process resource ledger (K6(b)).
///
/// Nine measurement columns beside the four `max_*` ceilings already here, and
/// the distinction between the two groups is the point: a `max_*` is a
/// *declaration* a caller made at admission, while these are *readings*. Nothing
/// in this table recorded a reading before, so "what did that run actually cost"
/// had no answer at all.
///
/// **Every column is nullable and NULL means "not measured", never zero.** A
/// resource ledger that reports 0 bytes egressed for a process nobody measured is
/// worse than one that reports nothing, because the zero is indistinguishable
/// from a real measurement of no egress. `usage_unavailable_json` carries the
/// per-field reasons — a `Vec<TraceFieldNote>`, the same
/// `{field, reason}` vocabulary `runtime_telemetry.rs` already uses for exactly
/// this problem — so a NULL always comes with a stated cause rather than a
/// shrug.
///
/// There is deliberately **no wall-time column**: `started_at_ms` and
/// `exited_at_ms` are already here and their difference is the wall time. A
/// stored copy would be a second source of truth for a fact this table already
/// holds, and the two would eventually disagree.
///
/// The trigger is the same kind of duplication as V5's and V7's, and guards the
/// invariant this migration exists for: a row cannot reach `exited` without its
/// reason list being written. `ProcessTable::transition` writes state and usage
/// in one statement precisely so this can be enforced in SQL rather than trusted
/// to every future writer. Rows that were already `exited` before this migration
/// keep their NULL — the trigger fires on the transition, not on history, so
/// nothing has to be back-filled with a number nobody measured.
const MIGRATION_V8_SQL: &str = r#"
ALTER TABLE agent_processes ADD COLUMN cpu_time_ms INTEGER
    CHECK (cpu_time_ms IS NULL OR cpu_time_ms >= 0);
ALTER TABLE agent_processes ADD COLUMN peak_rss_bytes INTEGER
    CHECK (peak_rss_bytes IS NULL OR peak_rss_bytes >= 0);
ALTER TABLE agent_processes ADD COLUMN bytes_read INTEGER
    CHECK (bytes_read IS NULL OR bytes_read >= 0);
ALTER TABLE agent_processes ADD COLUMN bytes_written INTEGER
    CHECK (bytes_written IS NULL OR bytes_written >= 0);
ALTER TABLE agent_processes ADD COLUMN bytes_egressed INTEGER
    CHECK (bytes_egressed IS NULL OR bytes_egressed >= 0);
ALTER TABLE agent_processes ADD COLUMN tokens_in INTEGER
    CHECK (tokens_in IS NULL OR tokens_in >= 0);
ALTER TABLE agent_processes ADD COLUMN tokens_out INTEGER
    CHECK (tokens_out IS NULL OR tokens_out >= 0);
ALTER TABLE agent_processes ADD COLUMN gpu_resident_bytes INTEGER
    CHECK (gpu_resident_bytes IS NULL OR gpu_resident_bytes >= 0);
ALTER TABLE agent_processes ADD COLUMN gpu_device_ms INTEGER
    CHECK (gpu_device_ms IS NULL OR gpu_device_ms >= 0);
ALTER TABLE agent_processes ADD COLUMN usage_unavailable_json TEXT;

CREATE TRIGGER agent_processes_close_out_states_its_gaps
BEFORE UPDATE OF state ON agent_processes
WHEN NEW.state = 'exited' AND NEW.usage_unavailable_json IS NULL
BEGIN
    SELECT RAISE(ABORT, 'an exited agent process must state its unmeasured fields');
END;
"#;

/// Teaches the ledger which of its own migrations an older binary can live with
/// (roadmap K12, and the reason `denial_sink.rs` is a separate file).
///
/// The column is `NOT NULL` with a default so the `ALTER` succeeds on a
/// populated table; every existing row is then rewritten from
/// [`min_reader_version_for`], which is the same table the guard reads. There is
/// no second source of truth to drift.
const MIGRATION_V13_SQL: &str = r#"
ALTER TABLE schema_migrations ADD COLUMN min_reader_version INTEGER NOT NULL DEFAULT 1;
"#;

/// Records who a process's *allowed* egress actually went to (roadmap K5, K12).
///
/// # The gap this closes
///
/// `denial_sink` writes down every refused request with the rule that refused
/// it, and V8 gave `agent_processes` a `bytes_egressed` column. Between them
/// they answer "what was blocked" and "how much got out" — and neither answers
/// **where it went**. A run that was never denied anything produced no record of
/// its network activity at all, which is the half `denial_sink`'s own module doc
/// names as still missing.
///
/// # Why a counter table and not more events
///
/// This is deliberately *not* part of V9's or V12's hash chains, and calling it
/// tamper-evident would be a lie, so it does not claim to be. The rows are
/// mutable by construction: `requests` is incremented in place, because the
/// alternative — one immutable row per request — is a per-request append to a
/// serialized chain for the highest-frequency thing this app does. A summary
/// keyed by destination is bounded by how many distinct places a process talked
/// to, which is a small number that does not grow with how much it said.
///
/// The chained streams keep their job: a *decision* (V11) and a subsystem
/// *action* (V12) are events and are chained. This is an aggregate beside them.
///
/// # `ON DELETE CASCADE`, where the rest of this schema uses `RESTRICT`
///
/// The difference is real rather than an inconsistency. `RESTRICT` protects rows
/// that are records in their own right — a run, a parent process — from
/// vanishing under something that references them. These rows are not their own
/// record: they are a property *of* a process, meaningless without it, so a
/// process that is ever pruned should take them with it rather than block on
/// them.
///
/// # Why the overflow count sits on `agent_processes`
///
/// `run_scope::MAX_DESTINATIONS` caps how many distinct destinations one process
/// names, because a run with no declared allowlist can be walked across
/// arbitrarily many hosts by the content it fetches. The requests past that cap
/// are still counted — a truncated list that does not say it is truncated reads
/// as a complete one — and they belong on the process rather than in a sentinel
/// row here, which would have to be excluded by every reader that ever joins
/// this table.
const MIGRATION_V14_SQL: &str = r#"
CREATE TABLE egress_destinations (
    process_id TEXT NOT NULL REFERENCES agent_processes(process_id) ON DELETE CASCADE,
    scheme TEXT NOT NULL CHECK (length(scheme) > 0),
    host TEXT NOT NULL CHECK (length(host) > 0),
    port INTEGER NOT NULL CHECK (port BETWEEN 1 AND 65535),
    requests INTEGER NOT NULL CHECK (requests > 0),
    first_seen_ms INTEGER NOT NULL CHECK (first_seen_ms > 0),
    last_seen_ms INTEGER NOT NULL CHECK (last_seen_ms >= first_seen_ms),
    PRIMARY KEY (process_id, scheme, host, port)
) STRICT;

CREATE INDEX egress_destinations_host_idx ON egress_destinations(host, last_seen_ms DESC);
"#;

/// Lets `egress_destinations` hold the traffic no run made (roadmap K5).
///
/// V14 recorded **where** allowed egress went, but only for traffic a process row
/// could be charged. Everything else — a user clicking Check for updates, a
/// scheduled fetch, an inbound request, startup, a shared MCP transport — had a
/// *reason* to be charged to and no row to hang a destination list off, so
/// `UNATTRIBUTED_EGRESS` reported volume by reason and named nowhere at all. "Which
/// hosts does the app itself reach outside a run" had no answer.
///
/// This gives those rows a home in the same table, keyed by
/// [`crate::run_scope::Unattributed`]'s own persisted `code()` rather than by a
/// second vocabulary invented here.
///
/// # Nullable `process_id`, and exactly one attribution
///
/// A row names a process or a reason, never both and never neither. That is a
/// `CHECK` rather than a convention, because "neither" is a destination charged to
/// nothing — which is the exact failure this whole item is about — and "both" is a
/// row two readers would each count once.
///
/// # Why the primary key becomes a unique index
///
/// SQLite permits NULLs in the columns of a non-`INTEGER` primary key, so
/// `PRIMARY KEY (process_id, scheme, host, port)` would stop deduplicating the
/// moment `process_id` went nullable: every unattributed insert would look
/// distinct and the upsert would never fire. The unique index over `COALESCE`d
/// attribution columns has the semantics the primary key was there to provide,
/// and `ON CONFLICT` can target it by the same expressions.
///
/// # Why the overflow count is a separate table
///
/// The attributed cap's overflow lives on `agent_processes`
/// (`egress_destinations_dropped`) because it is a property of that process. An
/// unattributed overflow has no process, and V14's own note rejects a sentinel row
/// in this table — it would have to be excluded by every reader that joins here.
/// So it gets the smallest thing that is not either: one row per reason.
const MIGRATION_V19_SQL: &str = r#"
CREATE TABLE egress_destinations_v19 (
    process_id TEXT REFERENCES agent_processes(process_id) ON DELETE CASCADE,
    unattributed_reason TEXT CHECK (unattributed_reason IS NULL OR length(unattributed_reason) > 0),
    scheme TEXT NOT NULL CHECK (length(scheme) > 0),
    host TEXT NOT NULL CHECK (length(host) > 0),
    port INTEGER NOT NULL CHECK (port BETWEEN 1 AND 65535),
    requests INTEGER NOT NULL CHECK (requests > 0),
    first_seen_ms INTEGER NOT NULL CHECK (first_seen_ms > 0),
    last_seen_ms INTEGER NOT NULL CHECK (last_seen_ms >= first_seen_ms),
    CHECK ((process_id IS NULL) <> (unattributed_reason IS NULL))
) STRICT;

INSERT INTO egress_destinations_v19
    (process_id, unattributed_reason, scheme, host, port, requests, first_seen_ms, last_seen_ms)
SELECT process_id, NULL, scheme, host, port, requests, first_seen_ms, last_seen_ms
  FROM egress_destinations;

DROP TABLE egress_destinations;
ALTER TABLE egress_destinations_v19 RENAME TO egress_destinations;

CREATE UNIQUE INDEX egress_destinations_key_idx ON egress_destinations(
    COALESCE(process_id, ''), COALESCE(unattributed_reason, ''), scheme, host, port
);
CREATE INDEX egress_destinations_host_idx ON egress_destinations(host, last_seen_ms DESC);

CREATE TABLE unattributed_egress_overflow (
    reason TEXT PRIMARY KEY,
    dropped INTEGER NOT NULL CHECK (dropped > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0)
) STRICT;
"#;

/// V14's second half — see [`MIGRATION_V14_SQL`] on why the count of dropped
/// destinations lives on the process rather than in a sentinel row.
///
/// Separate from the table so each half can be applied behind its own probe.
/// Nullable like every V8 measurement column and for the same rule: NULL is "no
/// flush ever reported one", which is a different fact from a measured zero.
const MIGRATION_V14_COLUMN_SQL: &str = r#"
ALTER TABLE agent_processes ADD COLUMN egress_destinations_dropped INTEGER
    CHECK (egress_destinations_dropped IS NULL OR egress_destinations_dropped >= 0);
"#;

/// Records whether a decision's `tool_call_id` names a real tool call (K12).
///
/// `tool_call_id` is `NOT NULL`, so `permissions::request_permission` invents
/// one for a caller that has none — a Settings model deletion, a local app run
/// over HTTP. The invented id is shaped exactly like a real one, so
/// `permission_decisions_tool_call_idx` holds entries that join to nothing and
/// look like they should. This column is the difference, and it is the same
/// distinction the `attribution` column already draws for the run: absence with
/// a stated reason rather than a plausible-looking value.
///
/// The default is `'unknown'` rather than `'caller'`, and that is the whole
/// care in this migration. Every row written before this column existed had its
/// origin unrecorded; defaulting them to `'caller'` would assert something
/// nobody checked, about precisely the rows most likely to be synthesized. The
/// ids could be pattern-matched — the synthesized ones are `tool-` plus a simple
/// UUID — but a heuristic that mislabels one real tool call is worse than an
/// honest `'unknown'`, and this backfill would be unreviewable either way.
const MIGRATION_V15_SQL: &str = r#"
ALTER TABLE permission_decisions ADD COLUMN tool_call_origin TEXT NOT NULL
    DEFAULT 'unknown'
    CHECK (tool_call_origin IN ('caller', 'synthesized', 'unknown'));
"#;

/// Prompt tokens a runtime reported it reused from its own prompt cache instead
/// of evaluating them, summed over this process's completions (roadmap K11).
///
/// **This is a measurement, and the column exists so that it cannot quietly stop
/// being one.** K11 asks for a hit rate and a tokens-saved figure that are
/// measured rather than estimated, and the only party that can measure either is
/// the runtime: llama-server reports the split as `timings.cache_n` and
/// `timings.prompt_n`, and an app-side estimate would just be
/// "how much of this prompt looks like the last one", which is a guess about the
/// contents of a cache it cannot see.
///
/// Nullable, like every V8 measurement column and for the same rule that governs
/// them: NULL is "no completion under this process ever reported reuse", which is
/// a different fact from a measured zero. Ollama and MLX report nothing here, so
/// their processes stay NULL rather than being recorded as having reused nothing
/// — a zero would sum into a denominator and pull a real hit rate down with it.
///
/// Two columns rather than a stored ratio: a ratio cannot be accumulated
/// additively, and `add_context_reuse` is additive like every other flush at this
/// boundary.
const MIGRATION_V16_REUSED_SQL: &str = r#"
ALTER TABLE agent_processes ADD COLUMN context_tokens_reused INTEGER
    CHECK (context_tokens_reused IS NULL OR context_tokens_reused >= 0);
"#;

/// The prompt-token ceiling a process may send in one request (roadmap K11).
///
/// The fifth `max_*` column, beside the four V5 installed, and it follows their
/// rule: `NULL` is "no budget", not "a budget of zero". A process with no budget
/// is the default and the only state any caller writes today — see
/// `ProcessLimits::max_context_tokens` on why this ships enforced and unset.
///
/// `> 0` rather than `>= 0` because a zero-token budget cannot be satisfied by
/// any request at all, including an empty one: the chat template alone is
/// tokens. A row claiming it would refuse every turn while reading like a
/// configured limit, which is the failure the `CHECK` exists to make impossible.
const MIGRATION_V17_SQL: &str = r#"
ALTER TABLE agent_processes ADD COLUMN max_context_tokens INTEGER
    CHECK (max_context_tokens IS NULL OR max_context_tokens > 0);
"#;

/// Admits `browser_session` to `agent_processes.kind` (roadmap K4).
///
/// # Why this is a whole-table rebuild and not an `ALTER`
///
/// The kind vocabulary is a column `CHECK`, and SQLite has no
/// `ALTER TABLE … DROP CONSTRAINT`. Widening it is the documented twelve-step
/// rebuild — new table, copy, drop, rename, recreate every index and trigger —
/// and it is the only correct route. The alternative, `PRAGMA writable_schema`,
/// edits the schema out from under a live connection with no validation at all,
/// which is not a shortcut worth taking on the table that holds every process
/// this app has ever run.
///
/// Every column is listed explicitly on both sides of the copy rather than
/// relying on `SELECT *`. The live table's column *order* is an artifact of the
/// order six migrations happened to run their `ADD COLUMN`s in; naming them makes
/// this correct even if that order ever changes, and makes a forgotten column a
/// compile-time-visible omission rather than a silent shift of every value one
/// place to the left.
///
/// Foreign keys are not disabled around this. The self-reference on
/// `parent_process_id` is the only one into this table, `PRAGMA foreign_keys` is
/// a no-op inside a transaction anyway, and the rename step carries child
/// references across by design — which is exactly the behaviour wanted here,
/// since the rows are the same rows.
///
/// The two rebuilt triggers and six rebuilt indexes are transcribed from V5, V6,
/// V7 and V8 unchanged. `agent_processes_kind_idx` and the rest are recreated by
/// hand because `DROP TABLE` takes them with it.
/// Admits `foreground_shell` to `agent_processes.kind`, and gives a limit kill a
/// typed representation instead of a sentence (roadmap K4).
///
/// Two changes that have to travel together because both need the table rebuilt.
///
/// **The kind.** The agent shell a turn blocks on is the most common native
/// process this app creates and the one that actually holds the memory a limit is
/// about, and it had no row at all — so a limit could be declared on the turn
/// while the process consuming the machine was invisible.
///
/// **The breach columns.** `ExitStatus::LimitExceeded` existed from V5 and the
/// only place the *detail* could go was `exit_reason`, free text — and in the
/// daemon's case a marker encoded into `last_error` and parsed back out, which
/// that slice recorded as a deliberate second-best pending a migration. This is
/// that migration. A reader asking "which limit, what was configured, what was
/// measured, and what held it" now gets four columns rather than a prose parse.
///
/// They are constrained as a group rather than individually: a partial write —
/// a limit named with no measurement, a measurement with no limit — is the one
/// shape that would let a UI print a confident half-truth.
/// A process row names a process, not a pid.
///
/// # Why a pid alone was not enough, and where it bit
///
/// Everything the resource controller does in memory is keyed on
/// `(pid, start_time)` — `ProcessIdentity` — precisely so a pid the kernel has
/// since handed to somebody else is never sampled as ours and, far worse, never
/// signalled as ours. That identity died with the process holding it.
///
/// Startup reconciliation is exactly where that matters. After a crash the app
/// reads rows a previous session left `running` and reclaims the trees behind
/// them, and the only handle it had was `native_pid` plus "is something alive at
/// that pid". On a machine that has been up for a while, and across a restart
/// that may be hours later, that is a coin flip against the pid space: the
/// answer is yes for the user's editor as readily as for the shell this app
/// abandoned.
///
/// So the identity is durable now. The value is the platform's own opaque
/// start-time stamp — `/proc` jiffies, a BSD start timeval, a Windows creation
/// FILETIME — never compared across hosts, only ever against
/// `ProcessIdentity::of(pid)` on the machine that wrote it.
const MIGRATION_V22_SQL: &str = r#"
ALTER TABLE agent_processes ADD COLUMN native_start_time INTEGER
    CHECK (native_start_time IS NULL OR native_start_time >= 0);
"#;

const MIGRATION_V21_SQL: &str = r#"
CREATE TABLE agent_processes_v21 (
    process_id TEXT PRIMARY KEY,
    parent_process_id TEXT REFERENCES agent_processes_v21(process_id) ON DELETE RESTRICT,
    kind TEXT NOT NULL CHECK (kind IN (
        'chat_turn', 'daemon_job', 'subagent', 'crew_member', 'workflow_run',
        'workflow_node', 'remote_run', 'background_shell', 'side_task',
        'browser_session', 'foreground_shell'
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
    stop_requested INTEGER NOT NULL DEFAULT 0 CHECK (stop_requested IN (0, 1)),
    suspend_requested INTEGER NOT NULL DEFAULT 0 CHECK (suspend_requested IN (0, 1)),
    signal_reason TEXT,
    signal_requested_at_ms INTEGER
        CHECK (signal_requested_at_ms IS NULL OR signal_requested_at_ms > 0),
    kill_requested INTEGER NOT NULL DEFAULT 0 CHECK (kill_requested IN (0, 1)),
    cpu_time_ms INTEGER CHECK (cpu_time_ms IS NULL OR cpu_time_ms >= 0),
    peak_rss_bytes INTEGER CHECK (peak_rss_bytes IS NULL OR peak_rss_bytes >= 0),
    bytes_read INTEGER CHECK (bytes_read IS NULL OR bytes_read >= 0),
    bytes_written INTEGER CHECK (bytes_written IS NULL OR bytes_written >= 0),
    bytes_egressed INTEGER CHECK (bytes_egressed IS NULL OR bytes_egressed >= 0),
    tokens_in INTEGER CHECK (tokens_in IS NULL OR tokens_in >= 0),
    tokens_out INTEGER CHECK (tokens_out IS NULL OR tokens_out >= 0),
    gpu_resident_bytes INTEGER CHECK (gpu_resident_bytes IS NULL OR gpu_resident_bytes >= 0),
    gpu_device_ms INTEGER CHECK (gpu_device_ms IS NULL OR gpu_device_ms >= 0),
    usage_unavailable_json TEXT,
    egress_destinations_dropped INTEGER
        CHECK (egress_destinations_dropped IS NULL OR egress_destinations_dropped >= 0),
    context_tokens_reused INTEGER
        CHECK (context_tokens_reused IS NULL OR context_tokens_reused >= 0),
    max_context_tokens INTEGER CHECK (max_context_tokens IS NULL OR max_context_tokens > 0),
    context_tokens_evaluated INTEGER
        CHECK (context_tokens_evaluated IS NULL OR context_tokens_evaluated >= 0),
    -- The `ProcessLimits` field name, so the row names the thing that was set
    -- rather than a category. Constrained to the five that exist: a breach
    -- naming a sixth is a bug, not a new resource.
    limit_kind TEXT CHECK (limit_kind IS NULL OR limit_kind IN (
        'max_wall_ms', 'max_memory_bytes', 'max_output_bytes',
        'max_child_processes', 'max_context_tokens'
    )),
    limit_configured INTEGER CHECK (limit_configured IS NULL OR limit_configured > 0),
    limit_observed INTEGER CHECK (limit_observed IS NULL OR limit_observed >= 0),
    -- Which mechanism noticed, and whether it was kernel-held or supervised. The
    -- second is the fact a reader cannot infer from the first: a supervised bound
    -- dies with the supervisor and a kernel one does not.
    limit_backend TEXT,
    limit_level TEXT CHECK (limit_level IS NULL OR limit_level IN
        ('kernel', 'supervised', 'owner-sourced')),
    limit_observed_at_ms INTEGER
        CHECK (limit_observed_at_ms IS NULL OR limit_observed_at_ms >= 0),
    -- The kernel counter or notification that carried the proof, where the
    -- breach came from a mechanism's own accounting rather than from the
    -- supervisor's comparison. Deliberately *outside* the grouped CHECKs below:
    -- a supervised bound has no such counter, and requiring one would force a
    -- writer to invent it. See `resource_control::LimitEvent`.
    limit_evidence TEXT,
    CHECK ((state = 'exited') = (exit_status IS NOT NULL)),
    CHECK (parent_process_id IS NULL OR parent_process_id <> process_id),
    -- The breach travels as a unit. A limit named with no measurement, or a
    -- measurement with no limit, is a partial write that would read as a
    -- confident half-truth.
    CHECK ((limit_kind IS NULL) = (limit_configured IS NULL)),
    CHECK ((limit_kind IS NULL) = (limit_observed IS NULL)),
    CHECK ((limit_kind IS NULL) = (limit_backend IS NULL)),
    CHECK ((limit_kind IS NULL) = (limit_level IS NULL)),
    -- A breach is only ever recorded on a row that ended because of it.
    CHECK (limit_kind IS NULL OR exit_status = 'limit_exceeded'),
    UNIQUE(kind, external_id)
) STRICT;

INSERT INTO agent_processes_v21 (
    process_id, parent_process_id, kind, external_id, state, run_id, workspace, profile,
    native_pid, max_wall_ms, max_memory_bytes, max_output_bytes, max_child_processes,
    exit_status, exit_code, exit_signal, exit_reason, created_at_ms, updated_at_ms,
    started_at_ms, exited_at_ms, stop_requested, suspend_requested, signal_reason,
    signal_requested_at_ms, kill_requested, cpu_time_ms, peak_rss_bytes, bytes_read,
    bytes_written, bytes_egressed, tokens_in, tokens_out, gpu_resident_bytes, gpu_device_ms,
    usage_unavailable_json, egress_destinations_dropped, context_tokens_reused,
    max_context_tokens, context_tokens_evaluated
)
SELECT
    process_id, parent_process_id, kind, external_id, state, run_id, workspace, profile,
    native_pid, max_wall_ms, max_memory_bytes, max_output_bytes, max_child_processes,
    exit_status, exit_code, exit_signal, exit_reason, created_at_ms, updated_at_ms,
    started_at_ms, exited_at_ms, stop_requested, suspend_requested, signal_reason,
    signal_requested_at_ms, kill_requested, cpu_time_ms, peak_rss_bytes, bytes_read,
    bytes_written, bytes_egressed, tokens_in, tokens_out, gpu_resident_bytes, gpu_device_ms,
    usage_unavailable_json, egress_destinations_dropped, context_tokens_reused,
    max_context_tokens, context_tokens_evaluated
FROM agent_processes;

DROP TABLE agent_processes;
ALTER TABLE agent_processes_v21 RENAME TO agent_processes;

CREATE INDEX agent_processes_live_idx ON agent_processes(created_at_ms DESC)
    WHERE state <> 'exited';
CREATE INDEX agent_processes_kind_idx ON agent_processes(kind, created_at_ms DESC);
CREATE INDEX agent_processes_parent_idx ON agent_processes(parent_process_id)
    WHERE parent_process_id IS NOT NULL;
CREATE INDEX agent_processes_run_idx ON agent_processes(run_id)
    WHERE run_id IS NOT NULL;
CREATE INDEX agent_processes_workspace_idx ON agent_processes(workspace, created_at_ms DESC)
    WHERE workspace IS NOT NULL;
CREATE INDEX agent_processes_pending_signal_idx ON agent_processes(kind)
    WHERE state <> 'exited' AND (stop_requested = 1 OR suspend_requested = 1);

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

CREATE TRIGGER agent_processes_kill_implies_stop
BEFORE UPDATE OF kill_requested ON agent_processes
WHEN NEW.kill_requested = 1 AND NEW.stop_requested <> 1
BEGIN
    SELECT RAISE(ABORT, 'kill_requested implies stop_requested');
END;

CREATE TRIGGER agent_processes_close_out_states_its_gaps
BEFORE UPDATE OF state ON agent_processes
WHEN NEW.state = 'exited' AND NEW.usage_unavailable_json IS NULL
BEGIN
    SELECT RAISE(ABORT, 'an exited agent process must state its unmeasured fields');
END;
"#;

const MIGRATION_V18_SQL: &str = r#"
CREATE TABLE agent_processes_v18 (
    process_id TEXT PRIMARY KEY,
    parent_process_id TEXT REFERENCES agent_processes_v18(process_id) ON DELETE RESTRICT,
    kind TEXT NOT NULL CHECK (kind IN (
        'chat_turn', 'daemon_job', 'subagent', 'crew_member', 'workflow_run',
        'workflow_node', 'remote_run', 'background_shell', 'side_task',
        'browser_session'
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
    stop_requested INTEGER NOT NULL DEFAULT 0 CHECK (stop_requested IN (0, 1)),
    suspend_requested INTEGER NOT NULL DEFAULT 0 CHECK (suspend_requested IN (0, 1)),
    signal_reason TEXT,
    signal_requested_at_ms INTEGER
        CHECK (signal_requested_at_ms IS NULL OR signal_requested_at_ms > 0),
    kill_requested INTEGER NOT NULL DEFAULT 0 CHECK (kill_requested IN (0, 1)),
    cpu_time_ms INTEGER CHECK (cpu_time_ms IS NULL OR cpu_time_ms >= 0),
    peak_rss_bytes INTEGER CHECK (peak_rss_bytes IS NULL OR peak_rss_bytes >= 0),
    bytes_read INTEGER CHECK (bytes_read IS NULL OR bytes_read >= 0),
    bytes_written INTEGER CHECK (bytes_written IS NULL OR bytes_written >= 0),
    bytes_egressed INTEGER CHECK (bytes_egressed IS NULL OR bytes_egressed >= 0),
    tokens_in INTEGER CHECK (tokens_in IS NULL OR tokens_in >= 0),
    tokens_out INTEGER CHECK (tokens_out IS NULL OR tokens_out >= 0),
    gpu_resident_bytes INTEGER CHECK (gpu_resident_bytes IS NULL OR gpu_resident_bytes >= 0),
    gpu_device_ms INTEGER CHECK (gpu_device_ms IS NULL OR gpu_device_ms >= 0),
    usage_unavailable_json TEXT,
    egress_destinations_dropped INTEGER
        CHECK (egress_destinations_dropped IS NULL OR egress_destinations_dropped >= 0),
    context_tokens_reused INTEGER
        CHECK (context_tokens_reused IS NULL OR context_tokens_reused >= 0),
    max_context_tokens INTEGER CHECK (max_context_tokens IS NULL OR max_context_tokens > 0),
    context_tokens_evaluated INTEGER
        CHECK (context_tokens_evaluated IS NULL OR context_tokens_evaluated >= 0),
    CHECK ((state = 'exited') = (exit_status IS NOT NULL)),
    CHECK (parent_process_id IS NULL OR parent_process_id <> process_id),
    UNIQUE(kind, external_id)
) STRICT;

INSERT INTO agent_processes_v18 (
    process_id, parent_process_id, kind, external_id, state, run_id, workspace, profile,
    native_pid, max_wall_ms, max_memory_bytes, max_output_bytes, max_child_processes,
    exit_status, exit_code, exit_signal, exit_reason, created_at_ms, updated_at_ms,
    started_at_ms, exited_at_ms, stop_requested, suspend_requested, signal_reason,
    signal_requested_at_ms, kill_requested, cpu_time_ms, peak_rss_bytes, bytes_read,
    bytes_written, bytes_egressed, tokens_in, tokens_out, gpu_resident_bytes, gpu_device_ms,
    usage_unavailable_json, egress_destinations_dropped, context_tokens_reused,
    max_context_tokens, context_tokens_evaluated
)
SELECT
    process_id, parent_process_id, kind, external_id, state, run_id, workspace, profile,
    native_pid, max_wall_ms, max_memory_bytes, max_output_bytes, max_child_processes,
    exit_status, exit_code, exit_signal, exit_reason, created_at_ms, updated_at_ms,
    started_at_ms, exited_at_ms, stop_requested, suspend_requested, signal_reason,
    signal_requested_at_ms, kill_requested, cpu_time_ms, peak_rss_bytes, bytes_read,
    bytes_written, bytes_egressed, tokens_in, tokens_out, gpu_resident_bytes, gpu_device_ms,
    usage_unavailable_json, egress_destinations_dropped, context_tokens_reused,
    max_context_tokens, context_tokens_evaluated
FROM agent_processes;

DROP TABLE agent_processes;
ALTER TABLE agent_processes_v18 RENAME TO agent_processes;

CREATE INDEX agent_processes_live_idx ON agent_processes(created_at_ms DESC)
    WHERE state <> 'exited';
CREATE INDEX agent_processes_kind_idx ON agent_processes(kind, created_at_ms DESC);
CREATE INDEX agent_processes_parent_idx ON agent_processes(parent_process_id)
    WHERE parent_process_id IS NOT NULL;
CREATE INDEX agent_processes_run_idx ON agent_processes(run_id)
    WHERE run_id IS NOT NULL;
CREATE INDEX agent_processes_workspace_idx ON agent_processes(workspace, created_at_ms DESC)
    WHERE workspace IS NOT NULL;
CREATE INDEX agent_processes_pending_signal_idx ON agent_processes(kind)
    WHERE state <> 'exited' AND (stop_requested = 1 OR suspend_requested = 1);

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

CREATE TRIGGER agent_processes_kill_implies_stop
BEFORE UPDATE OF kill_requested ON agent_processes
WHEN NEW.kill_requested = 1 AND NEW.stop_requested <> 1
BEGIN
    SELECT RAISE(ABORT, 'kill_requested implies stop_requested');
END;

CREATE TRIGGER agent_processes_close_out_states_its_gaps
BEFORE UPDATE OF state ON agent_processes
WHEN NEW.state = 'exited' AND NEW.usage_unavailable_json IS NULL
BEGIN
    SELECT RAISE(ABORT, 'an exited agent process must state its unmeasured fields');
END;
"#;

/// V16's other half — the prompt tokens the runtime said it actually evaluated,
/// which is the rest of the hit rate's denominator.
///
/// Applied behind its own probe for [`MIGRATION_V16_REUSED_SQL`]'s reason.
const MIGRATION_V16_EVALUATED_SQL: &str = r#"
ALTER TABLE agent_processes ADD COLUMN context_tokens_evaluated INTEGER
    CHECK (context_tokens_evaluated IS NULL OR context_tokens_evaluated >= 0);
"#;

/// Records every permission decision, including the ones no run can hold
/// (roadmap K12).
///
/// **Why this is a table and not more run events.** The acceptance says a tool
/// call whose authorizing decision cannot be produced from the log is a bug, and
/// that bug was live: `permissions.rs` wrote its `PermissionRequested` /
/// `PermissionDecided` events only when `durable_run_exists`, because
/// `run_events.run_id` is a foreign key onto `runs`. So deleting a model from
/// Settings, running a local app definition over HTTP, and posting a triage
/// reply to Slack — three gated, security-relevant approvals — left no record
/// anywhere at all.
///
/// The alternative was to register a `runs` row for that work so events could
/// hang off it. That was rejected: a run carries a spec, an idempotency key, an
/// event budget and a status lifecycle, and it shows up in the runs list. Making
/// one up so a Settings click has somewhere to write is inventing an identity,
/// which is the failure mode `run_scope`'s own module doc argues against.
///
/// So the attribution is recorded as what it actually is. `attribution` is a
/// closed set covering both arms of [`crate::run_scope::RunScope`] plus the two
/// states that scope cannot express: a run id that exists but was never
/// registered in the ledger, and nobody having said either way.
///
/// A row is written when the request is raised and updated exactly once when the
/// decision lands. The two triggers hold that shape against any writer: a
/// decision is final, the request half is immutable once recorded, and rows
/// cannot be deleted. This mirrors `run_events`' append-only triggers rather
/// than inventing a second discipline.
///
/// ponytail: `run_id` and `process_id` are plain columns, not foreign keys. A
/// permission is gated *before* its run is registered, so a foreign key here
/// would reintroduce the exact `durable_run_exists` gate this table exists to
/// remove — and `process_id` follows `run_events.process_id` for the reason
/// given there. Outer-join to read either.
/// The one event stream the run-less subsystems write to (roadmap K12).
///
/// **Why this is not `run_events`.** The acceptance asks for "one event stream
/// every subsystem writes to — desktop, daemon, HTTP, ACP, MCP, browser, remote
/// node". `run_events.run_id` is `NOT NULL REFERENCES runs(run_id)`, its insert
/// trigger requires contiguous per-run sequences, and its hash chain is per run.
/// Every one of those is run-shaped, and an inbound HTTP request, an MCP tool
/// call on a shared transport, and a browser action are not runs and will not
/// become them — `run_scope`'s module doc argues that case at length. The choice
/// was to manufacture a `runs` row per request or to have a stream that does not
/// need one; this is the second.
///
/// **What it does not duplicate.** A gated action's *authorization* already
/// lives in `permission_decisions` (V11), written before the action runs and for
/// every caller, so this table records what happened and points back at the
/// decision by `request_id` rather than restating it. That is also what closes
/// the acceptance's "for anything gated, the exact policy decision that
/// permitted it" for these subsystems: one join, in the direction the question
/// is actually asked.
///
/// **One global chain, not one per anything.** There is no per-run sequence to
/// hang a chain off, and a per-subsystem chain would let a whole subsystem's
/// tail be removed without breaking any other. `sequence` is a single
/// `AUTOINCREMENT` counter and each row's hash binds to its predecessor's,
/// exactly as `run_events` does within one run.
///
/// Unlike V9's chain this one has **no unchained era to tolerate**: the table is
/// new, so `event_hash` is `NOT NULL` from the first row and `prev_event_hash`
/// is `NULL` only for `sequence = 1`. There is nothing to backfill and therefore
/// nothing to launder.
///
/// ponytail: the global chain serializes appends, since each one reads the
/// current tail. Fine at the volume these subsystems produce; if an HTTP server
/// ever writes per-request rows at load, shard the chain by subsystem and anchor
/// each shard's head in a parent chain rather than dropping the linkage.
const MIGRATION_V12_SQL: &str = r#"
CREATE TABLE subsystem_events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT NOT NULL UNIQUE,
    subsystem TEXT NOT NULL CHECK (subsystem IN
        ('http', 'mcp', 'browser', 'acp', 'remote')),
    action TEXT NOT NULL CHECK (length(action) > 0),
    occurred_at_ms INTEGER NOT NULL CHECK (occurred_at_ms > 0),
    -- Plain columns, not foreign keys, for the reason `permission_decisions`
    -- gives: the whole point is that these events exist without a run.
    run_id TEXT,
    attribution TEXT NOT NULL CHECK (attribution IN (
        'ledger-run', 'unregistered-run', 'unknown',
        'unattributed.user-action', 'unattributed.scheduled',
        'unattributed.inbound-request', 'unattributed.startup',
        'unattributed.shared-transport'
    )),
    process_id TEXT,
    -- The `permission_decisions` row that authorized this action, when one did.
    -- NULL means nothing gated it, which is a finding rather than a blank.
    permission_request_id TEXT,
    outcome TEXT NOT NULL CHECK (outcome IN
        ('succeeded', 'failed', 'denied', 'cancelled')),
    detail_json BLOB,
    event_hash TEXT NOT NULL CHECK (length(event_hash) = 64),
    prev_event_hash TEXT CHECK (prev_event_hash IS NULL OR length(prev_event_hash) = 64),
    CHECK ((attribution IN ('ledger-run', 'unregistered-run')) = (run_id IS NOT NULL))
) STRICT;

CREATE INDEX subsystem_events_time_idx ON subsystem_events(occurred_at_ms, sequence);
CREATE INDEX subsystem_events_subsystem_idx ON subsystem_events(subsystem, sequence);
CREATE INDEX subsystem_events_run_idx
    ON subsystem_events(run_id, sequence) WHERE run_id IS NOT NULL;
CREATE INDEX subsystem_events_permission_idx
    ON subsystem_events(permission_request_id) WHERE permission_request_id IS NOT NULL;

CREATE TRIGGER subsystem_events_forbid_update
BEFORE UPDATE ON subsystem_events
BEGIN
    SELECT RAISE(ABORT, 'subsystem events are append-only');
END;

CREATE TRIGGER subsystem_events_forbid_delete
BEFORE DELETE ON subsystem_events
BEGIN
    SELECT RAISE(ABORT, 'subsystem events are append-only');
END;

-- Structural linkage, so it holds against a writer that never goes through
-- Rust. SQLite cannot compute SHA-256, so the hash's *content* is
-- `verify_subsystem_chain`'s job; "points at its predecessor" is the database's.
CREATE TRIGGER subsystem_events_chain_links_to_its_predecessor
BEFORE INSERT ON subsystem_events
FOR EACH ROW
WHEN NEW.prev_event_hash IS NOT (
        SELECT event_hash FROM subsystem_events ORDER BY sequence DESC LIMIT 1
    )
BEGIN
    SELECT RAISE(ABORT, 'subsystem event must carry its predecessor''s hash');
END;
"#;

/// V20: widens `subsystem_events.subsystem` to admit `'worktree'` — V18's
/// table-rebuild recipe applied to the subsystem stream. `sequence` is copied
/// verbatim so the hash chain's order (and `sqlite_sequence`'s counter, which
/// SQLite advances past the copied maximum) survive the rebuild, and the
/// append-only + chain-linkage triggers are re-stated because they die with
/// the dropped table.
const MIGRATION_V20_SQL: &str = r#"
CREATE TABLE subsystem_events_v20 (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT NOT NULL UNIQUE,
    subsystem TEXT NOT NULL CHECK (subsystem IN
        ('http', 'mcp', 'browser', 'acp', 'remote', 'worktree')),
    action TEXT NOT NULL CHECK (length(action) > 0),
    occurred_at_ms INTEGER NOT NULL CHECK (occurred_at_ms > 0),
    run_id TEXT,
    attribution TEXT NOT NULL CHECK (attribution IN (
        'ledger-run', 'unregistered-run', 'unknown',
        'unattributed.user-action', 'unattributed.scheduled',
        'unattributed.inbound-request', 'unattributed.startup',
        'unattributed.shared-transport'
    )),
    process_id TEXT,
    permission_request_id TEXT,
    outcome TEXT NOT NULL CHECK (outcome IN
        ('succeeded', 'failed', 'denied', 'cancelled')),
    detail_json BLOB,
    event_hash TEXT NOT NULL CHECK (length(event_hash) = 64),
    prev_event_hash TEXT CHECK (prev_event_hash IS NULL OR length(prev_event_hash) = 64),
    CHECK ((attribution IN ('ledger-run', 'unregistered-run')) = (run_id IS NOT NULL))
) STRICT;

INSERT INTO subsystem_events_v20 (
    sequence, event_id, subsystem, action, occurred_at_ms, run_id, attribution,
    process_id, permission_request_id, outcome, detail_json, event_hash, prev_event_hash
)
SELECT
    sequence, event_id, subsystem, action, occurred_at_ms, run_id, attribution,
    process_id, permission_request_id, outcome, detail_json, event_hash, prev_event_hash
FROM subsystem_events;

DROP TABLE subsystem_events;
ALTER TABLE subsystem_events_v20 RENAME TO subsystem_events;

CREATE INDEX subsystem_events_time_idx ON subsystem_events(occurred_at_ms, sequence);
CREATE INDEX subsystem_events_subsystem_idx ON subsystem_events(subsystem, sequence);
CREATE INDEX subsystem_events_run_idx
    ON subsystem_events(run_id, sequence) WHERE run_id IS NOT NULL;
CREATE INDEX subsystem_events_permission_idx
    ON subsystem_events(permission_request_id) WHERE permission_request_id IS NOT NULL;

CREATE TRIGGER subsystem_events_forbid_update
BEFORE UPDATE ON subsystem_events
BEGIN
    SELECT RAISE(ABORT, 'subsystem events are append-only');
END;

CREATE TRIGGER subsystem_events_forbid_delete
BEFORE DELETE ON subsystem_events
BEGIN
    SELECT RAISE(ABORT, 'subsystem events are append-only');
END;

CREATE TRIGGER subsystem_events_chain_links_to_its_predecessor
BEFORE INSERT ON subsystem_events
FOR EACH ROW
WHEN NEW.prev_event_hash IS NOT (
        SELECT event_hash FROM subsystem_events ORDER BY sequence DESC LIMIT 1
    )
BEGIN
    SELECT RAISE(ABORT, 'subsystem event must carry its predecessor''s hash');
END;
"#;

const MIGRATION_V11_SQL: &str = r#"
CREATE TABLE permission_decisions (
    request_id TEXT PRIMARY KEY,
    run_id TEXT,
    attribution TEXT NOT NULL CHECK (attribution IN (
        'ledger-run', 'unregistered-run', 'unknown',
        'unattributed.user-action', 'unattributed.scheduled',
        'unattributed.inbound-request', 'unattributed.startup',
        'unattributed.shared-transport'
    )),
    process_id TEXT,
    tool_name TEXT NOT NULL,
    tool_call_id TEXT NOT NULL,
    operation_sha256 TEXT NOT NULL CHECK (length(operation_sha256) = 64),
    mode TEXT NOT NULL,
    risk_level TEXT CHECK (risk_level IS NULL OR risk_level IN ('low', 'medium', 'high')),
    risk_floored INTEGER NOT NULL CHECK (risk_floored IN (0, 1)),
    requested_at_ms INTEGER NOT NULL CHECK (requested_at_ms > 0),
    expires_at_ms INTEGER NOT NULL CHECK (expires_at_ms > 0),
    decision TEXT CHECK (decision IS NULL OR decision IN
        ('allow_once', 'allow_for_run', 'deny', 'expired')),
    decided_by TEXT,
    decided_at_ms INTEGER CHECK (decided_at_ms IS NULL OR decided_at_ms > 0),
    CHECK ((attribution IN ('ledger-run', 'unregistered-run')) = (run_id IS NOT NULL)),
    CHECK ((decision IS NULL) = (decided_at_ms IS NULL)),
    CHECK ((decision IS NULL) = (decided_by IS NULL))
) STRICT;

CREATE INDEX permission_decisions_tool_call_idx
    ON permission_decisions(tool_call_id, requested_at_ms);
CREATE INDEX permission_decisions_run_idx
    ON permission_decisions(run_id, requested_at_ms) WHERE run_id IS NOT NULL;
CREATE INDEX permission_decisions_undecided_idx
    ON permission_decisions(expires_at_ms) WHERE decision IS NULL;

CREATE TRIGGER permission_decisions_decide_once
BEFORE UPDATE ON permission_decisions
FOR EACH ROW
BEGIN
    SELECT CASE WHEN OLD.decision IS NOT NULL
        THEN RAISE(ABORT, 'a permission decision is final') END;
    SELECT CASE WHEN NEW.run_id IS NOT OLD.run_id
                  OR NEW.attribution IS NOT OLD.attribution
                  OR NEW.process_id IS NOT OLD.process_id
                  OR NEW.tool_name IS NOT OLD.tool_name
                  OR NEW.tool_call_id IS NOT OLD.tool_call_id
                  OR NEW.operation_sha256 IS NOT OLD.operation_sha256
                  OR NEW.mode IS NOT OLD.mode
                  OR NEW.risk_level IS NOT OLD.risk_level
                  OR NEW.risk_floored IS NOT OLD.risk_floored
                  OR NEW.requested_at_ms IS NOT OLD.requested_at_ms
                  OR NEW.expires_at_ms IS NOT OLD.expires_at_ms
        THEN RAISE(ABORT, 'what was asked cannot change once it has been asked') END;
END;

CREATE TRIGGER permission_decisions_forbid_delete
BEFORE DELETE ON permission_decisions
BEGIN
    SELECT RAISE(ABORT, 'permission decisions are append-only');
END;
"#;

/// Names the K1 process each event came from (roadmap K12).
///
/// Deliberately **not** a foreign key onto `agent_processes`. An FK would make a
/// stale or already-reaped process id fail the *event* insert, and losing an
/// event to protect a join is the wrong trade for an append-only log. A
/// dangling id is honest data; a missing event is not.
///
/// ponytail: outer-join to read it, since an id may name a row that no longer
/// exists. Add the FK if `agent_processes` ever becomes strictly append-only too.
const MIGRATION_V10_SQL: &str = r#"
ALTER TABLE run_events ADD COLUMN process_id TEXT;

CREATE INDEX run_events_process_idx ON run_events(process_id) WHERE process_id IS NOT NULL;
"#;

/// Hash-chains the run event stream (roadmap K12).
///
/// **Deliberately does not backfill.** Hashing the rows already on disk would
/// compute a chain over whatever those rows currently say and then certify it —
/// so a row edited *before* this migration ran would be laundered into a valid
/// chain, and the feature would assert an integrity property it does not have.
/// Both columns are therefore nullable, `NULL` means "written before chaining
/// existed, and outside the chain's coverage", and
/// [`RunLedger::verify_run_chain`] reports where coverage begins instead of
/// implying it begins at sequence 1.
///
/// The two triggers make the *linkage* structural even for a writer that never
/// goes through Rust — SQLite cannot compute SHA-256, so the hash's *content* is
/// checked by `verify_run_chain`, but "this row points at its predecessor" and
/// "a chained run cannot silently stop being chained" are enforced here.
const MIGRATION_V9_SQL: &str = r#"
ALTER TABLE run_events ADD COLUMN event_hash TEXT
    CHECK (event_hash IS NULL OR length(event_hash) = 64);
ALTER TABLE run_events ADD COLUMN prev_event_hash TEXT
    CHECK (prev_event_hash IS NULL OR length(prev_event_hash) = 64);

CREATE TRIGGER run_events_chain_links_to_its_predecessor
BEFORE INSERT ON run_events
FOR EACH ROW
WHEN (
        SELECT event_hash FROM run_events
        WHERE run_id = NEW.run_id AND sequence = NEW.sequence - 1
    ) IS NOT NULL
    AND NEW.prev_event_hash IS NOT (
        SELECT event_hash FROM run_events
        WHERE run_id = NEW.run_id AND sequence = NEW.sequence - 1
    )
BEGIN
    SELECT RAISE(ABORT, 'run event must carry its predecessor''s hash');
END;

CREATE TRIGGER run_events_chain_must_not_stop
BEFORE INSERT ON run_events
FOR EACH ROW
WHEN NEW.event_hash IS NULL
    AND (
        SELECT event_hash FROM run_events
        WHERE run_id = NEW.run_id AND sequence = NEW.sequence - 1
    ) IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'a chained run cannot append an unchained event');
END;
"#;

const MIGRATION_V7_SQL: &str = r#"
ALTER TABLE agent_processes ADD COLUMN kill_requested INTEGER NOT NULL DEFAULT 0
    CHECK (kill_requested IN (0, 1));

CREATE TRIGGER agent_processes_kill_implies_stop
BEFORE UPDATE OF kill_requested ON agent_processes
WHEN NEW.kill_requested = 1 AND NEW.stop_requested <> 1
BEGIN
    SELECT RAISE(ABORT, 'kill_requested implies stop_requested');
END;
"#;

const MIGRATION_V6_SQL: &str = r#"
ALTER TABLE agent_processes ADD COLUMN stop_requested INTEGER NOT NULL DEFAULT 0
    CHECK (stop_requested IN (0, 1));
ALTER TABLE agent_processes ADD COLUMN suspend_requested INTEGER NOT NULL DEFAULT 0
    CHECK (suspend_requested IN (0, 1));
ALTER TABLE agent_processes ADD COLUMN signal_reason TEXT;
ALTER TABLE agent_processes ADD COLUMN signal_requested_at_ms INTEGER
    CHECK (signal_requested_at_ms IS NULL OR signal_requested_at_ms > 0);

CREATE INDEX agent_processes_pending_signal_idx ON agent_processes(kind)
    WHERE state <> 'exited' AND (stop_requested = 1 OR suspend_requested = 1);
"#;

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
        // No projection: a routing decision changes no run status. It is a
        // fact about *why* this run's target was chosen, recorded beside the
        // snapshot that records what it was.
        RunEvent::RoutingDecided { .. } => ("routing_decided", None, Projection::None),
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
        // Neither half of a migration changes the run's status, and that is
        // deliberate. A departure is an attempt the target can still refuse, so
        // it must not close the run; an arrival re-opens nothing, because the
        // target's `runs` row was just inserted from the frozen spec and is
        // already `queued`. What both events carry is the chain link, which is
        // in the envelope and therefore already covered by the row hash.
        RunEvent::MigrationDeparted { .. } => ("migration_departed", None, Projection::None),
        RunEvent::MigrationArrived { .. } => ("migration_arrived", None, Projection::None),
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

/// One link of the subsystem chain: the sequence and the two hashes, and
/// deliberately nothing else. See [`RunLedger::subsystem_chain_head`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainLink {
    pub sequence: u64,
    pub event_hash: String,
    /// `None` only for the very first event in the stream.
    pub previous_hash: Option<String>,
}

struct ChainLinkRow {
    sequence: i64,
    event_hash: String,
    previous_hash: Option<String>,
}

fn chain_link_columns(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChainLinkRow> {
    Ok(ChainLinkRow {
        sequence: row.get(0)?,
        event_hash: row.get(1)?,
        previous_hash: row.get(2)?,
    })
}

fn decode_chain_link(row: ChainLinkRow) -> LedgerResult<ChainLink> {
    Ok(ChainLink {
        sequence: from_sql_u64(row.sequence, "sequence")?,
        event_hash: row.event_hash,
        previous_hash: row.previous_hash,
    })
}

/// A `subsystem_events` row as SQLite hands it over, for the reason
/// [`PermissionDecisionRow`] exists: the closure rusqlite calls may only fail
/// with a rusqlite error, while parsing the three enum columns fails with a
/// [`LedgerError::Corrupt`].
struct SubsystemEventRow {
    sequence: i64,
    event_id: String,
    subsystem: String,
    action: String,
    occurred_at_ms: i64,
    run_id: Option<String>,
    attribution: String,
    process_id: Option<String>,
    permission_request_id: Option<String>,
    outcome: String,
}

fn subsystem_event_columns(row: &rusqlite::Row<'_>) -> rusqlite::Result<SubsystemEventRow> {
    Ok(SubsystemEventRow {
        sequence: row.get(0)?,
        event_id: row.get(1)?,
        subsystem: row.get(2)?,
        action: row.get(3)?,
        occurred_at_ms: row.get(4)?,
        run_id: row.get(5)?,
        attribution: row.get(6)?,
        process_id: row.get(7)?,
        permission_request_id: row.get(8)?,
        outcome: row.get(9)?,
    })
}

fn decode_subsystem_event(row: SubsystemEventRow) -> LedgerResult<StoredSubsystemEvent> {
    Ok(StoredSubsystemEvent {
        sequence: from_sql_u64(row.sequence, "sequence")?,
        event_id: row.event_id,
        subsystem: Subsystem::parse(&row.subsystem)?,
        action: row.action,
        occurred_at_ms: from_sql_u64(row.occurred_at_ms, "occurred_at_ms")?,
        run_id: row.run_id,
        attribution: PermissionAttribution::parse(&row.attribution)?,
        process_id: row.process_id,
        permission_request_id: row.permission_request_id,
        outcome: SubsystemOutcome::parse(&row.outcome)?,
    })
}

/// The column list every `permission_decisions` read shares, so the two readers
/// cannot drift out of order with [`permission_decision_columns`].
const PERMISSION_DECISION_SELECT: &str = "SELECT request_id, run_id, attribution, process_id, \
     tool_name, tool_call_id, operation_sha256, mode, risk_level, risk_floored, \
     requested_at_ms, expires_at_ms, decision, decided_by, decided_at_ms, \
     tool_call_origin \
     FROM permission_decisions";

/// A `permission_decisions` row exactly as SQLite hands it over. Kept separate
/// from [`StoredPermissionDecision`] because the closure rusqlite calls may only
/// fail with a rusqlite error, while parsing the three enum columns fails with a
/// [`LedgerError::Corrupt`].
struct PermissionDecisionRow {
    request_id: String,
    run_id: Option<String>,
    attribution: String,
    process_id: Option<String>,
    tool_name: String,
    tool_call_id: String,
    operation_sha256: String,
    mode: String,
    risk_level: Option<String>,
    risk_floored: i64,
    requested_at_ms: i64,
    expires_at_ms: i64,
    decision: Option<String>,
    decided_by: Option<String>,
    decided_at_ms: Option<i64>,
    tool_call_origin: String,
}

fn permission_decision_columns(row: &rusqlite::Row<'_>) -> rusqlite::Result<PermissionDecisionRow> {
    Ok(PermissionDecisionRow {
        request_id: row.get(0)?,
        run_id: row.get(1)?,
        attribution: row.get(2)?,
        process_id: row.get(3)?,
        tool_name: row.get(4)?,
        tool_call_id: row.get(5)?,
        operation_sha256: row.get(6)?,
        mode: row.get(7)?,
        risk_level: row.get(8)?,
        risk_floored: row.get(9)?,
        requested_at_ms: row.get(10)?,
        expires_at_ms: row.get(11)?,
        decision: row.get(12)?,
        decided_by: row.get(13)?,
        decided_at_ms: row.get(14)?,
        tool_call_origin: row.get(15)?,
    })
}

fn decode_permission_decision(
    row: PermissionDecisionRow,
) -> LedgerResult<StoredPermissionDecision> {
    Ok(StoredPermissionDecision {
        request: PermissionRequestRecord {
            request_id: row.request_id,
            run_id: row.run_id,
            attribution: PermissionAttribution::parse(&row.attribution)?,
            process_id: row.process_id,
            tool_name: row.tool_name,
            tool_call_id: row.tool_call_id,
            tool_call_origin: ToolCallOrigin::parse(&row.tool_call_origin)?,
            operation_sha256: row.operation_sha256,
            mode: row.mode,
            risk_level: row
                .risk_level
                .as_deref()
                .map(parse_risk_level)
                .transpose()?,
            risk_floored: row.risk_floored != 0,
            requested_at_ms: from_sql_u64(row.requested_at_ms, "requested_at_ms")?,
            expires_at_ms: from_sql_u64(row.expires_at_ms, "expires_at_ms")?,
        },
        decision: row
            .decision
            .as_deref()
            .map(parse_permission_decision)
            .transpose()?,
        decided_by: row.decided_by,
        decided_at_ms: row
            .decided_at_ms
            .map(|value| from_sql_u64(value, "decided_at_ms"))
            .transpose()?,
    })
}

fn parse_risk_level(value: &str) -> LedgerResult<RiskLevel> {
    match value {
        "low" => Ok(RiskLevel::Low),
        "medium" => Ok(RiskLevel::Medium),
        "high" => Ok(RiskLevel::High),
        other => Err(LedgerError::Corrupt(format!(
            "unknown risk level '{other}'"
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
                egress_allowlist: None,
                channel_send: None,
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

    /// K12's acceptance in one sentence: "a tool call whose authorizing decision
    /// cannot be produced from the log is a bug."
    ///
    /// `permission_decisions_for_tool_call` can only answer that for a call
    /// somebody already suspected. This asks it of every call in a run, which is
    /// the difference between a question and a check.
    #[test]
    fn a_runs_ungated_mutating_call_is_a_gap_and_an_ungated_read_is_not() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        ledger.submit_run(&spec("run-gaps", "submit/gaps")).unwrap();
        ledger.append_event(&queued("run-gaps", 1)).unwrap();

        let proposed = |sequence: u64, id: &str, tool: &str, mutation: bool| {
            envelope(
                "run-gaps",
                sequence,
                &format!("proposed-{id}"),
                RunEvent::ToolProposed {
                    tool_call_id: id.to_string(),
                    tool_name: tool.to_string(),
                    arguments: crate::run_protocol::RedactedPayload {
                        value: serde_json::json!({}),
                        redaction: crate::run_protocol::RedactionState::NotNeeded,
                    },
                    arguments_sha256: "b".repeat(64),
                    mutation,
                },
            )
        };
        ledger
            .append_event(&proposed(2, "tool-read", "read_file", false))
            .unwrap();
        ledger
            .append_event(&proposed(3, "tool-write", "write_file", true))
            .unwrap();
        // A call that started with nothing proposing it: the log cannot say what
        // it was allowed to do.
        ledger
            .append_event(&envelope(
                "run-gaps",
                4,
                "started-orphan",
                RunEvent::ToolStarted {
                    tool_call_id: "tool-orphan".to_string(),
                },
            ))
            .unwrap();

        let gaps = ledger.permission_gaps("run-gaps").unwrap();
        assert_eq!(
            gaps.iter()
                .map(|gap| gap.tool_call_id.as_str())
                .collect::<Vec<_>>(),
            vec!["tool-read", "tool-write", "tool-orphan"],
            "every ungated call is reported, in the run's own order"
        );
        assert_eq!(
            gaps.iter()
                .filter(|gap| gap.is_unauthorized_mutation())
                .map(|gap| gap.tool_call_id.as_str())
                .collect::<Vec<_>>(),
            vec!["tool-write", "tool-orphan"],
            "reading a file is not an authorization event, but an unknown call \
             counts as a bug rather than being waved through"
        );

        // Gate the write, and it stops being a gap — the check reads the log
        // rather than a list of tool names.
        let mut request = permission_request("req-write", PermissionAttribution::LedgerRun);
        request.run_id = Some("run-gaps".to_string());
        request.tool_call_id = "tool-write".to_string();
        ledger.record_permission_request(&request).unwrap();
        let after = ledger.permission_gaps("run-gaps").unwrap();
        assert!(
            !after.iter().any(|gap| gap.tool_call_id == "tool-write"),
            "{after:?}"
        );

        assert!(matches!(
            ledger.permission_gaps("run-missing"),
            Err(LedgerError::NotFound { entity: "run", .. })
        ));
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

    /// Length-prefixing is the whole reason the canonicalization is not a join.
    /// Concatenated, `("ab", "c")` and `("a", "bc")` are the same bytes — so a
    /// naive chain would let one event be rewritten as a different event with an
    /// unchanged hash, which is precisely the tamper this feature exists to
    /// catch. The two calls below differ only in where the field boundary falls.
    #[test]
    fn the_chain_hash_cannot_be_forged_by_moving_a_field_boundary() {
        let left = event_chain_hash(
            None, "ab", "c", 1, 2_000, None, "queued", b"{}", b"{}", None, false, None,
        );
        let right = event_chain_hash(
            None, "a", "bc", 1, 2_000, None, "queued", b"{}", b"{}", None, false, None,
        );
        assert_ne!(
            left, right,
            "concatenating fields without their lengths would make these collide"
        );

        // An absent optional and an empty one must also differ, or a row with
        // `actor_id = NULL` could be rewritten to `actor_id = ''` for free.
        assert_ne!(
            event_chain_hash(
                None, "e", "r", 1, 2_000, None, "queued", b"{}", b"{}", None, false, None,
            ),
            event_chain_hash(
                None,
                "e",
                "r",
                1,
                2_000,
                Some(""),
                "queued",
                b"{}",
                b"{}",
                None,
                false,
                None,
            ),
            "a NULL actor and an empty-string actor are different rows"
        );
        assert_eq!(
            left.len(),
            64,
            "the column's CHECK constraint expects 64 hex chars"
        );
    }

    /// V10 added a column to a table the chain already covered. The escape from
    /// "break every V9 row or leave the column unprotected" is that `process_id`
    /// contributes nothing when absent — so a row with no process hashes exactly
    /// as it did before the column existed, while every mutation of a present one
    /// is caught. This test is the contract for that, and it is the reason a
    /// future column may be added the same way.
    #[test]
    fn a_process_id_is_covered_when_present_and_costs_nothing_when_absent() {
        let without = event_chain_hash(
            None, "e", "r", 1, 2_000, None, "queued", b"{}", b"{}", None, false, None,
        );
        let with = event_chain_hash(
            None,
            "e",
            "r",
            1,
            2_000,
            None,
            "queued",
            b"{}",
            b"{}",
            None,
            false,
            Some("p-turn-1"),
        );
        let other = event_chain_hash(
            None,
            "e",
            "r",
            1,
            2_000,
            None,
            "queued",
            b"{}",
            b"{}",
            None,
            false,
            Some("p-turn-2"),
        );

        assert_ne!(with, without, "setting a process id must change the digest");
        assert_ne!(with, other, "changing it must change the digest");
        // The load-bearing assertion: an event with no process is byte-identical
        // to what V9 produced, so every row written before V10 still verifies.
        assert_eq!(
            without,
            {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                let mut field = |bytes: &[u8]| {
                    hasher.update((bytes.len() as u64).to_be_bytes());
                    hasher.update(bytes);
                };
                field(CHAIN_HASH_DOMAIN);
                field(b"");
                field(&[0]);
                field(b"e");
                field(b"r");
                field(&1u64.to_be_bytes());
                field(&2_000i64.to_be_bytes());
                field(b"");
                field(&[0]);
                field(b"queued");
                field(b"{}");
                field(b"{}");
                field(b"");
                field(&[0]);
                field(&[0]);
                const HEX: &[u8; 16] = b"0123456789abcdef";
                hasher
                    .finalize()
                    .iter()
                    .fold(String::new(), |mut out, byte| {
                        out.push(HEX[(byte >> 4) as usize] as char);
                        out.push(HEX[(byte & 0x0f) as usize] as char);
                        out
                    })
            },
            "the V9 field list, spelled out — a change here silently invalidates \
             every chain already on disk"
        );
    }

    /// Rewriting which process an event came from breaks the chain, which is the
    /// point of covering the column rather than merely adding it.
    #[test]
    fn editing_an_events_process_id_breaks_the_chain() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        ledger
            .submit_run(&spec("run-process", "submit/process"))
            .unwrap();
        ledger.append_event(&queued("run-process", 1)).unwrap();
        ledger
            .connection
            .execute_batch("DROP TRIGGER run_events_forbid_update")
            .unwrap();
        ledger
            .connection
            .execute(
                "UPDATE run_events SET process_id = 'p-someone-else'
                 WHERE run_id = 'run-process' AND sequence = 1",
                [],
            )
            .unwrap();

        match ledger.verify_run_chain("run-process").unwrap() {
            ChainVerification::Broken { sequence, detail } => {
                assert_eq!(sequence, 1);
                assert!(
                    detail.contains("do not match its recorded hash"),
                    "got {detail}"
                );
            }
            other => panic!("attributing an event to another process must break it, got {other:?}"),
        }
    }

    /// With no ambient scope there is no process to name, and the column records
    /// that rather than guessing one from the run — which it could not do
    /// correctly anyway, since a run owns many processes.
    #[test]
    fn an_append_with_no_process_scope_records_no_process() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        ledger
            .submit_run(&spec("run-scopeless", "submit/scopeless"))
            .unwrap();
        ledger.append_event(&queued("run-scopeless", 1)).unwrap();

        let process_id = ledger
            .connection
            .query_row(
                "SELECT process_id FROM run_events WHERE run_id = 'run-scopeless'",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap();
        assert_eq!(process_id, None);
        assert!(matches!(
            ledger.verify_run_chain("run-scopeless").unwrap(),
            ChainVerification::Intact { .. }
        ));
    }

    /// The wiring that makes the column worth having: an append inside a process
    /// scope names that process, with no change at any of the 46 call sites.
    #[test]
    fn an_append_inside_a_process_scope_names_that_process() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        ledger
            .submit_run(&spec("run-scoped", "submit/scoped"))
            .unwrap();

        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(crate::run_scope::scoped_with_process(
                crate::run_scope::RunScope::run("run-scoped"),
                crate::run_scope::ProcessScope::new("p-turn-9"),
                async {
                    ledger.append_event(&queued("run-scoped", 1)).unwrap();
                },
            ));

        let process_id = ledger
            .connection
            .query_row(
                "SELECT process_id FROM run_events WHERE run_id = 'run-scoped'",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap();
        assert_eq!(process_id.as_deref(), Some("p-turn-9"));
        assert!(
            matches!(
                ledger.verify_run_chain("run-scoped").unwrap(),
                ChainVerification::Intact { .. }
            ),
            "a named process must still verify — the hash covers it"
        );
    }

    #[test]
    fn a_freshly_appended_run_verifies_from_its_first_event() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        ledger
            .submit_run(&spec("run-chain", "submit/chain"))
            .unwrap();
        for sequence in 1..=3 {
            ledger.append_event(&queued("run-chain", sequence)).unwrap();
        }

        assert_eq!(
            ledger.verify_run_chain("run-chain").unwrap(),
            ChainVerification::Intact {
                covered_from: Some(1),
                covered_through: Some(3),
                events_seen: 3,
                // No ambient process scope in this test, so none is named — the
                // gap is reported rather than assumed away.
                events_naming_a_process: 0,
            }
        );
    }

    /// Editing any column of any event breaks the chain at that event. The
    /// pre-existing `load_events` revalidation only catches a payload that stops
    /// being *valid protocol*; this catches a replacement that parses perfectly.
    /// The two halves of a migrated run are one chain, and the seam is a hash
    /// inside a hashed envelope rather than a column spanning two databases.
    #[test]
    fn a_migrated_run_joins_its_two_halves_through_the_departure_hash() {
        let mut origin = RunLedger::open_in_memory().unwrap();
        origin.submit_run(&spec("run-move", "submit/move")).unwrap();
        origin.append_event(&queued("run-move", 1)).unwrap();
        origin
            .append_event(&envelope(
                "run-move",
                2,
                "event-depart",
                RunEvent::MigrationDeparted {
                    target_node_id: "node-b".to_string(),
                    payload_sha256: "e".repeat(64),
                    checkpoint_id: "cp-01".to_string(),
                },
            ))
            .unwrap();
        let departure = origin
            .migration_departure("run-move")
            .unwrap()
            .expect("the tip is a departure");
        assert_eq!(departure.sequence, 2);
        assert_eq!(departure.target_node_id, "node-b");
        // The origin's own half still verifies on its own machine.
        assert!(matches!(
            origin.verify_run_chain("run-move").unwrap(),
            ChainVerification::Intact { .. }
        ));

        let mut target = RunLedger::open_in_memory().unwrap();
        target.submit_run(&spec("run-move", "submit/move")).unwrap();
        target
            .append_event(&envelope(
                "run-move",
                1,
                "event-arrive",
                RunEvent::MigrationArrived {
                    origin_node_id: "node-a".to_string(),
                    origin_last_sequence: departure.sequence,
                    origin_last_event_hash: departure.event_hash.clone(),
                    payload_sha256: departure.payload_sha256.clone(),
                },
            ))
            .unwrap();
        let arrival = target
            .migration_arrival("run-move")
            .unwrap()
            .expect("the target's half opens with an arrival");

        match join_migration_chain(&departure, &arrival) {
            MigrationChainJoin::Joined {
                run_id,
                origin_node_id,
                target_node_id,
                origin_last_sequence,
                ..
            } => {
                assert_eq!(run_id, "run-move");
                assert_eq!(origin_node_id, "node-a");
                assert_eq!(target_node_id, "node-b");
                assert_eq!(origin_last_sequence, 2);
            }
            other => panic!("expected a join, got {other:?}"),
        }
    }

    /// Every disagreement between the halves is a break, not a warning — an
    /// auditor must not be able to read "audited" off a chain that does not meet.
    #[test]
    fn a_half_that_does_not_meet_the_other_is_a_broken_join() {
        let departure = MigrationDeparture {
            run_id: "run-move".to_string(),
            sequence: 4,
            event_hash: "a".repeat(64),
            target_node_id: "node-b".to_string(),
            payload_sha256: "e".repeat(64),
            checkpoint_id: "cp-01".to_string(),
        };
        let sound = MigrationArrival {
            run_id: "run-move".to_string(),
            origin_node_id: "node-a".to_string(),
            origin_last_sequence: 4,
            origin_last_event_hash: "a".repeat(64),
            payload_sha256: "e".repeat(64),
            event_hash: "f".repeat(64),
        };
        assert!(matches!(
            join_migration_chain(&departure, &sound),
            MigrationChainJoin::Joined { .. }
        ));

        for mutate in [
            (|arrival: &mut MigrationArrival| arrival.run_id = "run-other".to_string())
                as fn(&mut MigrationArrival),
            |arrival| arrival.origin_last_event_hash = "b".repeat(64),
            |arrival| arrival.origin_last_sequence = 5,
            |arrival| arrival.payload_sha256 = "d".repeat(64),
        ] {
            let mut arrival = sound.clone();
            mutate(&mut arrival);
            assert!(
                matches!(
                    join_migration_chain(&departure, &arrival),
                    MigrationChainJoin::Broken { .. }
                ),
                "a disagreeing half must not join"
            );
        }
    }

    /// A run that departed, was refused, and carried on locally has a departure
    /// in its history that no longer describes where it is — so the tip, and
    /// only the tip, may anchor a join.
    #[test]
    fn a_departure_that_is_no_longer_the_tip_cannot_anchor_a_join() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        ledger
            .submit_run(&spec("run-stayed", "submit/stayed"))
            .unwrap();
        ledger
            .append_event(&envelope(
                "run-stayed",
                1,
                "event-depart",
                RunEvent::MigrationDeparted {
                    target_node_id: "node-b".to_string(),
                    payload_sha256: "e".repeat(64),
                    checkpoint_id: "cp-01".to_string(),
                },
            ))
            .unwrap();
        assert!(ledger.migration_departure("run-stayed").unwrap().is_some());

        // The target refused, and the run continued here.
        ledger.append_event(&queued("run-stayed", 2)).unwrap();
        assert!(
            ledger.migration_departure("run-stayed").unwrap().is_none(),
            "a superseded departure is not a handover"
        );
    }

    #[test]
    fn editing_an_event_breaks_the_chain_at_that_event() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        ledger.submit_run(&spec("run-edit", "submit/edit")).unwrap();
        for sequence in 1..=3 {
            ledger.append_event(&queued("run-edit", sequence)).unwrap();
        }

        // A change that is entirely valid protocol and entirely plausible: the
        // second event now claims to have happened a minute later.
        ledger
            .connection
            .execute_batch("DROP TRIGGER run_events_forbid_update")
            .unwrap();
        ledger
            .connection
            .execute(
                "UPDATE run_events SET occurred_at_ms = occurred_at_ms + 60000
                 WHERE run_id = 'run-edit' AND sequence = 2",
                [],
            )
            .unwrap();

        match ledger.verify_run_chain("run-edit").unwrap() {
            ChainVerification::Broken { sequence, detail } => {
                assert_eq!(sequence, 2);
                assert!(
                    detail.contains("do not match its recorded hash"),
                    "got {detail}"
                );
            }
            other => panic!("an edited timestamp must break the chain, got {other:?}"),
        }
    }

    /// Deleting an interior event is caught by the link check rather than by the
    /// hash check: the survivors' own hashes are still correct, but the event
    /// after the hole now points at a predecessor that is not there.
    #[test]
    fn deleting_an_interior_event_breaks_the_link_to_its_predecessor() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        ledger.submit_run(&spec("run-hole", "submit/hole")).unwrap();
        for sequence in 1..=3 {
            ledger.append_event(&queued("run-hole", sequence)).unwrap();
        }

        ledger
            .connection
            .execute_batch("DROP TRIGGER run_events_forbid_delete")
            .unwrap();
        ledger
            .connection
            .execute(
                "DELETE FROM run_events WHERE run_id = 'run-hole' AND sequence = 2",
                [],
            )
            .unwrap();

        match ledger.verify_run_chain("run-hole").unwrap() {
            ChainVerification::Broken { sequence, detail } => {
                assert_eq!(sequence, 3, "the event after the hole is where it shows");
                assert!(detail.contains("does not link"), "got {detail}");
            }
            other => panic!("a deleted interior event must break the chain, got {other:?}"),
        }
    }

    /// Truncating the newest events leaves every surviving hash correct and every
    /// link intact, so the chain alone cannot see it. `runs.last_sequence` can:
    /// the projection trigger maintains it, so the run still claims events that
    /// are gone, and concealing that needs a second edit to a different table.
    #[test]
    fn truncating_the_newest_events_is_caught_by_the_runs_projection() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        ledger
            .submit_run(&spec("run-truncated", "submit/truncated"))
            .unwrap();
        for sequence in 1..=4 {
            ledger
                .append_event(&queued("run-truncated", sequence))
                .unwrap();
        }

        ledger
            .connection
            .execute_batch("DROP TRIGGER run_events_forbid_delete")
            .unwrap();
        ledger
            .connection
            .execute(
                "DELETE FROM run_events WHERE run_id = 'run-truncated' AND sequence >= 3",
                [],
            )
            .unwrap();

        match ledger.verify_run_chain("run-truncated").unwrap() {
            ChainVerification::Broken { sequence, detail } => {
                assert_eq!(sequence, 4);
                assert!(detail.contains("events were removed"), "got {detail}");
            }
            other => panic!("a truncated tail must be reported, got {other:?}"),
        }
    }

    /// A run whose events predate V9 is reported as covered from the first
    /// *chained* event, not from sequence 1. Backfilling those rows instead would
    /// have hashed whatever they currently say and certified it — laundering an
    /// edit made before chaining existed into a valid chain.
    #[test]
    fn events_written_before_the_chain_existed_are_reported_outside_its_coverage() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        ledger
            .submit_run(&spec("run-legacy", "submit/legacy"))
            .unwrap();
        for sequence in 1..=2 {
            ledger
                .append_event(&queued("run-legacy", sequence))
                .unwrap();
        }
        // Simulate rows written before V9 by clearing their hashes.
        ledger
            .connection
            .execute_batch("DROP TRIGGER run_events_forbid_update")
            .unwrap();
        ledger
            .connection
            .execute(
                "UPDATE run_events SET event_hash = NULL, prev_event_hash = NULL
                 WHERE run_id = 'run-legacy'",
                [],
            )
            .unwrap();
        ledger.append_event(&queued("run-legacy", 3)).unwrap();

        assert_eq!(
            ledger.verify_run_chain("run-legacy").unwrap(),
            ChainVerification::Intact {
                covered_from: Some(3),
                covered_through: Some(3),
                events_seen: 3,
                events_naming_a_process: 0,
            },
            "coverage begins where hashing began, and says so"
        );
    }

    /// The linkage is enforced in SQL, so it holds against a writer that never
    /// goes through this module. SQLite cannot compute SHA-256, so the hash's
    /// *content* is `verify_run_chain`'s job — but "points at its predecessor"
    /// and "a chained run does not silently stop being chained" are the
    /// database's.
    #[test]
    fn a_direct_sql_writer_cannot_append_an_event_outside_the_chain() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        ledger
            .submit_run(&spec("run-bypass", "submit/bypass"))
            .unwrap();
        ledger.append_event(&queued("run-bypass", 1)).unwrap();

        let unchained = ledger.connection.execute(
            "INSERT INTO run_events (
                 event_id, run_id, sequence, occurred_at_ms, actor_id,
                 emitter_json, event_type, envelope_json, derived_status, is_terminal
             ) VALUES ('event-raw', 'run-bypass', 2, 3000, NULL,
                       CAST('{}' AS BLOB), 'queued', CAST('{}' AS BLOB), NULL, 0)",
            [],
        );
        let message = unchained
            .expect_err("an unchained append must abort")
            .to_string();
        assert!(
            message.contains("cannot append an unchained event"),
            "got {message}"
        );

        let wrong_link = ledger.connection.execute(
            "INSERT INTO run_events (
                 event_id, run_id, sequence, occurred_at_ms, actor_id,
                 emitter_json, event_type, envelope_json, derived_status, is_terminal,
                 event_hash, prev_event_hash
             ) VALUES ('event-raw', 'run-bypass', 2, 3000, NULL,
                       CAST('{}' AS BLOB), 'queued', CAST('{}' AS BLOB), NULL, 0,
                       ?1, ?2)",
            params!["a".repeat(64), "b".repeat(64)],
        );
        let message = wrong_link
            .expect_err("a wrong predecessor hash must abort")
            .to_string();
        assert!(message.contains("predecessor"), "got {message}");
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

    /// V8's columns are the resource ledger, and every one of them has to be
    /// nullable: NULL is how the ledger says "not measured", so a `NOT NULL`
    /// column here would force a zero for every process nobody sampled.
    #[test]
    fn migration_v8_adds_nullable_measurement_columns_and_is_forward_only() {
        let database = TempDb::new("migration-v8");
        {
            let ledger = RunLedger::open(&database.path).unwrap();
            let nullable: Vec<(String, i64)> = ledger
                .connection
                .prepare("SELECT name, \"notnull\" FROM pragma_table_info('agent_processes')")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            for column in [
                "cpu_time_ms",
                "peak_rss_bytes",
                "bytes_read",
                "bytes_written",
                "bytes_egressed",
                "tokens_in",
                "tokens_out",
                "gpu_resident_bytes",
                "gpu_device_ms",
                "usage_unavailable_json",
            ] {
                let (_, not_null) = nullable
                    .iter()
                    .find(|(name, _)| name == column)
                    .unwrap_or_else(|| panic!("V8 must add {column}"));
                assert_eq!(
                    *not_null, 0,
                    "{column} must be nullable: NULL means unmeasured"
                );
            }
            // The ladder head, not V8's own number, and read from the ladder
            // rather than written out: the applier derives the pragma from
            // `SCHEMA_VERSION` too, so neither side needs bumping by hand.
            assert_eq!(
                ledger
                    .connection
                    .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                SCHEMA_VERSION
            );
        }
        // A database written by a newer build is refused only when that build
        // said so. This is the whole point of V13: a future migration that only
        // *adds* costs a rollback nothing, and one that rejects this binary's
        // writes still shuts the door.
        //
        // The version is derived from the ladder head rather than written out. It
        // has to be exactly one *above* it — a literal equal to the head asserts
        // a checksum mismatch instead, which is a different test passing for the
        // wrong reason — and a literal is exactly what went stale the next time a
        // migration landed.
        let from_the_future = SCHEMA_VERSION + 1;
        {
            let connection = Connection::open(&database.path).unwrap();
            connection
                .execute(
                    "INSERT INTO schema_migrations
                        (version, checksum, applied_at_ms, min_reader_version)
                     VALUES (?1, 'from-the-future-additive', 21, ?2)",
                    params![from_the_future, SCHEMA_VERSION],
                )
                .unwrap();
        }
        assert!(
            RunLedger::open(&database.path).is_ok(),
            "a future migration that declares this binary still safe must open"
        );

        {
            let connection = Connection::open(&database.path).unwrap();
            connection
                .execute(
                    "UPDATE schema_migrations SET min_reader_version = ?1 WHERE version = ?1",
                    params![from_the_future],
                )
                .unwrap();
        }
        assert!(
            matches!(
                RunLedger::open(&database.path),
                Err(LedgerError::MigrationConflict { version }) if version == from_the_future
            ),
            "a future migration that requires a newer binary must still refuse"
        );
    }

    /// V18 is a whole-table rebuild, which is the one migration shape that can
    /// lose data silently: a forgotten column in the copy shifts every value one
    /// place, and a forgotten index or trigger removes a guard nothing will
    /// notice until it is needed.
    ///
    /// So this asserts all three — the rows survive with their values intact, the
    /// widened vocabulary is genuinely widened, and every index and trigger the
    /// dropped table carried is back.
    #[test]
    fn migration_v18_widens_the_kind_vocabulary_without_losing_rows_or_guards() {
        let database = TempDb::new("kind-rebuild");
        let ledger = RunLedger::open(&database.path).unwrap();
        let connection = &ledger.connection;

        // A row on every column family the rebuild had to carry across: an
        // identity, a limit, a signal latch, a measurement, and a terminal exit.
        connection
            .execute(
                "INSERT INTO agent_processes (
                     process_id, kind, external_id, state, created_at_ms, updated_at_ms,
                     started_at_ms, native_pid, max_wall_ms, stop_requested, signal_reason,
                     cpu_time_ms, usage_unavailable_json, max_context_tokens
                 ) VALUES (
                     'turn-rebuild', 'chat_turn', 'ext-rebuild', 'running', 10, 20,
                     15, 4242, 60000, 1, 'stopped by the user',
                     77, '{}', 8192
                 )",
                [],
            )
            .unwrap();

        // The row is untouched by the rebuild, value for value.
        let (kind, pid, wall, stop, reason, cpu, budget): (
            String,
            i64,
            i64,
            i64,
            String,
            i64,
            i64,
        ) = connection
            .query_row(
                "SELECT kind, native_pid, max_wall_ms, stop_requested, signal_reason,
                        cpu_time_ms, max_context_tokens
                 FROM agent_processes WHERE process_id = 'turn-rebuild'",
                [],
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
            .unwrap();
        assert_eq!(kind, "chat_turn");
        assert_eq!(pid, 4242);
        assert_eq!(wall, 60_000);
        assert_eq!(stop, 1);
        assert_eq!(reason, "stopped by the user");
        assert_eq!(cpu, 77);
        assert_eq!(budget, 8192);

        // The point of the migration: the new kind is accepted…
        connection
            .execute(
                "INSERT INTO agent_processes
                     (process_id, kind, external_id, state, created_at_ms, updated_at_ms)
                 VALUES ('browser-1', 'browser_session', 'sess-1', 'running', 30, 30)",
                [],
            )
            .unwrap();
        // …and the constraint is still a constraint, not merely absent.
        assert!(
            connection
                .execute(
                    "INSERT INTO agent_processes
                         (process_id, kind, external_id, state, created_at_ms, updated_at_ms)
                     VALUES ('bogus-1', 'not_a_kind', 'sess-2', 'running', 30, 30)",
                    [],
                )
                .is_err(),
            "widening the vocabulary must not drop the CHECK altogether"
        );

        // Every index and trigger `DROP TABLE` took with it is back.
        let mut objects: Vec<String> = connection
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE tbl_name = 'agent_processes' AND type IN ('index', 'trigger')
                   AND name NOT LIKE 'sqlite_%'",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        objects.sort();
        assert_eq!(
            objects,
            vec![
                "agent_processes_close_out_states_its_gaps",
                "agent_processes_forbid_identity_update",
                "agent_processes_kill_implies_stop",
                "agent_processes_kind_idx",
                "agent_processes_live_idx",
                "agent_processes_parent_idx",
                "agent_processes_pending_signal_idx",
                "agent_processes_run_idx",
                "agent_processes_validate_transition",
                "agent_processes_workspace_idx",
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        );

        // And a rebuilt trigger still fires, which the name alone does not prove.
        assert!(
            connection
                .execute(
                    "UPDATE agent_processes SET state = 'running'
                     WHERE process_id = 'turn-rebuild'",
                    [],
                )
                .is_ok(),
            "a no-op transition is legal"
        );
        assert!(
            connection
                .execute(
                    "UPDATE agent_processes SET kind = 'subagent'
                     WHERE process_id = 'turn-rebuild'",
                    [],
                )
                .is_err(),
            "the identity trigger must have come back with the table"
        );
    }

    /// V19 is the second whole-table rebuild, and the one whose *key* changes:
    /// SQLite permits NULLs in a non-`INTEGER` primary key, so a nullable
    /// `process_id` would have silently stopped deduplicating these rows. This
    /// asserts the rows survive, the unique index does the primary key's old job,
    /// and the exactly-one-attribution rule is enforced rather than assumed.
    #[test]
    fn migration_v19_keeps_attributed_destinations_and_admits_unattributed_ones() {
        let database = TempDb::new("egress-destinations-rebuild");
        let ledger = RunLedger::open(&database.path).unwrap();
        let connection = &ledger.connection;

        connection
            .execute(
                "INSERT INTO agent_processes
                     (process_id, kind, external_id, state, created_at_ms, updated_at_ms)
                 VALUES ('p-dest', 'chat_turn', 'ext-dest', 'running', 10, 10)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO egress_destinations
                     (process_id, scheme, host, port, requests, first_seen_ms, last_seen_ms)
                 VALUES ('p-dest', 'https', 'api.test', 443, 7, 100, 200)",
                [],
            )
            .unwrap();

        // The attributed row is untouched, and its `unattributed_reason` is NULL
        // rather than a back-filled string — it was never unattributed.
        let (requests, reason): (i64, Option<String>) = connection
            .query_row(
                "SELECT requests, unattributed_reason FROM egress_destinations
                  WHERE process_id = 'p-dest'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(requests, 7);
        assert_eq!(reason, None);

        // The same host under a reason is a *different* row.
        connection
            .execute(
                "INSERT INTO egress_destinations
                     (unattributed_reason, scheme, host, port, requests, first_seen_ms, last_seen_ms)
                 VALUES ('unattributed.startup', 'https', 'api.test', 443, 1, 100, 200)",
                [],
            )
            .unwrap();
        let rows: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM egress_destinations WHERE host = 'api.test'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2, "a process and a reason must not share a row");

        // The unique index still refuses a duplicate of either, which is the
        // primary key's old job.
        for duplicate in [
            "INSERT INTO egress_destinations
                 (process_id, scheme, host, port, requests, first_seen_ms, last_seen_ms)
             VALUES ('p-dest', 'https', 'api.test', 443, 1, 100, 200)",
            "INSERT INTO egress_destinations
                 (unattributed_reason, scheme, host, port, requests, first_seen_ms, last_seen_ms)
             VALUES ('unattributed.startup', 'https', 'api.test', 443, 1, 100, 200)",
        ] {
            assert!(
                connection.execute(duplicate, []).is_err(),
                "the unique index must still deduplicate: {duplicate}"
            );
        }

        // Exactly one attribution, enforced. Neither is a destination charged to
        // nothing; both is a row two readers would each count once.
        assert!(
            connection
                .execute(
                    "INSERT INTO egress_destinations
                         (scheme, host, port, requests, first_seen_ms, last_seen_ms)
                     VALUES ('https', 'orphan.test', 443, 1, 100, 200)",
                    [],
                )
                .is_err(),
            "a destination attributed to nothing must be refused"
        );
        assert!(
            connection
                .execute(
                    "INSERT INTO egress_destinations
                         (process_id, unattributed_reason, scheme, host, port, requests,
                          first_seen_ms, last_seen_ms)
                     VALUES ('p-dest', 'unattributed.startup', 'https', 'both.test', 443, 1, 100, 200)",
                    [],
                )
                .is_err(),
            "a destination attributed to both must be refused"
        );
    }

    /// The floor is derived from the ladder, so it cannot drift from the
    /// compatibility each migration declares.
    #[test]
    fn the_reader_floor_is_the_newest_breaking_migration_not_the_newest_one() {
        // V9 is the last breaking migration before the additive run, so
        // everything from V10 to V12 inherits V9's floor.
        assert_eq!(min_reader_version_for(MIGRATION_V9), MIGRATION_V9);
        assert_eq!(min_reader_version_for(MIGRATION_V10), MIGRATION_V9);
        assert_eq!(min_reader_version_for(MIGRATION_V11), MIGRATION_V9);
        assert_eq!(min_reader_version_for(MIGRATION_V12), MIGRATION_V9);
        // V13 requires itself: a V12 binary applies the old blanket guard and
        // refuses regardless of what this column says.
        assert_eq!(min_reader_version_for(MIGRATION_V13), MIGRATION_V13);
        // V14 is additive and therefore inherits V13's floor rather than raising
        // it — the first migration to actually spend what V13 bought.
        assert_eq!(min_reader_version_for(MIGRATION_V14), MIGRATION_V13);
        assert_eq!(min_reader_version_for(MIGRATION_V15), MIGRATION_V13);
        // …until V18 raises it again: a widened kind vocabulary is not something
        // an older binary can read past.
        assert_eq!(min_reader_version_for(MIGRATION_V17), MIGRATION_V13);
        assert_eq!(min_reader_version_for(MIGRATION_V18), MIGRATION_V18);
        // V19 relaxes a constraint rather than adding a kind, so a V18 binary
        // reads past it unchanged and it inherits V18's floor.
        assert_eq!(min_reader_version_for(MIGRATION_V19), MIGRATION_V18);
        // V20 widens the subsystem vocabulary itself, so like V18 it raises the
        // floor to itself.
        assert_eq!(min_reader_version_for(MIGRATION_V20), MIGRATION_V20);
        // V21 widens the *kind* vocabulary, exactly as V18 did, so it raises the
        // floor to itself for the same reason: an older binary's
        // `ProcessKind::parse` errors on `foreground_shell` rather than ignoring
        // it, which turns every process listing into a failure.
        assert_eq!(min_reader_version_for(MIGRATION_V21), MIGRATION_V21);
        // And the pre-V9 ladder keeps exactly its old behaviour.
        assert_eq!(min_reader_version_for(MIGRATION_V8), MIGRATION_V8);
        assert_eq!(min_reader_version_for(MIGRATION_V1), MIGRATION_V1);

        // The floor never exceeds the version asking for it, or a freshly
        // migrated database could not be opened by the binary that wrote it.
        for (version, _, _) in MIGRATION_LADDER {
            assert!(
                min_reader_version_for(*version) <= *version,
                "migration {version} claims a floor above itself"
            );
        }
    }

    /// A database this binary just wrote must record a floor this binary meets —
    /// the round trip the guard actually performs on every open.
    #[test]
    fn a_freshly_migrated_database_records_a_floor_this_binary_meets() {
        let database = TempDb::new("reader-floor");
        let ledger = RunLedger::open(&database.path).unwrap();
        let required = required_reader_version(&ledger.connection)
            .unwrap()
            .unwrap();
        // V21 is the newest breaking migration: it widened
        // `agent_processes.kind` to admit `foreground_shell`, which an older
        // binary's `ProcessKind::parse` rejects outright rather than ignoring.
        assert_eq!(required, MIGRATION_V21);
        assert!(required <= SCHEMA_VERSION);

        // No row may be left at the `DEFAULT 1` the ALTER used: that would claim
        // a compatibility nobody checked.
        let unstated: i64 = ledger
            .connection
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations
                 WHERE min_reader_version != ?1 AND min_reader_version = 1 AND version > 1",
                [MIGRATION_V1],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unstated, 0, "every row states a floor from the ladder");
    }

    #[test]
    fn migration_is_safe_to_rerun_and_installs_the_shared_profile_schema() {
        let database = TempDb::new("migration");
        {
            let ledger = RunLedger::open(&database.path).unwrap();
            assert_eq!(
                ledger.applied_migrations().unwrap(),
                MIGRATION_LADDER
                    .iter()
                    .map(|(version, _, _)| *version)
                    .collect::<Vec<_>>()
            );
        }
        let ledger = RunLedger::open(&database.path).unwrap();
        assert_eq!(
            ledger.applied_migrations().unwrap(),
            MIGRATION_LADDER
                .iter()
                .map(|(version, _, _)| *version)
                .collect::<Vec<_>>(),
            "reopening must not re-apply or add a migration"
        );

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
        assert_eq!(ledger.load_events("run-archive", 0, 10).unwrap().len(), 2);
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

    fn permission_request(
        request_id: &str,
        attribution: PermissionAttribution,
    ) -> PermissionRequestRecord {
        PermissionRequestRecord {
            request_id: request_id.to_string(),
            run_id: match attribution {
                PermissionAttribution::LedgerRun | PermissionAttribution::UnregisteredRun => {
                    Some("run-1".to_string())
                }
                _ => None,
            },
            attribution,
            process_id: None,
            tool_name: "delete_model".to_string(),
            tool_call_id: "tool-77".to_string(),
            tool_call_origin: ToolCallOrigin::Caller,
            operation_sha256: "a".repeat(64),
            mode: "manual".to_string(),
            risk_level: Some(RiskLevel::High),
            risk_floored: true,
            requested_at_ms: 1_000,
            expires_at_ms: 2_000,
        }
    }

    /// The upgrade path, not just the fresh-install one. A database written by a
    /// build that predates V11 must gain the table on open, and gain it *only*
    /// once — which is the branch `migration_is_safe_to_rerun_…` cannot reach,
    /// since that test never sees a database missing a version.
    #[test]
    fn a_database_written_before_v11_gains_the_later_tables_on_open() {
        let database = TempDb::new("permission-upgrade");
        {
            let ledger = RunLedger::open(&database.path).unwrap();
            ledger
                .record_permission_request(&permission_request(
                    "req-old",
                    PermissionAttribution::Unknown,
                ))
                .unwrap();
        }

        // Wind the database back to what V10 left behind. Dropping the table is
        // the only way to get there from here — the build that wrote it is gone.
        {
            let connection = Connection::open(&database.path).unwrap();
            connection
                .execute_batch(
                    // `egress_destinations_dropped`, V16's two token columns and
                    // V17's budget column are deliberately left in place: SQLite
                    // cannot drop a column carrying a `CHECK`, so this is the
                    // half-wound-back state V14's, V16's and V17's probes exist
                    // for.
                    "DROP TABLE permission_decisions;
                     DROP TABLE subsystem_events;
                     DROP TABLE egress_destinations;
                     DELETE FROM schema_migrations WHERE version IN (11, 12, 13, 14, 15, 16, 17);
                     PRAGMA user_version = 10;",
                )
                .unwrap();
        }

        let ledger = RunLedger::open(&database.path).unwrap();
        assert_eq!(
            ledger.applied_migrations().unwrap(),
            MIGRATION_LADDER
                .iter()
                .map(|(version, _, _)| *version)
                .collect::<Vec<_>>(),
            "opening a V10 database must apply every migration above it"
        );
        assert_eq!(
            ledger
                .connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        // The table is back and empty: the upgrade adds the surface, it does not
        // invent rows for permissions nobody recorded at the time.
        assert!(ledger
            .load_permission_decision("req-old")
            .unwrap()
            .is_none());
        ledger
            .record_permission_request(&permission_request(
                "req-new",
                PermissionAttribution::Unknown,
            ))
            .unwrap();
        assert!(ledger
            .load_permission_decision("req-new")
            .unwrap()
            .is_some());
    }

    /// A database a previous release wrote gains V21 and V22 without losing a row.
    ///
    /// V21 is the only process migration that rebuilds the table rather than
    /// adding to it — it widens the `kind` vocabulary for `foreground_shell` and
    /// adds the five typed breach columns with an all-or-none `CHECK` — so it is
    /// the one where an upgrade can silently drop a shipped user's history. The
    /// probe is the same shape as the pre-V11 one above: wind the schema back to
    /// what V20 left, reopen, and assert the ladder replays over real rows.
    ///
    /// What it pins beyond "the rows are still there": a row written before the
    /// breach columns existed reads back as a row with *no* breach rather than one
    /// with zeros, which is the difference between "this process was not stopped
    /// by a limit" and "it was stopped by a limit configured at 0 bytes".
    #[test]
    fn a_v20_database_gains_the_breach_columns_and_keeps_its_rows() {
        let database = TempDb::new("process-v21-upgrade");
        // The V20 column set, which is also what V21's `INSERT ... SELECT` names.
        const V20_COLUMNS: &str = "process_id, parent_process_id, kind, external_id, state, \
             run_id, workspace, profile, native_pid, max_wall_ms, max_memory_bytes, \
             max_output_bytes, max_child_processes, exit_status, exit_code, exit_signal, \
             exit_reason, created_at_ms, updated_at_ms, started_at_ms, exited_at_ms, \
             stop_requested, suspend_requested, signal_reason, signal_requested_at_ms, \
             kill_requested, cpu_time_ms, peak_rss_bytes, bytes_read, bytes_written, \
             bytes_egressed, tokens_in, tokens_out, gpu_resident_bytes, gpu_device_ms, \
             usage_unavailable_json, egress_destinations_dropped, context_tokens_reused, \
             max_context_tokens, context_tokens_evaluated";

        {
            let ledger = RunLedger::open(&database.path).unwrap();
            ledger
                .connection
                .execute(
                    "INSERT INTO agent_processes (
                         process_id, kind, external_id, state, created_at_ms, updated_at_ms,
                         started_at_ms, exited_at_ms, native_pid, max_memory_bytes,
                         exit_status, exit_code, exit_reason
                     ) VALUES (
                         'shell-v20', 'background_shell', 'ext-v20', 'exited', 10, 40,
                         12, 40, 4242, 536870912,
                         'failed', 1, 'the build failed'
                     )",
                    [],
                )
                .unwrap();
        }

        // Back to V20: a table with the old column set and none of the new
        // constraints, which is all V21's rebuild reads from. Dropping is the only
        // way there — the release that wrote the original is gone.
        {
            let connection = Connection::open(&database.path).unwrap();
            connection
                .execute_batch(&format!(
                    "CREATE TABLE agent_processes_v20 AS SELECT {V20_COLUMNS} FROM agent_processes;
                     DROP TABLE agent_processes;
                     ALTER TABLE agent_processes_v20 RENAME TO agent_processes;
                     DELETE FROM schema_migrations WHERE version IN (21, 22);
                     PRAGMA user_version = 20;"
                ))
                .unwrap();
        }

        let ledger = RunLedger::open(&database.path).unwrap();
        assert_eq!(
            ledger.applied_migrations().unwrap(),
            MIGRATION_LADDER
                .iter()
                .map(|(version, _, _)| *version)
                .collect::<Vec<_>>(),
            "opening a V20 database must apply every migration above it"
        );

        // The row survived, value for value, and its breach columns are absent
        // rather than zeroed.
        let (kind, pid, memory, status, reason, limit_kind, configured, start_time): (
            String,
            i64,
            i64,
            String,
            String,
            Option<String>,
            Option<i64>,
            Option<i64>,
        ) = ledger
            .connection
            .query_row(
                "SELECT kind, native_pid, max_memory_bytes, exit_status, exit_reason,
                        limit_kind, limit_configured, native_start_time
                 FROM agent_processes WHERE process_id = 'shell-v20'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(kind, "background_shell");
        assert_eq!(pid, 4242);
        assert_eq!(memory, 536_870_912);
        assert_eq!(status, "failed");
        assert_eq!(reason, "the build failed");
        assert_eq!(limit_kind, None, "a legacy row states no breach");
        assert_eq!(configured, None, "and no number for one");
        assert_eq!(start_time, None, "V22's identity is unknown for it");

        // The vocabulary V21 exists for is accepted…
        ledger
            .connection
            .execute(
                "INSERT INTO agent_processes
                     (process_id, kind, external_id, state, created_at_ms, updated_at_ms)
                 VALUES ('fg-1', 'foreground_shell', 'ext-fg-1', 'running', 50, 50)",
                [],
            )
            .unwrap();
        // …and the all-or-none breach guard is a constraint, not merely absent.
        assert!(
            ledger
                .connection
                .execute(
                    "UPDATE agent_processes SET limit_kind = 'max_memory_bytes'
                     WHERE process_id = 'shell-v20'",
                    [],
                )
                .is_err(),
            "half a breach must be refused by the rebuilt table's CHECK"
        );
    }

    /// The bug K12's acceptance names: before this table, a gated tool call
    /// outside a ledger-registered run — deleting a model from Settings, a local
    /// app definition run over HTTP, a triage reply posted to Slack — produced no
    /// permission event and no approval row anywhere at all.
    #[test]
    fn a_permission_with_no_run_is_still_recorded_and_readable() {
        let database = TempDb::new("permission-no-run");
        let ledger = RunLedger::open(&database.path).unwrap();

        let request = permission_request(
            "req-1",
            PermissionAttribution::Unattributed(crate::run_scope::Unattributed::UserAction),
        );
        ledger.record_permission_request(&request).unwrap();
        ledger
            .record_permission_decision(
                "req-1",
                PermissionDecision::AllowOnce,
                "user:desktop-prompt",
                1_500,
            )
            .unwrap();

        let stored = ledger
            .permission_decisions_for_tool_call("tool-77")
            .unwrap();
        assert_eq!(
            stored.len(),
            1,
            "the authorizing decision must be findable from the tool call"
        );
        assert_eq!(stored[0].request, request);
        assert_eq!(stored[0].decision, Some(PermissionDecision::AllowOnce));
        assert_eq!(stored[0].decided_by.as_deref(), Some("user:desktop-prompt"));
        assert_eq!(stored[0].decided_at_ms, Some(1_500));
        assert_eq!(
            stored[0].request.attribution.code(),
            "unattributed.user-action",
            "the reason it has no run is recorded, not left blank"
        );
    }

    /// Every attribution code must survive the round trip. A code the CHECK
    /// accepts but `parse` rejects would make a row unreadable after it is
    /// written, which is the worst time to find out.
    #[test]
    fn every_attribution_round_trips_through_the_database() {
        let database = TempDb::new("permission-attribution");
        let ledger = RunLedger::open(&database.path).unwrap();

        let mut attributions = vec![
            PermissionAttribution::LedgerRun,
            PermissionAttribution::UnregisteredRun,
            PermissionAttribution::Unknown,
        ];
        attributions.extend(
            crate::run_scope::Unattributed::ALL
                .iter()
                .map(|reason| PermissionAttribution::Unattributed(*reason)),
        );

        for (index, attribution) in attributions.iter().enumerate() {
            let request_id = format!("req-{index}");
            ledger
                .record_permission_request(&permission_request(&request_id, *attribution))
                .unwrap();
            let stored = ledger
                .load_permission_decision(&request_id)
                .unwrap()
                .expect("just written");
            assert_eq!(stored.request.attribution, *attribution);
            assert_eq!(stored.decision, None, "an open request has no decision yet");
        }
    }

    /// A generated tool call id must be distinguishable from a real one.
    ///
    /// This is the bug the column exists for. `tool_call_id` is `NOT NULL`, so a
    /// gated operation that is not a tool call — deleting a model from Settings
    /// — still gets an id, shaped exactly like a real one and joining to
    /// nothing. Without the origin beside it, a trail query returns nothing and
    /// the reader cannot tell "no decision was recorded" (the bug K12 names)
    /// from "this was never a tool call" (not a bug at all).
    #[test]
    fn a_synthesized_tool_call_id_says_so_and_a_real_one_stays_silent() {
        let database = TempDb::new("tool-call-origin");
        let ledger = RunLedger::open(&database.path).unwrap();

        for (request_id, origin) in [
            ("req-real", ToolCallOrigin::Caller),
            ("req-generated", ToolCallOrigin::Synthesized),
        ] {
            let mut record = permission_request(request_id, PermissionAttribution::Unknown);
            record.tool_call_id = format!("tool-{request_id}");
            record.tool_call_origin = origin;
            ledger.record_permission_request(&record).unwrap();

            let stored = ledger
                .load_permission_decision(request_id)
                .unwrap()
                .expect("just written");
            assert_eq!(stored.request.tool_call_origin, origin);
        }

        // A row written before the column existed keeps the third state rather
        // than being asserted into one of the other two.
        ledger
            .connection
            .execute(
                "INSERT INTO permission_decisions
                    (request_id, run_id, attribution, tool_name, tool_call_id,
                     operation_sha256, mode, risk_floored, requested_at_ms, expires_at_ms)
                 VALUES ('req-legacy', NULL, 'unknown', 't', 'tool-legacy', ?1, 'manual', 0, 1, 2)",
                params!["a".repeat(64)],
            )
            .unwrap();
        assert_eq!(
            ledger
                .load_permission_decision("req-legacy")
                .unwrap()
                .expect("the legacy row reads")
                .request
                .tool_call_origin,
            ToolCallOrigin::Unknown,
            "a row that predates the column must not claim its id was the caller's"
        );

        // The CHECK is the enforcement, not the enum: a writer that bypasses
        // `ToolCallOrigin::code` cannot invent a fourth state.
        assert!(ledger
            .connection
            .execute(
                "UPDATE permission_decisions SET tool_call_origin = 'probably-real'
                  WHERE request_id = 'req-legacy'",
                [],
            )
            .is_err());
    }

    /// A decision is final. Answering twice — a replayed IPC message, a second
    /// window — must fail loudly rather than overwrite what was decided.
    #[test]
    fn a_permission_decision_cannot_be_changed_once_made() {
        let database = TempDb::new("permission-final");
        let ledger = RunLedger::open(&database.path).unwrap();
        ledger
            .record_permission_request(&permission_request("req-1", PermissionAttribution::Unknown))
            .unwrap();
        ledger
            .record_permission_decision(
                "req-1",
                PermissionDecision::Deny,
                "user:desktop-prompt",
                1_500,
            )
            .unwrap();

        let error = ledger
            .record_permission_decision(
                "req-1",
                PermissionDecision::AllowOnce,
                "user:desktop-prompt",
                1_600,
            )
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("a permission decision is final"),
            "expected the decide-once trigger, got {error}"
        );

        // And the request half cannot be rewritten either, so an approval cannot
        // be relabelled onto a different, more dangerous operation after the fact.
        let error = ledger
            .connection
            .execute(
                "UPDATE permission_decisions SET tool_name = 'run_shell' WHERE request_id = 'req-1'",
                [],
            )
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("a permission decision is final"),
            "expected the decide-once trigger, got {error}"
        );

        let error = ledger
            .connection
            .execute("DELETE FROM permission_decisions", [])
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("permission decisions are append-only"),
            "expected the delete trigger, got {error}"
        );
    }

    /// The `attribution` code and the presence of a run id are two spellings of
    /// one fact, so they are checked in Rust *and* by the table, and neither is
    /// allowed to be the only guard.
    #[test]
    fn an_attribution_that_disagrees_with_its_run_id_is_refused() {
        let database = TempDb::new("permission-disagree");
        let ledger = RunLedger::open(&database.path).unwrap();

        let mut record = permission_request("req-1", PermissionAttribution::Unknown);
        record.run_id = Some("run-1".to_string());
        let error = ledger
            .record_permission_request(&record)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("disagree about whether this permission has a run"),
            "expected the Rust guard, got {error}"
        );

        let error = ledger
            .connection
            .execute(
                "INSERT INTO permission_decisions (
                    request_id, run_id, attribution, tool_name, tool_call_id,
                    operation_sha256, mode, risk_floored, requested_at_ms, expires_at_ms
                 ) VALUES ('req-2', 'run-1', 'unknown', 't', 'tc', ?1, 'manual', 0, 1, 2)",
                params!["a".repeat(64)],
            )
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("CHECK constraint failed"),
            "expected the table's own CHECK, got {error}"
        );
    }

    /// Deciding a request that was never recorded is a caller bug, not a silent
    /// no-op — a silent one would leave the decision nowhere at all.
    #[test]
    fn deciding_an_unrecorded_permission_is_an_error() {
        let database = TempDb::new("permission-missing");
        let ledger = RunLedger::open(&database.path).unwrap();
        assert!(matches!(
            ledger.record_permission_decision(
                "req-nope",
                PermissionDecision::AllowOnce,
                "user:desktop-prompt",
                1_500
            ),
            Err(LedgerError::NotFound {
                entity: "permission request",
                ..
            })
        ));
    }

    fn subsystem_event(action: &str, outcome: SubsystemOutcome) -> SubsystemEvent {
        SubsystemEvent {
            event_id: format!("subsystem-{action}"),
            subsystem: Subsystem::Mcp,
            action: action.to_string(),
            occurred_at_ms: 1_000,
            run_id: None,
            attribution: PermissionAttribution::Unattributed(
                crate::run_scope::Unattributed::SharedTransport,
            ),
            process_id: None,
            permission_request_id: Some("req-1".to_string()),
            outcome,
            detail_json: None,
        }
    }

    /// The acceptance's "one event stream every subsystem writes to". `run_events`
    /// cannot hold these — its `run_id` is a foreign key onto `runs`, and an MCP
    /// call on a shared transport has no run.
    #[test]
    fn a_subsystem_event_needs_no_run_and_names_what_authorized_it() {
        let database = TempDb::new("subsystem-basic");
        let mut ledger = RunLedger::open(&database.path).unwrap();

        let first = ledger
            .append_subsystem_event(&subsystem_event(
                "github:create_issue",
                SubsystemOutcome::Succeeded,
            ))
            .unwrap();
        let second = ledger
            .append_subsystem_event(&subsystem_event(
                "github:delete_repo",
                SubsystemOutcome::Denied,
            ))
            .unwrap();
        assert_eq!(
            (first, second),
            (1, 2),
            "one global stream, not per subsystem"
        );

        let events = ledger.recent_subsystem_events(None, 10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].sequence, 2, "newest first");
        assert_eq!(events[0].outcome, SubsystemOutcome::Denied);
        assert_eq!(events[0].permission_request_id.as_deref(), Some("req-1"));
        assert_eq!(
            events[0].attribution.code(),
            "unattributed.shared-transport",
            "why it has no run is recorded, not left blank"
        );

        // Filtering is by the persisted code, so a renamed variant cannot
        // silently stop matching rows written by an older build.
        assert_eq!(
            ledger
                .recent_subsystem_events(Some(Subsystem::Mcp), 10)
                .unwrap()
                .len(),
            2
        );
        assert!(ledger
            .recent_subsystem_events(Some(Subsystem::Http), 10)
            .unwrap()
            .is_empty());
    }

    /// The chain covers every column, so an edited row is detectable. Unlike V9's
    /// run chain there is no unchained era: this table was born chained.
    #[test]
    fn editing_a_subsystem_event_breaks_the_chain() {
        let database = TempDb::new("subsystem-chain");
        let mut ledger = RunLedger::open(&database.path).unwrap();
        for index in 0..3 {
            ledger
                .append_subsystem_event(&subsystem_event(
                    &format!("server:tool-{index}"),
                    SubsystemOutcome::Succeeded,
                ))
                .unwrap();
        }

        assert_eq!(
            ledger.verify_subsystem_chain().unwrap(),
            ChainVerification::Intact {
                covered_from: Some(1),
                covered_through: Some(3),
                events_seen: 3,
                events_naming_a_process: 0,
            }
        );

        // The append-only triggers are the first line of defence, so an edit has
        // to drop them first — which is precisely the attacker the chain exists
        // to catch, since dropping a trigger leaves the hashes untouched.
        ledger
            .connection
            .execute_batch(
                "DROP TRIGGER subsystem_events_forbid_update;
                 UPDATE subsystem_events SET outcome = 'succeeded' WHERE sequence = 2;
                 UPDATE subsystem_events SET action = 'server:something-else' WHERE sequence = 2;",
            )
            .unwrap();

        let verdict = ledger.verify_subsystem_chain().unwrap();
        assert!(
            matches!(verdict, ChainVerification::Broken { sequence: 2, .. }),
            "expected a break at the edited row, got {verdict:?}"
        );
    }

    #[test]
    fn subsystem_events_cannot_be_updated_or_deleted() {
        let database = TempDb::new("subsystem-append-only");
        let mut ledger = RunLedger::open(&database.path).unwrap();
        ledger
            .append_subsystem_event(&subsystem_event("server:tool", SubsystemOutcome::Succeeded))
            .unwrap();

        for statement in [
            "UPDATE subsystem_events SET action = 'other'",
            "DELETE FROM subsystem_events",
        ] {
            let error = ledger
                .connection
                .execute(statement, [])
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("subsystem events are append-only"),
                "expected the append-only trigger for `{statement}`, got {error}"
            );
        }
    }

    /// A row whose `prev_event_hash` does not name the current tail is refused by
    /// the database itself, so the linkage holds against a writer that never goes
    /// through Rust.
    #[test]
    fn a_subsystem_event_must_carry_its_predecessors_hash() {
        let database = TempDb::new("subsystem-linkage");
        let mut ledger = RunLedger::open(&database.path).unwrap();
        ledger
            .append_subsystem_event(&subsystem_event(
                "server:first",
                SubsystemOutcome::Succeeded,
            ))
            .unwrap();

        let error = ledger
            .connection
            .execute(
                "INSERT INTO subsystem_events (
                    event_id, subsystem, action, occurred_at_ms, attribution, outcome,
                    event_hash, prev_event_hash
                 ) VALUES ('forged', 'mcp', 'server:forged', 1, 'unknown', 'succeeded', ?1, ?2)",
                params!["a".repeat(64), "b".repeat(64)],
            )
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("must carry its predecessor's hash"),
            "expected the linkage trigger, got {error}"
        );
    }

    /// Every persisted code must survive the round trip. A code the CHECK accepts
    /// but `parse` rejects makes a row unreadable after it is written.
    #[test]
    fn every_subsystem_and_outcome_code_round_trips() {
        let database = TempDb::new("subsystem-codes");
        let mut ledger = RunLedger::open(&database.path).unwrap();

        for (index, subsystem) in Subsystem::ALL.iter().enumerate() {
            for (offset, outcome) in SubsystemOutcome::ALL.iter().enumerate() {
                let mut event = subsystem_event(&format!("action-{index}-{offset}"), *outcome);
                event.event_id = format!("subsystem-{index}-{offset}");
                event.subsystem = *subsystem;
                ledger.append_subsystem_event(&event).unwrap();
            }
        }

        let stored = ledger.recent_subsystem_events(None, 100).unwrap();
        assert_eq!(
            stored.len(),
            Subsystem::ALL.len() * SubsystemOutcome::ALL.len()
        );
        assert!(matches!(
            ledger.verify_subsystem_chain().unwrap(),
            ChainVerification::Intact { .. }
        ));
    }

    #[test]
    fn a_subsystem_event_whose_attribution_disagrees_with_its_run_id_is_refused() {
        let database = TempDb::new("subsystem-disagree");
        let mut ledger = RunLedger::open(&database.path).unwrap();
        let mut event = subsystem_event("server:tool", SubsystemOutcome::Succeeded);
        event.run_id = Some("run-1".to_string());
        let error = ledger
            .append_subsystem_event(&event)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("disagree about whether this event has a run"),
            "expected the Rust guard, got {error}"
        );
    }
}
