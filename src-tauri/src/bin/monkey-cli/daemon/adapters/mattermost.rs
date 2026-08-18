//! Mattermost adapter: WebSocket inbound (`/api/v4/websocket`), REST outbound
//! (`/api/v4`), against the user's own server.
//!
//! Unlike Discord and Slack, the endpoint here is not a fixed provider host —
//! it is whatever the operator typed into `non_secret_config.base_url`, which
//! makes [`validate_base_url`] a trust-boundary check rather than a formality:
//! a plain-`http` URL could otherwise walk a bearer token to whatever that
//! string names, so `http` is accepted only for `localhost`.
//!
//! One task (spawned in [`MattermostAdapter::new`]) resolves our own identity
//! via `GET /users/me`, then owns the WebSocket connection: it authenticates
//! with `authentication_challenge`, normalizes `posted` events, and pushes
//! them into a bounded channel that [`ChannelAdapter::poll`] drains.
//! [`handle_socket_frame`] is the pure part of that: given one text frame and
//! our identity, what to do about it.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use little_monkey_lib::channels::policy::EchoCorrelation;
use little_monkey_lib::channels::types::{
    AttachmentKind, AttachmentSource, ChannelAttachment, ChannelConversation, ChannelEnvelope,
    ChannelHealth, ChannelKind, ChannelSender, HealthState, InboundTransport, OutboundMessage,
    ProviderCapabilities, SendOutcome,
};
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

use crate::daemon::channel_adapter::{
    fetch_url, load_attachments, AdapterConfig, BlobSource, ChannelAdapter, DaemonBlobs,
    InboundBatch, LoadedAttachment, TransportStatus,
};

const INBOUND_CHANNEL_CAPACITY: usize = 256;
const POLL_WAIT: Duration = Duration::from_secs(20);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A connection that stayed up at least this long counts as recovered, so the
/// next drop backs off from the beginning. Measured on the connection's own
/// lifetime rather than on "did we reach the socket at all": a server that
/// accepts and immediately closes would otherwise reset the backoff every time
/// and turn reconnection into a tight loop against it.
const STABLE_AFTER: Duration = Duration::from_secs(30);
/// Mattermost's default `MaxPostSize`. Server-configurable, not fetched here —
/// ponytail: a wrong-way-round split (limit raised on the server, adapter
/// still splits at the stock default) costs nothing but an unnecessary extra
/// message; upgrade path is reading `GET /api/v4/config/client` once at
/// startup if that ever matters.
const MATTERMOST_MAX_TEXT_CHARS: usize = 16_383;

/// Validates `non_secret_config.base_url`: must be a bare origin (no path, no
/// query) so it cannot be mistaken for an API path, and must be `https` —
/// `http` is accepted only for `localhost`/`127.0.0.1`/`::1`, since a bearer
/// token is going out on every request this adapter makes to it.
fn validate_base_url(raw: &str) -> Result<String, String> {
    let parsed =
        url::Url::parse(raw).map_err(|_| "Mattermost base_url is not a valid URL".to_string())?;
    if !matches!(parsed.path(), "" | "/") {
        return Err("Mattermost base_url must not include a path".to_string());
    }
    if parsed.query().is_some() {
        return Err("Mattermost base_url must not include a query string".to_string());
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
                    "Mattermost base_url must be https (plain http is only accepted for localhost)"
                        .to_string(),
                );
            }
        }
        _ => return Err("Mattermost base_url must use http or https".to_string()),
    }
    Ok(raw.trim_end_matches('/').to_string())
}

fn websocket_url(base_url: &str) -> String {
    let ws_base = if let Some(rest) = base_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base_url.to_string()
    };
    format!("{ws_base}/api/v4/websocket")
}

#[derive(Default)]
struct Shared {
    permanent_error: Mutex<Option<String>>,
    /// What the websocket is actually doing — see [`TransportStatus`]. `poll`
    /// returns an empty batch whether the socket is live or dropped, so it
    /// cannot answer this.
    status: TransportStatus,
    /// Posts normalized but never handed on, because `poll` fell far enough
    /// behind to fill the inbound queue.
    ///
    /// Counted rather than waited on: the reader must never block on a
    /// downstream consumer, or it stops answering the server's pings and
    /// Mattermost closes the socket underneath it. Surfaced as degraded health
    /// so an overflow is something an operator sees rather than a message that
    /// quietly never arrived.
    dropped: std::sync::atomic::AtomicU64,
}

pub struct MattermostAdapter {
    account_id: String,
    token: String,
    base_url: String,
    http: reqwest::Client,
    inbound_tx: mpsc::Sender<ChannelEnvelope>,
    inbound_rx: Mutex<mpsc::Receiver<ChannelEnvelope>>,
    shared: Arc<Shared>,
    /// Guards the one-time spawn of the socket task. `new` itself stays
    /// side-effect-free — see the Discord adapter's module doc for why.
    started: tokio::sync::OnceCell<()>,
    blobs: Arc<dyn BlobSource>,
}

impl MattermostAdapter {
    pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
        if config.secret.is_empty() {
            return Err(
                "This Mattermost account has no personal access token configured".to_string(),
            );
        }
        let base_url = config
            .account
            .non_secret_config
            .get("base_url")
            .and_then(Value::as_str)
            .ok_or_else(|| "Mattermost account is missing base_url".to_string())?;
        let base_url = validate_base_url(base_url)?;
        let http = little_monkey_lib::egress::hardened()
            .build()
            .map_err(|error| format!("Failed to build the Mattermost HTTP client: {error}"))?;
        let (tx, rx) = mpsc::channel(INBOUND_CHANNEL_CAPACITY);
        Ok(Self {
            account_id: config.account.account_id.clone(),
            token: config.secret.clone(),
            base_url,
            http,
            inbound_tx: tx,
            inbound_rx: Mutex::new(rx),
            shared: Arc::new(Shared::default()),
            started: tokio::sync::OnceCell::new(),
            blobs: Arc::new(DaemonBlobs),
        })
    }

    #[cfg(test)]
    fn with_blobs(mut self, blobs: Arc<dyn BlobSource>) -> Self {
        self.blobs = blobs;
        self
    }

    async fn ensure_started(&self) {
        self.started
            .get_or_init(|| async {
                tokio::spawn(run_socket_loop(
                    self.account_id.clone(),
                    self.token.clone(),
                    self.base_url.clone(),
                    self.http.clone(),
                    self.inbound_tx.clone(),
                    self.shared.clone(),
                ));
            })
            .await;
    }
}

#[async_trait]
impl ChannelAdapter for MattermostAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Mattermost
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            max_text_chars: MATTERMOST_MAX_TEXT_CHARS,
            supports_threads: true,
            supports_attachments: true,
            supports_mention_metadata: true,
            supports_idempotency_key: false,
            supports_delivery_receipts: false,
            echo_correlation: EchoCorrelation::HostAdapter,
            ..ProviderCapabilities::minimal(ChannelKind::Mattermost, InboundTransport::Socket)
        }
    }

    /// The websocket's own state, which a quiet poll cannot report.
    fn live_transport(&self) -> Option<HealthState> {
        Some(self.shared.status.get())
    }

    async fn probe(&self) -> ChannelHealth {
        self.ensure_started().await;
        let now = now_ms();
        if let Some(error) = self.shared.permanent_error.lock().await.clone() {
            return ChannelHealth::error(now, error);
        }
        match fetch_me(&self.http, &self.base_url, &self.token).await {
            Ok(me) => {
                let dropped = self
                    .shared
                    .dropped
                    .load(std::sync::atomic::Ordering::Relaxed);
                if dropped > 0 {
                    return ChannelHealth::degraded(
                        now,
                        format!(
                            "Authenticated to Mattermost as {} · {dropped} post(s) dropped under \
                             load",
                            me.username
                        ),
                    );
                }
                // Authentication alone is half the story. Inbound arrives on the
                // WebSocket, so an account whose token works but whose socket is
                // down receives nothing — reporting that as Connected is exactly
                // the false positive the health column exists to prevent.
                match self.shared.status.get() {
                    HealthState::Connected => ChannelHealth::connected(
                        now,
                        Some(format!("Connected to Mattermost as {}", me.username)),
                    ),
                    HealthState::Connecting => ChannelHealth::connecting(
                        now,
                        Some(format!(
                            "Authenticated to Mattermost as {} · opening the WebSocket",
                            me.username
                        )),
                    ),
                    _ => ChannelHealth::degraded(
                        now,
                        format!(
                            "Authenticated to Mattermost as {} · the WebSocket is down, so no \
                             message can arrive",
                            me.username
                        ),
                    ),
                }
            }
            Err(error) => ChannelHealth::error(
                now,
                scrub(&format!("Mattermost probe failed: {error}"), &self.token),
            ),
        }
    }

    async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
        // Socket transport: the WebSocket task pushes as events arrive, so
        // there is no page or offset for the cursor to carry.
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
        if !message.attachments.is_empty() {
            let files = match load_attachments(self.blobs.as_ref(), message) {
                Ok(files) => files,
                Err(outcome) => return outcome,
            };
            return self.send_with_attachments(message, &files).await;
        }
        let mut chunks = split_message(&message.text, MATTERMOST_MAX_TEXT_CHARS);
        if chunks.is_empty() {
            chunks.push(String::new());
        }
        let mut any_sent = false;
        let mut last_id = None;
        for chunk in &chunks {
            let mut body = serde_json::json!({
                "channel_id": message.conversation_id,
                "message": chunk,
            });
            if let Some(root_id) = &message.thread_id {
                body["root_id"] = Value::String(root_id.clone());
            }
            let request = self
                .http
                .post(format!("{}/api/v4/posts", self.base_url))
                .header("Authorization", format!("Bearer {}", self.token))
                .json(&body);
            let response = match little_monkey_lib::egress::send(request).await {
                Ok(response) => response,
                Err(error) => return self.transport_failure(&error, any_sent),
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
            if let Some(outcome) = map_send_status(status, any_sent, retry_after_ms) {
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

    /// Mattermost serves the bytes for a post's file id straight from the API,
    /// authenticated with the same bot token as everything else.
    async fn fetch_attachment(
        &self,
        attachment: &ChannelAttachment,
        limits: crate::daemon::channel_adapter::AttachmentLimits,
    ) -> Result<Vec<u8>, String> {
        let AttachmentSource::ProviderHandle { handle } = &attachment.source else {
            return Err("This Mattermost attachment has no file id.".to_string());
        };
        // The id is path-concatenated, and it arrives inside a post anybody in
        // the channel can craft, so anything that could climb out of
        // `/api/v4/files/` is refused instead of escaped.
        if handle.is_empty() || !handle.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err("That Mattermost file id is not usable".to_string());
        }
        fetch_url(
            &format!("{}/api/v4/files/{handle}", self.base_url),
            Some(&self.token),
            limits.max_bytes,
        )
        .await
    }
}

impl MattermostAdapter {
    /// One failed HTTP call as the outcome the outbox may act on.
    ///
    /// The two questions, in order, are "has Mattermost already accepted part of
    /// this message?" and "could this particular request have reached it?". A
    /// yes to either forbids a blind retry.
    fn transport_failure(&self, error: &reqwest::Error, already_accepted: bool) -> SendOutcome {
        let message = scrub(&error.to_string(), &self.token);
        if already_accepted {
            return SendOutcome::NeedsReconciliation {
                error: format!(
                    "{message} (an earlier part of this message was already accepted by \
                     Mattermost)"
                ),
            };
        }
        match certainty_of(error) {
            DeliveryCertainty::DefinitelyNotSent => SendOutcome::RetryableFailure {
                error: message,
                retry_after_ms: None,
            },
            DeliveryCertainty::PossiblySent => SendOutcome::NeedsReconciliation { error: message },
        }
    }

    /// Mattermost uploads first and posts second: `/api/v4/files` returns file
    /// ids, and the post that carries them names those ids. Both halves use the
    /// same bot token.
    async fn send_with_attachments(
        &self,
        message: &OutboundMessage,
        files: &[LoadedAttachment],
    ) -> SendOutcome {
        let mut form =
            reqwest::multipart::Form::new().text("channel_id", message.conversation_id.clone());
        for file in files {
            let part = reqwest::multipart::Part::bytes(file.bytes.clone())
                .file_name(file.filename.clone())
                .mime_str(&file.mime_type)
                .unwrap_or_else(|_| {
                    reqwest::multipart::Part::bytes(file.bytes.clone())
                        .file_name(file.filename.clone())
                });
            form = form.part("files", part);
        }
        let request = self
            .http
            .post(format!("{}/api/v4/files", self.base_url))
            .header("Authorization", format!("Bearer {}", self.token))
            .multipart(form);
        // An upload that may have landed leaves an orphaned file rather than a
        // visible post, so only a failure that provably never left this process
        // is retried.
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => return self.transport_failure(&error, false),
        };
        let status = response.status().as_u16();
        if let Some(outcome) = map_send_status(status, false, None) {
            return outcome;
        }
        let file_ids: Vec<String> = match response.json::<Value>().await {
            Ok(value) => value
                .get("file_infos")
                .and_then(Value::as_array)
                .map(|infos| {
                    infos
                        .iter()
                        .filter_map(|info| {
                            info.get("id").and_then(Value::as_str).map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        if file_ids.is_empty() {
            return SendOutcome::NeedsReconciliation {
                error: "Mattermost accepted the upload but named no files".to_string(),
            };
        }
        let mut body = serde_json::json!({
            "channel_id": message.conversation_id,
            "message": message.text,
            "file_ids": file_ids,
        });
        if let Some(root_id) = &message.thread_id {
            body["root_id"] = Value::String(root_id.clone());
        }
        let request = self
            .http
            .post(format!("{}/api/v4/posts", self.base_url))
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&body);
        // The upload is accepted at this point, so nothing below may be retried
        // from the beginning: that would re-upload the files and, if the post
        // did land, post them twice.
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => return self.transport_failure(&error, true),
        };
        let status = response.status().as_u16();
        if let Some(outcome) = map_send_status(status, true, None) {
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

/// Whether a failed outbound HTTP call can have reached Mattermost.
///
/// The whole point of the distinction: `RetryableFailure` re-sends the same
/// message, so it may only be used when it is *provable* that the provider never
/// saw the request. Everything else is [`SendOutcome::NeedsReconciliation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryCertainty {
    /// No application byte can have crossed the network boundary.
    DefinitelyNotSent,
    /// Mattermost may already have created the post.
    PossiblySent,
}

/// Classify one `reqwest` failure.
///
/// Only two kinds prove nothing was sent:
///
/// - a builder error — the request was never handed to the client at all;
/// - a connect error — the failure came out of the connector, which runs before
///   any request byte is written to a socket. `egress::send`'s own allowlist
///   refusal surfaces here too, because it is implemented as a resolver that
///   refuses every name, and a name that never resolved was never connected to.
///
/// Everything else is treated as possibly-sent on purpose, including a timeout:
/// reqwest's timeout covers the *response* as well as the connection, so a
/// timed-out send may be a post Mattermost already created. A body or decode
/// error is even more clearly past the boundary — bytes went out to produce it.
fn certainty_of(error: &reqwest::Error) -> DeliveryCertainty {
    // `is_body` first: a request body that failed mid-stream has already put
    // bytes on the wire, whatever else the error also claims to be.
    if error.is_body() {
        return DeliveryCertainty::PossiblySent;
    }
    if error.is_builder() || error.is_connect() {
        return DeliveryCertainty::DefinitelyNotSent;
    }
    DeliveryCertainty::PossiblySent
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

/// One HTTP status as an outcome, or `None` when it was a success.
///
/// `any_sent_before` is the dominant term, not a modifier on one arm: once
/// Mattermost has accepted *any* part of this logical message — an earlier text
/// chunk, or the file upload an attachment post is about to reference — no
/// status may be answered with `RetryableFailure`, because the retry re-sends
/// the accepted part too. That covers 429 and 401 exactly as much as it covers
/// 500: a rate limit halfway through a two-chunk message is still a message
/// whose first half is already in the channel.
fn map_send_status(
    status: u16,
    any_sent_before: bool,
    retry_after_ms: Option<i64>,
) -> Option<SendOutcome> {
    if (200..=299).contains(&status) {
        return None;
    }
    if any_sent_before {
        return Some(SendOutcome::NeedsReconciliation {
            error: format!(
                "Mattermost returned HTTP {status} after it had already accepted part of this \
                 message"
            ),
        });
    }
    Some(match status {
        429 => SendOutcome::RetryableFailure {
            error: "Mattermost rate limited the request".to_string(),
            retry_after_ms,
        },
        401 | 403 => SendOutcome::PermanentFailure {
            error: format!("Mattermost rejected the request: HTTP {status}"),
        },
        500..=599 => SendOutcome::RetryableFailure {
            error: format!("Mattermost returned HTTP {status}"),
            retry_after_ms: None,
        },
        _ => SendOutcome::PermanentFailure {
            error: format!("Mattermost rejected the message: HTTP {status}"),
        },
    })
}

// ---------------------------------------------------------------------------
// WebSocket framing (pure)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Action {
    Envelope(Box<ChannelEnvelope>),
}

/// Handles one WebSocket text frame. `our_user_id` gates `is_self`;
/// `our_username` backs the `@username` mention fallback when the event's own
/// `mentions` field is absent or unparsable.
fn handle_socket_frame(
    account_id: &str,
    text: &str,
    our_user_id: Option<&str>,
    our_username: Option<&str>,
    now_ms: i64,
) -> Vec<Action> {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    if value.get("event").and_then(Value::as_str) != Some("posted") {
        return Vec::new();
    }
    match normalize_posted_event(account_id, &value, our_user_id, our_username, now_ms) {
        Some(envelope) => vec![Action::Envelope(Box::new(envelope))],
        None => Vec::new(),
    }
}

fn authentication_challenge(token: &str) -> Value {
    serde_json::json!({
        "seq": 1,
        "action": "authentication_challenge",
        "data": { "token": token },
    })
}

fn normalize_posted_event(
    account_id: &str,
    event: &Value,
    our_user_id: Option<&str>,
    our_username: Option<&str>,
    now_ms: i64,
) -> Option<ChannelEnvelope> {
    let data = event.get("data")?;
    let post_raw = data.get("post")?.as_str()?;
    let post: Value = serde_json::from_str(post_raw).ok()?;

    let id = post.get("id")?.as_str()?.to_string();
    let channel_id = post.get("channel_id")?.as_str()?.to_string();
    let user_id = post.get("user_id")?.as_str()?.to_string();
    let text = post
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let root_id = post
        .get("root_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    let is_direct = data.get("channel_type").and_then(Value::as_str) == Some("D");
    let conversation = if is_direct {
        ChannelConversation::direct(channel_id)
    } else {
        ChannelConversation::group(channel_id)
    }
    .with_thread(root_id);

    let is_self = our_user_id.is_some_and(|id| id == user_id);

    let mentions_self = our_user_id.is_some_and(|id| {
        data.get("mentions")
            .and_then(Value::as_str)
            .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
            .is_some_and(|ids| ids.iter().any(|mentioned| mentioned == id))
    }) || our_username
        .is_some_and(|username| text.contains(&format!("@{username}")));

    let attachments = post
        .get("file_ids")
        .and_then(Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(|file_id| ChannelAttachment {
                    stored_artifact_id: None,
                    text_excerpt: None,
                    fetch_error: None,
                    provider_id: Some(file_id.to_string()),
                    kind: AttachmentKind::Other,
                    filename: None,
                    mime_type: None,
                    declared_size_bytes: None,
                    stored_size_bytes: None,
                    source: AttachmentSource::ProviderHandle {
                        handle: file_id.to_string(),
                    },
                })
                .collect()
        })
        .unwrap_or_default();

    Some(ChannelEnvelope {
        account_id: account_id.to_string(),
        kind: ChannelKind::Mattermost,
        provider_event_id: id,
        provider_message_id: None,
        conversation,
        sender: ChannelSender {
            sender_id: user_id,
            display_label: data
                .get("sender_name")
                .and_then(Value::as_str)
                .map(str::to_string),
            is_self,
            is_bot: false,
        },
        text,
        attachments,
        reply_to_provider_id: None,
        mentions_self,
        received_at_ms: now_ms,
        metadata: little_monkey_lib::channels::types::BoundedMetadata::new(),
    })
}

// ---------------------------------------------------------------------------
// I/O loop
// ---------------------------------------------------------------------------

struct Me {
    user_id: String,
    username: String,
}

#[derive(Debug)]
enum FetchMeError {
    Retryable(String),
    /// The token itself is rejected — retrying will not help.
    Permanent(String),
}

impl std::fmt::Display for FetchMeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchMeError::Retryable(message) | FetchMeError::Permanent(message) => {
                write!(f, "{message}")
            }
        }
    }
}

async fn fetch_me(http: &reqwest::Client, base_url: &str, token: &str) -> Result<Me, FetchMeError> {
    let request = http
        .get(format!("{base_url}/api/v4/users/me"))
        .header("Authorization", format!("Bearer {token}"));
    let response = little_monkey_lib::egress::send(request)
        .await
        .map_err(|error| FetchMeError::Retryable(scrub(&error.to_string(), token)))?;
    let status = response.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(FetchMeError::Permanent(
            "Mattermost rejected the personal access token".to_string(),
        ));
    }
    if !status.is_success() {
        return Err(FetchMeError::Retryable(format!(
            "Mattermost /users/me failed: HTTP {status}"
        )));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|error| FetchMeError::Retryable(scrub(&error.to_string(), token)))?;
    let user_id = body
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            FetchMeError::Retryable("Mattermost /users/me response had no id".to_string())
        })?
        .to_string();
    let username = body
        .get("username")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(Me { user_id, username })
}

async fn run_socket_loop(
    account_id: String,
    token: String,
    base_url: String,
    http: reqwest::Client,
    tx: mpsc::Sender<ChannelEnvelope>,
    shared: Arc<Shared>,
) {
    let mut backoff = MIN_BACKOFF;
    loop {
        // Identity first, and no connection without it. `is_self` and mention
        // detection are both keyed on who we are, so a socket opened while that
        // is unknown would let the account answer its own posts — a loop that
        // is much worse than waiting one backoff for `/users/me`.
        match fetch_me(&http, &base_url, &token).await {
            Ok(me) => {
                let connected_at = std::time::Instant::now();
                run_one_connection(&account_id, &base_url, &token, &me, &tx, &shared).await;
                shared.status.set(HealthState::Degraded);
                if tx.is_closed() {
                    return;
                }
                if connected_at.elapsed() >= STABLE_AFTER {
                    backoff = MIN_BACKOFF;
                }
            }
            Err(FetchMeError::Permanent(error)) => {
                shared.status.set(HealthState::Error);
                *shared.permanent_error.lock().await = Some(error);
                return;
            }
            Err(FetchMeError::Retryable(error)) => {
                shared.status.set(HealthState::Degraded);
                *shared.permanent_error.lock().await = None;
                if tx.is_closed() {
                    return;
                }
                let _ = error;
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Hand one normalized post to `poll`, or count it as lost.
///
/// Never `send().await`. The caller is the WebSocket reader, and the reader is
/// also what answers the server's pings: blocking it on a downstream consumer
/// is how the socket gets closed underneath a connection that looked healthy.
/// An overflow is therefore dropped, counted, and reported as degraded — the
/// one thing it must never be is silently treated as delivered.
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
    me: &Me,
    tx: &mpsc::Sender<ChannelEnvelope>,
    shared: &Arc<Shared>,
) {
    let status = &shared.status;
    let (mut ws, _) = match tokio_tungstenite::connect_async(websocket_url(base_url)).await {
        Ok(pair) => pair,
        Err(_) => return,
    };
    let challenge = authentication_challenge(token);
    if ws
        .send(Message::Text(challenge.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    status.set(HealthState::Connected);

    let our_user_id = Some(me.user_id.as_str());
    let our_username = Some(me.username.as_str());

    loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => {
                for action in
                    handle_socket_frame(account_id, &text, our_user_id, our_username, now_ms())
                {
                    match action {
                        Action::Envelope(envelope) => deliver(shared, tx, *envelope),
                    }
                }
            }
            Some(Ok(Message::Ping(payload))) => {
                let _ = ws.send(Message::Pong(payload)).await;
            }
            Some(Ok(Message::Close(_))) => return,
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn account_fixture(base_url: &str) -> crate::daemon::channel_store::ChannelAccountRecord {
        crate::daemon::channel_store::ChannelAccountRecord {
            account_id: "acct-1".to_string(),
            kind: ChannelKind::Mattermost,
            label: "Bot".to_string(),
            enabled: true,
            non_secret_config: serde_json::json!({ "base_url": base_url }),
            credential_ref: Some("mattermost/acct-1".to_string()),
            access_policy: Default::default(),
            health: ChannelHealth::error(0, "unused"),
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    // -- construction ---------------------------------------------------------

    #[test]
    fn rejects_an_invalid_base_url() {
        let account = account_fixture("http://mm.example.com");
        let config = AdapterConfig {
            account: &account,
            secret: "tok".to_string(),
        };
        assert!(MattermostAdapter::new(&config).is_err());
    }

    #[test]
    fn rejects_an_empty_token() {
        let account = account_fixture("https://mm.example.com");
        let config = AdapterConfig {
            account: &account,
            secret: String::new(),
        };
        assert!(MattermostAdapter::new(&config).is_err());
    }

    #[test]
    fn construction_does_not_start_the_socket_task() {
        // Plain #[test], no tokio runtime: `new` spawning anything would
        // panic here, so a passing test proves it does not.
        let account = account_fixture("https://mm.example.com");
        let config = AdapterConfig {
            account: &account,
            secret: "tok-123".to_string(),
        };
        let adapter = MattermostAdapter::new(&config).expect("adapter");
        assert_eq!(adapter.base_url, "https://mm.example.com");
    }

    fn posted_event_fixture() -> Value {
        serde_json::json!({
            "event": "posted",
            "data": {
                "channel_type": "O",
                "sender_name": "alice",
                "mentions": "[\"bot-id\"]",
                "post": serde_json::json!({
                    "id": "post-1",
                    "channel_id": "chan-1",
                    "user_id": "user-1",
                    "message": "hi @bot",
                    "root_id": "",
                    "file_ids": ["file-1"],
                }).to_string(),
            },
        })
    }

    // -- base_url validation --------------------------------------------------

    #[test]
    fn accepts_https_bare_origin() {
        assert_eq!(
            validate_base_url("https://mm.example.com").unwrap(),
            "https://mm.example.com"
        );
    }

    #[test]
    fn rejects_path_and_query() {
        assert!(validate_base_url("https://mm.example.com/team").is_err());
        assert!(validate_base_url("https://mm.example.com?x=1").is_err());
    }

    #[test]
    fn rejects_plain_http_for_non_localhost() {
        assert!(validate_base_url("http://mm.example.com").is_err());
    }

    #[test]
    fn accepts_plain_http_for_localhost() {
        assert!(validate_base_url("http://localhost:8065").is_ok());
        assert!(validate_base_url("http://127.0.0.1:8065").is_ok());
    }

    #[test]
    fn websocket_url_swaps_scheme() {
        assert_eq!(
            websocket_url("https://mm.example.com"),
            "wss://mm.example.com/api/v4/websocket"
        );
        assert_eq!(
            websocket_url("http://localhost:8065"),
            "ws://localhost:8065/api/v4/websocket"
        );
    }

    // -- normalization ----------------------------------------------------

    #[test]
    fn normalizes_group_post_with_mention_and_file() {
        let envelope = normalize_posted_event(
            "acct",
            &posted_event_fixture(),
            Some("bot-id"),
            Some("bot"),
            500,
        )
        .expect("envelope");
        assert_eq!(envelope.provider_event_id, "post-1");
        assert_eq!(
            envelope.conversation.kind,
            little_monkey_lib::channels::types::ConversationKind::Group
        );
        assert!(envelope.mentions_self);
        assert_eq!(envelope.attachments.len(), 1);
        assert_eq!(
            envelope.attachments[0].source,
            AttachmentSource::ProviderHandle {
                handle: "file-1".to_string()
            }
        );
    }

    #[test]
    fn direct_channel_type_is_direct() {
        let mut fixture = posted_event_fixture();
        fixture["data"]["channel_type"] = Value::String("D".to_string());
        let envelope = normalize_posted_event("acct", &fixture, None, None, 500).unwrap();
        assert_eq!(
            envelope.conversation.kind,
            little_monkey_lib::channels::types::ConversationKind::Direct
        );
    }

    #[test]
    fn empty_root_id_is_not_a_thread() {
        let envelope =
            normalize_posted_event("acct", &posted_event_fixture(), None, None, 500).unwrap();
        assert_eq!(envelope.conversation.thread_id, None);
    }

    #[test]
    fn nonempty_root_id_is_a_thread() {
        let mut fixture = posted_event_fixture();
        let mut post: Value =
            serde_json::from_str(fixture["data"]["post"].as_str().unwrap()).unwrap();
        post["root_id"] = Value::String("root-1".to_string());
        fixture["data"]["post"] = Value::String(post.to_string());
        let envelope = normalize_posted_event("acct", &fixture, None, None, 500).unwrap();
        assert_eq!(envelope.conversation.thread_id.as_deref(), Some("root-1"));
    }

    #[test]
    fn self_authored_post_is_flagged() {
        let envelope =
            normalize_posted_event("acct", &posted_event_fixture(), Some("user-1"), None, 500)
                .unwrap();
        assert!(envelope.sender.is_self);
    }

    #[test]
    fn mention_falls_back_to_username_in_text_when_mentions_field_absent() {
        let mut fixture = posted_event_fixture();
        fixture["data"].as_object_mut().unwrap().remove("mentions");
        let envelope =
            normalize_posted_event("acct", &fixture, Some("other-id"), Some("bot"), 500).unwrap();
        assert!(envelope.mentions_self);
    }

    #[test]
    fn provider_event_id_is_deterministic() {
        let first = normalize_posted_event("acct", &posted_event_fixture(), None, None, 1).unwrap();
        let second =
            normalize_posted_event("acct", &posted_event_fixture(), None, None, 2).unwrap();
        assert_eq!(first.provider_event_id, second.provider_event_id);
    }

    // -- socket framing -----------------------------------------------------

    #[test]
    fn non_posted_events_are_ignored() {
        let text = serde_json::json!({ "event": "hello" }).to_string();
        assert!(handle_socket_frame("acct", &text, None, None, 500).is_empty());
    }

    #[test]
    fn posted_event_yields_one_envelope_action() {
        let text = posted_event_fixture().to_string();
        let actions = handle_socket_frame("acct", &text, Some("bot-id"), Some("bot"), 500);
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], Action::Envelope(_)));
    }

    #[test]
    fn authentication_challenge_carries_the_token() {
        let payload = authentication_challenge("tok-123");
        assert_eq!(payload["action"], "authentication_challenge");
        assert_eq!(payload["data"]["token"], "tok-123");
    }

    // -- outbound mapping ---------------------------------------------------

    #[test]
    fn rate_limit_maps_to_retryable_with_ms() {
        let outcome = map_send_status(429, false, Some(1500)).unwrap();
        assert!(matches!(
            outcome,
            SendOutcome::RetryableFailure {
                retry_after_ms: Some(1500),
                ..
            }
        ));
    }

    #[test]
    fn auth_failure_is_permanent() {
        assert!(matches!(
            map_send_status(401, false, None),
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

    /// Requirement: no logical retry path can duplicate an already-accepted
    /// chunk. This is the adapter's half — once anything has been accepted, no
    /// status may map to a retryable outcome, whatever it is. The other half
    /// (the outbox parks a reconciliation row forever rather than reclaiming
    /// it) is pinned in `channel_worker.rs`.
    #[test]
    fn nothing_after_an_accepted_chunk_is_ever_retryable() {
        for status in [
            200, 201, 204, 301, 400, 401, 403, 404, 409, 413, 418, 429, 500, 502, 503, 504,
        ] {
            match map_send_status(status, true, Some(1_000)) {
                None => assert!(
                    (200..=299).contains(&status),
                    "HTTP {status} read as a success"
                ),
                Some(SendOutcome::NeedsReconciliation { .. }) => {}
                other => panic!("HTTP {status} after an accepted chunk mapped to {other:?}"),
            }
        }
    }

    #[test]
    fn message_splitting_respects_the_limit() {
        let text = "a".repeat(30_000);
        let chunks = split_message(&text, MATTERMOST_MAX_TEXT_CHARS);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), MATTERMOST_MAX_TEXT_CHARS);
    }

    // -- token hygiene -------------------------------------------------------

    #[test]
    fn scrub_removes_the_token() {
        let rendered = scrub("failed near token-abc-123", "token-abc-123");
        assert!(!rendered.contains("token-abc-123"));
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
    async fn fetch_me_reads_identity_from_a_fixture_server() {
        let base =
            fixture_server("200 OK", r#"{"id":"user-9","username":"bot"}"#.to_string()).await;
        let client = little_monkey_lib::egress::hardened().build().unwrap();
        let me = fetch_me(&client, &base, "tok").await.unwrap();
        assert_eq!(me.user_id, "user-9");
        assert_eq!(me.username, "bot");
    }

    #[tokio::test]
    async fn a_rejected_token_is_permanent_and_a_server_error_is_not() {
        let client = little_monkey_lib::egress::hardened().build().unwrap();
        let base = fixture_server("401 Unauthorized", "{}".to_string()).await;
        assert!(matches!(
            fetch_me(&client, &base, "tok").await,
            Err(FetchMeError::Permanent(_))
        ));
        let base = fixture_server("503 Service Unavailable", "{}".to_string()).await;
        assert!(matches!(
            fetch_me(&client, &base, "tok").await,
            Err(FetchMeError::Retryable(_))
        ));
    }

    // -- outbound delivery certainty ------------------------------------------

    /// What a scripted fixture does with one request.
    #[derive(Clone)]
    enum Reply {
        /// Answer with this status line and body.
        Status(&'static str, &'static str),
        /// Read the whole request, then close without answering.
        ///
        /// This is the case the certainty rule exists for: the request crossed
        /// the network — Mattermost may well have created the post — and only
        /// the response was lost.
        Silence,
    }

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

    /// Drain one whole HTTP request, headers and declared body.
    ///
    /// Reading only what one `read` happens to return would close the socket
    /// under a client still writing a 16 KB chunk, which is a *connect-side*
    /// failure and would let a test pass for the wrong reason.
    async fn drain_request(stream: &mut tokio::net::TcpStream) {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
            let Some(header_end) = find(&buffer, b"\r\n\r\n") else {
                continue;
            };
            let body_start = header_end + 4;
            if buffer.len() - body_start >= declared_length(&buffer[..header_end]) {
                return;
            }
        }
    }

    /// A Mattermost stand-in that answers a fixed script, one reply per
    /// request, and counts what it was asked.
    async fn scripted_server(script: Vec<Reply>) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let address = listener.local_addr().expect("fixture address");
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = seen.clone();
        tokio::spawn(async move {
            for reply in script {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                drain_request(&mut stream).await;
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if let Reply::Status(status, body) = reply {
                    let response = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: \
                         {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                }
            }
        });
        (format!("http://{address}"), seen)
    }

    fn adapter_for(base: &str) -> MattermostAdapter {
        let account = account_fixture(base);
        MattermostAdapter::new(&AdapterConfig {
            account: &account,
            secret: "tok".to_string(),
        })
        .expect("adapter")
    }

    fn outbound(text: &str) -> OutboundMessage {
        OutboundMessage {
            account_id: "acct-1".to_string(),
            kind: ChannelKind::Mattermost,
            conversation_id: "chan-1".to_string(),
            thread_id: None,
            text: text.to_string(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            idempotency_key: "key-1".to_string(),
        }
    }

    /// Long enough to be split into exactly two `POST /api/v4/posts` calls.
    fn two_chunks() -> String {
        "a".repeat(MATTERMOST_MAX_TEXT_CHARS + 10)
    }

    #[tokio::test]
    async fn a_first_post_whose_response_is_lost_is_reconciled_not_retried() {
        let (base, seen) = scripted_server(vec![Reply::Silence]).await;
        let outcome = adapter_for(&base).send(&outbound("hello")).await;
        assert!(
            matches!(outcome, SendOutcome::NeedsReconciliation { .. }),
            "a request that crossed the network must never be blind-retried: {outcome:?}"
        );
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failure_before_the_request_could_leave_is_retryable() {
        // A port nobody is listening on: the connector fails, so no application
        // byte can have reached a server. This is the one shape that may retry.
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
    async fn a_rate_limit_after_the_first_chunk_landed_is_reconciled() {
        let (base, seen) = scripted_server(vec![
            Reply::Status("201 Created", r#"{"id":"post-1"}"#),
            Reply::Status("429 Too Many Requests", "{}"),
        ])
        .await;
        let outcome = adapter_for(&base).send(&outbound(&two_chunks())).await;
        assert!(
            matches!(outcome, SendOutcome::NeedsReconciliation { .. }),
            "retrying would repost the chunk Mattermost already accepted: {outcome:?}"
        );
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_server_error_after_the_first_chunk_landed_is_reconciled() {
        let (base, _) = scripted_server(vec![
            Reply::Status("201 Created", r#"{"id":"post-1"}"#),
            Reply::Status("500 Internal Server Error", "{}"),
        ])
        .await;
        let outcome = adapter_for(&base).send(&outbound(&two_chunks())).await;
        assert!(
            matches!(outcome, SendOutcome::NeedsReconciliation { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_lost_response_after_the_first_chunk_landed_is_reconciled() {
        let (base, seen) = scripted_server(vec![
            Reply::Status("201 Created", r#"{"id":"post-1"}"#),
            Reply::Silence,
        ])
        .await;
        let outcome = adapter_for(&base).send(&outbound(&two_chunks())).await;
        assert!(
            matches!(outcome, SendOutcome::NeedsReconciliation { .. }),
            "{outcome:?}"
        );
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn two_chunks_that_both_land_are_one_sent_message() {
        let (base, seen) = scripted_server(vec![
            Reply::Status("201 Created", r#"{"id":"post-1"}"#),
            Reply::Status("201 Created", r#"{"id":"post-2"}"#),
        ])
        .await;
        let outcome = adapter_for(&base).send(&outbound(&two_chunks())).await;
        assert_eq!(
            outcome,
            SendOutcome::Sent {
                provider_message_id: Some("post-2".to_string())
            }
        );
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    fn with_attachment(mut message: OutboundMessage) -> OutboundMessage {
        message.attachments = vec![little_monkey_lib::channels::types::OutboundAttachment {
            artifact_id: "artifact-1".to_string(),
            filename: Some("note.txt".to_string()),
            mime_type: Some("text/plain".to_string()),
        }];
        message
    }

    #[tokio::test]
    async fn a_post_that_fails_after_the_upload_landed_is_reconciled() {
        // Upload accepted, post lost: retrying from the beginning would upload
        // the file a second time and might post it twice.
        let (base, seen) = scripted_server(vec![
            Reply::Status("201 Created", r#"{"file_infos":[{"id":"file-1"}]}"#),
            Reply::Silence,
        ])
        .await;
        let adapter = adapter_for(&base).with_blobs(Arc::new(
            crate::daemon::channel_adapter::test_http::FixtureBlobs(b"hello".to_vec()),
        ));
        let outcome = adapter.send(&with_attachment(outbound("see this"))).await;
        assert!(
            matches!(outcome, SendOutcome::NeedsReconciliation { .. }),
            "{outcome:?}"
        );
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_rate_limited_post_after_the_upload_landed_is_reconciled() {
        let (base, _) = scripted_server(vec![
            Reply::Status("201 Created", r#"{"file_infos":[{"id":"file-1"}]}"#),
            Reply::Status("429 Too Many Requests", "{}"),
        ])
        .await;
        let adapter = adapter_for(&base).with_blobs(Arc::new(
            crate::daemon::channel_adapter::test_http::FixtureBlobs(b"hello".to_vec()),
        ));
        let outcome = adapter.send(&with_attachment(outbound("see this"))).await;
        assert!(
            matches!(outcome, SendOutcome::NeedsReconciliation { .. }),
            "a retry would re-upload the file Mattermost already stored: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn an_upload_that_names_no_file_is_reconciled_rather_than_posted() {
        let (base, _) = scripted_server(vec![Reply::Status("201 Created", "{}")]).await;
        let adapter = adapter_for(&base).with_blobs(Arc::new(
            crate::daemon::channel_adapter::test_http::FixtureBlobs(b"hello".to_vec()),
        ));
        let outcome = adapter.send(&with_attachment(outbound("see this"))).await;
        assert!(
            matches!(outcome, SendOutcome::NeedsReconciliation { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_working_token_with_a_dead_socket_is_not_connected() {
        // `/users/me` answers, the WebSocket upgrade never does. Inbound
        // arrives on the socket, so calling this Connected would tell an
        // operator messages are flowing into a channel that receives nothing.
        let (base, _) = scripted_server(vec![
            Reply::Status("200 OK", r#"{"id":"user-9","username":"bot"}"#),
            Reply::Status("500 Internal Server Error", "{}"),
            Reply::Status("200 OK", r#"{"id":"user-9","username":"bot"}"#),
        ])
        .await;
        let adapter = adapter_for(&base);
        let health = adapter.probe().await;
        assert_ne!(
            health.state,
            HealthState::Connected,
            "{health:?} claims a connection the WebSocket never made"
        );
    }

    /// An opt-in round trip against a Mattermost the *operator* names.
    ///
    /// Never runs unless all three variables are set, so CI and every
    /// contributor's `cargo test` skip it. There is no maintainer server, no
    /// maintainer token and no default channel anywhere in this tree: the
    /// destination is one the person running it chose, and it is the only
    /// place a message is ever sent.
    ///
    /// ```text
    /// LM_MATTERMOST_LIVE_URL=https://chat.example.com \
    /// LM_MATTERMOST_LIVE_TOKEN=… \
    /// LM_MATTERMOST_LIVE_CHANNEL=<channel id> \
    ///   cargo test --bin monkey-cli a_live_mattermost_round_trip -- --nocapture
    /// ```
    #[tokio::test]
    async fn a_live_mattermost_round_trip() {
        let (Ok(base_url), Ok(token), Ok(channel)) = (
            std::env::var("LM_MATTERMOST_LIVE_URL"),
            std::env::var("LM_MATTERMOST_LIVE_TOKEN"),
            std::env::var("LM_MATTERMOST_LIVE_CHANNEL"),
        ) else {
            return;
        };
        let account = account_fixture(&base_url);
        let adapter = MattermostAdapter::new(&AdapterConfig {
            account: &account,
            secret: token,
        })
        .expect("adapter");

        // The socket has to come up before health can say Connected, which is
        // the same wait the daemon does.
        for _ in 0..40 {
            if adapter.live_transport() == Some(HealthState::Connected) {
                break;
            }
            let _ = adapter.probe().await;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let health = adapter.probe().await;
        assert_eq!(health.state, HealthState::Connected, "{health:?}");

        let marker = uuid::Uuid::new_v4().simple().to_string();
        let mut message = outbound(&format!("little-monkey live smoke test {marker}"));
        message.conversation_id = channel;
        let outcome = adapter.send(&message).await;
        assert!(matches!(outcome, SendOutcome::Sent { .. }), "{outcome:?}");
    }

    // -- the reader must never block on the consumer --------------------------

    fn posted_envelope(id: &str) -> ChannelEnvelope {
        let mut event = posted_event_fixture();
        let post = event["data"]["post"].as_str().expect("post").to_string();
        event["data"]["post"] = Value::String(post.replace("post-1", id));
        normalize_posted_event("acct-1", &event, Some("user-9"), Some("bot"), 0)
            .expect("normalizes")
    }

    #[tokio::test]
    async fn a_full_queue_drops_and_reports_rather_than_stalling_the_reader() {
        let shared = Arc::new(Shared::default());
        let (tx, _rx) = mpsc::channel(1);

        deliver(&shared, &tx, posted_envelope("post-a"));
        assert_eq!(shared.dropped.load(std::sync::atomic::Ordering::Relaxed), 0);

        // The queue is now full. A second post must not park the reader here —
        // the reader is also what answers the server's pings.
        let overflowed = tokio::time::timeout(Duration::from_millis(200), async {
            deliver(&shared, &tx, posted_envelope("post-b"));
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

    // -- WebSocket ------------------------------------------------------------

    /// A Mattermost stand-in: one HTTP reply to `/users/me`, then a WebSocket
    /// that expects the authentication challenge and pushes one `posted` event.
    ///
    /// Two connections, because that is what the adapter makes: identity first,
    /// socket second.
    async fn websocket_fixture() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let address = listener.local_addr().expect("fixture address");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept identity request");
            let body = r#"{"id":"user-9","username":"bot"}"#;
            let mut request = vec![0u8; 8 * 1024];
            let _ = stream.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            drop(stream);

            let (socket, _) = listener.accept().await.expect("accept websocket");
            let mut ws = tokio_tungstenite::accept_async(socket)
                .await
                .expect("websocket handshake");
            // The adapter authenticates before anything else, and a server that
            // never saw the challenge would happily deliver to a stranger.
            let challenge = ws.next().await.expect("a frame").expect("a text frame");
            let challenge: Value =
                serde_json::from_str(challenge.to_text().expect("text")).expect("json");
            assert_eq!(challenge["action"], "authentication_challenge");
            assert_eq!(challenge["data"]["token"], "tok");

            let _ = ws
                .send(Message::Text(posted_event_fixture().to_string().into()))
                .await;
            // Held open: closing here would look like a dropped connection.
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        format!("http://{address}")
    }

    /// The same stand-in, but it serves `rounds` identity+socket pairs and
    /// drops each socket after one event — which is what an unreliable server
    /// looks like, and what the reconnect loop exists for.
    async fn dropping_websocket_fixture(rounds: usize) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let address = listener.local_addr().expect("fixture address");
        tokio::spawn(async move {
            for round in 0..rounds {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let body = r#"{"id":"user-9","username":"bot"}"#;
                let mut request = vec![0u8; 8 * 1024];
                let _ = stream.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: \
                     {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                drop(stream);

                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let Ok(mut ws) = tokio_tungstenite::accept_async(socket).await else {
                    return;
                };
                let _ = ws.next().await;
                let mut event = posted_event_fixture();
                let post = event["data"]["post"].as_str().expect("post").to_string();
                event["data"]["post"] =
                    Value::String(post.replace("post-1", &format!("post-{round}")));
                let _ = ws.send(Message::Text(event.to_string().into())).await;
                // Dropped, not held: the adapter has to notice and come back.
                let _ = ws.close(None).await;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn a_dropped_socket_is_reconnected_and_keeps_delivering() {
        // Inbound stops forever if the reader gives up on the first close, and
        // that failure is invisible: `poll` returns empty batches exactly as it
        // does on a quiet channel.
        let base = dropping_websocket_fixture(2).await;
        let account = account_fixture(&base);
        let adapter = MattermostAdapter::new(&AdapterConfig {
            account: &account,
            secret: "tok".to_string(),
        })
        .expect("adapter");

        let first = adapter.poll(None).await.expect("poll");
        assert_eq!(first.envelopes.len(), 1, "{first:?}");
        assert_eq!(first.envelopes[0].provider_event_id, "post-0");

        // The second one can only arrive over a connection the loop made
        // itself, after backing off from the first drop.
        let second = adapter.poll(None).await.expect("poll");
        assert_eq!(
            second.envelopes.len(),
            1,
            "the adapter never came back: {second:?}"
        );
        assert_eq!(second.envelopes[0].provider_event_id, "post-1");
    }

    #[tokio::test]
    async fn a_posted_event_travels_the_whole_socket_path_into_poll() {
        let base = websocket_fixture().await;
        let account = account_fixture(&base);
        let adapter = MattermostAdapter::new(&AdapterConfig {
            account: &account,
            secret: "tok".to_string(),
        })
        .expect("adapter");

        let batch = adapter.poll(None).await.expect("poll");
        assert_eq!(batch.envelopes.len(), 1, "{batch:?}");
        let envelope = &batch.envelopes[0];
        assert_eq!(envelope.provider_event_id, "post-1");
        assert_eq!(envelope.conversation.conversation_id, "chan-1");
        // Identity was resolved before the socket opened, so our own posts are
        // recognizable rather than answered.
        assert!(!envelope.sender.is_self);
        assert_eq!(
            adapter.live_transport(),
            Some(HealthState::Connected),
            "a live socket is what Connected means here"
        );
    }
}
