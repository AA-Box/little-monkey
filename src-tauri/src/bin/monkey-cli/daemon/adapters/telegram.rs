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
    /// The most recent poll failure, cleared by the next successful poll.
    /// Folded into `probe` so the health an operator reads reflects the
    /// transport actually moving messages, not only the credential: a valid
    /// token whose `getUpdates` keeps failing is degraded, not connected.
    last_poll_error: Mutex<Option<String>>,
    /// The Bot API origin. Always [`API_BASE`] in production; swappable in
    /// tests so an upload can be exercised against a loopback fixture.
    api_base: String,
    /// Where an outbound attachment's bytes come from. The daemon's content
    /// store in production, a fixture in tests.
    blobs: std::sync::Arc<dyn crate::daemon::channel_adapter::BlobSource>,
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
            last_poll_error: Mutex::new(None),
            api_base: API_BASE.to_string(),
            blobs: std::sync::Arc::new(crate::daemon::channel_adapter::DaemonBlobs),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_base_url(mut self, base: &str) -> Self {
        self.api_base = base.to_string();
        self
    }

    #[cfg(test)]
    pub(crate) fn with_blobs(
        mut self,
        blobs: std::sync::Arc<dyn crate::daemon::channel_adapter::BlobSource>,
    ) -> Self {
        self.blobs = blobs;
        self
    }

    /// `https://api.telegram.org/bot<token>/<method>`. Never logged and never
    /// handed to a diagnostic unredacted — see [`Self::redact`].
    fn method_url(&self, method: &str) -> String {
        format!("{}/bot{}/{method}", self.api_base, self.token)
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

    /// Upload one file with `sendPhoto` or `sendDocument`.
    ///
    /// A picture is sent as a photo so it renders inline in the chat, and
    /// everything else as a document so Telegram does not re-encode it. The
    /// bytes come from the content store, where the reply tool copied them when
    /// the agent asked — a retry minutes later sends what was meant then, not
    /// whatever now occupies that path.
    ///
    /// `Err` carries the outcome the outbox should record, which is the same
    /// distinction the text path makes: a failed handshake provably never left
    /// this machine, anything later is unknown and must be reconciled rather
    /// than retried into a duplicate upload.
    async fn send_one_attachment(
        &self,
        client: &reqwest::Client,
        message: &OutboundMessage,
        attachment: &little_monkey_lib::channels::types::OutboundAttachment,
        any_sent: bool,
    ) -> Result<Option<String>, SendOutcome> {
        let bytes = self
            .blobs
            .read(&attachment.artifact_id)
            .map_err(|error| SendOutcome::PermanentFailure { error })?;
        let mime = crate::daemon::channel_adapter::attachment_mime(attachment).to_string();
        let filename = attachment
            .filename
            .clone()
            .unwrap_or_else(|| "attachment".to_string());
        let (method, field) = upload_method(&mime);
        // The provider's own limit, checked before a single byte goes on the
        // wire: an oversized file fails the same way every time, and burning
        // the whole upload to hear that from Telegram helps nobody.
        let limit = upload_limit(method);
        if bytes.len() as u64 > limit {
            return Err(SendOutcome::PermanentFailure {
                error: format!(
                    "'{filename}' is {} bytes, over Telegram's {limit}-byte limit for {method}{}",
                    bytes.len(),
                    if method == "sendPhoto" {
                        "; send it as a document instead"
                    } else {
                        ""
                    }
                ),
            });
        }

        let part = match reqwest::multipart::Part::bytes(bytes)
            .file_name(filename)
            .mime_str(&mime)
        {
            Ok(part) => part,
            Err(_) => reqwest::multipart::Part::bytes(Vec::new()),
        };
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", message.conversation_id.clone())
            .part(field.to_string(), part);
        if let Some(thread_id) = message.thread_id.clone() {
            form = form.text("message_thread_id", thread_id);
        }
        if let Some(parameters) = reply_parameters(message) {
            // Multipart carries the object as its JSON text, which is how the
            // Bot API accepts every non-scalar field in a form-encoded request.
            form = form.text("reply_parameters", parameters.to_string());
        }

        let request = client.post(self.method_url(method)).multipart(form);
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => {
                // A connect failure provably sent nothing — but once any
                // chunk or earlier file of this message has been delivered,
                // the whole-message retry the outbox would run duplicates it.
                return Err(if error.is_connect() && !any_sent {
                    SendOutcome::RetryableFailure {
                        error: self.redact(format!("Could not connect to Telegram: {error}")),
                        retry_after_ms: None,
                    }
                } else {
                    SendOutcome::NeedsReconciliation {
                        error: self.redact(format!("Telegram upload outcome unknown: {error}")),
                    }
                });
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
            return Err(if any_sent {
                SendOutcome::NeedsReconciliation {
                    error: "Telegram rate-limited the upload after part of the message was sent"
                        .to_string(),
                }
            } else {
                SendOutcome::RetryableFailure {
                    error: "Telegram rate-limited the upload (429)".to_string(),
                    retry_after_ms,
                }
            });
        }
        if !status.is_success() {
            // A Telegram outage (5xx) is worth retrying when nothing has
            // been delivered; after partial delivery every failure parks.
            return Err(if any_sent {
                SendOutcome::NeedsReconciliation {
                    error: self.redact(format!(
                        "Telegram returned {status} for {method} after part of the message was sent"
                    )),
                }
            } else if status.is_server_error() {
                SendOutcome::RetryableFailure {
                    error: self.redact(format!("Telegram returned {status} for {method}")),
                    retry_after_ms: None,
                }
            } else {
                SendOutcome::PermanentFailure {
                    error: self.redact(format!("Telegram returned {status} for {method}")),
                }
            });
        }
        match serde_json::from_str::<TelegramApiResponse<TelegramMessage>>(&body_text) {
            Ok(parsed) if parsed.ok => {
                Ok(parsed.result.map(|message| message.message_id.to_string()))
            }
            _ => Err(SendOutcome::NeedsReconciliation {
                error: format!("Telegram accepted {method} but returned an unparseable response"),
            }),
        }
    }
}

/// Telegram's own cap on a photo uploaded through `sendPhoto`: 10 MB.
/// Everything else — documents, audio, video — goes through the 50 MB
/// bot-upload cap. Both are the provider's published limits, named here so
/// the pre-upload check and the tests agree on the same numbers.
const TELEGRAM_MAX_PHOTO_BYTES: u64 = 10 * 1024 * 1024;
/// Telegram's cap on every non-photo bot upload (documents, audio, video).
const TELEGRAM_MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// The API method and form field one MIME type should be uploaded through.
///
/// A picture goes as a photo so it renders inline in the chat; audio and
/// video go through their own methods so they arrive playable rather than as
/// opaque files. Everything else goes as a document — which is also the
/// right answer for SVG: Telegram's photo path re-encodes what it is given,
/// and an SVG that survives as a file is more useful than one that arrives
/// as a raster.
fn upload_method(mime: &str) -> (&'static str, &'static str) {
    if mime.starts_with("image/") && mime != "image/svg+xml" {
        ("sendPhoto", "photo")
    } else if mime.starts_with("audio/") {
        ("sendAudio", "audio")
    } else if mime.starts_with("video/") {
        ("sendVideo", "video")
    } else {
        ("sendDocument", "document")
    }
}

/// The provider's size cap for one upload method.
fn upload_limit(method: &str) -> u64 {
    match method {
        "sendPhoto" => TELEGRAM_MAX_PHOTO_BYTES,
        _ => TELEGRAM_MAX_FILE_BYTES,
    }
}

/// Translates the provider-independent `reply_to_provider_id` into the field
/// the current Bot API reads.
///
/// One helper for both request shapes, because the text and upload paths must
/// not drift: a message that replies when it is text and does not when it
/// carries a file is the kind of difference nobody notices until a thread
/// reads wrong. `None` when there is nothing to reply to, or when the stored
/// id is not a message number this provider could have issued.
fn reply_parameters(message: &OutboundMessage) -> Option<serde_json::Value> {
    let message_id = message
        .reply_to_provider_id
        .as_deref()?
        .parse::<i64>()
        .ok()?;
    Some(serde_json::json!({ "message_id": message_id }))
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
                    // The credential works, but health describes the whole
                    // transport: a poll loop that keeps failing means
                    // messages are not arriving, and that is degraded.
                    let poll_error = self
                        .last_poll_error
                        .lock()
                        .ok()
                        .and_then(|slot| slot.clone());
                    match poll_error {
                        Some(error) => ChannelHealth {
                            state: little_monkey_lib::channels::types::HealthState::Degraded,
                            detail: Some(format!(
                                "Authenticated to Telegram as {detail}; the last poll failed"
                            )),
                            last_error: Some(error),
                            probed_at_ms: now,
                        },
                        None => ChannelHealth::connected(now, Some(detail)),
                    }
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
        // The outcome is mirrored into `last_poll_error` either way, so the
        // next probe reports the polling loop's actual condition rather than
        // only whether the credential works.
        let result = async {
            // `is_self` and `mentions_self` are only meaningful once the bot knows
            // who it is, and nothing in the daemon's inbound loop calls `probe`.
            // Resolved here, once, so a group configured to answer on mention works
            // from the first poll rather than from whenever an operator happens to
            // run `channels probe`. A failure leaves the identity unknown, which is
            // the conservative answer — never a claim of self it cannot back.
            let identity_known = self.self_id.lock().map(|id| id.is_some()).unwrap_or(false);
            if !identity_known {
                let _ = self.probe().await;
            }
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
                .map_err(|error| {
                    self.redact(format!("Telegram getUpdates parse failed: {error}"))
                })?;
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
        .await;
        if let Ok(mut slot) = self.last_poll_error.lock() {
            *slot = result.as_ref().err().cloned();
        }
        result
    }

    /// Telegram hands out a `file_id`, not a URL: it is resolved with
    /// `getFile`, which answers with a short-lived path under the bot's own
    /// file endpoint. Both calls carry the token, which is why this cannot be
    /// the generic URL download.
    async fn fetch_attachment(
        &self,
        attachment: &little_monkey_lib::channels::types::ChannelAttachment,
        limits: crate::daemon::channel_adapter::AttachmentLimits,
    ) -> Result<Vec<u8>, String> {
        let AttachmentSource::ProviderHandle { handle } = &attachment.source else {
            return crate::daemon::channel_adapter::fetch_url(
                match &attachment.source {
                    AttachmentSource::Url { url } => url,
                    AttachmentSource::ProviderHandle { .. } => unreachable!(),
                },
                None,
                limits.max_bytes,
            )
            .await;
        };
        let client = self.client()?;
        let request = client
            .get(self.method_url("getFile"))
            .query(&[("file_id", handle.as_str())]);
        let response = little_monkey_lib::egress::send(request)
            .await
            .map_err(|error| self.redact(format!("Telegram getFile failed: {error}")))?;
        if !response.status().is_success() {
            return Err(format!(
                "Telegram returned {} for getFile",
                response.status()
            ));
        }
        let body = response
            .text()
            .await
            .map_err(|error| self.redact(error.to_string()))?;
        let file_path = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("result")?
                    .get("file_path")?
                    .as_str()
                    .map(str::to_string)
            })
            .ok_or_else(|| "Telegram named no path for that file".to_string())?;
        crate::daemon::channel_adapter::fetch_url(
            &format!("{}/file/bot{}/{file_path}", self.api_base, self.token),
            None,
            limits.max_bytes,
        )
        .await
        .map_err(|error| self.redact(error))
    }

    async fn send(&self, message: &OutboundMessage) -> SendOutcome {
        let mut chunks = split_utf16_chunks(&message.text, MAX_MESSAGE_UTF16);
        // A reply that is only a file sends only the file. Pushing an empty
        // chunk here would post a blank message first, which Telegram rejects
        // and which nobody asked for.
        if chunks.is_empty() && message.attachments.is_empty() {
            chunks.push(String::new());
        }
        let client = match self.client() {
            Ok(client) => client,
            Err(error) => return SendOutcome::PermanentFailure { error },
        };
        let mut last_message_id: Option<String> = None;
        // Once any chunk has been delivered, a whole-message retry would
        // deliver it twice — from that point every failure parks the row.
        let mut any_sent = false;
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
            if let Some(parameters) = reply_parameters(message) {
                body["reply_parameters"] = parameters;
            }
            let request = client.post(self.method_url("sendMessage")).json(&body);
            let response = match little_monkey_lib::egress::send(request).await {
                Ok(response) => response,
                Err(error) => {
                    return if error.is_connect() && !any_sent {
                        // The TCP/TLS handshake itself failed: the request
                        // provably never left this machine, and nothing of
                        // this message has been delivered yet.
                        SendOutcome::RetryableFailure {
                            error: self.redact(format!("Could not connect to Telegram: {error}")),
                            retry_after_ms: None,
                        }
                    } else {
                        // Anything else (a stalled read, a reset mid-response)
                        // may have happened after the request was already
                        // written — and once a chunk has landed, retrying the
                        // whole message would deliver it twice.
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
                return if any_sent {
                    SendOutcome::NeedsReconciliation {
                        error:
                            "Telegram rate-limited the request after part of the message was sent"
                                .to_string(),
                    }
                } else {
                    SendOutcome::RetryableFailure {
                        error: "Telegram rate-limited the request (429)".to_string(),
                        retry_after_ms,
                    }
                };
            }
            if !status.is_success() {
                // A Telegram outage (5xx) retries while nothing has been
                // delivered; after partial delivery every failure parks, and
                // a definite rejection with nothing sent is permanent.
                return if any_sent {
                    SendOutcome::NeedsReconciliation {
                        error: self.redact(format!(
                            "Telegram returned {status} after part of the message was sent"
                        )),
                    }
                } else if status.is_server_error() {
                    SendOutcome::RetryableFailure {
                        error: self.redact(format!("Telegram returned {status} for sendMessage")),
                        retry_after_ms: None,
                    }
                } else {
                    SendOutcome::PermanentFailure {
                        error: self.redact(format!("Telegram returned {status} for sendMessage")),
                    }
                };
            }
            match serde_json::from_str::<TelegramApiResponse<TelegramMessage>>(&body_text) {
                Ok(parsed) if parsed.ok => {
                    any_sent = true;
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
        for attachment in &message.attachments {
            match self
                .send_one_attachment(&client, message, attachment, any_sent)
                .await
            {
                Ok(message_id) => {
                    any_sent = true;
                    last_message_id = message_id.or(last_message_id);
                }
                Err(outcome) => return outcome,
            }
        }
        SendOutcome::Sent {
            provider_message_id: last_message_id,
        }
    }
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
                stored_artifact_id: None,
                text_excerpt: None,
                fetch_error: None,
                provider_id: Some(largest.file_id.clone()),
                kind: AttachmentKind::Image,
                filename: None,
                mime_type: None,
                declared_size_bytes: largest.file_size.map(|size| size as u64),
                stored_size_bytes: None,
                source: AttachmentSource::ProviderHandle {
                    handle: largest.file_id.clone(),
                },
            });
        }
    }
    if let Some(document) = &message.document {
        attachments.push(ChannelAttachment {
            stored_artifact_id: None,
            text_excerpt: None,
            fetch_error: None,
            provider_id: Some(document.file_id.clone()),
            kind: AttachmentKind::Document,
            filename: document.file_name.clone(),
            mime_type: document.mime_type.clone(),
            declared_size_bytes: document.file_size.map(|size| size as u64),
            stored_size_bytes: None,
            source: AttachmentSource::ProviderHandle {
                handle: document.file_id.clone(),
            },
        });
    }
    if let Some(voice) = &message.voice {
        attachments.push(ChannelAttachment {
            stored_artifact_id: None,
            text_excerpt: None,
            fetch_error: None,
            provider_id: Some(voice.file_id.clone()),
            kind: AttachmentKind::Audio,
            filename: None,
            mime_type: voice.mime_type.clone(),
            declared_size_bytes: voice.file_size.map(|size| size as u64),
            stored_size_bytes: None,
            source: AttachmentSource::ProviderHandle {
                handle: voice.file_id.clone(),
            },
        });
    }
    if let Some(audio) = &message.audio {
        attachments.push(ChannelAttachment {
            stored_artifact_id: None,
            text_excerpt: None,
            fetch_error: None,
            provider_id: Some(audio.file_id.clone()),
            kind: AttachmentKind::Audio,
            filename: audio.file_name.clone(),
            mime_type: audio.mime_type.clone(),
            declared_size_bytes: audio.file_size.map(|size| size as u64),
            stored_size_bytes: None,
            source: AttachmentSource::ProviderHandle {
                handle: audio.file_id.clone(),
            },
        });
    }
    if let Some(video) = &message.video {
        attachments.push(ChannelAttachment {
            stored_artifact_id: None,
            text_excerpt: None,
            fetch_error: None,
            provider_id: Some(video.file_id.clone()),
            kind: AttachmentKind::Video,
            filename: None,
            mime_type: video.mime_type.clone(),
            declared_size_bytes: video.file_size.map(|size| size as u64),
            stored_size_bytes: None,
            source: AttachmentSource::ProviderHandle {
                handle: video.file_id.clone(),
            },
        });
    }

    let mut metadata = little_monkey_lib::channels::types::BoundedMetadata::new();
    metadata.insert("chat_type", message.chat.kind.clone());
    metadata.insert("update_kind", update_kind);
    // The envelope's provider_event_id is the update_id, which is what the
    // poll stream dedupes by — but a reply must be addressed to the chat-scoped
    // message_id, a different number. Recorded here so the reply-building side
    // anchors to the message Telegram can actually find.
    metadata.insert("provider_message_id", message.message_id.to_string());

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
    use super::*;

    /// An adapter pointed at a loopback fixture, holding one file.
    fn upload_adapter(base: &str, bytes: &[u8]) -> TelegramAdapter {
        let account = crate::daemon::channel_store::ChannelAccountRecord {
            account_id: "acct-tg".into(),
            kind: ChannelKind::Telegram,
            label: "Test".into(),
            enabled: true,
            non_secret_config: serde_json::json!({}),
            credential_ref: Some("tg".into()),
            access_policy: little_monkey_lib::channels::policy::ChannelAccessPolicy::default(),
            health: ChannelHealth {
                state: little_monkey_lib::channels::types::HealthState::Unconfigured,
                detail: None,
                last_error: None,
                probed_at_ms: 0,
            },
            created_at_ms: 1_000,
            updated_at_ms: 1_000,
        };
        TelegramAdapter::new(&AdapterConfig {
            account: &account,
            secret: "bot-token".into(),
        })
        .expect("adapter")
        .with_base_url(base)
        .with_blobs(std::sync::Arc::new(
            crate::daemon::channel_adapter::test_http::FixtureBlobs(bytes.to_vec()),
        ))
    }

    /// One multipart request replies to `message_id` the way the current Bot
    /// API reads a reply, and says so in no other way. The negative half is
    /// the point: a form carrying both fields would satisfy a looser check
    /// while still asking the provider to honour the retired one.
    fn assert_reply_parameters(request: &str, message_id: i64) {
        assert!(
            request.contains("name=\"reply_parameters\""),
            "no reply_parameters part: {request}"
        );
        assert!(
            request.contains(&format!(r#"{{"message_id":{message_id}}}"#)),
            "reply_parameters does not name message {message_id}: {request}"
        );
        assert!(
            !request.contains("reply_to_message_id"),
            "the retired reply field is still on the wire: {request}"
        );
    }

    fn message_with_file(filename: &str) -> OutboundMessage {
        OutboundMessage {
            account_id: "acct-tg".into(),
            kind: ChannelKind::Telegram,
            conversation_id: "chat-7".into(),
            thread_id: None,
            text: String::new(),
            attachments: vec![little_monkey_lib::channels::types::OutboundAttachment {
                artifact_id: "blob-1".into(),
                filename: Some(filename.to_string()),
                mime_type: None,
            }],
            reply_to_provider_id: Some("42".into()),
            idempotency_key: "reply-1".into(),
        }
    }

    #[tokio::test]
    async fn a_png_is_uploaded_to_sendphoto_with_the_bytes_in_the_body() {
        let (base, requests) = crate::daemon::channel_adapter::test_http::serve(vec![(
            200,
            r#"{"ok":true,"result":{"message_id":99,"chat":{"id":7,"type":"private"}}}"#
                .to_string(),
        )]);
        let adapter = upload_adapter(&base, b"\x89PNG-not-really");

        let outcome = adapter.send(&message_with_file("shot.png")).await;
        assert!(
            matches!(&outcome, SendOutcome::Sent { provider_message_id } if provider_message_id.as_deref() == Some("99")),
            "{outcome:?}"
        );

        let request = requests
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("request");
        let text = String::from_utf8_lossy(&request);
        assert!(text.starts_with("POST /botbot-token/sendPhoto"), "{text}");
        assert!(text.contains("multipart/form-data"), "{text}");
        assert!(text.contains("name=\"photo\""), "{text}");
        assert!(text.contains("filename=\"shot.png\""), "{text}");
        assert!(text.contains("name=\"chat_id\""), "{text}");
        assert_reply_parameters(&text, 42);
        // The file the store held is what went on the wire, not a placeholder.
        assert!(
            request
                .windows(b"PNG-not-really".len())
                .any(|window| window == b"PNG-not-really"),
            "the uploaded bytes are missing from the request"
        );
    }

    #[tokio::test]
    async fn a_pdf_is_uploaded_to_senddocument() {
        let (base, requests) = crate::daemon::channel_adapter::test_http::serve(vec![(
            200,
            r#"{"ok":true,"result":{"message_id":100,"chat":{"id":7,"type":"private"}}}"#
                .to_string(),
        )]);
        let adapter = upload_adapter(&base, b"%PDF-1.7");

        let outcome = adapter.send(&message_with_file("report.pdf")).await;
        assert!(matches!(outcome, SendOutcome::Sent { .. }), "{outcome:?}");
        let request = requests
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("request");
        let text = String::from_utf8_lossy(&request);
        assert!(
            text.starts_with("POST /botbot-token/sendDocument"),
            "{text}"
        );
        assert!(text.contains("name=\"document\""), "{text}");
    }

    #[tokio::test]
    async fn an_inbound_file_is_resolved_through_getfile_and_then_downloaded() {
        let (base, requests) = crate::daemon::channel_adapter::test_http::serve(vec![
            (
                200,
                r#"{"ok":true,"result":{"file_id":"f1","file_path":"documents/build.log"}}"#
                    .to_string(),
            ),
            (200, "error: nope".to_string()),
        ]);
        let adapter = upload_adapter(&base, b"unused");
        let attachment = ChannelAttachment {
            provider_id: Some("f1".into()),
            kind: AttachmentKind::Document,
            filename: Some("build.log".into()),
            mime_type: Some("text/plain".into()),
            declared_size_bytes: None,
            stored_size_bytes: None,
            source: AttachmentSource::ProviderHandle {
                handle: "f1".into(),
            },
            stored_artifact_id: None,
            text_excerpt: None,
            fetch_error: None,
        };

        let bytes = adapter
            .fetch_attachment(&attachment, Default::default())
            .await
            .expect("downloaded");
        assert_eq!(bytes, b"error: nope");

        let lookup = String::from_utf8_lossy(
            &requests
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("getFile"),
        )
        .to_string();
        assert!(
            lookup.starts_with("GET /botbot-token/getFile?file_id=f1"),
            "{lookup}"
        );
        let download = String::from_utf8_lossy(
            &requests
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("download"),
        )
        .to_string();
        // The path Telegram named, under the bot's own file endpoint.
        assert!(
            download.starts_with("GET /file/botbot-token/documents/build.log"),
            "{download}"
        );
    }

    #[tokio::test]
    async fn hydration_stores_the_bytes_and_keeps_a_text_excerpt() {
        let (base, _requests) = crate::daemon::channel_adapter::test_http::serve(vec![
            (
                200,
                r#"{"ok":true,"result":{"file_path":"documents/build.log"}}"#.to_string(),
            ),
            (200, "error: nope".to_string()),
        ]);
        let adapter = upload_adapter(&base, b"unused");
        let mut envelopes = vec![ChannelEnvelope {
            account_id: "acct-tg".into(),
            kind: ChannelKind::Telegram,
            provider_event_id: "1".into(),
            conversation: ChannelConversation::direct("chat-7"),
            sender: ChannelSender::new("user-3"),
            text: String::new(),
            attachments: vec![ChannelAttachment {
                provider_id: Some("f1".into()),
                kind: AttachmentKind::Document,
                filename: Some("build.log".into()),
                mime_type: Some("text/plain".into()),
                declared_size_bytes: None,
                stored_size_bytes: None,
                source: AttachmentSource::ProviderHandle {
                    handle: "f1".into(),
                },
                stored_artifact_id: None,
                text_excerpt: None,
                fetch_error: None,
            }],
            reply_to_provider_id: None,
            mentions_self: false,
            received_at_ms: 0,
            metadata: Default::default(),
        }];

        crate::daemon::channel_adapter::hydrate_attachments(
            &adapter,
            &crate::daemon::channel_adapter::test_http::FixtureBlobs(Vec::new()),
            Default::default(),
            &mut envelopes,
        )
        .await;

        let attachment = &envelopes[0].attachments[0];
        assert_eq!(
            attachment.stored_artifact_id.as_deref(),
            Some("fixture-blob")
        );
        assert_eq!(attachment.text_excerpt.as_deref(), Some("error: nope"));
        // The measured size lands in its own field. `declared_size_bytes` stays
        // what the provider said, which here is nothing — hydration used to
        // overwrite it with the measurement, which is what made the two
        // impossible to compare.
        assert_eq!(attachment.stored_size_bytes, Some(11));
        assert_eq!(attachment.declared_size_bytes, None);
        assert!(attachment.fetch_error.is_none());
    }

    #[tokio::test]
    async fn a_download_the_provider_refuses_is_recorded_on_the_attachment() {
        let (base, _requests) = crate::daemon::channel_adapter::test_http::serve(vec![(
            404,
            r#"{"ok":false}"#.to_string(),
        )]);
        let adapter = upload_adapter(&base, b"unused");
        let mut envelopes = vec![ChannelEnvelope {
            account_id: "acct-tg".into(),
            kind: ChannelKind::Telegram,
            provider_event_id: "1".into(),
            conversation: ChannelConversation::direct("chat-7"),
            sender: ChannelSender::new("user-3"),
            text: "look".into(),
            attachments: vec![ChannelAttachment {
                provider_id: Some("gone".into()),
                kind: AttachmentKind::Image,
                filename: Some("gone.png".into()),
                mime_type: None,
                declared_size_bytes: None,
                stored_size_bytes: None,
                source: AttachmentSource::ProviderHandle {
                    handle: "gone".into(),
                },
                stored_artifact_id: None,
                text_excerpt: None,
                fetch_error: None,
            }],
            reply_to_provider_id: None,
            mentions_self: false,
            received_at_ms: 0,
            metadata: Default::default(),
        }];

        crate::daemon::channel_adapter::hydrate_attachments(
            &adapter,
            &crate::daemon::channel_adapter::test_http::FixtureBlobs(Vec::new()),
            Default::default(),
            &mut envelopes,
        )
        .await;

        let attachment = &envelopes[0].attachments[0];
        assert!(attachment.stored_artifact_id.is_none());
        assert!(
            attachment
                .fetch_error
                .as_deref()
                .is_some_and(|error| error.contains("404")),
            "{:?}",
            attachment.fetch_error
        );
    }

    #[tokio::test]
    async fn a_rejected_upload_is_a_permanent_failure_and_not_a_retry() {
        let (base, _requests) = crate::daemon::channel_adapter::test_http::serve(vec![(
            400,
            r#"{"ok":false,"description":"file too big"}"#.to_string(),
        )]);
        let adapter = upload_adapter(&base, b"bytes");
        let outcome = adapter.send(&message_with_file("shot.png")).await;
        assert!(
            matches!(outcome, SendOutcome::PermanentFailure { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn each_attachment_type_picks_the_method_telegram_renders_it_with() {
        assert_eq!(upload_method("image/png"), ("sendPhoto", "photo"));
        assert_eq!(upload_method("image/jpeg"), ("sendPhoto", "photo"));
        assert_eq!(upload_method("audio/mpeg"), ("sendAudio", "audio"));
        assert_eq!(upload_method("video/mp4"), ("sendVideo", "video"));
        assert_eq!(
            upload_method("application/pdf"),
            ("sendDocument", "document")
        );
        // Telegram's photo path re-encodes, which destroys an SVG.
        assert_eq!(upload_method("image/svg+xml"), ("sendDocument", "document"));
        assert_eq!(
            upload_method("application/octet-stream"),
            ("sendDocument", "document")
        );
    }

    #[test]
    fn each_method_carries_its_own_provider_limit() {
        assert_eq!(upload_limit("sendPhoto"), 10 * 1024 * 1024);
        for method in ["sendDocument", "sendAudio", "sendVideo"] {
            assert_eq!(upload_limit(method), 50 * 1024 * 1024);
        }
    }

    #[tokio::test]
    async fn a_photo_at_exactly_the_limit_is_uploaded() {
        let (base, requests) = crate::daemon::channel_adapter::test_http::serve(vec![(
            200,
            r#"{"ok":true,"result":{"message_id":101,"chat":{"id":7,"type":"private"}}}"#
                .to_string(),
        )]);
        let adapter = upload_adapter(&base, &vec![0u8; TELEGRAM_MAX_PHOTO_BYTES as usize]);
        let outcome = adapter.send(&message_with_file("exact.png")).await;
        assert!(matches!(outcome, SendOutcome::Sent { .. }), "{outcome:?}");
        assert!(requests
            .recv_timeout(std::time::Duration::from_secs(10))
            .is_ok());
    }

    #[tokio::test]
    async fn a_photo_one_byte_over_the_limit_is_refused_before_any_upload() {
        // No server at all: the refusal must happen before a request exists,
        // and a PermanentFailure (not a connect error) proves it did.
        let adapter = upload_adapter(
            "http://127.0.0.1:9",
            &vec![0u8; TELEGRAM_MAX_PHOTO_BYTES as usize + 1],
        );
        match adapter.send(&message_with_file("big.png")).await {
            SendOutcome::PermanentFailure { error } => {
                assert!(error.contains("sendPhoto"), "{error}");
                assert!(error.contains("document instead"), "{error}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_document_one_byte_over_the_limit_is_refused_before_any_upload() {
        let adapter = upload_adapter(
            "http://127.0.0.1:9",
            &vec![0u8; TELEGRAM_MAX_FILE_BYTES as usize + 1],
        );
        match adapter.send(&message_with_file("big.pdf")).await {
            SendOutcome::PermanentFailure { error } => {
                assert!(error.contains("sendDocument"), "{error}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_upload_targets_the_forum_topic_it_replies_into() {
        let (base, requests) = crate::daemon::channel_adapter::test_http::serve(vec![(
            200,
            r#"{"ok":true,"result":{"message_id":102,"chat":{"id":7,"type":"private"}}}"#
                .to_string(),
        )]);
        let adapter = upload_adapter(&base, b"%PDF-1.7");
        let message = OutboundMessage {
            thread_id: Some("42".into()),
            ..message_with_file("report.pdf")
        };
        let outcome = adapter.send(&message).await;
        assert!(matches!(outcome, SendOutcome::Sent { .. }), "{outcome:?}");
        let request = requests
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("request");
        let text = String::from_utf8_lossy(&request);
        assert!(text.contains("name=\"message_thread_id\""), "{text}");
        // Topic targeting and a reply travel together, in the current fields.
        assert_reply_parameters(&text, 42);
    }

    #[tokio::test]
    async fn a_text_reply_names_its_target_in_reply_parameters() {
        let (base, requests) = crate::daemon::channel_adapter::test_http::serve(vec![(
            200,
            r#"{"ok":true,"result":{"message_id":103,"chat":{"id":7,"type":"private"}}}"#
                .to_string(),
        )]);
        let adapter = upload_adapter(&base, b"unused");
        let message = OutboundMessage {
            text: "answering you".into(),
            attachments: Vec::new(),
            thread_id: Some("42".into()),
            ..message_with_file("unused.txt")
        };
        assert!(matches!(
            adapter.send(&message).await,
            SendOutcome::Sent { .. }
        ));

        let request = requests
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("request");
        let text = String::from_utf8_lossy(&request);
        assert!(text.starts_with("POST /botbot-token/sendMessage"), "{text}");
        assert!(text.contains(r#""message_thread_id":42"#), "{text}");
        assert!(
            text.contains(r#""reply_parameters":{"message_id":42}"#),
            "{text}"
        );
        assert!(
            !text.contains("reply_to_message_id"),
            "the retired reply field is still on the wire: {text}"
        );
    }

    #[tokio::test]
    async fn a_message_with_no_reply_target_sends_no_reply_field_at_all() {
        let (base, requests) = crate::daemon::channel_adapter::test_http::serve(vec![(
            200,
            r#"{"ok":true,"result":{"message_id":104,"chat":{"id":7,"type":"private"}}}"#
                .to_string(),
        )]);
        let adapter = upload_adapter(&base, b"unused");
        let message = OutboundMessage {
            text: "unprompted".into(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            ..message_with_file("unused.txt")
        };
        assert!(matches!(
            adapter.send(&message).await,
            SendOutcome::Sent { .. }
        ));

        let request = requests
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("request");
        let text = String::from_utf8_lossy(&request);
        assert!(!text.contains("reply_parameters"), "{text}");
        assert!(!text.contains("reply_to_message_id"), "{text}");
    }

    #[tokio::test]
    async fn a_rate_limited_second_chunk_parks_the_message() {
        let (base, _requests) = crate::daemon::channel_adapter::test_http::serve(vec![
            (
                200,
                r#"{"ok":true,"result":{"message_id":1,"chat":{"id":7,"type":"private"}}}"#
                    .to_string(),
            ),
            (
                429,
                r#"{"ok":false,"parameters":{"retry_after":5}}"#.to_string(),
            ),
        ]);
        let adapter = upload_adapter(&base, b"unused");
        let message = OutboundMessage {
            text: "a".repeat(MAX_MESSAGE_UTF16 + 100),
            attachments: Vec::new(),
            ..message_with_file("unused.txt")
        };
        let outcome = adapter.send(&message).await;
        assert!(
            matches!(outcome, SendOutcome::NeedsReconciliation { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn an_upload_failure_after_the_text_landed_parks_the_message() {
        // The text chunk is delivered; the attachment upload is then rate
        // limited. A whole-message retry would say the text twice.
        let (base, _requests) = crate::daemon::channel_adapter::test_http::serve(vec![
            (
                200,
                r#"{"ok":true,"result":{"message_id":1,"chat":{"id":7,"type":"private"}}}"#
                    .to_string(),
            ),
            (
                429,
                r#"{"ok":false,"parameters":{"retry_after":5}}"#.to_string(),
            ),
        ]);
        let adapter = upload_adapter(&base, b"bytes");
        let message = OutboundMessage {
            text: "here is the file".into(),
            ..message_with_file("notes.txt")
        };
        let outcome = adapter.send(&message).await;
        assert!(
            matches!(outcome, SendOutcome::NeedsReconciliation { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_telegram_outage_is_retried_when_nothing_was_delivered() {
        let (base, _requests) = crate::daemon::channel_adapter::test_http::serve(vec![(
            502,
            r#"{"ok":false}"#.to_string(),
        )]);
        let adapter = upload_adapter(&base, b"unused");
        let message = OutboundMessage {
            text: "hello".into(),
            attachments: Vec::new(),
            ..message_with_file("unused.txt")
        };
        let outcome = adapter.send(&message).await;
        assert!(
            matches!(outcome, SendOutcome::RetryableFailure { .. }),
            "a 502 with nothing delivered must retry, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_failing_poll_degrades_health_until_it_recovers() {
        // getMe (identity), a failing getUpdates, then the probe's own getMe:
        // the probe must fold the poll failure in rather than report the
        // working credential as a working channel.
        let (base, _requests) = crate::daemon::channel_adapter::test_http::serve(vec![
            (
                200,
                r#"{"ok":true,"result":{"id":42,"is_bot":true,"first_name":"Monkey","username":"m"}}"#
                    .to_string(),
            ),
            (500, r#"{"ok":false}"#.to_string()),
            (
                200,
                r#"{"ok":true,"result":{"id":42,"is_bot":true,"first_name":"Monkey","username":"m"}}"#
                    .to_string(),
            ),
        ]);
        let adapter = upload_adapter(&base, b"unused");
        assert!(adapter.poll(None).await.is_err());
        let health = adapter.probe().await;
        assert_eq!(
            health.state,
            little_monkey_lib::channels::types::HealthState::Degraded,
            "{health:?}"
        );
        assert!(health.last_error.is_some());
    }

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
