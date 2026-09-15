//! Home Assistant adapter: the operator's own instance — inbound over the
//! WebSocket API, outbound through the REST notify service.
//!
//! # Why the WebSocket API and not the webhook trigger
//!
//! Home Assistant can also drive a webhook trigger, but that needs a publicly
//! reachable callback through `callback_exposure`, a verification challenge and
//! Security Doctor's whole set of callback questions. `/api/websocket` is an
//! *outbound* connection from this machine and needs none of them, so it is
//! what [`inbound_transport_for`](super::inbound_transport_for) classifies this
//! kind as: `Socket`. One configured event type is subscribed to, and every
//! frame of any other type is ignored.
//!
//! One task (started lazily on the first `poll` or `probe`, never in `new`)
//! owns that connection: it authenticates, subscribes, normalizes matching
//! event frames and pushes them into a bounded channel `poll` drains.
//! [`handle_socket_frame`] is the pure part of it — given one text frame and
//! the handshake state, what to send, what to deliver, and when the connection
//! has actually earned `Connected`.
//!
//! # Trust boundary
//!
//! The long-lived access token lives in the OS keychain and reaches this
//! adapter already resolved; it is never in the account's configuration JSON.
//! `base_url` is pinned the way Mattermost's is — a bare origin, `https` unless
//! the host is loopback — because a bearer token goes out on every request made
//! to whatever that string names, and the WebSocket URL is derived from that
//! validated origin alone rather than from anything a frame carries.
//! `notify_service` is concatenated into a REST path and `event_type` into a
//! subscription, so both are validated as bare identifiers rather than merely
//! non-empty.
//!
//! Everything an event carries is untrusted operator-automation payload: the
//! sender is never `is_self` however the payload spells it, and text,
//! conversation and user are bounded before they become an envelope. This
//! adapter decides nothing about access: `channel_ingress` does, for this
//! provider exactly as for every other.
//!
//! # What it deliberately does not do
//!
//! A Home Assistant notify service has no upload and no per-recipient address:
//! a reply goes wherever that one service is configured to go — the
//! conversation that caused the run does not steer it — and no file can ride
//! along. `sends_attachments(HomeAssistant)` is therefore `false`, and `send`
//! refuses a message carrying a file before it makes any request rather than
//! delivering a reply whose attachment silently vanished.
//!
//! `notify` answers with no message id, so [`SendOutcome::Sent`] here carries
//! `provider_message_id: None`. Inventing one would put a value into the
//! outbound echo ledger that no inbound event can ever match.
//!
//! There is no chunking either. A notify service cannot be split into two
//! notifications that stay in order, so a reply longer than
//! `HOME_ASSISTANT_MAX_TEXT_CHARS` arrives cut down to that ceiling with a
//! visible `[truncated]` marker rather than silently shortened or split.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use little_monkey_lib::channels::types::{
    BoundedMetadata, ChannelConversation, ChannelEnvelope, ChannelHealth, ChannelKind,
    ChannelSender, HealthState, InboundTransport, OutboundMessage, ProviderCapabilities,
    SendOutcome,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

use crate::daemon::channel_adapter::{
    AdapterConfig, ChannelAdapter, InboundBatch, TransportStatus,
};

/// The event type an account subscribes to when it names none. A Home
/// Assistant automation fires this with `event_type: little_monkey_message`.
const DEFAULT_EVENT_TYPE: &str = "little_monkey_message";

/// Home Assistant imposes no message length on `notify`; this is the host's own
/// ceiling, chosen to be comfortably under what a push transport behind a
/// notify service will carry. Nothing outside this file reads
/// `ProviderCapabilities::max_text_chars` — there is no worker-side chunker —
/// so it is a declaration to the agent's tool schema and the setup UI, and
/// `send` is what has to honour it. It does, by truncating with a visible
/// marker: a notify service cannot be split into two messages that stay in
/// order, and a silently shortened reply is worse than one that says so.
const HOME_ASSISTANT_MAX_TEXT_CHARS: usize = 4_000;
/// Appended when `send` has to cut a reply down to the ceiling above.
const TRUNCATION_MARKER: &str = "… [truncated]";
/// Bound on the free-form strings an event may name a conversation or a person
/// with. Both become durable keys, so an automation cannot grow the database a
/// row at a time with a megabyte of `conversation_id`.
const MAX_IDENTIFIER_CHARS: usize = 128;

const INBOUND_CHANNEL_CAPACITY: usize = 256;
const POLL_WAIT: Duration = Duration::from_secs(20);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A connection that stayed up at least this long counts as recovered, so the
/// next drop backs off from the beginning. Measured on the connection's own
/// lifetime, so a server that accepts and immediately closes cannot reset the
/// ladder into a tight loop.
const STABLE_AFTER: Duration = Duration::from_secs(30);
/// The one request id this adapter ever sends on a connection: it makes exactly
/// one subscription and never unsubscribes.
const SUBSCRIBE_ID: u64 = 1;

/// A bare origin, and `https` unless the host is this machine.
///
/// Same rule and same reason as Mattermost's: a long-lived access token is
/// attached to every request this adapter makes to whatever `base_url` names,
/// so a plain-`http` remote host would walk it across the network in the clear.
/// A path or a query string is refused because the adapter appends its own.
pub(crate) fn validate_base_url(raw: &str) -> Result<String, String> {
    let parsed = url::Url::parse(raw)
        .map_err(|_| "Home Assistant base_url is not a valid URL".to_string())?;
    if !matches!(parsed.path(), "" | "/") {
        return Err("Home Assistant base_url must not include a path".to_string());
    }
    if parsed.query().is_some() {
        return Err("Home Assistant base_url must not include a query string".to_string());
    }
    match parsed.scheme() {
        "https" => {}
        "http" => {
            let is_local = matches!(
                parsed.host_str(),
                Some("localhost") | Some("127.0.0.1") | Some("::1")
            );
            if !is_local {
                return Err(
                    "Home Assistant base_url must be https (plain http is only accepted for \
                     localhost). A stock http://homeassistant.local:8123 install has to be put \
                     behind TLS first, or the long-lived access token rides your network in the \
                     clear."
                        .to_string(),
                );
            }
        }
        _ => return Err("Home Assistant base_url must use http or https".to_string()),
    }
    Ok(raw.trim_end_matches('/').to_string())
}

/// A bare Home Assistant identifier: lowercase letters, digits, underscore.
///
/// Applied to `notify_service`, which is concatenated into
/// `/api/services/notify/<service>`, and to `event_type`, which names the
/// subscription. Refusing `/` and `.` here is what keeps a configuration string
/// from selecting a different endpoint.
pub(crate) fn validate_identifier(kind: &str, raw: &str) -> Result<String, String> {
    let value = raw.trim();
    if value.is_empty() || value.len() > 128 {
        return Err(format!("Home Assistant {kind} must be 1-128 characters"));
    }
    if !value.chars().all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
    }) {
        return Err(format!(
            "Home Assistant {kind} must be a bare name of lowercase letters, digits and \
             underscores (got '{value}')"
        ));
    }
    Ok(value.to_string())
}

/// The socket URL, built from the validated origin and nothing else.
///
/// Deliberately a function of `base_url` alone: Home Assistant frames carry
/// hosts and URLs of their own, and none of them may ever decide where the
/// token is sent.
pub(crate) fn websocket_url(base_url: &str) -> String {
    let ws_base = if let Some(rest) = base_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base_url.to_string()
    };
    format!("{ws_base}/api/websocket")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn scrub(text: &str, token: &str) -> String {
    if token.is_empty() {
        text.to_string()
    } else {
        text.replace(token, "[redacted]")
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect()
}

#[derive(Default)]
struct Shared {
    /// Set once when the instance has answered in a way retrying cannot fix —
    /// a rejected token, or a refused subscription. The loop stops instead of
    /// hammering an instance that has already said no.
    permanent_error: Mutex<Option<String>>,
    /// What the socket is actually doing. A quiet `poll` cannot report this:
    /// it returns an empty batch whether the connection is live or gone.
    status: TransportStatus,
    /// Events normalized but never handed on, because `poll` fell far enough
    /// behind to fill the queue. Counted rather than waited on — the reader is
    /// also what answers the server's pings.
    dropped: std::sync::atomic::AtomicU64,
}

pub struct HomeAssistantAdapter {
    account_id: String,
    base_url: String,
    notify_service: String,
    event_type: String,
    token: String,
    http: reqwest::Client,
    inbound_tx: mpsc::Sender<ChannelEnvelope>,
    inbound_rx: Mutex<mpsc::Receiver<ChannelEnvelope>>,
    shared: Arc<Shared>,
    /// Guards the one-time spawn of the socket task, so `new` itself stays
    /// side-effect-free and can be called outside a runtime.
    started: tokio::sync::OnceCell<()>,
}

impl HomeAssistantAdapter {
    pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
        let setting = |key: &str| {
            config
                .account
                .non_secret_config
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        let base_url = validate_base_url(
            &setting("base_url")
                .ok_or_else(|| "Home Assistant configuration requires 'base_url'".to_string())?,
        )?;
        let notify_service = validate_identifier(
            "notify_service",
            &setting("notify_service").ok_or_else(|| {
                "Home Assistant configuration requires 'notify_service'".to_string()
            })?,
        )?;
        let event_type = validate_identifier(
            "event_type",
            &setting("event_type").unwrap_or_else(|| DEFAULT_EVENT_TYPE.to_string()),
        )?;
        let token = config.secret.trim().to_string();
        if token.is_empty() {
            return Err(
                "Home Assistant needs a long-lived access token; store it with \
                 `monkey channels set-token`"
                    .to_string(),
            );
        }
        let http = little_monkey_lib::egress::hardened()
            .build()
            .map_err(|error| format!("Failed to build the Home Assistant HTTP client: {error}"))?;
        let (tx, rx) = mpsc::channel(INBOUND_CHANNEL_CAPACITY);
        Ok(Self {
            account_id: config.account.account_id.clone(),
            base_url,
            notify_service,
            event_type,
            token,
            http,
            inbound_tx: tx,
            inbound_rx: Mutex::new(rx),
            shared: Arc::new(Shared::default()),
            started: tokio::sync::OnceCell::new(),
        })
    }

    async fn ensure_started(&self) {
        self.started
            .get_or_init(|| async {
                tokio::spawn(run_socket_loop(
                    self.account_id.clone(),
                    self.token.clone(),
                    self.base_url.clone(),
                    self.event_type.clone(),
                    self.inbound_tx.clone(),
                    self.shared.clone(),
                ));
            })
            .await;
    }
}

#[async_trait]
impl ChannelAdapter for HomeAssistantAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::HomeAssistant
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            max_text_chars: HOME_ASSISTANT_MAX_TEXT_CHARS,
            ..ProviderCapabilities::minimal(ChannelKind::HomeAssistant, InboundTransport::Socket)
        }
    }

    /// The socket's own state, which a quiet poll cannot report.
    fn live_transport(&self) -> Option<HealthState> {
        Some(self.shared.status.get())
    }

    async fn probe(&self) -> ChannelHealth {
        self.ensure_started().await;
        let now = now_ms();
        if let Some(error) = self.shared.permanent_error.lock().await.clone() {
            return ChannelHealth::error(now, error);
        }
        let request = self
            .http
            .get(format!("{}/api/", self.base_url))
            .header("Authorization", format!("Bearer {}", self.token));
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => {
                return ChannelHealth::error(
                    now,
                    scrub(
                        &format!("Home Assistant probe failed: {error}"),
                        &self.token,
                    ),
                )
            }
        };
        let status = response.status().as_u16();
        if status == 401 || status == 403 {
            return ChannelHealth::error(
                now,
                format!(
                    "Home Assistant rejected the long-lived access token (HTTP {status}); replace \
                     it with `monkey channels set-token`"
                ),
            );
        }
        if !(200..=299).contains(&status) {
            return ChannelHealth::degraded(
                now,
                format!("Home Assistant answered /api/ with HTTP {status}"),
            );
        }
        let dropped = self
            .shared
            .dropped
            .load(std::sync::atomic::Ordering::Relaxed);
        if dropped > 0 {
            return ChannelHealth::degraded(
                now,
                format!("Authenticated to Home Assistant · {dropped} event(s) dropped under load"),
            );
        }
        // The token working is half the story. Inbound arrives on the socket,
        // so an account whose token is fine but whose subscription is down
        // receives nothing, and calling that Connected is exactly the false
        // positive the health column exists to prevent.
        match self.shared.status.get() {
            HealthState::Connected => ChannelHealth::connected(
                now,
                Some(format!(
                    "Subscribed to Home Assistant event '{}'",
                    self.event_type
                )),
            ),
            HealthState::Connecting => ChannelHealth::connecting(
                now,
                Some("Authenticated to Home Assistant · opening the WebSocket".to_string()),
            ),
            _ => ChannelHealth::degraded(
                now,
                "Authenticated to Home Assistant · the WebSocket is down, so no event can arrive"
                    .to_string(),
            ),
        }
    }

    async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
        // Socket transport: the subscription is the resume token, so there is
        // no page or offset for a cursor to carry.
        self.ensure_started().await;
        let mut rx = self.inbound_rx.lock().await;
        let mut envelopes = Vec::new();
        match tokio::time::timeout(POLL_WAIT, rx.recv()).await {
            Ok(Some(envelope)) => {
                envelopes.push(envelope);
                while let Ok(next) = rx.try_recv() {
                    envelopes.push(next);
                }
            }
            Ok(None) => {
                if let Some(error) = self.shared.permanent_error.lock().await.clone() {
                    return Err(error);
                }
            }
            Err(_) => {}
        }
        Ok(InboundBatch {
            envelopes,
            cursor: None,
        })
    }

    async fn send(&self, message: &OutboundMessage) -> SendOutcome {
        // Refused before any request: a notify service has no upload, so
        // queueing this would deliver a reply whose file silently vanished.
        if !message.attachments.is_empty() {
            return SendOutcome::PermanentFailure {
                error: "A Home Assistant notify service cannot carry a file, so this reply was \
                        not sent at all rather than sent without its attachment"
                    .to_string(),
            };
        }
        let text = if message.text.chars().count() > HOME_ASSISTANT_MAX_TEXT_CHARS {
            format!(
                "{}{TRUNCATION_MARKER}",
                truncate_chars(
                    &message.text,
                    HOME_ASSISTANT_MAX_TEXT_CHARS - TRUNCATION_MARKER.chars().count()
                )
            )
        } else {
            message.text.clone()
        };
        let request = self
            .http
            .post(format!(
                "{}/api/services/notify/{}",
                self.base_url, self.notify_service
            ))
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&serde_json::json!({ "message": text, "title": "Little Monkey" }));
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => {
                let rendered = scrub(&error.to_string(), &self.token);
                return match certainty_of(&error) {
                    DeliveryCertainty::DefinitelyNotSent => SendOutcome::RetryableFailure {
                        error: rendered,
                        retry_after_ms: None,
                    },
                    DeliveryCertainty::PossiblySent => {
                        SendOutcome::NeedsReconciliation { error: rendered }
                    }
                };
            }
        };
        let status = response.status().as_u16();
        let retry_after_ms = if status == 429 {
            parse_retry_after_seconds(
                response
                    .headers()
                    .get("Retry-After")
                    .and_then(|value| value.to_str().ok()),
            )
        } else {
            None
        };
        match map_send_status(status, &self.notify_service, retry_after_ms) {
            Some(outcome) => outcome,
            // A notify service answers with no id of its own, and inventing one
            // would put a value in the echo ledger no inbound event can match.
            None => SendOutcome::Sent {
                provider_message_id: None,
            },
        }
    }
}

/// Whether a failed outbound HTTP call can have reached Home Assistant.
///
/// `RetryableFailure` re-sends the same message, so it may only be used when it
/// is provable the instance never saw the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryCertainty {
    DefinitelyNotSent,
    PossiblySent,
}

/// Classify one `reqwest` failure. Only a builder error (never handed to the
/// client) and a connect error (raised by the connector, before any request
/// byte reaches a socket) prove nothing was sent. A timeout does not: reqwest's
/// timeout covers the response too, so the notification may already have gone
/// out to a phone.
fn certainty_of(error: &reqwest::Error) -> DeliveryCertainty {
    if error.is_body() {
        return DeliveryCertainty::PossiblySent;
    }
    if error.is_builder() || error.is_connect() {
        return DeliveryCertainty::DefinitelyNotSent;
    }
    DeliveryCertainty::PossiblySent
}

fn parse_retry_after_seconds(header_value: Option<&str>) -> Option<i64> {
    let seconds: f64 = header_value?.parse().ok()?;
    Some((seconds * 1000.0).round() as i64)
}

/// One HTTP status as an outcome, or `None` when it was a success.
///
/// Unlike Mattermost there is no partial-send term: one queued reply is exactly
/// one `notify` call, so nothing can have been half-accepted.
fn map_send_status(status: u16, service: &str, retry_after_ms: Option<i64>) -> Option<SendOutcome> {
    if (200..=299).contains(&status) {
        return None;
    }
    Some(match status {
        429 => SendOutcome::RetryableFailure {
            error: "Home Assistant rate limited the notify call".to_string(),
            retry_after_ms,
        },
        401 | 403 => SendOutcome::PermanentFailure {
            error: format!(
                "Home Assistant rejected the long-lived access token (HTTP {status}); replace it \
                 with `monkey channels set-token`"
            ),
        },
        404 => SendOutcome::PermanentFailure {
            error: format!("Home Assistant has no notify service called '{service}' (HTTP 404)"),
        },
        500..=599 => SendOutcome::RetryableFailure {
            error: format!("Home Assistant returned HTTP {status}"),
            retry_after_ms: None,
        },
        _ => SendOutcome::PermanentFailure {
            error: format!("Home Assistant rejected the notification: HTTP {status}"),
        },
    })
}

// ---------------------------------------------------------------------------
// WebSocket framing (pure)
// ---------------------------------------------------------------------------

/// Where one connection is in Home Assistant's `auth_required` -> `auth` ->
/// `auth_ok` -> `subscribe_events` -> `result` handshake.
///
/// An `event` frame is only ever normalized in [`Handshake::Subscribed`], which
/// is what makes "authenticates before it subscribes" a property of the code
/// rather than of the order a server happens to send things in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Handshake {
    #[default]
    AwaitingAuthRequired,
    AwaitingAuthOk,
    AwaitingSubscribeResult,
    Subscribed,
}

#[derive(Debug, PartialEq)]
enum SocketAction {
    /// A frame to write back.
    Send(String),
    /// The subscription is live: this, and only this, may set `Connected`.
    Subscribed,
    Envelope(Box<ChannelEnvelope>),
    /// The instance answered in a way retrying will not fix.
    Fatal(String),
}

/// Handles one WebSocket text frame against the handshake state.
///
/// Every unexpected frame — a `pong`, a `result` for another id, an `event`
/// before the subscription is live, an `auth_ok` that arrives twice — produces
/// nothing at all.
fn handle_socket_frame(
    state: &mut Handshake,
    account_id: &str,
    token: &str,
    event_type: &str,
    text: &str,
    now_ms: i64,
) -> Vec<SocketAction> {
    let Ok(frame) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let frame_type = frame
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match (*state, frame_type) {
        (Handshake::AwaitingAuthRequired, "auth_required") => {
            *state = Handshake::AwaitingAuthOk;
            vec![SocketAction::Send(
                serde_json::json!({ "type": "auth", "access_token": token }).to_string(),
            )]
        }
        (Handshake::AwaitingAuthOk, "auth_ok") => {
            *state = Handshake::AwaitingSubscribeResult;
            vec![SocketAction::Send(
                serde_json::json!({
                    "id": SUBSCRIBE_ID,
                    "type": "subscribe_events",
                    "event_type": event_type,
                })
                .to_string(),
            )]
        }
        (Handshake::AwaitingAuthOk, "auth_invalid") => vec![SocketAction::Fatal(
            "Home Assistant rejected the long-lived access token; replace it with \
             `monkey channels set-token`"
                .to_string(),
        )],
        (Handshake::AwaitingSubscribeResult, "result") => {
            if frame.get("id").and_then(Value::as_u64) != Some(SUBSCRIBE_ID) {
                return Vec::new();
            }
            if frame.get("success").and_then(Value::as_bool) == Some(true) {
                *state = Handshake::Subscribed;
                vec![SocketAction::Subscribed]
            } else {
                // The instance's own error text is untrusted payload and is not
                // repeated here; the configured name is ours and is enough to
                // act on.
                vec![SocketAction::Fatal(format!(
                    "Home Assistant refused a subscription to event type '{event_type}'"
                ))]
            }
        }
        (Handshake::Subscribed, "event") => {
            match normalize_event(&frame, account_id, event_type, now_ms) {
                Some(envelope) => vec![SocketAction::Envelope(Box::new(envelope))],
                None => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

/// One subscribed Home Assistant event as a channel envelope, or nothing.
///
/// Everything below `event.data` is written by an operator's automation and is
/// treated as such: `text` is required and bounded, `conversation_id` and
/// `user` are bounded with defaults, and `is_self` is hard-coded false — a
/// payload may not claim to be this account, or it could suppress its own
/// message as an echo, or worse, be believed about who it is.
pub(crate) fn normalize_event(
    frame: &Value,
    account_id: &str,
    expected_event_type: &str,
    now_ms: i64,
) -> Option<ChannelEnvelope> {
    if frame.get("type").and_then(Value::as_str) != Some("event") {
        return None;
    }
    let event = frame.get("event")?;
    if event.get("event_type").and_then(Value::as_str) != Some(expected_event_type) {
        return None;
    }
    let data = event.get("data")?;
    let text = data.get("text").and_then(Value::as_str)?.trim();
    if text.is_empty() {
        return None;
    }
    let text = truncate_chars(text, HOME_ASSISTANT_MAX_TEXT_CHARS);

    let bounded = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty() && value.chars().count() <= MAX_IDENTIFIER_CHARS)
            .map(str::to_string)
    };
    let conversation_id =
        bounded("conversation_id").unwrap_or_else(|| "home_assistant".to_string());
    let sender_id = bounded("user").unwrap_or_else(|| "home_assistant".to_string());

    let time_fired = event
        .get("time_fired")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let provider_event_id = event
        .get("context")
        .and_then(|context| context.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            deterministic_event_id(account_id, expected_event_type, time_fired, &text, data)
        });

    let mut metadata = BoundedMetadata::new();
    metadata.insert("event_type", expected_event_type);
    if !time_fired.is_empty() {
        metadata.insert("time_fired", time_fired);
    }

    Some(ChannelEnvelope {
        account_id: account_id.to_string(),
        kind: ChannelKind::HomeAssistant,
        provider_event_id,
        provider_message_id: None,
        conversation: ChannelConversation::direct(conversation_id),
        sender: ChannelSender::new(sender_id),
        text,
        attachments: Vec::new(),
        reply_to_provider_id: None,
        mentions_self: false,
        received_at_ms: now_ms,
        metadata,
    })
}

/// A deterministic dedupe key for an event that carries no `context.id`.
///
/// `channel_ingress` dedupes on `provider_event_id`, so this must never be
/// random: the same event seen twice has to collide. It is hashed over the
/// account, the event type, the fire time and the whole `data` object, so two
/// genuinely different events differ. Two byte-identical events fired in the
/// same instant do collide, and that is the honest direction to be wrong in —
/// an automation that fires the same payload twice is far more likely to be a
/// redelivery than two things a person said.
fn deterministic_event_id(
    account_id: &str,
    event_type: &str,
    time_fired: &str,
    text: &str,
    data: &Value,
) -> String {
    let mut hasher = Sha256::new();
    for part in [account_id, event_type, time_fired, text] {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(data.to_string().as_bytes());
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// I/O loop
// ---------------------------------------------------------------------------

/// Why one connection ended.
enum ConnectionEnd {
    /// Dropped, refused or never opened — worth coming back to.
    Dropped,
    /// The instance said no in a way a reconnect cannot change.
    Fatal(String),
}

async fn run_socket_loop(
    account_id: String,
    token: String,
    base_url: String,
    event_type: String,
    tx: mpsc::Sender<ChannelEnvelope>,
    shared: Arc<Shared>,
) {
    let mut backoff = MIN_BACKOFF;
    loop {
        let started = std::time::Instant::now();
        match run_one_connection(&account_id, &base_url, &token, &event_type, &tx, &shared).await {
            ConnectionEnd::Fatal(error) => {
                shared.status.set(HealthState::Error);
                *shared.permanent_error.lock().await = Some(error);
                return;
            }
            ConnectionEnd::Dropped => shared.status.set(HealthState::Degraded),
        }
        if tx.is_closed() {
            return;
        }
        if started.elapsed() >= STABLE_AFTER {
            backoff = MIN_BACKOFF;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Hand one normalized event to `poll`, or count it as lost.
///
/// Never `send().await`: the caller is the socket reader, and the reader is
/// also what answers Home Assistant's pings. An overflow is dropped, counted
/// and surfaced as degraded — the one thing it must never be is silently
/// treated as delivered.
fn deliver(shared: &Arc<Shared>, tx: &mpsc::Sender<ChannelEnvelope>, envelope: ChannelEnvelope) {
    if tx.try_send(envelope).is_err() {
        shared
            .dropped
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        shared.status.set(HealthState::Degraded);
    }
}

async fn run_one_connection(
    account_id: &str,
    base_url: &str,
    token: &str,
    event_type: &str,
    tx: &mpsc::Sender<ChannelEnvelope>,
    shared: &Arc<Shared>,
) -> ConnectionEnd {
    let (mut ws, _) = match tokio_tungstenite::connect_async(websocket_url(base_url)).await {
        Ok(pair) => pair,
        Err(_) => return ConnectionEnd::Dropped,
    };
    let mut state = Handshake::default();
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => {
                for action in
                    handle_socket_frame(&mut state, account_id, token, event_type, &text, now_ms())
                {
                    match action {
                        SocketAction::Send(frame) => {
                            if ws.send(Message::Text(frame.into())).await.is_err() {
                                return ConnectionEnd::Dropped;
                            }
                        }
                        // Only a successful subscribe result reaches here, so
                        // Connected cannot be claimed by an authenticated
                        // socket that is subscribed to nothing.
                        SocketAction::Subscribed => shared.status.set(HealthState::Connected),
                        SocketAction::Envelope(envelope) => deliver(shared, tx, *envelope),
                        SocketAction::Fatal(error) => return ConnectionEnd::Fatal(error),
                    }
                }
            }
            Some(Ok(Message::Ping(payload))) => {
                let _ = ws.send(Message::Pong(payload)).await;
            }
            Some(Ok(Message::Close(_))) => return ConnectionEnd::Dropped,
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return ConnectionEnd::Dropped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // -----------------------------------------------------------------------
    // Recorded frames. These are the shapes a real instance sends on
    // `/api/websocket`; nothing in this module ever reaches the network.
    // -----------------------------------------------------------------------

    const AUTH_REQUIRED: &str = r#"{"type":"auth_required","ha_version":"2026.8.1"}"#;
    const AUTH_OK: &str = r#"{"type":"auth_ok","ha_version":"2026.8.1"}"#;
    const AUTH_INVALID: &str =
        r#"{"type":"auth_invalid","message":"Invalid access token or password"}"#;
    const SUBSCRIBE_OK: &str = r#"{"id":1,"type":"result","success":true,"result":null}"#;
    const SUBSCRIBE_REFUSED: &str = r#"{"id":1,"type":"result","success":false,"error":{"code":"invalid_format","message":"nope"}}"#;
    const PONG: &str = r#"{"id":7,"type":"pong"}"#;

    fn event_frame() -> Value {
        serde_json::json!({
            "id": 1,
            "type": "event",
            "event": {
                "event_type": "little_monkey_message",
                "data": {
                    "text": "is the garage door still open?",
                    "conversation_id": "kitchen_tablet",
                    "user": "ada"
                },
                "origin": "LOCAL",
                "time_fired": "2026-09-03T09:14:02.117321+00:00",
                "context": {
                    "id": "01JZ6QK8Y0S9F2N4V7B3D5H1M6",
                    "parent_id": null,
                    "user_id": null
                }
            }
        })
    }

    fn account_fixture(base_url: &str) -> crate::daemon::channel_store::ChannelAccountRecord {
        crate::daemon::channel_store::ChannelAccountRecord {
            account_id: "acct-ha".to_string(),
            kind: ChannelKind::HomeAssistant,
            label: "House".to_string(),
            enabled: true,
            non_secret_config: serde_json::json!({
                "base_url": base_url,
                "notify_service": "persistent_notification",
            }),
            credential_ref: Some("home_assistant/acct-ha".to_string()),
            access_policy: Default::default(),
            health: ChannelHealth::error(0, "unused"),
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn adapter_for(base_url: &str) -> HomeAssistantAdapter {
        let account = account_fixture(base_url);
        HomeAssistantAdapter::new(&AdapterConfig {
            account: &account,
            secret: "llat-secret-token".to_string(),
        })
        .expect("adapter")
    }

    fn outbound(text: &str) -> OutboundMessage {
        OutboundMessage {
            account_id: "acct-ha".to_string(),
            kind: ChannelKind::HomeAssistant,
            conversation_id: "kitchen_tablet".to_string(),
            thread_id: None,
            text: text.to_string(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            idempotency_key: "key-1".to_string(),
        }
    }

    // -- configuration --------------------------------------------------------

    #[test]
    fn a_base_url_that_could_walk_the_token_somewhere_else_is_refused() {
        for raw in [
            "https://ha.example.org/api",
            "https://ha.example.org/?token=x",
            "http://192.168.1.9:8123",
            "http://homeassistant.local:8123",
            "ftp://ha.example.org",
        ] {
            assert!(validate_base_url(raw).is_err(), "{raw}");
        }
        assert_eq!(
            validate_base_url("https://ha.example.org/").expect("https"),
            "https://ha.example.org"
        );
        // Loopback is the one place plain http cannot leave the machine.
        assert!(validate_base_url("http://localhost:8123").is_ok());
    }

    #[test]
    fn a_service_name_cannot_select_a_different_endpoint() {
        // `notify_service` is concatenated into `/api/services/notify/<it>`.
        for raw in [
            "notify/persistent_notification",
            "../states",
            "mobile_app.pixel",
            "Mobile_App",
            "",
        ] {
            assert!(validate_identifier("notify_service", raw).is_err(), "{raw}");
        }
        assert_eq!(
            validate_identifier("notify_service", " mobile_app_pixel ").expect("bare name"),
            "mobile_app_pixel"
        );
    }

    #[test]
    fn the_websocket_url_is_built_from_the_validated_origin_only() {
        assert_eq!(
            websocket_url("https://ha.example.org"),
            "wss://ha.example.org/api/websocket"
        );
        // The port an operator actually runs on survives, and loopback http
        // stays ws rather than being silently upgraded to something that will
        // not connect.
        assert_eq!(
            websocket_url("http://localhost:8123"),
            "ws://localhost:8123/api/websocket"
        );
    }

    #[test]
    fn a_missing_token_is_refused_at_construction() {
        let account = account_fixture("https://ha.example.org");
        assert!(HomeAssistantAdapter::new(&AdapterConfig {
            account: &account,
            secret: "   ".to_string(),
        })
        .is_err());
    }

    #[test]
    fn construction_does_not_start_the_socket_task() {
        // Plain #[test], no tokio runtime: `new` spawning anything would panic
        // here, so a passing test proves it does not.
        let adapter = adapter_for("https://ha.example.org");
        assert_eq!(adapter.event_type, DEFAULT_EVENT_TYPE);
        assert_eq!(adapter.notify_service, "persistent_notification");
    }

    // -- normalization --------------------------------------------------------

    #[test]
    fn a_recorded_event_becomes_the_expected_envelope() {
        let envelope =
            normalize_event(&event_frame(), "acct-ha", DEFAULT_EVENT_TYPE, 500).expect("envelope");
        assert_eq!(envelope.kind, ChannelKind::HomeAssistant);
        assert_eq!(envelope.provider_event_id, "01JZ6QK8Y0S9F2N4V7B3D5H1M6");
        assert_eq!(envelope.provider_message_id, None);
        assert_eq!(envelope.conversation.conversation_id, "kitchen_tablet");
        assert_eq!(
            envelope.conversation.kind,
            little_monkey_lib::channels::types::ConversationKind::Direct
        );
        assert_eq!(envelope.sender.sender_id, "ada");
        assert_eq!(envelope.text, "is the garage door still open?");
        assert!(envelope.attachments.is_empty());
        assert_eq!(envelope.received_at_ms, 500);
        assert_eq!(
            envelope.metadata.get("event_type"),
            Some(DEFAULT_EVENT_TYPE)
        );
    }

    #[test]
    fn an_event_of_another_type_is_ignored() {
        let mut frame = event_frame();
        frame["event"]["event_type"] = Value::String("state_changed".to_string());
        assert!(normalize_event(&frame, "acct-ha", DEFAULT_EVENT_TYPE, 500).is_none());
    }

    #[test]
    fn a_result_or_pong_frame_produces_nothing() {
        for raw in [SUBSCRIBE_OK, PONG, AUTH_OK] {
            let frame: Value = serde_json::from_str(raw).expect("json");
            assert!(
                normalize_event(&frame, "acct-ha", DEFAULT_EVENT_TYPE, 500).is_none(),
                "{raw}"
            );
        }
    }

    #[test]
    fn an_event_with_no_text_is_not_a_message() {
        for data in [
            serde_json::json!({ "conversation_id": "kitchen_tablet" }),
            serde_json::json!({ "text": "   " }),
            serde_json::json!({ "text": 42 }),
        ] {
            let mut frame = event_frame();
            frame["event"]["data"] = data.clone();
            assert!(
                normalize_event(&frame, "acct-ha", DEFAULT_EVENT_TYPE, 500).is_none(),
                "{data}"
            );
        }
    }

    #[test]
    fn a_payload_cannot_claim_to_be_us() {
        // An automation can write anything into `data`. If it could set
        // `is_self`, one event would suppress itself as our own echo — or, in
        // the other direction, be believed about who sent it.
        let mut frame = event_frame();
        frame["event"]["data"]["is_self"] = Value::Bool(true);
        frame["event"]["data"]["is_bot"] = Value::Bool(true);
        frame["event"]["data"]["mentions_self"] = Value::Bool(true);
        let envelope =
            normalize_event(&frame, "acct-ha", DEFAULT_EVENT_TYPE, 500).expect("envelope");
        assert!(!envelope.sender.is_self);
        assert!(!envelope.sender.is_bot);
        assert!(!envelope.mentions_self);

        // And the host is what decides self-echo for this provider at all.
        let adapter = adapter_for("https://ha.example.org");
        assert_eq!(
            adapter.capabilities().echo_correlation,
            little_monkey_lib::channels::policy::EchoCorrelation::HostAdapter
        );
    }

    #[test]
    fn an_event_with_no_context_id_gets_a_deterministic_event_id() {
        let mut frame = event_frame();
        frame["event"]["context"] = Value::Null;
        let first = normalize_event(&frame, "acct-ha", DEFAULT_EVENT_TYPE, 1).expect("envelope");
        let second = normalize_event(&frame, "acct-ha", DEFAULT_EVENT_TYPE, 999).expect("envelope");
        assert_eq!(
            first.provider_event_id, second.provider_event_id,
            "the same event seen twice has to dedupe"
        );
        assert_eq!(first.provider_event_id.len(), 64);
        assert!(first
            .provider_event_id
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
        // Not a UUID: a random id would defeat `channel_ingress`'s dedupe.
        assert!(!first.provider_event_id.contains('-'));

        let mut other = frame.clone();
        other["event"]["data"]["text"] = Value::String("something else".to_string());
        let different =
            normalize_event(&other, "acct-ha", DEFAULT_EVENT_TYPE, 1).expect("envelope");
        assert_ne!(first.provider_event_id, different.provider_event_id);
    }

    #[test]
    fn an_unbounded_identifier_falls_back_to_the_default() {
        let mut frame = event_frame();
        frame["event"]["data"]["conversation_id"] = Value::String("x".repeat(4_096));
        frame["event"]["data"]["user"] = Value::String(String::new());
        let envelope =
            normalize_event(&frame, "acct-ha", DEFAULT_EVENT_TYPE, 500).expect("envelope");
        assert_eq!(envelope.conversation.conversation_id, "home_assistant");
        assert_eq!(envelope.sender.sender_id, "home_assistant");
    }

    #[test]
    fn an_oversized_text_is_bounded_rather_than_stored_whole() {
        let mut frame = event_frame();
        frame["event"]["data"]["text"] =
            Value::String("y".repeat(HOME_ASSISTANT_MAX_TEXT_CHARS * 3));
        let envelope =
            normalize_event(&frame, "acct-ha", DEFAULT_EVENT_TYPE, 500).expect("envelope");
        assert_eq!(envelope.text.chars().count(), HOME_ASSISTANT_MAX_TEXT_CHARS);
    }

    // -- handshake ------------------------------------------------------------

    fn drive(state: &mut Handshake, frame: &str) -> Vec<SocketAction> {
        handle_socket_frame(
            state,
            "acct-ha",
            "llat-secret-token",
            DEFAULT_EVENT_TYPE,
            frame,
            500,
        )
    }

    #[test]
    fn the_socket_handshake_authenticates_before_it_subscribes() {
        let mut state = Handshake::default();

        // An event that arrives before the handshake is done is not a message:
        // nothing on this socket is trusted until the subscription is ours.
        assert!(drive(&mut state, &event_frame().to_string()).is_empty());

        let actions = drive(&mut state, AUTH_REQUIRED);
        let SocketAction::Send(auth) = &actions[0] else {
            panic!("expected an auth frame, got {actions:?}");
        };
        let auth: Value = serde_json::from_str(auth).expect("json");
        assert_eq!(auth["type"], "auth");
        assert_eq!(auth["access_token"], "llat-secret-token");

        // Authenticated but not yet subscribed: still nothing may be delivered
        // and nothing may claim Connected.
        let actions = drive(&mut state, AUTH_OK);
        let SocketAction::Send(subscribe) = &actions[0] else {
            panic!("expected a subscribe frame, got {actions:?}");
        };
        let subscribe: Value = serde_json::from_str(subscribe).expect("json");
        assert_eq!(subscribe["type"], "subscribe_events");
        assert_eq!(subscribe["event_type"], DEFAULT_EVENT_TYPE);
        assert_eq!(subscribe["id"], SUBSCRIBE_ID);
        assert!(drive(&mut state, &event_frame().to_string()).is_empty());

        assert_eq!(
            drive(&mut state, SUBSCRIBE_OK),
            vec![SocketAction::Subscribed]
        );
        let actions = drive(&mut state, &event_frame().to_string());
        assert!(matches!(actions.as_slice(), [SocketAction::Envelope(_)]));
    }

    #[test]
    fn a_result_for_another_request_does_not_open_the_subscription() {
        let mut state = Handshake::default();
        drive(&mut state, AUTH_REQUIRED);
        drive(&mut state, AUTH_OK);
        assert!(drive(&mut state, r#"{"id":9,"type":"result","success":true}"#).is_empty());
        assert!(drive(&mut state, &event_frame().to_string()).is_empty());
    }

    #[test]
    fn a_rejected_token_is_fatal_rather_than_retried() {
        let mut state = Handshake::default();
        drive(&mut state, AUTH_REQUIRED);
        let actions = drive(&mut state, AUTH_INVALID);
        assert!(matches!(actions.as_slice(), [SocketAction::Fatal(_)]));
    }

    #[test]
    fn a_refused_subscription_is_fatal_and_repeats_no_payload_text() {
        let mut state = Handshake::default();
        drive(&mut state, AUTH_REQUIRED);
        drive(&mut state, AUTH_OK);
        let actions = drive(&mut state, SUBSCRIBE_REFUSED);
        let [SocketAction::Fatal(error)] = actions.as_slice() else {
            panic!("expected a fatal, got {actions:?}");
        };
        assert!(error.contains(DEFAULT_EVENT_TYPE));
        assert!(
            !error.contains("nope"),
            "the instance's own error text is untrusted payload: {error}"
        );
    }

    #[test]
    fn a_garbled_frame_is_ignored() {
        let mut state = Handshake::default();
        assert!(drive(&mut state, "not json at all").is_empty());
        assert_eq!(state, Handshake::AwaitingAuthRequired);
    }

    #[test]
    fn no_token_appears_in_any_rendered_error_string() {
        let token = "llat-secret-token";
        let mut state = Handshake::default();
        drive(&mut state, AUTH_REQUIRED);
        let mut rendered = vec![scrub(&format!("connect to {token} failed"), token)];
        for actions in [drive(&mut state, AUTH_INVALID)] {
            for action in actions {
                if let SocketAction::Fatal(error) = action {
                    rendered.push(error);
                }
            }
        }
        for status in [401, 403, 404, 429, 500, 418] {
            if let Some(outcome) = map_send_status(status, "persistent_notification", Some(1_000)) {
                rendered.push(format!("{outcome:?}"));
            }
        }
        for text in &rendered {
            assert!(!text.contains(token), "{text}");
        }
    }

    // -- outbound mapping -----------------------------------------------------

    #[test]
    fn a_rate_limit_is_retryable_with_its_retry_after() {
        assert!(matches!(
            map_send_status(429, "persistent_notification", Some(1500)),
            Some(SendOutcome::RetryableFailure {
                retry_after_ms: Some(1500),
                ..
            })
        ));
    }

    #[test]
    fn an_unauthorized_or_unknown_service_is_permanent() {
        for status in [401, 403, 404, 400] {
            assert!(
                matches!(
                    map_send_status(status, "persistent_notification", None),
                    Some(SendOutcome::PermanentFailure { .. })
                ),
                "HTTP {status}"
            );
        }
        assert!(matches!(
            map_send_status(503, "persistent_notification", None),
            Some(SendOutcome::RetryableFailure { .. })
        ));
        assert!(map_send_status(200, "persistent_notification", None).is_none());
    }

    // -- HTTP fixtures --------------------------------------------------------

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn declared_length(headers: &[u8]) -> usize {
        String::from_utf8_lossy(headers)
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0)
    }

    /// Drain one whole HTTP request, headers and declared body, and return it.
    async fn drain_request(stream: &mut tokio::net::TcpStream) -> String {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
            let Some(header_end) = find(&buffer, b"\r\n\r\n") else {
                continue;
            };
            let body_start = header_end + 4;
            if buffer.len() - body_start >= declared_length(&buffer[..header_end]) {
                break;
            }
        }
        String::from_utf8_lossy(&buffer).to_string()
    }

    /// A Home Assistant stand-in that answers a fixed script — one
    /// `(status, extra header, body)` per request — and records what it was
    /// asked, so a test can assert on the request as well as the outcome.
    async fn recording_server(
        script: Vec<(&'static str, &'static str, &'static str)>,
    ) -> (String, Arc<Mutex<Vec<String>>>, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let address = listener.local_addr().expect("fixture address");
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let count = Arc::new(AtomicUsize::new(0));
        let recorder = seen.clone();
        let counter = count.clone();
        tokio::spawn(async move {
            for (status, extra, body) in script {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let request = drain_request(&mut stream).await;
                recorder.lock().await.push(request);
                counter.fetch_add(1, Ordering::SeqCst);
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{extra}content-length: \
                     {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });
        (format!("http://{address}"), seen, count)
    }

    /// A server that answers every request the same way, forever. Used where
    /// the socket task is also running and would otherwise eat a scripted
    /// reply.
    async fn always_server(status: &'static str, body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let address = listener.local_addr().expect("fixture address");
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let _ = drain_request(&mut stream).await;
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: \
                     {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn a_notify_success_is_sent_with_no_message_id() {
        let (base, seen, count) = recording_server(vec![("200 OK", "", "[]")]).await;
        let outcome = adapter_for(&base).send(&outbound("the door is shut")).await;
        assert_eq!(
            outcome,
            SendOutcome::Sent {
                provider_message_id: None
            },
            "a notify service returns no id, and inventing one would poison the echo ledger"
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let request = seen.lock().await[0].clone();
        assert!(request.contains("POST /api/services/notify/persistent_notification"));
        assert!(request.contains("authorization: Bearer llat-secret-token"));
        assert!(request.contains("the door is shut"));
    }

    #[tokio::test]
    async fn a_rate_limited_notify_carries_its_retry_after() {
        let (base, _, _) =
            recording_server(vec![("429 Too Many Requests", "retry-after: 2\r\n", "{}")]).await;
        let outcome = adapter_for(&base).send(&outbound("hello")).await;
        assert!(
            matches!(
                outcome,
                SendOutcome::RetryableFailure {
                    retry_after_ms: Some(2000),
                    ..
                }
            ),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn an_unauthorized_notify_is_permanent() {
        let (base, _, _) = recording_server(vec![("401 Unauthorized", "", "{}")]).await;
        let outcome = adapter_for(&base).send(&outbound("hello")).await;
        assert!(
            matches!(outcome, SendOutcome::PermanentFailure { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_notify_that_could_not_leave_this_machine_is_retryable() {
        // A port nobody is listening on: the connector fails, so no application
        // byte can have reached an instance. This is the one shape that may
        // retry.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        drop(listener);
        let outcome = adapter_for(&format!("http://{address}"))
            .send(&outbound("hello"))
            .await;
        assert!(
            matches!(outcome, SendOutcome::RetryableFailure { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_message_with_an_attachment_is_refused_before_a_request_is_made() {
        let (base, _, count) = recording_server(vec![("200 OK", "", "[]")]).await;
        let mut message = outbound("here is the photo");
        message.attachments = vec![little_monkey_lib::channels::types::OutboundAttachment {
            artifact_id: "artifact-1".to_string(),
            filename: Some("cam.jpg".to_string()),
            mime_type: Some("image/jpeg".to_string()),
        }];
        let outcome = adapter_for(&base).send(&message).await;
        assert!(
            matches!(outcome, SendOutcome::PermanentFailure { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "the file cannot ride along, so nothing should have been notified at all"
        );
    }

    #[tokio::test]
    async fn an_over_long_reply_is_truncated_and_says_so() {
        let (base, seen, _) = recording_server(vec![("200 OK", "", "[]")]).await;
        let outcome = adapter_for(&base)
            .send(&outbound(&"z".repeat(HOME_ASSISTANT_MAX_TEXT_CHARS * 2)))
            .await;
        assert!(matches!(outcome, SendOutcome::Sent { .. }), "{outcome:?}");
        let request = seen.lock().await[0].clone();
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).expect("body"))
            .expect("json body");
        let sent = body["message"].as_str().expect("message");
        assert_eq!(sent.chars().count(), HOME_ASSISTANT_MAX_TEXT_CHARS);
        assert!(sent.ends_with(TRUNCATION_MARKER));
    }

    // -- health ---------------------------------------------------------------

    #[tokio::test]
    async fn a_working_token_with_a_dead_socket_is_not_connected() {
        // `/api/` answers, the WebSocket upgrade never does. Inbound arrives on
        // the socket, so calling this Connected would tell an operator that
        // messages are flowing into a channel that receives nothing.
        let base = always_server("200 OK", r#"{"message":"API running."}"#).await;
        let health = adapter_for(&base).probe().await;
        assert_ne!(
            health.state,
            HealthState::Connected,
            "{health:?} claims a subscription the socket never made"
        );
    }

    #[tokio::test]
    async fn a_rejected_token_probes_as_an_error() {
        let base = always_server("401 Unauthorized", "{}").await;
        let health = adapter_for(&base).probe().await;
        assert_eq!(health.state, HealthState::Error, "{health:?}");
        assert!(!format!("{health:?}").contains("llat-secret-token"));
    }

    // -- the reader must never block on the consumer --------------------------

    #[tokio::test]
    async fn a_full_queue_drops_and_reports_rather_than_stalling_the_reader() {
        let shared = Arc::new(Shared::default());
        let (tx, _rx) = mpsc::channel(1);
        let envelope =
            normalize_event(&event_frame(), "acct-ha", DEFAULT_EVENT_TYPE, 0).expect("envelope");

        deliver(&shared, &tx, envelope.clone());
        assert_eq!(shared.dropped.load(std::sync::atomic::Ordering::Relaxed), 0);

        // The queue is now full. A second event must not park the reader — the
        // reader is also what answers Home Assistant's pings.
        let overflowed = tokio::time::timeout(Duration::from_millis(200), async {
            deliver(&shared, &tx, envelope);
        })
        .await;
        assert!(overflowed.is_ok(), "the reader blocked on a full queue");
        assert_eq!(shared.dropped.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(
            shared.status.get(),
            HealthState::Degraded,
            "an overflow an operator cannot see is a message that silently vanished"
        );
    }

    // -- the whole socket path ------------------------------------------------

    /// A Home Assistant stand-in on `/api/websocket`: it drives the real
    /// handshake, refuses to deliver anything the client did not authenticate
    /// and subscribe for, and then pushes one recorded event.
    async fn websocket_fixture() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let address = listener.local_addr().expect("fixture address");
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("accept websocket");
            let mut ws = tokio_tungstenite::accept_async(socket)
                .await
                .expect("websocket handshake");
            let _ = ws.send(Message::Text(AUTH_REQUIRED.into())).await;

            let auth = ws.next().await.expect("a frame").expect("a text frame");
            let auth: Value = serde_json::from_str(auth.to_text().expect("text")).expect("json");
            assert_eq!(auth["type"], "auth");
            assert_eq!(auth["access_token"], "llat-secret-token");
            let _ = ws.send(Message::Text(AUTH_OK.into())).await;

            let subscribe = ws.next().await.expect("a frame").expect("a text frame");
            let subscribe: Value =
                serde_json::from_str(subscribe.to_text().expect("text")).expect("json");
            assert_eq!(subscribe["type"], "subscribe_events");
            assert_eq!(subscribe["event_type"], DEFAULT_EVENT_TYPE);
            let _ = ws.send(Message::Text(SUBSCRIBE_OK.into())).await;

            let _ = ws
                .send(Message::Text(event_frame().to_string().into()))
                .await;
            // Held open: closing here would look like a dropped connection.
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn an_event_travels_the_whole_socket_path_into_poll() {
        let adapter = adapter_for(&websocket_fixture().await);
        let batch = adapter.poll(None).await.expect("poll");
        assert_eq!(batch.envelopes.len(), 1, "{batch:?}");
        assert_eq!(
            batch.envelopes[0].provider_event_id,
            "01JZ6QK8Y0S9F2N4V7B3D5H1M6"
        );
        assert_eq!(batch.cursor, None, "the subscription is the resume token");
        assert_eq!(
            adapter.live_transport(),
            Some(HealthState::Connected),
            "a live subscription is what Connected means here"
        );
    }
}
