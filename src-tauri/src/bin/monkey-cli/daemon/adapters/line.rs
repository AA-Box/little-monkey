//! LINE Messaging API adapter (official webhook integration only).
//!
//! Inbound is a webhook: LINE signs the raw POST body with the channel secret
//! and sends the base64 digest in `X-Line-Signature`. As with WhatsApp, the
//! signature covers only the body — there is no separate signed timestamp
//! header to enforce a skew window against — so redelivery safety comes from
//! `provider_event_id` dedupe (`webhookEventId`, or the message id as a
//! fallback for older payloads that predate it).
//!
//! # Outbound, and why the reply token is not used at all
//!
//! LINE offers two send paths. The reply endpoint answers a specific inbound
//! event using the `replyToken` that event carried; it is free of the push
//! quota, but the token is **single use, short lived, and scoped to one
//! event**. The push endpoint addresses the conversation itself and always
//! works.
//!
//! Nothing here uses the reply token. Every answer this adapter sends is a run
//! that was queued after the delivery was acknowledged, ran for however long it
//! ran, and may well have outlived the process that received the message — so
//! the token would be dead by the time there is anything to say, and a token
//! stored per conversation would be overwritten anyway by the next of several
//! events arriving in the same delivery. Keying it per event to fix that would
//! buy a saved push at the cost of durable state that is only ever valid for
//! seconds and is a credential to answer as this bot.
//!
//! So the token is never stored, never durable, and never consulted:
//! [`ChannelAdapter::send`] always POSTs `/v2/bot/message/push` to the
//! normalized destination, with `X-Line-Retry-Key` derived from the outbox
//! row's own idempotency key so a retried row can never become two messages.
//! Restart recovery therefore depends on nothing that expires.

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
    AdapterConfig, ChannelAdapter, InboundBatch, WebhookAck, WebhookChannelAdapter,
};

const LINE_API_BASE: &str = "https://api.line.me";
/// LINE serves message content (images, video, audio, files) from its own
/// data host rather than the API host.
const LINE_CONTENT_BASE: &str = "https://api-data.line.me";
/// LINE's own per-message character cap.
const MAX_TEXT_CHARS: usize = 5000;

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
        })
    }

    #[cfg(test)]
    pub(crate) fn with_base_url(mut self, base: &str) -> Self {
        self.api_base = base.to_string();
        self
    }
}

impl WebhookChannelAdapter for LineAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Line
    }

    /// LINE's own requirement, and the one its console's Verify button checks:
    /// status 200. It reads nothing else, including on a redelivery of an event
    /// this daemon already has.
    fn ack(&self) -> WebhookAck {
        WebhookAck::empty_ok()
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
        // Nothing is written here. A LINE delivery carries one reply token per
        // event and this adapter answers with push, so there is no per-event
        // state to keep and nothing a second event in the same delivery could
        // overwrite — see the module doc.
        Ok(normalize_payload(&payload, &self.account_id, now_ms))
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
        // Always push, to the normalized destination — see the module doc for
        // why no reply token is consulted, stored or hoped for.
        let body = serde_json::json!({
            "to": message.conversation_id,
            "messages": messages,
        });
        // LINE's own idempotency: a push carrying a retry key it has already
        // accepted is refused as a duplicate rather than delivered twice, which
        // is what makes the outbox safe to retry.
        let request = client
            .post(format!("{}/v2/bot/message/push", self.api_base))
            .bearer_auth(&self.channel_access_token)
            .header(RETRY_KEY_HEADER, retry_key(&message.idempotency_key))
            .json(&body);
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

        // The retry key did its job: LINE has already accepted this exact
        // outbox row and refuses to deliver it a second time. That is a
        // delivered message, not a failure — treating it as one would either
        // retry forever or park a row the recipient can already read.
        if status.as_u16() == 409 {
            return SendOutcome::Sent {
                provider_message_id: None,
            };
        }
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
    // `replyToken` is deliberately dropped rather than carried: nothing sends
    // with it, and a short-lived credential to answer as this bot has no
    // business in an envelope that is written to disk and shown to an operator.
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
        // LINE marks a quoted message with the id of the message being quoted,
        // which is the same fact every other provider calls a reply. Without
        // this a LINE reply looked unrelated to what it answered, and the
        // reply-depth chain that bounds automated loops saw nothing at all.
        reply_to_provider_id: message
            .get("quotedMessageId")
            .and_then(JsonValue::as_str)
            .map(str::to_string),
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
        LineAdapter::new(&config).expect("adapter builds")
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
            None,
            "a reply token is not carried anywhere: nothing sends with one"
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

    /// Every media type LINE delivers by message id, and the two grouped
    /// sources. A type that normalized to empty text with no attachment used
    /// to be dropped by the access gate as a message with no content at all.
    #[test]
    fn every_media_type_and_grouped_source_normalizes() {
        for (message_type, expected) in [
            ("image", AttachmentKind::Image),
            ("video", AttachmentKind::Video),
            ("audio", AttachmentKind::Audio),
            ("file", AttachmentKind::Document),
        ] {
            let event = serde_json::json!({
                "type": "message",
                "message": {"id": "466273", "type": message_type, "fileName": "notes.pdf"},
                "timestamp": 1_700_000_000_000i64,
                "source": {"type": "user", "userId": "U1"},
            });
            let envelope = normalize_event(&event, "acct-1", 0)
                .unwrap_or_else(|| panic!("{message_type} normalizes"));
            assert_eq!(envelope.attachments.len(), 1, "{message_type}");
            assert_eq!(envelope.attachments[0].kind, expected, "{message_type}");
            assert_eq!(
                envelope.metadata.get("line_message_type"),
                Some(message_type)
            );
        }

        // A room is LINE's other multi-person source, and it addresses the
        // room rather than whoever spoke.
        let room = serde_json::json!({
            "type": "message",
            "message": {"id": "m9", "type": "text", "text": "hi room"},
            "timestamp": 1_700_000_000_000i64,
            "source": {"type": "room", "roomId": "R1", "userId": "U9"},
        });
        let envelope = normalize_event(&room, "acct-1", 0).expect("a room normalizes");
        assert_eq!(envelope.conversation.kind, ConversationKind::Group);
        assert_eq!(envelope.conversation.conversation_id, "R1");
        assert_eq!(envelope.sender.sender_id, "U9");
    }

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
    fn the_reply_token_reaches_nothing_at_all() {
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
        // Not the text, not the metadata, not the durable envelope: the
        // token is dropped at normalization and nothing downstream can find
        // one to use, refresh or leak.
        assert!(!serde_json::to_string(&envelopes[0])
            .unwrap()
            .contains("reply-token-1"));
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
    async fn every_send_is_a_push_carrying_a_retry_key() {
        let (base, requests) =
            crate::daemon::channel_adapter::test_http::serve(vec![(200, "{}".to_string())]);
        let adapter = adapter("s", "t").with_base_url(&base);
        // A delivery carrying a perfectly fresh reply token, moments ago.
        deliver(&adapter, &text_event("reply-token-1"), now_ms());

        let outcome = adapter.send(&outbound_message()).await;
        assert!(matches!(outcome, SendOutcome::Sent { .. }), "{outcome:?}");

        let request = requests.recv().expect("the fixture saw a request");
        let text = String::from_utf8_lossy(&request);
        assert!(
            text.starts_with("POST /v2/bot/message/push"),
            "an answer must not depend on a token that expires: {text}"
        );
        assert!(
            !text.contains("reply-token-1"),
            "a reply token must never leave this process: {text}"
        );
        assert!(
            text.to_ascii_lowercase().contains("x-line-retry-key:"),
            "a push must carry the retry key that makes it idempotent: {text}"
        );
        assert_eq!(
            sent_body(&request).get("to").and_then(JsonValue::as_str),
            Some("U1"),
            "the normalized destination, not an event-scoped token"
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

    /// A redelivery is where a per-conversation reply token used to be
    /// refreshed by the local clock and look usable long after LINE retired
    /// it. Nothing stores one now, so there is nothing to refresh — and the
    /// send is the same push either way.
    #[tokio::test]
    async fn a_redelivered_event_leaves_nothing_that_could_look_fresh() {
        let (base, requests) = crate::daemon::channel_adapter::test_http::serve(vec![
            (200, "{}".to_string()),
            (200, "{}".to_string()),
        ]);
        let adapter = adapter("s", "t").with_base_url(&base);
        // The same event delivered twice, ten minutes apart, exactly as LINE
        // redelivers one it never got a 200 for.
        deliver(&adapter, &text_event("reply-token-1"), now_ms());
        deliver(&adapter, &text_event("reply-token-1"), now_ms() + 600_000);

        adapter.send(&outbound_message()).await;
        adapter.send(&outbound_message()).await;

        for _ in 0..2 {
            let request = requests.recv().expect("a request");
            let text = String::from_utf8_lossy(&request);
            assert!(
                text.starts_with("POST /v2/bot/message/push"),
                "a redelivery must not resurrect a reply path: {text}"
            );
            assert!(!text.contains("reply-token-1"), "{text}");
        }
    }

    /// Several events in one delivery, from the same conversation. The old
    /// per-conversation token would have had the second overwrite the first;
    /// there is no per-event state left to lose, and both normalize whole.
    #[test]
    fn two_events_from_one_conversation_in_one_delivery_both_survive() {
        let body = serde_json::json!({
            "destination": "Ubot",
            "events": [
                {
                    "type": "message",
                    "webhookEventId": "01FIRST",
                    "replyToken": "token-first",
                    "timestamp": 1_700_000_000_000i64,
                    "source": {"type": "user", "userId": "U1"},
                    "message": {"id": "m1", "type": "text", "text": "first"}
                },
                {
                    "type": "message",
                    "webhookEventId": "01SECOND",
                    "replyToken": "token-second",
                    "timestamp": 1_700_000_000_500i64,
                    "source": {"type": "user", "userId": "U1"},
                    "message": {"id": "m2", "type": "text", "text": "second"}
                }
            ]
        })
        .to_string();
        let adapter = adapter("s", "t");
        let envelopes = adapter
            .verify_and_normalize(
                &[("x-line-signature".to_string(), sign("s", body.as_bytes()))],
                body.as_bytes(),
                None,
                now_ms(),
            )
            .expect("verifies");

        assert_eq!(envelopes.len(), 2);
        assert_eq!(envelopes[0].provider_event_id, "01FIRST");
        assert_eq!(envelopes[1].provider_event_id, "01SECOND");
        assert_eq!(envelopes[0].text, "first");
        assert_eq!(envelopes[1].text, "second");
        for envelope in &envelopes {
            assert!(
                serde_json::to_string(envelope)
                    .unwrap()
                    .find("token-")
                    .is_none(),
                "a reply token reached the durable envelope: {envelope:?}"
            );
        }
    }

    /// LINE's own answer to a push whose retry key it has already accepted.
    /// The message is with the recipient; treating the refusal as a failure
    /// would either retry forever or park a row that was delivered.
    #[tokio::test]
    async fn a_duplicate_retry_key_response_counts_as_delivered() {
        let base = serve_once(
            "409 Conflict",
            r#"{"message":"The retry key is already accepted"}"#,
        );
        let outcome = adapter("s", "t")
            .with_base_url(&base)
            .send(&outbound_message())
            .await;
        assert!(
            matches!(outcome, SendOutcome::Sent { .. }),
            "a retry-key conflict is an accepted delivery: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_refused_connection_is_retryable_and_a_hangup_needs_reconciliation() {
        let refused = crate::daemon::channel_adapter::test_http::refused();
        let outcome = adapter("s", "t")
            .with_base_url(&refused)
            .send(&outbound_message())
            .await;
        assert!(
            matches!(outcome, SendOutcome::RetryableFailure { .. }),
            "a connection that was never established proves the push never left: {outcome:?}"
        );

        let hangup = crate::daemon::channel_adapter::test_http::accept_then_hangup();
        let outcome = adapter("s", "t")
            .with_base_url(&hangup)
            .send(&outbound_message())
            .await;
        assert!(
            matches!(outcome, SendOutcome::NeedsReconciliation { .. }),
            "a push that may have landed must never be repeated blindly: {outcome:?}"
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
