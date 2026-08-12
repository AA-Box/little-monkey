//! Matrix adapter: Client-Server REST API directly against the operator's
//! own homeserver, long-polling `/sync`. No `matrix-sdk` dependency — this
//! speaks the REST API by hand through
//! `little_monkey_lib::egress::hardened()` / `egress::send`, this tree's one
//! hardened `reqwest` entry point (see `egress.rs`).
//!
//! # Encryption is NOT implemented
//!
//! This adapter cannot read `m.room.encrypted` events — no Olm/Megolm
//! session handling, no device keys, no key backup. An encrypted room
//! delivers events this adapter cannot decrypt; [`normalize_sync`] SKIPS
//! them and COUNTS them rather than silently dropping them, and `poll`
//! folds that count into a running total that `probe`'s connected detail
//! surfaces, so an operator watching the account's health can notice rather
//! than wonder why a room goes quiet. [`ProviderCapabilities`] makes no
//! claim of encryption support, because there is none.
//!
//! # DM detection
//!
//! The Client-Server REST API has no reliable "is this room a DM" bit on
//! the room itself — a two-person room can legitimately be a group, and a
//! DM that later adds a bot is still a DM. The real signal is the
//! `m.direct` account data event
//! (`GET /_matrix/client/v3/user/{user_id}/account_data/m.direct`), a map
//! of other-user-id to the list of room ids that are DMs with them. Fetched
//! once per adapter instance and cached; a room id absent from every cached
//! list falls back to `Group` rather than guessing — see [`dm_room_ids`].

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::daemon::channel_adapter::{AdapterConfig, ChannelAdapter, InboundBatch};
use little_monkey_lib::channels::types::{
    AttachmentKind, AttachmentSource, ChannelAttachment, ChannelConversation, ChannelEnvelope,
    ChannelHealth, ChannelKind, ChannelSender, ConversationKind, InboundTransport, OutboundMessage,
    ProviderCapabilities, SendOutcome,
};

/// Matrix events are capped by the homeserver's own `max_event_size`
/// (Synapse's default is 65536 bytes for the whole serialized event, not
/// just the body). A conservative character budget, not a byte-exact one —
/// see `telegram.rs`'s `MAX_MESSAGE_UTF16` note on chars vs wire units.
const MAX_TEXT_CHARS: usize = 32_000;

pub struct MatrixAdapter {
    homeserver_url: String,
    user_id: String,
    access_token: String,
    /// `None` until the first successful (or attempted) `m.direct` fetch —
    /// see the module doc.
    dm_rooms: Mutex<Option<HashSet<String>>>,
    /// Running total of `m.room.encrypted` events skipped across every
    /// `poll` this adapter instance has made. Surfaced in `probe`'s detail.
    encrypted_skipped: AtomicU64,
}

impl MatrixAdapter {
    pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
        if config.secret.trim().is_empty() {
            return Err("Matrix requires an access token".to_string());
        }
        let raw_homeserver = config
            .account
            .non_secret_config
            .get("homeserver_url")
            .and_then(Value::as_str)
            .ok_or_else(|| "Matrix account is missing homeserver_url".to_string())?;
        let homeserver_url = validate_homeserver_url(raw_homeserver)?;
        let user_id = config
            .account
            .non_secret_config
            .get("user_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "Matrix account is missing user_id".to_string())?
            .to_string();
        Ok(Self {
            homeserver_url,
            user_id,
            access_token: config.secret.clone(),
            dm_rooms: Mutex::new(None),
            encrypted_skipped: AtomicU64::new(0),
        })
    }

    fn client(&self) -> Result<reqwest::Client, String> {
        little_monkey_lib::egress::hardened()
            .build()
            .map_err(|error| format!("Failed to build the Matrix HTTP client: {error}"))
    }

    fn authorize(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder.bearer_auth(&self.access_token)
    }

    /// Returns the cached DM room-id set, fetching and caching it on first
    /// use. A fetch failure (or an account with no `m.direct` recorded yet,
    /// which the homeserver reports as 404) caches an empty set — every room
    /// then falls back to `Group`, never to an error that would stall
    /// `poll`.
    async fn dm_room_ids(&self) -> HashSet<String> {
        if let Some(cached) = self.dm_rooms.lock().unwrap().clone() {
            return cached;
        }
        let fetched = self.fetch_dm_room_ids().await.unwrap_or_default();
        *self.dm_rooms.lock().unwrap() = Some(fetched.clone());
        fetched
    }

    async fn fetch_dm_room_ids(&self) -> Result<HashSet<String>, String> {
        let client = self.client()?;
        let url = format!(
            "{}/_matrix/client/v3/user/{}/account_data/m.direct",
            self.homeserver_url,
            encode_path_segment(&self.user_id)
        );
        let request = self.authorize(client.get(url));
        let response = little_monkey_lib::egress::send(request)
            .await
            .map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            // Includes 404, which is exactly what an account with no DMs
            // recorded yet returns — not an error, just "nothing cached".
            return Ok(HashSet::new());
        }
        let body: BTreeMap<String, Vec<String>> =
            response.json().await.map_err(|error| error.to_string())?;
        Ok(body.into_values().flatten().collect())
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Validates `non_secret_config.homeserver_url`: a bare origin (no path, no
/// query) so it can never be mistaken for an API path — mirrors
/// `mattermost.rs`'s `validate_base_url` and the same reasoning: this is a
/// trust boundary once an access token goes out on every request. `https`
/// is required; plain `http` is accepted only for
/// localhost/127.0.0.1/::1, which is how a self-hosted homeserver is
/// commonly reached during setup.
fn validate_homeserver_url(raw: &str) -> Result<String, String> {
    let parsed =
        url::Url::parse(raw).map_err(|_| "Matrix homeserver_url is not a valid URL".to_string())?;
    if !matches!(parsed.path(), "" | "/") {
        return Err("Matrix homeserver_url must not include a path".to_string());
    }
    if parsed.query().is_some() {
        return Err("Matrix homeserver_url must not include a query string".to_string());
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
                    "Matrix homeserver_url must be https (plain http is only accepted for localhost)"
                        .to_string(),
                );
            }
        }
        _ => return Err("Matrix homeserver_url must use http or https".to_string()),
    }
    Ok(raw.trim_end_matches('/').to_string())
}

/// Percent-encodes one path segment (RFC 3986 unreserved set kept literal,
/// everything else escaped). Matrix identifiers routinely carry `@`, `:` and
/// `!`, all of which must survive as exactly one path segment.
fn encode_path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// True when `needle` appears in `text` at a word boundary (a Unicode
/// alphanumeric character or `_` on either side counts as "still inside a
/// word"). Same shape as `irc.rs`'s `mentions_nick`, generalized to Matrix
/// ids and non-ASCII text, which is why this is char-indexed rather than
/// byte-indexed.
fn mentions_word_boundary(text: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let haystack: Vec<char> = text.to_lowercase().chars().collect();
    let needle: Vec<char> = needle.to_lowercase().chars().collect();
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    let is_word_char = |c: char| c.is_alphanumeric() || c == '_';
    for start in 0..=(haystack.len() - needle.len()) {
        if haystack[start..start + needle.len()] == needle[..] {
            let end = start + needle.len();
            let before_ok = start == 0 || !is_word_char(haystack[start - 1]);
            let after_ok = end >= haystack.len() || !is_word_char(haystack[end]);
            if before_ok && after_ok {
                return true;
            }
        }
    }
    false
}

#[async_trait]
impl ChannelAdapter for MatrixAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Matrix
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            kind: ChannelKind::Matrix,
            // The Client-Server `/sync` endpoint this adapter polls behaves
            // exactly like Telegram's long-poll `getUpdates` — a bounded
            // GET that blocks server-side — not like a held-open socket.
            // `InboundTransport`'s own doc comment groups Matrix with the
            // socket-based providers, which describes a matrix-sdk-style
            // persistent connection this adapter deliberately does not use.
            inbound_transport: InboundTransport::LongPoll,
            max_text_chars: MAX_TEXT_CHARS,
            supports_threads: false,
            supports_attachments: false, // inbound only: this adapter does not upload files yet
            supports_mention_metadata: true,
            // The PUT txn_id below is a real caller-supplied idempotency key
            // the homeserver dedupes on.
            supports_idempotency_key: true,
            supports_delivery_receipts: false,
        }
    }

    async fn probe(&self) -> ChannelHealth {
        let now = now_ms();
        let client = match self.client() {
            Ok(client) => client,
            Err(error) => return ChannelHealth::error(now, error),
        };
        let url = format!("{}/_matrix/client/v3/account/whoami", self.homeserver_url);
        let request = self.authorize(client.get(url));
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => {
                return ChannelHealth::error(
                    now,
                    format!("Could not reach the Matrix homeserver: {error}"),
                )
            }
        };
        let status = response.status();
        if status.as_u16() == 401 {
            return ChannelHealth::error(
                now,
                "The Matrix homeserver rejected the access token (401)",
            );
        }
        if !status.is_success() {
            return ChannelHealth::error(now, format!("Matrix returned {status} for whoami"));
        }
        let body = response.text().await.unwrap_or_default();
        match serde_json::from_str::<WhoAmI>(&body) {
            Ok(parsed) => {
                let skipped = self.encrypted_skipped.load(Ordering::Relaxed);
                let detail = if skipped > 0 {
                    format!(
                        "{} ({skipped} encrypted message(s) could not be read and were skipped)",
                        parsed.user_id
                    )
                } else {
                    parsed.user_id
                };
                ChannelHealth::connected(now, Some(detail))
            }
            Err(_) => ChannelHealth::error(now, "Matrix returned an unexpected whoami response"),
        }
    }

    async fn poll(&self, cursor: Option<&str>) -> Result<InboundBatch, String> {
        let client = self.client()?;
        let mut query = vec![("timeout".to_string(), "25000".to_string())];
        if let Some(since) = cursor {
            query.push(("since".to_string(), since.to_string()));
        }
        let url = format!("{}/_matrix/client/v3/sync", self.homeserver_url);
        let request = self.authorize(client.get(url).query(&query));
        let response = little_monkey_lib::egress::send(request)
            .await
            .map_err(|error| format!("Matrix sync failed: {error}"))?;
        let status = response.status();
        if status.as_u16() == 401 {
            return Err("Matrix rejected the access token (401) during sync".to_string());
        }
        if !status.is_success() {
            return Err(format!("Matrix returned {status} for sync"));
        }
        let body: SyncResponse = response
            .json()
            .await
            .map_err(|error| format!("Matrix sync parse failed: {error}"))?;
        let dm_rooms = self.dm_room_ids().await;
        let (envelopes, encrypted_skipped) = normalize_sync(&body, &self.user_id, &dm_rooms);
        if encrypted_skipped > 0 {
            self.encrypted_skipped
                .fetch_add(encrypted_skipped, Ordering::Relaxed);
        }
        let next_batch = body.next_batch;
        Ok(InboundBatch {
            envelopes,
            cursor: Some(next_batch),
        })
    }

    async fn send(&self, message: &OutboundMessage) -> SendOutcome {
        let client = match self.client() {
            Ok(client) => client,
            Err(error) => return SendOutcome::PermanentFailure { error },
        };
        let mut body = serde_json::json!({
            "msgtype": "m.text",
            "body": message.text,
        });
        if let Some(reply_to) = &message.reply_to_provider_id {
            body["m.relates_to"] = serde_json::json!({
                "m.in_reply_to": { "event_id": reply_to },
            });
        }
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            self.homeserver_url,
            encode_path_segment(&message.conversation_id),
            // Matrix dedupes a PUT by (access token, txn_id) — exactly the
            // idempotency guarantee the outbox needs on a retried send.
            encode_path_segment(&message.idempotency_key),
        );
        let request = self.authorize(client.put(url).json(&body));
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => {
                return if error.is_connect() {
                    // The TCP/TLS handshake itself failed: the request
                    // provably never left this machine.
                    SendOutcome::RetryableFailure {
                        error: format!("Could not connect to the Matrix homeserver: {error}"),
                        retry_after_ms: None,
                    }
                } else {
                    // Anything else may have happened after the request was
                    // already written, so whether the homeserver received it
                    // is unknown.
                    SendOutcome::NeedsReconciliation {
                        error: format!("Matrix send outcome unknown: {error}"),
                    }
                };
            }
        };
        let status = response.status();
        if status.as_u16() == 429 {
            return SendOutcome::RetryableFailure {
                error: "Matrix rate-limited the request (429)".to_string(),
                retry_after_ms: None,
            };
        }
        let body_text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return SendOutcome::PermanentFailure {
                error: format!("Matrix returned {status} for send"),
            };
        }
        match serde_json::from_str::<SendResponse>(&body_text) {
            Ok(parsed) => SendOutcome::Sent {
                provider_message_id: Some(parsed.event_id),
            },
            Err(_) => SendOutcome::NeedsReconciliation {
                error: "Matrix accepted the send but returned an unparseable response".to_string(),
            },
        }
    }
}

#[derive(Debug, Deserialize)]
struct WhoAmI {
    user_id: String,
}

#[derive(Debug, Deserialize)]
struct SendResponse {
    event_id: String,
}

#[derive(Debug, Deserialize)]
struct SyncResponse {
    next_batch: String,
    #[serde(default)]
    rooms: Option<SyncRooms>,
}

#[derive(Debug, Deserialize)]
struct SyncRooms {
    #[serde(default)]
    join: BTreeMap<String, JoinedRoom>,
}

#[derive(Debug, Deserialize)]
struct JoinedRoom {
    #[serde(default)]
    timeline: Timeline,
}

#[derive(Debug, Default, Deserialize)]
struct Timeline {
    #[serde(default)]
    events: Vec<RoomEvent>,
}

#[derive(Debug, Deserialize)]
struct RoomEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    event_id: String,
    #[serde(default)]
    sender: String,
    #[serde(default)]
    origin_server_ts: i64,
    #[serde(default)]
    content: Value,
}

/// Normalizes one `/sync` response into envelopes plus the number of
/// `m.room.encrypted` events it had to skip. Pure — no network, no clock —
/// so it is what the tests below exercise directly.
fn normalize_sync(
    body: &SyncResponse,
    self_user_id: &str,
    dm_rooms: &HashSet<String>,
) -> (Vec<ChannelEnvelope>, u64) {
    let mut envelopes = Vec::new();
    let mut encrypted_skipped = 0u64;
    let Some(rooms) = &body.rooms else {
        return (envelopes, encrypted_skipped);
    };
    for (room_id, room) in &rooms.join {
        for event in &room.timeline.events {
            if event.event_type == "m.room.encrypted" {
                encrypted_skipped += 1;
                continue;
            }
            if event.event_type != "m.room.message" {
                continue;
            }
            if let Some(envelope) = normalize_message_event(room_id, event, self_user_id, dm_rooms)
            {
                envelopes.push(envelope);
            }
        }
    }
    (envelopes, encrypted_skipped)
}

fn normalize_message_event(
    room_id: &str,
    event: &RoomEvent,
    self_user_id: &str,
    dm_rooms: &HashSet<String>,
) -> Option<ChannelEnvelope> {
    let msgtype = event
        .content
        .get("msgtype")
        .and_then(Value::as_str)
        .unwrap_or("");
    let body_text = event
        .content
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let mut attachments = Vec::new();
    let text = match msgtype {
        "m.text" => body_text,
        "m.image" | "m.file" | "m.audio" | "m.video" => {
            if let Some(mxc) = event.content.get("url").and_then(Value::as_str) {
                let kind = match msgtype {
                    "m.image" => AttachmentKind::Image,
                    "m.file" => AttachmentKind::Document,
                    "m.audio" => AttachmentKind::Audio,
                    _ => AttachmentKind::Video,
                };
                attachments.push(ChannelAttachment {
                    stored_artifact_id: None,
                    text_excerpt: None,
                    fetch_error: None,
                    provider_id: Some(mxc.to_string()),
                    kind,
                    filename: (!body_text.is_empty()).then(|| body_text.clone()),
                    mime_type: event
                        .content
                        .get("info")
                        .and_then(|info| info.get("mimetype"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    declared_size_bytes: event
                        .content
                        .get("info")
                        .and_then(|info| info.get("size"))
                        .and_then(Value::as_u64),
                    source: AttachmentSource::ProviderHandle {
                        handle: mxc.to_string(),
                    },
                });
            }
            String::new()
        }
        // Unrecognized msgtype (m.location, m.key.verification.request, a
        // future type): nothing here to normalize into text or attachment.
        _ => return None,
    };

    if text.is_empty() && attachments.is_empty() {
        return None;
    }

    let reply_to_provider_id = event
        .content
        .get("m.relates_to")
        .and_then(|relates| relates.get("m.in_reply_to"))
        .and_then(|reply| reply.get("event_id"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let mentioned_by_metadata = event
        .content
        .get("m.mentions")
        .and_then(|mentions| mentions.get("user_ids"))
        .and_then(Value::as_array)
        .map(|ids| ids.iter().any(|id| id.as_str() == Some(self_user_id)))
        .unwrap_or(false);
    let localpart = self_user_id
        .strip_prefix('@')
        .and_then(|rest| rest.split(':').next())
        .unwrap_or(self_user_id);
    let mentions_self = mentioned_by_metadata
        || mentions_word_boundary(&text, self_user_id)
        || mentions_word_boundary(&text, localpart);

    let conversation = if dm_rooms.contains(room_id) {
        ChannelConversation::direct(room_id.to_string())
    } else {
        ChannelConversation::group(room_id.to_string())
    };

    Some(ChannelEnvelope {
        account_id: String::new(),
        kind: ChannelKind::Matrix,
        provider_event_id: event.event_id.clone(),
        conversation,
        sender: ChannelSender {
            sender_id: event.sender.clone(),
            display_label: None,
            is_self: event.sender == self_user_id,
            is_bot: false,
        },
        text,
        attachments,
        reply_to_provider_id,
        mentions_self,
        received_at_ms: event.origin_server_ts,
        metadata: little_monkey_lib::channels::types::BoundedMetadata::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SELF_ID: &str = "@self:example.org";

    fn dm_rooms() -> HashSet<String> {
        ["!dm:example.org".to_string()].into_iter().collect()
    }

    fn parse(json: &str) -> SyncResponse {
        serde_json::from_str(json).expect("fixture parses")
    }

    const TWO_ROOMS: &str = r#"{
        "next_batch": "s1",
        "rooms": {
            "join": {
                "!dm:example.org": {
                    "timeline": {
                        "events": [{
                            "type": "m.room.message",
                            "event_id": "$1",
                            "sender": "@bob:example.org",
                            "origin_server_ts": 1700000000000,
                            "content": {"msgtype": "m.text", "body": "hello there"}
                        }]
                    }
                },
                "!group:example.org": {
                    "timeline": {
                        "events": [{
                            "type": "m.room.message",
                            "event_id": "$2",
                            "sender": "@carol:example.org",
                            "origin_server_ts": 1700000001000,
                            "content": {"msgtype": "m.text", "body": "hi @self:example.org can you help"}
                        }]
                    }
                }
            }
        }
    }"#;

    #[test]
    fn distinguishes_dm_from_group_via_cached_m_direct() {
        let sync = parse(TWO_ROOMS);
        let (envelopes, skipped) = normalize_sync(&sync, SELF_ID, &dm_rooms());
        assert_eq!(skipped, 0);
        let dm = envelopes
            .iter()
            .find(|e| e.conversation.conversation_id == "!dm:example.org")
            .unwrap();
        assert_eq!(dm.conversation.kind, ConversationKind::Direct);
        let group = envelopes
            .iter()
            .find(|e| e.conversation.conversation_id == "!group:example.org")
            .unwrap();
        assert_eq!(group.conversation.kind, ConversationKind::Group);
    }

    #[test]
    fn unknown_room_falls_back_to_group() {
        let sync = parse(TWO_ROOMS);
        let (envelopes, _) = normalize_sync(&sync, SELF_ID, &HashSet::new());
        for envelope in &envelopes {
            assert_eq!(envelope.conversation.kind, ConversationKind::Group);
        }
    }

    #[test]
    fn detects_metadata_and_text_mentions() {
        let sync = parse(TWO_ROOMS);
        let (envelopes, _) = normalize_sync(&sync, SELF_ID, &dm_rooms());
        let group = envelopes
            .iter()
            .find(|e| e.conversation.conversation_id == "!group:example.org")
            .unwrap();
        assert!(group.mentions_self);
        let dm = envelopes
            .iter()
            .find(|e| e.conversation.conversation_id == "!dm:example.org")
            .unwrap();
        assert!(!dm.mentions_self);
    }

    #[test]
    fn suppresses_self_only_for_the_configured_user() {
        let sync = parse(TWO_ROOMS);
        let (envelopes, _) = normalize_sync(&sync, "@bob:example.org", &dm_rooms());
        let dm = envelopes
            .iter()
            .find(|e| e.conversation.conversation_id == "!dm:example.org")
            .unwrap();
        assert!(dm.sender.is_self);
        let group = envelopes
            .iter()
            .find(|e| e.conversation.conversation_id == "!group:example.org")
            .unwrap();
        assert!(!group.sender.is_self);
    }

    const REPLY_EVENT: &str = r#"{
        "next_batch": "s2",
        "rooms": {
            "join": {
                "!dm:example.org": {
                    "timeline": {
                        "events": [{
                            "type": "m.room.message",
                            "event_id": "$3",
                            "sender": "@bob:example.org",
                            "origin_server_ts": 1700000002000,
                            "content": {
                                "msgtype": "m.text",
                                "body": "sure thing",
                                "m.relates_to": {"m.in_reply_to": {"event_id": "$1"}}
                            }
                        }]
                    }
                }
            }
        }
    }"#;

    #[test]
    fn carries_the_reply_target_id() {
        let sync = parse(REPLY_EVENT);
        let (envelopes, _) = normalize_sync(&sync, SELF_ID, &dm_rooms());
        assert_eq!(envelopes[0].reply_to_provider_id.as_deref(), Some("$1"));
    }

    const IMAGE_EVENT: &str = r#"{
        "next_batch": "s3",
        "rooms": {
            "join": {
                "!dm:example.org": {
                    "timeline": {
                        "events": [{
                            "type": "m.room.message",
                            "event_id": "$4",
                            "sender": "@bob:example.org",
                            "origin_server_ts": 1700000003000,
                            "content": {
                                "msgtype": "m.image",
                                "body": "photo.png",
                                "url": "mxc://example.org/abc123",
                                "info": {"mimetype": "image/png", "size": 4096}
                            }
                        }]
                    }
                }
            }
        }
    }"#;

    #[test]
    fn normalizes_an_image_as_a_provider_handle_attachment() {
        let sync = parse(IMAGE_EVENT);
        let (envelopes, _) = normalize_sync(&sync, SELF_ID, &dm_rooms());
        assert_eq!(envelopes.len(), 1);
        let attachment = &envelopes[0].attachments[0];
        assert_eq!(attachment.kind, AttachmentKind::Image);
        assert_eq!(attachment.declared_size_bytes, Some(4096));
        match &attachment.source {
            AttachmentSource::ProviderHandle { handle } => {
                assert_eq!(handle, "mxc://example.org/abc123")
            }
            other => panic!("expected a provider handle, got {other:?}"),
        }
    }

    const ENCRYPTED_EVENT: &str = r#"{
        "next_batch": "s4",
        "rooms": {
            "join": {
                "!dm:example.org": {
                    "timeline": {
                        "events": [
                            {
                                "type": "m.room.encrypted",
                                "event_id": "$5",
                                "sender": "@bob:example.org",
                                "origin_server_ts": 1700000004000,
                                "content": {"algorithm": "m.megolm.v1.aes-sha2"}
                            },
                            {
                                "type": "m.room.message",
                                "event_id": "$6",
                                "sender": "@bob:example.org",
                                "origin_server_ts": 1700000005000,
                                "content": {"msgtype": "m.text", "body": "readable"}
                            }
                        ]
                    }
                }
            }
        }
    }"#;

    #[test]
    fn skips_and_counts_encrypted_events_without_dropping_the_rest_of_the_batch() {
        let sync = parse(ENCRYPTED_EVENT);
        let (envelopes, skipped) = normalize_sync(&sync, SELF_ID, &dm_rooms());
        assert_eq!(skipped, 1);
        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[0].text, "readable");
    }

    #[test]
    fn next_batch_becomes_the_new_cursor_value() {
        let sync = parse(TWO_ROOMS);
        assert_eq!(sync.next_batch, "s1");
    }

    #[test]
    fn homeserver_url_requires_https_except_for_localhost() {
        assert!(validate_homeserver_url("https://matrix.example.org").is_ok());
        assert!(validate_homeserver_url("http://matrix.example.org").is_err());
        assert!(validate_homeserver_url("http://localhost:8008").is_ok());
        assert!(validate_homeserver_url("http://127.0.0.1:8008").is_ok());
    }

    #[test]
    fn homeserver_url_rejects_a_path_or_query() {
        assert!(validate_homeserver_url("https://matrix.example.org/_matrix").is_err());
        assert!(validate_homeserver_url("https://matrix.example.org?x=1").is_err());
    }

    #[test]
    fn mention_matches_at_word_boundaries_only() {
        assert!(mentions_word_boundary(
            "hi @self:example.org there",
            "@self:example.org"
        ));
        assert!(!mentions_word_boundary(
            "notself:example.org",
            "self:example.org"
        ));
        assert!(mentions_word_boundary("SELF is here", "self"));
    }

    fn test_account(
        non_secret_config: Value,
    ) -> super::super::super::channel_store::ChannelAccountRecord {
        super::super::super::channel_store::ChannelAccountRecord {
            account_id: "acct-1".to_string(),
            kind: ChannelKind::Matrix,
            label: "Matrix".to_string(),
            enabled: true,
            non_secret_config,
            credential_ref: Some("matrix/acct-1".to_string()),
            access_policy: Default::default(),
            health: ChannelHealth::error(0, "unused"),
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn rejects_a_missing_access_token() {
        let account = test_account(serde_json::json!({
            "homeserver_url": "https://matrix.example.org",
            "user_id": SELF_ID,
        }));
        let config = AdapterConfig {
            account: &account,
            secret: String::new(),
        };
        assert!(MatrixAdapter::new(&config).is_err());
    }

    #[test]
    fn rejects_a_missing_homeserver_url() {
        let account = test_account(serde_json::json!({ "user_id": SELF_ID }));
        let config = AdapterConfig {
            account: &account,
            secret: "token".to_string(),
        };
        assert!(MatrixAdapter::new(&config).is_err());
    }

    #[test]
    fn rejects_a_missing_user_id() {
        let account =
            test_account(serde_json::json!({ "homeserver_url": "https://matrix.example.org" }));
        let config = AdapterConfig {
            account: &account,
            secret: "token".to_string(),
        };
        assert!(MatrixAdapter::new(&config).is_err());
    }

    #[test]
    fn capabilities_report_the_declared_text_limit_and_no_encryption_claim() {
        let account = test_account(serde_json::json!({
            "homeserver_url": "https://matrix.example.org",
            "user_id": SELF_ID,
        }));
        let config = AdapterConfig {
            account: &account,
            secret: "token".to_string(),
        };
        let adapter = MatrixAdapter::new(&config).expect("adapter");
        let capabilities = adapter.capabilities();
        assert_eq!(capabilities.max_text_chars, MAX_TEXT_CHARS);
        assert_eq!(capabilities.kind, ChannelKind::Matrix);
        // Inbound attachments are normalized, but nothing here uploads one, and
        // the capability says what this adapter does rather than what Matrix
        // could do.
        assert!(!capabilities.supports_attachments);
    }

    #[test]
    fn accepts_a_fully_configured_account() {
        let account = test_account(serde_json::json!({
            "homeserver_url": "https://matrix.example.org",
            "user_id": SELF_ID,
        }));
        let config = AdapterConfig {
            account: &account,
            secret: "token".to_string(),
        };
        assert!(MatrixAdapter::new(&config).is_ok());
    }
}
