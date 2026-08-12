//! Telegram Bot API adapter: long polling over plain HTTPS.
//!
//! Every request goes through `little_monkey_lib::egress::hardened()` /
//! `egress::send`, which is this tree's one hardened `reqwest` entry point —
//! see `egress.rs` for what that buys (connect/read timeouts, a same-origin
//! redirect policy, DNS pinning).
//!
//! # The token never appears in a diagnostic
//!
//! The bot token is the whole authorization scheme for this provider: it is
//! baked into every request URL (`.../bot<token>/<method>`), so a naive
//! `format!("... {error}")` around a `reqwest::Error` — whose `Display` prints
//! the request URL — would leak it straight into `ChannelHealth.last_error`, a
//! log line, or a poll-loop error string. [`TelegramAdapter::redact`] is
//! applied to every string built from a request/response before it leaves
//! this module, so the token is scrubbed even when the error text did not
//! obviously carry it.

use std::sync::Mutex;

use async_trait::async_trait;
use serde::Deserialize;

use crate::daemon::channel_adapter::{AdapterConfig, ChannelAdapter, InboundBatch};
use little_monkey_lib::channels::types::{
    AttachmentKind, AttachmentSource, ChannelAttachment, ChannelConversation, ChannelEnvelope,
    ChannelHealth, ChannelKind, ChannelSender, ConversationKind, InboundTransport, OutboundMessage,
    ProviderCapabilities, SendOutcome,
};

const API_BASE: &str = "https://api.telegram.org";

/// Telegram's hard cap on one message: 4096 UTF-16 code units. Used both as
/// the split boundary in `send` and as `ProviderCapabilities::max_text_chars`
/// — Telegram counts UTF-16 units, not Rust `chars`, but this is the same
/// number the provider itself enforces and the two only diverge for text
/// outside the Basic Multilingual Plane, which is rare enough that a caller
/// sizing a message against this constant will not be surprised.
const MAX_MESSAGE_UTF16: usize = 4096;

pub struct TelegramAdapter {
    token: String,
    /// Cached from the first successful `getMe`. `None` until then, which is
    /// what makes `is_self`/`mentions_self` correctly conservative (`false`)
    /// before the adapter has ever confirmed its own identity — see the
    /// `ChannelSender::is_self` doc: an adapter that cannot tell must say
    /// `false` and let the ingress gate handle it.
    self_id: Mutex<Option<i64>>,
    self_username: Mutex<Option<String>>,
}

impl TelegramAdapter {
    pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
        if config.secret.trim().is_empty() {
            return Err("Telegram requires a bot token".to_string());
        }
        Ok(Self {
            token: config.secret.clone(),
            self_id: Mutex::new(None),
            self_username: Mutex::new(None),
        })
    }

    /// `https://api.telegram.org/bot<token>/<method>`. Never logged and never
    /// handed to a diagnostic unredacted — see [`Self::redact`].
    fn method_url(&self, method: &str) -> String {
        format!("{API_BASE}/bot{}/{method}", self.token)
    }

    /// Scrubs the bot token out of any string before it becomes a
    /// `ChannelHealth.last_error`, a `poll`/`send` error, or a log line. Every
    /// diagnostic this module builds from a `reqwest::Error` or an HTTP body
    /// passes through here, because `reqwest::Error`'s own `Display` prints
    /// the request URL — which contains the token — and a caller downstream
    /// has no way to know that without reading this module.
    fn redact(&self, message: impl Into<String>) -> String {
        let message = message.into();
        if self.token.is_empty() {
            message
        } else {
            message.replace(self.token.as_str(), "<redacted>")
        }
    }

    fn client(&self) -> Result<reqwest::Client, String> {
        little_monkey_lib::egress::hardened()
            .build()
            .map_err(|error| self.redact(error.to_string()))
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[async_trait]
impl ChannelAdapter for TelegramAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Telegram
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            kind: ChannelKind::Telegram,
            inbound_transport: InboundTransport::LongPoll,
            max_text_chars: MAX_MESSAGE_UTF16,
            supports_threads: true,
            supports_attachments: true,
            supports_mention_metadata: true,
            // Telegram's Bot API has no caller-supplied idempotency key for
            // sendMessage, and no delivery/read receipts are polled here.
            supports_idempotency_key: false,
            supports_delivery_receipts: false,
        }
    }

    async fn probe(&self) -> ChannelHealth {
        let now = now_ms();
        let client = match self.client() {
            Ok(client) => client,
            Err(error) => return ChannelHealth::error(now, error),
        };
        let request = client.get(self.method_url("getMe"));
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => {
                return ChannelHealth::error(
                    now,
                    self.redact(format!("Could not reach Telegram: {error}")),
                )
            }
        };
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status.as_u16() == 401 {
            return ChannelHealth::error(
                now,
                "The bot token was rejected by Telegram (401); create a new token with @BotFather and paste it again",
            );
        }
        if !status.is_success() {
            return ChannelHealth::error(
                now,
                self.redact(format!("Telegram returned {status} for getMe")),
            );
        }
        match serde_json::from_str::<TelegramApiResponse<TelegramUser>>(&body) {
            Ok(parsed) if parsed.ok => match parsed.result {
                Some(user) => {
                    *self.self_id.lock().unwrap() = Some(user.id);
                    if let Some(username) = &user.username {
                        *self.self_username.lock().unwrap() = Some(username.clone());
                    }
                    let detail = user.username.unwrap_or(user.first_name);
                    ChannelHealth::connected(now, Some(detail))
                }
                None => ChannelHealth::error(now, "Telegram returned no bot identity for getMe"),
            },
            _ => ChannelHealth::error(
                now,
                self.redact("Telegram returned an unexpected getMe response".to_string()),
            ),
        }
    }

    async fn poll(&self, cursor: Option<&str>) -> Result<InboundBatch, String> {
        let offset = cursor.and_then(|value| value.parse::<i64>().ok());
        let client = self.client()?;
        let mut query = vec![
            ("timeout".to_string(), "25".to_string()),
            (
                "allowed_updates".to_string(),
                r#"["message","edited_message"]"#.to_string(),
            ),
        ];
        if let Some(offset) = offset {
            query.push(("offset".to_string(), (offset + 1).to_string()));
        }
        let request = client.get(self.method_url("getUpdates")).query(&query);
        let response = little_monkey_lib::egress::send(request)
            .await
            .map_err(|error| self.redact(format!("Telegram poll failed: {error}")))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| self.redact(format!("Telegram poll body failed: {error}")))?;
        if !status.is_success() {
            return Err(self.redact(format!("Telegram returned {status} for getUpdates")));
        }
        let parsed: TelegramApiResponse<Vec<TelegramUpdate>> = serde_json::from_str(&body)
            .map_err(|error| self.redact(format!("Telegram getUpdates parse failed: {error}")))?;
        if !parsed.ok {
            return Err(self.redact(format!(
                "Telegram getUpdates failed: {}",
                parsed.description.unwrap_or_default()
            )));
        }
        let updates = parsed.result.unwrap_or_default();
        let new_cursor = next_cursor(&updates, cursor);
        let self_id = *self.self_id.lock().unwrap();
        let self_username = self.self_username.lock().unwrap().clone();
        let envelopes = updates
            .iter()
            .filter_map(|update| normalize_update(update, self_id, self_username.as_deref()))
            .collect();
        Ok(InboundBatch {
            envelopes,
            cursor: new_cursor,
        })
    }

    async fn send(&self, message: &OutboundMessage) -> SendOutcome {
        let mut chunks = split_utf16_chunks(&message.text, MAX_MESSAGE_UTF16);
        if chunks.is_empty() {
            chunks.push(String::new());
        }
        let client = match self.client() {
            Ok(client) => client,
            Err(error) => return SendOutcome::PermanentFailure { error },
        };
        let mut last_message_id: Option<String> = None;
        for chunk in &chunks {
            let mut body = serde_json::json!({
                "chat_id": message.conversation_id,
                "text": chunk,
            });
            if let Some(thread_id) = message
                .thread_id
                .as_deref()
                .and_then(|value| value.parse::<i64>().ok())
            {
                body["message_thread_id"] = serde_json::Value::from(thread_id);
            }
            if let Some(reply_to) = message
                .reply_to_provider_id
                .as_deref()
                .and_then(|value| value.parse::<i64>().ok())
            {
                body["reply_to_message_id"] = serde_json::Value::from(reply_to);
            }
            let request = client.post(self.method_url("sendMessage")).json(&body);
            let response = match little_monkey_lib::egress::send(request).await {
                Ok(response) => response,
                Err(error) => {
                    return if error.is_connect() {
                        // The TCP/TLS handshake itself failed: the request
                        // provably never left this machine.
                        SendOutcome::RetryableFailure {
                            error: self.redact(format!("Could not connect to Telegram: {error}")),
                            retry_after_ms: None,
                        }
                    } else {
                        // Anything else (a stalled read, a reset mid-response)
                        // may have happened after the request was already
                        // written, so whether Telegram received it is unknown.
                        SendOutcome::NeedsReconciliation {
                            error: self.redact(format!("Telegram send outcome unknown: {error}")),
                        }
                    };
                }
            };
            let status = response.status();
            let body_text = response.text().await.unwrap_or_default();
            if status.as_u16() == 429 {
                let retry_after_ms = serde_json::from_str::<TelegramErrorResponse>(&body_text)
                    .ok()
                    .and_then(|error| error.parameters)
                    .and_then(|parameters| parameters.retry_after)
                    .map(|seconds| seconds * 1000);
                return SendOutcome::RetryableFailure {
                    error: "Telegram rate-limited the request (429)".to_string(),
                    retry_after_ms,
                };
            }
            if !status.is_success() {
                return SendOutcome::PermanentFailure {
                    error: self.redact(format!("Telegram returned {status} for sendMessage")),
                };
            }
            match serde_json::from_str::<TelegramApiResponse<TelegramMessage>>(&body_text) {
                Ok(parsed) if parsed.ok => {
                    last_message_id = parsed.result.map(|message| message.message_id.to_string());
                }
                _ => {
                    return SendOutcome::NeedsReconciliation {
                        error: "Telegram accepted sendMessage but returned an unparseable response"
                            .to_string(),
                    };
                }
            }
        }
        SendOutcome::Sent {
            provider_message_id: last_message_id,
        }
    }

    /// Telegram hands out a `file_id`, not a URL: `getFile` exchanges it for a
    /// path valid for about an hour, and the bytes live under a different host
    /// prefix (`/file/bot<token>/`) than the API methods do.
    async fn fetch_attachment(
        &self,
        attachment: &ChannelAttachment,
        max_bytes: u64,
    ) -> Result<Vec<u8>, String> {
        let AttachmentSource::ProviderHandle { handle } = &attachment.source else {
            return Err("This Telegram attachment has no file id.".to_string());
        };
        let client = self.client()?;
        let response = little_monkey_lib::egress::send(
            client
                .get(self.method_url("getFile"))
                .query(&[("file_id", handle.as_str())]),
        )
        .await
        .map_err(|error| self.redact(format!("Telegram getFile failed: {error}")))?;
        let body = response
            .text()
            .await
            .map_err(|error| self.redact(format!("Telegram getFile failed: {error}")))?;
        let parsed = serde_json::from_str::<TelegramApiResponse<TelegramFile>>(&body)
            .map_err(|_| "Telegram returned an unparseable getFile response".to_string())?;
        let file_path = parsed
            .result
            .filter(|_| parsed.ok)
            .and_then(|file| file.file_path)
            .ok_or_else(|| "Telegram did not return a path for that file".to_string())?;
        if !usable_file_path(&file_path) {
            return Err("Telegram returned an unusable file path".to_string());
        }
        let url = format!("{API_BASE}/file/bot{}/{file_path}", self.token);
        crate::daemon::channel_adapter::download_bounded(client.get(url), max_bytes)
            .await
            .map_err(|error| self.redact(error))
    }
}

/// Whether a `getFile` path may be concatenated onto the download endpoint.
///
/// The path is Telegram's own answer, but it is still string-joined into a URL,
/// so anything that could climb out of `/file/bot<token>/` — or start a new
/// authority — is refused rather than normalized.
fn usable_file_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains("..")
        && !path.starts_with('/')
        && !path.starts_with('\\')
        && !path.contains("://")
        && !path.contains('?')
        && !path.contains('#')
}

/// `getFile`'s result. Only the path matters here — the size Telegram reports
/// is advisory, and the byte cap is enforced against what actually arrives.
#[derive(Debug, serde::Deserialize)]
struct TelegramFile {
    file_path: Option<String>,
}

/// The new cursor to persist: the highest `update_id` seen in `updates`, or
/// the previous cursor unchanged when the batch was empty. `InboundBatch`'s
/// own contract is that `None` leaves the stored cursor alone, so an empty
/// poll must not overwrite a real cursor with nothing.
fn next_cursor(updates: &[TelegramUpdate], previous: Option<&str>) -> Option<String> {
    let highest = updates.iter().map(|update| update.update_id).max();
    match highest {
        Some(id) => Some(id.to_string()),
        None => previous.map(str::to_string),
    }
}

fn normalize_update(
    update: &TelegramUpdate,
    self_id: Option<i64>,
    self_username: Option<&str>,
) -> Option<ChannelEnvelope> {
    let (message, update_kind) = match (&update.message, &update.edited_message) {
        (Some(message), _) => (message, "message"),
        (None, Some(message)) => (message, "edited_message"),
        (None, None) => return None,
    };

    let conversation_kind = match message.chat.kind.as_str() {
        "private" => ConversationKind::Direct,
        _ => ConversationKind::Group,
    };
    let conversation = ChannelConversation {
        conversation_id: message.chat.id.to_string(),
        kind: conversation_kind,
        thread_id: None,
        title: None,
    }
    .with_thread(message.message_thread_id.map(|id| id.to_string()))
    .with_title(message.chat.title.clone());

    let sender = match &message.from {
        Some(user) => ChannelSender {
            sender_id: user.id.to_string(),
            display_label: user
                .username
                .clone()
                .or_else(|| Some(user.first_name.clone())),
            is_self: self_id == Some(user.id),
            is_bot: user.is_bot,
        },
        // Channel posts and some anonymous-admin messages carry no `from`.
        // The chat id is the least-wrong stable identifier available.
        None => ChannelSender::new(message.chat.id.to_string()),
    };

    let text = message
        .text
        .clone()
        .or_else(|| message.caption.clone())
        .unwrap_or_default();

    let entities = message
        .entities
        .as_deref()
        .or(message.caption_entities.as_deref())
        .unwrap_or(&[]);
    let mentions_self = self_username
        .map(|username| entity_mentions(&text, entities, username))
        .unwrap_or(false);

    let mut attachments = Vec::new();
    if let Some(sizes) = &message.photo {
        if let Some(largest) = sizes.iter().max_by_key(|size| size.width * size.height) {
            attachments.push(ChannelAttachment {
                provider_id: Some(largest.file_id.clone()),
                kind: AttachmentKind::Image,
                filename: None,
                mime_type: None,
                declared_size_bytes: largest.file_size.map(|size| size as u64),
                source: AttachmentSource::ProviderHandle {
                    handle: largest.file_id.clone(),
                },
            });
        }
    }
    if let Some(document) = &message.document {
        attachments.push(ChannelAttachment {
            provider_id: Some(document.file_id.clone()),
            kind: AttachmentKind::Document,
            filename: document.file_name.clone(),
            mime_type: document.mime_type.clone(),
            declared_size_bytes: document.file_size.map(|size| size as u64),
            source: AttachmentSource::ProviderHandle {
                handle: document.file_id.clone(),
            },
        });
    }
    if let Some(voice) = &message.voice {
        attachments.push(ChannelAttachment {
            provider_id: Some(voice.file_id.clone()),
            kind: AttachmentKind::Audio,
            filename: None,
            mime_type: voice.mime_type.clone(),
            declared_size_bytes: voice.file_size.map(|size| size as u64),
            source: AttachmentSource::ProviderHandle {
                handle: voice.file_id.clone(),
            },
        });
    }
    if let Some(audio) = &message.audio {
        attachments.push(ChannelAttachment {
            provider_id: Some(audio.file_id.clone()),
            kind: AttachmentKind::Audio,
            filename: audio.file_name.clone(),
            mime_type: audio.mime_type.clone(),
            declared_size_bytes: audio.file_size.map(|size| size as u64),
            source: AttachmentSource::ProviderHandle {
                handle: audio.file_id.clone(),
            },
        });
    }
    if let Some(video) = &message.video {
        attachments.push(ChannelAttachment {
            provider_id: Some(video.file_id.clone()),
            kind: AttachmentKind::Video,
            filename: None,
            mime_type: video.mime_type.clone(),
            declared_size_bytes: video.file_size.map(|size| size as u64),
            source: AttachmentSource::ProviderHandle {
                handle: video.file_id.clone(),
            },
        });
    }

    let mut metadata = little_monkey_lib::channels::types::BoundedMetadata::new();
    metadata.insert("chat_type", message.chat.kind.clone());
    metadata.insert("update_kind", update_kind);

    Some(ChannelEnvelope {
        account_id: String::new(),
        kind: ChannelKind::Telegram,
        provider_event_id: update.update_id.to_string(),
        conversation,
        sender,
        text,
        attachments,
        reply_to_provider_id: message
            .reply_to_message
            .as_ref()
            .map(|reply| reply.message_id.to_string()),
        mentions_self,
        received_at_ms: message.date * 1000,
        metadata,
    })
}

/// True when one of `entities` is a self-mention: an explicit `@username`
/// mention entity naming the bot, or a `bot_command` addressed to it
/// (`/cmd@botusername`).
fn entity_mentions(text: &str, entities: &[TelegramEntity], username: &str) -> bool {
    let target = format!("@{}", username.to_ascii_lowercase());
    entities.iter().any(|entity| match entity.kind.as_str() {
        "mention" => {
            utf16_substr(text, entity.offset, entity.length).to_ascii_lowercase() == target
        }
        "bot_command" => utf16_substr(text, entity.offset, entity.length)
            .to_ascii_lowercase()
            .ends_with(&target),
        _ => false,
    })
}

/// Slices `text` by UTF-16 code unit offset/length, the unit Telegram's
/// `MessageEntity.offset`/`.length` are specified in. Rust's own indexing is
/// byte-based, so this has to go through an explicit UTF-16 round trip rather
/// than any `str` slicing.
fn utf16_substr(text: &str, offset: i64, length: i64) -> String {
    if offset < 0 || length < 0 {
        return String::new();
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    let start = (offset as usize).min(units.len());
    let end = (start + length as usize).min(units.len());
    String::from_utf16_lossy(&units[start..end])
}

/// Splits `text` into chunks of at most `max_units` UTF-16 code units each,
/// never inside a `char` (so never inside a surrogate pair). Used both to
/// respect Telegram's 4096-unit `sendMessage` limit and, via
/// [`MAX_MESSAGE_UTF16`], to size `ProviderCapabilities::max_text_chars`.
fn split_utf16_chunks(text: &str, max_units: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_units = 0usize;
    for ch in text.chars() {
        let ch_units = ch.len_utf16();
        if current_units + ch_units > max_units && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_units = 0;
        }
        current.push(ch);
        current_units += ch_units;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

// `#[serde(bound(...))]` pinned explicitly: serde's automatic bound inference
// adds `T: Default` for every generic parameter once any field on the struct
// carries `#[serde(default)]`, even though only `result: Option<T>` uses it
// and `Option<T>: Default` holds unconditionally. Left to infer, that would
// force every `T` this is instantiated with (`TelegramUser`, `TelegramMessage`,
// `Vec<TelegramUpdate>`) to implement `Default` for no reason but a
// derive-macro limitation.
#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: serde::de::Deserialize<'de>"))]
struct TelegramApiResponse<T> {
    ok: bool,
    #[serde(default)]
    result: Option<T>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramErrorResponse {
    #[serde(default)]
    #[allow(dead_code)]
    ok: bool,
    #[serde(default)]
    parameters: Option<TelegramResponseParameters>,
}

#[derive(Debug, Deserialize)]
struct TelegramResponseParameters {
    #[serde(default)]
    retry_after: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct TelegramUser {
    id: i64,
    #[serde(default)]
    is_bot: bool,
    #[serde(default)]
    first_name: String,
    #[serde(default)]
    username: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramEntity {
    #[serde(rename = "type")]
    kind: String,
    offset: i64,
    length: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramPhotoSize {
    file_id: String,
    #[serde(default)]
    width: i64,
    #[serde(default)]
    height: i64,
    #[serde(default)]
    file_size: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct TelegramDocument {
    file_id: String,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    file_size: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct TelegramVoice {
    file_id: String,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    file_size: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct TelegramAudio {
    file_id: String,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    file_size: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct TelegramVideo {
    file_id: String,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    file_size: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    message_id: i64,
    #[serde(default)]
    message_thread_id: Option<i64>,
    #[serde(default)]
    date: i64,
    chat: TelegramChat,
    #[serde(default)]
    from: Option<TelegramUser>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    entities: Option<Vec<TelegramEntity>>,
    #[serde(default)]
    caption_entities: Option<Vec<TelegramEntity>>,
    #[serde(default)]
    reply_to_message: Option<Box<TelegramMessage>>,
    #[serde(default)]
    photo: Option<Vec<TelegramPhotoSize>>,
    #[serde(default)]
    document: Option<TelegramDocument>,
    #[serde(default)]
    voice: Option<TelegramVoice>,
    #[serde(default)]
    audio: Option<TelegramAudio>,
    #[serde(default)]
    video: Option<TelegramVideo>,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    #[serde(default)]
    message: Option<TelegramMessage>,
    #[serde(default)]
    edited_message: Option<TelegramMessage>,
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_file_path_that_could_leave_the_download_endpoint_is_refused() {
        assert!(usable_file_path("photos/file_0.jpg"));
        assert!(!usable_file_path("../bot123456:secret/getMe"));
        assert!(!usable_file_path("/etc/passwd"));
        assert!(!usable_file_path("https://evil.example.com/x"));
        assert!(!usable_file_path(""));
    }

    use super::*;

    const PRIVATE_MESSAGE: &str = r#"{
        "update_id": 100,
        "message": {
            "message_id": 1,
            "date": 1700000000,
            "chat": {"id": 555, "type": "private"},
            "from": {"id": 555, "is_bot": false, "first_name": "Ada", "username": "ada"},
            "text": "hello there"
        }
    }"#;

    const GROUP_THREAD_MESSAGE: &str = r#"{
        "update_id": 101,
        "message": {
            "message_id": 2,
            "message_thread_id": 42,
            "date": 1700000001,
            "chat": {"id": -999, "type": "supergroup", "title": "Ops"},
            "from": {"id": 777, "is_bot": false, "first_name": "Bo"},
            "text": "/deploy@little_monkey_bot now",
            "entities": [{"type": "bot_command", "offset": 0, "length": 25}]
        }
    }"#;

    const DOCUMENT_MESSAGE: &str = r#"{
        "update_id": 102,
        "message": {
            "message_id": 3,
            "date": 1700000002,
            "chat": {"id": 555, "type": "private"},
            "from": {"id": 555, "is_bot": false, "first_name": "Ada"},
            "document": {"file_id": "FILE123", "file_name": "report.pdf", "mime_type": "application/pdf", "file_size": 4096}
        }
    }"#;

    const REPLY_MESSAGE: &str = r#"{
        "update_id": 103,
        "message": {
            "message_id": 4,
            "date": 1700000003,
            "chat": {"id": 555, "type": "private"},
            "from": {"id": 555, "is_bot": false, "first_name": "Ada"},
            "text": "thanks",
            "reply_to_message": {
                "message_id": 1,
                "date": 1700000000,
                "chat": {"id": 555, "type": "private"}
            }
        }
    }"#;

    fn parse(json: &str) -> TelegramUpdate {
        serde_json::from_str(json).expect("fixture parses")
    }

    #[test]
    fn normalizes_a_private_chat_message() {
        let update = parse(PRIVATE_MESSAGE);
        let envelope = normalize_update(&update, None, None).expect("envelope");
        assert_eq!(envelope.provider_event_id, "100");
        assert_eq!(envelope.conversation.kind, ConversationKind::Direct);
        assert_eq!(envelope.conversation.conversation_id, "555");
        assert_eq!(envelope.sender.sender_id, "555");
        assert_eq!(envelope.text, "hello there");
        assert_eq!(envelope.received_at_ms, 1700000000 * 1000);
    }

    #[test]
    fn separates_forum_threads_from_the_group() {
        let update = parse(GROUP_THREAD_MESSAGE);
        let envelope = normalize_update(&update, None, None).expect("envelope");
        assert_eq!(envelope.conversation.kind, ConversationKind::Group);
        assert_eq!(envelope.conversation.conversation_id, "-999");
        assert_eq!(envelope.conversation.thread_id.as_deref(), Some("42"));
    }

    #[test]
    fn detects_bot_command_mentions_by_username() {
        let update = parse(GROUP_THREAD_MESSAGE);
        let envelope =
            normalize_update(&update, None, Some("little_monkey_bot")).expect("envelope");
        assert!(envelope.mentions_self);

        let not_mentioned = normalize_update(&update, None, Some("someone_else")).expect("env");
        assert!(!not_mentioned.mentions_self);
    }

    #[test]
    fn is_self_true_only_when_sender_matches_cached_bot_id() {
        let update = parse(PRIVATE_MESSAGE);
        let matched = normalize_update(&update, Some(555), None).expect("envelope");
        assert!(matched.sender.is_self);

        let unmatched = normalize_update(&update, Some(9), None).expect("envelope");
        assert!(!unmatched.sender.is_self);

        let unknown = normalize_update(&update, None, None).expect("envelope");
        assert!(!unknown.sender.is_self);
    }

    #[test]
    fn normalizes_a_document_attachment_as_a_provider_handle() {
        let update = parse(DOCUMENT_MESSAGE);
        let envelope = normalize_update(&update, None, None).expect("envelope");
        assert_eq!(envelope.attachments.len(), 1);
        let attachment = &envelope.attachments[0];
        assert_eq!(attachment.kind, AttachmentKind::Document);
        assert_eq!(attachment.declared_size_bytes, Some(4096));
        match &attachment.source {
            AttachmentSource::ProviderHandle { handle } => assert_eq!(handle, "FILE123"),
            other => panic!("expected a provider handle, got {other:?}"),
        }
    }

    #[test]
    fn carries_the_reply_target_id() {
        let update = parse(REPLY_MESSAGE);
        let envelope = normalize_update(&update, None, None).expect("envelope");
        assert_eq!(envelope.reply_to_provider_id.as_deref(), Some("1"));
    }

    #[test]
    fn cursor_advances_to_the_highest_update_id_seen() {
        let updates = vec![parse(PRIVATE_MESSAGE), parse(GROUP_THREAD_MESSAGE)];
        assert_eq!(next_cursor(&updates, Some("5")), Some("101".to_string()));
    }

    #[test]
    fn cursor_holds_steady_on_an_empty_batch() {
        assert_eq!(next_cursor(&[], Some("101")), Some("101".to_string()));
        assert_eq!(next_cursor(&[], None), None);
    }

    #[test]
    fn splits_replies_over_4096_utf16_units() {
        let long = "a".repeat(9000);
        let chunks = split_utf16_chunks(&long, MAX_MESSAGE_UTF16);
        assert_eq!(chunks.len(), 3);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.encode_utf16().count() <= MAX_MESSAGE_UTF16));
        assert_eq!(chunks.concat(), long);
    }

    #[test]
    fn split_never_breaks_a_surrogate_pair() {
        // U+1F600 (an emoji) is two UTF-16 units; pad so the boundary lands
        // exactly on it and check nothing panics or corrupts the character.
        let padding = "a".repeat(MAX_MESSAGE_UTF16 - 1);
        let text = format!("{padding}\u{1F600}");
        let chunks = split_utf16_chunks(&text, MAX_MESSAGE_UTF16);
        assert_eq!(chunks.concat(), text);
        for chunk in &chunks {
            assert!(chunk.encode_utf16().count() <= MAX_MESSAGE_UTF16);
        }
    }

    #[test]
    fn short_text_is_a_single_chunk() {
        assert_eq!(split_utf16_chunks("hi", MAX_MESSAGE_UTF16), vec!["hi"]);
    }

    #[test]
    fn bot_token_never_appears_in_a_redacted_error() {
        let config_account = super::super::super::channel_store::ChannelAccountRecord {
            account_id: "acct-1".to_string(),
            kind: ChannelKind::Telegram,
            label: "Bot".to_string(),
            enabled: true,
            non_secret_config: serde_json::json!({}),
            credential_ref: Some("telegram/acct-1".to_string()),
            access_policy: Default::default(),
            health: ChannelHealth::error(0, "unused"),
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let config = AdapterConfig {
            account: &config_account,
            secret: "123456:SUPER-SECRET-TOKEN".to_string(),
        };
        let adapter = TelegramAdapter::new(&config).expect("adapter");
        let url = adapter.method_url("getMe");
        let rendered = adapter.redact(format!(
            "error sending request for url ({url}): connection refused"
        ));
        assert!(!rendered.contains("SUPER-SECRET-TOKEN"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn rejects_an_empty_token() {
        let config_account = super::super::super::channel_store::ChannelAccountRecord {
            account_id: "acct-1".to_string(),
            kind: ChannelKind::Telegram,
            label: "Bot".to_string(),
            enabled: true,
            non_secret_config: serde_json::json!({}),
            credential_ref: None,
            access_policy: Default::default(),
            health: ChannelHealth::error(0, "unused"),
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let config = AdapterConfig {
            account: &config_account,
            secret: String::new(),
        };
        assert!(TelegramAdapter::new(&config).is_err());
    }
}
