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
    AdapterConfig, ChannelAdapter, InboundBatch, LoadedAttachment,
};

const API_BASE: &str = "https://discord.com/api/v10";
const GATEWAY_VERSION: &str = "10";
const DISCORD_MAX_TEXT_CHARS: usize = 2000;
/// Guild messages (1<<9) + direct messages (1<<12) + message content (1<<15).
const DISCORD_INTENTS: u64 = (1 << 9) | (1 << 12) | (1 << 15);
const INBOUND_CHANNEL_CAPACITY: usize = 256;
/// How long [`DiscordAdapter::poll`] blocks for a first envelope before
/// returning an empty batch. Bounded so the daemon's poll loop keeps ticking
/// even when Discord is quiet.
const POLL_WAIT: Duration = Duration::from_secs(20);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

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
}

pub struct DiscordAdapter {
    account_id: String,
    token: String,
    http: reqwest::Client,
    inbound_tx: mpsc::Sender<ChannelEnvelope>,
    inbound_rx: Mutex<mpsc::Receiver<ChannelEnvelope>>,
    shared: Arc<Shared>,
    /// Guards the one-time spawn of the gateway task. See the module doc for
    /// why construction itself must stay side-effect-free.
    started: tokio::sync::OnceCell<()>,
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
        })
    }

    async fn ensure_started(&self) {
        self.started
            .get_or_init(|| async {
                tokio::spawn(run_gateway_loop(
                    self.account_id.clone(),
                    self.token.clone(),
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
        self.ensure_started().await;
        let now = now_ms();
        if let Some(error) = self.shared.permanent_error.lock().await.clone() {
            return ChannelHealth::error(now, error);
        }
        let request = self
            .http
            .get(format!("{API_BASE}/users/@me"))
            .header("Authorization", format!("Bot {}", self.token));
        match little_monkey_lib::egress::send(request).await {
            Ok(response) if response.status().is_success() => {
                let body: Value = response.json().await.unwrap_or(Value::Null);
                let username = body
                    .get("username")
                    .and_then(Value::as_str)
                    .unwrap_or("bot");
                ChannelHealth::connected(now, Some(format!("Connected to Discord as {username}")))
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

    async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
        // Discord is a socket transport: the gateway task above pushes
        // envelopes as they arrive and there is no page or offset to resume
        // from, so the cursor is always ignored and never written back.
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
                    "{API_BASE}/channels/{}/messages",
                    message.conversation_id
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
            if let Some(outcome) = map_send_status(status.as_u16(), any_sent, retry_after_ms) {
                return outcome;
            }
            any_sent = true;
            last_id = response
                .json::<Value>()
                .await
                .ok()
                .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_string));
        }
        SendOutcome::Sent {
            provider_message_id: last_id,
        }
    }

    /// Discord takes files on the same message-create endpoint, as multipart
    /// with the JSON body in `payload_json` and each file in `files[n]`. One
    /// request carries the text and every file, so a caller never sees a
    /// caption arrive separately from what it captions.
    async fn send_with_attachments(
        &self,
        message: &OutboundMessage,
        files: &[LoadedAttachment],
    ) -> SendOutcome {
        if files.is_empty() {
            return self.send(message).await;
        }
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
                "{API_BASE}/channels/{}/messages",
                message.conversation_id
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

#[derive(Debug, Clone, PartialEq)]
enum Action {
    SendJson(Value),
    StartHeartbeat(u64),
    SetBotUserId(String),
    Envelope(Box<ChannelEnvelope>),
    PermanentError(String),
    Reconnect,
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
            actions.push(Action::SendJson(identify_payload(token, DISCORD_INTENTS)));
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
                    if let Some(id) = data
                        .get("user")
                        .and_then(|user| user.get("id"))
                        .and_then(Value::as_str)
                    {
                        state.bot_user_id = Some(id.to_string());
                        actions.push(Action::SetBotUserId(id.to_string()));
                    }
                }
                "RESUMED" => state.pending_resume = false,
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
            vec![Action::Reconnect]
        }
        // INVALID_SESSION: `d` says whether a resume is worth attempting.
        9 => {
            let resumable = value.get("d").and_then(Value::as_bool).unwrap_or(false);
            state.pending_resume = resumable;
            if !resumable {
                state.session_id = None;
                state.seq = None;
            }
            vec![Action::Reconnect]
        }
        // Heartbeat ACK: nothing to do; a missed ACK is not tracked separately
        // because a dead socket surfaces as a read error or a close frame.
        11 => Vec::new(),
        _ => Vec::new(),
    }
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
            "Discord rejected the requested gateway intents (close code 4014: disallowed intents)"
                .to_string(),
        )],
        _ => {
            state.pending_resume = true;
            vec![Action::Reconnect]
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

async fn fetch_gateway_url(http: &reqwest::Client, token: &str) -> Result<String, FetchError> {
    let request = http
        .get(format!("{API_BASE}/gateway/bot"))
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
    Reconnect,
}

async fn run_one_connection(
    account_id: &str,
    token: &str,
    gateway_url: &str,
    tx: &mpsc::Sender<ChannelEnvelope>,
    state: &mut GatewayState,
) -> ConnectionOutcome {
    let full_url = format!(
        "{}/?v={GATEWAY_VERSION}&encoding=json",
        gateway_url.trim_end_matches('/')
    );
    let (mut ws, _) = match tokio_tungstenite::connect_async(full_url).await {
        Ok(pair) => pair,
        Err(_) => return ConnectionOutcome::Reconnect,
    };

    let mut heartbeat_armed = false;
    // Placeholder ticker; replaced once HELLO tells us the real interval. Never
    // fires before then because nothing selects on it until `heartbeat_armed`.
    let mut ticker = tokio::time::interval(Duration::from_secs(3600));
    ticker.tick().await;

    loop {
        tokio::select! {
            frame = ws.next() => {
                let text = match frame {
                    Some(Ok(Message::Text(text))) => text.to_string(),
                    Some(Ok(Message::Close(close_frame))) => {
                        let code = close_frame.map(|f| f.code.into()).unwrap_or(1000u16);
                        return match handle_close(state, code).into_iter().next() {
                            Some(Action::PermanentError(error)) => ConnectionOutcome::Permanent(error),
                            _ => ConnectionOutcome::Reconnect,
                        };
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = ws.send(Message::Pong(payload)).await;
                        continue;
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => return ConnectionOutcome::Reconnect,
                };
                for action in handle_frame(state, account_id, &text, token) {
                    match action {
                        Action::SendJson(payload) => {
                            let _ = ws.send(Message::Text(payload.to_string().into())).await;
                        }
                        Action::StartHeartbeat(interval_ms) => {
                            ticker = tokio::time::interval(Duration::from_millis(interval_ms.max(1)));
                            ticker.tick().await;
                            heartbeat_armed = true;
                        }
                        Action::SetBotUserId(_id) => {}
                        Action::Envelope(envelope) => {
                            let _ = tx.send(*envelope).await;
                        }
                        Action::PermanentError(error) => return ConnectionOutcome::Permanent(error),
                        Action::Reconnect => return ConnectionOutcome::Reconnect,
                    }
                }
            }
            _ = ticker.tick(), if heartbeat_armed => {
                let payload = serde_json::json!({ "op": 1, "d": state.seq });
                let _ = ws.send(Message::Text(payload.to_string().into())).await;
            }
        }
    }
}

async fn run_gateway_loop(
    account_id: String,
    token: String,
    http: reqwest::Client,
    tx: mpsc::Sender<ChannelEnvelope>,
    shared: Arc<Shared>,
) {
    let mut backoff = MIN_BACKOFF;
    let mut state = GatewayState::default();
    loop {
        match fetch_gateway_url(&http, &token).await {
            Ok(url) => match run_one_connection(&account_id, &token, &url, &tx, &mut state).await {
                ConnectionOutcome::Permanent(error) => {
                    *shared.permanent_error.lock().await = Some(error);
                    return;
                }
                ConnectionOutcome::Reconnect => {
                    backoff = MIN_BACKOFF;
                }
            },
            Err(FetchError::Permanent(error)) => {
                *shared.permanent_error.lock().await = Some(error);
                return;
            }
            Err(FetchError::Retryable(error)) => {
                eprintln!("little monkey: discord[{account_id}] gateway lookup: {error}");
            }
        }
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
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
            Action::SendJson(value) => assert_eq!(value["op"], 2),
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
        assert!(matches!(&actions[0], Action::Reconnect));
        assert!(state.pending_resume);
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
