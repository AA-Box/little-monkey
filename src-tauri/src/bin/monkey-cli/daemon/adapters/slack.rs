//! Slack adapter: Socket Mode inbound, Web API outbound.
//!
//! # Two tokens, one secret
//!
//! Slack Socket Mode needs an app-level token (`xapp-…`, opens the socket via
//! `apps.connections.open`) *and* a bot token (`xoxb-…`, used for every Web
//! API call). Both are credentials, so both live in the keychain secret rather
//! than one being treated as `non_secret_config`. The stored secret is a JSON
//! object:
//!
//! ```json
//! { "bot_token": "xoxb-...", "app_token": "xapp-..." }
//! ```
//!
//! which the setup UI is expected to write; [`parse_secret`] is the one place
//! that shape is read back.
//!
//! # Shape
//!
//! A single task (spawned in [`SlackAdapter::new`]) resolves our own bot
//! identity once via `auth.test`, then owns the Socket Mode connection for the
//! adapter's lifetime: every incoming envelope is acknowledged immediately
//! (Slack redelivers an unacknowledged envelope), and `events_api` envelopes
//! carrying a `message` event are normalized and pushed into a bounded
//! channel. [`ChannelAdapter::poll`] only drains that channel. The envelope
//! framing is [`handle_socket_frame`], a pure function so it is testable
//! without a socket.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use little_monkey_lib::channels::types::{
    AttachmentSource, ChannelAttachment, ChannelConversation, ChannelEnvelope, ChannelHealth,
    ChannelKind, ChannelSender, InboundTransport, OutboundMessage, ProviderCapabilities,
    SendOutcome,
};
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

use crate::daemon::channel_adapter::{
    AdapterConfig, ChannelAdapter, InboundBatch, LoadedAttachment,
};

const API_BASE: &str = "https://slack.com/api";
/// Slack's `chat.postMessage` `text` field limit.
const SLACK_MAX_TEXT_CHARS: usize = 40_000;
const INBOUND_CHANNEL_CAPACITY: usize = 256;
const POLL_WAIT: Duration = Duration::from_secs(20);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq)]
struct SlackSecret {
    bot_token: String,
    app_token: String,
}

/// Parses the `{"bot_token":…,"app_token":…}` secret shape documented at the
/// top of this file. Neither token is logged on failure — only the shape
/// error is.
fn parse_secret(secret: &str) -> Result<SlackSecret, String> {
    let value: Value = serde_json::from_str(secret)
        .map_err(|_| "Slack credential is not the expected JSON object".to_string())?;
    let bot_token = value
        .get("bot_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "Slack credential is missing bot_token".to_string())?
        .to_string();
    let app_token = value
        .get("app_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "Slack credential is missing app_token".to_string())?
        .to_string();
    Ok(SlackSecret {
        bot_token,
        app_token,
    })
}

#[derive(Default)]
struct Shared {
    permanent_error: Mutex<Option<String>>,
}

pub struct SlackAdapter {
    account_id: String,
    secret: SlackSecret,
    http: reqwest::Client,
    inbound_tx: mpsc::Sender<ChannelEnvelope>,
    inbound_rx: Mutex<mpsc::Receiver<ChannelEnvelope>>,
    shared: Arc<Shared>,
    /// Guards the one-time spawn of the socket task. `new` itself stays
    /// side-effect-free — see [`DiscordAdapter`](super::discord::DiscordAdapter)'s
    /// module doc for why.
    started: tokio::sync::OnceCell<()>,
}

impl SlackAdapter {
    pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
        let secret = parse_secret(&config.secret)?;
        let http = little_monkey_lib::egress::hardened()
            .build()
            .map_err(|error| format!("Failed to build the Slack HTTP client: {error}"))?;
        let (tx, rx) = mpsc::channel(INBOUND_CHANNEL_CAPACITY);
        Ok(Self {
            account_id: config.account.account_id.clone(),
            secret,
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
                    self.secret.clone(),
                    self.http.clone(),
                    self.inbound_tx.clone(),
                    self.shared.clone(),
                ));
            })
            .await;
    }
}

#[async_trait]
impl ChannelAdapter for SlackAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Slack
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            max_text_chars: SLACK_MAX_TEXT_CHARS,
            supports_threads: true,
            supports_attachments: true,
            // Slack's events carry no first-class "you were mentioned" flag —
            // mention gating has to scan `text` for `<@BOT_ID>` itself.
            supports_mention_metadata: false,
            supports_idempotency_key: false,
            supports_delivery_receipts: false,
            ..ProviderCapabilities::minimal(ChannelKind::Slack, InboundTransport::Socket)
        }
    }

    async fn probe(&self) -> ChannelHealth {
        self.ensure_started().await;
        let now = now_ms();
        if let Some(error) = self.shared.permanent_error.lock().await.clone() {
            return ChannelHealth::error(now, error);
        }
        match auth_test(&self.http, &self.secret.bot_token).await {
            Ok(identity) if identity.ok => ChannelHealth::connected(
                now,
                Some(format!("Connected to Slack as {}", identity.user_id)),
            ),
            Ok(identity) => ChannelHealth::error(
                now,
                scrub(
                    &format!("Slack probe failed: {}", identity.error),
                    &self.secret.bot_token,
                ),
            ),
            Err(error) => ChannelHealth::error(
                now,
                scrub(
                    &format!("Slack probe failed: {error}"),
                    &self.secret.bot_token,
                ),
            ),
        }
    }

    async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
        // Socket Mode has no page or offset to resume from; the socket task
        // pushes as it receives, so cursor is always ignored.
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
        let mut chunks = split_message(&message.text, SLACK_MAX_TEXT_CHARS);
        if chunks.is_empty() {
            chunks.push(String::new());
        }
        let mut any_sent = false;
        let mut last_ts = None;
        for chunk in &chunks {
            let mut body = serde_json::json!({
                "channel": message.conversation_id,
                "text": chunk,
            });
            if let Some(thread_id) = &message.thread_id {
                body["thread_ts"] = Value::String(thread_id.clone());
            }
            let request = self
                .http
                .post(format!("{API_BASE}/chat.postMessage"))
                .header("Authorization", format!("Bearer {}", self.secret.bot_token))
                .json(&body);
            let response = match little_monkey_lib::egress::send(request).await {
                Ok(response) => response,
                Err(error) => {
                    let error = scrub(&error.to_string(), &self.secret.bot_token);
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
            let body: Value = response.json().await.unwrap_or(Value::Null);
            match map_send_response(status, retry_after_ms, &body) {
                SendOutcome::Sent {
                    provider_message_id,
                } => {
                    any_sent = true;
                    last_ts = provider_message_id;
                }
                outcome if any_sent => {
                    return match outcome {
                        SendOutcome::RetryableFailure { error, .. } => {
                            SendOutcome::NeedsReconciliation { error }
                        }
                        other => other,
                    };
                }
                outcome => return outcome,
            }
        }
        SendOutcome::Sent {
            provider_message_id: last_ts,
        }
    }

    /// Slack's current upload flow is three calls: ask for a one-time upload
    /// URL, POST the bytes to it, then complete the upload naming the channel.
    /// The older `files.upload` endpoint is retired, so this is the only path
    /// that still works.
    ///
    /// The text rides `initial_comment` on the completing call, so the file and
    /// what the model said about it arrive as one message.
    async fn send_with_attachments(
        &self,
        message: &OutboundMessage,
        files: &[LoadedAttachment],
    ) -> SendOutcome {
        if files.is_empty() {
            return self.send(message).await;
        }
        let mut uploaded: Vec<Value> = Vec::with_capacity(files.len());
        for file in files {
            let request = self
                .http
                .get(format!("{API_BASE}/files.getUploadURLExternal"))
                .header("Authorization", format!("Bearer {}", self.secret.bot_token))
                .query(&[
                    ("filename", file.filename.as_str()),
                    ("length", &file.bytes.len().to_string()),
                ]);
            let response = match little_monkey_lib::egress::send(request).await {
                Ok(response) => response,
                Err(error) => {
                    return SendOutcome::RetryableFailure {
                        error: scrub(&error.to_string(), &self.secret.bot_token),
                        retry_after_ms: None,
                    }
                }
            };
            let body: Value = response.json().await.unwrap_or(Value::Null);
            if !body.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                return SendOutcome::PermanentFailure {
                    error: format!(
                        "Slack refused the upload URL: {}",
                        body.get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown_error")
                    ),
                };
            }
            let (Some(upload_url), Some(file_id)) = (
                body.get("upload_url").and_then(Value::as_str),
                body.get("file_id").and_then(Value::as_str),
            ) else {
                return SendOutcome::PermanentFailure {
                    error: "Slack returned no upload URL".to_string(),
                };
            };
            let part = reqwest::multipart::Part::bytes(file.bytes.clone())
                .file_name(file.filename.clone())
                .mime_str(&file.mime_type)
                .unwrap_or_else(|_| {
                    reqwest::multipart::Part::bytes(file.bytes.clone())
                        .file_name(file.filename.clone())
                });
            // The upload URL is single-use and already carries its own
            // authorization, so the bot token is deliberately not sent to it.
            let upload = self
                .http
                .post(upload_url)
                .multipart(reqwest::multipart::Form::new().part("file", part));
            match little_monkey_lib::egress::send(upload).await {
                Ok(response) if response.status().is_success() => {}
                Ok(response) => {
                    return SendOutcome::PermanentFailure {
                        error: format!("Slack refused the upload ({})", response.status().as_u16()),
                    }
                }
                Err(error) => {
                    let is_connect = error.is_connect();
                    let error = scrub(&error.to_string(), &self.secret.bot_token);
                    return if is_connect {
                        SendOutcome::RetryableFailure {
                            error,
                            retry_after_ms: None,
                        }
                    } else {
                        SendOutcome::NeedsReconciliation { error }
                    };
                }
            }
            uploaded.push(serde_json::json!({ "id": file_id, "title": file.filename }));
        }
        let mut body = serde_json::json!({
            "files": uploaded,
            "channel_id": message.conversation_id,
        });
        if !message.text.is_empty() {
            body["initial_comment"] = Value::String(message.text.clone());
        }
        if let Some(thread_id) = &message.thread_id {
            body["thread_ts"] = Value::String(thread_id.clone());
        }
        let request = self
            .http
            .post(format!("{API_BASE}/files.completeUploadExternal"))
            .header("Authorization", format!("Bearer {}", self.secret.bot_token))
            .json(&body);
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => {
                // The bytes are already in Slack's store; whether the message
                // exists is what is unknown.
                return SendOutcome::NeedsReconciliation {
                    error: scrub(&error.to_string(), &self.secret.bot_token),
                };
            }
        };
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if !body.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            return SendOutcome::PermanentFailure {
                error: format!(
                    "Slack refused to complete the upload: {}",
                    body.get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown_error")
                ),
            };
        }
        SendOutcome::Sent {
            provider_message_id: body
                .get("files")
                .and_then(Value::as_array)
                .and_then(|files| files.first())
                .and_then(|file| file.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string),
        }
    }

    /// Slack shares a file id. `files.info` exchanges it for `url_private`,
    /// which is on Slack's own host and needs the bot token as a bearer header
    /// — fetching it unauthenticated silently returns Slack's HTML sign-in
    /// page rather than an error, which is exactly the kind of "file arrived,
    /// contents are garbage" outcome worth spending a second request to avoid.
    ///
    /// Needs the `files:read` scope; without it Slack answers
    /// `missing_scope` and the attachment is refused by name.
    async fn fetch_attachment(
        &self,
        attachment: &ChannelAttachment,
        max_bytes: u64,
    ) -> Result<Vec<u8>, String> {
        let AttachmentSource::ProviderHandle { handle } = &attachment.source else {
            return Err("This Slack attachment has no file id.".to_string());
        };
        let info = little_monkey_lib::egress::send(
            self.http
                .get(format!("{API_BASE}/files.info"))
                .bearer_auth(&self.secret.bot_token)
                .query(&[("file", handle.as_str())]),
        )
        .await
        .map_err(|error| format!("Slack files.info failed: {error}"))?;
        let body: Value = info.json().await.unwrap_or(Value::Null);
        if !body.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            let reason = body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error");
            return Err(format!("Slack refused files.info: {reason}"));
        }
        let url = body
            .get("file")
            .and_then(|file| file.get("url_private"))
            .and_then(Value::as_str)
            .ok_or_else(|| "Slack returned no private URL for that file".to_string())?;
        crate::daemon::channel_adapter::download_bounded(
            self.http.get(url).bearer_auth(&self.secret.bot_token),
            max_bytes,
        )
        .await
    }
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

fn parse_retry_after_seconds(header_value: Option<&str>) -> Option<i64> {
    let seconds: f64 = header_value?.parse().ok()?;
    Some((seconds * 1000.0).round() as i64)
}

/// Maps one `chat.postMessage` response — the transport-level HTTP status plus
/// Slack's own `ok`/`error` envelope — to a terminal [`SendOutcome`]. Pure so
/// both layers of Slack's "200 OK, ok:false" convention are testable without
/// a socket or an HTTP server.
fn map_send_response(http_status: u16, retry_after_ms: Option<i64>, body: &Value) -> SendOutcome {
    if http_status == 429 {
        return SendOutcome::RetryableFailure {
            error: "Slack rate limited the request".to_string(),
            retry_after_ms,
        };
    }
    if (500..600).contains(&http_status) {
        return SendOutcome::RetryableFailure {
            error: format!("Slack returned HTTP {http_status}"),
            retry_after_ms: None,
        };
    }
    if !(200..300).contains(&http_status) {
        return SendOutcome::PermanentFailure {
            error: format!("Slack rejected the request: HTTP {http_status}"),
        };
    }
    let ok = body.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if ok {
        return SendOutcome::Sent {
            provider_message_id: body.get("ts").and_then(Value::as_str).map(str::to_string),
        };
    }
    let error = body
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown_error");
    match error {
        "ratelimited" => SendOutcome::RetryableFailure {
            error: "Slack rate limited the request (ok:false)".to_string(),
            retry_after_ms,
        },
        other => SendOutcome::PermanentFailure {
            error: format!("Slack rejected the message: {other}"),
        },
    }
}

// ---------------------------------------------------------------------------
// Socket Mode framing (pure)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Action {
    Ack(String),
    Envelope(Box<ChannelEnvelope>),
    Reconnect,
}

/// Handles one Socket Mode text frame. Every envelope that carries an
/// `envelope_id` is acknowledged first and unconditionally — Slack redelivers
/// otherwise — independent of whether the payload turns into a
/// [`ChannelEnvelope`].
fn handle_socket_frame(
    account_id: &str,
    text: &str,
    our_user_id: Option<&str>,
    our_bot_id: Option<&str>,
    now_ms: i64,
) -> Vec<Action> {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let mut actions = Vec::new();
    if let Some(envelope_id) = value.get("envelope_id").and_then(Value::as_str) {
        actions.push(Action::Ack(envelope_id.to_string()));
    }
    match value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        // Slack asks us to open a fresh connection ahead of tearing this one
        // down. ponytail: reconnect-then-drop rather than the overlap-two-
        // sockets dance Slack's docs describe; costs a few seconds of gap
        // per reconnect, upgrade if that gap starts mattering.
        "disconnect" => actions.push(Action::Reconnect),
        "events_api" => {
            if let Some(event) = value
                .get("payload")
                .and_then(|payload| payload.get("event"))
            {
                if event.get("type").and_then(Value::as_str) == Some("message") {
                    if let Some(envelope) =
                        normalize_message_event(account_id, event, our_user_id, our_bot_id, now_ms)
                    {
                        actions.push(Action::Envelope(Box::new(envelope)));
                    }
                }
            }
        }
        _ => {}
    }
    actions
}

fn normalize_message_event(
    account_id: &str,
    event: &Value,
    our_user_id: Option<&str>,
    our_bot_id: Option<&str>,
    now_ms: i64,
) -> Option<ChannelEnvelope> {
    let channel = event.get("channel")?.as_str()?.to_string();
    let ts = event.get("ts")?.as_str()?.to_string();
    let subtype = event.get("subtype").and_then(Value::as_str);
    let bot_id = event.get("bot_id").and_then(Value::as_str);
    let user = event
        .get("user")
        .and_then(Value::as_str)
        .map(str::to_string);
    let sender_id = user
        .clone()
        .or_else(|| bot_id.map(|id| format!("bot:{id}")))
        .unwrap_or_default();

    let is_self = (our_bot_id.is_some() && our_bot_id == bot_id)
        || our_user_id.is_some_and(|id| user.as_deref() == Some(id));
    let is_bot = subtype == Some("bot_message") || bot_id.is_some();

    let provider_event_id = event
        .get("client_msg_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{channel}:{ts}"));

    let thread_id = event
        .get("thread_ts")
        .and_then(Value::as_str)
        .filter(|thread_ts| *thread_ts != ts)
        .map(str::to_string);

    let is_direct = event.get("channel_type").and_then(Value::as_str) == Some("im");
    let conversation = if is_direct {
        ChannelConversation::direct(channel)
    } else {
        ChannelConversation::group(channel)
    }
    .with_thread(thread_id);

    let text = event
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mentions_self = our_user_id.is_some_and(|id| text.contains(&format!("<@{id}>")));

    let attachments = event
        .get("files")
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter_map(|file| {
                    let handle = file.get("id")?.as_str()?.to_string();
                    Some(ChannelAttachment {
                        provider_id: Some(handle.clone()),
                        kind: file
                            .get("mimetype")
                            .and_then(Value::as_str)
                            .map(little_monkey_lib::channels::types::AttachmentKind::from_mime)
                            .unwrap_or(little_monkey_lib::channels::types::AttachmentKind::Other),
                        filename: file.get("name").and_then(Value::as_str).map(str::to_string),
                        mime_type: file
                            .get("mimetype")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        declared_size_bytes: file.get("size").and_then(Value::as_u64),
                        source: AttachmentSource::ProviderHandle { handle },
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let mut metadata = little_monkey_lib::channels::types::BoundedMetadata::new();
    if let Some(subtype) = subtype {
        metadata.insert("subtype", subtype);
    }

    Some(ChannelEnvelope {
        account_id: account_id.to_string(),
        kind: ChannelKind::Slack,
        provider_event_id,
        conversation,
        sender: ChannelSender {
            sender_id,
            display_label: None,
            is_self,
            is_bot,
        },
        text,
        attachments,
        reply_to_provider_id: None,
        mentions_self,
        received_at_ms: now_ms,
        metadata,
    })
}

// ---------------------------------------------------------------------------
// Socket I/O loop
// ---------------------------------------------------------------------------

struct Identity {
    ok: bool,
    user_id: String,
    bot_id: String,
    error: String,
}

async fn auth_test(http: &reqwest::Client, bot_token: &str) -> Result<Identity, String> {
    let request = http
        .post(format!("{API_BASE}/auth.test"))
        .header("Authorization", format!("Bearer {bot_token}"));
    let response = little_monkey_lib::egress::send(request)
        .await
        .map_err(|error| scrub(&error.to_string(), bot_token))?;
    let body: Value = response
        .json()
        .await
        .map_err(|error| scrub(&error.to_string(), bot_token))?;
    Ok(Identity {
        ok: body.get("ok").and_then(Value::as_bool).unwrap_or(false),
        user_id: body
            .get("user_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        bot_id: body
            .get("bot_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        error: body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown_error")
            .to_string(),
    })
}

async fn open_socket_url(http: &reqwest::Client, app_token: &str) -> Result<String, String> {
    let request = http
        .post(format!("{API_BASE}/apps.connections.open"))
        .header("Authorization", format!("Bearer {app_token}"));
    let response = little_monkey_lib::egress::send(request)
        .await
        .map_err(|error| scrub(&error.to_string(), app_token))?;
    let body: Value = response
        .json()
        .await
        .map_err(|error| scrub(&error.to_string(), app_token))?;
    if body.get("ok").and_then(Value::as_bool) != Some(true) {
        let error = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown_error");
        return Err(format!(
            "Slack refused to open a Socket Mode connection: {error}"
        ));
    }
    body.get("url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "Slack's apps.connections.open response had no url".to_string())
}

async fn run_socket_loop(
    account_id: String,
    secret: SlackSecret,
    http: reqwest::Client,
    tx: mpsc::Sender<ChannelEnvelope>,
    shared: Arc<Shared>,
) {
    let identity = match auth_test(&http, &secret.bot_token).await {
        Ok(identity) if identity.ok => identity,
        Ok(identity) => {
            *shared.permanent_error.lock().await =
                Some(format!("Slack rejected the bot token: {}", identity.error));
            return;
        }
        Err(_) => {
            // Network failure resolving our own identity: keep retrying
            // rather than giving up, same as a connect failure below.
            Identity {
                ok: false,
                user_id: String::new(),
                bot_id: String::new(),
                error: String::new(),
            }
        }
    };
    let our_user_id = (!identity.user_id.is_empty()).then_some(identity.user_id);
    let our_bot_id = (!identity.bot_id.is_empty()).then_some(identity.bot_id);

    let mut backoff = MIN_BACKOFF;
    loop {
        let socket_url = match open_socket_url(&http, &secret.app_token).await {
            Ok(url) => url,
            Err(_) => {
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        let reconnected = run_one_connection(
            &account_id,
            &socket_url,
            our_user_id.as_deref(),
            our_bot_id.as_deref(),
            &tx,
        )
        .await;
        if tx.is_closed() {
            return;
        }
        if reconnected {
            backoff = MIN_BACKOFF;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Runs one Socket Mode connection until it drops or asks us to reconnect.
/// Always returns to the caller for a fresh `apps.connections.open` URL — a
/// Socket Mode URL is single-use.
async fn run_one_connection(
    account_id: &str,
    socket_url: &str,
    our_user_id: Option<&str>,
    our_bot_id: Option<&str>,
    tx: &mpsc::Sender<ChannelEnvelope>,
) -> bool {
    let (mut ws, _) = match tokio_tungstenite::connect_async(socket_url).await {
        Ok(pair) => pair,
        Err(_) => return false,
    };
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => {
                for action in
                    handle_socket_frame(account_id, &text, our_user_id, our_bot_id, now_ms())
                {
                    match action {
                        Action::Ack(envelope_id) => {
                            let payload = serde_json::json!({ "envelope_id": envelope_id });
                            let _ = ws.send(Message::Text(payload.to_string().into())).await;
                        }
                        Action::Envelope(envelope) => {
                            let _ = tx.send(*envelope).await;
                        }
                        Action::Reconnect => return true,
                    }
                }
            }
            Some(Ok(Message::Ping(payload))) => {
                let _ = ws.send(Message::Pong(payload)).await;
            }
            Some(Ok(Message::Close(_))) => return true,
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn account_fixture() -> crate::daemon::channel_store::ChannelAccountRecord {
        crate::daemon::channel_store::ChannelAccountRecord {
            account_id: "acct-1".to_string(),
            kind: ChannelKind::Slack,
            label: "Bot".to_string(),
            enabled: true,
            non_secret_config: serde_json::json!({}),
            credential_ref: Some("slack/acct-1".to_string()),
            access_policy: Default::default(),
            health: ChannelHealth::error(0, "unused"),
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    // -- construction ---------------------------------------------------------

    #[test]
    fn rejects_a_malformed_secret() {
        let account = account_fixture();
        let config = AdapterConfig {
            account: &account,
            secret: "not json".to_string(),
        };
        assert!(SlackAdapter::new(&config).is_err());
    }

    #[test]
    fn construction_does_not_start_the_socket_task() {
        // Plain #[test], no tokio runtime: `new` spawning anything would
        // panic here, so a passing test proves it does not.
        let account = account_fixture();
        let config = AdapterConfig {
            account: &account,
            secret: r#"{"bot_token":"xoxb-1","app_token":"xapp-1"}"#.to_string(),
        };
        let adapter = SlackAdapter::new(&config).expect("adapter");
        assert_eq!(adapter.secret.bot_token, "xoxb-1");
    }

    fn message_event_fixture() -> Value {
        serde_json::json!({
            "type": "message",
            "channel": "C1",
            "channel_type": "channel",
            "user": "U1",
            "text": "hi <@BOT1>",
            "ts": "1000.001",
            "client_msg_id": "cmid-1",
        })
    }

    // -- secret shape ---------------------------------------------------------

    #[test]
    fn parses_the_two_token_secret() {
        let secret = parse_secret(r#"{"bot_token":"xoxb-1","app_token":"xapp-1"}"#).unwrap();
        assert_eq!(secret.bot_token, "xoxb-1");
        assert_eq!(secret.app_token, "xapp-1");
        assert!(parse_secret("not json").is_err());
        assert!(parse_secret(r#"{"bot_token":"xoxb-1"}"#).is_err());
    }

    // -- normalization ----------------------------------------------------

    #[test]
    fn normalizes_channel_message_with_mention() {
        let envelope =
            normalize_message_event("acct", &message_event_fixture(), Some("BOT1"), None, 500)
                .expect("envelope");
        assert_eq!(envelope.provider_event_id, "cmid-1");
        assert_eq!(
            envelope.conversation.kind,
            little_monkey_lib::channels::types::ConversationKind::Group
        );
        assert!(envelope.mentions_self);
        assert_eq!(envelope.sender.sender_id, "U1");
    }

    #[test]
    fn im_channel_type_is_direct() {
        let mut fixture = message_event_fixture();
        fixture["channel_type"] = Value::String("im".to_string());
        let envelope = normalize_message_event("acct", &fixture, None, None, 500).unwrap();
        assert_eq!(
            envelope.conversation.kind,
            little_monkey_lib::channels::types::ConversationKind::Direct
        );
    }

    #[test]
    fn thread_ts_distinct_from_ts_becomes_thread_id() {
        let mut fixture = message_event_fixture();
        fixture["thread_ts"] = Value::String("999.000".to_string());
        let envelope = normalize_message_event("acct", &fixture, None, None, 500).unwrap();
        assert_eq!(envelope.conversation.thread_id.as_deref(), Some("999.000"));
    }

    #[test]
    fn root_message_thread_ts_equal_to_ts_is_not_a_thread() {
        let mut fixture = message_event_fixture();
        fixture["thread_ts"] = fixture["ts"].clone();
        let envelope = normalize_message_event("acct", &fixture, None, None, 500).unwrap();
        assert_eq!(envelope.conversation.thread_id, None);
    }

    #[test]
    fn provider_event_id_falls_back_to_channel_and_ts_deterministically() {
        let mut fixture = message_event_fixture();
        fixture.as_object_mut().unwrap().remove("client_msg_id");
        let first = normalize_message_event("acct", &fixture, None, None, 1).unwrap();
        let second = normalize_message_event("acct", &fixture, None, None, 2).unwrap();
        assert_eq!(first.provider_event_id, "C1:1000.001");
        assert_eq!(first.provider_event_id, second.provider_event_id);
    }

    #[test]
    fn bot_message_from_our_own_bot_is_flagged_self() {
        let mut fixture = message_event_fixture();
        fixture["subtype"] = Value::String("bot_message".to_string());
        fixture["bot_id"] = Value::String("B1".to_string());
        fixture.as_object_mut().unwrap().remove("user");
        let envelope = normalize_message_event("acct", &fixture, Some("BOT1"), Some("B1"), 500)
            .expect("envelope");
        assert!(envelope.sender.is_self);
        assert!(envelope.sender.is_bot);
    }

    #[test]
    fn files_become_provider_handle_attachments() {
        let mut fixture = message_event_fixture();
        fixture["files"] = serde_json::json!([{ "id": "F1", "name": "a.png", "mimetype": "image/png", "size": 10 }]);
        let envelope = normalize_message_event("acct", &fixture, None, None, 500).unwrap();
        assert_eq!(envelope.attachments.len(), 1);
        assert_eq!(
            envelope.attachments[0].source,
            AttachmentSource::ProviderHandle {
                handle: "F1".to_string()
            }
        );
    }

    // -- socket framing -----------------------------------------------------

    #[test]
    fn envelope_with_id_is_always_acked() {
        let text = serde_json::json!({
            "envelope_id": "env-1",
            "type": "events_api",
            "payload": { "event": message_event_fixture() },
        })
        .to_string();
        let actions = handle_socket_frame("acct", &text, Some("BOT1"), None, 500);
        assert!(matches!(&actions[0], Action::Ack(id) if id == "env-1"));
        assert!(matches!(&actions[1], Action::Envelope(_)));
    }

    #[test]
    fn disconnect_triggers_reconnect() {
        let text = serde_json::json!({ "type": "disconnect", "reason": "warning" }).to_string();
        let actions = handle_socket_frame("acct", &text, None, None, 500);
        assert!(matches!(actions.last(), Some(Action::Reconnect)));
    }

    // -- outbound mapping ---------------------------------------------------

    #[test]
    fn ok_true_maps_to_sent() {
        let body = serde_json::json!({ "ok": true, "ts": "111.222" });
        match map_send_response(200, None, &body) {
            SendOutcome::Sent {
                provider_message_id,
            } => {
                assert_eq!(provider_message_id.as_deref(), Some("111.222"))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn ok_false_ratelimited_is_retryable_with_retry_after() {
        let body = serde_json::json!({ "ok": false, "error": "ratelimited" });
        match map_send_response(200, Some(3000), &body) {
            SendOutcome::RetryableFailure { retry_after_ms, .. } => {
                assert_eq!(retry_after_ms, Some(3000))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn ok_false_invalid_auth_is_permanent() {
        let body = serde_json::json!({ "ok": false, "error": "invalid_auth" });
        assert!(matches!(
            map_send_response(200, None, &body),
            SendOutcome::PermanentFailure { .. }
        ));
    }

    #[test]
    fn http_429_is_retryable() {
        let body = Value::Null;
        assert!(matches!(
            map_send_response(429, Some(1000), &body),
            SendOutcome::RetryableFailure { .. }
        ));
    }

    #[test]
    fn message_splitting_respects_the_limit() {
        let text = "a".repeat(90_000);
        let chunks = split_message(&text, SLACK_MAX_TEXT_CHARS);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].chars().count(), SLACK_MAX_TEXT_CHARS);
    }

    // -- token hygiene -------------------------------------------------------

    #[test]
    fn scrub_removes_the_token() {
        let rendered = scrub("error near xoxb-secret-1", "xoxb-secret-1");
        assert!(!rendered.contains("xoxb-secret-1"));
    }

    // -- HTTP fixture ---------------------------------------------------------

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
    async fn auth_test_reads_identity_from_a_fixture_server() {
        let base = fixture_server(
            "200 OK",
            r#"{"ok":true,"user_id":"U1","bot_id":"B1"}"#.to_string(),
        )
        .await;
        let client = little_monkey_lib::egress::hardened().build().unwrap();
        let request = client
            .post(format!("{base}/auth.test"))
            .header("Authorization", "Bearer xoxb-tok");
        let response = little_monkey_lib::egress::send(request).await.unwrap();
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["user_id"], "U1");
    }
}
