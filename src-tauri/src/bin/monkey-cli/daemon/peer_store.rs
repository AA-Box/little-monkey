//! Durable storage for peer threads and the messages in them. Owns the
//! `peer_*` tables created by `DAEMON_V9_SQL` and reshaped by `DAEMON_V16_SQL`
//! in `store.rs`.
//!
//! # Identity is the pairing, not the envelope
//!
//! Every key here starts with `peer_device_id` — the authenticated pairing the
//! signature resolved to. A thread id and a message id are the peer's own
//! words, so two peers may legitimately use the same ones; scoping to the
//! pairing is what stops one peer landing in another's conversation or
//! collapsing another's message onto its own dedupe row.
//!
//! # Recording before deciding
//!
//! A message is written before it is acted on, and a *rejected* message is
//! written too. That ordering is what makes a redelivery cheap and safe: the
//! second copy collapses onto the row that is already here instead of being
//! re-judged, so a peer cannot turn a retry into a second run, and cannot turn
//! a rejection into a retry loop either.
//!
//! # No secrets
//!
//! Nothing here holds a pairing secret. The peer's identity, its secret
//! generation and its capability grant live in the remote store with every
//! other pairing; these rows only reference the device id.

use rusqlite::{params, OptionalExtension, TransactionBehavior};

use little_monkey_lib::peers::{PeerEnvelope, PeerMessageKind, PeerRejection};

use super::store::DaemonStore;

/// Which way a peer message crossed the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerDirection {
    Inbound,
    Outbound,
}

impl PeerDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            PeerDirection::Inbound => "inbound",
            PeerDirection::Outbound => "outbound",
        }
    }

    pub fn parse(value: &str) -> Result<PeerDirection, String> {
        match value {
            "inbound" => Ok(PeerDirection::Inbound),
            "outbound" => Ok(PeerDirection::Outbound),
            other => Err(format!("unknown peer message direction '{other}'")),
        }
    }
}

/// What happened to a peer message once the gates ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerDisposition {
    /// Taken, and — for a message or a task request — turned into a turn.
    Accepted,
    /// Refused. The reason is in `rejection`.
    Rejected,
    /// A row this node wrote and made available to the peer (a result).
    Delivered,
}

impl PeerDisposition {
    pub fn as_str(self) -> &'static str {
        match self {
            PeerDisposition::Accepted => "accepted",
            PeerDisposition::Rejected => "rejected",
            PeerDisposition::Delivered => "delivered",
        }
    }

    pub fn parse(value: &str) -> Result<PeerDisposition, String> {
        match value {
            "accepted" => Ok(PeerDisposition::Accepted),
            "rejected" => Ok(PeerDisposition::Rejected),
            "delivered" => Ok(PeerDisposition::Delivered),
            other => Err(format!("unknown peer message disposition '{other}'")),
        }
    }
}

/// A stored peer conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerThreadRecord {
    pub thread_id: String,
    pub peer_device_id: String,
    pub peer_instance_id: String,
    /// Durable conversation session every turn in this thread continues.
    pub session_key: String,
    pub created_at_ms: i64,
    pub last_activity_at_ms: i64,
}

/// One stored peer message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerMessageRecord {
    pub row_id: String,
    pub thread_id: String,
    pub peer_device_id: String,
    pub sender_instance_id: String,
    pub message_id: String,
    pub direction: PeerDirection,
    /// `message`, `task_request`, `artifact`, or `result` for a row this node
    /// wrote about a finished run.
    pub kind: String,
    pub correlation_id: Option<String>,
    pub disposition: PeerDisposition,
    pub rejection: Option<String>,
    pub envelope_json: String,
    pub ingress_id: Option<String>,
    pub job_id: Option<String>,
    pub created_at_ms: i64,
}

/// A refusal that never became a message row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRejectionEvent {
    pub peer_device_id: String,
    pub reason: String,
    pub occurred_at_ms: i64,
}

/// Rejection events kept per pairing before the oldest are dropped.
///
/// The bound is per peer rather than global on purpose: an envelope that fails
/// validation costs its sender nothing to retry, so one broken or hostile peer
/// could otherwise push every other peer's evidence out of the table and take
/// Security Doctor's answer about *them* with it. Two hundred is far more than
/// a finding needs — the doctor reads a few hundred at most and only counts
/// them — and it bounds the table at pairings × 200 rows, which an operator's
/// own pairing list bounds in turn.
pub const MAX_PEER_REJECTION_EVENTS_PER_PEER: u32 = 200;

/// Rejection events kept across every pairing.
///
/// The per-peer bound alone leaves the table's real size to the pairing count,
/// which is an operator's decision rather than a bound this code enforces. This
/// is the one the table itself has: whatever the pairing list does, the oldest
/// refusals beyond this many are dropped. Set well above per-peer × a plausible
/// pairing list, so it is the backstop and the per-peer bound stays the rule
/// that decides whose evidence is kept.
pub const MAX_PEER_REJECTION_EVENTS_TOTAL: u32 = 5_000;

/// Artifact admissions kept per pairing, and across all of them.
///
/// Expiry alone is not retention: an admission stops *authorizing* anything
/// after [`PEER_ARTIFACT_ADMISSION_TTL_MS`], but the row outlives it, so a peer
/// uploading unique content in a loop grows the table for as long as it cares
/// to. Expired rows are pruned on the next upload and these bound what is left,
/// which is at most one pairing's working set of live admissions.
pub const MAX_PEER_ARTIFACT_RECEIPTS_PER_PEER: u32 = 500;
pub const MAX_PEER_ARTIFACT_RECEIPTS_TOTAL: u32 = 5_000;

/// How long an uploaded artifact stays referenceable by the peer that uploaded
/// it.
///
/// Handing bytes over once must not buy permanent standing to name them. The
/// bound is the envelope's own maximum life: an envelope may claim at most
/// [`MAX_TTL_MS`] of validity, so an admission that outlived that could only
/// ever authorize an envelope which was itself already expired.
///
/// [`MAX_TTL_MS`]: little_monkey_lib::peers::MAX_TTL_MS
pub const PEER_ARTIFACT_ADMISSION_TTL_MS: i64 = little_monkey_lib::peers::MAX_TTL_MS;

/// Proof that one authenticated pairing handed this installation one blob.
///
/// The authorization record for a later artifact reference, and the source of
/// the attachment metadata that reference produces. Both matter: the digest in
/// an envelope proves integrity and nothing about who may name it, and the
/// filename in an envelope is the sender's second chance to describe bytes it
/// already described once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerArtifactReceipt {
    pub peer_device_id: String,
    pub artifact_id: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub filename: Option<String>,
    pub media_type: Option<String>,
    pub uploaded_at_ms: i64,
    pub expires_at_ms: i64,
}

/// One thing this installation sent to a peer, and the last state a poll saw.
///
/// Kept because the far side offers no way to enumerate threads and should not:
/// the only installation that legitimately knows which threads exist is the one
/// that opened them, which is this one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerOutboundMessage {
    pub alias: String,
    pub message_id: String,
    pub thread_id: String,
    pub correlation_id: Option<String>,
    pub kind: String,
    /// Last known state: what the send returned, or what a later poll of the
    /// peer's own thread reported.
    pub state: String,
    pub result_text: Option<String>,
    pub sent_at_ms: i64,
    pub checked_at_ms: Option<i64>,
}

/// What recording a message did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerRecording {
    Recorded {
        row_id: String,
    },
    /// The peer sent this one before. Carries what was decided then, so the
    /// caller answers a retry with the original outcome.
    Duplicate {
        row_id: String,
        disposition: PeerDisposition,
        job_id: Option<String>,
    },
}

impl DaemonStore {
    /// Ensure a thread exists and stamp it as active. Returns the thread.
    ///
    /// The session key is set once, when the thread is created: a peer that
    /// keeps talking in the same thread keeps the same durable conversation,
    /// and a peer cannot move an existing thread onto another session by
    /// asking again.
    pub fn upsert_peer_thread(
        &mut self,
        thread_id: &str,
        peer_device_id: &str,
        peer_instance_id: &str,
        session_key: &str,
        now_ms: i64,
    ) -> Result<PeerThreadRecord, String> {
        let now_ms = now_ms.max(1);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT INTO peer_threads (
                    peer_device_id, thread_id, peer_instance_id, session_key,
                    created_at_ms, last_activity_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                 ON CONFLICT(peer_device_id, thread_id)
                 DO UPDATE SET last_activity_at_ms=excluded.last_activity_at_ms",
                params![
                    peer_device_id,
                    thread_id,
                    peer_instance_id,
                    session_key,
                    now_ms
                ],
            )
            .map_err(|error| format!("Failed to record the peer thread: {error}"))?;
        let thread = transaction
            .query_row(
                "SELECT thread_id, peer_device_id, peer_instance_id, session_key,
                        created_at_ms, last_activity_at_ms
                 FROM peer_threads WHERE peer_device_id=?1 AND thread_id=?2",
                params![peer_device_id, thread_id],
                read_thread,
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(thread)
    }

    /// Record a refusal that happened before a thread could exist.
    ///
    /// Shape, hop, loop and expiry rules run *before* anything is written, so
    /// those refusals leave no message row by design — a peer must not be able
    /// to fill this database with junk that never became anything. This bounded
    /// table is what Security Doctor reads instead: a pairing, a reason and a
    /// time, with no peer text and no unvalidated identifier.
    ///
    /// # Bounded means bounded on disk
    ///
    /// The insert and the prune are one transaction, so the table never holds
    /// more than [`MAX_PEER_REJECTION_EVENTS_PER_PEER`] rows for any pairing
    /// however hard that pairing tries. Reading with a `LIMIT` is not
    /// retention: a peer sending a fresh, correctly signed, malformed envelope
    /// in a loop is the exact case this has to survive, and before the prune it
    /// would have grown SQLite without end while every read still looked tidy.
    pub fn record_peer_rejection_event(
        &mut self,
        peer_device_id: &str,
        message_id: Option<&str>,
        thread_id: Option<&str>,
        reason: PeerRejection,
        now_ms: i64,
    ) -> Result<(), String> {
        // The identifiers are the peer's own and may be anything at all — this
        // refusal is often *because* they were malformed. Only well-formed ones
        // are kept; the rest become NULL rather than failing the insert or
        // storing an unbounded string.
        fn bounded<'a>(value: Option<&'a str>) -> Option<&'a str> {
            value.filter(|candidate| {
                !candidate.is_empty()
                    && candidate.len() <= 128
                    && candidate
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
            })
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT INTO peer_rejection_events (
                    event_id, peer_device_id, message_id, thread_id, reason, occurred_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    format!("prej-{}", uuid::Uuid::new_v4().simple()),
                    peer_device_id,
                    bounded(message_id),
                    bounded(thread_id),
                    reason.as_str(),
                    now_ms.max(1),
                ],
            )
            .map_err(|error| format!("Failed to record the peer refusal: {error}"))?;
        transaction
            .execute(
                "DELETE FROM peer_rejection_events
                  WHERE peer_device_id = ?1
                    AND event_id NOT IN (
                        SELECT event_id FROM peer_rejection_events
                         WHERE peer_device_id = ?1
                         ORDER BY occurred_at_ms DESC, event_id DESC
                         LIMIT ?2
                    )",
                params![peer_device_id, MAX_PEER_REJECTION_EVENTS_PER_PEER],
            )
            .map_err(|error| format!("Failed to bound the peer refusal table: {error}"))?;
        // And the bound the table has regardless of how many pairings exist.
        transaction
            .execute(
                "DELETE FROM peer_rejection_events
                  WHERE event_id NOT IN (
                        SELECT event_id FROM peer_rejection_events
                         ORDER BY occurred_at_ms DESC, event_id DESC
                         LIMIT ?1
                    )",
                params![MAX_PEER_REJECTION_EVENTS_TOTAL],
            )
            .map_err(|error| format!("Failed to bound the peer refusal table: {error}"))?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(())
    }

    /// How many refusals are stored for one pairing. The bound, observable.
    pub fn peer_rejection_event_count(&self, peer_device_id: Option<&str>) -> Result<u32, String> {
        self.connection
            .query_row(
                "SELECT COUNT(*) FROM peer_rejection_events
                  WHERE (?1 IS NULL OR peer_device_id = ?1)",
                params![peer_device_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| u32::try_from(count).unwrap_or(u32::MAX))
            .map_err(|error| error.to_string())
    }

    /// Recent pre-thread refusals, newest first. Bounded by the caller.
    pub fn peer_rejection_events(&self, limit: u32) -> Result<Vec<PeerRejectionEvent>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT peer_device_id, reason, occurred_at_ms
                 FROM peer_rejection_events
                 ORDER BY occurred_at_ms DESC, event_id DESC LIMIT ?1",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([i64::from(limit)], |row| {
                Ok(PeerRejectionEvent {
                    peer_device_id: row.get(0)?,
                    reason: row.get(1)?,
                    occurred_at_ms: row.get(2)?,
                })
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    /// Record one inbound envelope, before anything decides what to do with it.
    pub fn record_peer_message(
        &mut self,
        thread_id: &str,
        peer_device_id: &str,
        envelope: &PeerEnvelope,
        now_ms: i64,
    ) -> Result<PeerRecording, String> {
        let envelope_json = serde_json::to_string(envelope).map_err(|error| error.to_string())?;
        self.insert_peer_message(
            thread_id,
            peer_device_id,
            &envelope.sender_instance_id,
            &envelope.message_id,
            PeerDirection::Inbound,
            envelope.kind.as_str(),
            envelope.correlation_id.as_deref(),
            PeerDisposition::Accepted,
            None,
            &envelope_json,
            now_ms,
        )
    }

    /// Record a result this node produced for a peer's task request.
    ///
    /// The message id is the caller's and is derived from the job, so
    /// materializing the same finished run twice writes one row.
    #[allow(clippy::too_many_arguments)]
    pub fn record_peer_result(
        &mut self,
        thread_id: &str,
        peer_device_id: &str,
        local_instance_id: &str,
        message_id: &str,
        correlation_id: Option<&str>,
        job_id: &str,
        payload_json: &str,
        now_ms: i64,
    ) -> Result<PeerRecording, String> {
        let recording = self.insert_peer_message(
            thread_id,
            peer_device_id,
            local_instance_id,
            message_id,
            PeerDirection::Outbound,
            "result",
            correlation_id,
            PeerDisposition::Delivered,
            None,
            payload_json,
            now_ms,
        )?;
        if let PeerRecording::Recorded { row_id } = &recording {
            self.connection
                .execute(
                    "UPDATE peer_messages SET job_id=?2 WHERE row_id=?1",
                    params![row_id, job_id],
                )
                .map_err(|error| error.to_string())?;
        }
        Ok(recording)
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_peer_message(
        &mut self,
        thread_id: &str,
        peer_device_id: &str,
        sender_instance_id: &str,
        message_id: &str,
        direction: PeerDirection,
        kind: &str,
        correlation_id: Option<&str>,
        disposition: PeerDisposition,
        rejection: Option<&str>,
        envelope_json: &str,
        now_ms: i64,
    ) -> Result<PeerRecording, String> {
        let row_id = format!("pmsg-{}", uuid::Uuid::new_v4().simple());
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let changed = transaction
            .execute(
                "INSERT INTO peer_messages (
                    row_id, thread_id, peer_device_id, sender_instance_id, message_id,
                    direction, kind, correlation_id, disposition, rejection, envelope_json,
                    ingress_id, job_id, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, NULL, ?12)
                 ON CONFLICT(peer_device_id, message_id, direction) DO NOTHING",
                params![
                    row_id,
                    thread_id,
                    peer_device_id,
                    sender_instance_id,
                    message_id,
                    direction.as_str(),
                    kind,
                    correlation_id,
                    disposition.as_str(),
                    rejection,
                    envelope_json,
                    now_ms.max(1),
                ],
            )
            .map_err(|error| format!("Failed to record the peer message: {error}"))?;
        let recording = if changed == 1 {
            PeerRecording::Recorded { row_id }
        } else {
            let (row_id, disposition, job_id): (String, String, Option<String>) = transaction
                .query_row(
                    "SELECT row_id, disposition, job_id FROM peer_messages
                     WHERE peer_device_id=?1 AND message_id=?2 AND direction=?3",
                    params![peer_device_id, message_id, direction.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .map_err(|error| error.to_string())?;
            PeerRecording::Duplicate {
                row_id,
                disposition: PeerDisposition::parse(&disposition)?,
                job_id,
            }
        };
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(recording)
    }

    /// Mark a recorded message as refused, with the reason the peer was told.
    pub fn reject_peer_message(
        &mut self,
        row_id: &str,
        rejection: PeerRejection,
    ) -> Result<(), String> {
        let changed = self
            .connection
            .execute(
                "UPDATE peer_messages SET disposition='rejected', rejection=?2 WHERE row_id=?1",
                params![row_id, rejection.as_str()],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err(format!("Unknown peer message '{row_id}'"));
        }
        Ok(())
    }

    /// Attach the durable turn and the job it produced to a recorded message.
    pub fn attach_peer_message_run(
        &mut self,
        row_id: &str,
        ingress_id: Option<&str>,
        job_id: Option<&str>,
    ) -> Result<(), String> {
        let changed = self
            .connection
            .execute(
                "UPDATE peer_messages SET ingress_id=?2, job_id=?3 WHERE row_id=?1",
                params![row_id, ingress_id, job_id],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err(format!("Unknown peer message '{row_id}'"));
        }
        Ok(())
    }

    /// One thread, named by the pairing that owns it.
    ///
    /// The device id is not a filter that could be dropped for convenience:
    /// without it a peer could read another peer's thread by guessing its id.
    pub fn peer_thread(
        &self,
        peer_device_id: &str,
        thread_id: &str,
    ) -> Result<Option<PeerThreadRecord>, String> {
        self.connection
            .query_row(
                "SELECT thread_id, peer_device_id, peer_instance_id, session_key,
                        created_at_ms, last_activity_at_ms
                 FROM peer_threads WHERE peer_device_id=?1 AND thread_id=?2",
                params![peer_device_id, thread_id],
                read_thread,
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    /// Threads for one peer, or for every peer when `peer_device_id` is absent.
    pub fn peer_threads(
        &self,
        peer_device_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<PeerThreadRecord>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT thread_id, peer_device_id, peer_instance_id, session_key,
                        created_at_ms, last_activity_at_ms
                 FROM peer_threads
                 WHERE (?1 IS NULL OR peer_device_id = ?1)
                 ORDER BY last_activity_at_ms DESC LIMIT ?2",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(params![peer_device_id, i64::from(limit)], read_thread)
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn peer_messages(
        &self,
        peer_device_id: &str,
        thread_id: &str,
        limit: u32,
    ) -> Result<Vec<PeerMessageRecord>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT row_id, thread_id, peer_device_id, sender_instance_id, message_id,
                        direction, kind, correlation_id, disposition, rejection, envelope_json,
                        ingress_id, job_id, created_at_ms
                 FROM peer_messages WHERE peer_device_id=?1 AND thread_id=?2
                 ORDER BY created_at_ms ASC, row_id ASC LIMIT ?3",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(
                params![peer_device_id, thread_id, i64::from(limit)],
                read_message,
            )
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
            .into_iter()
            .collect()
    }

    /// Accepted task requests in this thread whose result has not been written
    /// yet — the work list for materializing results.
    pub fn peer_messages_awaiting_result(
        &self,
        peer_device_id: &str,
        thread_id: &str,
    ) -> Result<Vec<PeerMessageRecord>, String> {
        Ok(self
            .peer_messages(peer_device_id, thread_id, 1_000)?
            .into_iter()
            .filter(|message| {
                message.direction == PeerDirection::Inbound
                    && message.disposition == PeerDisposition::Accepted
                    && message.job_id.is_some()
                    && PeerMessageKind::parse(&message.kind)
                        .is_some_and(PeerMessageKind::expects_result)
            })
            .collect())
    }

    /// Admit one artifact for one authenticated pairing.
    ///
    /// Called only after the bytes were decoded, stored and verified against
    /// the digest the sender declared — a failed upload must leave no row, or
    /// the row would authorize content this installation does not hold.
    ///
    /// Re-uploading the same content refreshes the admission rather than
    /// failing: a peer that hands the bytes over again is doing the one thing
    /// that legitimately renews standing to reference them.
    ///
    /// Every upload also prunes: admissions that have expired are deleted
    /// outright, and what remains is trimmed to
    /// [`MAX_PEER_ARTIFACT_RECEIPTS_PER_PEER`] and
    /// [`MAX_PEER_ARTIFACT_RECEIPTS_TOTAL`]. A row that no longer authorizes
    /// anything is not evidence of anything either, and the alternative is a
    /// table a peer can grow forever one unique blob at a time.
    #[allow(clippy::too_many_arguments)]
    pub fn record_peer_artifact_receipt(
        &mut self,
        peer_device_id: &str,
        artifact_id: &str,
        sha256: &str,
        size_bytes: u64,
        filename: Option<&str>,
        media_type: Option<&str>,
        now_ms: i64,
    ) -> Result<PeerArtifactReceipt, String> {
        let uploaded_at_ms = now_ms.max(1);
        let expires_at_ms = uploaded_at_ms.saturating_add(PEER_ARTIFACT_ADMISSION_TTL_MS);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT INTO peer_artifact_receipts (
                    peer_device_id, artifact_id, sha256, size_bytes, filename, media_type,
                    uploaded_at_ms, expires_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(peer_device_id, artifact_id) DO UPDATE SET
                    sha256=excluded.sha256,
                    size_bytes=excluded.size_bytes,
                    filename=excluded.filename,
                    media_type=excluded.media_type,
                    uploaded_at_ms=excluded.uploaded_at_ms,
                    expires_at_ms=excluded.expires_at_ms",
                params![
                    peer_device_id,
                    artifact_id,
                    sha256.to_ascii_lowercase(),
                    i64::try_from(size_bytes).unwrap_or(i64::MAX),
                    filename,
                    media_type,
                    uploaded_at_ms,
                    expires_at_ms,
                ],
            )
            .map_err(|error| format!("Failed to record the peer artifact: {error}"))?;
        // Expired first — those authorize nothing and are the ones that
        // accumulate — then the caps, which only ever bite on live admissions.
        // The row just written is the newest, so no prune can drop it.
        transaction
            .execute(
                "DELETE FROM peer_artifact_receipts WHERE expires_at_ms <= ?1",
                params![uploaded_at_ms],
            )
            .map_err(|error| format!("Failed to prune expired peer artifacts: {error}"))?;
        transaction
            .execute(
                "DELETE FROM peer_artifact_receipts
                  WHERE peer_device_id = ?1
                    AND artifact_id NOT IN (
                        SELECT artifact_id FROM peer_artifact_receipts
                         WHERE peer_device_id = ?1
                         ORDER BY uploaded_at_ms DESC, artifact_id DESC
                         LIMIT ?2
                    )",
                params![peer_device_id, MAX_PEER_ARTIFACT_RECEIPTS_PER_PEER],
            )
            .map_err(|error| format!("Failed to bound the peer artifact table: {error}"))?;
        transaction
            .execute(
                "DELETE FROM peer_artifact_receipts
                  WHERE rowid NOT IN (
                        SELECT rowid FROM peer_artifact_receipts
                         ORDER BY uploaded_at_ms DESC, rowid DESC
                         LIMIT ?1
                    )",
                params![MAX_PEER_ARTIFACT_RECEIPTS_TOTAL],
            )
            .map_err(|error| format!("Failed to bound the peer artifact table: {error}"))?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(PeerArtifactReceipt {
            peer_device_id: peer_device_id.to_string(),
            artifact_id: artifact_id.to_string(),
            sha256: sha256.to_ascii_lowercase(),
            size_bytes,
            filename: filename.map(str::to_string),
            media_type: media_type.map(str::to_string),
            uploaded_at_ms,
            expires_at_ms,
        })
    }

    /// The live admission this pairing holds for one artifact, if any.
    ///
    /// The device id is in the key, not a filter that could be dropped for
    /// convenience: without it, a peer could reference content another peer
    /// uploaded — or any blob at all that happens to be in the shared content
    /// store — by knowing nothing but its digest.
    ///
    /// An expired admission answers `None`. It is not deleted here: reads stay
    /// read-only, and the row is cleared when the peer is cleared or revoked.
    pub fn peer_artifact_receipt(
        &self,
        peer_device_id: &str,
        artifact_id: &str,
        now_ms: i64,
    ) -> Result<Option<PeerArtifactReceipt>, String> {
        self.connection
            .query_row(
                "SELECT peer_device_id, artifact_id, sha256, size_bytes, filename, media_type,
                        uploaded_at_ms, expires_at_ms
                   FROM peer_artifact_receipts
                  WHERE peer_device_id=?1 AND artifact_id=?2 AND expires_at_ms > ?3",
                params![peer_device_id, artifact_id, now_ms],
                read_receipt,
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    /// Admissions this pairing still holds, newest first.
    pub fn peer_artifact_receipts(
        &self,
        peer_device_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<PeerArtifactReceipt>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT peer_device_id, artifact_id, sha256, size_bytes, filename, media_type,
                        uploaded_at_ms, expires_at_ms
                   FROM peer_artifact_receipts
                  WHERE (?1 IS NULL OR peer_device_id = ?1)
                  ORDER BY uploaded_at_ms DESC LIMIT ?2",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(params![peer_device_id, i64::from(limit)], read_receipt)
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    /// Note that this installation sent something to a peer.
    ///
    /// Written *before* the call goes out, as `pending`, and updated with what
    /// the far side answered — so a row means this side asked, not that the
    /// peer took it. See `peer_tool::deliver_envelope` for why that order is
    /// the one that survives a crash. Re-sending the same message id updates
    /// the state rather than duplicating the row.
    #[allow(clippy::too_many_arguments)]
    pub fn record_outbound_peer_message(
        &mut self,
        alias: &str,
        message_id: &str,
        thread_id: &str,
        correlation_id: Option<&str>,
        kind: &str,
        state: &str,
        now_ms: i64,
    ) -> Result<(), String> {
        self.connection
            .execute(
                "INSERT INTO peer_outbound_messages (
                    alias, message_id, thread_id, correlation_id, kind, state,
                    result_text, sent_at_ms, checked_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, NULL)
                 ON CONFLICT(alias, message_id) DO UPDATE SET state=excluded.state",
                params![
                    alias,
                    message_id,
                    thread_id,
                    correlation_id,
                    kind,
                    state,
                    now_ms.max(1),
                ],
            )
            .map_err(|error| format!("Failed to record the outgoing peer message: {error}"))?;
        Ok(())
    }

    /// Record what a poll of the peer's own thread reported for one message.
    pub fn record_outbound_peer_result(
        &mut self,
        alias: &str,
        message_id: &str,
        state: &str,
        result_text: Option<&str>,
        now_ms: i64,
    ) -> Result<(), String> {
        // Bounded because it is the peer's text and this table is the operator's
        // screen, not a transcript.
        let text = result_text.map(|value| {
            let mut bounded: String = value.chars().take(2_000).collect();
            if bounded.len() > 4_096 {
                bounded.truncate(4_096);
                while !bounded.is_char_boundary(bounded.len()) {
                    bounded.pop();
                }
            }
            bounded
        });
        self.connection
            .execute(
                "UPDATE peer_outbound_messages
                    SET state=?3, result_text=COALESCE(?4, result_text), checked_at_ms=?5
                  WHERE alias=?1 AND message_id=?2",
                params![alias, message_id, state, text, now_ms.max(1)],
            )
            .map_err(|error| format!("Failed to record the peer's answer: {error}"))?;
        Ok(())
    }

    /// What this installation has sent, newest first, for one peer or all.
    pub fn outbound_peer_messages(
        &self,
        alias: Option<&str>,
        limit: u32,
    ) -> Result<Vec<PeerOutboundMessage>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT alias, message_id, thread_id, correlation_id, kind, state,
                        result_text, sent_at_ms, checked_at_ms
                   FROM peer_outbound_messages
                  WHERE (?1 IS NULL OR alias = ?1)
                  ORDER BY sent_at_ms DESC, message_id DESC LIMIT ?2",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(params![alias, i64::from(limit)], |row| {
                Ok(PeerOutboundMessage {
                    alias: row.get(0)?,
                    message_id: row.get(1)?,
                    thread_id: row.get(2)?,
                    correlation_id: row.get(3)?,
                    kind: row.get(4)?,
                    state: row.get(5)?,
                    result_text: row.get(6)?,
                    sent_at_ms: row.get(7)?,
                    checked_at_ms: row.get(8)?,
                })
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    /// Forget what this installation sent to one peer. Used when the operator
    /// forgets the peer itself; there is nothing left to poll.
    pub fn delete_outbound_peer_messages(&mut self, alias: &str) -> Result<u32, String> {
        self.connection
            .execute("DELETE FROM peer_outbound_messages WHERE alias=?1", [alias])
            .map(|removed| u32::try_from(removed).unwrap_or(u32::MAX))
            .map_err(|error| error.to_string())
    }

    /// Drop everything a revoked peer left behind.
    ///
    /// Revocation itself happens in the remote store, where the pairing is;
    /// this is the traffic that pairing produced, removed so a peer the
    /// operator threw out does not keep occupying the Peers screen.
    ///
    /// The artifact *admissions* go too, which is the point: an operator who
    /// clears a peer has withdrawn the standing it had to reference anything it
    /// handed over, so the next reference has to be preceded by a fresh upload.
    /// The blobs themselves stay — the content store is shared, and a blob may
    /// equally belong to a run, a channel attachment or the operator's own
    /// import, none of which this peer's departure has anything to say about.
    pub fn delete_peer_traffic(&mut self, peer_device_id: &str) -> Result<u32, String> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "DELETE FROM peer_messages WHERE peer_device_id=?1",
                [peer_device_id],
            )
            .map_err(|error| error.to_string())?;
        let threads = transaction
            .execute(
                "DELETE FROM peer_threads WHERE peer_device_id=?1",
                [peer_device_id],
            )
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "DELETE FROM peer_rejection_events WHERE peer_device_id=?1",
                [peer_device_id],
            )
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "DELETE FROM peer_artifact_receipts WHERE peer_device_id=?1",
                [peer_device_id],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(u32::try_from(threads).unwrap_or(u32::MAX))
    }
}

fn read_receipt(row: &rusqlite::Row<'_>) -> rusqlite::Result<PeerArtifactReceipt> {
    Ok(PeerArtifactReceipt {
        peer_device_id: row.get(0)?,
        artifact_id: row.get(1)?,
        sha256: row.get(2)?,
        size_bytes: u64::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
        filename: row.get(4)?,
        media_type: row.get(5)?,
        uploaded_at_ms: row.get(6)?,
        expires_at_ms: row.get(7)?,
    })
}

fn read_thread(row: &rusqlite::Row<'_>) -> rusqlite::Result<PeerThreadRecord> {
    Ok(PeerThreadRecord {
        thread_id: row.get(0)?,
        peer_device_id: row.get(1)?,
        peer_instance_id: row.get(2)?,
        session_key: row.get(3)?,
        created_at_ms: row.get(4)?,
        last_activity_at_ms: row.get(5)?,
    })
}

type MessageRow = Result<PeerMessageRecord, String>;

fn read_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRow> {
    let direction_token: String = row.get(5)?;
    let disposition_token: String = row.get(8)?;
    let direction = match PeerDirection::parse(&direction_token) {
        Ok(direction) => direction,
        Err(error) => return Ok(Err(error)),
    };
    let disposition = match PeerDisposition::parse(&disposition_token) {
        Ok(disposition) => disposition,
        Err(error) => return Ok(Err(error)),
    };
    Ok(Ok(PeerMessageRecord {
        row_id: row.get(0)?,
        thread_id: row.get(1)?,
        peer_device_id: row.get(2)?,
        sender_instance_id: row.get(3)?,
        message_id: row.get(4)?,
        direction,
        kind: row.get(6)?,
        correlation_id: row.get(7)?,
        disposition,
        rejection: row.get(9)?,
        envelope_json: row.get(10)?,
        ingress_id: row.get(11)?,
        job_id: row.get(12)?,
        created_at_ms: row.get(13)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::peers::PeerMessageKind;

    const NOW: i64 = 1_700_000_000_000;

    fn envelope(message_id: &str, kind: PeerMessageKind) -> PeerEnvelope {
        PeerEnvelope::new(
            message_id,
            "thread-1",
            kind,
            "instance-remote",
            "look at the failing test",
            NOW,
            60_000,
        )
    }

    fn store_with_thread() -> DaemonStore {
        let mut store = DaemonStore::open_in_memory().expect("open");
        store
            .upsert_peer_thread(
                "thread-1",
                "device-1",
                "instance-remote",
                "peer:device-1:thread-1",
                NOW,
            )
            .expect("thread");
        store
    }

    #[test]
    fn a_thread_keeps_its_session_when_the_peer_writes_again() {
        let mut store = store_with_thread();
        let again = store
            .upsert_peer_thread(
                "thread-1",
                "device-1",
                "instance-remote",
                "peer:device-1:somewhere-else",
                NOW + 5_000,
            )
            .expect("thread");

        assert_eq!(again.session_key, "peer:device-1:thread-1");
        assert_eq!(again.created_at_ms, NOW);
        assert_eq!(again.last_activity_at_ms, NOW + 5_000);
    }

    #[test]
    fn a_redelivered_message_collapses_onto_the_first_decision() {
        let mut store = store_with_thread();
        let first = store
            .record_peer_message(
                "thread-1",
                "device-1",
                &envelope("msg-1", PeerMessageKind::TaskRequest),
                NOW,
            )
            .expect("record");
        let PeerRecording::Recorded { row_id } = first else {
            panic!("expected a new row");
        };
        store
            .attach_peer_message_run(&row_id, Some("ingr-1"), Some("ingress-abc"))
            .expect("attach");

        let second = store
            .record_peer_message(
                "thread-1",
                "device-1",
                &envelope("msg-1", PeerMessageKind::TaskRequest),
                NOW + 1_000,
            )
            .expect("record again");

        assert_eq!(
            second,
            PeerRecording::Duplicate {
                row_id,
                disposition: PeerDisposition::Accepted,
                job_id: Some("ingress-abc".into()),
            }
        );
        assert_eq!(
            store
                .peer_messages("device-1", "thread-1", 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_rejected_message_stays_rejected_when_it_is_sent_again() {
        let mut store = store_with_thread();
        let PeerRecording::Recorded { row_id } = store
            .record_peer_message(
                "thread-1",
                "device-1",
                &envelope("msg-1", PeerMessageKind::Message),
                NOW,
            )
            .expect("record")
        else {
            panic!("expected a new row");
        };
        store
            .reject_peer_message(&row_id, PeerRejection::MissingCapability)
            .expect("reject");

        let retried = store
            .record_peer_message(
                "thread-1",
                "device-1",
                &envelope("msg-1", PeerMessageKind::Message),
                NOW + 1,
            )
            .expect("record again");
        assert!(matches!(
            retried,
            PeerRecording::Duplicate {
                disposition: PeerDisposition::Rejected,
                ..
            }
        ));

        let stored = &store.peer_messages("device-1", "thread-1", 10).unwrap()[0];
        assert_eq!(stored.rejection.as_deref(), Some("missing_capability"));
    }

    #[test]
    fn a_result_is_written_once_per_finished_run() {
        let mut store = store_with_thread();
        let payload = serde_json::json!({ "state": "succeeded", "text": "done" }).to_string();

        let first = store
            .record_peer_result(
                "thread-1",
                "device-1",
                "instance-local",
                "result-ingress-abc",
                Some("corr-1"),
                "ingress-abc",
                &payload,
                NOW,
            )
            .expect("result");
        assert!(matches!(first, PeerRecording::Recorded { .. }));

        let again = store
            .record_peer_result(
                "thread-1",
                "device-1",
                "instance-local",
                "result-ingress-abc",
                Some("corr-1"),
                "ingress-abc",
                &payload,
                NOW + 10,
            )
            .expect("result again");
        assert!(matches!(again, PeerRecording::Duplicate { .. }));

        let messages = store.peer_messages("device-1", "thread-1", 10).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].direction, PeerDirection::Outbound);
        assert_eq!(messages[0].job_id.as_deref(), Some("ingress-abc"));
    }

    #[test]
    fn only_accepted_task_requests_wait_for_a_result() {
        let mut store = store_with_thread();
        for (id, kind) in [
            ("msg-1", PeerMessageKind::TaskRequest),
            ("msg-2", PeerMessageKind::Message),
        ] {
            let PeerRecording::Recorded { row_id } = store
                .record_peer_message("thread-1", "device-1", &envelope(id, kind), NOW)
                .expect("record")
            else {
                panic!("expected a new row");
            };
            store
                .attach_peer_message_run(&row_id, Some("ingr-x"), Some("job-x"))
                .expect("attach");
        }
        let PeerRecording::Recorded { row_id } = store
            .record_peer_message(
                "thread-1",
                "device-1",
                &envelope("msg-3", PeerMessageKind::TaskRequest),
                NOW,
            )
            .expect("record")
        else {
            panic!("expected a new row");
        };
        store
            .reject_peer_message(&row_id, PeerRejection::PeerRevoked)
            .expect("reject");

        let awaiting = store
            .peer_messages_awaiting_result("device-1", "thread-1")
            .unwrap();
        assert_eq!(awaiting.len(), 1);
        assert_eq!(awaiting[0].message_id, "msg-1");
    }

    /// The bound is on the table, not on the query that reads it.
    #[test]
    fn a_peer_flooding_refusals_cannot_grow_the_table_past_the_bound() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let limit = MAX_PEER_REJECTION_EVENTS_PER_PEER;
        for index in 0..limit * 3 {
            store
                .record_peer_rejection_event(
                    "device-1",
                    Some("msg-1"),
                    Some("thread-1"),
                    PeerRejection::OriginLoop,
                    NOW + i64::from(index),
                )
                .expect("record");
        }

        assert_eq!(
            store.peer_rejection_event_count(Some("device-1")).unwrap(),
            limit
        );
        // What survives is the *newest*, which is what a doctor reading recent
        // traffic needs — and the older two thirds are gone from disk rather
        // than merely unread.
        let events = store.peer_rejection_events(limit + 10).expect("events");
        assert_eq!(events.len() as u32, limit);
        assert_eq!(events[0].occurred_at_ms, NOW + i64::from(limit * 3 - 1));
        let oldest_kept = NOW + i64::from(limit * 2);
        assert_eq!(
            events.last().map(|event| event.occurred_at_ms),
            Some(oldest_kept)
        );
        assert!(
            events
                .iter()
                .all(|event| event.occurred_at_ms >= oldest_kept),
            "an event older than the window survived the prune"
        );
    }

    #[test]
    fn one_peers_flood_does_not_evict_another_peers_evidence() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        store
            .record_peer_rejection_event("device-quiet", None, None, PeerRejection::Expired, NOW)
            .expect("record");
        for index in 0..MAX_PEER_REJECTION_EVENTS_PER_PEER * 2 {
            store
                .record_peer_rejection_event(
                    "device-loud",
                    None,
                    None,
                    PeerRejection::MalformedId,
                    NOW + 1 + i64::from(index),
                )
                .expect("record");
        }

        assert_eq!(
            store
                .peer_rejection_event_count(Some("device-quiet"))
                .unwrap(),
            1,
            "the quiet peer's single refusal is still attributable to it"
        );
        assert_eq!(
            store
                .peer_rejection_event_count(Some("device-loud"))
                .unwrap(),
            MAX_PEER_REJECTION_EVENTS_PER_PEER
        );
    }

    /// Per-peer fairness bounds who gets evicted; this bounds the table.
    #[test]
    fn refusals_are_bounded_across_every_pairing_too() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        // Enough pairings that the per-peer bound alone would allow more rows
        // than the global one: 40 × 200 = 8000 against a 5000 ceiling. Seeded
        // straight into the table rather than through 8000 API calls, each of
        // which would re-run both prunes — the prune under test is the one the
        // single recorded event below triggers.
        let pairings = (MAX_PEER_REJECTION_EVENTS_TOTAL / MAX_PEER_REJECTION_EVENTS_PER_PEER) + 15;
        let transaction = store.connection.transaction().expect("seed");
        for peer in 0..pairings {
            for index in 0..MAX_PEER_REJECTION_EVENTS_PER_PEER {
                let sequence = peer * MAX_PEER_REJECTION_EVENTS_PER_PEER + index;
                transaction
                    .execute(
                        "INSERT INTO peer_rejection_events (
                            event_id, peer_device_id, message_id, thread_id, reason, occurred_at_ms
                         ) VALUES (?1, ?2, NULL, NULL, 'origin_loop', ?3)",
                        params![
                            format!("prej-seed-{sequence}"),
                            format!("device-{peer}"),
                            NOW + i64::from(sequence),
                        ],
                    )
                    .expect("seed");
            }
        }
        transaction.commit().expect("seed");
        assert_eq!(
            store.peer_rejection_event_count(None).unwrap(),
            pairings * MAX_PEER_REJECTION_EVENTS_PER_PEER
        );

        store
            .record_peer_rejection_event(
                "device-0",
                None,
                None,
                PeerRejection::OriginLoop,
                NOW + 1_000_000,
            )
            .expect("record");

        assert_eq!(
            store.peer_rejection_event_count(None).unwrap(),
            MAX_PEER_REJECTION_EVENTS_TOTAL,
            "the table's own bound has to hold however many pairings exist"
        );
    }

    /// An admission that authorizes nothing is not kept as though it did.
    #[test]
    fn expired_admissions_are_pruned_by_the_next_upload() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let stale = "a".repeat(64);
        store
            .record_peer_artifact_receipt("device-1", &stale, &stale, 4, None, None, NOW)
            .expect("admit");
        assert_eq!(store.peer_artifact_receipts(None, 10).unwrap().len(), 1);

        // One upload after the first has expired. Nothing is scanning in the
        // background; the write path is what does the pruning.
        let later = NOW + PEER_ARTIFACT_ADMISSION_TTL_MS + 1;
        let fresh = "b".repeat(64);
        store
            .record_peer_artifact_receipt("device-1", &fresh, &fresh, 4, None, None, later)
            .expect("admit");

        let remaining = store.peer_artifact_receipts(None, 10).expect("read");
        assert_eq!(
            remaining.len(),
            1,
            "the expired row is gone from disk, not merely filtered out of reads"
        );
        assert_eq!(remaining[0].artifact_id, fresh);
    }

    /// Unique content in a loop is the case expiry alone does not answer, since
    /// every upload can be inside its own window.
    #[test]
    fn live_admissions_are_bounded_per_pairing() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let over = MAX_PEER_ARTIFACT_RECEIPTS_PER_PEER + 25;
        for index in 0..over {
            let digest = format!("{index:064x}");
            store
                .record_peer_artifact_receipt(
                    "device-1",
                    &digest,
                    &digest,
                    4,
                    None,
                    None,
                    NOW + i64::from(index),
                )
                .expect("admit");
        }

        assert_eq!(
            store
                .peer_artifact_receipts(Some("device-1"), MAX_PEER_ARTIFACT_RECEIPTS_PER_PEER + 100)
                .unwrap()
                .len() as u32,
            MAX_PEER_ARTIFACT_RECEIPTS_PER_PEER
        );
        // The newest survive, so the admissions a peer is actually using are
        // the ones still standing.
        let newest = format!("{:064x}", over - 1);
        assert!(store
            .peer_artifact_receipt("device-1", &newest, NOW + i64::from(over))
            .unwrap()
            .is_some());
    }

    #[test]
    fn an_artifact_admission_belongs_to_one_pairing_and_expires() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let digest = "a".repeat(64);
        store
            .record_peer_artifact_receipt(
                "device-1",
                &digest,
                &digest,
                21,
                Some("build.log"),
                Some("text/plain"),
                NOW,
            )
            .expect("admit");

        let receipt = store
            .peer_artifact_receipt("device-1", &digest, NOW + 1)
            .expect("read")
            .expect("admitted");
        assert_eq!(receipt.size_bytes, 21);
        assert_eq!(receipt.filename.as_deref(), Some("build.log"));

        // Another pairing knowing the digest learns nothing from it.
        assert!(store
            .peer_artifact_receipt("device-2", &digest, NOW + 1)
            .unwrap()
            .is_none());
        // And the admission does not outlive its window.
        assert!(store
            .peer_artifact_receipt("device-1", &digest, receipt.expires_at_ms)
            .unwrap()
            .is_none());
    }

    #[test]
    fn re_uploading_the_same_content_renews_the_admission() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let digest = "b".repeat(64);
        let first = store
            .record_peer_artifact_receipt("device-1", &digest, &digest, 4, None, None, NOW)
            .expect("admit");
        let again = store
            .record_peer_artifact_receipt(
                "device-1",
                &digest,
                &digest,
                4,
                Some("again.log"),
                None,
                first.expires_at_ms - 1,
            )
            .expect("admit again");

        assert!(again.expires_at_ms > first.expires_at_ms);
        assert_eq!(
            store
                .peer_artifact_receipts(Some("device-1"), 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .peer_artifact_receipt("device-1", &digest, first.expires_at_ms)
                .unwrap()
                .and_then(|receipt| receipt.filename),
            Some("again.log".into())
        );
    }

    #[test]
    fn clearing_a_peer_takes_its_artifact_admissions_with_it() {
        let mut store = store_with_thread();
        let digest = "c".repeat(64);
        store
            .record_peer_artifact_receipt("device-1", &digest, &digest, 8, None, None, NOW)
            .expect("admit");
        store
            .record_peer_artifact_receipt("device-2", &digest, &digest, 8, None, None, NOW)
            .expect("admit for the other peer");

        store.delete_peer_traffic("device-1").expect("clear");

        assert!(store
            .peer_artifact_receipt("device-1", &digest, NOW + 1)
            .unwrap()
            .is_none());
        assert!(
            store
                .peer_artifact_receipt("device-2", &digest, NOW + 1)
                .unwrap()
                .is_some(),
            "clearing one peer says nothing about another's admissions"
        );
    }

    #[test]
    fn what_this_installation_sent_is_remembered_with_the_answer_it_got() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        store
            .record_outbound_peer_message(
                "studio",
                "pmsg-1",
                "thread-1",
                Some("corr-1"),
                "task_request",
                "queued",
                NOW,
            )
            .expect("record");
        store
            .record_outbound_peer_result(
                "studio",
                "pmsg-1",
                "succeeded",
                Some("all green"),
                NOW + 5,
            )
            .expect("result");

        let rows = store.outbound_peer_messages(Some("studio"), 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, "succeeded");
        assert_eq!(rows[0].result_text.as_deref(), Some("all green"));
        assert_eq!(rows[0].correlation_id.as_deref(), Some("corr-1"));
        assert_eq!(rows[0].checked_at_ms, Some(NOW + 5));

        assert_eq!(store.delete_outbound_peer_messages("studio").unwrap(), 1);
        assert!(store.outbound_peer_messages(None, 10).unwrap().is_empty());
    }

    #[test]
    fn revoking_a_peer_takes_its_traffic_with_it() {
        let mut store = store_with_thread();
        store
            .record_peer_message(
                "thread-1",
                "device-1",
                &envelope("msg-1", PeerMessageKind::Message),
                NOW,
            )
            .expect("record");
        store
            .upsert_peer_thread(
                "thread-2",
                "device-2",
                "instance-other",
                "peer:device-2:thread-2",
                NOW,
            )
            .expect("other thread");

        assert_eq!(store.delete_peer_traffic("device-1").unwrap(), 1);
        assert!(store.peer_thread("device-1", "thread-1").unwrap().is_none());
        assert!(store.peer_thread("device-2", "thread-2").unwrap().is_some());
        assert_eq!(store.peer_threads(None, 10).unwrap().len(), 1);
    }
}
