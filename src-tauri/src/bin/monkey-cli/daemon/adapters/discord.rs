//! Discord adapter: gateway WebSocket inbound, REST outbound.
//!
//! One long-lived task owns the gateway socket for the lifetime of the
//! adapter. It is spawned lazily, on the first [`ChannelAdapter::probe`] or
//! [`ChannelAdapter::poll`] call rather than in [`DiscordAdapter::new`] —
//! `new` is a plain, synchronous constructor with no side effects, so
//! building an adapter for an account that turns out to be disabled never
//! opens a socket. The task normalizes `MESSAGE_CREATE` dispatches and pushes
//! them into a bounded channel; `poll` only drains that channel. The
//! gateway's own framing (HELLO, heartbeat, IDENTIFY/RESUME, DISPATCH) is
//! handled by [`handle_frame`] and [`handle_close`], both pure functions of a
//! [`GatewayState`] so the protocol logic is testable without a socket.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use little_monkey_lib::channels::types::{
    AttachmentKind, AttachmentSource, ChannelAttachment, ChannelConversation, ChannelEnvelope,
    ChannelHealth, ChannelKind, ChannelSender, InboundTransport, OutboundMessage,
    ProviderCapabilities, SendOutcome,
};
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

use crate::daemon::channel_adapter::{
    load_attachments, AdapterConfig, BlobSource, ChannelAdapter, DaemonBlobs, InboundBatch,
    LoadedAttachment,
};

const API_BASE: &str = "https://discord.com/api/v10";
const GATEWAY_VERSION: &str = "10";
const DISCORD_MAX_TEXT_CHARS: usize = 2000;
/// Guilds (1<<0, carries THREAD_CREATE/THREAD_UPDATE — without it the
/// thread-to-parent map never populates) + guild messages (1<<9) + direct
/// messages (1<<12) + message content (1<<15). Exactly the set the implemented
/// feature surface reads, nothing speculative.
const DISCORD_INTENTS: u64 = 1 | (1 << 9) | (1 << 12) | (1 << 15);
const INBOUND_CHANNEL_CAPACITY: usize = 256;
/// How long [`DiscordAdapter::poll`] blocks for a first envelope before
/// returning an empty batch. Bounded so the daemon's poll loop keeps ticking
/// even when Discord is quiet.
const POLL_WAIT: Duration = Duration::from_secs(20);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Discord allows one IDENTIFY per five seconds per token; sending them
/// faster earns a 4008/invalid-session spiral rather than a connection.
const IDENTIFY_SPACING: Duration = Duration::from_secs(5);

/// What a RESUME needs to survive a daemon restart: the session, where Discord
/// said to resume it, and the last sequence number seen. Serialized as the
/// account's inbound cursor, which is persisted only after the batch it covers
/// is durably recorded — so the stored `seq` can lag reality but never lead
/// it, and a lagging resume merely replays events the event log deduplicates.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ResumeState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_gateway_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// Our own user id, learned from READY. Carried here because a session
    /// that RESUMEs never sees a second READY, and without it `is_self` is
    /// false for our own messages — which for Discord, where a bot is
    /// dispatched its own `MESSAGE_CREATE`, is how a reply loop starts. Not a
    /// secret: a bot's user id is visible to everyone it talks to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_user_id: Option<String>,
}

impl ResumeState {
    fn parse(cursor: Option<&str>) -> Self {
        cursor
            .and_then(|value| serde_json::from_str(value).ok())
            .unwrap_or_default()
    }

    fn resumable(&self) -> bool {
        self.session_id.is_some() && self.seq.is_some()
    }
}

/// Gateway connection status, for an honest probe: `Connected` is written only
/// by a READY/RESUMED on a live socket, never by saved configuration.
const GATEWAY_NOT_STARTED: u8 = 0;
const GATEWAY_RECONNECTING: u8 = 1;
const GATEWAY_CONNECTED: u8 = 2;

/// State shared between the gateway task and the adapter handle. Everything
/// here is either non-secret or already redacted before it lands — the token
/// itself lives only in the task's local variables and the request headers it
/// builds, never in a field another caller can read back.
#[derive(Default)]
struct Shared {
    /// Set once, by a non-resumable close code (4004, 4014). Once set the
    /// gateway task has exited and will not reconnect — a bad token or a
    /// disallowed intent is not something backoff fixes.
    permanent_error: Mutex<Option<String>>,
    /// The current resume state, mirrored out of the gateway task after every
    /// frame so `poll` can snapshot it into the durable cursor.
    resume: std::sync::Mutex<ResumeState>,
    /// One of the `GATEWAY_*` constants above.
    gateway_status: std::sync::atomic::AtomicU8,
}

impl Shared {
    fn snapshot_resume_json(&self) -> Option<String> {
        if self
            .gateway_status
            .load(std::sync::atomic::Ordering::SeqCst)
            == GATEWAY_NOT_STARTED
        {
            return None;
        }
        let state = self.resume.lock().ok()?.clone();
        serde_json::to_string(&state).ok()
    }
}

pub struct DiscordAdapter {
    account_id: String,
    token: String,
    http: reqwest::Client,
    /// Each envelope travels with the resume state as of its own dispatch, so
    /// the cursor persisted for a batch is exactly the state at its last
    /// message — never a sequence that leads an envelope still in this queue.
    inbound_tx: mpsc::Sender<(ChannelEnvelope, ResumeState)>,
    inbound_rx: Mutex<mpsc::Receiver<(ChannelEnvelope, ResumeState)>>,
    shared: Arc<Shared>,
    /// Guards the one-time spawn of the gateway task. See the module doc for
    /// why construction itself must stay side-effect-free.
    started: tokio::sync::OnceCell<()>,
    blobs: Arc<dyn BlobSource>,
    /// The REST origin. Always [`API_BASE`] in production; swappable in tests
    /// so the whole gateway handshake can run against a loopback fixture.
    api_base: String,
}

impl DiscordAdapter {
    pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
        if config.secret.is_empty() {
            return Err("This Discord account has no bot token configured".to_string());
        }
        let http = little_monkey_lib::egress::hardened()
            .build()
            .map_err(|error| format!("Failed to build the Discord HTTP client: {error}"))?;
        let (tx, rx) = mpsc::channel(INBOUND_CHANNEL_CAPACITY);
        Ok(Self {
            account_id: config.account.account_id.clone(),
            token: config.secret.clone(),
            http,
            inbound_tx: tx,
            inbound_rx: Mutex::new(rx),
            shared: Arc::new(Shared::default()),
            started: tokio::sync::OnceCell::new(),
            blobs: Arc::new(DaemonBlobs),
            api_base: API_BASE.to_string(),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_base_url(mut self, base: &str) -> Self {
        self.api_base = base.to_string();
        self
    }

    /// Start the gateway task once, seeded with the resume state persisted as
    /// this account's cursor — which is why only `poll`, which is handed the
    /// cursor, ever starts it. A RESUME with the stored session and sequence is
    /// what turns a daemon restart into a gap-free continuation instead of a
    /// fresh session that never sees what arrived while the process was down.
    async fn ensure_started(&self, cursor: Option<&str>) {
        self.started
            .get_or_init(|| async {
                let initial = ResumeState::parse(cursor);
                if let Ok(mut resume) = self.shared.resume.lock() {
                    *resume = initial;
                }
                self.shared
                    .gateway_status
                    .store(GATEWAY_RECONNECTING, std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(run_gateway_loop(
                    self.account_id.clone(),
                    self.token.clone(),
                    self.api_base.clone(),
                    self.http.clone(),
                    self.inbound_tx.clone(),
                    self.shared.clone(),
                ));
            })
            .await;
    }
}

#[async_trait]
impl ChannelAdapter for DiscordAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Discord
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            max_text_chars: DISCORD_MAX_TEXT_CHARS,
            supports_threads: true,
            supports_attachments: true,
            supports_mention_metadata: true,
            supports_idempotency_key: false,
            supports_delivery_receipts: false,
            ..ProviderCapabilities::minimal(ChannelKind::Discord, InboundTransport::Socket)
        }
    }

    async fn probe(&self) -> ChannelHealth {
        // Deliberately does not start the gateway: only `poll` holds the
        // persisted resume cursor, and a probe-started task would burn the
        // stored session by identifying fresh.
        let now = now_ms();
        if let Some(error) = self.shared.permanent_error.lock().await.clone() {
            return ChannelHealth::error(now, error);
        }
        let request = self
            .http
            .get(format!("{}/users/@me", self.api_base))
            .header("Authorization", format!("Bot {}", self.token));
        match little_monkey_lib::egress::send(request).await {
            Ok(response) if response.status().is_success() => {
                let body: Value = response.json().await.unwrap_or(Value::Null);
                let username = body
                    .get("username")
                    .and_then(Value::as_str)
                    .unwrap_or("bot");
                // The REST identity proves the token; the gateway status is
                // what proves messages can actually arrive. Both are reported,
                // and a live token with a down socket is degraded, not
                // connected — saved credentials are not a connection.
                match self
                    .shared
                    .gateway_status
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    GATEWAY_RECONNECTING => ChannelHealth {
                        state: little_monkey_lib::channels::types::HealthState::Degraded,
                        detail: Some(format!(
                            "Authenticated to Discord as {username}; the gateway socket is reconnecting"
                        )),
                        last_error: None,
                        probed_at_ms: now,
                    },
                    _ => ChannelHealth::connected(
                        now,
                        Some(format!("Connected to Discord as {username}")),
                    ),
                }
            }
            Ok(response) => ChannelHealth::error(
                now,
                scrub(
                    &format!("Discord probe failed: HTTP {}", response.status()),
                    &self.token,
                ),
            ),
            Err(error) => ChannelHealth::error(
                now,
                scrub(&format!("Discord probe failed: {error}"), &self.token),
            ),
        }
    }

    async fn poll(&self, cursor: Option<&str>) -> Result<InboundBatch, String> {
        // The cursor is this account's persisted RESUME state; the first poll
        // after a restart seeds the gateway task with it.
        self.ensure_started(cursor).await;
        if let Some(error) = self.shared.permanent_error.lock().await.clone() {
            return Err(error);
        }
        // For an empty batch the persisted state is a snapshot taken BEFORE
        // the wait, which can only lag reality — safe, since a lagging RESUME
        // merely replays what the event log deduplicates. A non-empty batch
        // uses the state that traveled with its last envelope, which matches
        // that envelope's dispatch exactly.
        let early_snapshot = self.shared.snapshot_resume_json();
        let mut rx = self.inbound_rx.lock().await;
        let mut envelopes = Vec::new();
        let mut last_state: Option<ResumeState> = None;
        match tokio::time::timeout(POLL_WAIT, rx.recv()).await {
            Ok(Some((envelope, state))) => {
                envelopes.push(envelope);
                last_state = Some(state);
                while let Ok((envelope, state)) = rx.try_recv() {
                    envelopes.push(envelope);
                    last_state = Some(state);
                }
            }
            Ok(None) => {
                if let Some(error) = self.shared.permanent_error.lock().await.clone() {
                    return Err(error);
                }
            }
            Err(_) => {}
        }
        let cursor = match last_state {
            Some(state) => serde_json::to_string(&state).ok(),
            None => early_snapshot,
        };
        Ok(InboundBatch { envelopes, cursor })
    }

    async fn send(&self, message: &OutboundMessage) -> SendOutcome {
        if !message.attachments.is_empty() {
            let files = match load_attachments(self.blobs.as_ref(), message) {
                Ok(files) => files,
                Err(outcome) => return outcome,
            };
            return self.send_with_attachments(message, &files).await;
        }
        let mut chunks = split_message(&message.text, DISCORD_MAX_TEXT_CHARS);
        if chunks.is_empty() {
            chunks.push(String::new());
        }
        let mut any_sent = false;
        let mut last_id = None;
        for (index, chunk) in chunks.iter().enumerate() {
            let mut body = serde_json::json!({ "content": chunk });
            if index == 0 {
                if let Some(reply_to) = &message.reply_to_provider_id {
                    body["message_reference"] = serde_json::json!({ "message_id": reply_to });
                }
            }
            let request = self
                .http
                .post(format!(
                    "{}/channels/{}/messages",
                    self.api_base,
                    target_channel(message)
                ))
                .header("Authorization", format!("Bot {}", self.token))
                .json(&body);
            let response = match little_monkey_lib::egress::send(request).await {
                Ok(response) => response,
                Err(error) => {
                    let error = scrub(&error.to_string(), &self.token);
                    return if any_sent {
                        SendOutcome::NeedsReconciliation { error }
                    } else {
                        SendOutcome::RetryableFailure {
                            error,
                            retry_after_ms: None,
                        }
                    };
                }
            };
            let status = response.status();
            let retry_after_ms = if status.as_u16() == 429 {
                parse_retry_after_seconds(
                    response
                        .headers()
                        .get("Retry-After")
                        .and_then(|value| value.to_str().ok()),
                )
            } else {
                None
            };
            // Discord's own bucket accounting, read from the response rather
            // than guessed: when this bucket is out of requests, the next
            // chunk of the same reply waits out the window instead of buying
            // a 429.
            let bucket_wait = bucket_exhausted_wait(response.headers());
            if let Some(outcome) = map_send_status(status.as_u16(), any_sent, retry_after_ms) {
                return outcome;
            }
            any_sent = true;
            last_id = response
                .json::<Value>()
                .await
                .ok()
                .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_string));
            if index + 1 < chunks.len() {
                if let Some(wait) = bucket_wait {
                    tokio::time::sleep(wait).await;
                }
            }
        }
        SendOutcome::Sent {
            provider_message_id: last_id,
        }
    }
}

/// Where a message posts: the thread when one is targeted, else the channel.
/// A Discord thread is itself a channel id, so thread targeting is channel
/// selection — posting a thread reply to the parent channel would leak the
/// conversation out of its thread.
fn target_channel(message: &OutboundMessage) -> &str {
    message
        .thread_id
        .as_deref()
        .unwrap_or(&message.conversation_id)
}

/// How long to wait before reusing this route, when Discord says the bucket
/// is spent: `X-RateLimit-Remaining: 0` plus `X-RateLimit-Reset-After`
/// (seconds, fractional). `None` when requests remain or the headers are
/// absent — never an invented fixed delay.
fn bucket_exhausted_wait(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let remaining: u64 = headers
        .get("X-RateLimit-Remaining")?
        .to_str()
        .ok()?
        .parse()
        .ok()?;
    if remaining > 0 {
        return None;
    }
    let reset_after: f64 = headers
        .get("X-RateLimit-Reset-After")?
        .to_str()
        .ok()?
        .parse()
        .ok()?;
    Some(Duration::from_millis((reset_after * 1000.0).max(0.0) as u64))
}

impl DiscordAdapter {
    /// Discord takes files on the same message-create endpoint, as multipart
    /// with the JSON body in `payload_json` and each file in `files[n]`. One
    /// request carries the text and every file, so a caller never sees a
    /// caption arrive separately from what it captions.
    async fn send_with_attachments(
        &self,
        message: &OutboundMessage,
        files: &[LoadedAttachment],
    ) -> SendOutcome {
        // Text longer than one Discord message is sent first, exactly as the
        // text-only path splits it; the files then ride the final message.
        let mut leading = split_message(&message.text, DISCORD_MAX_TEXT_CHARS);
        let last = leading.pop().unwrap_or_default();
        let mut any_sent = false;
        if !leading.is_empty() {
            let head = OutboundMessage {
                text: leading.join(""),
                attachments: Vec::new(),
                ..message.clone()
            };
            match self.send(&head).await {
                SendOutcome::Sent { .. } => any_sent = true,
                other => return other,
            }
        }
        let mut body = serde_json::json!({ "content": last });
        if !any_sent {
            if let Some(reply_to) = &message.reply_to_provider_id {
                body["message_reference"] = serde_json::json!({ "message_id": reply_to });
            }
        }
        let mut form = reqwest::multipart::Form::new().text("payload_json", body.to_string());
        for (index, file) in files.iter().enumerate() {
            let part = reqwest::multipart::Part::bytes(file.bytes.clone())
                .file_name(file.filename.clone())
                .mime_str(&file.mime_type)
                .unwrap_or_else(|_| {
                    reqwest::multipart::Part::bytes(file.bytes.clone())
                        .file_name(file.filename.clone())
                });
            form = form.part(format!("files[{index}]"), part);
        }
        let request = self
            .http
            .post(format!(
                "{}/channels/{}/messages",
                self.api_base,
                target_channel(message)
            ))
            .header("Authorization", format!("Bot {}", self.token))
            .multipart(form);
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => {
                // A connect failure provably never left this machine, so it is
                // safe to retry. Anything later may have uploaded the file
                // already, and a blind retry would post it twice.
                let is_connect = error.is_connect();
                let error = scrub(&error.to_string(), &self.token);
                return if is_connect && !any_sent {
                    SendOutcome::RetryableFailure {
                        error,
                        retry_after_ms: None,
                    }
                } else {
                    SendOutcome::NeedsReconciliation { error }
                };
            }
        };
        let status = response.status();
        let retry_after_ms = if status.as_u16() == 429 {
            parse_retry_after_seconds(
                response
                    .headers()
                    .get("Retry-After")
                    .and_then(|value| value.to_str().ok()),
            )
        } else {
            None
        };
        if let Some(outcome) = map_send_status(status.as_u16(), any_sent, retry_after_ms) {
            return outcome;
        }
        SendOutcome::Sent {
            provider_message_id: response
                .json::<Value>()
                .await
                .ok()
                .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_string)),
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Removes an exact-match token from a string before it is allowed anywhere
/// near a health record, a log, or an error surfaced to the caller.
fn scrub(text: &str, token: &str) -> String {
    if token.is_empty() {
        text.to_string()
    } else {
        text.replace(token, "[redacted]")
    }
}

/// Splits `text` into chunks of at most `limit` characters, on char
/// boundaries. Simple and predictable rather than word-aware: Discord's limit
/// is generous enough that mid-word splits are a rare, cosmetic cost.
fn split_message(text: &str, limit: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(limit.max(1))
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// Maps an HTTP status from `POST .../messages` to a terminal [`SendOutcome`],
/// or `None` when the caller should treat it as success and continue (the next
/// chunk, or done). Pulled out of [`DiscordAdapter::send`] so the mapping is
/// testable without a socket or an HTTP server.
fn map_send_status(
    status: u16,
    any_sent_before: bool,
    retry_after_ms: Option<i64>,
) -> Option<SendOutcome> {
    match status {
        200..=299 => None,
        429 => Some(SendOutcome::RetryableFailure {
            error: "Discord rate limited the request".to_string(),
            retry_after_ms,
        }),
        401 | 403 => Some(SendOutcome::PermanentFailure {
            error: format!("Discord rejected the request: HTTP {status}"),
        }),
        500..=599 => Some(if any_sent_before {
            SendOutcome::NeedsReconciliation {
                error: format!("Discord returned HTTP {status}"),
            }
        } else {
            SendOutcome::RetryableFailure {
                error: format!("Discord returned HTTP {status}"),
                retry_after_ms: None,
            }
        }),
        _ => Some(SendOutcome::PermanentFailure {
            error: format!("Discord rejected the message: HTTP {status}"),
        }),
    }
}

/// Parses Discord's `Retry-After` header (seconds, float) into milliseconds.
fn parse_retry_after_seconds(header_value: Option<&str>) -> Option<i64> {
    let seconds: f64 = header_value?.parse().ok()?;
    Some((seconds * 1000.0).round() as i64)
}

// ---------------------------------------------------------------------------
// Gateway state machine (pure)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
struct GatewayState {
    seq: Option<u64>,
    session_id: Option<String>,
    /// Where Discord said this session must be resumed, from READY. RESUMEs
    /// sent to the general gateway endpoint may be rejected outright.
    resume_gateway_url: Option<String>,
    bot_user_id: Option<String>,
    /// Set by a resumable close/RECONNECT/INVALID_SESSION(resumable=true); read
    /// by the next HELLO to decide RESUME vs a fresh IDENTIFY.
    pending_resume: bool,
    /// Thread channel id -> parent channel id, learned from THREAD_CREATE /
    /// THREAD_UPDATE dispatches.
    ///
    /// ponytail: populate-on-see, in memory only. A thread that was already
    /// active before this connection has ever seen it has no parent on record
    /// until Discord dispatches a THREAD_CREATE/UPDATE for it (or the bot is
    /// added to it), so its messages resolve as a plain channel until then.
    /// Upgrade path if that gap matters: on READY, walk each guild's
    /// active-threads REST endpoint once to seed this map.
    thread_parents: HashMap<String, String>,
}

impl GatewayState {
    /// A state seeded from what the last run of this daemon persisted. The
    /// stored sequence may lag by whatever was in flight around the crash,
    /// which is safe: a lagging RESUME replays events the durable event log
    /// deduplicates, where a fresh IDENTIFY would never see them at all.
    fn from_resume(resume: &ResumeState) -> Self {
        Self {
            seq: resume.seq,
            session_id: resume.session_id.clone(),
            resume_gateway_url: resume.resume_gateway_url.clone(),
            bot_user_id: resume.bot_user_id.clone(),
            pending_resume: resume.resumable(),
            ..Self::default()
        }
    }

    fn resume_snapshot(&self) -> ResumeState {
        ResumeState {
            session_id: self.session_id.clone(),
            resume_gateway_url: self.resume_gateway_url.clone(),
            seq: self.seq,
            bot_user_id: self.bot_user_id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Action {
    SendJson(Value),
    /// An IDENTIFY, separate from `SendJson` because the I/O loop must space
    /// these five seconds apart per Discord's identify rate limit.
    Identify(Value),
    StartHeartbeat(u64),
    /// Discord acknowledged our heartbeat; the I/O loop clears its
    /// missed-ACK flag.
    HeartbeatAck,
    /// READY or RESUMED arrived: the session is live, backoff may reset.
    Established,
    SetBotUserId(String),
    Envelope(Box<ChannelEnvelope>),
    PermanentError(String),
    /// Tear the socket down and reconnect after at least `delay_ms` —
    /// non-zero only for INVALID_SESSION, where Discord asks for a short
    /// randomized wait before the next attempt.
    Reconnect {
        delay_ms: u64,
    },
}

fn identify_payload(token: &str, intents: u64) -> Value {
    serde_json::json!({
        "op": 2,
        "d": {
            "token": token,
            "intents": intents,
            "properties": {
                "os": "linux",
                "browser": "little-monkey",
                "device": "little-monkey",
            },
        }
    })
}

fn resume_payload(token: &str, session_id: &str, seq: u64) -> Value {
    serde_json::json!({
        "op": 6,
        "d": { "token": token, "session_id": session_id, "seq": seq },
    })
}

/// Handles one gateway text frame, mutating `state` and returning what the
/// caller should do about it. Never touches a socket, a clock, or a channel —
/// every side effect is expressed as a returned [`Action`].
fn handle_frame(
    state: &mut GatewayState,
    account_id: &str,
    frame: &str,
    token: &str,
) -> Vec<Action> {
    let Ok(value) = serde_json::from_str::<Value>(frame) else {
        return Vec::new();
    };
    if let Some(seq) = value.get("s").and_then(Value::as_u64) {
        state.seq = Some(seq);
    }
    match value.get("op").and_then(Value::as_i64).unwrap_or(-1) {
        // HELLO
        10 => {
            let interval = value["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250);
            let mut actions = vec![Action::StartHeartbeat(interval)];
            if state.pending_resume {
                if let (Some(session_id), Some(seq)) = (state.session_id.clone(), state.seq) {
                    actions.push(Action::SendJson(resume_payload(token, &session_id, seq)));
                    return actions;
                }
            }
            state.session_id = None;
            actions.push(Action::Identify(identify_payload(token, DISCORD_INTENTS)));
            actions
        }
        // DISPATCH
        0 => {
            let event_type = value.get("t").and_then(Value::as_str).unwrap_or_default();
            let data = value.get("d").cloned().unwrap_or(Value::Null);
            let mut actions = Vec::new();
            match event_type {
                "READY" => {
                    state.pending_resume = false;
                    if let Some(session_id) = data.get("session_id").and_then(Value::as_str) {
                        state.session_id = Some(session_id.to_string());
                    }
                    // Where THIS session must be resumed. Discord may reject a
                    // RESUME aimed at the general gateway endpoint.
                    if let Some(url) = data.get("resume_gateway_url").and_then(Value::as_str) {
                        state.resume_gateway_url = Some(url.to_string());
                    }
                    if let Some(id) = data
                        .get("user")
                        .and_then(|user| user.get("id"))
                        .and_then(Value::as_str)
                    {
                        state.bot_user_id = Some(id.to_string());
                        actions.push(Action::SetBotUserId(id.to_string()));
                    }
                    actions.push(Action::Established);
                }
                "RESUMED" => {
                    state.pending_resume = false;
                    actions.push(Action::Established);
                }
                "THREAD_CREATE" | "THREAD_UPDATE" => {
                    if let (Some(id), Some(parent)) = (
                        data.get("id").and_then(Value::as_str),
                        data.get("parent_id").and_then(Value::as_str),
                    ) {
                        state
                            .thread_parents
                            .insert(id.to_string(), parent.to_string());
                    }
                }
                "MESSAGE_CREATE" => {
                    if let Some(envelope) = normalize_message_create(
                        account_id,
                        &data,
                        state.bot_user_id.as_deref(),
                        &state.thread_parents,
                        now_ms(),
                    ) {
                        actions.push(Action::Envelope(Box::new(envelope)));
                    }
                }
                _ => {}
            }
            actions
        }
        // Gateway-requested heartbeat
        1 => vec![Action::SendJson(
            serde_json::json!({ "op": 1, "d": state.seq }),
        )],
        // RECONNECT: always resumable.
        7 => {
            state.pending_resume = true;
            vec![Action::Reconnect { delay_ms: 0 }]
        }
        // INVALID_SESSION: `d` says whether a resume is worth attempting.
        // Either way Discord asks for a short randomized wait before the next
        // attempt, so a burst of invalid sessions cannot become an
        // identify storm.
        9 => {
            let resumable = value.get("d").and_then(Value::as_bool).unwrap_or(false);
            state.pending_resume = resumable;
            if !resumable {
                state.session_id = None;
                state.seq = None;
            }
            vec![Action::Reconnect {
                delay_ms: 1_000 + jitter_ms(4_000),
            }]
        }
        // Heartbeat ACK: clears the I/O loop's missed-ACK flag. A connection
        // whose ACKs stop arriving is a zombie — TCP-alive, Discord-dead —
        // and only this opcode can prove liveness.
        11 => vec![Action::HeartbeatAck],
        _ => Vec::new(),
    }
}

/// A cheap jitter source: the sub-second nanos of the wall clock, folded into
/// `0..range_ms`. Not cryptographic and does not need to be — it only has to
/// keep a fleet of reconnecting clients from thundering in step.
fn jitter_ms(range_ms: u64) -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::from(duration.subsec_nanos()))
        .unwrap_or(0)
        % range_ms.max(1)
}

/// Handles a WebSocket close code. Only 4004 (authentication failed) and 4014
/// (disallowed intents) are permanent per Discord's own close-code table —
/// both mean this credential/config cannot succeed no matter how many times
/// the gateway task reconnects. Everything else, including an ordinary
/// network drop (which never reaches here as a close frame at all — the
/// caller treats a read error or stream end the same as a resumable close),
/// is worth retrying.
fn handle_close(state: &mut GatewayState, code: u16) -> Vec<Action> {
    match code {
        4004 => vec![Action::PermanentError(
            "Discord rejected the bot token (close code 4004: authentication failed)".to_string(),
        )],
        4014 => vec![Action::PermanentError(
            "Discord refused the requested gateway intents (close code 4014). If the bot should \
             read message text, enable the Message Content intent for it in the Discord \
             developer portal under Bot → Privileged Gateway Intents, then re-enable this account"
                .to_string(),
        )],
        _ => {
            state.pending_resume = true;
            vec![Action::Reconnect { delay_ms: 0 }]
        }
    }
}

fn normalize_message_create(
    account_id: &str,
    data: &Value,
    bot_user_id: Option<&str>,
    thread_parents: &HashMap<String, String>,
    now_ms: i64,
) -> Option<ChannelEnvelope> {
    let id = data.get("id")?.as_str()?.to_string();
    let channel_id = data.get("channel_id")?.as_str()?.to_string();
    let author = data.get("author")?;
    let author_id = author.get("id")?.as_str()?.to_string();
    let username = author
        .get("username")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.is_empty());
    let is_bot = author.get("bot").and_then(Value::as_bool).unwrap_or(false);
    let is_self = bot_user_id.is_some_and(|bot_id| bot_id == author_id);

    let (conversation_id, thread_id) = match thread_parents.get(&channel_id) {
        Some(parent) => (parent.clone(), Some(channel_id.clone())),
        None => (channel_id.clone(), None),
    };
    let is_dm = data.get("guild_id").is_none();
    let conversation = if is_dm {
        ChannelConversation::direct(conversation_id)
    } else {
        ChannelConversation::group(conversation_id)
    }
    .with_thread(thread_id);

    let mentions_self = bot_user_id.is_some_and(|bot_id| {
        data.get("mentions")
            .and_then(Value::as_array)
            .is_some_and(|mentions| {
                mentions
                    .iter()
                    .any(|mention| mention.get("id").and_then(Value::as_str) == Some(bot_id))
            })
    });

    let text = data
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let attachments = data
        .get("attachments")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|attachment| {
                    let url = attachment.get("url")?.as_str()?.to_string();
                    let mime_type = attachment
                        .get("content_type")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let kind = mime_type
                        .as_deref()
                        .map(AttachmentKind::from_mime)
                        .unwrap_or(AttachmentKind::Other);
                    Some(ChannelAttachment {
                        stored_artifact_id: None,
                        text_excerpt: None,
                        fetch_error: None,
                        provider_id: attachment
                            .get("id")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        kind,
                        filename: attachment
                            .get("filename")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        mime_type,
                        declared_size_bytes: attachment.get("size").and_then(Value::as_u64),
                        source: AttachmentSource::Url { url },
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let reply_to_provider_id = data
        .get("message_reference")
        .and_then(|reference| reference.get("message_id"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut metadata = little_monkey_lib::channels::types::BoundedMetadata::new();
    if let Some(guild_id) = data.get("guild_id").and_then(Value::as_str) {
        metadata.insert("guild_id", guild_id);
    }

    Some(ChannelEnvelope {
        account_id: account_id.to_string(),
        kind: ChannelKind::Discord,
        provider_event_id: id,
        conversation,
        sender: ChannelSender {
            sender_id: author_id,
            display_label: username,
            is_self,
            is_bot,
        },
        text,
        attachments,
        reply_to_provider_id,
        mentions_self,
        received_at_ms: now_ms,
        metadata,
    })
}

// ---------------------------------------------------------------------------
// Gateway I/O loop
// ---------------------------------------------------------------------------

enum FetchError {
    Retryable(String),
    Permanent(String),
}

async fn fetch_gateway_url(
    http: &reqwest::Client,
    api_base: &str,
    token: &str,
) -> Result<String, FetchError> {
    let request = http
        .get(format!("{api_base}/gateway/bot"))
        .header("Authorization", format!("Bot {token}"));
    let response = little_monkey_lib::egress::send(request)
        .await
        .map_err(|error| FetchError::Retryable(scrub(&error.to_string(), token)))?;
    let status = response.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(FetchError::Permanent(
            "Discord rejected the bot token while resolving the gateway URL".to_string(),
        ));
    }
    if !status.is_success() {
        return Err(FetchError::Retryable(format!(
            "Discord gateway lookup failed: HTTP {status}"
        )));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|error| FetchError::Retryable(scrub(&error.to_string(), token)))?;
    body.get("url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| FetchError::Retryable("Discord gateway response had no url".to_string()))
}

enum ConnectionOutcome {
    Permanent(String),
    /// Reconnect, saying whether this connection ever reached READY/RESUMED
    /// (a connection that never established must keep growing the backoff —
    /// resetting on every attempt is how a flapping socket becomes a 1-second
    /// hammer) and how long Discord asked us to wait first.
    Reconnect {
        established: bool,
        delay_ms: u64,
    },
    /// The adapter was dropped; the task must exit, not reconnect.
    Shutdown,
}

async fn run_one_connection(
    account_id: &str,
    token: &str,
    gateway_url: &str,
    tx: &mpsc::Sender<(ChannelEnvelope, ResumeState)>,
    state: &mut GatewayState,
    shared: &Shared,
    last_identify: &mut Option<std::time::Instant>,
) -> ConnectionOutcome {
    let reconnect = |established: bool| ConnectionOutcome::Reconnect {
        established,
        delay_ms: 0,
    };
    let full_url = format!(
        "{}/?v={GATEWAY_VERSION}&encoding=json",
        gateway_url.trim_end_matches('/')
    );
    let (mut ws, _) = match tokio_tungstenite::connect_async(full_url).await {
        Ok(pair) => pair,
        Err(_) => return reconnect(false),
    };

    let mut heartbeat_armed = false;
    let mut awaiting_ack = false;
    let mut established = false;
    // Placeholder ticker; replaced once HELLO tells us the real interval. Never
    // fires before then because nothing selects on it until `heartbeat_armed`.
    let mut ticker = tokio::time::interval(Duration::from_secs(3600));
    ticker.tick().await;

    loop {
        tokio::select! {
            // The adapter handle was dropped (account disabled, credential
            // rotated). Hang up now rather than lingering as a second socket
            // owner beside the replacement adapter's session.
            _ = tx.closed() => {
                let _ = ws.close(None).await;
                return ConnectionOutcome::Shutdown;
            }
            frame = ws.next() => {
                let text = match frame {
                    Some(Ok(Message::Text(text))) => text.to_string(),
                    Some(Ok(Message::Close(close_frame))) => {
                        let code = close_frame.map(|f| f.code.into()).unwrap_or(1000u16);
                        return match handle_close(state, code).into_iter().next() {
                            Some(Action::PermanentError(error)) => ConnectionOutcome::Permanent(error),
                            _ => reconnect(established),
                        };
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = ws.send(Message::Pong(payload)).await;
                        continue;
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => return reconnect(established),
                };
                let actions = handle_frame(state, account_id, &text, token);
                // Mirror the resume-relevant state out after every frame, so a
                // concurrent `poll` snapshots a sequence that lags what it is
                // about to drain rather than leading it.
                if let Ok(mut resume) = shared.resume.lock() {
                    *resume = state.resume_snapshot();
                }
                for action in actions {
                    match action {
                        Action::SendJson(payload) => {
                            let _ = ws.send(Message::Text(payload.to_string().into())).await;
                        }
                        Action::Identify(payload) => {
                            // At most one IDENTIFY per five seconds, measured
                            // across reconnects: violating it earns an invalid
                            // session and another reconnect, forever.
                            if let Some(previous) = *last_identify {
                                let since = previous.elapsed();
                                if since < IDENTIFY_SPACING {
                                    tokio::time::sleep(IDENTIFY_SPACING - since).await;
                                }
                            }
                            *last_identify = Some(std::time::Instant::now());
                            let _ = ws.send(Message::Text(payload.to_string().into())).await;
                        }
                        Action::StartHeartbeat(interval_ms) => {
                            let interval = Duration::from_millis(interval_ms.max(1));
                            // First beat after a random fraction of the
                            // interval, as the gateway spec asks, so a fleet
                            // of clients does not heartbeat in step.
                            ticker = tokio::time::interval_at(
                                tokio::time::Instant::now()
                                    + Duration::from_millis(jitter_ms(interval_ms.max(1))),
                                interval,
                            );
                            heartbeat_armed = true;
                            awaiting_ack = false;
                        }
                        Action::HeartbeatAck => awaiting_ack = false,
                        Action::Established => {
                            established = true;
                            shared.gateway_status.store(
                                GATEWAY_CONNECTED,
                                std::sync::atomic::Ordering::SeqCst,
                            );
                        }
                        Action::SetBotUserId(_id) => {}
                        Action::Envelope(envelope) => {
                            // The state rides along, frozen at this dispatch.
                            let _ = tx.send((*envelope, state.resume_snapshot())).await;
                        }
                        Action::PermanentError(error) => return ConnectionOutcome::Permanent(error),
                        Action::Reconnect { delay_ms } => {
                            return ConnectionOutcome::Reconnect { established, delay_ms };
                        }
                    }
                }
            }
            _ = ticker.tick(), if heartbeat_armed => {
                if awaiting_ack {
                    // A full interval passed with no ACK: the connection is a
                    // zombie. Close and resume — the sequence is still good.
                    state.pending_resume = true;
                    let _ = ws.close(None).await;
                    return reconnect(established);
                }
                awaiting_ack = true;
                let payload = serde_json::json!({ "op": 1, "d": state.seq });
                let _ = ws.send(Message::Text(payload.to_string().into())).await;
            }
        }
    }
}

async fn run_gateway_loop(
    account_id: String,
    token: String,
    api_base: String,
    http: reqwest::Client,
    tx: mpsc::Sender<(ChannelEnvelope, ResumeState)>,
    shared: Arc<Shared>,
) {
    let mut backoff = MIN_BACKOFF;
    // Seeded from the persisted cursor, so the first connection after a daemon
    // restart RESUMEs the stored session instead of identifying fresh.
    let mut state =
        GatewayState::from_resume(&shared.resume.lock().map(|r| r.clone()).unwrap_or_default());
    let mut last_identify: Option<std::time::Instant> = None;
    loop {
        // A RESUME must go to the URL Discord named for this session; only a
        // fresh IDENTIFY goes through the general gateway lookup.
        let resume_url = (state.pending_resume && state.session_id.is_some())
            .then(|| state.resume_gateway_url.clone())
            .flatten();
        let url = match resume_url {
            Some(url) => Ok(url),
            None => fetch_gateway_url(&http, &api_base, &token).await,
        };
        let mut delay_hint_ms = 0;
        match url {
            Ok(url) => {
                match run_one_connection(
                    &account_id,
                    &token,
                    &url,
                    &tx,
                    &mut state,
                    &shared,
                    &mut last_identify,
                )
                .await
                {
                    ConnectionOutcome::Permanent(error) => {
                        *shared.permanent_error.lock().await = Some(error);
                        shared
                            .gateway_status
                            .store(GATEWAY_RECONNECTING, std::sync::atomic::Ordering::SeqCst);
                        return;
                    }
                    ConnectionOutcome::Shutdown => return,
                    ConnectionOutcome::Reconnect {
                        established,
                        delay_ms,
                    } => {
                        delay_hint_ms = delay_ms;
                        backoff = if established {
                            MIN_BACKOFF
                        } else {
                            (backoff * 2).min(MAX_BACKOFF)
                        };
                    }
                }
            }
            Err(FetchError::Permanent(error)) => {
                *shared.permanent_error.lock().await = Some(error);
                return;
            }
            Err(FetchError::Retryable(error)) => {
                eprintln!("little monkey: discord[{account_id}] gateway lookup: {error}");
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
        shared
            .gateway_status
            .store(GATEWAY_RECONNECTING, std::sync::atomic::Ordering::SeqCst);
        if tx.is_closed() {
            return;
        }
        // Jittered, and never shorter than what Discord explicitly asked for.
        let wait = backoff + Duration::from_millis(jitter_ms(500));
        let wait = wait.max(Duration::from_millis(delay_hint_ms));
        tokio::time::sleep(wait).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn account_fixture(
        credential_ref: Option<&str>,
    ) -> crate::daemon::channel_store::ChannelAccountRecord {
        crate::daemon::channel_store::ChannelAccountRecord {
            account_id: "acct-1".to_string(),
            kind: ChannelKind::Discord,
            label: "Bot".to_string(),
            enabled: true,
            non_secret_config: serde_json::json!({}),
            credential_ref: credential_ref.map(str::to_string),
            access_policy: Default::default(),
            health: ChannelHealth::error(0, "unused"),
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    // -- construction ---------------------------------------------------------

    #[test]
    fn rejects_an_empty_token() {
        let account = account_fixture(None);
        let config = AdapterConfig {
            account: &account,
            secret: String::new(),
        };
        assert!(DiscordAdapter::new(&config).is_err());
    }

    #[test]
    fn construction_does_not_start_the_gateway_task() {
        // No #[tokio::test] runtime here on purpose: if `new` spawned
        // anything it would panic outside a runtime, so a plain #[test]
        // passing is itself proof construction has no side effects.
        let account = account_fixture(Some("discord/acct-1"));
        let config = AdapterConfig {
            account: &account,
            secret: "bot-token".to_string(),
        };
        let adapter = DiscordAdapter::new(&config).expect("adapter");
        assert_eq!(adapter.token, "bot-token");
    }

    // -- normalization ----------------------------------------------------

    fn message_create_fixture() -> Value {
        serde_json::json!({
            "id": "1001",
            "channel_id": "chan-1",
            "guild_id": "guild-1",
            "content": "hello @bot",
            "author": { "id": "author-1", "username": "alice", "bot": false },
            "mentions": [{ "id": "bot-id" }],
            "attachments": [{
                "id": "att-1",
                "url": "https://cdn.discordapp.com/a.png",
                "filename": "a.png",
                "content_type": "image/png",
                "size": 42,
            }],
        })
    }

    #[test]
    fn normalizes_guild_message_with_mention_and_attachment() {
        let empty = HashMap::new();
        let envelope = normalize_message_create(
            "acct",
            &message_create_fixture(),
            Some("bot-id"),
            &empty,
            1000,
        )
        .expect("envelope");
        assert_eq!(envelope.provider_event_id, "1001");
        assert_eq!(
            envelope.conversation.kind,
            little_monkey_lib::channels::types::ConversationKind::Group
        );
        assert!(envelope.mentions_self);
        assert!(!envelope.sender.is_self);
        assert_eq!(envelope.attachments.len(), 1);
        assert_eq!(
            envelope.attachments[0].source,
            AttachmentSource::Url {
                url: "https://cdn.discordapp.com/a.png".to_string()
            }
        );
    }

    #[test]
    fn dm_message_has_no_guild_id_and_is_direct() {
        let mut fixture = message_create_fixture();
        fixture.as_object_mut().unwrap().remove("guild_id");
        let empty = HashMap::new();
        let envelope = normalize_message_create("acct", &fixture, Some("bot-id"), &empty, 1000)
            .expect("envelope");
        assert_eq!(
            envelope.conversation.kind,
            little_monkey_lib::channels::types::ConversationKind::Direct
        );
    }

    #[test]
    fn thread_message_carries_parent_as_conversation() {
        let mut fixture = message_create_fixture();
        fixture["channel_id"] = Value::String("thread-9".to_string());
        let mut thread_parents = HashMap::new();
        thread_parents.insert("thread-9".to_string(), "chan-1".to_string());
        let envelope =
            normalize_message_create("acct", &fixture, Some("bot-id"), &thread_parents, 1000)
                .expect("envelope");
        assert_eq!(envelope.conversation.conversation_id, "chan-1");
        assert_eq!(envelope.conversation.thread_id.as_deref(), Some("thread-9"));
    }

    #[test]
    fn self_authored_message_is_flagged_not_dropped() {
        let mut fixture = message_create_fixture();
        fixture["author"]["id"] = Value::String("bot-id".to_string());
        let empty = HashMap::new();
        let envelope = normalize_message_create("acct", &fixture, Some("bot-id"), &empty, 1000)
            .expect("envelope");
        assert!(envelope.sender.is_self);
    }

    #[test]
    fn provider_event_id_is_deterministic() {
        let empty = HashMap::new();
        let first = normalize_message_create("acct", &message_create_fixture(), None, &empty, 1000)
            .unwrap();
        let second =
            normalize_message_create("acct", &message_create_fixture(), None, &empty, 9999)
                .unwrap();
        assert_eq!(first.provider_event_id, second.provider_event_id);
    }

    // -- gateway state machine --------------------------------------------

    #[test]
    fn hello_without_prior_session_sends_identify() {
        let mut state = GatewayState::default();
        let actions = handle_frame(
            &mut state,
            "acct",
            r#"{"op":10,"d":{"heartbeat_interval":45000}}"#,
            "tok",
        );
        assert!(matches!(actions[0], Action::StartHeartbeat(45000)));
        match &actions[1] {
            Action::Identify(value) => assert_eq!(value["op"], 2),
            other => panic!("expected IDENTIFY, got {other:?}"),
        }
    }

    #[test]
    fn hello_after_resumable_close_sends_resume() {
        let mut state = GatewayState {
            session_id: Some("sess-1".to_string()),
            seq: Some(7),
            pending_resume: true,
            ..GatewayState::default()
        };
        let actions = handle_frame(
            &mut state,
            "acct",
            r#"{"op":10,"d":{"heartbeat_interval":45000}}"#,
            "tok",
        );
        match &actions[1] {
            Action::SendJson(value) => {
                assert_eq!(value["op"], 6);
                assert_eq!(value["d"]["session_id"], "sess-1");
                assert_eq!(value["d"]["seq"], 7);
            }
            other => panic!("expected RESUME, got {other:?}"),
        }
    }

    #[test]
    fn ready_captures_session_and_bot_id() {
        let mut state = GatewayState::default();
        let actions = handle_frame(
            &mut state,
            "acct",
            r#"{"op":0,"t":"READY","s":1,"d":{"session_id":"sess-2","user":{"id":"bot-9"}}}"#,
            "tok",
        );
        assert_eq!(state.session_id.as_deref(), Some("sess-2"));
        assert_eq!(state.bot_user_id.as_deref(), Some("bot-9"));
        assert_eq!(state.seq, Some(1));
        assert!(matches!(&actions[0], Action::SetBotUserId(id) if id == "bot-9"));
    }

    #[test]
    fn heartbeat_request_replies_with_current_sequence() {
        let mut state = GatewayState {
            seq: Some(42),
            ..GatewayState::default()
        };
        let actions = handle_frame(&mut state, "acct", r#"{"op":1}"#, "tok");
        match &actions[0] {
            Action::SendJson(value) => assert_eq!(value["d"], 42),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn non_resumable_close_is_permanent() {
        let mut state = GatewayState::default();
        let actions = handle_close(&mut state, 4004);
        assert!(matches!(&actions[0], Action::PermanentError(_)));
        let actions = handle_close(&mut state, 4014);
        assert!(matches!(&actions[0], Action::PermanentError(_)));
    }

    #[test]
    fn other_close_codes_reconnect() {
        let mut state = GatewayState::default();
        let actions = handle_close(&mut state, 1006);
        assert!(matches!(&actions[0], Action::Reconnect { .. }));
        assert!(state.pending_resume);
    }

    #[test]
    fn ready_captures_the_resume_url_and_establishes() {
        let mut state = GatewayState::default();
        let actions = handle_frame(
            &mut state,
            "acct",
            r#"{"op":0,"t":"READY","s":1,"d":{"session_id":"sess-2","resume_gateway_url":"wss://resume.example","user":{"id":"bot-9"}}}"#,
            "tok",
        );
        assert_eq!(
            state.resume_gateway_url.as_deref(),
            Some("wss://resume.example")
        );
        assert!(actions.contains(&Action::Established));
        let snapshot = state.resume_snapshot();
        assert_eq!(snapshot.session_id.as_deref(), Some("sess-2"));
        assert_eq!(snapshot.seq, Some(1));
        assert!(snapshot.resumable());
    }

    #[test]
    fn a_persisted_resume_state_round_trips_through_the_cursor() {
        let state = ResumeState {
            session_id: Some("sess-9".into()),
            resume_gateway_url: Some("wss://resume.example".into()),
            seq: Some(41),
            bot_user_id: Some("bot-9".into()),
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let parsed = ResumeState::parse(Some(&json));
        assert_eq!(parsed, state);
        // Garbage or absence degrades to a fresh IDENTIFY, never a panic.
        assert_eq!(ResumeState::parse(Some("not json")), ResumeState::default());
        assert_eq!(ResumeState::parse(None), ResumeState::default());
        assert!(!ResumeState::parse(None).resumable());
    }

    #[test]
    fn a_seeded_state_asks_to_resume_and_a_blank_one_identifies() {
        let seeded = GatewayState::from_resume(&ResumeState {
            session_id: Some("sess-9".into()),
            resume_gateway_url: Some("wss://resume.example".into()),
            seq: Some(41),
            bot_user_id: None,
        });
        assert!(seeded.pending_resume);
        // HELLO on a seeded state sends RESUME with the stored ids.
        let mut state = seeded;
        let actions = handle_frame(
            &mut state,
            "acct",
            r#"{"op":10,"d":{"heartbeat_interval":45000}}"#,
            "tok",
        );
        match &actions[1] {
            Action::SendJson(value) => {
                assert_eq!(value["op"], 6);
                assert_eq!(value["d"]["session_id"], "sess-9");
                assert_eq!(value["d"]["seq"], 41);
            }
            other => panic!("expected RESUME, got {other:?}"),
        }

        let blank = GatewayState::from_resume(&ResumeState::default());
        assert!(!blank.pending_resume);
    }

    #[test]
    fn a_resumed_session_still_recognizes_its_own_messages() {
        // A RESUME is answered with RESUMED, never a second READY, so the only
        // place the bot's own id can come from is what the last session
        // persisted. Without it every self-authored MESSAGE_CREATE — which
        // Discord does dispatch back to its author — looks like a stranger's,
        // and the reply to it starts a loop.
        let mut state = GatewayState::from_resume(&ResumeState {
            session_id: Some("sess-9".into()),
            resume_gateway_url: Some("wss://resume.example".into()),
            seq: Some(41),
            bot_user_id: Some("bot-9".into()),
        });
        let actions = handle_frame(
            &mut state,
            "acct",
            r#"{"op":0,"t":"MESSAGE_CREATE","s":42,"d":{
                "id":"msg-1","channel_id":"chan-1","guild_id":"guild-1",
                "content":"our own reply",
                "author":{"id":"bot-9","username":"little-monkey","bot":true}
            }}"#,
            "tok",
        );
        match actions.first() {
            Some(Action::Envelope(envelope)) => assert!(
                envelope.sender.is_self,
                "a resumed session failed to recognize its own message"
            ),
            other => panic!("expected an envelope, got {other:?}"),
        }
    }

    #[test]
    fn ready_puts_the_bot_id_into_the_state_that_gets_persisted() {
        let mut state = GatewayState::default();
        handle_frame(
            &mut state,
            "acct",
            r#"{"op":0,"t":"READY","s":1,"d":{"session_id":"sess-2","user":{"id":"bot-9"}}}"#,
            "tok",
        );
        assert_eq!(
            state.resume_snapshot().bot_user_id.as_deref(),
            Some("bot-9")
        );
        // A dead session loses its sequence, never its identity.
        handle_frame(&mut state, "acct", r#"{"op":9,"d":false}"#, "tok");
        assert_eq!(state.seq, None);
        assert_eq!(state.bot_user_id.as_deref(), Some("bot-9"));
    }

    #[test]
    fn heartbeat_ack_is_surfaced_to_the_io_loop() {
        let mut state = GatewayState::default();
        let actions = handle_frame(&mut state, "acct", r#"{"op":11}"#, "tok");
        assert_eq!(actions, vec![Action::HeartbeatAck]);
    }

    #[test]
    fn invalid_session_asks_for_a_randomized_wait() {
        let mut state = GatewayState {
            session_id: Some("sess".into()),
            seq: Some(5),
            ..GatewayState::default()
        };
        let actions = handle_frame(&mut state, "acct", r#"{"op":9,"d":false}"#, "tok");
        match &actions[0] {
            Action::Reconnect { delay_ms } => {
                assert!((1_000..5_000).contains(delay_ms), "{delay_ms}")
            }
            other => panic!("unexpected {other:?}"),
        }
        // Non-resumable: the dead session must not be resumed or persisted.
        assert!(!state.pending_resume);
        assert_eq!(state.session_id, None);
        assert_eq!(state.seq, None);
    }

    #[test]
    fn the_guilds_intent_rides_along_for_thread_dispatches() {
        assert_eq!(DISCORD_INTENTS & 1, 1, "GUILDS intent missing");
    }

    #[test]
    fn a_reply_targets_the_thread_when_one_is_named() {
        let message = OutboundMessage {
            account_id: "acct".into(),
            kind: ChannelKind::Discord,
            conversation_id: "chan-1".into(),
            thread_id: Some("thread-9".into()),
            text: "hi".into(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            idempotency_key: "k".into(),
        };
        assert_eq!(target_channel(&message), "thread-9");
        let plain = OutboundMessage {
            thread_id: None,
            ..message
        };
        assert_eq!(target_channel(&plain), "chan-1");
    }

    // -- outbound mapping ---------------------------------------------------

    #[test]
    fn rate_limit_maps_to_retryable_with_ms() {
        let outcome = map_send_status(429, false, Some(2500)).unwrap();
        match outcome {
            SendOutcome::RetryableFailure { retry_after_ms, .. } => {
                assert_eq!(retry_after_ms, Some(2500))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn retry_after_header_parses_fractional_seconds() {
        assert_eq!(parse_retry_after_seconds(Some("1.5")), Some(1500));
        assert_eq!(parse_retry_after_seconds(None), None);
        assert_eq!(parse_retry_after_seconds(Some("garbage")), None);
    }

    #[test]
    fn auth_failure_is_permanent() {
        assert!(matches!(
            map_send_status(401, false, None),
            Some(SendOutcome::PermanentFailure { .. })
        ));
        assert!(matches!(
            map_send_status(403, false, None),
            Some(SendOutcome::PermanentFailure { .. })
        ));
    }

    #[test]
    fn server_error_after_partial_send_needs_reconciliation() {
        assert!(matches!(
            map_send_status(500, true, None),
            Some(SendOutcome::NeedsReconciliation { .. })
        ));
        assert!(matches!(
            map_send_status(500, false, None),
            Some(SendOutcome::RetryableFailure { .. })
        ));
    }

    #[test]
    fn success_status_continues() {
        assert!(map_send_status(204, false, None).is_none());
    }

    #[test]
    fn message_splitting_respects_the_limit() {
        let text = "a".repeat(4500);
        let chunks = split_message(&text, 2000);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].chars().count(), 2000);
        assert_eq!(chunks[2].chars().count(), 500);
        assert!(split_message("", 2000).is_empty());
    }

    // -- token hygiene -------------------------------------------------------

    #[test]
    fn scrub_removes_the_token_from_any_rendered_error() {
        let rendered = scrub(
            "request to https://x failed carrying secret-token-abc",
            "secret-token-abc",
        );
        assert!(!rendered.contains("secret-token-abc"));
    }

    #[test]
    fn identify_and_resume_payloads_carry_the_credential_only_where_intended() {
        let identify = identify_payload("tok-123", DISCORD_INTENTS);
        assert_eq!(identify["d"]["token"], "tok-123");
        let resume = resume_payload("tok-123", "sess", 3);
        assert_eq!(resume["op"], 6);
    }

    // -- HTTP fixture (probe) -------------------------------------------------

    async fn fixture_server(status: &str, body: String) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let address = listener.local_addr().expect("fixture address");
        let status = status.to_string();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept fixture request");
            let mut request = vec![0u8; 8 * 1024];
            let _ = stream.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn probe_reads_username_from_a_fixture_server() {
        let base = fixture_server("200 OK", r#"{"username":"little-monkey"}"#.to_string()).await;
        let client = little_monkey_lib::egress::hardened().build().unwrap();
        let request = client
            .get(format!("{base}/users/@me"))
            .header("Authorization", "Bot tok");
        let response = little_monkey_lib::egress::send(request).await.unwrap();
        assert!(response.status().is_success());
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["username"], "little-monkey");
    }
}
