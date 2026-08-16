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
        self.connection
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
        Ok(())
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

    /// Drop everything a revoked peer left behind.
    ///
    /// Revocation itself happens in the remote store, where the pairing is;
    /// this is the traffic that pairing produced, removed so a peer the
    /// operator threw out does not keep occupying the Peers screen.
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
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(u32::try_from(threads).unwrap_or(u32::MAX))
    }
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
