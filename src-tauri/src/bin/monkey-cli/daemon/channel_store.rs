//! Durable storage for the messaging channel subsystem: accounts, sender
//! authorization, routes, the session map, the inbound event log, and the
//! outbox. Owns the `channel_*` tables created by `DAEMON_V5_SQL` in
//! `store.rs`.
//!
//! # Secrets
//!
//! No row written here ever carries a plaintext credential or pairing code.
//! `channel_accounts.credential_ref` is a keychain account name, and
//! `channel_sender_authorizations.pairing_code_digest` is a SHA-256 digest —
//! both are safe to find in a copied database file.

use rusqlite::{params, OptionalExtension, TransactionBehavior};

use little_monkey_lib::channels::ingress::ConversationSource;
use little_monkey_lib::channels::policy::{ChannelAccessPolicy, SenderState};
use little_monkey_lib::channels::routing::{ChannelRoute, RouteScope, RouteTarget};
use little_monkey_lib::channels::types::{
    BoundedMetadata, ChannelHealth, ChannelKind, HealthState,
};

use super::store::DaemonStore;

/// A stored messaging account.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelAccountRecord {
    pub account_id: String,
    pub kind: ChannelKind,
    pub label: String,
    pub enabled: bool,
    pub non_secret_config: serde_json::Value,
    pub credential_ref: Option<String>,
    pub access_policy: ChannelAccessPolicy,
    pub health: ChannelHealth,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Durable authorization state for one sender on one account.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredSenderAuthorization {
    pub sender_id: String,
    pub state: SenderState,
    pub pairing_code_digest: Option<String>,
    pub requested_at_ms: i64,
    pub expires_at_ms: Option<i64>,
    pub approved_at_ms: Option<i64>,
    pub blocked_at_ms: Option<i64>,
    pub display_label: Option<String>,
    pub metadata: BoundedMetadata,
}

/// Which way an event crossed the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventDirection {
    Inbound,
    Outbound,
}

impl EventDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            EventDirection::Inbound => "inbound",
            EventDirection::Outbound => "outbound",
        }
    }

    pub fn parse(value: &str) -> Result<EventDirection, String> {
        match value {
            "inbound" => Ok(EventDirection::Inbound),
            "outbound" => Ok(EventDirection::Outbound),
            other => Err(format!("unknown channel event direction '{other}'")),
        }
    }
}

/// What happened to an inbound message once the access/activation gates ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventDisposition {
    Accepted,
    Challenged,
    Ignored,
    Duplicate,
    Failed,
}

impl EventDisposition {
    pub fn as_str(self) -> &'static str {
        match self {
            EventDisposition::Accepted => "accepted",
            EventDisposition::Challenged => "challenged",
            EventDisposition::Ignored => "ignored",
            EventDisposition::Duplicate => "duplicate",
            EventDisposition::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Result<EventDisposition, String> {
        match value {
            "accepted" => Ok(EventDisposition::Accepted),
            "challenged" => Ok(EventDisposition::Challenged),
            "ignored" => Ok(EventDisposition::Ignored),
            "duplicate" => Ok(EventDisposition::Duplicate),
            "failed" => Ok(EventDisposition::Failed),
            other => Err(format!("unknown channel event disposition '{other}'")),
        }
    }
}

/// A row to insert into `channel_events`.
#[derive(Debug, Clone)]
pub struct NewChannelEvent {
    pub account_id: String,
    pub source: ConversationSource,
    pub direction: EventDirection,
    pub provider_event_id: String,
    pub conversation_id: String,
    pub thread_id: Option<String>,
    pub sender_id: Option<String>,
    pub envelope_json: String,
    pub disposition: EventDisposition,
    pub received_at_ms: i64,
}

/// A stored inbound/outbound event, as read back for the recent-events view.
///
/// Every field is read by callers outside this file (the events UI and audit
/// tooling) once they land; nothing in this store consumes them itself.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct StoredChannelEvent {
    pub event_id: String,
    pub account_id: String,
    pub source: ConversationSource,
    pub direction: EventDirection,
    pub provider_event_id: String,
    pub conversation_id: String,
    pub thread_id: Option<String>,
    pub sender_id: Option<String>,
    pub envelope_json: String,
    pub disposition: EventDisposition,
    pub ignore_reason: Option<String>,
    pub job_id: Option<String>,
    pub received_at_ms: i64,
    /// The accepted turn this event became, for an event that became one.
    pub ingress_id: Option<String>,
}

/// Outcome of [`DaemonStore::record_channel_event`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventRecording {
    Recorded { event_id: String },
    Duplicate { event_id: String },
}

/// A provider event this account has already recorded, as much of it as the
/// dedupe decision needs.
///
/// `envelope_json` rides along because the one case that cannot be answered
/// from identifiers alone — an accepted event with no turn behind it, written
/// by a build that committed the two separately — is recoverable only from the
/// envelope the row still holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingChannelEvent {
    pub event_id: String,
    pub disposition: EventDisposition,
    pub ingress_id: Option<String>,
    pub job_id: Option<String>,
    pub envelope_json: String,
}

/// One durably accepted inbound event that has not been processed yet.
///
/// Deliberately only what continuing it needs: the row to finalize, the
/// account whose adapter downloads its files, and the envelope the decision is
/// made from again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingChannelEvent {
    pub event_id: String,
    pub account_id: String,
    pub envelope_json: String,
}

/// What the caller decided one inbound envelope means, with everything that
/// decision has to write.
///
/// The adapters never see this: they produce a normalized envelope and nothing
/// else. It exists so the *decision* — which needs the policy, the route table
/// and a resolved execution context — can be made outside the transaction and
/// committed inside one.
pub enum EnvelopeDecision<'a> {
    /// Run it. The turn carries its own frozen route and execution context.
    Run {
        ingress: &'a little_monkey_lib::channels::ingress::ConversationIngress,
        params: &'a [String],
    },
    /// Ask the sender to pair. The authorization and the challenge reply are
    /// committed with the event, so a crash cannot leave a sender recorded as
    /// challenged with no challenge on its way.
    Challenge {
        sender: &'a StoredSenderAuthorization,
        reply: &'a NewOutboxMessage,
    },
    /// Recorded and dropped, with the reason.
    Ignore { reason: &'a str },
    /// Recorded as failed: an operator problem — no route, or a message this
    /// build cannot act on — that the sender is deliberately not told about.
    Refuse { error: &'a str },
}

/// What one durable acceptance committed.
///
/// Both variants mean the same thing to a transport: this delivery is safe to
/// acknowledge. They differ in whether anything still has to reach the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DurableAcceptance {
    /// The event and an accepted turn are committed. `existing` is set when the
    /// turn was already there — a redelivery arriving before the first
    /// submission finished — and carries where that turn had got to.
    Runnable {
        event_id: String,
        ingress_id: String,
        existing: Option<(super::ingress_store::IngressState, Option<String>)>,
    },
    /// The event and a final decision are committed. Nothing runs.
    Settled {
        event_id: String,
        disposition: EventDisposition,
    },
}

/// A row to insert into `channel_outbox`.
#[derive(Debug, Clone)]
pub struct NewOutboxMessage {
    pub account_id: String,
    pub conversation_id: String,
    pub thread_id: Option<String>,
    pub reply_to_provider_id: Option<String>,
    pub payload_json: String,
    pub payload_digest: String,
    pub idempotency_key: String,
    /// The durable tool invocation this send *is*, when one asked for it:
    /// the job and the runtime's tool-call id, and nothing about where or
    /// what is being sent. Unique across every account, so one invocation
    /// can never become two outbound intents however its destination is
    /// recomputed on a replay. `None` for sends with no invocation behind
    /// them — an inbound auto-reply is identified by the event it answers.
    pub invocation_id: Option<String>,
    pub max_attempts: u32,
    pub job_id: Option<String>,
    pub created_at_ms: i64,
}

/// A claimed or otherwise read-back outbox row.
///
/// Every field is read by the outbox worker and its diagnostics once they
/// land; nothing in this store consumes them itself.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct StoredOutboxMessage {
    pub outbox_id: String,
    pub account_id: String,
    pub conversation_id: String,
    pub thread_id: Option<String>,
    pub reply_to_provider_id: Option<String>,
    pub state: String,
    pub payload_json: String,
    pub payload_digest: String,
    pub idempotency_key: String,
    pub provider_message_id: Option<String>,
    pub attempt: u32,
    pub max_attempts: u32,
    pub next_attempt_at_ms: Option<i64>,
    pub last_error: Option<String>,
    pub job_id: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub sent_at_ms: Option<i64>,
}

/// The conversation a run must reply into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelOrigin {
    pub account_id: String,
    pub conversation_id: String,
    pub thread_id: Option<String>,
    /// The inbound message being answered, so the reply threads correctly on
    /// providers that support it.
    pub provider_event_id: String,
}

/// Outcome of [`DaemonStore::enqueue_channel_message`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboxEnqueue {
    Queued { outbox_id: String },
    AlreadyQueued { outbox_id: String },
}

/// Base retry backoff: 30 seconds, doubled per attempt, capped at 15 minutes.
const OUTBOX_BASE_BACKOFF_MS: i64 = 30_000;
const OUTBOX_MAX_BACKOFF_MS: i64 = 15 * 60 * 1000;

/// Ceiling on a provider-supplied `Retry-After`. Higher than our own cap
/// because an hour-long rate limit is a real thing a provider says, and lower
/// than "whatever the provider claims", which is not a number we control.
const MAX_PROVIDER_RETRY_AFTER_MS: i64 = 60 * 60 * 1000;

fn backoff_for_attempt(attempt: u32) -> i64 {
    let exponent = attempt.saturating_sub(1).min(20);
    let scaled = OUTBOX_BASE_BACKOFF_MS.saturating_mul(1_i64 << exponent);
    scaled.min(OUTBOX_MAX_BACKOFF_MS)
}

fn new_event_id() -> String {
    format!("chev-{}", uuid::Uuid::new_v4().simple())
}

fn new_outbox_id() -> String {
    format!("chout-{}", uuid::Uuid::new_v4().simple())
}

impl DaemonStore {
    // -- Accounts ---------------------------------------------------------

    pub fn upsert_channel_account(&mut self, account: &ChannelAccountRecord) -> Result<(), String> {
        let non_secret_config =
            serde_json::to_string(&account.non_secret_config).map_err(|error| error.to_string())?;
        let access_policy =
            serde_json::to_string(&account.access_policy).map_err(|error| error.to_string())?;
        self.connection
            .execute(
                "INSERT INTO channel_accounts (
                    account_id, kind, label, enabled, non_secret_config_json, credential_ref,
                    access_policy_json, health, health_detail, last_error, last_probe_at_ms,
                    created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(account_id) DO UPDATE SET
                    kind = excluded.kind,
                    label = excluded.label,
                    enabled = excluded.enabled,
                    non_secret_config_json = excluded.non_secret_config_json,
                    credential_ref = excluded.credential_ref,
                    access_policy_json = excluded.access_policy_json,
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
                    non_secret_config,
                    account.credential_ref,
                    access_policy,
                    account.health.state.as_str(),
                    account.health.detail,
                    account.health.last_error,
                    account.health.probed_at_ms,
                    account.created_at_ms,
                    account.updated_at_ms,
                ],
            )
            .map_err(|error| format!("Failed to upsert channel account: {error}"))?;
        Ok(())
    }

    pub fn channel_account(
        &self,
        account_id: &str,
    ) -> Result<Option<ChannelAccountRecord>, String> {
        self.connection
            .query_row(
                "SELECT account_id, kind, label, enabled, non_secret_config_json, credential_ref,
                        access_policy_json, health, health_detail, last_error, last_probe_at_ms,
                        created_at_ms, updated_at_ms
                 FROM channel_accounts WHERE account_id=?1",
                [account_id],
                read_channel_account,
            )
            .optional()
            .map_err(|error| error.to_string())?
            .transpose()
    }

    pub fn channel_accounts(&self) -> Result<Vec<ChannelAccountRecord>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT account_id, kind, label, enabled, non_secret_config_json, credential_ref,
                        access_policy_json, health, health_detail, last_error, last_probe_at_ms,
                        created_at_ms, updated_at_ms
                 FROM channel_accounts ORDER BY created_at_ms ASC",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], read_channel_account)
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
            .into_iter()
            .collect()
    }

    pub fn set_channel_account_health(
        &mut self,
        account_id: &str,
        health: &ChannelHealth,
        now_ms: i64,
    ) -> Result<(), String> {
        let changed = self
            .connection
            .execute(
                "UPDATE channel_accounts
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
            return Err(format!("Unknown channel account '{account_id}'"));
        }
        Ok(())
    }

    pub fn delete_channel_account(&mut self, account_id: &str) -> Result<bool, String> {
        self.connection
            .execute(
                "DELETE FROM channel_accounts WHERE account_id=?1",
                [account_id],
            )
            .map(|changed| changed == 1)
            .map_err(|error| error.to_string())
    }

    // -- Sender authorization ----------------------------------------------

    pub fn channel_sender(
        &self,
        account_id: &str,
        sender_id: &str,
    ) -> Result<Option<StoredSenderAuthorization>, String> {
        self.connection
            .query_row(
                "SELECT sender_id, state, pairing_code_digest, requested_at_ms, expires_at_ms,
                        approved_at_ms, blocked_at_ms, display_label, metadata_json
                 FROM channel_sender_authorizations WHERE account_id=?1 AND sender_id=?2",
                params![account_id, sender_id],
                read_sender_authorization,
            )
            .optional()
            .map_err(|error| error.to_string())?
            .transpose()
    }

    pub fn upsert_channel_sender(
        &mut self,
        account_id: &str,
        sender_id: &str,
        record: &StoredSenderAuthorization,
    ) -> Result<(), String> {
        upsert_sender(&self.connection, account_id, sender_id, record)
    }

    pub fn pending_channel_senders(
        &self,
        account_id: &str,
    ) -> Result<Vec<StoredSenderAuthorization>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT sender_id, state, pairing_code_digest, requested_at_ms, expires_at_ms,
                        approved_at_ms, blocked_at_ms, display_label, metadata_json
                 FROM channel_sender_authorizations
                 WHERE account_id=?1 AND state='pending'
                 ORDER BY requested_at_ms ASC",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([account_id], read_sender_authorization)
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
            .into_iter()
            .collect()
    }

    pub fn count_pending_channel_senders(&self, account_id: &str) -> Result<usize, String> {
        self.connection
            .query_row(
                "SELECT COUNT(*) FROM channel_sender_authorizations
                 WHERE account_id=?1 AND state='pending'",
                [account_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count as usize)
            .map_err(|error| error.to_string())
    }

    pub fn expire_channel_senders(&mut self, now_ms: i64) -> Result<u32, String> {
        self.connection
            .execute(
                "DELETE FROM channel_sender_authorizations
                 WHERE state='pending' AND expires_at_ms IS NOT NULL AND expires_at_ms <= ?1",
                [now_ms],
            )
            .map(|changed| u32::try_from(changed).unwrap_or(u32::MAX))
            .map_err(|error| error.to_string())
    }

    // -- Public callback base ------------------------------------------------

    /// Set or clear the one externally advertised base URL webhook providers
    /// deliver to. Stored daemon-wide: the operator runs one tunnel or reverse
    /// proxy in front of one webhook listener, not one per account.
    pub fn set_channel_public_base_url(&mut self, base: Option<&str>) -> Result<(), String> {
        match base {
            Some(base) => {
                let normalized = validate_public_base_url(base)?;
                self.set_meta(CHANNEL_PUBLIC_BASE_URL_KEY, &normalized)
            }
            None => {
                self.connection
                    .execute(
                        "DELETE FROM daemon_meta WHERE key=?1",
                        [CHANNEL_PUBLIC_BASE_URL_KEY],
                    )
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
        }
    }

    pub fn channel_public_base_url(&self) -> Result<Option<String>, String> {
        self.get_meta(CHANNEL_PUBLIC_BASE_URL_KEY)
    }

    /// The complete callback URL an operator pastes into a provider's console
    /// for one account, or `None` while no public base URL is configured.
    ///
    /// This is the one place the full URL is composed; every front end shows
    /// what this returns rather than gluing a host onto the path itself.
    pub fn channel_callback_url(&self, account_id: &str) -> Result<Option<String>, String> {
        Ok(self
            .channel_public_base_url()?
            .map(|base| format!("{base}{}", channel_callback_path(account_id))))
    }

    // -- Routes --------------------------------------------------------------

    pub fn insert_channel_route(&mut self, route: &ChannelRoute) -> Result<(), String> {
        route
            .scope
            .validate()
            .map_err(|error| error.message().to_string())?;
        self.reject_route_conflict(&route.scope, None)?;
        let scope_json = serde_json::to_string(&route.scope).map_err(|error| error.to_string())?;
        let target_json =
            serde_json::to_string(&route.target).map_err(|error| error.to_string())?;
        self.connection
            .execute(
                "INSERT INTO channel_routes (
                    route_id, scope_json, target_json, enabled, created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    route.route_id,
                    scope_json,
                    target_json,
                    route.enabled,
                    route.created_at_ms,
                    route.updated_at_ms,
                ],
            )
            .map_err(|error| format!("Failed to insert channel route: {error}"))?;
        Ok(())
    }

    /// Replace a route's scope and target in place, keeping its identity.
    ///
    /// The same rules as insertion: the scope must be on the ladder and must
    /// not tie with another route, because an edit that would make two routes
    /// ambiguous should fail while the operator is looking at it, not when a
    /// message arrives.
    pub fn update_channel_route(&mut self, route: &ChannelRoute) -> Result<(), String> {
        route
            .scope
            .validate()
            .map_err(|error| error.message().to_string())?;
        self.reject_route_conflict(&route.scope, Some(&route.route_id))?;
        let scope_json = serde_json::to_string(&route.scope).map_err(|error| error.to_string())?;
        let target_json =
            serde_json::to_string(&route.target).map_err(|error| error.to_string())?;
        let changed = self
            .connection
            .execute(
                "UPDATE channel_routes
                 SET scope_json=?2, target_json=?3, enabled=?4, updated_at_ms=?5
                 WHERE route_id=?1",
                params![
                    route.route_id,
                    scope_json,
                    target_json,
                    route.enabled,
                    route.updated_at_ms,
                ],
            )
            .map_err(|error| format!("Failed to update channel route: {error}"))?;
        if changed == 0 {
            return Err(format!("No such route '{}'", route.route_id));
        }
        Ok(())
    }

    /// Flip a route on or off without touching what it routes to.
    pub fn set_channel_route_enabled(
        &mut self,
        route_id: &str,
        enabled: bool,
        now_ms: i64,
    ) -> Result<bool, String> {
        self.connection
            .execute(
                "UPDATE channel_routes SET enabled=?2, updated_at_ms=?3 WHERE route_id=?1",
                params![route_id, enabled, now_ms],
            )
            .map(|changed| changed == 1)
            .map_err(|error| error.to_string())
    }

    /// Refuse a scope that would tie with an existing route.
    ///
    /// Compared semantically — parsed scopes, not stored JSON bytes — and at
    /// the same specificity only: a message matching two routes on different
    /// rungs resolves by rung, but two overlapping scopes on one rung can
    /// only ever be reported as ambiguous.
    fn reject_route_conflict(
        &self,
        scope: &little_monkey_lib::channels::routing::RouteScope,
        exclude_route_id: Option<&str>,
    ) -> Result<(), String> {
        for existing in self.channel_routes()? {
            if Some(existing.route_id.as_str()) == exclude_route_id {
                continue;
            }
            if existing.scope.specificity() == scope.specificity() && existing.scope.overlaps(scope)
            {
                return Err(format!(
                    "Route '{}' already owns this scope: two routes at the same specificity \
                     would be ambiguous for any message matching both",
                    existing.route_id
                ));
            }
        }
        Ok(())
    }

    pub fn channel_routes(&self) -> Result<Vec<ChannelRoute>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT route_id, scope_json, target_json, enabled, created_at_ms, updated_at_ms
                 FROM channel_routes ORDER BY created_at_ms ASC",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], read_channel_route)
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
            .into_iter()
            .collect()
    }

    pub fn delete_channel_route(&mut self, route_id: &str) -> Result<bool, String> {
        self.connection
            .execute("DELETE FROM channel_routes WHERE route_id=?1", [route_id])
            .map(|changed| changed == 1)
            .map_err(|error| error.to_string())
    }

    // -- Session map -----------------------------------------------------

    pub fn channel_session(&self, session_key: &str) -> Result<Option<String>, String> {
        self.connection
            .query_row(
                "SELECT session_id FROM channel_session_map WHERE session_key=?1",
                [session_key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    pub fn bind_channel_session(
        &mut self,
        session_key: &str,
        account_id: &str,
        conversation_id: &str,
        thread_id: Option<&str>,
        session_id: &str,
        now_ms: i64,
    ) -> Result<String, String> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let bound = bind_session(
            &transaction,
            session_key,
            account_id,
            conversation_id,
            thread_id,
            session_id,
            now_ms,
        )?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(bound)
    }

    // -- Events ------------------------------------------------------------

    pub fn record_channel_event(
        &mut self,
        event: &NewChannelEvent,
    ) -> Result<EventRecording, String> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let result = insert_channel_event(&transaction, event)?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(result)
    }

    pub fn set_channel_event_disposition(
        &mut self,
        event_id: &str,
        disposition: EventDisposition,
        ignore_reason: Option<&str>,
        job_id: Option<&str>,
    ) -> Result<(), String> {
        let changed = self
            .connection
            .execute(
                "UPDATE channel_events SET disposition=?2, ignore_reason=?3, job_id=?4
                 WHERE event_id=?1",
                params![event_id, disposition.as_str(), ignore_reason, job_id],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err(format!("Unknown channel event '{event_id}'"));
        }
        Ok(())
    }

    /// What this account already recorded for one provider event id, if
    /// anything.
    ///
    /// The read half of provider dedupe, and deliberately richer than a
    /// boolean: "we have seen this" is not the same fact as "this was fully
    /// handled", and an ACK owed to the provider depends on the second.
    pub fn existing_channel_event(
        &self,
        source: ConversationSource,
        account_id: &str,
        direction: EventDirection,
        provider_event_id: &str,
    ) -> Result<Option<ExistingChannelEvent>, String> {
        self.connection
            .query_row(
                "SELECT event_id, disposition, ingress_id, job_id, envelope_json
                 FROM channel_events
                 WHERE source=?1 AND account_id=?2 AND direction=?3 AND provider_event_id=?4",
                params![
                    source.as_str(),
                    account_id,
                    direction.as_str(),
                    provider_event_id
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| error.to_string())?
            .map(
                |(event_id, disposition, ingress_id, job_id, envelope_json)| {
                    Ok(ExistingChannelEvent {
                        event_id,
                        disposition: EventDisposition::parse(&disposition)?,
                        ingress_id,
                        job_id,
                        envelope_json,
                    })
                },
            )
            .transpose()
    }

    /// Inbound events that are durably accepted and own no turn yet.
    ///
    /// **This is the queue the webhook route feeds.** A delivered-to provider
    /// is acknowledged the moment its event is committed, which is before
    /// anything has been downloaded, routed or run; the row sits in exactly
    /// this shape until the channel worker picks it up. Ordering by arrival
    /// means the oldest unfinished message is always the next one continued,
    /// including the ones a restart interrupted.
    ///
    /// A database an older build wrote can hold the same shape for a different
    /// reason — a crash between two transactions that are now one — and it is
    /// continued identically, from the envelope the row still carries.
    pub fn accepted_events_awaiting_processing(
        &self,
        limit: u32,
    ) -> Result<Vec<PendingChannelEvent>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT event_id, account_id, envelope_json
                 FROM channel_events
                 WHERE direction='inbound' AND disposition='accepted'
                   AND ingress_id IS NULL AND job_id IS NULL
                   AND source='messaging_channel'
                 ORDER BY received_at_ms ASC LIMIT ?1",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([i64::from(limit)], |row| {
                Ok(PendingChannelEvent {
                    event_id: row.get(0)?,
                    account_id: row.get(1)?,
                    envelope_json: row.get(2)?,
                })
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    /// Replace one event's stored envelope.
    ///
    /// The only writer is attachment hydration, and the reason it writes at all
    /// is restart safety: a file that was downloaded and stored before the
    /// process died must not be downloaded again, and one that failed must
    /// carry its reason into the turn. Nothing else about the row moves.
    pub fn set_channel_event_envelope(
        &mut self,
        event_id: &str,
        envelope_json: &str,
    ) -> Result<(), String> {
        let changed = self
            .connection
            .execute(
                "UPDATE channel_events SET envelope_json=?2 WHERE event_id=?1",
                params![event_id, envelope_json],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err(format!("Unknown channel event '{event_id}'"));
        }
        Ok(())
    }

    /// Record one inbound provider event and everything its decision implies,
    /// in a single transaction.
    ///
    /// **This is the durable acceptance boundary.** Before it, nothing is
    /// committed and the provider must redeliver; after it, enough state exists
    /// to finish the work from a cold start, so the provider may be
    /// acknowledged and its cursor advanced. There is no state in between: an
    /// event recorded as accepted and the turn it became are committed
    /// together, which is what stops a crash from leaving a row that suppresses
    /// redelivery for a message nothing will ever run.
    ///
    /// Everything the decision needed — the access policy, the route, the
    /// frozen execution context — is resolved by the caller *before* this is
    /// called, so no file, keychain or network read happens with the
    /// transaction open. What happens after it is the queue submission, which
    /// is recoverable precisely because this committed first.
    pub fn accept_channel_envelope(
        &mut self,
        event: &NewChannelEvent,
        decision: &EnvelopeDecision<'_>,
        now_ms: i64,
    ) -> Result<DurableAcceptance, String> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        // An id already here is adopted rather than refused: this is reached
        // only when the caller has established that the existing row's decision
        // never completed, and re-inserting is not one of the options.
        let event_id = match insert_channel_event(&transaction, event)? {
            EventRecording::Recorded { event_id } => event_id,
            EventRecording::Duplicate { event_id } => event_id,
        };
        super::fail_points::fire(super::fail_points::FailPoint::AfterEventInsert)?;

        let accepted = match decision {
            EnvelopeDecision::Run { ingress, params } => {
                let acceptance = super::ingress_store::insert_ingress_turn(
                    &transaction,
                    ingress,
                    params,
                    now_ms,
                )?;
                let (ingress_id, existing) = match acceptance {
                    super::ingress_store::IngressAcceptance::Accepted { ingress_id } => {
                        (ingress_id, None)
                    }
                    super::ingress_store::IngressAcceptance::Existing {
                        ingress_id,
                        state,
                        job_id,
                    } => (ingress_id, Some((state, job_id))),
                };
                bind_session(
                    &transaction,
                    &ingress.session_key,
                    &event.account_id,
                    &event.conversation_id,
                    event.thread_id.as_deref(),
                    &ingress.session_key,
                    now_ms,
                )?;
                finalize_event(
                    &transaction,
                    &event_id,
                    EventDisposition::Accepted,
                    None,
                    Some(&ingress_id),
                    None,
                )?;
                DurableAcceptance::Runnable {
                    event_id,
                    ingress_id,
                    existing,
                }
            }
            EnvelopeDecision::Challenge { sender, reply } => {
                upsert_sender(&transaction, &event.account_id, &sender.sender_id, sender)?;
                insert_outbox_message(&transaction, reply)?;
                finalize_event(
                    &transaction,
                    &event_id,
                    EventDisposition::Challenged,
                    None,
                    None,
                    None,
                )?;
                DurableAcceptance::Settled {
                    event_id,
                    disposition: EventDisposition::Challenged,
                }
            }
            EnvelopeDecision::Ignore { reason } => {
                finalize_event(
                    &transaction,
                    &event_id,
                    EventDisposition::Ignored,
                    Some(reason),
                    None,
                    None,
                )?;
                DurableAcceptance::Settled {
                    event_id,
                    disposition: EventDisposition::Ignored,
                }
            }
            EnvelopeDecision::Refuse { error } => {
                finalize_event(
                    &transaction,
                    &event_id,
                    EventDisposition::Failed,
                    Some(error),
                    None,
                    None,
                )?;
                DurableAcceptance::Settled {
                    event_id,
                    disposition: EventDisposition::Failed,
                }
            }
        };
        super::fail_points::fire(super::fail_points::FailPoint::BeforeAcceptCommit)?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(accepted)
    }

    /// Point an already recorded event at the turn that owns it.
    ///
    /// Only ever used to repair a link a previous build never wrote; the
    /// acceptance path writes both in one transaction and never needs this.
    pub fn link_channel_event_to_ingress(
        &mut self,
        event_id: &str,
        ingress_id: &str,
    ) -> Result<(), String> {
        self.connection
            .execute(
                "UPDATE channel_events SET ingress_id=?2 WHERE event_id=?1",
                params![event_id, ingress_id],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn recent_channel_events(
        &self,
        account_id: &str,
        limit: u32,
    ) -> Result<Vec<StoredChannelEvent>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT event_id, account_id, source, direction, provider_event_id, conversation_id,
                        thread_id, sender_id, envelope_json, disposition, ignore_reason, job_id,
                        received_at_ms, ingress_id
                 FROM channel_events WHERE account_id=?1
                 ORDER BY received_at_ms DESC LIMIT ?2",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(params![account_id, i64::from(limit)], read_channel_event)
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
            .into_iter()
            .collect()
    }

    // -- Outbox --------------------------------------------------------------

    pub fn enqueue_channel_message(
        &mut self,
        row: &NewOutboxMessage,
    ) -> Result<OutboxEnqueue, String> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let result = insert_outbox_message(&transaction, row)?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(result)
    }

    pub fn claim_outbox_batch(
        &mut self,
        now_ms: i64,
        limit: u32,
    ) -> Result<Vec<StoredOutboxMessage>, String> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let ids: Vec<String> = {
            let mut statement = transaction
                .prepare(
                    "SELECT outbox_id FROM channel_outbox
                     WHERE state='queued' AND (next_attempt_at_ms IS NULL OR next_attempt_at_ms <= ?1)
                     ORDER BY created_at_ms ASC LIMIT ?2",
                )
                .map_err(|error| error.to_string())?;
            let rows = statement
                .query_map(params![now_ms, i64::from(limit)], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(|error| error.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?
        };
        for outbox_id in &ids {
            transaction
                .execute(
                    "UPDATE channel_outbox SET state='sending', attempt=attempt+1, updated_at_ms=?2
                     WHERE outbox_id=?1",
                    params![outbox_id, now_ms],
                )
                .map_err(|error| error.to_string())?;
        }
        let claimed = {
            let mut statement = transaction
                .prepare(
                    "SELECT outbox_id, account_id, conversation_id, thread_id, reply_to_provider_id,
                            state, payload_json, payload_digest, idempotency_key, provider_message_id,
                            attempt, max_attempts, next_attempt_at_ms, last_error, job_id,
                            created_at_ms, updated_at_ms, sent_at_ms
                     FROM channel_outbox WHERE outbox_id=?1",
                )
                .map_err(|error| error.to_string())?;
            let mut claimed = Vec::with_capacity(ids.len());
            for outbox_id in &ids {
                let row = statement
                    .query_row([outbox_id], read_outbox_message)
                    .map_err(|error| error.to_string())?;
                claimed.push(row);
            }
            claimed
        };
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(claimed)
    }

    pub fn complete_outbox_send(
        &mut self,
        outbox_id: &str,
        outcome: &SendOutcome,
        now_ms: i64,
    ) -> Result<(), String> {
        match outcome {
            SendOutcome::Sent {
                provider_message_id,
            } => {
                let changed = self
                    .connection
                    .execute(
                        "UPDATE channel_outbox
                         SET state='sent', provider_message_id=?2, sent_at_ms=?3, updated_at_ms=?3,
                             last_error=NULL, next_attempt_at_ms=NULL
                         WHERE outbox_id=?1",
                        params![outbox_id, provider_message_id, now_ms],
                    )
                    .map_err(|error| error.to_string())?;
                if changed != 1 {
                    return Err(format!("Unknown outbox message '{outbox_id}'"));
                }
                Ok(())
            }
            SendOutcome::RetryableFailure {
                error,
                retry_after_ms,
            } => {
                let row: Option<(i64, i64)> = self
                    .connection
                    .query_row(
                        "SELECT attempt, max_attempts FROM channel_outbox WHERE outbox_id=?1",
                        [outbox_id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()
                    .map_err(|error| error.to_string())?;
                let Some((attempt, max_attempts)) = row else {
                    return Err(format!("Unknown outbox message '{outbox_id}'"));
                };
                if attempt >= max_attempts {
                    self.connection
                        .execute(
                            "UPDATE channel_outbox
                             SET state='failed', last_error=?2, updated_at_ms=?3, next_attempt_at_ms=NULL
                             WHERE outbox_id=?1",
                            params![outbox_id, error, now_ms],
                        )
                        .map_err(|error| error.to_string())?;
                    return Ok(());
                }
                // A provider-supplied `Retry-After` is honored but clamped: it is
                // an untrusted number, and a bad or hostile one would otherwise
                // park a message past any horizon an operator would look at.
                let backoff = retry_after_ms
                    .map(|requested| requested.clamp(0, MAX_PROVIDER_RETRY_AFTER_MS))
                    .unwrap_or_else(|| {
                        backoff_for_attempt(u32::try_from(attempt).unwrap_or(u32::MAX))
                    });
                self.connection
                    .execute(
                        "UPDATE channel_outbox
                         SET state='queued', last_error=?2, next_attempt_at_ms=?3, updated_at_ms=?4
                         WHERE outbox_id=?1",
                        params![outbox_id, error, now_ms.saturating_add(backoff), now_ms],
                    )
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            SendOutcome::PermanentFailure { error } => {
                let changed = self
                    .connection
                    .execute(
                        "UPDATE channel_outbox
                         SET state='failed', last_error=?2, updated_at_ms=?3, next_attempt_at_ms=NULL
                         WHERE outbox_id=?1",
                        params![outbox_id, error, now_ms],
                    )
                    .map_err(|error| error.to_string())?;
                if changed != 1 {
                    return Err(format!("Unknown outbox message '{outbox_id}'"));
                }
                Ok(())
            }
            SendOutcome::NeedsReconciliation { error } => {
                // The send may have already reached the provider — moved out of
                // 'queued' for good, never picked up by `claim_outbox_batch` again.
                let changed = self
                    .connection
                    .execute(
                        "UPDATE channel_outbox
                         SET state='needs_reconciliation', last_error=?2, updated_at_ms=?3,
                             next_attempt_at_ms=NULL
                         WHERE outbox_id=?1",
                        params![outbox_id, error, now_ms],
                    )
                    .map_err(|error| error.to_string())?;
                if changed != 1 {
                    return Err(format!("Unknown outbox message '{outbox_id}'"));
                }
                Ok(())
            }
        }
    }

    /// Put a claimed row back without spending its attempt.
    ///
    /// The claim itself incremented `attempt`, but the drain never tried the
    /// send — its account's adapter was not loaded. Handing the attempt back
    /// is what keeps a temporarily disabled account from burning through
    /// `max_attempts` and permanently failing replies nothing ever sent.
    pub fn release_outbox_claim(
        &mut self,
        outbox_id: &str,
        delay_ms: i64,
        now_ms: i64,
    ) -> Result<(), String> {
        let changed = self
            .connection
            .execute(
                "UPDATE channel_outbox
                 SET state='queued',
                     attempt=CASE WHEN attempt>0 THEN attempt-1 ELSE 0 END,
                     next_attempt_at_ms=?2, updated_at_ms=?3
                 WHERE outbox_id=?1 AND state='sending'",
                params![outbox_id, now_ms.saturating_add(delay_ms), now_ms],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err(format!("Unknown or unclaimed outbox message '{outbox_id}'"));
        }
        Ok(())
    }

    pub fn cancel_outbox_message(&mut self, outbox_id: &str, now_ms: i64) -> Result<bool, String> {
        self.connection
            .execute(
                "UPDATE channel_outbox SET state='cancelled', updated_at_ms=?2
                 WHERE outbox_id=?1 AND state='queued'",
                params![outbox_id, now_ms],
            )
            .map(|changed| changed == 1)
            .map_err(|error| error.to_string())
    }

    /// Rows left in `sending` by a daemon that crashed mid-send go to
    /// `needs_reconciliation`, not back to `queued`: the request may already have
    /// reached the provider, so an automatic retry risks a duplicate send. An
    /// operator (or a reconciliation pass that can check with the provider) has
    /// to clear these explicitly.
    pub fn requeue_stuck_sending(&mut self, now_ms: i64) -> Result<u32, String> {
        self.connection
            .execute(
                "UPDATE channel_outbox
                 SET state='needs_reconciliation', updated_at_ms=?1
                 WHERE state='sending'",
                [now_ms],
            )
            .map(|changed| u32::try_from(changed).unwrap_or(u32::MAX))
            .map_err(|error| error.to_string())
    }

    /// Where a run's reply belongs: the conversation whose accepted inbound
    /// event produced this job.
    ///
    /// This is the whole reason `send_message` needs no destination
    /// parameter. The answer comes from the durable event log, so a model that
    /// is told "reply to my other account instead" by the very message it is
    /// reading has nothing to act on.
    pub fn channel_origin_for_job(&self, job_id: &str) -> Result<Option<ChannelOrigin>, String> {
        self.connection
            .query_row(
                "SELECT account_id, conversation_id, thread_id, provider_event_id, envelope_json
                 FROM channel_events
                 WHERE job_id=?1 AND direction='inbound'
                 ORDER BY received_at_ms DESC LIMIT 1",
                [job_id],
                |row| {
                    let event_id: String = row.get(3)?;
                    // The id a reply anchors to is not always the id the log
                    // dedupes by: Telegram polls by update_id but addresses
                    // replies by chat-scoped message_id. An adapter whose two
                    // ids differ records the reply anchor in envelope metadata.
                    let envelope_json: Option<String> = row.get(4)?;
                    let anchor = envelope_json
                        .as_deref()
                        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
                        .and_then(|envelope| {
                            envelope
                                .get("metadata")?
                                .get("provider_message_id")?
                                .as_str()
                                .map(str::to_string)
                        })
                        .unwrap_or(event_id);
                    Ok(ChannelOrigin {
                        account_id: row.get(0)?,
                        conversation_id: row.get(1)?,
                        thread_id: row.get(2)?,
                        provider_event_id: anchor,
                    })
                },
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    /// The inbound envelope that produced this job, as stored.
    pub fn inbound_envelope_for_job(&self, job_id: &str) -> Result<Option<String>, String> {
        self.connection
            .query_row(
                "SELECT envelope_json FROM channel_events
                 WHERE job_id=?1 AND direction='inbound'
                 ORDER BY received_at_ms DESC LIMIT 1",
                [job_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    /// How many outbound rows this job queued.
    ///
    /// Test-only: the send path used to derive its idempotency key from this
    /// count, which shifted under a replayed run and let the first message go
    /// out twice. The key is derived from the invocation identity now, and
    /// this remains only as the assertion tests make about how many rows a
    /// run produced.
    #[cfg(test)]
    pub fn outbox_count_for_job(&self, job_id: &str) -> Result<u32, String> {
        self.connection
            .query_row(
                "SELECT COUNT(*) FROM channel_outbox WHERE job_id=?1",
                [job_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| u32::try_from(count).unwrap_or(u32::MAX))
            .map_err(|error| error.to_string())
    }

    /// The payload of a message we sent, looked up by the id the provider gave
    /// it. Used by the inbound path to tell a reply to our own message from a
    /// fresh one, which is how an automated exchange gets a depth at all.
    pub fn sent_outbox_payload(
        &self,
        account_id: &str,
        provider_message_id: &str,
    ) -> Result<Option<String>, String> {
        self.connection
            .query_row(
                "SELECT payload_json FROM channel_outbox
                 WHERE account_id=?1 AND provider_message_id=?2 AND state='sent'",
                params![account_id, provider_message_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    // -- Cursors -------------------------------------------------------------

    pub fn channel_cursor(&self, account_id: &str, key: &str) -> Result<Option<String>, String> {
        self.connection
            .query_row(
                "SELECT cursor_value FROM channel_cursors WHERE account_id=?1 AND cursor_key=?2",
                params![account_id, key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    pub fn set_channel_cursor(
        &mut self,
        account_id: &str,
        key: &str,
        value: &str,
        now_ms: i64,
    ) -> Result<(), String> {
        if value.len() > 4096 {
            return Err(format!(
                "Channel cursor '{key}' value exceeds 4096 bytes ({} bytes)",
                value.len()
            ));
        }
        self.connection
            .execute(
                "INSERT INTO channel_cursors (account_id, cursor_key, cursor_value, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(account_id, cursor_key) DO UPDATE SET
                    cursor_value = excluded.cursor_value,
                    updated_at_ms = excluded.updated_at_ms",
                params![account_id, key, value, now_ms],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    // -- Conversation references ---------------------------------------------

    /// Where this provider wants a reply to one conversation sent.
    ///
    /// `None` means nothing authenticated has ever named an address for it, and
    /// an adapter that needs one says so rather than guessing: a reply endpoint
    /// invented from a conversation id is how a message goes to the wrong
    /// tenant.
    pub fn channel_conversation_ref(
        &self,
        account_id: &str,
        conversation_id: &str,
    ) -> Result<Option<serde_json::Value>, String> {
        let stored: Option<String> = self
            .connection
            .query_row(
                "SELECT reference_json FROM channel_conversation_refs
                 WHERE account_id=?1 AND conversation_id=?2",
                params![account_id, conversation_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        stored
            .map(|json| serde_json::from_str(&json).map_err(|error| error.to_string()))
            .transpose()
    }

    /// Record where a reply to one conversation goes, replacing whatever was
    /// there.
    ///
    /// The newest authenticated address wins: providers move conversations
    /// between regional endpoints, and the stale one answers 404 rather than
    /// delivering. Callers must have authenticated and validated the value
    /// first — see this table's own doc for what may and may not live here.
    pub fn set_channel_conversation_ref(
        &mut self,
        account_id: &str,
        conversation_id: &str,
        reference: &serde_json::Value,
        now_ms: i64,
    ) -> Result<(), String> {
        let json = serde_json::to_string(reference).map_err(|error| error.to_string())?;
        if json.len() > 8192 {
            return Err("That conversation reference is too large to store".to_string());
        }
        self.connection
            .execute(
                "INSERT INTO channel_conversation_refs (
                    account_id, conversation_id, reference_json, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(account_id, conversation_id) DO UPDATE SET
                    reference_json = excluded.reference_json,
                    updated_at_ms = excluded.updated_at_ms",
                params![account_id, conversation_id, json, now_ms],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

// Re-export so callers of `complete_outbox_send` do not need a separate
// import path for the outcome type.
pub use little_monkey_lib::channels::types::SendOutcome;

fn read_channel_account(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<ChannelAccountRecord, String>> {
    let account_id: String = row.get(0)?;
    let kind_token: String = row.get(1)?;
    let Some(kind) = ChannelKind::parse(&kind_token) else {
        return Ok(Err(format!(
            "channel account '{account_id}' has unknown kind '{kind_token}'"
        )));
    };
    let non_secret_config_json: String = row.get(4)?;
    let access_policy_json: String = row.get(6)?;
    let health_token: String = row.get(7)?;
    let Some(health_state) = HealthState::parse(&health_token) else {
        return Ok(Err(format!(
            "channel account '{account_id}' has unknown health state '{health_token}'"
        )));
    };

    let label: String = row.get(2)?;
    let enabled: i64 = row.get(3)?;
    let credential_ref: Option<String> = row.get(5)?;
    let detail: Option<String> = row.get(8)?;
    let last_error: Option<String> = row.get(9)?;
    let probed_at_ms: i64 = row.get(10)?;
    let created_at_ms: i64 = row.get(11)?;
    let updated_at_ms: i64 = row.get(12)?;

    let non_secret_config: Result<serde_json::Value, serde_json::Error> =
        serde_json::from_str(&non_secret_config_json);
    let access_policy: Result<ChannelAccessPolicy, serde_json::Error> =
        serde_json::from_str(&access_policy_json);
    let (non_secret_config, access_policy) = match (non_secret_config, access_policy) {
        (Ok(non_secret_config), Ok(access_policy)) => (non_secret_config, access_policy),
        _ => {
            return Ok(Err(format!(
                "channel account '{account_id}' has malformed JSON"
            )))
        }
    };

    Ok(Ok(ChannelAccountRecord {
        account_id,
        kind,
        label,
        enabled: enabled != 0,
        non_secret_config,
        credential_ref,
        access_policy,
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

fn read_sender_authorization(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<StoredSenderAuthorization, String>> {
    let sender_id: String = row.get(0)?;
    let state_token: String = row.get(1)?;
    let Some(state) = SenderState::parse(&state_token) else {
        return Ok(Err(format!(
            "channel sender '{sender_id}' has unknown state '{state_token}'"
        )));
    };
    let metadata_json: String = row.get(8)?;
    let entries: Result<Vec<(String, String)>, serde_json::Error> =
        serde_json::from_str(&metadata_json);
    let entries = match entries {
        Ok(entries) => entries,
        Err(error) => {
            return Ok(Err(format!(
                "channel sender '{sender_id}' has malformed metadata: {error}"
            )))
        }
    };
    Ok(Ok(StoredSenderAuthorization {
        sender_id,
        state,
        pairing_code_digest: row.get(2)?,
        requested_at_ms: row.get(3)?,
        expires_at_ms: row.get(4)?,
        approved_at_ms: row.get(5)?,
        blocked_at_ms: row.get(6)?,
        display_label: row.get(7)?,
        // Caps re-applied to whatever landed on disk, so a row written by an
        // older or looser build can never smuggle an oversized map back in.
        metadata: BoundedMetadata::sanitized(entries),
    }))
}

fn read_channel_route(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<ChannelRoute, String>> {
    let route_id: String = row.get(0)?;
    let scope_json: String = row.get(1)?;
    let target_json: String = row.get(2)?;
    let scope: Result<RouteScope, serde_json::Error> = serde_json::from_str(&scope_json);
    let target: Result<RouteTarget, serde_json::Error> = serde_json::from_str(&target_json);
    let (scope, target) = match (scope, target) {
        (Ok(scope), Ok(target)) => (scope, target),
        _ => {
            return Ok(Err(format!(
                "channel route '{route_id}' has malformed scope or target JSON"
            )))
        }
    };
    Ok(Ok(ChannelRoute {
        route_id,
        scope,
        target,
        enabled: row.get::<_, i64>(3)? != 0,
        created_at_ms: row.get(4)?,
        updated_at_ms: row.get(5)?,
    }))
}

fn read_channel_event(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<StoredChannelEvent, String>> {
    let event_id: String = row.get(0)?;
    let source_token: String = row.get(2)?;
    let Some(source) = ConversationSource::parse(&source_token) else {
        return Ok(Err(format!(
            "channel event '{event_id}' has unknown source '{source_token}'"
        )));
    };
    let direction_token: String = row.get(3)?;
    let direction = match EventDirection::parse(&direction_token) {
        Ok(direction) => direction,
        Err(error) => return Ok(Err(format!("channel event '{event_id}': {error}"))),
    };
    let disposition_token: String = row.get(9)?;
    let disposition = match EventDisposition::parse(&disposition_token) {
        Ok(disposition) => disposition,
        Err(error) => return Ok(Err(format!("channel event '{event_id}': {error}"))),
    };
    Ok(Ok(StoredChannelEvent {
        event_id,
        account_id: row.get(1)?,
        source,
        direction,
        provider_event_id: row.get(4)?,
        conversation_id: row.get(5)?,
        thread_id: row.get(6)?,
        sender_id: row.get(7)?,
        envelope_json: row.get(8)?,
        disposition,
        ignore_reason: row.get(10)?,
        job_id: row.get(11)?,
        received_at_ms: row.get(12)?,
        ingress_id: row.get(13)?,
    }))
}

// -- The durable bodies, shared by the single-statement methods and by the
//    one transaction that has to hold several of them at once ----------------

/// Insert one event, or report the one already there.
fn insert_channel_event(
    connection: &rusqlite::Connection,
    event: &NewChannelEvent,
) -> Result<EventRecording, String> {
    let event_id = new_event_id();
    let changed = connection
        .execute(
            "INSERT INTO channel_events (
                event_id, account_id, source, direction, provider_event_id, conversation_id,
                thread_id, sender_id, envelope_json, disposition, ignore_reason, job_id,
                received_at_ms, ingress_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, NULL, ?11, NULL)
             ON CONFLICT(source, account_id, direction, provider_event_id) DO NOTHING",
            params![
                event_id,
                event.account_id,
                event.source.as_str(),
                event.direction.as_str(),
                event.provider_event_id,
                event.conversation_id,
                event.thread_id,
                event.sender_id,
                event.envelope_json,
                event.disposition.as_str(),
                event.received_at_ms,
            ],
        )
        .map_err(|error| format!("Failed to record channel event: {error}"))?;
    if changed == 1 {
        return Ok(EventRecording::Recorded { event_id });
    }
    let existing_id: String = connection
        .query_row(
            "SELECT event_id FROM channel_events
             WHERE source=?1 AND account_id=?2 AND direction=?3 AND provider_event_id=?4",
            params![
                event.source.as_str(),
                event.account_id,
                event.direction.as_str(),
                event.provider_event_id,
            ],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    Ok(EventRecording::Duplicate {
        event_id: existing_id,
    })
}

/// Write an event's decision, including the turn it became.
fn finalize_event(
    connection: &rusqlite::Connection,
    event_id: &str,
    disposition: EventDisposition,
    ignore_reason: Option<&str>,
    ingress_id: Option<&str>,
    job_id: Option<&str>,
) -> Result<(), String> {
    // The link and the job id are only ever added, never cleared: a decision
    // that owns no turn must not be able to take one away from a row that has
    // one. Two passes can only ever disagree about an event that was already
    // accepted — a policy edit between a worker and a recovery sweep — and the
    // turn behind it is what recovery needs to find.
    let changed = connection
        .execute(
            "UPDATE channel_events
                SET disposition=?2, ignore_reason=?3,
                    ingress_id=COALESCE(?4, ingress_id), job_id=COALESCE(?5, job_id)
              WHERE event_id=?1",
            params![
                event_id,
                disposition.as_str(),
                ignore_reason,
                ingress_id,
                job_id
            ],
        )
        .map_err(|error| error.to_string())?;
    if changed != 1 {
        return Err(format!("Unknown channel event '{event_id}'"));
    }
    Ok(())
}

/// Bind a conversation to a durable session, never overwriting an existing one.
fn bind_session(
    connection: &rusqlite::Connection,
    session_key: &str,
    account_id: &str,
    conversation_id: &str,
    thread_id: Option<&str>,
    session_id: &str,
    now_ms: i64,
) -> Result<String, String> {
    let existing: Option<String> = connection
        .query_row(
            "SELECT session_id FROM channel_session_map WHERE session_key=?1",
            [session_key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    // Never overwrite: the binding is the source of truth for which durable
    // session a conversation continues, and clobbering it would fork history a
    // caller has already replied into.
    if let Some(existing_session_id) = existing {
        connection
            .execute(
                "UPDATE channel_session_map SET last_used_at_ms=?2 WHERE session_key=?1",
                params![session_key, now_ms],
            )
            .map_err(|error| error.to_string())?;
        return Ok(existing_session_id);
    }
    connection
        .execute(
            "INSERT INTO channel_session_map (
                session_key, account_id, conversation_id, thread_id, session_id,
                created_at_ms, last_used_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                session_key,
                account_id,
                conversation_id,
                thread_id,
                session_id,
                now_ms,
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(session_id.to_string())
}

/// Insert or replace one sender's authorization state.
fn upsert_sender(
    connection: &rusqlite::Connection,
    account_id: &str,
    sender_id: &str,
    record: &StoredSenderAuthorization,
) -> Result<(), String> {
    let metadata_json = serde_json::to_string(&record.metadata.iter().collect::<Vec<_>>())
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "INSERT INTO channel_sender_authorizations (
                account_id, sender_id, state, pairing_code_digest, requested_at_ms,
                expires_at_ms, approved_at_ms, blocked_at_ms, display_label, metadata_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(account_id, sender_id) DO UPDATE SET
                state = excluded.state,
                pairing_code_digest = excluded.pairing_code_digest,
                requested_at_ms = excluded.requested_at_ms,
                expires_at_ms = excluded.expires_at_ms,
                approved_at_ms = excluded.approved_at_ms,
                blocked_at_ms = excluded.blocked_at_ms,
                display_label = excluded.display_label,
                metadata_json = excluded.metadata_json",
            params![
                account_id,
                sender_id,
                record.state.as_str(),
                record.pairing_code_digest,
                record.requested_at_ms,
                record.expires_at_ms,
                record.approved_at_ms,
                record.blocked_at_ms,
                record.display_label,
                metadata_json,
            ],
        )
        .map_err(|error| format!("Failed to upsert channel sender: {error}"))?;
    Ok(())
}

/// Queue one outbound message, or report the one already queued under the
/// identity it carries — its tool invocation, or its account-scoped
/// idempotency key when no invocation asked for it.
fn insert_outbox_message(
    connection: &rusqlite::Connection,
    row: &NewOutboxMessage,
) -> Result<OutboxEnqueue, String> {
    let outbox_id = new_outbox_id();
    let changed = connection
        .execute(
            // `DO NOTHING` with no conflict target on purpose: the row has
            // two durable identities and either may be the one already
            // taken — the account-scoped key, and the invocation, which is
            // unique across all accounts.
            "INSERT INTO channel_outbox (
                outbox_id, account_id, conversation_id, thread_id, reply_to_provider_id,
                state, payload_json, payload_digest, idempotency_key, invocation_id,
                provider_message_id, attempt, max_attempts, next_attempt_at_ms, last_error,
                job_id, created_at_ms, updated_at_ms, sent_at_ms
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?7, ?8, ?9, NULL, 0, ?10, NULL, NULL,
                ?11, ?12, ?12, NULL
             )
             ON CONFLICT DO NOTHING",
            params![
                outbox_id,
                row.account_id,
                row.conversation_id,
                row.thread_id,
                row.reply_to_provider_id,
                row.payload_json,
                row.payload_digest,
                row.idempotency_key,
                row.invocation_id,
                row.max_attempts,
                row.job_id,
                row.created_at_ms,
            ],
        )
        .map_err(|error| format!("Failed to enqueue channel message: {error}"))?;
    if changed == 1 {
        return Ok(OutboxEnqueue::Queued { outbox_id });
    }
    // Found by whichever identity the row carries. An invocation is looked
    // up across every account — that is the point of it, and an
    // account-scoped lookup here would miss the row a replay is colliding
    // with precisely when the replay changed the account, turning the fault
    // below into a bare "no rows".
    let (identity, existing) = match row.invocation_id.as_deref() {
        Some(invocation_id) => (
            format!("invocation '{invocation_id}'"),
            connection.query_row(
                "SELECT outbox_id, payload_digest FROM channel_outbox
                 WHERE invocation_id=?1",
                params![invocation_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ),
        ),
        None => (
            format!("idempotency key '{}'", row.idempotency_key),
            connection.query_row(
                "SELECT outbox_id, payload_digest FROM channel_outbox
                 WHERE account_id=?1 AND idempotency_key=?2",
                params![row.account_id, row.idempotency_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ),
        ),
    };
    let (existing_id, existing_digest): (String, String) =
        existing.map_err(|error| error.to_string())?;
    // The identity names one durable invocation; the digest pins what that
    // invocation asked to send — account and destination included. The same
    // identity carrying different bytes is a consistency fault, not a retry
    // — fail closed rather than overwrite the original or queue a second
    // row, because there is no way to know which version should reach a
    // person.
    if existing_digest != row.payload_digest {
        return Err(format!(
            "Internal consistency error: outbox row {existing_id} already holds \
             {identity} with a different payload digest; refusing to overwrite or \
             duplicate it."
        ));
    }
    Ok(OutboxEnqueue::AlreadyQueued {
        outbox_id: existing_id,
    })
}

/// `daemon_meta` key holding the operator's public base URL for webhook
/// callbacks. A KV row rather than a schema column: one value, daemon-wide.
const CHANNEL_PUBLIC_BASE_URL_KEY: &str = "channels.public_base_url";

/// The listener-relative path one account's webhook deliveries arrive on.
pub fn channel_callback_path(account_id: &str) -> String {
    format!("/v1/channels/{account_id}")
}

/// One acceptable public base: an absolute http(s) URL with a host, no query,
/// no fragment, and no trailing slash so composing with a path never doubles
/// one. A path prefix is allowed — reverse proxies mount things under paths.
fn validate_public_base_url(base: &str) -> Result<String, String> {
    let parsed = url::Url::parse(base.trim())
        .map_err(|error| format!("'{base}' is not a valid URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "The public base URL must use http or https, not '{}'.",
            parsed.scheme()
        ));
    }
    if parsed.host_str().is_none() {
        return Err("The public base URL must include a host.".to_string());
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("The public base URL must not carry a query or fragment.".to_string());
    }
    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

fn read_outbox_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredOutboxMessage> {
    Ok(StoredOutboxMessage {
        outbox_id: row.get(0)?,
        account_id: row.get(1)?,
        conversation_id: row.get(2)?,
        thread_id: row.get(3)?,
        reply_to_provider_id: row.get(4)?,
        state: row.get(5)?,
        payload_json: row.get(6)?,
        payload_digest: row.get(7)?,
        idempotency_key: row.get(8)?,
        provider_message_id: row.get(9)?,
        attempt: row.get::<_, i64>(10)? as u32,
        max_attempts: row.get::<_, i64>(11)? as u32,
        next_attempt_at_ms: row.get(12)?,
        last_error: row.get(13)?,
        job_id: row.get(14)?,
        created_at_ms: row.get(15)?,
        updated_at_ms: row.get(16)?,
        sent_at_ms: row.get(17)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str) -> ChannelAccountRecord {
        ChannelAccountRecord {
            account_id: id.into(),
            kind: ChannelKind::Telegram,
            label: "Test bot".into(),
            enabled: true,
            non_secret_config: serde_json::json!({"bot_username": "test_bot"}),
            credential_ref: Some("keychain:telegram:test".into()),
            access_policy: ChannelAccessPolicy::default(),
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
            .upsert_channel_account(&account("acct-1"))
            .expect("seed account");
        store
    }

    #[test]
    fn account_upsert_is_idempotent_and_preserves_created_at() {
        let mut store = seeded();
        let mut updated = account("acct-1");
        updated.label = "Renamed bot".into();
        updated.created_at_ms = 9_999; // must not stick — created_at is preserved
        updated.updated_at_ms = 2_000;
        store
            .upsert_channel_account(&updated)
            .expect("upsert again");

        let stored = store
            .channel_account("acct-1")
            .expect("query")
            .expect("present");
        assert_eq!(stored.label, "Renamed bot");
        assert_eq!(stored.created_at_ms, 1_000);
        assert_eq!(stored.updated_at_ms, 2_000);

        assert_eq!(store.channel_accounts().expect("list").len(), 1);
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
            .set_channel_account_health("acct-1", &health, 5_500)
            .expect("set health");
        let stored = store
            .channel_account("acct-1")
            .expect("query")
            .expect("present");
        assert_eq!(stored.health, health);
        assert_eq!(stored.updated_at_ms, 5_500);
    }

    #[test]
    fn delete_cascades_to_events_and_outbox() {
        let mut store = seeded();
        store
            .record_channel_event(&new_event("acct-1", "evt-1"))
            .expect("record event");
        store
            .enqueue_channel_message(&new_outbox("acct-1", "idem-1"))
            .expect("enqueue");

        assert!(store.delete_channel_account("acct-1").expect("delete"));
        assert_eq!(
            store
                .recent_channel_events("acct-1", 10)
                .expect("events")
                .len(),
            0
        );
        // The outbox row is gone too — ON DELETE CASCADE on channel_outbox's FK.
        let claimed = store.claim_outbox_batch(10_000, 10).expect("claim");
        assert!(claimed.is_empty());
    }

    fn new_event(account_id: &str, provider_event_id: &str) -> NewChannelEvent {
        NewChannelEvent {
            account_id: account_id.into(),
            source: ConversationSource::MessagingChannel,
            direction: EventDirection::Inbound,
            provider_event_id: provider_event_id.into(),
            conversation_id: "conv-1".into(),
            thread_id: None,
            sender_id: Some("sender-1".into()),
            envelope_json: "{}".into(),
            disposition: EventDisposition::Accepted,
            received_at_ms: 1_000,
        }
    }

    #[test]
    fn duplicate_provider_event_is_deduped() {
        let mut store = seeded();
        let event = new_event("acct-1", "evt-1");
        let first = store.record_channel_event(&event).expect("first insert");
        let EventRecording::Recorded { event_id } = first else {
            panic!("expected Recorded, got {first:?}");
        };
        let second = store.record_channel_event(&event).expect("second insert");
        assert_eq!(second, EventRecording::Duplicate { event_id });
        assert_eq!(
            store
                .recent_channel_events("acct-1", 10)
                .expect("events")
                .len(),
            1
        );
    }

    #[test]
    fn different_source_or_direction_is_not_a_duplicate() {
        let mut store = seeded();
        let base = new_event("acct-1", "evt-1");
        store.record_channel_event(&base).expect("first");

        let mut different_source = base.clone();
        different_source.source = ConversationSource::Peer;
        let by_source = store
            .record_channel_event(&different_source)
            .expect("second");
        assert!(matches!(by_source, EventRecording::Recorded { .. }));

        let mut different_direction = base.clone();
        different_direction.direction = EventDirection::Outbound;
        let by_direction = store
            .record_channel_event(&different_direction)
            .expect("third");
        assert!(matches!(by_direction, EventRecording::Recorded { .. }));

        assert_eq!(
            store
                .recent_channel_events("acct-1", 10)
                .expect("events")
                .len(),
            3
        );
    }

    fn sender(
        id: &str,
        state: SenderState,
        expires_at_ms: Option<i64>,
    ) -> StoredSenderAuthorization {
        StoredSenderAuthorization {
            sender_id: id.into(),
            state,
            pairing_code_digest: Some("digest".into()),
            requested_at_ms: 1_000,
            expires_at_ms,
            approved_at_ms: None,
            blocked_at_ms: None,
            display_label: None,
            metadata: BoundedMetadata::new(),
        }
    }

    #[test]
    fn pending_cap_counts_only_pending_and_expire_only_removes_expired_pending() {
        let mut store = seeded();
        store
            .upsert_channel_sender(
                "acct-1",
                "s1",
                &sender("s1", SenderState::Pending, Some(2_000)),
            )
            .expect("s1");
        store
            .upsert_channel_sender(
                "acct-1",
                "s2",
                &sender("s2", SenderState::Pending, Some(50_000)),
            )
            .expect("s2");
        store
            .upsert_channel_sender("acct-1", "s3", &sender("s3", SenderState::Approved, None))
            .expect("s3");
        store
            .upsert_channel_sender("acct-1", "s4", &sender("s4", SenderState::Blocked, None))
            .expect("s4");

        assert_eq!(
            store
                .count_pending_channel_senders("acct-1")
                .expect("count"),
            2
        );
        assert_eq!(
            store.pending_channel_senders("acct-1").expect("list").len(),
            2
        );

        let removed = store.expire_channel_senders(10_000).expect("expire");
        assert_eq!(removed, 1);
        assert_eq!(
            store
                .count_pending_channel_senders("acct-1")
                .expect("count"),
            1
        );
        assert!(store.channel_sender("acct-1", "s3").expect("s3").is_some());
        assert!(store.channel_sender("acct-1", "s4").expect("s4").is_some());
        assert!(store.channel_sender("acct-1", "s2").expect("s2").is_some());
    }

    fn route(id: &str, scope: RouteScope) -> ChannelRoute {
        ChannelRoute {
            route_id: id.into(),
            scope,
            target: RouteTarget::new("assistant"),
            enabled: true,
            created_at_ms: 1_000,
            updated_at_ms: 1_000,
        }
    }

    #[test]
    fn duplicate_scope_is_rejected() {
        let mut store = seeded();
        store
            .insert_channel_route(&route("r1", RouteScope::account("acct-1")))
            .expect("first route");
        let error = store
            .insert_channel_route(&route("r2", RouteScope::account("acct-1")))
            .unwrap_err();
        assert!(error.contains("r1"), "error should name the owner: {error}");
        assert_eq!(store.channel_routes().expect("routes").len(), 1);
    }

    /// Two routes that would tie are refused even when their stored JSON
    /// differs — the comparison is what a message would match, not what the
    /// bytes look like. A scope on a *different* rung is fine: resolution
    /// picks the more specific one.
    #[test]
    fn same_rung_overlap_is_rejected_and_a_different_rung_is_not() {
        let mut store = seeded();
        store
            .insert_channel_route(&route(
                "r1",
                RouteScope::conversation("acct-1", "C1").with_thread("T1"),
            ))
            .expect("thread route");
        // Same rung, same message: ambiguous.
        let error = store
            .insert_channel_route(&route(
                "r2",
                RouteScope::conversation("acct-1", "C1").with_thread("T1"),
            ))
            .unwrap_err();
        assert!(error.contains("r1"), "{error}");
        // Same conversation, a different thread: no message matches both.
        store
            .insert_channel_route(&route(
                "r3",
                RouteScope::conversation("acct-1", "C1").with_thread("T2"),
            ))
            .expect("other thread");
        // A less specific rung under it: resolution decides, not insertion.
        store
            .insert_channel_route(&route("r4", RouteScope::conversation("acct-1", "C1")))
            .expect("conversation route");
        assert_eq!(store.channel_routes().expect("routes").len(), 3);
    }

    /// Every rung of the declared ladder, stored together and resolved
    /// against one message. The six coexist — different rungs never tie — and
    /// disabling the winner hands the message to the next one down, all the
    /// way to the global default.
    #[test]
    fn the_whole_ladder_stores_and_resolves_in_order() {
        use little_monkey_lib::channels::routing::resolve_route;
        use little_monkey_lib::channels::types::{
            ChannelConversation, ChannelEnvelope, ChannelSender, ConversationKind,
        };

        let mut store = seeded();
        let rungs = [
            (
                "r-sender",
                RouteScope::conversation("acct-1", "C1")
                    .with_thread("T1")
                    .with_sender("U1"),
            ),
            (
                "r-thread",
                RouteScope::conversation("acct-1", "C1").with_thread("T1"),
            ),
            ("r-conversation", RouteScope::conversation("acct-1", "C1")),
            ("r-account", RouteScope::account("acct-1")),
            (
                "r-provider",
                RouteScope::channel_default(ChannelKind::Telegram),
            ),
            ("r-global", RouteScope::global_default()),
        ];
        for (id, scope) in rungs.clone() {
            store
                .insert_channel_route(&route(id, scope))
                .unwrap_or_else(|error| panic!("{id}: {error}"));
        }

        let message = ChannelEnvelope {
            account_id: "acct-1".into(),
            kind: ChannelKind::Telegram,
            provider_event_id: "evt-1".into(),
            conversation: ChannelConversation {
                conversation_id: "C1".into(),
                kind: ConversationKind::Channel,
                thread_id: Some("T1".into()),
                title: None,
            },
            sender: ChannelSender::new("U1"),
            text: "hello".into(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            mentions_self: true,
            received_at_ms: 1_000,
            metadata: Default::default(),
        };

        // Turn the winner off and the next rung down takes it, in order.
        for (id, _) in rungs {
            let routes = store.channel_routes().expect("routes");
            let resolved = resolve_route(&routes, &message).unwrap_or_else(|error| {
                panic!("expected {id} to win, got {error}");
            });
            assert_eq!(resolved.route_id, id);
            store
                .set_channel_route_enabled(id, false, 2_000)
                .expect("disable");
        }
        // With every rung disabled nothing routes — silently dropping the
        // message instead would be the failure this reports.
        assert!(resolve_route(&store.channel_routes().expect("routes"), &message).is_err());
    }

    /// Ambiguity is refused on every rung, not just the one that had a test.
    /// Two routes tie the moment nothing in their scopes separates them, and
    /// the rung they sit on does not change that.
    #[test]
    fn a_second_route_on_any_rung_with_the_same_scope_is_refused() {
        for (index, scope) in [
            RouteScope::global_default(),
            RouteScope::channel_default(ChannelKind::Telegram),
            RouteScope::account("acct-1"),
            RouteScope::conversation("acct-1", "C1"),
            RouteScope::conversation("acct-1", "C1").with_thread("T1"),
            RouteScope::conversation("acct-1", "C1")
                .with_thread("T1")
                .with_sender("U1"),
        ]
        .into_iter()
        .enumerate()
        {
            let mut store = seeded();
            store
                .insert_channel_route(&route("first", scope.clone()))
                .unwrap_or_else(|error| panic!("rung {index}: {error}"));
            let error = store
                .insert_channel_route(&route("second", scope.clone()))
                .unwrap_err();
            assert!(error.contains("first"), "rung {index}: {error}");
            // And an edit into the same conflict fails exactly like insertion.
            store
                .insert_channel_route(&route("other", RouteScope::account("acct-2")))
                .expect("a route on a different scope");
            let error = store
                .update_channel_route(&route("other", scope))
                .unwrap_err();
            assert!(error.contains("first"), "rung {index}: {error}");
            assert_eq!(store.channel_routes().expect("routes").len(), 2);
        }
    }

    /// The rung the daemon declares is `account + conversation + thread +
    /// sender`; a sender route without a thread is not a narrower rung, it is
    /// a route that would match that sender in every thread of the
    /// conversation while claiming to be the most specific one there is.
    #[test]
    fn a_sender_scope_with_no_thread_never_reaches_the_table() {
        let mut store = seeded();
        let error = store
            .insert_channel_route(&route(
                "r1",
                RouteScope::conversation("acct-1", "C1").with_sender("U1"),
            ))
            .unwrap_err();
        assert!(error.contains("thread"), "{error}");
        assert!(store.channel_routes().expect("routes").is_empty());

        // Nor by editing a legal route into one.
        store
            .insert_channel_route(&route(
                "r1",
                RouteScope::conversation("acct-1", "C1")
                    .with_thread("T1")
                    .with_sender("U1"),
            ))
            .expect("the legal sender rung");
        assert!(store
            .update_channel_route(&route(
                "r1",
                RouteScope::conversation("acct-1", "C1").with_sender("U1"),
            ))
            .is_err());
        assert_eq!(
            store.channel_routes().expect("routes")[0]
                .scope
                .thread_id
                .as_deref(),
            Some("T1")
        );
    }

    #[test]
    fn a_route_is_edited_in_place_and_can_be_turned_off() {
        let mut store = seeded();
        store
            .insert_channel_route(&route("r1", RouteScope::account("acct-1")))
            .expect("route");

        let mut edited = route("r1", RouteScope::conversation("acct-1", "C1"));
        edited.target = RouteTarget::new("triage");
        edited.updated_at_ms = 2_000;
        store.update_channel_route(&edited).expect("update");

        let stored = &store.channel_routes().expect("routes")[0];
        assert_eq!(stored.route_id, "r1");
        assert_eq!(stored.target.recipe, "triage");
        assert_eq!(stored.scope.conversation_id.as_deref(), Some("C1"));
        // Identity survives the edit; only what it routes changed.
        assert_eq!(stored.created_at_ms, 1_000);

        assert!(store
            .set_channel_route_enabled("r1", false, 3_000)
            .expect("disable"));
        assert!(!store.channel_routes().expect("routes")[0].enabled);
        // A route that does not exist is reported rather than silently ignored.
        assert!(!store
            .set_channel_route_enabled("nope", false, 3_000)
            .expect("missing route"));
    }

    #[test]
    fn an_edit_that_would_tie_with_another_route_is_refused() {
        let mut store = seeded();
        store
            .insert_channel_route(&route("r1", RouteScope::account("acct-1")))
            .expect("route");
        store
            .insert_channel_route(&route("r2", RouteScope::conversation("acct-1", "C1")))
            .expect("route");

        // Moving r2 onto r1's rung would make both match the same message.
        let error = store
            .update_channel_route(&route("r2", RouteScope::account("acct-1")))
            .unwrap_err();
        assert!(error.contains("r1"), "{error}");
        // Editing a route without moving it is not a conflict with itself.
        let mut same_scope = route("r2", RouteScope::conversation("acct-1", "C1"));
        same_scope.target = RouteTarget::new("triage");
        store.update_channel_route(&same_scope).expect("self-edit");
    }

    #[test]
    fn a_callback_url_is_composed_only_from_the_configured_public_base() {
        let mut store = seeded();
        // Nothing configured: the path is known, the URL is not, and the store
        // says so rather than guessing a host.
        assert_eq!(store.channel_public_base_url().expect("base"), None);
        assert_eq!(store.channel_callback_url("acct-1").expect("url"), None);
        assert_eq!(channel_callback_path("acct-1"), "/v1/channels/acct-1");

        store
            .set_channel_public_base_url(Some("https://hooks.example.com/"))
            .expect("set base");
        assert_eq!(
            store.channel_callback_url("acct-1").expect("url"),
            Some("https://hooks.example.com/v1/channels/acct-1".to_string())
        );

        // A proxy mounting the daemon under a path prefix is normal.
        store
            .set_channel_public_base_url(Some("https://example.com/monkey"))
            .expect("set base");
        assert_eq!(
            store.channel_callback_url("acct-1").expect("url"),
            Some("https://example.com/monkey/v1/channels/acct-1".to_string())
        );

        store.set_channel_public_base_url(None).expect("clear");
        assert_eq!(store.channel_callback_url("acct-1").expect("url"), None);
    }

    #[test]
    fn a_public_base_that_could_not_receive_a_callback_is_refused() {
        let mut store = seeded();
        for bad in [
            "not a url",
            "ftp://example.com",
            "https://",
            "https://example.com?token=abc",
            "https://example.com#fragment",
        ] {
            assert!(
                store.set_channel_public_base_url(Some(bad)).is_err(),
                "accepted {bad}"
            );
        }
    }

    #[test]
    fn invalid_scope_is_rejected_by_validate() {
        let mut store = seeded();
        let orphan = RouteScope {
            conversation_id: Some("C1".into()),
            ..RouteScope::default()
        };
        assert!(store.insert_channel_route(&route("r1", orphan)).is_err());
        assert_eq!(store.channel_routes().expect("routes").len(), 0);
    }

    #[test]
    fn bind_channel_session_never_rebinds() {
        let mut store = seeded();
        let first = store
            .bind_channel_session("key-1", "acct-1", "conv-1", None, "sess-A", 1_000)
            .expect("first bind");
        assert_eq!(first, "sess-A");

        let second = store
            .bind_channel_session("key-1", "acct-1", "conv-1", None, "sess-B", 2_000)
            .expect("second bind");
        assert_eq!(second, "sess-A", "must not rebind to sess-B");
        assert_eq!(
            store.channel_session("key-1").expect("lookup"),
            Some("sess-A".to_string())
        );
    }

    fn new_outbox(account_id: &str, idempotency_key: &str) -> NewOutboxMessage {
        NewOutboxMessage {
            account_id: account_id.into(),
            conversation_id: "conv-1".into(),
            thread_id: None,
            reply_to_provider_id: None,
            payload_json: "{}".into(),
            payload_digest: "digest".into(),
            idempotency_key: idempotency_key.into(),
            invocation_id: None,
            max_attempts: 3,
            job_id: None,
            created_at_ms: 1_000,
        }
    }

    #[test]
    fn enqueue_is_idempotent_on_idempotency_key() {
        let mut store = seeded();
        let row = new_outbox("acct-1", "idem-1");
        let first = store.enqueue_channel_message(&row).expect("first");
        let OutboxEnqueue::Queued { outbox_id } = first else {
            panic!("expected Queued, got {first:?}");
        };
        let second = store.enqueue_channel_message(&row).expect("second");
        assert_eq!(second, OutboxEnqueue::AlreadyQueued { outbox_id });
    }

    /// The unique constraint names one durable invocation; the digest pins
    /// what it asked to send. The same key with different bytes must neither
    /// overwrite the original nor queue a second row.
    #[test]
    fn enqueue_fails_closed_when_the_same_key_carries_a_different_payload() {
        let mut store = seeded();
        let row = new_outbox("acct-1", "idem-1");
        store.enqueue_channel_message(&row).expect("first");

        let mut changed = new_outbox("acct-1", "idem-1");
        changed.payload_json = r#"{"text":"different"}"#.into();
        changed.payload_digest = "another-digest".into();
        let error = store
            .enqueue_channel_message(&changed)
            .expect_err("a changed payload under the same key");
        assert!(error.contains("consistency"), "{error}");

        // The original row is untouched and still the only one.
        let claimed = store.claim_outbox_batch(1_000, 10).expect("claim");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].payload_json, "{}");
    }

    /// One invocation, one row — across every account.
    ///
    /// The table's own `UNIQUE(account_id, idempotency_key)` cannot say this:
    /// the same key on a second account is a different pair and inserts
    /// happily. The invocation index is what makes the second enqueue collide
    /// at all, and the digest then refuses it rather than overwriting the
    /// first. Enforced by the database inside the transaction, not by a
    /// read-then-write check that two daemons could interleave.
    #[test]
    fn an_invocation_is_unique_across_accounts_not_within_one() {
        let mut store = seeded();
        store
            .upsert_channel_account(&account("acct-2"))
            .expect("second account");

        let mut first = new_outbox("acct-1", "channel-send:job-1:tool-1-2");
        first.invocation_id = Some("channel-send:job-1:tool-1-2".into());
        store.enqueue_channel_message(&first).expect("first");

        // Same invocation, another account, and a payload that says so.
        let mut elsewhere = new_outbox("acct-2", "channel-send:job-1:tool-1-2");
        elsewhere.invocation_id = Some("channel-send:job-1:tool-1-2".into());
        elsewhere.payload_json = r#"{"account":"acct-2"}"#.into();
        elsewhere.payload_digest = "another-digest".into();
        let error = store
            .enqueue_channel_message(&elsewhere)
            .expect_err("the same invocation on another account");
        assert!(error.contains("consistency"), "{error}");

        // Byte-identical on the other account is the same invocation being
        // replayed, not a second message: it finds the first row.
        let mut replay = new_outbox("acct-2", "channel-send:job-1:tool-1-2");
        replay.invocation_id = Some("channel-send:job-1:tool-1-2".into());
        let again = store.enqueue_channel_message(&replay).expect("replay");
        assert!(matches!(again, OutboxEnqueue::AlreadyQueued { .. }));

        let claimed = store.claim_outbox_batch(1_000, 10).expect("claim");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].account_id, "acct-1");
    }

    /// A send with no invocation behind it keeps the account-scoped identity
    /// it has always had: an inbound auto-reply on two accounts is two
    /// replies, and nothing about this change may collapse them.
    #[test]
    fn an_account_scoped_key_without_an_invocation_still_belongs_to_its_account() {
        let mut store = seeded();
        store
            .upsert_channel_account(&account("acct-2"))
            .expect("second account");

        store
            .enqueue_channel_message(&new_outbox("acct-1", "reply:msg-42"))
            .expect("first account");
        store
            .enqueue_channel_message(&new_outbox("acct-2", "reply:msg-42"))
            .expect("second account");

        assert_eq!(store.claim_outbox_batch(1_000, 10).expect("claim").len(), 2);
    }

    #[test]
    fn claim_marks_sending_and_does_not_double_claim() {
        let mut store = seeded();
        store
            .enqueue_channel_message(&new_outbox("acct-1", "idem-1"))
            .expect("enqueue");

        let first_batch = store.claim_outbox_batch(1_000, 10).expect("claim 1");
        assert_eq!(first_batch.len(), 1);
        assert_eq!(first_batch[0].state, "sending");
        assert_eq!(first_batch[0].attempt, 1);

        let second_batch = store.claim_outbox_batch(1_000, 10).expect("claim 2");
        assert!(
            second_batch.is_empty(),
            "already-claimed row must not reappear"
        );
    }

    #[test]
    fn retryable_failure_reschedules_and_is_reclaimed_after_backoff() {
        let mut store = seeded();
        store
            .enqueue_channel_message(&new_outbox("acct-1", "idem-1"))
            .expect("enqueue");
        let claimed = store.claim_outbox_batch(1_000, 10).expect("claim");
        let outbox_id = claimed[0].outbox_id.clone();

        store
            .complete_outbox_send(
                &outbox_id,
                &SendOutcome::RetryableFailure {
                    error: "timeout".into(),
                    retry_after_ms: None,
                },
                1_000,
            )
            .expect("complete");

        // First attempt's backoff is 30s; not yet claimable at +1s.
        let too_soon = store.claim_outbox_batch(1_001, 10).expect("too soon");
        assert!(too_soon.is_empty());

        let after_backoff = store
            .claim_outbox_batch(1_000 + 30_000, 10)
            .expect("after backoff");
        assert_eq!(after_backoff.len(), 1);
        assert_eq!(after_backoff[0].attempt, 2);
    }

    #[test]
    fn max_attempts_reached_marks_failed() {
        let mut store = seeded();
        let mut row = new_outbox("acct-1", "idem-1");
        row.max_attempts = 1;
        store.enqueue_channel_message(&row).expect("enqueue");
        let claimed = store.claim_outbox_batch(1_000, 10).expect("claim");
        let outbox_id = claimed[0].outbox_id.clone();
        assert_eq!(claimed[0].attempt, 1);

        store
            .complete_outbox_send(
                &outbox_id,
                &SendOutcome::RetryableFailure {
                    error: "boom".into(),
                    retry_after_ms: None,
                },
                1_000,
            )
            .expect("complete");

        let never_reclaimed = store.claim_outbox_batch(999_999_999, 10).expect("claim");
        assert!(never_reclaimed.is_empty());
    }

    #[test]
    fn needs_reconciliation_is_never_reclaimed() {
        let mut store = seeded();
        store
            .enqueue_channel_message(&new_outbox("acct-1", "idem-1"))
            .expect("enqueue");
        let claimed = store.claim_outbox_batch(1_000, 10).expect("claim");
        let outbox_id = claimed[0].outbox_id.clone();

        store
            .complete_outbox_send(
                &outbox_id,
                &SendOutcome::NeedsReconciliation {
                    error: "unknown".into(),
                },
                1_000,
            )
            .expect("complete");

        let never_reclaimed = store.claim_outbox_batch(999_999_999, 10).expect("claim");
        assert!(never_reclaimed.is_empty());
    }

    #[test]
    fn requeue_stuck_sending_moves_to_needs_reconciliation() {
        let mut store = seeded();
        store
            .enqueue_channel_message(&new_outbox("acct-1", "idem-1"))
            .expect("enqueue");
        store.claim_outbox_batch(1_000, 10).expect("claim");

        let moved = store.requeue_stuck_sending(2_000).expect("requeue");
        assert_eq!(moved, 1);

        let never_reclaimed = store.claim_outbox_batch(999_999_999, 10).expect("claim");
        assert!(never_reclaimed.is_empty());
    }

    #[test]
    fn cursor_round_trips_and_rejects_oversize() {
        let mut store = seeded();
        assert_eq!(store.channel_cursor("acct-1", "offset").expect("get"), None);
        store
            .set_channel_cursor("acct-1", "offset", "12345", 1_000)
            .expect("set");
        assert_eq!(
            store.channel_cursor("acct-1", "offset").expect("get"),
            Some("12345".to_string())
        );

        let oversize = "x".repeat(5_000);
        let error = store
            .set_channel_cursor("acct-1", "offset", &oversize, 2_000)
            .unwrap_err();
        assert!(error.contains("4096"));
        // Rejected write must not clobber the prior value.
        assert_eq!(
            store.channel_cursor("acct-1", "offset").expect("get"),
            Some("12345".to_string())
        );
    }

    #[test]
    fn delete_channel_route_removes_only_that_route() {
        let mut store = seeded();
        store
            .insert_channel_route(&route("r1", RouteScope::account("acct-1")))
            .expect("insert");
        assert!(!store
            .delete_channel_route("missing")
            .expect("delete missing"));
        assert!(store.delete_channel_route("r1").expect("delete r1"));
        assert_eq!(store.channel_routes().expect("routes").len(), 0);
    }

    #[test]
    fn event_disposition_updates_in_place() {
        let mut store = seeded();
        let recorded = store
            .record_channel_event(&new_event("acct-1", "evt-1"))
            .expect("record");
        let EventRecording::Recorded { event_id } = recorded else {
            panic!("expected Recorded, got {recorded:?}");
        };
        store
            .set_channel_event_disposition(
                &event_id,
                EventDisposition::Ignored,
                Some("not_mentioned"),
                None,
            )
            .expect("set disposition");
        let events = store.recent_channel_events("acct-1", 10).expect("events");
        assert_eq!(events[0].disposition, EventDisposition::Ignored);
        assert_eq!(events[0].ignore_reason.as_deref(), Some("not_mentioned"));
    }

    #[test]
    fn cancel_outbox_message_only_works_from_queued() {
        let mut store = seeded();
        store
            .enqueue_channel_message(&new_outbox("acct-1", "idem-1"))
            .expect("enqueue");
        let claimed = store.claim_outbox_batch(1_000, 10).expect("claim");
        let outbox_id = claimed[0].outbox_id.clone();

        // Already 'sending', not 'queued' — cancel must refuse.
        assert!(!store
            .cancel_outbox_message(&outbox_id, 2_000)
            .expect("cancel sending"));

        let second = new_outbox("acct-1", "idem-2");
        let OutboxEnqueue::Queued {
            outbox_id: queued_id,
        } = store.enqueue_channel_message(&second).expect("enqueue 2")
        else {
            panic!("expected Queued");
        };
        assert!(store
            .cancel_outbox_message(&queued_id, 2_000)
            .expect("cancel queued"));
    }
}
