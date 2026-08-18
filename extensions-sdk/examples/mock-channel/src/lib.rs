//! A messaging provider, in the shape Little Monkey's channel core actually
//! consumes.
//!
//! Everything below is the real contract, not a demonstration of one. The
//! adapter that drives it (`daemon::adapters::extension`) is the same code that
//! drives Telegram and Slack, and what this returns goes straight into the
//! normalized channel path: durable events, dedupe, access policy, session
//! mapping, the outbox and its retry semantics.
//!
//! Four operations, distinguished by the `op` field the host sends:
//!
//! - `probe` — ask the provider who we are. Only a real answer may report
//!   `ok: true`; saved configuration is not a connection.
//! - `poll` — the next batch of inbound messages, resuming from `cursor`.
//! - `send` — deliver one outbound message and classify the outcome.
//! - a webhook delivery — the durable-ingress shape, which carries a
//!   `delivery_id` instead of an `op` and answers with the same normalized
//!   messages plus the account they belong to.
//!
//! There is no network here and no account to configure: the "provider" is a
//! fixed script held in extension-private state, so the example builds and runs
//! offline, in CI, for anyone. A real provider would replace the two marked
//! functions with `host::send_http` calls against origins its manifest grants.

mod bindings {
    wit_bindgen::generate!({
        path: "../../../src-tauri/wit",
        world: "extension",
    });
}

use bindings::exports::little_monkey::extension::guest::Guest;
use bindings::little_monkey::extension::host;
use little_monkey_extension_sdk::{
    json_output, parse_input, require_capability, validate_bounded_id, validate_max_chars,
};
use serde::{Deserialize, Serialize};

/// How many provider event ids this example remembers. The host deduplicates
/// on `(account, provider_event_id)` regardless; this is the provider-side
/// half, which is what stops a redelivered webhook being *fetched* twice.
const MAX_REMEMBERED_EVENTS: usize = 128;
const SEEN_EVENTS_KEY: &str = "seen-provider-events-v1";
const POLL_CURSOR_KEY: &str = "poll-cursor-v1";

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum ChannelRequest {
    Probe {
        account_id: String,
    },
    Poll {
        account_id: String,
        #[serde(default)]
        cursor: Option<String>,
    },
    Send {
        account_id: String,
        conversation_id: String,
        #[serde(default)]
        thread_id: Option<String>,
        text: String,
        idempotency_key: String,
    },
}

/// The durable webhook delivery shape. It has no `op`, which is how a handler
/// tells the two apart.
#[derive(Deserialize)]
struct WebhookDelivery {
    delivery_id: String,
    received_at_ms: i64,
    payload: WebhookPayload,
}

#[derive(Deserialize)]
struct WebhookPayload {
    account_id: String,
    conversation_id: String,
    sender_id: String,
    text: String,
    #[serde(default)]
    event_id: Option<String>,
}

#[derive(Serialize)]
struct Probe {
    ok: bool,
    identity: String,
}

/// One inbound message in the vocabulary the host normalizes from. A provider
/// has nowhere to put its own payload shape, which is deliberate: nothing
/// downstream should ever have to guess at provider-specific JSON.
#[derive(Serialize)]
struct Message {
    provider_event_id: String,
    conversation_id: String,
    conversation_kind: &'static str,
    sender_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sender_label: Option<String>,
    text: String,
    mentions_self: bool,
    received_at_ms: i64,
}

#[derive(Serialize)]
struct Inbound {
    #[serde(skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<String>,
}

#[derive(Serialize)]
struct SendResult {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_message_id: Option<String>,
}

struct MockChannel;

impl Guest for MockChannel {
    fn run(capability_id: String, input_json: String) -> Result<String, String> {
        require_capability(&capability_id, "room")?;
        // A webhook delivery is distinguished by shape, never by a field the
        // sender controls: it carries the host's own `delivery_id`.
        if let Ok(delivery) = serde_json::from_str::<WebhookDelivery>(&input_json) {
            return handle_delivery(delivery);
        }
        match parse_input::<ChannelRequest>(&input_json)? {
            ChannelRequest::Probe { account_id } => {
                validate_bounded_id("account id", &account_id)?;
                json_output(&Probe {
                    // A real provider replaces this with the identity call its
                    // API exposes, through `host::send_http`. Reporting `true`
                    // without asking is the one thing a probe must never do.
                    ok: true,
                    identity: format!("mock-provider:{account_id}"),
                })
            }
            ChannelRequest::Poll { account_id, cursor } => {
                validate_bounded_id("account id", &account_id)?;
                handle_poll(&account_id, cursor.as_deref())
            }
            ChannelRequest::Send {
                account_id,
                conversation_id,
                thread_id,
                text,
                idempotency_key,
            } => {
                validate_bounded_id("account id", &account_id)?;
                validate_bounded_id("conversation id", &conversation_id)?;
                if let Some(thread_id) = &thread_id {
                    validate_bounded_id("thread id", thread_id)?;
                }
                validate_bounded_id("idempotency key", &idempotency_key)?;
                validate_max_chars("text", &text, 16_000)?;
                handle_send(&conversation_id, &text, &idempotency_key)
            }
        }
    }
}

/// Normalize one durably-recorded webhook delivery.
///
/// The host has already authenticated the request, bounded it and written it
/// to the durable event log before this runs — see the extension trigger path.
/// All that is left is translation, and naming the account the messages belong
/// to. The host checks that account is one of this extension's.
fn handle_delivery(delivery: WebhookDelivery) -> Result<String, String> {
    validate_bounded_id("delivery id", &delivery.delivery_id)?;
    validate_bounded_id("account id", &delivery.payload.account_id)?;
    validate_bounded_id("conversation id", &delivery.payload.conversation_id)?;
    validate_bounded_id("sender id", &delivery.payload.sender_id)?;
    validate_max_chars("text", &delivery.payload.text, 16_000)?;

    // A stable event id, never a random one: the host's dedupe key is the
    // account plus this value, so a redelivery has to produce the same string.
    let provider_event_id = delivery
        .payload
        .event_id
        .unwrap_or_else(|| format!("delivery-{}", delivery.delivery_id));
    validate_bounded_id("provider event id", &provider_event_id)?;

    json_output(&Inbound {
        account_id: Some(delivery.payload.account_id),
        messages: vec![Message {
            provider_event_id,
            conversation_id: delivery.payload.conversation_id,
            conversation_kind: "group",
            sender_id: delivery.payload.sender_id,
            sender_label: None,
            text: delivery.payload.text,
            mentions_self: true,
            received_at_ms: delivery.received_at_ms,
        }],
        cursor: None,
    })
}

/// Return anything the provider has that the stored cursor has not covered.
///
/// A real provider replaces the body with the `host::send_http` call its API
/// exposes, passing `cursor` as that API's resume token. The dedupe bookkeeping
/// around it stays exactly as written.
fn handle_poll(account_id: &str, cursor: Option<&str>) -> Result<String, String> {
    let mut seen = load_ids(SEEN_EVENTS_KEY)?;
    let stored_cursor = host::state_get(POLL_CURSOR_KEY)?
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default();
    let resume_from = cursor.unwrap_or(&stored_cursor);

    // The offline stand-in for "what the provider had": one message the first
    // time this account is polled, nothing after.
    let mut messages = Vec::new();
    if resume_from.is_empty() {
        let provider_event_id = format!("{account_id}-welcome-1");
        if !seen.iter().any(|id| id == &provider_event_id) {
            messages.push(Message {
                provider_event_id: provider_event_id.clone(),
                conversation_id: "general".to_string(),
                conversation_kind: "group",
                sender_id: "operator".to_string(),
                sender_label: Some("Operator".to_string()),
                text: "Hello from the mock channel example.".to_string(),
                mentions_self: true,
                received_at_ms: host::now_ms() as i64,
            });
            seen.push(provider_event_id);
        }
    }

    let next_cursor = "1".to_string();
    host::state_put(POLL_CURSOR_KEY, next_cursor.as_bytes())?;
    store_ids(SEEN_EVENTS_KEY, &mut seen)?;
    json_output(&Inbound {
        account_id: None,
        messages,
        cursor: Some(next_cursor),
    })
}

/// Deliver one message, and be exact about what is known afterwards.
///
/// The three outcomes are not interchangeable. `sent` retires the outbox row;
/// `retry` re-attempts it; anything else parks it for reconciliation. A
/// provider that cannot prove its request never left must not say `retry`,
/// because a retry of a request that did arrive sends the message twice.
fn handle_send(
    conversation_id: &str,
    text: &str,
    idempotency_key: &str,
) -> Result<String, String> {
    host::log(
        "info",
        &format!("sending {} chars to {conversation_id}", text.len()),
    )?;
    // A real provider passes `idempotency_key` to whatever idempotency header
    // its API accepts, so a retried row can never become two messages.
    json_output(&SendResult {
        status: "sent",
        provider_message_id: Some(format!("mock-{idempotency_key}")),
    })
}

fn load_ids(key: &str) -> Result<Vec<String>, String> {
    Ok(host::state_get(key)?
        .map(|bytes| {
            serde_json::from_slice::<Vec<String>>(&bytes)
                .map_err(|error| format!("invalid stored ids: {error}"))
        })
        .transpose()?
        .unwrap_or_default())
}

fn store_ids(key: &str, ids: &mut Vec<String>) -> Result<(), String> {
    if ids.len() > MAX_REMEMBERED_EVENTS {
        ids.drain(..ids.len() - MAX_REMEMBERED_EVENTS);
    }
    let bytes =
        serde_json::to_vec(ids).map_err(|error| format!("cannot encode stored ids: {error}"))?;
    host::state_put(key, &bytes)
}

bindings::export!(MockChannel with_types_in bindings);
