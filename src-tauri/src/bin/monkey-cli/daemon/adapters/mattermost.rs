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
use little_monkey_lib::channels::types::{
    AttachmentKind, AttachmentSource, ChannelAttachment, ChannelConversation, ChannelEnvelope,
    ChannelHealth, ChannelKind, ChannelSender, InboundTransport, OutboundMessage,
    ProviderCapabilities, SendOutcome,
};
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

use crate::daemon::channel_adapter::{AdapterConfig, ChannelAdapter, InboundBatch};

const INBOUND_CHANNEL_CAPACITY: usize = 256;
const POLL_WAIT: Duration = Duration::from_secs(20);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
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
        })
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
            supports_attachments: false,
            supports_mention_metadata: true,
            supports_idempotency_key: false,
            supports_delivery_receipts: false,
            ..ProviderCapabilities::minimal(ChannelKind::Mattermost, InboundTransport::Socket)
        }
    }

    async fn probe(&self) -> ChannelHealth {
        self.ensure_started().await;
        let now = now_ms();
        if let Some(error) = self.shared.permanent_error.lock().await.clone() {
            return ChannelHealth::error(now, error);
        }
        match fetch_me(&self.http, &self.base_url, &self.token).await {
            Ok(me) => ChannelHealth::connected(
                now,
                Some(format!("Connected to Mattermost as {}", me.username)),
            ),
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

fn map_send_status(
    status: u16,
    any_sent_before: bool,
    retry_after_ms: Option<i64>,
) -> Option<SendOutcome> {
    match status {
        200..=299 => None,
        429 => Some(SendOutcome::RetryableFailure {
            error: "Mattermost rate limited the request".to_string(),
            retry_after_ms,
        }),
        401 | 403 => Some(SendOutcome::PermanentFailure {
            error: format!("Mattermost rejected the request: HTTP {status}"),
        }),
        500..=599 => Some(if any_sent_before {
            SendOutcome::NeedsReconciliation {
                error: format!("Mattermost returned HTTP {status}"),
            }
        } else {
            SendOutcome::RetryableFailure {
                error: format!("Mattermost returned HTTP {status}"),
                retry_after_ms: None,
            }
        }),
        _ => Some(SendOutcome::PermanentFailure {
            error: format!("Mattermost rejected the message: HTTP {status}"),
        }),
    }
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
        let me = match fetch_me(&http, &base_url, &token).await {
            Ok(me) => Some(me),
            Err(FetchMeError::Permanent(error)) => {
                *shared.permanent_error.lock().await = Some(error);
                return;
            }
            Err(FetchMeError::Retryable(_)) => None,
        };
        let reconnected =
            run_one_connection(&account_id, &base_url, &token, me.as_ref(), &tx).await;
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

async fn run_one_connection(
    account_id: &str,
    base_url: &str,
    token: &str,
    me: Option<&Me>,
    tx: &mpsc::Sender<ChannelEnvelope>,
) -> bool {
    let (mut ws, _) = match tokio_tungstenite::connect_async(websocket_url(base_url)).await {
        Ok(pair) => pair,
        Err(_) => return false,
    };
    let challenge = authentication_challenge(token);
    if ws
        .send(Message::Text(challenge.to_string().into()))
        .await
        .is_err()
    {
        return false;
    }

    let our_user_id = me.map(|me| me.user_id.as_str());
    let our_username = me.map(|me| me.username.as_str());

    loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => {
                for action in
                    handle_socket_frame(account_id, &text, our_user_id, our_username, now_ms())
                {
                    match action {
                        Action::Envelope(envelope) => {
                            let _ = tx.send(*envelope).await;
                        }
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
}
