//! The `send_message` agent tool.
//!
//! A run that arrived from a messaging channel can answer it. That is the
//! entire capability, and the shape of this file is what keeps it that small:
//!
//! - The destination is read from the durable event that produced the job, not
//!   from a tool argument. A model cannot be talked into replying somewhere
//!   else by the message it is reading, because there is no parameter for it.
//! - The reply is queued into the outbox rather than sent here. The tool
//!   returns as soon as the row is durable, so a crash between "the model said
//!   it" and "the provider has it" resolves the same way every other outbound
//!   message does.
//! - The idempotency key is derived from the job and the number of replies it
//!   has already queued, so a retried run cannot duplicate a reply.
//! - Reply depth is carried forward, which is what lets the inbound gate stop
//!   two agents from talking to each other forever.

use little_monkey_lib::channels::types::{ChannelEnvelope, OutboundMessage};

use super::channel_ingress::OutboxPayload;
use super::channel_store::{ChannelOrigin, NewOutboxMessage, OutboxEnqueue};
use super::store::{DaemonPaths, DaemonStore};
use super::trigger::sha256_hex;

/// Retry budget for an agent's reply. Matches the pairing challenge: a reply
/// that will not go out in a few attempts needs an operator, not a longer tail.
const REPLY_MAX_ATTEMPTS: u32 = 3;

/// Longest reply this tool will queue. Providers impose their own, much smaller
/// limits and adapters split accordingly; this is only the outer bound that
/// keeps a runaway model from writing a megabyte into the daemon database.
const MAX_REPLY_CHARS: usize = 16_000;

/// Environment variable the daemon sets on a task child so it knows which job
/// it is. Absent for every other kind of run, which is exactly how this tool
/// knows it has nothing to reply to.
const JOB_ID_ENV: &str = "LITTLE_MONKEY_DAEMON_JOB_ID";

/// The origin of the current process's run, if it has one.
pub(crate) fn current_channel_origin() -> Option<(String, ChannelOrigin)> {
    let job_id = std::env::var(JOB_ID_ENV).ok().filter(|id| !id.is_empty())?;
    let paths = DaemonPaths::resolve().ok()?;
    let store = DaemonStore::open(&paths).ok()?;
    let origin = store.channel_origin_for_job(&job_id).ok().flatten()?;
    Some((job_id, origin))
}

/// Queue one reply to the conversation this run came from.
///
/// Returns the JSON the tool loop hands back to the model. Deliberately terse:
/// the model is told the reply is queued and nothing about the transport, the
/// account, or the recipient — none of which it needs, and all of which would
/// be new material for it to try to act on.
pub(crate) fn send_message(text: &str) -> Result<serde_json::Value, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("A reply must contain some text.".to_string());
    }
    if text.chars().count() > MAX_REPLY_CHARS {
        return Err(format!(
            "A reply must be at most {MAX_REPLY_CHARS} characters; this one is {}.",
            text.chars().count()
        ));
    }

    let Some((job_id, origin)) = current_channel_origin() else {
        return Err(
            "This run did not arrive from a messaging conversation, so there is nowhere to send a message."
                .to_string(),
        );
    };

    let paths = DaemonPaths::resolve()?;
    let mut store = DaemonStore::open(&paths)?;
    let account = store
        .channel_account(&origin.account_id)?
        .ok_or_else(|| "The account this conversation belongs to no longer exists.".to_string())?;
    if !account.enabled {
        return Err("The account this conversation belongs to is disabled.".to_string());
    }

    // The depth of the message being answered plus one, so an exchange between
    // two automated systems is bounded rather than perpetual.
    let reply_depth = inbound_reply_depth(&store, &job_id).saturating_add(1);
    let sequence = store.outbox_count_for_job(&job_id)?;
    let idempotency_key = format!("reply-{job_id}-{sequence}");

    let payload = OutboxPayload {
        message: OutboundMessage {
            account_id: origin.account_id.clone(),
            kind: account.kind,
            conversation_id: origin.conversation_id.clone(),
            thread_id: origin.thread_id.clone(),
            text: text.to_string(),
            attachments: Vec::new(),
            reply_to_provider_id: Some(origin.provider_event_id.clone()),
            idempotency_key: idempotency_key.clone(),
        },
        reply_depth,
    };
    let payload_json = serde_json::to_string(&payload).map_err(|error| error.to_string())?;

    let queued = store.enqueue_channel_message(&NewOutboxMessage {
        account_id: origin.account_id,
        conversation_id: origin.conversation_id,
        thread_id: origin.thread_id,
        reply_to_provider_id: Some(origin.provider_event_id),
        payload_digest: sha256_hex(payload_json.as_bytes()),
        payload_json,
        idempotency_key,
        max_attempts: REPLY_MAX_ATTEMPTS,
        job_id: Some(job_id),
        created_at_ms: now_ms()?,
    })?;

    Ok(match queued {
        OutboxEnqueue::Queued { .. } => serde_json::json!({
            "status": "queued",
            "note": "The reply is queued for delivery to the originating conversation."
        }),
        OutboxEnqueue::AlreadyQueued { .. } => serde_json::json!({
            "status": "already_queued",
            "note": "An identical reply was already queued for this run; nothing was duplicated."
        }),
    })
}

/// Depth of the message being answered.
///
/// Recomputed from the stored inbound envelope rather than carried in the
/// environment, for the same reason the destination is: the model's process
/// must not be able to influence the number that bounds an automated chain.
fn inbound_reply_depth(store: &DaemonStore, job_id: &str) -> u32 {
    let Ok(Some(envelope_json)) = store.inbound_envelope_for_job(job_id) else {
        return 0;
    };
    let Ok(envelope) = serde_json::from_str::<ChannelEnvelope>(&envelope_json) else {
        return 0;
    };
    super::channel_ingress::inherited_reply_depth(store, &envelope).unwrap_or(0)
}

fn now_ms() -> Result<i64, String> {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "System clock is before the Unix epoch".to_string())?
            .as_millis(),
    )
    .map_err(|_| "System clock is beyond the supported range".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_reply_is_refused_before_anything_is_opened() {
        assert!(send_message("   ").is_err());
    }

    #[test]
    fn an_oversized_reply_is_refused() {
        let huge = "x".repeat(MAX_REPLY_CHARS + 1);
        let error = send_message(&huge).expect_err("too long");
        assert!(error.contains("at most"));
    }

    #[test]
    fn a_run_with_no_channel_origin_has_nowhere_to_send() {
        // No job id in the environment: every non-channel run looks like this.
        std::env::remove_var(JOB_ID_ENV);
        let error = send_message("hello").expect_err("no origin");
        assert!(error.contains("did not arrive from a messaging conversation"));
    }
}
