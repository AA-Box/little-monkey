//! Durable storage for the telephony subsystem: carrier accounts, the calls
//! placed or answered through them, and the carrier callback log. Owns the
//! `telecom_*` tables created by `DAEMON_V6_SQL` in `store.rs`.
//!
//! SMS has no tables here on purpose. An inbound text is already a
//! `ChannelEnvelope` and lives in `channel_events` like every other message —
//! see `daemon::telephony`'s module doc. What this store adds is the one
//! thing messaging has no concept of: a *call*, which costs the operator
//! money at their carrier and so can never be silently retried.
//!
//! # Secrets
//!
//! No column here ever carries a plaintext credential.
//! `telecom_accounts.credential_ref` is a keychain account name, safe to find
//! in a copied database file — the actual carrier secret lives in the OS
//! keychain and is resolved by the caller before a provider is built.

use rusqlite::{params, OptionalExtension, TransactionBehavior};

use little_monkey_lib::channels::types::{ChannelHealth, HealthState};

use super::store::DaemonStore;
use super::telephony::{CallState, TelecomKind};

/// Who may call this account's number in. Separate from
/// [`OutboundCallApproval`] on purpose: answering the phone and dialing out
/// are separate grants, and an operator who allows one has not thereby
/// allowed the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundCallPolicy {
    Reject,
    Voicemail,
    Answer,
}

impl InboundCallPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            InboundCallPolicy::Reject => "reject",
            InboundCallPolicy::Voicemail => "voicemail",
            InboundCallPolicy::Answer => "answer",
        }
    }

    pub fn parse(value: &str) -> Option<InboundCallPolicy> {
        match value {
            "reject" => Some(InboundCallPolicy::Reject),
            "voicemail" => Some(InboundCallPolicy::Voicemail),
            "answer" => Some(InboundCallPolicy::Answer),
            _ => None,
        }
    }
}

/// Whether this account may place outbound calls, and under what gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundCallApproval {
    Never,
    Approval,
    Allow,
}

impl OutboundCallApproval {
    pub fn as_str(self) -> &'static str {
        match self {
            OutboundCallApproval::Never => "never",
            OutboundCallApproval::Approval => "approval",
            OutboundCallApproval::Allow => "allow",
        }
    }

    pub fn parse(value: &str) -> Option<OutboundCallApproval> {
        match value {
            "never" => Some(OutboundCallApproval::Never),
            "approval" => Some(OutboundCallApproval::Approval),
            "allow" => Some(OutboundCallApproval::Allow),
            _ => None,
        }
    }
}

/// What one account is allowed to spend, in calls rather than currency.
///
/// Every field bounds something with a cost attached — a concurrent call, a
/// ring nobody answers, a call left open, a recording of somebody who did not
/// ask to be recorded — so [`CallLimits::default`] is the cautious answer and
/// an operator has to choose anything looser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallLimits {
    pub max_concurrent_calls: u32,
    pub ring_timeout_s: u32,
    pub max_duration_s: u32,
    pub recording_enabled: bool,
}

impl Default for CallLimits {
    fn default() -> Self {
        Self {
            max_concurrent_calls: 1,
            ring_timeout_s: 60,
            max_duration_s: 1_800,
            recording_enabled: false,
        }
    }
}

/// Which conversation a call continues.
///
/// A person who calls back usually means to carry on the last conversation, so
/// the default keys the session to their number. `per_call` is for a line where
/// each call is a different matter — a booking line, a reception desk — and
/// carrying context across callers would be wrong.
pub fn call_session_key(
    account: &TelecomAccountRecord,
    peer_number: &str,
    call_id: &str,
) -> String {
    let per_call = account
        .non_secret_config
        .get("session_scope")
        .and_then(|value| value.as_str())
        == Some("per_call");
    if per_call {
        format!("call:{}:{call_id}", account.account_id)
    } else {
        format!("call:{}:{peer_number}", account.account_id)
    }
}

impl CallLimits {
    /// Clamp to what the schema and a sane operator can mean. A zero here would
    /// otherwise read as "no limit" and mean the opposite of what the column is
    /// for.
    pub fn sanitized(self) -> Self {
        Self {
            max_concurrent_calls: self.max_concurrent_calls.clamp(1, 100),
            ring_timeout_s: self.ring_timeout_s.clamp(5, 600),
            max_duration_s: self.max_duration_s.clamp(30, 14_400),
            recording_enabled: self.recording_enabled,
        }
    }
}

/// A stored carrier account.
#[derive(Debug, Clone, PartialEq)]
pub struct TelecomAccountRecord {
    pub account_id: String,
    pub kind: TelecomKind,
    pub label: String,
    pub enabled: bool,
    /// The account identifier the carrier issues. Not a secret.
    pub carrier_account_id: String,
    pub from_number: String,
    pub credential_ref: Option<String>,
    pub public_base_url: Option<String>,
    pub non_secret_config: serde_json::Value,
    pub inbound_policy: InboundCallPolicy,
    pub outbound_approval: OutboundCallApproval,
    pub limits: CallLimits,
    pub health: ChannelHealth,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Which way a call was initiated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallDirection {
    Inbound,
    Outbound,
}

impl CallDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            CallDirection::Inbound => "inbound",
            CallDirection::Outbound => "outbound",
        }
    }

    pub fn parse(value: &str) -> Option<CallDirection> {
        match value {
            "inbound" => Some(CallDirection::Inbound),
            "outbound" => Some(CallDirection::Outbound),
            _ => None,
        }
    }
}

/// A stored call, in or out.
///
/// Every field is read by callers outside this file (the calls UI, the
/// approval flow, and reconciliation tooling) once they land; nothing in
/// this store consumes them itself.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub struct TelecomCallRecord {
    pub call_id: String,
    pub account_id: String,
    pub provider_call_id: Option<String>,
    pub direction: CallDirection,
    pub peer_number: String,
    pub state: CallState,
    pub session_key: Option<String>,
    pub job_id: Option<String>,
    pub idempotency_key: String,
    pub last_error: Option<String>,
    /// Spoken when the call connects. See `DAEMON_V8_SQL`.
    pub opening_line: Option<String>,
    pub started_at_ms: Option<i64>,
    pub ended_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Outcome of [`DaemonStore::start_call`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallRecording {
    Recorded { call_id: String },
    Duplicate { call_id: String },
}

/// Which limit a live call outlived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitBreach {
    /// Nobody picked up inside the account's ring timeout.
    RingTimeout,
    /// The call has been connected longer than the account allows.
    MaxDuration,
}

impl LimitBreach {
    pub fn detail(self) -> &'static str {
        match self {
            LimitBreach::RingTimeout => "No answer inside this account's ring timeout",
            LimitBreach::MaxDuration => "Ended at this account's maximum call duration",
        }
    }
}

/// A live call that has outlived one of its account's limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverdueCall {
    pub call_id: String,
    pub account_id: String,
    pub provider_call_id: Option<String>,
    pub breach: LimitBreach,
}

/// Outcome of [`DaemonStore::record_telecom_event`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelecomEventRecording {
    Recorded { event_id: String },
    Duplicate { event_id: String },
}

fn new_event_id() -> String {
    format!("tev-{}", uuid::Uuid::new_v4().simple())
}

impl DaemonStore {
    // -- Accounts ---------------------------------------------------------

    pub fn upsert_telecom_account(&mut self, account: &TelecomAccountRecord) -> Result<(), String> {
        let non_secret_config =
            serde_json::to_string(&account.non_secret_config).map_err(|error| error.to_string())?;
        // Sanitized on the way in, so a limit read back is always one the rest
        // of the subsystem can act on.
        let limits = account.limits.sanitized();
        self.connection
            .execute(
                "INSERT INTO telecom_accounts (
                    account_id, kind, label, enabled, carrier_account_id, from_number,
                    credential_ref, public_base_url, non_secret_config_json, inbound_policy,
                    outbound_approval, max_concurrent_calls, ring_timeout_s, max_duration_s,
                    recording_enabled, health, health_detail, last_error, last_probe_at_ms,
                    created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                           ?17, ?18, ?19, ?20, ?21)
                 ON CONFLICT(account_id) DO UPDATE SET
                    kind = excluded.kind,
                    label = excluded.label,
                    enabled = excluded.enabled,
                    carrier_account_id = excluded.carrier_account_id,
                    from_number = excluded.from_number,
                    credential_ref = excluded.credential_ref,
                    public_base_url = excluded.public_base_url,
                    non_secret_config_json = excluded.non_secret_config_json,
                    inbound_policy = excluded.inbound_policy,
                    outbound_approval = excluded.outbound_approval,
                    max_concurrent_calls = excluded.max_concurrent_calls,
                    ring_timeout_s = excluded.ring_timeout_s,
                    max_duration_s = excluded.max_duration_s,
                    recording_enabled = excluded.recording_enabled,
                    health = excluded.health,
                    health_detail = excluded.health_detail,
                    last_error = excluded.last_error,
                    last_probe_at_ms = excluded.last_probe_at_ms,
                    updated_at_ms = excluded.updated_at_ms",
                params![
                    account.account_id,
                    account.kind.as_str(),
                    account.label,
                    account.enabled,
                    account.carrier_account_id,
                    account.from_number,
                    account.credential_ref,
                    account.public_base_url,
                    non_secret_config,
                    account.inbound_policy.as_str(),
                    account.outbound_approval.as_str(),
                    limits.max_concurrent_calls,
                    limits.ring_timeout_s,
                    limits.max_duration_s,
                    limits.recording_enabled,
                    account.health.state.as_str(),
                    account.health.detail,
                    account.health.last_error,
                    account.health.probed_at_ms,
                    account.created_at_ms,
                    account.updated_at_ms,
                ],
            )
            .map_err(|error| format!("Failed to upsert telecom account: {error}"))?;
        Ok(())
    }

    pub fn telecom_account(
        &self,
        account_id: &str,
    ) -> Result<Option<TelecomAccountRecord>, String> {
        self.connection
            .query_row(
                "SELECT account_id, kind, label, enabled, carrier_account_id, from_number,
                        credential_ref, public_base_url, non_secret_config_json, inbound_policy,
                        outbound_approval, health, health_detail, last_error, last_probe_at_ms,
                        created_at_ms, updated_at_ms, max_concurrent_calls, ring_timeout_s,
                        max_duration_s, recording_enabled
                 FROM telecom_accounts WHERE account_id=?1",
                [account_id],
                read_telecom_account,
            )
            .optional()
            .map_err(|error| error.to_string())?
            .transpose()
    }

    pub fn telecom_accounts(&self) -> Result<Vec<TelecomAccountRecord>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT account_id, kind, label, enabled, carrier_account_id, from_number,
                        credential_ref, public_base_url, non_secret_config_json, inbound_policy,
                        outbound_approval, health, health_detail, last_error, last_probe_at_ms,
                        created_at_ms, updated_at_ms, max_concurrent_calls, ring_timeout_s,
                        max_duration_s, recording_enabled
                 FROM telecom_accounts ORDER BY created_at_ms ASC",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], read_telecom_account)
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
            .into_iter()
            .collect()
    }

    pub fn set_telecom_account_health(
        &mut self,
        account_id: &str,
        health: &ChannelHealth,
        now_ms: i64,
    ) -> Result<(), String> {
        let changed = self
            .connection
            .execute(
                "UPDATE telecom_accounts
                 SET health=?2, health_detail=?3, last_error=?4, last_probe_at_ms=?5, updated_at_ms=?6
                 WHERE account_id=?1",
                params![
                    account_id,
                    health.state.as_str(),
                    health.detail,
                    health.last_error,
                    health.probed_at_ms,
                    now_ms,
                ],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err(format!("Unknown telecom account '{account_id}'"));
        }
        Ok(())
    }

    pub fn delete_telecom_account(&mut self, account_id: &str) -> Result<bool, String> {
        self.connection
            .execute(
                "DELETE FROM telecom_accounts WHERE account_id=?1",
                [account_id],
            )
            .map(|changed| changed == 1)
            .map_err(|error| error.to_string())
    }

    // -- Calls ---------------------------------------------------------------

    /// Insert a new call row, deduped on `(account_id, idempotency_key)`. This
    /// is what stops a retried run from dialing the same number twice: the
    /// caller derives the idempotency key from whatever triggered the call
    /// (a job id, a tool call id), and a re-delivery of that same trigger
    /// collapses onto the row already inserted.
    pub fn start_call(&mut self, record: &TelecomCallRecord) -> Result<CallRecording, String> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let changed = transaction
            .execute(
                "INSERT INTO telecom_calls (
                    call_id, account_id, provider_call_id, direction, peer_number, state,
                    session_key, job_id, idempotency_key, last_error, started_at_ms, ended_at_ms,
                    created_at_ms, updated_at_ms, opening_line
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                 ON CONFLICT(account_id, idempotency_key) DO NOTHING",
                params![
                    record.call_id,
                    record.account_id,
                    record.provider_call_id,
                    record.direction.as_str(),
                    record.peer_number,
                    record.state.as_str(),
                    record.session_key,
                    record.job_id,
                    record.idempotency_key,
                    record.last_error,
                    record.started_at_ms,
                    record.ended_at_ms,
                    record.created_at_ms,
                    record.updated_at_ms,
                    record.opening_line,
                ],
            )
            .map_err(|error| format!("Failed to start call: {error}"))?;
        let result = if changed == 1 {
            CallRecording::Recorded {
                call_id: record.call_id.clone(),
            }
        } else {
            let existing_id: String = transaction
                .query_row(
                    "SELECT call_id FROM telecom_calls WHERE account_id=?1 AND idempotency_key=?2",
                    params![record.account_id, record.idempotency_key],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            CallRecording::Duplicate {
                call_id: existing_id,
            }
        };
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(result)
    }

    pub fn telecom_call(&self, call_id: &str) -> Result<Option<TelecomCallRecord>, String> {
        self.connection
            .query_row(
                "SELECT call_id, account_id, provider_call_id, direction, peer_number, state,
                        session_key, job_id, idempotency_key, last_error, started_at_ms,
                        ended_at_ms, created_at_ms, updated_at_ms, opening_line
                 FROM telecom_calls WHERE call_id=?1",
                [call_id],
                read_telecom_call,
            )
            .optional()
            .map_err(|error| error.to_string())?
            .transpose()
    }

    /// How a carrier callback finds the row it is about: carriers speak in
    /// their own call id, never ours.
    pub fn call_by_provider_id(
        &self,
        account_id: &str,
        provider_call_id: &str,
    ) -> Result<Option<TelecomCallRecord>, String> {
        self.connection
            .query_row(
                "SELECT call_id, account_id, provider_call_id, direction, peer_number, state,
                        session_key, job_id, idempotency_key, last_error, started_at_ms,
                        ended_at_ms, created_at_ms, updated_at_ms, opening_line
                 FROM telecom_calls WHERE account_id=?1 AND provider_call_id=?2",
                params![account_id, provider_call_id],
                read_telecom_call,
            )
            .optional()
            .map_err(|error| error.to_string())?
            .transpose()
    }

    pub fn set_call_provider_id(
        &mut self,
        call_id: &str,
        provider_call_id: &str,
        now_ms: i64,
    ) -> Result<(), String> {
        let changed = self
            .connection
            .execute(
                "UPDATE telecom_calls SET provider_call_id=?2, updated_at_ms=?3 WHERE call_id=?1",
                params![call_id, provider_call_id, now_ms],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err(format!("Unknown call '{call_id}'"));
        }
        Ok(())
    }

    /// Move a call to a new state. Sets `started_at_ms` the first time the
    /// call reaches `in_progress` and `ended_at_ms` the moment it reaches any
    /// terminal state.
    ///
    /// Refuses to move a call OUT of a terminal state: a late or replayed
    /// carrier callback must never resurrect a call this store already
    /// considers finished, because the finished state is what the approval
    /// and billing story downstream is built on.
    pub fn advance_call(
        &mut self,
        call_id: &str,
        state: CallState,
        detail: Option<&str>,
        now_ms: i64,
    ) -> Result<(), String> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let current: Option<(String, Option<i64>)> = transaction
            .query_row(
                "SELECT state, started_at_ms FROM telecom_calls WHERE call_id=?1",
                [call_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let Some((current_state_token, started_at_ms)) = current else {
            return Err(format!("Unknown call '{call_id}'"));
        };
        let Some(current_state) = CallState::parse(&current_state_token) else {
            return Err(format!(
                "call '{call_id}' has unknown state '{current_state_token}'"
            ));
        };
        if current_state.is_terminal() {
            // A crashed daemon or a duplicated webhook can replay a callback
            // after the call is already settled; changing nothing here is the
            // correct outcome, not an error.
            transaction.commit().map_err(|error| error.to_string())?;
            return Ok(());
        }
        let new_started_at_ms = if started_at_ms.is_none() && state == CallState::InProgress {
            Some(now_ms)
        } else {
            started_at_ms
        };
        let ended_at_ms = if state.is_terminal() {
            Some(now_ms)
        } else {
            None
        };
        transaction
            .execute(
                "UPDATE telecom_calls
                 SET state=?2, last_error=?3, started_at_ms=?4, ended_at_ms=?5, updated_at_ms=?6
                 WHERE call_id=?1",
                params![
                    call_id,
                    state.as_str(),
                    detail,
                    new_started_at_ms,
                    ended_at_ms,
                    now_ms,
                ],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn live_calls(&self, account_id: &str) -> Result<Vec<TelecomCallRecord>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT call_id, account_id, provider_call_id, direction, peer_number, state,
                        session_key, job_id, idempotency_key, last_error, started_at_ms,
                        ended_at_ms, created_at_ms, updated_at_ms, opening_line
                 FROM telecom_calls
                 WHERE account_id=?1 AND state NOT IN ('completed','failed','needs_reconciliation')
                 ORDER BY created_at_ms ASC",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([account_id], read_telecom_call)
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
            .into_iter()
            .collect()
    }

    pub fn recent_calls(
        &self,
        account_id: &str,
        limit: u32,
    ) -> Result<Vec<TelecomCallRecord>, String> {
        let limit = limit.clamp(1, 200);
        let mut statement = self
            .connection
            .prepare(
                "SELECT call_id, account_id, provider_call_id, direction, peer_number, state,
                        session_key, job_id, idempotency_key, last_error, started_at_ms,
                        ended_at_ms, created_at_ms, updated_at_ms, opening_line
                 FROM telecom_calls WHERE account_id=?1
                 ORDER BY created_at_ms DESC LIMIT ?2",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(params![account_id, i64::from(limit)], read_telecom_call)
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
            .into_iter()
            .collect()
    }

    /// Anything a crashed daemon left in `queued`, `ringing`, or
    /// `in_progress` moves to `needs_reconciliation` — never back to a
    /// retryable state. The call may already have connected at the carrier
    /// and be running up a bill; only an operator (or a reconciliation pass
    /// that can ask the carrier directly) may resolve that, so the automatic
    /// path stops here.
    pub fn reconcile_stuck_calls(&mut self, now_ms: i64) -> Result<u32, String> {
        self.connection
            .execute(
                "UPDATE telecom_calls
                 SET state='needs_reconciliation', updated_at_ms=?1
                 WHERE state IN ('queued','ringing','in_progress')",
                [now_ms],
            )
            .map(|changed| u32::try_from(changed).unwrap_or(u32::MAX))
            .map_err(|error| error.to_string())
    }

    /// Calls that have outlived a limit, with the limit they broke.
    ///
    /// Read rather than written here so the caller can hang the call up at the
    /// carrier before the row is closed: a row marked completed while the
    /// carrier still has the line open is a bill that keeps running.
    pub fn overdue_calls(&self, now_ms: i64) -> Result<Vec<OverdueCall>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT c.call_id, c.account_id, c.provider_call_id, c.state,
                        c.started_at_ms, c.created_at_ms, a.ring_timeout_s, a.max_duration_s
                 FROM telecom_calls c
                 JOIN telecom_accounts a ON a.account_id = c.account_id
                 WHERE c.state IN ('queued','ringing','in_progress')",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                let call_id: String = row.get(0)?;
                let account_id: String = row.get(1)?;
                let provider_call_id: Option<String> = row.get(2)?;
                let state_token: String = row.get(3)?;
                let started_at_ms: Option<i64> = row.get(4)?;
                let created_at_ms: i64 = row.get(5)?;
                let ring_timeout_s: i64 = row.get(6)?;
                let max_duration_s: i64 = row.get(7)?;
                Ok((
                    call_id,
                    account_id,
                    provider_call_id,
                    state_token,
                    started_at_ms,
                    created_at_ms,
                    ring_timeout_s,
                    max_duration_s,
                ))
            })
            .map_err(|error| error.to_string())?;
        let mut overdue = Vec::new();
        for row in rows {
            let (
                call_id,
                account_id,
                provider_call_id,
                state_token,
                started_at_ms,
                created_at_ms,
                ring_timeout_s,
                max_duration_s,
            ) = row.map_err(|error| error.to_string())?;
            let Some(state) = CallState::parse(&state_token) else {
                continue;
            };
            let breach = match state {
                CallState::InProgress => (now_ms - started_at_ms.unwrap_or(created_at_ms)
                    > max_duration_s.saturating_mul(1_000))
                .then_some(LimitBreach::MaxDuration),
                CallState::Queued | CallState::Ringing => (now_ms - created_at_ms
                    > ring_timeout_s.saturating_mul(1_000))
                .then_some(LimitBreach::RingTimeout),
                _ => None,
            };
            if let Some(breach) = breach {
                overdue.push(OverdueCall {
                    call_id,
                    account_id,
                    provider_call_id,
                    breach,
                });
            }
        }
        Ok(overdue)
    }

    /// How many calls this account has live right now. What the concurrency
    /// limit is compared against before a new one is allowed to start.
    pub fn live_call_count(&self, account_id: &str) -> Result<u32, String> {
        self.connection
            .query_row(
                "SELECT COUNT(*) FROM telecom_calls
                 WHERE account_id=?1 AND state IN ('queued','ringing','in_progress')",
                [account_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| u32::try_from(count).unwrap_or(u32::MAX))
            .map_err(|error| error.to_string())
    }

    // -- Events (carrier callback dedupe) -------------------------------------

    pub fn record_telecom_event(
        &mut self,
        account_id: &str,
        provider_event_id: &str,
        kind: &str,
        call_id: Option<&str>,
        payload_digest: &str,
        now_ms: i64,
    ) -> Result<TelecomEventRecording, String> {
        let event_id = new_event_id();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let changed = transaction
            .execute(
                "INSERT INTO telecom_events (
                    event_id, account_id, provider_event_id, kind, call_id, payload_digest,
                    received_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(account_id, provider_event_id) DO NOTHING",
                params![
                    event_id,
                    account_id,
                    provider_event_id,
                    kind,
                    call_id,
                    payload_digest,
                    now_ms,
                ],
            )
            .map_err(|error| format!("Failed to record telecom event: {error}"))?;
        let result = if changed == 1 {
            TelecomEventRecording::Recorded { event_id }
        } else {
            let existing_id: String = transaction
                .query_row(
                    "SELECT event_id FROM telecom_events WHERE account_id=?1 AND provider_event_id=?2",
                    params![account_id, provider_event_id],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            TelecomEventRecording::Duplicate {
                event_id: existing_id,
            }
        };
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(result)
    }
}

fn read_telecom_account(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<TelecomAccountRecord, String>> {
    let account_id: String = row.get(0)?;
    let kind_token: String = row.get(1)?;
    let Some(kind) = TelecomKind::parse(&kind_token) else {
        return Ok(Err(format!(
            "telecom account '{account_id}' has unknown kind '{kind_token}'"
        )));
    };
    let non_secret_config_json: String = row.get(8)?;
    let inbound_policy_token: String = row.get(9)?;
    let Some(inbound_policy) = InboundCallPolicy::parse(&inbound_policy_token) else {
        return Ok(Err(format!(
            "telecom account '{account_id}' has unknown inbound policy '{inbound_policy_token}'"
        )));
    };
    let outbound_approval_token: String = row.get(10)?;
    let Some(outbound_approval) = OutboundCallApproval::parse(&outbound_approval_token) else {
        return Ok(Err(format!(
            "telecom account '{account_id}' has unknown outbound approval '{outbound_approval_token}'"
        )));
    };
    let health_token: String = row.get(11)?;
    let Some(health_state) = HealthState::parse(&health_token) else {
        return Ok(Err(format!(
            "telecom account '{account_id}' has unknown health state '{health_token}'"
        )));
    };

    let label: String = row.get(2)?;
    let enabled: i64 = row.get(3)?;
    let carrier_account_id: String = row.get(4)?;
    let from_number: String = row.get(5)?;
    let credential_ref: Option<String> = row.get(6)?;
    let public_base_url: Option<String> = row.get(7)?;
    let detail: Option<String> = row.get(12)?;
    let last_error: Option<String> = row.get(13)?;
    let probed_at_ms: i64 = row.get(14)?;
    let created_at_ms: i64 = row.get(15)?;
    let updated_at_ms: i64 = row.get(16)?;
    let max_concurrent_calls: i64 = row.get(17)?;
    let ring_timeout_s: i64 = row.get(18)?;
    let max_duration_s: i64 = row.get(19)?;
    let recording_enabled: i64 = row.get(20)?;

    let non_secret_config: serde_json::Value = match serde_json::from_str(&non_secret_config_json) {
        Ok(value) => value,
        Err(_) => {
            return Ok(Err(format!(
                "telecom account '{account_id}' has malformed config JSON"
            )))
        }
    };

    Ok(Ok(TelecomAccountRecord {
        account_id,
        kind,
        label,
        enabled: enabled != 0,
        carrier_account_id,
        from_number,
        credential_ref,
        public_base_url,
        non_secret_config,
        inbound_policy,
        outbound_approval,
        limits: CallLimits {
            max_concurrent_calls: u32::try_from(max_concurrent_calls).unwrap_or(1),
            ring_timeout_s: u32::try_from(ring_timeout_s).unwrap_or(60),
            max_duration_s: u32::try_from(max_duration_s).unwrap_or(1_800),
            recording_enabled: recording_enabled != 0,
        }
        .sanitized(),
        health: ChannelHealth {
            state: health_state,
            detail,
            last_error,
            probed_at_ms,
        },
        created_at_ms,
        updated_at_ms,
    }))
}

fn read_telecom_call(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<TelecomCallRecord, String>> {
    let call_id: String = row.get(0)?;
    let direction_token: String = row.get(3)?;
    let Some(direction) = CallDirection::parse(&direction_token) else {
        return Ok(Err(format!(
            "call '{call_id}' has unknown direction '{direction_token}'"
        )));
    };
    let state_token: String = row.get(5)?;
    let Some(state) = CallState::parse(&state_token) else {
        return Ok(Err(format!(
            "call '{call_id}' has unknown state '{state_token}'"
        )));
    };
    Ok(Ok(TelecomCallRecord {
        call_id,
        account_id: row.get(1)?,
        provider_call_id: row.get(2)?,
        direction,
        peer_number: row.get(4)?,
        state,
        session_key: row.get(6)?,
        job_id: row.get(7)?,
        idempotency_key: row.get(8)?,
        last_error: row.get(9)?,
        started_at_ms: row.get(10)?,
        ended_at_ms: row.get(11)?,
        created_at_ms: row.get(12)?,
        updated_at_ms: row.get(13)?,
        opening_line: row.get(14)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str) -> TelecomAccountRecord {
        TelecomAccountRecord {
            account_id: id.into(),
            kind: TelecomKind::Mock,
            label: "Test line".into(),
            enabled: true,
            carrier_account_id: "AC-test".into(),
            from_number: "+15005550006".into(),
            credential_ref: Some("keychain:telecom:test".into()),
            public_base_url: Some("https://example.test".into()),
            non_secret_config: serde_json::json!({}),
            inbound_policy: InboundCallPolicy::Voicemail,
            outbound_approval: OutboundCallApproval::Approval,
            limits: CallLimits::default(),
            health: ChannelHealth {
                state: HealthState::Disconnected,
                detail: None,
                last_error: None,
                probed_at_ms: 1_000,
            },
            created_at_ms: 1_000,
            updated_at_ms: 1_000,
        }
    }

    fn seeded() -> DaemonStore {
        let mut store = DaemonStore::open_in_memory().expect("open store");
        store
            .upsert_telecom_account(&account("acct-1"))
            .expect("seed account");
        store
    }

    fn call(
        call_id: &str,
        account_id: &str,
        idempotency_key: &str,
        state: CallState,
    ) -> TelecomCallRecord {
        TelecomCallRecord {
            call_id: call_id.into(),
            account_id: account_id.into(),
            provider_call_id: None,
            direction: CallDirection::Outbound,
            peer_number: "+15005550001".into(),
            state,
            session_key: None,
            job_id: None,
            idempotency_key: idempotency_key.into(),
            opening_line: None,
            last_error: None,
            started_at_ms: None,
            ended_at_ms: None,
            created_at_ms: 1_000,
            updated_at_ms: 1_000,
        }
    }

    #[test]
    fn account_upsert_is_idempotent_and_preserves_created_at() {
        let mut store = seeded();
        let mut updated = account("acct-1");
        updated.label = "Renamed line".into();
        updated.created_at_ms = 9_999; // must not stick
        updated.updated_at_ms = 2_000;
        store
            .upsert_telecom_account(&updated)
            .expect("upsert again");

        let stored = store
            .telecom_account("acct-1")
            .expect("query")
            .expect("present");
        assert_eq!(stored.label, "Renamed line");
        assert_eq!(stored.created_at_ms, 1_000);
        assert_eq!(stored.updated_at_ms, 2_000);
        assert_eq!(store.telecom_accounts().expect("list").len(), 1);
    }

    #[test]
    fn health_update_round_trips() {
        let mut store = seeded();
        let health = ChannelHealth {
            state: HealthState::Connected,
            detail: Some("ok".into()),
            last_error: None,
            probed_at_ms: 5_000,
        };
        store
            .set_telecom_account_health("acct-1", &health, 5_500)
            .expect("set health");
        let stored = store
            .telecom_account("acct-1")
            .expect("query")
            .expect("present");
        assert_eq!(stored.health, health);
        assert_eq!(stored.updated_at_ms, 5_500);
    }

    #[test]
    fn delete_cascades_to_calls_and_events() {
        let mut store = seeded();
        store
            .start_call(&call("call-1", "acct-1", "idem-1", CallState::Queued))
            .expect("start call");
        store
            .record_telecom_event(
                "acct-1",
                "prov-evt-1",
                "call.progress",
                None,
                "digest",
                1_000,
            )
            .expect("record event");

        assert!(store.delete_telecom_account("acct-1").expect("delete"));
        assert_eq!(store.telecom_call("call-1").expect("lookup"), None);
        // The event row is gone too — ON DELETE CASCADE on telecom_events' FK.
        // Re-inserting under a fresh account with the same provider_event_id
        // must succeed, which it could not if the old row still existed.
        store
            .upsert_telecom_account(&account("acct-1"))
            .expect("recreate account");
        let recorded = store
            .record_telecom_event(
                "acct-1",
                "prov-evt-1",
                "call.progress",
                None,
                "digest",
                2_000,
            )
            .expect("record again");
        assert!(matches!(recorded, TelecomEventRecording::Recorded { .. }));
    }

    #[test]
    fn a_caller_who_rings_back_continues_the_same_conversation() {
        let mut account = account("tel-1");
        let first = call_session_key(&account, "+15551234567", "call-1");
        let second = call_session_key(&account, "+15551234567", "call-2");
        assert_eq!(first, second, "the same person keeps their session");
        assert_ne!(
            first,
            call_session_key(&account, "+15559999999", "call-3"),
            "a different caller does not inherit it"
        );

        // A line where each call is a different matter says so, and then two
        // calls from one number are two conversations.
        account.non_secret_config = serde_json::json!({ "session_scope": "per_call" });
        assert_ne!(
            call_session_key(&account, "+15551234567", "call-1"),
            call_session_key(&account, "+15551234567", "call-2")
        );
    }

    #[test]
    fn start_call_twice_with_same_idempotency_key_is_a_duplicate() {
        let mut store = seeded();
        let first = store
            .start_call(&call("call-1", "acct-1", "idem-1", CallState::Queued))
            .expect("first start");
        let CallRecording::Recorded { call_id } = first else {
            panic!("expected Recorded, got {first:?}");
        };
        assert_eq!(call_id, "call-1");

        let second = store
            .start_call(&call("call-2", "acct-1", "idem-1", CallState::Queued))
            .expect("second start");
        assert_eq!(second, CallRecording::Duplicate { call_id });
        assert_eq!(store.recent_calls("acct-1", 10).expect("list").len(), 1);
    }

    #[test]
    fn advance_call_sets_started_and_ended_and_ignores_late_callbacks() {
        let mut store = seeded();
        store
            .start_call(&call("call-1", "acct-1", "idem-1", CallState::Queued))
            .expect("start");

        store
            .advance_call("call-1", CallState::Ringing, None, 1_100)
            .expect("ringing");
        let after_ringing = store.telecom_call("call-1").expect("get").expect("present");
        assert_eq!(after_ringing.started_at_ms, None);

        store
            .advance_call("call-1", CallState::InProgress, None, 1_200)
            .expect("in progress");
        let in_progress = store.telecom_call("call-1").expect("get").expect("present");
        assert_eq!(in_progress.started_at_ms, Some(1_200));
        assert_eq!(in_progress.ended_at_ms, None);

        store
            .advance_call("call-1", CallState::Completed, Some("hangup"), 1_300)
            .expect("completed");
        let completed = store.telecom_call("call-1").expect("get").expect("present");
        assert_eq!(completed.started_at_ms, Some(1_200));
        assert_eq!(completed.ended_at_ms, Some(1_300));
        assert_eq!(completed.state, CallState::Completed);

        // A late carrier callback trying to resurrect a finished call must
        // change nothing.
        store
            .advance_call("call-1", CallState::Ringing, Some("replayed"), 1_400)
            .expect("late callback");
        let still_completed = store.telecom_call("call-1").expect("get").expect("present");
        assert_eq!(still_completed, completed);
    }

    #[test]
    fn reconcile_stuck_calls_moves_live_and_leaves_terminal_alone() {
        let mut store = seeded();
        store
            .start_call(&call("call-1", "acct-1", "idem-1", CallState::Queued))
            .expect("start 1");
        store
            .start_call(&call("call-2", "acct-1", "idem-2", CallState::InProgress))
            .expect("start 2");
        store
            .start_call(&call("call-3", "acct-1", "idem-3", CallState::Completed))
            .expect("start 3");

        let moved = store.reconcile_stuck_calls(5_000).expect("reconcile");
        assert_eq!(moved, 2);

        assert_eq!(
            store
                .telecom_call("call-1")
                .expect("get")
                .expect("present")
                .state,
            CallState::NeedsReconciliation
        );
        assert_eq!(
            store
                .telecom_call("call-2")
                .expect("get")
                .expect("present")
                .state,
            CallState::NeedsReconciliation
        );
        assert_eq!(
            store
                .telecom_call("call-3")
                .expect("get")
                .expect("present")
                .state,
            CallState::Completed
        );
        assert_eq!(store.live_calls("acct-1").expect("live").len(), 0);
    }

    #[test]
    fn call_by_provider_id_finds_row_after_set() {
        let mut store = seeded();
        store
            .start_call(&call("call-1", "acct-1", "idem-1", CallState::Queued))
            .expect("start");
        assert_eq!(
            store
                .call_by_provider_id("acct-1", "prov-1")
                .expect("lookup before set"),
            None
        );

        store
            .set_call_provider_id("call-1", "prov-1", 1_500)
            .expect("set provider id");
        let found = store
            .call_by_provider_id("acct-1", "prov-1")
            .expect("lookup after set")
            .expect("present");
        assert_eq!(found.call_id, "call-1");
        assert_eq!(found.updated_at_ms, 1_500);
    }

    #[test]
    fn duplicate_carrier_event_is_deduped() {
        let mut store = seeded();
        let first = store
            .record_telecom_event(
                "acct-1",
                "prov-evt-1",
                "call.progress",
                None,
                "digest",
                1_000,
            )
            .expect("first");
        let TelecomEventRecording::Recorded { event_id } = first else {
            panic!("expected Recorded, got {first:?}");
        };
        let second = store
            .record_telecom_event(
                "acct-1",
                "prov-evt-1",
                "call.progress",
                None,
                "digest",
                2_000,
            )
            .expect("second");
        assert_eq!(second, TelecomEventRecording::Duplicate { event_id });
    }

    #[test]
    fn recent_calls_clamps_limit_and_orders_newest_first() {
        let mut store = seeded();
        for index in 0..3 {
            let mut row = call(
                &format!("call-{index}"),
                "acct-1",
                &format!("idem-{index}"),
                CallState::Completed,
            );
            row.created_at_ms = 1_000 + i64::from(index);
            row.updated_at_ms = row.created_at_ms;
            store.start_call(&row).expect("start");
        }

        let clamped_low = store.recent_calls("acct-1", 0).expect("limit 0");
        assert_eq!(clamped_low.len(), 1);

        let all = store.recent_calls("acct-1", 10).expect("limit 10");
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].call_id, "call-2");
        assert_eq!(all[1].call_id, "call-1");
        assert_eq!(all[2].call_id, "call-0");

        let clamped_high = store.recent_calls("acct-1", 10_000).expect("limit huge");
        assert_eq!(clamped_high.len(), 3);
    }
}
