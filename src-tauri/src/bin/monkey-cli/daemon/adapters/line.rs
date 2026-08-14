//! LINE Messaging API adapter (official webhook integration only).
//!
//! Inbound is a webhook: LINE signs the raw POST body with the channel secret
//! and sends the base64 digest in `X-Line-Signature`. As with WhatsApp, the
//! signature covers only the body — there is no separate signed timestamp
//! header to enforce a skew window against — so redelivery safety comes from
//! `provider_event_id` dedupe (`webhookEventId`, or the message id as a
//! fallback for older payloads that predate it).
//!
//! # Outbound, and why a reply token is never trusted twice
//!
//! LINE offers two send paths. The reply endpoint answers a specific inbound
//! event using the `replyToken` that event carried; it is free of the push
//! quota, but the token is **single use and short lived**. The push endpoint
//! addresses the conversation itself and always works.
//!
//! An agent turn can take minutes and can outlive a restart, so a queued reply
//! must never *depend* on a token that was minted before it started. This
//! adapter therefore treats the token as a durable but expiring optimization:
//! [`WebhookChannelAdapter::verify_and_normalize`] records it with the moment
//! it arrived, and [`ChannelAdapter::send`] uses it only while it is inside
//! [`REPLY_TOKEN_USABLE_MS`] — clearing it *before* the request goes out, so a
//! retry can never replay a token the provider has already retired. Every
//! other send, including every send after a restart that took long enough, is
//! a push to the normalized destination. Nothing is ever dropped for want of a
//! valid token.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use little_monkey_lib::channels::types::{
    AttachmentKind, AttachmentSource, BoundedMetadata, ChannelAttachment, ChannelConversation,
    ChannelEnvelope, ChannelHealth, ChannelKind, ChannelSender, InboundTransport, OutboundMessage,
    ProviderCapabilities, SendOutcome,
};
use serde::Deserialize;
use serde_json::Value as JsonValue;

use crate::daemon::channel_adapter::{
    AdapterConfig, ChannelAdapter, ConversationReferences, DaemonConversationReferences,
    InboundBatch, WebhookChannelAdapter,
};

const LINE_API_BASE: &str = "https://api.line.me";
/// LINE serves message content (images, video, audio, files) from its own
/// data host rather than the API host.
const LINE_CONTENT_BASE: &str = "https://api-data.line.me";
/// LINE's own per-message character cap.
const MAX_TEXT_CHARS: usize = 5000;
/// How long after an event arrived its reply token is still worth trying.
///
/// LINE gives a reply token a short life and one use. Deliberately shorter
/// than the provider's own window: the cost of being wrong in this direction
/// is one push message that would have been free, and in the other direction
/// it is an answer the sender never receives.
const REPLY_TOKEN_USABLE_MS: i64 = 50_000;

/// The two secrets this provider needs, bundled into the single opaque
/// `AdapterConfig::secret` string as JSON. Neither is ever logged.
#[derive(Debug, Deserialize)]
struct LineSecrets {
    channel_secret: String,
    channel_access_token: String,
}

pub struct LineAdapter {
    account_id: String,
    channel_secret: String,
    channel_access_token: String,
    /// The Messaging API origin. Always [`LINE_API_BASE`] in production;
    /// swappable in tests so `send`/`probe` can be exercised against a
    /// loopback fixture instead of the real network.
    api_base: String,
    /// Where message *content* lives, which LINE serves from a different host
    /// than its API. Always [`LINE_CONTENT_BASE`] in production.
    content_base: String,
    /// The freshest reply token per conversation, with when it arrived.
    /// Durable so a reply queued just before a restart can still take the
    /// cheap path; expiring so nothing ever waits on one. See the module doc.
    references: std::sync::Arc<dyn ConversationReferences>,
}

impl LineAdapter {
    pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
        let secrets: LineSecrets = serde_json::from_str(&config.secret)
            .map_err(|_| "LINE account credential is missing or malformed".to_string())?;
        if secrets.channel_secret.trim().is_empty()
            || secrets.channel_access_token.trim().is_empty()
        {
            return Err(
                "LINE account credential is missing channel_secret or channel_access_token"
                    .to_string(),
            );
        }
        Ok(Self {
            account_id: config.account.account_id.clone(),
            channel_secret: secrets.channel_secret,
            channel_access_token: secrets.channel_access_token,
            api_base: LINE_API_BASE.to_string(),
            content_base: LINE_CONTENT_BASE.to_string(),
            references: std::sync::Arc::new(DaemonConversationReferences),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_base_url(mut self, base: &str) -> Self {
        self.api_base = base.to_string();
        self
    }

    /// Swap the durable reference store, as the restart tests do to prove a
    /// second adapter answers what the first was told.
    #[cfg(test)]
    pub(crate) fn with_references(
        mut self,
        references: std::sync::Arc<dyn ConversationReferences>,
    ) -> Self {
        self.references = references;
        self
    }

    /// Claim this conversation's reply token if it is still usable, removing it
    /// as it is handed out.
    ///
    /// Removal happens *before* the send rather than after it succeeds, which
    /// is the whole point: LINE retires a reply token on first use, so a
    /// retried outbox row must take the push path rather than replay it. The
    /// cost is that a reply lost to a connection failure is pushed instead of
    /// replied — the same message, through the path that always works.
    fn claim_reply_token(&self, conversation_id: &str, now_ms: i64) -> Option<String> {
        let stored = self.references.get(&self.account_id, conversation_id)?;
        let token = stored
            .get("reply_token")
            .and_then(JsonValue::as_str)?
            .to_string();
        let issued_at_ms = stored.get("issued_at_ms").and_then(JsonValue::as_i64)?;
        let _ = self.references.clear(&self.account_id, conversation_id);
        // A clock that moved backwards, or an event stamped in the future,
        // both read as "not provably fresh" and fall through to push.
        let age_ms = now_ms.checked_sub(issued_at_ms)?;
        (0..REPLY_TOKEN_USABLE_MS)
            .contains(&age_ms)
            .then_some(token)
    }
}

impl WebhookChannelAdapter for LineAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Line
    }

    fn verify_and_normalize(
        &self,
        headers: &[(String, String)],
        body: &[u8],
        _public_base_url: Option<&str>,
        now_ms: i64,
    ) -> Result<Vec<ChannelEnvelope>, String> {
        // LINE's signature covers only the body, same as WhatsApp's — see the
        // module doc for why there is no timestamp skew check here, and why
        // `public_base_url` is unused (it is only for providers whose
        // signature covers the delivery URL itself).
        let signature = headers
            .iter()
            .find(|(name, _)| name == "x-line-signature")
            .map(|(_, value)| value.as_str())
            .ok_or_else(|| "Missing X-Line-Signature header".to_string())?;
        verify_line_signature(&self.channel_secret, body, signature)
            .map_err(|_| "LINE webhook signature verification failed".to_string())?;

        let payload: JsonValue = serde_json::from_slice(body)
            .map_err(|error| format!("LINE webhook body is not valid JSON: {error}"))?;
        let envelopes = normalize_payload(&payload, &self.account_id, now_ms);

        // Only now that the signature has verified: a reply token decides where
        // an outbound message goes, so an unsigned body must not be able to
        // plant one. Stored with `now_ms` rather than the event's own timestamp
        // — the age that matters is how long *this machine* has held it, and a
        // provider timestamp can be arbitrarily stale on a redelivery.
        for envelope in &envelopes {
            let Some(reply_token) = envelope.metadata.get("line_reply_token") else {
                continue;
            };
            let _ = self.references.put(
                &self.account_id,
                &envelope.conversation.conversation_id,
                &serde_json::json!({
                    "reply_token": reply_token,
                    "issued_at_ms": now_ms,
                }),
            );
        }
        Ok(envelopes)
    }
}

#[async_trait]
impl ChannelAdapter for LineAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Line
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            max_text_chars: MAX_TEXT_CHARS,
            supports_threads: false,
            supports_attachments: false,
            // LINE puts mentions on the message rather than in the text, and
            // `normalize_event` reads them — so mention-only group activation
            // means something here.
            supports_mention_metadata: true,
            // `X-Line-Retry-Key` on the push endpoint, which is the path any
            // retried send takes.
            supports_idempotency_key: true,
            supports_delivery_receipts: false,
            ..ProviderCapabilities::minimal(ChannelKind::Line, InboundTransport::Webhook)
        }
    }

    async fn probe(&self) -> ChannelHealth {
        let now = now_ms();
        let client = match little_monkey_lib::egress::hardened().build() {
            Ok(client) => client,
            Err(error) => {
                return ChannelHealth::error(now, format!("Failed to build client: {error}"))
            }
        };
        let url = format!("{}/v2/bot/info", self.api_base);
        let request = client.get(url).bearer_auth(&self.channel_access_token);
        match little_monkey_lib::egress::send(request).await {
            Ok(response) if response.status().is_success() => {
                ChannelHealth::connected(now, Some("Bot info reachable".to_string()))
            }
            Ok(response) => ChannelHealth::error(
                now,
                format!("LINE probe failed with status {}", response.status()),
            ),
            Err(error) => ChannelHealth::error(now, sanitize_transport_error(&error)),
        }
    }

    async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
        // LINE is delivered to, not polled — see the module doc.
        Ok(InboundBatch::default())
    }

    async fn send(&self, message: &OutboundMessage) -> SendOutcome {
        let client = match little_monkey_lib::egress::hardened().build() {
            Ok(client) => client,
            Err(error) => {
                return SendOutcome::PermanentFailure {
                    error: format!("Failed to build client: {error}"),
                }
            }
        };
        let chunks = split_text(&message.text, MAX_TEXT_CHARS);
        let messages: Vec<JsonValue> = chunks
            .iter()
            .map(|chunk| serde_json::json!({ "type": "text", "text": chunk }))
            .collect();
        // Reply while the token is provably fresh, push otherwise — and the
        // token is spent by the attempt rather than by its success, so nothing
        // downstream can replay it. See the module doc.
        let reply_token = self.claim_reply_token(&message.conversation_id, now_ms());
        let (url, body) = match &reply_token {
            Some(reply_token) => (
                format!("{}/v2/bot/message/reply", self.api_base),
                serde_json::json!({
                    "replyToken": reply_token,
                    "messages": messages,
                }),
            ),
            None => (
                format!("{}/v2/bot/message/push", self.api_base),
                serde_json::json!({
                    "to": message.conversation_id,
                    "messages": messages,
                }),
            ),
        };
        let mut request = client
            .post(url)
            .bearer_auth(&self.channel_access_token)
            .json(&body);
        // LINE's own idempotency: a push carrying a retry key it has already
        // seen is answered rather than delivered again. Only push — a reply
        // token is single-use, so the endpoint that takes one needs no help
        // being idempotent, and LINE does not accept the header there.
        if reply_token.is_none() {
            request = request.header(RETRY_KEY_HEADER, retry_key(&message.idempotency_key));
        }
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => return map_transport_error(&error),
        };
        let status = response.status();
        let retry_after_ms = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i64>().ok())
            .map(|seconds| seconds * 1000);
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(error) => return map_transport_error(&error),
        };

        let parsed: JsonValue = serde_json::from_slice(&bytes).unwrap_or(JsonValue::Null);

        if status.is_success() {
            // Newer Messaging API versions name what they sent; older ones
            // answer `{}`, and there is nothing to invent when they do.
            return SendOutcome::Sent {
                provider_message_id: parsed
                    .get("sentMessages")
                    .and_then(JsonValue::as_array)
                    .and_then(|sent| sent.first())
                    .and_then(|first| first.get("id"))
                    .and_then(JsonValue::as_str)
                    .map(str::to_string),
            };
        }

        let error_message = parsed
            .get("message")
            .and_then(JsonValue::as_str)
            .unwrap_or("LINE send failed")
            .to_string();

        if status.as_u16() == 429 {
            return SendOutcome::RetryableFailure {
                error: error_message,
                retry_after_ms,
            };
        }
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return SendOutcome::PermanentFailure {
                error: error_message,
            };
        }
        if status.is_server_error() {
            return SendOutcome::RetryableFailure {
                error: error_message,
                retry_after_ms,
            };
        }
        SendOutcome::PermanentFailure {
            error: error_message,
        }
    }

    /// LINE keeps a message's media on a separate host: the Messaging API
    /// answers on `api.line.me`, but the bytes come from
    /// `api-data.line.me/v2/bot/message/{id}/content`, authorized with the same
    /// channel access token.
    async fn fetch_attachment(
        &self,
        attachment: &ChannelAttachment,
        limits: crate::daemon::channel_adapter::AttachmentLimits,
    ) -> Result<Vec<u8>, String> {
        let AttachmentSource::ProviderHandle { handle } = &attachment.source else {
            return Err("This LINE attachment has no message id.".to_string());
        };
        // The id is concatenated into a URL and it arrives inside a webhook
        // body, so anything that is not LINE's own id alphabet is refused.
        if handle.is_empty() || !handle.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err("That LINE message id is not usable".to_string());
        }
        crate::daemon::channel_adapter::fetch_url(
            &format!("{}/v2/bot/message/{handle}/content", self.content_base),
            Some(&self.channel_access_token),
            limits.max_bytes,
        )
        .await
    }
}

/// Verifies the base64 HMAC-SHA256 in `X-Line-Signature` with a
/// constant-time comparison (`ring::hmac::verify` is constant-time).
/// LINE's own idempotency header for the push endpoints.
const RETRY_KEY_HEADER: &str = "X-Line-Retry-Key";

/// The outbox's idempotency key, in the UUID shape LINE requires.
///
/// The key itself is an internal id of no fixed format, and LINE rejects a
/// retry key that is not a UUID. Derived by digest rather than randomly so the
/// same queued row produces the same key on every attempt — a fresh one each
/// time would be an idempotency key that idempotates nothing.
fn retry_key(idempotency_key: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, idempotency_key.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest.as_ref()[..16]);
    // Version 4 and the RFC 4122 variant, so the value parses as a UUID
    // wherever LINE checks it.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn verify_line_signature(secret: &str, body: &[u8], header_value: &str) -> Result<(), ()> {
    if header_value.is_empty() {
        return Err(());
    }
    let expected = BASE64.decode(header_value).map_err(|_| ())?;
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    ring::hmac::verify(&key, body, &expected).map_err(|_| ())
}

fn sanitize_transport_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "LINE request timed out".to_string()
    } else if error.is_connect() {
        "Could not connect to the LINE API".to_string()
    } else {
        "LINE request failed".to_string()
    }
}

fn map_transport_error(error: &reqwest::Error) -> SendOutcome {
    if error.is_connect() {
        SendOutcome::RetryableFailure {
            error: sanitize_transport_error(error),
            retry_after_ms: None,
        }
    } else {
        SendOutcome::NeedsReconciliation {
            error: sanitize_transport_error(error),
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Splits `text` into chunks of at most `max_chars` **characters** (not
/// bytes), so a UTF-8 codepoint is never cut in half. LINE's 5000-character
/// cap applies per message, and the push endpoint accepts a `messages` array,
/// so a caller can carry a longer text as several message objects in one
/// request rather than several requests.
///
/// ponytail: caps at LINE's own 5-messages-per-push limit only implicitly —
/// text long enough to need more than 5 chunks (25,000+ characters) will
/// still be split correctly here, but the push call itself would then be
/// rejected by LINE for exceeding its own array-length limit. Upgrade path if
/// that ever matters: fall back to multiple sequential push calls above 5
/// chunks.
fn split_text(text: &str, max_chars: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(max_chars.max(1))
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// Normalizes `events[]` of type `message` into envelopes. Every other event
/// type (follow, unfollow, join, leave, postback, beacon, ...) normalizes to
/// nothing.
fn normalize_payload(
    payload: &JsonValue,
    account_id: &str,
    fallback_received_at_ms: i64,
) -> Vec<ChannelEnvelope> {
    let mut envelopes = Vec::new();
    let Some(events) = payload.get("events").and_then(JsonValue::as_array) else {
        return envelopes;
    };
    for event in events {
        if event.get("type").and_then(JsonValue::as_str) != Some("message") {
            continue;
        }
        if let Some(envelope) = normalize_event(event, account_id, fallback_received_at_ms) {
            envelopes.push(envelope);
        }
    }
    envelopes
}

fn normalize_event(
    event: &JsonValue,
    account_id: &str,
    fallback_received_at_ms: i64,
) -> Option<ChannelEnvelope> {
    let message = event.get("message")?;
    let message_id = message.get("id").and_then(JsonValue::as_str)?;
    let provider_event_id = event
        .get("webhookEventId")
        .and_then(JsonValue::as_str)
        .unwrap_or(message_id)
        .to_string();

    let source = event.get("source")?;
    let source_type = source.get("type").and_then(JsonValue::as_str).unwrap_or("");
    let source_user_id = source.get("userId").and_then(JsonValue::as_str);
    let conversation = match source_type {
        "user" => {
            let user_id = source_user_id?;
            ChannelConversation::direct(user_id)
        }
        "group" => {
            let group_id = source.get("groupId").and_then(JsonValue::as_str)?;
            ChannelConversation::group(group_id)
        }
        "room" => {
            let room_id = source.get("roomId").and_then(JsonValue::as_str)?;
            ChannelConversation::group(room_id)
        }
        _ => return None,
    };

    // `source.userId` is present for user, group and room sources alike
    // (absent only under LINE's privacy-mode groups); fall back to the
    // conversation id so a sender is never empty.
    let sender_id = source_user_id
        .unwrap_or(conversation.conversation_id.as_str())
        .to_string();

    let message_type = message
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    let text = if message_type == "text" {
        message
            .get("text")
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_string()
    } else {
        String::new()
    };
    // A photo, video, audio clip or file is carried by its message id: LINE
    // does not put the bytes in the webhook, and the id is what the content
    // endpoint takes. Without this, a media message normalized to empty text
    // and was dropped by the gate as having no content at all.
    let attachments = match message_type {
        "image" | "video" | "audio" | "file" => {
            let kind = match message_type {
                "image" => AttachmentKind::Image,
                "video" => AttachmentKind::Video,
                "audio" => AttachmentKind::Audio,
                _ => AttachmentKind::Document,
            };
            message
                .get("id")
                .and_then(JsonValue::as_str)
                .map(|id| {
                    vec![ChannelAttachment {
                        provider_id: Some(id.to_string()),
                        kind,
                        filename: message
                            .get("fileName")
                            .and_then(JsonValue::as_str)
                            .map(str::to_string),
                        mime_type: None,
                        declared_size_bytes: message.get("fileSize").and_then(JsonValue::as_u64),
                        source: AttachmentSource::ProviderHandle {
                            handle: id.to_string(),
                        },
                        stored_artifact_id: None,
                        fetch_error: None,
                        text_excerpt: None,
                    }]
                })
                .unwrap_or_default()
        }
        _ => Vec::new(),
    };

    let received_at_ms = event
        .get("timestamp")
        .and_then(JsonValue::as_i64)
        .unwrap_or(fallback_received_at_ms);

    let mut metadata = BoundedMetadata::new();
    // The reply token expires and is never part of the model's text — only
    // the outbound path may read it back out of metadata.
    if let Some(reply_token) = event.get("replyToken").and_then(JsonValue::as_str) {
        metadata.insert("line_reply_token", reply_token);
    }
    if !message_type.is_empty() {
        metadata.insert("line_message_type", message_type);
    }
    // LINE marks a redelivered event rather than hiding it. Dedupe on
    // `webhookEventId` is what actually stops a second run; recording the flag
    // is so an operator reading the activity list can tell a provider retry
    // from a person sending the same thing twice.
    if event
        .get("deliveryContext")
        .and_then(|context| context.get("isRedelivery"))
        .and_then(JsonValue::as_bool)
        .unwrap_or(false)
    {
        metadata.insert("line_redelivery", "true");
    }

    // Whether the bot itself was named. Group activation can be set to
    // mention-only, and without this every LINE group message would look
    // unaddressed and be ignored — LINE puts mentions on the message rather
    // than marking the text.
    let mentionees = message
        .get("mention")
        .and_then(|mention| mention.get("mentionees"))
        .and_then(JsonValue::as_array);
    let mentions_self = mentionees.is_some_and(|mentionees| {
        mentionees.iter().any(|mentionee| {
            // `type: "all"` is an @all, which addresses the bot as much as
            // anyone.
            if matches!(
                mentionee.get("type").and_then(JsonValue::as_str),
                Some("all")
            ) {
                return true;
            }
            // LINE says outright whether a mention is the bot's own. Where it
            // does, that answer is taken and nothing is inferred: a missing
            // `userId` also happens for a member whose profile this bot may not
            // read, and guessing there makes every such mention wake the agent
            // in a group set to mention-only.
            match mentionee.get("isSelf").and_then(JsonValue::as_bool) {
                Some(is_self) => is_self,
                None => mentionee.get("userId").is_none(),
            }
        })
    });
    if let Some(count) = mentionees.map(Vec::len).filter(|count| *count > 0) {
        metadata.insert("line_mentions", count.to_string());
    }

    Some(ChannelEnvelope {
        account_id: account_id.to_string(),
        kind: ChannelKind::Line,
        provider_event_id,
        conversation,
        sender: ChannelSender::new(sender_id),
        text,
        attachments,
        reply_to_provider_id: None,
        mentions_self,
        received_at_ms,
        metadata,
    })
}

#[cfg(test)]
pub(crate) mod tests {

    #[test]
    fn a_photo_message_becomes_an_attachment_rather_than_an_empty_turn() {
        let event = serde_json::json!({
            "type": "message",
            "message": {"id": "466273", "type": "image"},
            "timestamp": 1_700_000_000_000i64,
            "source": {"type": "user", "userId": "U1"},
            "replyToken": "tok"
        });
        let envelope = normalize_event(&event, "acct-1", 0).expect("an envelope");

        assert_eq!(envelope.attachments.len(), 1, "the image is carried");
        match &envelope.attachments[0].source {
            AttachmentSource::ProviderHandle { handle } => assert_eq!(handle, "466273"),
            other => panic!("expected a provider handle, got {other:?}"),
        }
        assert_eq!(envelope.attachments[0].kind, AttachmentKind::Image);
        assert!(envelope.text.is_empty(), "an image message has no text");
    }
    use super::*;
    use crate::daemon::channel_adapter::MemoryConversationReferences;
    use crate::daemon::channel_store::ChannelAccountRecord;
    use little_monkey_lib::channels::policy::ChannelAccessPolicy;
    use little_monkey_lib::channels::types::{ConversationKind, HealthState};
    use std::io::{Read, Write};

    pub(crate) fn test_account() -> ChannelAccountRecord {
        ChannelAccountRecord {
            account_id: "acct-line".to_string(),
            kind: ChannelKind::Line,
            label: "Test LINE".to_string(),
            enabled: true,
            non_secret_config: serde_json::json!({}),
            credential_ref: Some("line-cred".to_string()),
            access_policy: ChannelAccessPolicy::default(),
            health: ChannelHealth {
                state: HealthState::Unconfigured,
                detail: None,
                last_error: None,
                probed_at_ms: 0,
            },
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn adapter(channel_secret: &str, channel_access_token: &str) -> LineAdapter {
        let account = test_account();
        let secret = serde_json::json!({
            "channel_secret": channel_secret,
            "channel_access_token": channel_access_token,
        })
        .to_string();
        let config = AdapterConfig {
            account: &account,
            secret,
        };
        // Never the daemon-backed reference store: a unit test must not reach
        // for the operator's real state database.
        LineAdapter::new(&config)
            .expect("adapter builds")
            .with_references(std::sync::Arc::new(MemoryConversationReferences::default()))
    }

    fn sign(secret: &str, body: &[u8]) -> String {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
        let tag = ring::hmac::sign(&key, body);
        BASE64.encode(tag.as_ref())
    }

    fn user_message_body() -> Vec<u8> {
        serde_json::json!({
            "destination": "dest-1",
            "events": [{
                "type": "message",
                "message": {"id": "msg-1", "type": "text", "text": "hello"},
                "webhookEventId": "evt-1",
                "timestamp": 1700000000000i64,
                "source": {"type": "user", "userId": "U1"},
                "replyToken": "reply-token-1"
            }]
        })
        .to_string()
        .into_bytes()
    }

    fn non_message_event_body() -> Vec<u8> {
        serde_json::json!({
            "destination": "dest-1",
            "events": [{
                "type": "follow",
                "webhookEventId": "evt-2",
                "timestamp": 1700000000000i64,
                "source": {"type": "user", "userId": "U1"}
            }]
        })
        .to_string()
        .into_bytes()
    }

    // --- Signature verification ---------------------------------------------

    #[test]
    fn a_correctly_signed_body_verifies_and_normalizes() {
        let adapter = adapter("channel-secret-value", "token-value");
        let body = user_message_body();
        let signature = sign("channel-secret-value", &body);
        let envelopes = adapter
            .verify_and_normalize(
                &[("x-line-signature".to_string(), signature)],
                &body,
                None,
                0,
            )
            .expect("verifies");
        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[0].provider_event_id, "evt-1");
        assert_eq!(envelopes[0].text, "hello");
        assert_eq!(envelopes[0].conversation.kind, ConversationKind::Direct);
        assert_eq!(
            envelopes[0].metadata.get("line_reply_token"),
            Some("reply-token-1")
        );
    }

    #[test]
    fn a_one_byte_changed_body_fails_verification_and_yields_no_envelopes() {
        let adapter = adapter("channel-secret-value", "token-value");
        let body = user_message_body();
        let signature = sign("channel-secret-value", &body);
        let mut tampered = body.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        let result = adapter.verify_and_normalize(
            &[("x-line-signature".to_string(), signature)],
            &tampered,
            None,
            0,
        );
        assert!(result.is_err());
    }

    #[test]
    fn a_signature_from_the_wrong_secret_fails() {
        let adapter = adapter("channel-secret-value", "token-value");
        let body = user_message_body();
        let signature = sign("wrong-secret", &body);
        let result = adapter.verify_and_normalize(
            &[("x-line-signature".to_string(), signature)],
            &body,
            None,
            0,
        );
        assert!(result.is_err());
    }

    #[test]
    fn a_missing_signature_header_fails() {
        let adapter = adapter("channel-secret-value", "token-value");
        let body = user_message_body();
        let result = adapter.verify_and_normalize(&[], &body, None, 0);
        assert!(result.is_err());
    }

    #[test]
    fn an_empty_signature_header_fails() {
        let adapter = adapter("channel-secret-value", "token-value");
        let body = user_message_body();
        let result = adapter.verify_and_normalize(
            &[("x-line-signature".to_string(), String::new())],
            &body,
            None,
            0,
        );
        assert!(result.is_err());
    }

    // --- Normalization -------------------------------------------------------

    #[test]
    fn a_group_source_normalizes_to_a_group_conversation() {
        let adapter = adapter("channel-secret-value", "token-value");
        let body = serde_json::json!({
            "events": [{
                "type": "message",
                "message": {"id": "msg-2", "type": "text", "text": "hi group"},
                "source": {"type": "group", "groupId": "G1", "userId": "U9"}
            }]
        })
        .to_string()
        .into_bytes();
        let signature = sign("channel-secret-value", &body);
        let envelopes = adapter
            .verify_and_normalize(
                &[("x-line-signature".to_string(), signature)],
                &body,
                None,
                0,
            )
            .expect("verifies");
        assert_eq!(envelopes[0].conversation.kind, ConversationKind::Group);
        assert_eq!(envelopes[0].conversation.conversation_id, "G1");
        assert_eq!(envelopes[0].sender.sender_id, "U9");
    }

    #[test]
    fn a_non_message_event_normalizes_to_no_envelopes() {
        let adapter = adapter("channel-secret-value", "token-value");
        let body = non_message_event_body();
        let signature = sign("channel-secret-value", &body);
        let envelopes = adapter
            .verify_and_normalize(
                &[("x-line-signature".to_string(), signature)],
                &body,
                None,
                0,
            )
            .expect("verifies");
        assert!(envelopes.is_empty());
    }

    #[test]
    fn provider_event_id_falls_back_to_the_message_id_when_no_webhook_event_id() {
        let adapter = adapter("channel-secret-value", "token-value");
        let body = serde_json::json!({
            "events": [{
                "type": "message",
                "message": {"id": "msg-only-3", "type": "text", "text": "hi"},
                "source": {"type": "user", "userId": "U1"}
            }]
        })
        .to_string()
        .into_bytes();
        let signature = sign("channel-secret-value", &body);
        let envelopes = adapter
            .verify_and_normalize(
                &[("x-line-signature".to_string(), signature)],
                &body,
                None,
                0,
            )
            .expect("verifies");
        assert_eq!(envelopes[0].provider_event_id, "msg-only-3");
    }

    #[test]
    fn the_reply_token_never_reaches_the_normalized_text() {
        let adapter = adapter("channel-secret-value", "token-value");
        let body = user_message_body();
        let signature = sign("channel-secret-value", &body);
        let envelopes = adapter
            .verify_and_normalize(
                &[("x-line-signature".to_string(), signature)],
                &body,
                None,
                0,
            )
            .expect("verifies");
        assert!(!envelopes[0].text.contains("reply-token-1"));
    }

    // --- No secret leakage ----------------------------------------------------

    #[test]
    fn no_secret_appears_in_any_rendered_error_string() {
        let adapter = adapter("super-secret-channel-value", "super-secret-token-value");
        let body = user_message_body();
        let bad_signature = BASE64.encode([0u8; 32]);
        let error = adapter
            .verify_and_normalize(
                &[("x-line-signature".to_string(), bad_signature)],
                &body,
                None,
                0,
            )
            .unwrap_err();
        assert!(!error.contains("super-secret-channel-value"));
        assert!(!error.contains("super-secret-token-value"));
    }

    // --- Text splitting --------------------------------------------------

    #[test]
    fn text_over_the_limit_splits_into_multiple_chunks() {
        let text: String = "a".repeat(12_000);
        let chunks = split_text(&text, MAX_TEXT_CHARS);
        assert_eq!(chunks.len(), 3);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.chars().count() <= MAX_TEXT_CHARS));
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn text_under_the_limit_is_a_single_chunk() {
        let chunks = split_text("short text", MAX_TEXT_CHARS);
        assert_eq!(chunks, vec!["short text".to_string()]);
    }

    // --- Outbound mapping -------------------------------------------------

    fn serve_once(status: &str, body: &str) -> String {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut scratch = [0u8; 2048];
                let _ = stream.read(&mut scratch);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn outbound_message() -> OutboundMessage {
        OutboundMessage {
            account_id: "acct-line".to_string(),
            kind: ChannelKind::Line,
            conversation_id: "U1".to_string(),
            thread_id: None,
            text: "hi".to_string(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            idempotency_key: "idem-1".to_string(),
        }
    }

    #[tokio::test]
    async fn a_429_response_maps_to_retryable_failure() {
        let base = serve_once("429 Too Many Requests", r#"{"message":"rate limited"}"#);
        let outcome = adapter("s", "t")
            .with_base_url(&base)
            .send(&outbound_message())
            .await;
        assert!(
            matches!(outcome, SendOutcome::RetryableFailure { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_401_response_maps_to_permanent_failure() {
        let base = serve_once("401 Unauthorized", r#"{"message":"invalid token"}"#);
        let outcome = adapter("s", "t")
            .with_base_url(&base)
            .send(&outbound_message())
            .await;
        assert!(
            matches!(outcome, SendOutcome::PermanentFailure { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_successful_send_reports_sent() {
        let base = serve_once("200 OK", "{}");
        let outcome = adapter("s", "t")
            .with_base_url(&base)
            .send(&outbound_message())
            .await;
        assert!(matches!(outcome, SendOutcome::Sent { .. }), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_provider_named_message_id_is_carried_back() {
        let base = serve_once(
            "200 OK",
            r#"{"sentMessages":[{"id":"461230966842064897"}]}"#,
        );
        let outcome = adapter("s", "t")
            .with_base_url(&base)
            .send(&outbound_message())
            .await;
        match outcome {
            SendOutcome::Sent {
                provider_message_id,
            } => assert_eq!(provider_message_id.as_deref(), Some("461230966842064897")),
            other => panic!("expected Sent, got {other:?}"),
        }
    }

    // --- Reply token lifetime ---------------------------------------------

    /// The body of the one request the fixture received.
    fn sent_body(request: &[u8]) -> JsonValue {
        let text = String::from_utf8_lossy(request);
        let (_, body) = text.split_once("\r\n\r\n").expect("a request with a body");
        serde_json::from_str(body).expect("the request body is JSON")
    }

    fn text_event(reply_token: &str) -> String {
        serde_json::json!({
            "destination": "Ubot",
            "events": [{
                "type": "message",
                "webhookEventId": "01HELLO",
                "replyToken": reply_token,
                "timestamp": 1_700_000_000_000i64,
                "source": {"type": "user", "userId": "U1"},
                "message": {"id": "m1", "type": "text", "text": "hello"}
            }]
        })
        .to_string()
    }

    /// Verify one signed delivery through the production path, so the reply
    /// token is recorded exactly as a real webhook would record it.
    fn deliver(adapter: &LineAdapter, body: &str, now_ms: i64) {
        let signature = sign("s", body.as_bytes());
        adapter
            .verify_and_normalize(
                &[("x-line-signature".to_string(), signature)],
                body.as_bytes(),
                None,
                now_ms,
            )
            .expect("a correctly signed delivery verifies");
    }

    #[tokio::test]
    async fn a_fresh_reply_token_takes_the_reply_endpoint() {
        let references = std::sync::Arc::new(MemoryConversationReferences::default());
        let (base, requests) =
            crate::daemon::channel_adapter::test_http::serve(vec![(200, "{}".to_string())]);
        let adapter = adapter("s", "t")
            .with_base_url(&base)
            .with_references(references.clone());
        deliver(&adapter, &text_event("reply-token-1"), now_ms());

        let outcome = adapter.send(&outbound_message()).await;
        assert!(matches!(outcome, SendOutcome::Sent { .. }), "{outcome:?}");

        let request = requests.recv().expect("the fixture saw a request");
        let text = String::from_utf8_lossy(&request);
        assert!(
            text.starts_with("POST /v2/bot/message/reply"),
            "a fresh token should answer the event it belongs to: {text}"
        );
        assert_eq!(
            sent_body(&request)
                .get("replyToken")
                .and_then(JsonValue::as_str),
            Some("reply-token-1")
        );
    }

    #[tokio::test]
    async fn an_aged_reply_token_is_abandoned_for_push_rather_than_replayed() {
        let references = std::sync::Arc::new(MemoryConversationReferences::default());
        let (base, requests) =
            crate::daemon::channel_adapter::test_http::serve(vec![(200, "{}".to_string())]);
        let adapter = adapter("s", "t")
            .with_base_url(&base)
            .with_references(references.clone());
        // What a long agent turn — or a restart — leaves behind: the token was
        // minted well outside its own usable window.
        references
            .put(
                "acct-line",
                "U1",
                &serde_json::json!({
                    "reply_token": "stale-token",
                    "issued_at_ms": now_ms() - REPLY_TOKEN_USABLE_MS - 1_000,
                }),
            )
            .unwrap();

        let outcome = adapter.send(&outbound_message()).await;
        assert!(
            matches!(outcome, SendOutcome::Sent { .. }),
            "an expired token must never cost the sender their answer: {outcome:?}"
        );

        let request = requests.recv().expect("the fixture saw a request");
        let text = String::from_utf8_lossy(&request);
        assert!(
            text.starts_with("POST /v2/bot/message/push"),
            "an aged token must fall back to push: {text}"
        );
        assert!(
            !text.contains("stale-token"),
            "an expired reply token must not be sent at all: {text}"
        );
        // A push is the path a retried outbox row takes, so it is the one that
        // needs LINE's own idempotency to stop a duplicate landing.
        assert!(
            text.to_ascii_lowercase().contains("x-line-retry-key:"),
            "a push must carry a retry key: {text}"
        );
    }

    #[test]
    fn a_retry_key_is_a_uuid_and_is_the_same_one_on_every_attempt() {
        let key = retry_key("outbox-row-7");
        assert_eq!(
            key,
            retry_key("outbox-row-7"),
            "a key that changed per attempt would deduplicate nothing"
        );
        assert_ne!(key, retry_key("outbox-row-8"));
        // LINE refuses a retry key that is not a UUID.
        let groups: Vec<usize> = key.split('-').map(str::len).collect();
        assert_eq!(groups, vec![8, 4, 4, 4, 12], "{key}");
        assert!(key.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert_eq!(key.as_bytes()[14], b'4', "version 4: {key}");
        assert!(
            matches!(key.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
            "{key}"
        );
    }

    #[tokio::test]
    async fn a_reply_token_is_spent_by_the_attempt_so_a_second_send_pushes() {
        let references = std::sync::Arc::new(MemoryConversationReferences::default());
        let (base, requests) = crate::daemon::channel_adapter::test_http::serve(vec![
            (200, "{}".to_string()),
            (200, "{}".to_string()),
        ]);
        let adapter = adapter("s", "t")
            .with_base_url(&base)
            .with_references(references.clone());
        deliver(&adapter, &text_event("reply-token-1"), now_ms());

        adapter.send(&outbound_message()).await;
        adapter.send(&outbound_message()).await;

        let first = requests.recv().expect("first request");
        let second = requests.recv().expect("second request");
        assert!(String::from_utf8_lossy(&first).starts_with("POST /v2/bot/message/reply"));
        // LINE retires a reply token on first use. A second send — a retry, or
        // simply a second message in the same turn — must not replay it.
        assert!(
            String::from_utf8_lossy(&second).starts_with("POST /v2/bot/message/push"),
            "a spent token was replayed: {}",
            String::from_utf8_lossy(&second)
        );
    }

    #[test]
    fn an_unsigned_body_cannot_plant_a_reply_token() {
        let references = std::sync::Arc::new(MemoryConversationReferences::default());
        let adapter = adapter("s", "t").with_references(references.clone());
        let body = text_event("attacker-token");
        adapter
            .verify_and_normalize(
                &[(
                    "x-line-signature".to_string(),
                    sign("wrong-secret", body.as_bytes()),
                )],
                body.as_bytes(),
                None,
                now_ms(),
            )
            .expect_err("a bad signature is refused");
        assert!(
            references.get("acct-line", "U1").is_none(),
            "an unverified body must leave no outbound state behind"
        );
    }

    // --- Mentions ---------------------------------------------------------

    #[test]
    fn a_mention_of_the_bot_is_what_makes_a_group_message_addressed() {
        // LINE marks a bot's own mention by leaving out the user id, and marks
        // an @all with a type. Both address the bot; a mention of some other
        // member does not.
        let event = serde_json::json!({
            "type": "message",
            "webhookEventId": "01GROUP",
            "timestamp": 1_700_000_000_000i64,
            "source": {"type": "group", "groupId": "G1", "userId": "U2"},
            "message": {
                "id": "m2", "type": "text", "text": "@monkey look",
                "mention": {"mentionees": [{"index": 0, "length": 7}]}
            }
        });
        let envelope = normalize_event(&event, "acct-line", 0).expect("normalizes");
        assert!(envelope.mentions_self);
        assert_eq!(envelope.metadata.get("line_mentions"), Some("1"));

        let other_member = serde_json::json!({
            "type": "message",
            "webhookEventId": "01GROUP2",
            "timestamp": 1_700_000_000_000i64,
            "source": {"type": "group", "groupId": "G1", "userId": "U2"},
            "message": {
                "id": "m3", "type": "text", "text": "@ada look",
                "mention": {"mentionees": [{"index": 0, "length": 4, "userId": "U9"}]}
            }
        });
        let envelope = normalize_event(&other_member, "acct-line", 0).expect("normalizes");
        assert!(
            !envelope.mentions_self,
            "naming another member does not address the bot"
        );

        // A member whose profile this bot may not read is delivered without a
        // user id too. LINE says who the mention is for, and that answer beats
        // the shape of the payload — otherwise every such mention would wake
        // the agent in a group set to mention-only.
        let unreadable_member = serde_json::json!({
            "type": "message",
            "webhookEventId": "01GROUP3",
            "timestamp": 1_700_000_000_000i64,
            "source": {"type": "group", "groupId": "G1", "userId": "U2"},
            "message": {
                "id": "m4", "type": "text", "text": "@ada look",
                "mention": {"mentionees": [
                    {"index": 0, "length": 4, "type": "user", "isSelf": false}
                ]}
            }
        });
        let envelope = normalize_event(&unreadable_member, "acct-line", 0).expect("normalizes");
        assert!(!envelope.mentions_self);
    }

    #[test]
    fn a_redelivered_event_is_marked_and_keeps_its_dedupe_identity() {
        let event = serde_json::json!({
            "type": "message",
            "webhookEventId": "01SAME",
            "deliveryContext": {"isRedelivery": true},
            "timestamp": 1_700_000_000_000i64,
            "source": {"type": "user", "userId": "U1"},
            "message": {"id": "m1", "type": "text", "text": "hello"}
        });
        let envelope = normalize_event(&event, "acct-line", 0).expect("normalizes");
        assert_eq!(envelope.metadata.get("line_redelivery"), Some("true"));
        // The flag is only for the operator's activity list. What actually
        // stops a second run is that the id is unchanged, so the durable event
        // log collapses it onto the row already there.
        assert_eq!(envelope.provider_event_id, "01SAME");
    }
}
