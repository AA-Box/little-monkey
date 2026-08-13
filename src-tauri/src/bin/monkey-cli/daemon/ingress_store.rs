//! Durable storage for accepted conversation turns, whatever they arrived on.
//!
//! A turn is *accepted* the moment the gate that owns its origin decides it
//! should run, and it is *queued* only once the daemon queue has taken it. The
//! window between those two facts is the one a crash can land in, and this
//! table is what makes that window survivable: an accepted row carries the
//! frozen [`ConversationIngress`] and the recipe parameters it was going to be
//! submitted with, so recovery re-submits the same turn rather than trying to
//! reconstruct it from the provider.
//!
//! Owns the `ingress_turns` table created by `DAEMON_V8_SQL` in `store.rs`.
//!
//! # At most once
//!
//! `dedupe_key` is `source:source_account:source_event_id` and it is UNIQUE, so
//! a provider redelivery, a replayed polling window and a recovery pass all
//! land on the row that already exists. The queue's `deterministic_job_id` is
//! the second line of defense: re-submitting a row whose job already exists is
//! a no-op there, which is what makes recovery safe to run at any time.
//!
//! # Content
//!
//! `ingress_json` contains the message text, exactly as `channel_events`
//! already stores the envelope it came in. It never carries a credential: the
//! ingress record has no field for one.

use rusqlite::{params, OptionalExtension, TransactionBehavior};

use little_monkey_lib::channels::ingress::{ConversationIngress, ConversationSource};

use super::store::DaemonStore;

/// Where one accepted turn is in its journey to the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngressState {
    /// Durably accepted, not yet queued. Recovery re-submits these.
    Accepted,
    /// The daemon queue has it; `job_id` is set.
    Queued,
    /// Submission failed too many times, or the row was rejected outright.
    /// Nothing retries a failed row automatically.
    Failed,
}

impl IngressState {
    pub fn as_str(self) -> &'static str {
        match self {
            IngressState::Accepted => "accepted",
            IngressState::Queued => "queued",
            IngressState::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Result<IngressState, String> {
        match value {
            "accepted" => Ok(IngressState::Accepted),
            "queued" => Ok(IngressState::Queued),
            "failed" => Ok(IngressState::Failed),
            other => Err(format!("unknown ingress state '{other}'")),
        }
    }
}

/// What accepting a turn did.
#[derive(Debug, Clone, PartialEq)]
pub enum IngressAcceptance {
    /// A new row. The caller owns submitting it.
    Accepted { ingress_id: String },
    /// This turn was accepted before. The caller must not submit it again
    /// unless the state is still `Accepted`.
    Existing {
        ingress_id: String,
        state: IngressState,
        job_id: Option<String>,
    },
}

/// One accepted turn as stored, without its content.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredIngressTurn {
    pub ingress_id: String,
    pub source: ConversationSource,
    pub source_account_id: String,
    pub source_event_id: String,
    pub session_key: String,
    pub state: IngressState,
    pub job_id: Option<String>,
    pub attempts: u32,
    pub last_error: Option<String>,
    /// Which frozen-context shape this turn was accepted with, and its digest.
    /// Both absent for a turn accepted before execution contexts were frozen.
    pub execution_version: Option<u32>,
    pub execution_digest: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// An accepted turn that never reached the queue, with everything needed to
/// submit it unchanged.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingIngressTurn {
    pub ingress_id: String,
    pub ingress: ConversationIngress,
    pub params: Vec<String>,
    pub attempts: u32,
}

impl DaemonStore {
    /// Record that a turn was accepted, before anything tries to run it.
    ///
    /// Returns [`IngressAcceptance::Existing`] when the turn is already known,
    /// which is the durable half of dedupe — the half that still holds after
    /// the process that first saw the message is gone.
    pub fn accept_ingress_turn(
        &mut self,
        ingress: &ConversationIngress,
        params: &[String],
        now_ms: i64,
    ) -> Result<IngressAcceptance, String> {
        let ingress_json = serde_json::to_string(ingress).map_err(|error| error.to_string())?;
        let params_json = serde_json::to_string(params).map_err(|error| error.to_string())?;
        let dedupe_key = ingress.dedupe_key();
        let ingress_id = format!("ingr-{}", uuid::Uuid::new_v4().simple());
        let now_ms = now_ms.max(1);

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let changed = transaction
            .execute(
                "INSERT INTO ingress_turns (
                    ingress_id, dedupe_key, source, source_account_id, source_event_id,
                    session_key, state, ingress_json, params_json, job_id, attempts,
                    last_error, execution_version, execution_digest,
                    created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'accepted', ?7, ?8, NULL, 0, NULL,
                           ?9, ?10, ?11, ?11)
                 ON CONFLICT(dedupe_key) DO NOTHING",
                params![
                    ingress_id,
                    dedupe_key,
                    ingress.source.as_str(),
                    ingress.source_account_id,
                    ingress.source_event_id,
                    ingress.session_key,
                    ingress_json,
                    params_json,
                    ingress
                        .execution
                        .as_ref()
                        .map(|execution| i64::from(execution.version())),
                    ingress
                        .execution
                        .as_ref()
                        .map(|execution| execution.digest().to_string()),
                    now_ms,
                ],
            )
            .map_err(|error| format!("Failed to record the accepted turn: {error}"))?;
        let acceptance = if changed == 1 {
            IngressAcceptance::Accepted { ingress_id }
        } else {
            let (ingress_id, state, job_id): (String, String, Option<String>) = transaction
                .query_row(
                    "SELECT ingress_id, state, job_id FROM ingress_turns WHERE dedupe_key=?1",
                    [&dedupe_key],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .map_err(|error| error.to_string())?;
            IngressAcceptance::Existing {
                ingress_id,
                state: IngressState::parse(&state)?,
                job_id,
            }
        };
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(acceptance)
    }

    /// Record that the queue took this turn. Idempotent: replaying it with the
    /// same job id changes nothing.
    pub fn mark_ingress_queued(
        &mut self,
        ingress_id: &str,
        job_id: &str,
        now_ms: i64,
    ) -> Result<(), String> {
        let changed = self
            .connection
            .execute(
                "UPDATE ingress_turns
                 SET state='queued', job_id=?2, attempts=attempts+1, last_error=NULL,
                     updated_at_ms=?3
                 WHERE ingress_id=?1",
                params![ingress_id, job_id, now_ms.max(1)],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err(format!("Unknown accepted turn '{ingress_id}'"));
        }
        Ok(())
    }

    /// Record a failed submission. `terminal` parks the row so nothing retries
    /// it; otherwise it stays accepted and recovery will try again.
    pub fn mark_ingress_submit_failed(
        &mut self,
        ingress_id: &str,
        error: &str,
        terminal: bool,
        now_ms: i64,
    ) -> Result<(), String> {
        let state = if terminal {
            IngressState::Failed
        } else {
            IngressState::Accepted
        };
        let changed = self
            .connection
            .execute(
                "UPDATE ingress_turns
                 SET state=?2, attempts=attempts+1, last_error=?3, updated_at_ms=?4
                 WHERE ingress_id=?1",
                params![ingress_id, state.as_str(), error, now_ms.max(1)],
            )
            .map_err(|sql_error| sql_error.to_string())?;
        if changed != 1 {
            return Err(format!("Unknown accepted turn '{ingress_id}'"));
        }
        Ok(())
    }

    /// Turns that were accepted but never queued, oldest first — a restart's
    /// work list.
    pub fn pending_ingress_turns(&self, limit: u32) -> Result<Vec<PendingIngressTurn>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT ingress_id, ingress_json, params_json, attempts
                 FROM ingress_turns WHERE state='accepted'
                 ORDER BY created_at_ms ASC LIMIT ?1",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([i64::from(limit)], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(|error| error.to_string())?;
        let mut pending = Vec::new();
        for row in rows {
            let (ingress_id, ingress_json, params_json, attempts) =
                row.map_err(|error| error.to_string())?;
            pending.push(PendingIngressTurn {
                ingress_id,
                ingress: serde_json::from_str(&ingress_json)
                    .map_err(|error| format!("Stored turn is unreadable: {error}"))?,
                params: serde_json::from_str(&params_json)
                    .map_err(|error| format!("Stored turn parameters are unreadable: {error}"))?,
                attempts: u32::try_from(attempts).unwrap_or(u32::MAX),
            });
        }
        Ok(pending)
    }

    /// One accepted-but-unqueued turn, with everything it was accepted with.
    ///
    /// The single-row form of [`Self::pending_ingress_turns`], for the case a
    /// turn is re-submitted before the first submission succeeded: what runs
    /// has to be what was frozen then, not what the caller is holding now.
    pub fn pending_ingress_turn(
        &self,
        ingress_id: &str,
    ) -> Result<Option<PendingIngressTurn>, String> {
        let row: Option<(String, String, i64)> = self
            .connection
            .query_row(
                "SELECT ingress_json, params_json, attempts
                 FROM ingress_turns WHERE ingress_id=?1 AND state='accepted'",
                [ingress_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let Some((ingress_json, params_json, attempts)) = row else {
            return Ok(None);
        };
        Ok(Some(PendingIngressTurn {
            ingress_id: ingress_id.to_string(),
            ingress: serde_json::from_str(&ingress_json)
                .map_err(|error| format!("Stored turn is unreadable: {error}"))?,
            params: serde_json::from_str(&params_json)
                .map_err(|error| format!("Stored turn parameters are unreadable: {error}"))?,
            attempts: u32::try_from(attempts).unwrap_or(u32::MAX),
        }))
    }

    /// Recent turns across every origin, newest first. The read model behind
    /// the typed bridge: identifiers and status, never message text.
    pub fn recent_ingress_turns(&self, limit: u32) -> Result<Vec<StoredIngressTurn>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT ingress_id, source, source_account_id, source_event_id, session_key,
                        state, job_id, attempts, last_error, execution_version, execution_digest,
                        created_at_ms, updated_at_ms
                 FROM ingress_turns ORDER BY created_at_ms DESC LIMIT ?1",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([i64::from(limit)], read_ingress_turn)
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
            .into_iter()
            .collect()
    }

    /// One turn by the durable submission id handed back to its origin.
    pub fn ingress_turn(&self, ingress_id: &str) -> Result<Option<StoredIngressTurn>, String> {
        self.connection
            .query_row(
                "SELECT ingress_id, source, source_account_id, source_event_id, session_key,
                        state, job_id, attempts, last_error, execution_version, execution_digest,
                        created_at_ms, updated_at_ms
                 FROM ingress_turns WHERE ingress_id=?1",
                [ingress_id],
                read_ingress_turn,
            )
            .optional()
            .map_err(|error| error.to_string())?
            .transpose()
    }
}

type IngressRow = Result<StoredIngressTurn, String>;

fn read_ingress_turn(row: &rusqlite::Row<'_>) -> rusqlite::Result<IngressRow> {
    let source_token: String = row.get(1)?;
    let state_token: String = row.get(5)?;
    let Some(source) = ConversationSource::parse(&source_token) else {
        return Ok(Err(format!("unknown ingress source '{source_token}'")));
    };
    let state = match IngressState::parse(&state_token) {
        Ok(state) => state,
        Err(error) => return Ok(Err(error)),
    };
    Ok(Ok(StoredIngressTurn {
        ingress_id: row.get(0)?,
        source,
        source_account_id: row.get(2)?,
        source_event_id: row.get(3)?,
        session_key: row.get(4)?,
        state,
        job_id: row.get(6)?,
        attempts: u32::try_from(row.get::<_, i64>(7)?).unwrap_or(u32::MAX),
        last_error: row.get(8)?,
        execution_version: row
            .get::<_, Option<i64>>(9)?
            .and_then(|version| u32::try_from(version).ok()),
        execution_digest: row.get(10)?,
        created_at_ms: row.get(11)?,
        updated_at_ms: row.get(12)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::channels::routing::RouteTarget;

    const NOW: i64 = 1_700_000_000_000;

    fn ingress(source: ConversationSource, event_id: &str) -> ConversationIngress {
        ConversationIngress::direct(
            source,
            "acct-1",
            event_id,
            "session-1",
            "ship it",
            RouteTarget::new("chat"),
            NOW,
        )
    }

    #[test]
    fn accepting_the_same_turn_twice_returns_the_first_row() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let turn = ingress(ConversationSource::Peer, "e-1");

        let IngressAcceptance::Accepted { ingress_id } = store
            .accept_ingress_turn(&turn, &["message=ship it".into()], NOW)
            .expect("accept")
        else {
            panic!("expected a new row");
        };
        let second = store
            .accept_ingress_turn(&turn, &["message=ship it".into()], NOW + 10)
            .expect("accept again");

        assert_eq!(
            second,
            IngressAcceptance::Existing {
                ingress_id: ingress_id.clone(),
                state: IngressState::Accepted,
                job_id: None,
            }
        );
        assert_eq!(store.recent_ingress_turns(10).unwrap().len(), 1);
    }

    #[test]
    fn each_source_and_account_dedupes_on_its_own() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        for source in [
            ConversationSource::MessagingChannel,
            ConversationSource::Peer,
            ConversationSource::Voice,
            ConversationSource::Telephone,
            ConversationSource::Mobile,
            ConversationSource::Desktop,
        ] {
            store
                .accept_ingress_turn(&ingress(source, "e-1"), &[], NOW)
                .expect("accept");
        }
        let mut other_account = ingress(ConversationSource::Peer, "e-1");
        other_account.source_account_id = "acct-2".into();
        store
            .accept_ingress_turn(&other_account, &[], NOW)
            .expect("accept");

        assert_eq!(store.recent_ingress_turns(20).unwrap().len(), 7);
    }

    #[test]
    fn a_pending_turn_round_trips_with_its_parameters() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let turn = ingress(ConversationSource::Telephone, "call-1");
        store
            .accept_ingress_turn(
                &turn,
                &["message=hello".into(), "caller=+15550100".into()],
                NOW,
            )
            .expect("accept");

        let pending = store.pending_ingress_turns(10).expect("pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].ingress, turn);
        assert_eq!(pending[0].params, ["message=hello", "caller=+15550100"]);
        assert_eq!(pending[0].attempts, 0);
    }

    #[test]
    fn a_queued_turn_is_no_longer_pending() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let IngressAcceptance::Accepted { ingress_id } = store
            .accept_ingress_turn(&ingress(ConversationSource::Voice, "utt-1"), &[], NOW)
            .expect("accept")
        else {
            panic!("expected a new row");
        };
        store
            .mark_ingress_queued(&ingress_id, "ingress-abc", NOW + 5)
            .expect("queued");

        assert!(store.pending_ingress_turns(10).unwrap().is_empty());
        let stored = store.ingress_turn(&ingress_id).unwrap().expect("row");
        assert_eq!(stored.state, IngressState::Queued);
        assert_eq!(stored.job_id.as_deref(), Some("ingress-abc"));
        assert_eq!(stored.attempts, 1);
        assert_eq!(stored.source, ConversationSource::Voice);
    }

    #[test]
    fn a_retryable_failure_stays_pending_and_a_terminal_one_does_not() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let IngressAcceptance::Accepted { ingress_id } = store
            .accept_ingress_turn(&ingress(ConversationSource::Peer, "e-1"), &[], NOW)
            .expect("accept")
        else {
            panic!("expected a new row");
        };

        store
            .mark_ingress_submit_failed(&ingress_id, "queue is busy", false, NOW + 1)
            .expect("retryable");
        assert_eq!(store.pending_ingress_turns(10).unwrap()[0].attempts, 1);

        store
            .mark_ingress_submit_failed(&ingress_id, "kill switch", true, NOW + 2)
            .expect("terminal");
        assert!(store.pending_ingress_turns(10).unwrap().is_empty());
        let stored = store.ingress_turn(&ingress_id).unwrap().expect("row");
        assert_eq!(stored.state, IngressState::Failed);
        assert_eq!(stored.last_error.as_deref(), Some("kill switch"));
    }

    #[test]
    fn the_read_model_carries_no_message_text() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        store
            .accept_ingress_turn(
                &ingress(ConversationSource::MessagingChannel, "e-1"),
                &["message=ship it".into()],
                NOW,
            )
            .expect("accept");

        let listing = serde_json::to_string(
            &store
                .recent_ingress_turns(10)
                .unwrap()
                .iter()
                .map(|turn| turn.session_key.clone())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(!listing.contains("ship it"));
    }
}
