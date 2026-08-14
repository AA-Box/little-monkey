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
//! A single task (spawned lazily on the first `poll`) resolves our own bot
//! identity via `auth.test`, then owns the Socket Mode connection for the
//! adapter's lifetime. `events_api` envelopes carrying a `message` event are
//! normalized and pushed into a bounded channel; [`ChannelAdapter::poll`] only
//! drains that channel. The envelope framing is [`classify_socket_frame`], a
//! pure function so it is testable without a socket; its result is the ACK
//! decision, and a frame this adapter claims to support but cannot validate
//! is surfaced and left unACKed rather than silently dropped.
//!
//! # The ACK is earned, not automatic
//!
//! Slack redelivers an envelope until it is acknowledged — that redelivery is
//! the only at-least-once guarantee this transport has, and acknowledging
//! before the event is durably recorded trades it away: a crash between the
//! ACK and the insert loses the message forever, and Slack will never send it
//! again. So an envelope that carries a message is *not* acknowledged by the
//! socket reader. Its id is parked, the message flows through `poll` into the
//! durable event log, and only [`ChannelAdapter::commit_batch`] — called by
//! the worker strictly after the insert (or its dedupe) succeeded — releases
//! the ACK back to the socket. Envelopes that carry nothing durable (a
//! `disconnect` warning, an event type this adapter does not ingest, a body
//! that does not parse into a message) are acknowledged immediately: there is
//! nothing to lose, and redelivery of the unparseable is just noise. The
//! reader never waits on agent execution — durable acceptance is a local
//! SQLite insert.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use little_monkey_lib::channels::types::{
    AttachmentSource, ChannelAttachment, ChannelConversation, ChannelEnvelope, ChannelHealth,
    ChannelKind, ChannelSender, HealthState, InboundTransport, OutboundMessage,
    ProviderCapabilities, SendOutcome,
};
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

use crate::daemon::channel_adapter::{
    fetch_url, load_attachments, AdapterConfig, BlobSource, ChannelAdapter, DaemonBlobs,
    InboundBatch, LoadedAttachment, TransportStatus,
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

struct Shared {
    permanent_error: Mutex<Option<String>>,
    /// Envelope ids waiting for durable receipt, keyed by the normalized
    /// event's `provider_event_id`. A redelivered event parks a second id
    /// under the same key, and the one durable insert releases them all.
    pending_acks: std::sync::Mutex<std::collections::HashMap<String, Vec<String>>>,
    /// Hands released envelope ids back to whichever socket connection is
    /// current, which is the only place an ACK frame can be written.
    ack_tx: mpsc::Sender<String>,
    /// What the Socket Mode connection is actually doing — see
    /// [`TransportStatus`]; `poll` cannot answer this, and the probe reports
    /// the transport rather than the credential.
    status: TransportStatus,
}

pub struct SlackAdapter {
    account_id: String,
    secret: SlackSecret,
    http: reqwest::Client,
    inbound_tx: mpsc::Sender<ChannelEnvelope>,
    inbound_rx: Mutex<mpsc::Receiver<ChannelEnvelope>>,
    shared: Arc<Shared>,
    /// Owned here until the socket task takes it at first start.
    ack_rx: std::sync::Mutex<Option<mpsc::Receiver<String>>>,
    /// Guards the one-time spawn of the socket task. `new` itself stays
    /// side-effect-free — see [`DiscordAdapter`](super::discord::DiscordAdapter)'s
    /// module doc for why.
    started: tokio::sync::OnceCell<()>,
    blobs: Arc<dyn BlobSource>,
    /// The Web API origin. Always [`API_BASE`] in production; swappable in
    /// tests so the whole Socket Mode handshake can run against a loopback
    /// fixture.
    api_base: String,
}

impl SlackAdapter {
    pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
        let secret = parse_secret(&config.secret)?;
        let http = little_monkey_lib::egress::hardened()
            .build()
            .map_err(|error| format!("Failed to build the Slack HTTP client: {error}"))?;
        let (tx, rx) = mpsc::channel(INBOUND_CHANNEL_CAPACITY);
        let (ack_tx, ack_rx) = mpsc::channel(INBOUND_CHANNEL_CAPACITY);
        Ok(Self {
            account_id: config.account.account_id.clone(),
            secret,
            http,
            inbound_tx: tx,
            inbound_rx: Mutex::new(rx),
            shared: Arc::new(Shared {
                permanent_error: Mutex::new(None),
                pending_acks: std::sync::Mutex::new(std::collections::HashMap::new()),
                ack_tx,
                status: TransportStatus::default(),
            }),
            ack_rx: std::sync::Mutex::new(Some(ack_rx)),
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

    #[cfg(test)]
    pub(crate) fn with_blobs(mut self, blobs: Arc<dyn BlobSource>) -> Self {
        self.blobs = blobs;
        self
    }

    async fn ensure_started(&self) {
        self.started
            .get_or_init(|| async {
                let ack_rx = self
                    .ack_rx
                    .lock()
                    .ok()
                    .and_then(|mut slot| slot.take())
                    .expect("the ack receiver is taken exactly once, here");
                tokio::spawn(run_socket_loop(
                    self.account_id.clone(),
                    self.secret.clone(),
                    self.api_base.clone(),
                    self.http.clone(),
                    self.inbound_tx.clone(),
                    ack_rx,
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

    /// The Socket Mode connection's own state. A poll coming back empty is
    /// the normal quiet case and says nothing about whether it is live.
    fn live_transport(&self) -> Option<HealthState> {
        Some(self.shared.status.get())
    }

    async fn probe(&self) -> ChannelHealth {
        let now = now_ms();
        if let Some(error) = self.shared.permanent_error.lock().await.clone() {
            return ChannelHealth::error(now, error);
        }
        match auth_test(&self.http, &self.api_base, &self.secret.bot_token).await {
            Ok(identity) if identity.ok => {
                // The Web API proves the bot token; the socket is what proves
                // events can actually arrive. `Connected` is written for
                // exactly one combination — a started task with a live Socket
                // Mode connection. A token whose socket has never started is
                // not connected, and one whose socket dropped is degraded.
                let started = self.started.get().is_some();
                let socket_up = self.shared.status.get() == HealthState::Connected;
                if started && socket_up {
                    ChannelHealth::connected(
                        now,
                        Some(format!("Connected to Slack as {}", identity.user_id)),
                    )
                } else if started {
                    ChannelHealth {
                        state: little_monkey_lib::channels::types::HealthState::Degraded,
                        detail: Some(format!(
                            "Authenticated to Slack as {}; the Socket Mode connection is down",
                            identity.user_id
                        )),
                        last_error: None,
                        probed_at_ms: now,
                    }
                } else {
                    ChannelHealth {
                        state: little_monkey_lib::channels::types::HealthState::Disconnected,
                        detail: Some(format!(
                            "Authenticated to Slack as {}; the Socket Mode connection has not started yet",
                            identity.user_id
                        )),
                        last_error: None,
                        probed_at_ms: now,
                    }
                }
            }
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
        // pushes as it receives, so cursor is always ignored. Durability is
        // carried by the deferred ACK instead — see the module doc.
        self.ensure_started().await;
        if let Some(error) = self.shared.permanent_error.lock().await.clone() {
            return Err(error);
        }
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

    /// The worker has durably recorded (or deduplicated) these envelopes;
    /// their parked Socket Mode acknowledgements may now be released. This is
    /// the second half of the handshake the module doc describes — an ACK
    /// sent any earlier would convert Slack's at-least-once redelivery into
    /// at-most-once.
    async fn commit_batch(&self, envelopes: &[ChannelEnvelope]) {
        for envelope in envelopes {
            let released = self
                .shared
                .pending_acks
                .lock()
                .ok()
                .and_then(|mut pending| pending.remove(&envelope.provider_event_id))
                .unwrap_or_default();
            for envelope_id in released {
                // try_send, never a blocking send: a dead socket task must
                // not wedge the worker. A dropped ACK is always safe — Slack
                // redelivers and the event log deduplicates.
                let _ = self.shared.ack_tx.try_send(envelope_id);
            }
        }
    }

    async fn send(&self, message: &OutboundMessage) -> SendOutcome {
        if !message.attachments.is_empty() {
            let files = match load_attachments(self.blobs.as_ref(), message) {
                Ok(files) => files,
                Err(outcome) => return outcome,
            };
            return self.send_with_attachments(message, &files).await;
        }
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
                .post(format!("{}/chat.postMessage", self.api_base))
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
        limits: crate::daemon::channel_adapter::AttachmentLimits,
    ) -> Result<Vec<u8>, String> {
        let AttachmentSource::ProviderHandle { handle } = &attachment.source else {
            return Err("This Slack attachment has no file id.".to_string());
        };
        let info = little_monkey_lib::egress::send(
            self.http
                .get(format!("{}/files.info", self.api_base))
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
            return Err(missing_scope_error(reason));
        }
        let url = body
            .get("file")
            .and_then(|file| file.get("url_private"))
            .and_then(Value::as_str)
            .ok_or_else(|| "Slack returned no private URL for that file".to_string())?;
        fetch_url(url, Some(&self.secret.bot_token), limits.max_bytes).await
    }
}

impl SlackAdapter {
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
        let mut uploaded: Vec<Value> = Vec::with_capacity(files.len());
        // Flipped the moment Slack's store has accepted any file's bytes.
        // From then on the whole logical message has an external footprint,
        // and a failure that would otherwise be a plain retry parks the row
        // instead — the outbox retries a message from its first byte.
        let mut any_uploaded = false;
        for file in files {
            let request = self
                .http
                .get(format!("{}/files.getUploadURLExternal", self.api_base))
                .header("Authorization", format!("Bearer {}", self.secret.bot_token))
                .query(&[
                    ("filename", file.filename.as_str()),
                    ("length", &file.bytes.len().to_string()),
                ]);
            let response = match little_monkey_lib::egress::send(request).await {
                Ok(response) => response,
                Err(error) => {
                    let error = scrub(&error.to_string(), &self.secret.bot_token);
                    return parked_after_upload(
                        SendOutcome::RetryableFailure {
                            error,
                            retry_after_ms: None,
                        },
                        any_uploaded,
                    );
                }
            };
            let status = response.status().as_u16();
            let retry_after_ms = parse_retry_after_seconds(
                response
                    .headers()
                    .get("Retry-After")
                    .and_then(|value| value.to_str().ok()),
            );
            let body: Value = response.json().await.unwrap_or(Value::Null);
            // Before the first byte upload this step is a plain retry — only
            // a refusal is permanent. After one, see `any_uploaded`.
            if let Some(outcome) = upload_step_failure(status, retry_after_ms, &body, "upload URL")
            {
                return parked_after_upload(outcome, any_uploaded);
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
                    let status = response.status().as_u16();
                    // A rate limit or a server error from the storage host is
                    // transient — the upload URL flow simply starts over —
                    // but only while no earlier file's bytes have landed.
                    // Anything else is a refusal that will repeat.
                    return match status {
                        429 | 500..=599 => parked_after_upload(
                            SendOutcome::RetryableFailure {
                                error: format!("Slack returned HTTP {status} for the byte upload"),
                                retry_after_ms: None,
                            },
                            any_uploaded,
                        ),
                        _ => SendOutcome::PermanentFailure {
                            error: format!("Slack refused the upload ({status})"),
                        },
                    };
                }
                Err(error) => {
                    // Only a connect failure proves no byte reached Slack's
                    // store; anything later — and anything at all once an
                    // earlier file landed — is an unknown external footprint.
                    let is_connect = error.is_connect();
                    let error = scrub(&error.to_string(), &self.secret.bot_token);
                    return if is_connect && !any_uploaded {
                        SendOutcome::RetryableFailure {
                            error,
                            retry_after_ms: None,
                        }
                    } else {
                        SendOutcome::NeedsReconciliation { error }
                    };
                }
            }
            any_uploaded = true;
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
            .post(format!("{}/files.completeUploadExternal", self.api_base))
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
        let status = response.status().as_u16();
        let retry_after_ms = parse_retry_after_seconds(
            response
                .headers()
                .get("Retry-After")
                .and_then(|value| value.to_str().ok()),
        );
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if let Some(outcome) = completion_failure(status, retry_after_ms, &body) {
            return outcome;
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

/// Why a `files.info` lookup was refused, said in terms an operator can act on.
///
/// `missing_scope` is the one worth translating: it is not a broken file or a
/// transient failure but a permission this app was never granted, and the fix
/// is a specific scope in a specific place. The error travels onto the
/// attachment, so whoever sent the file is told why it did not arrive.
fn missing_scope_error(reason: &str) -> String {
    match reason {
        "missing_scope" | "not_allowed_token_type" => "Slack refused the download: this app is \
             missing the files:read scope. Add it under OAuth & Permissions in the Slack app \
             settings and reinstall the app to the workspace"
            .to_string(),
        other => format!("Slack refused files.info: {other}"),
    }
}

/// Converts a would-be retry into a parked row once any file's bytes have
/// already landed in Slack's store: the outbox retries a logical message
/// from its first byte, and a message with an external footprint is not the
/// outbox's to repeat blindly. (An unpublished upload is invisible in the
/// channel, but it exists — reconciliation is the honest state.)
fn parked_after_upload(outcome: SendOutcome, any_uploaded: bool) -> SendOutcome {
    match outcome {
        SendOutcome::RetryableFailure { error, .. } if any_uploaded => {
            SendOutcome::NeedsReconciliation { error }
        }
        other => other,
    }
}

/// The failure outcome of `files.completeUploadExternal`, or `None` on
/// success. This is the one call that makes the upload visible, so its
/// failures split by whether Slack may have processed it: a 429 (either
/// shape) is refused before processing and retries safely, while a server
/// error may have posted the message before dying — that parks.
fn completion_failure(
    http_status: u16,
    retry_after_ms: Option<i64>,
    body: &Value,
) -> Option<SendOutcome> {
    if http_status == 429 {
        return Some(SendOutcome::RetryableFailure {
            error: "Slack rate limited the upload completion".to_string(),
            retry_after_ms,
        });
    }
    if (500..600).contains(&http_status) {
        return Some(SendOutcome::NeedsReconciliation {
            error: format!(
                "Slack returned HTTP {http_status} for the upload completion; the message may have posted"
            ),
        });
    }
    if body.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let error = body
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown_error");
    Some(match error {
        "ratelimited" => SendOutcome::RetryableFailure {
            error: "Slack rate limited the upload completion (ok:false)".to_string(),
            retry_after_ms,
        },
        other => SendOutcome::PermanentFailure {
            error: format!("Slack refused the upload completion: {other}"),
        },
    })
}

/// The failure outcome for one step of the upload flow, or `None` when the
/// step succeeded. Rate limits and server errors are retries — nothing has
/// posted yet at either step, so retrying cannot duplicate a message — and
/// only an explicit refusal is permanent.
fn upload_step_failure(
    http_status: u16,
    retry_after_ms: Option<i64>,
    body: &Value,
    step: &str,
) -> Option<SendOutcome> {
    if http_status == 429 {
        return Some(SendOutcome::RetryableFailure {
            error: format!("Slack rate limited the {step}"),
            retry_after_ms,
        });
    }
    if (500..600).contains(&http_status) {
        return Some(SendOutcome::RetryableFailure {
            error: format!("Slack returned HTTP {http_status} for the {step}"),
            retry_after_ms: None,
        });
    }
    if body.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let error = body
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown_error");
    Some(match error {
        "ratelimited" => SendOutcome::RetryableFailure {
            error: format!("Slack rate limited the {step} (ok:false)"),
            retry_after_ms,
        },
        other => SendOutcome::PermanentFailure {
            error: format!("Slack refused the {step}: {other}"),
        },
    })
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

/// What one Socket Mode text frame means, decided before any I/O.
///
/// The variants are the ACK decision. "Nothing durable came out of this
/// frame" used to collapse two very different cases — an event we ignore on
/// purpose and an event we claim to support but could not parse — into the
/// same immediate ACK, and acknowledging the second silently drops a message
/// forever. Here the two are distinct: `IgnoredAckSafe` is acknowledged
/// because redelivery could never make it actionable, `Reject` is *not*
/// acknowledged, so Slack redelivers and the failure stays visible instead of
/// vanishing.
#[derive(Debug)]
enum SocketFrameResult {
    /// Slack finished its Socket Mode handshake; only now is the connection
    /// proven live end to end.
    Hello,
    /// A message to ingest. Its ACK is parked until the worker reports
    /// durable receipt via `commit_batch`.
    DurableMessage {
        envelope: Box<ChannelEnvelope>,
        envelope_id: String,
    },
    /// Intentionally not ingested — a control frame, an event type outside
    /// this adapter's support. Acknowledge now: redelivery would change
    /// nothing.
    IgnoredAckSafe {
        envelope_id: Option<String>,
        reason: &'static str,
    },
    /// A frame this adapter claims to support but could not validate into a
    /// durable identity. Never acknowledged: an ACK here would tell Slack the
    /// event was handled when it was actually lost.
    Reject { error: String },
    /// Slack asked for this connection to be replaced. Carries the
    /// replacement URL when the frame names one; without one the caller mints
    /// a fresh URL through `apps.connections.open`.
    Reconnect {
        replacement_url: Option<String>,
        envelope_id: Option<String>,
    },
}

/// Classifies one Socket Mode text frame.
///
/// A message envelope is *not* acknowledged here: its id rides the
/// `DurableMessage` result so the I/O loop can park it until durable receipt —
/// see the module doc.
fn classify_socket_frame(
    account_id: &str,
    text: &str,
    our_user_id: Option<&str>,
    our_bot_id: Option<&str>,
    now_ms: i64,
) -> SocketFrameResult {
    let value = match serde_json::from_str::<Value>(text) {
        Ok(value) => value,
        Err(error) => {
            return SocketFrameResult::Reject {
                error: format!("the frame is not valid JSON: {error}"),
            }
        }
    };
    let envelope_id = value
        .get("envelope_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    match value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "hello" => SocketFrameResult::Hello,
        // Slack asks us to open a replacement connection ahead of tearing
        // this one down.
        "disconnect" => SocketFrameResult::Reconnect {
            replacement_url: value
                .pointer("/payload/connection_url")
                .or_else(|| value.get("connection_url"))
                .and_then(Value::as_str)
                .map(str::to_string),
            envelope_id,
        },
        "events_api" => {
            let Some(event) = value
                .get("payload")
                .and_then(|payload| payload.get("event"))
            else {
                return SocketFrameResult::Reject {
                    error: "an events_api envelope carries no event".to_string(),
                };
            };
            if event.get("type").and_then(Value::as_str) != Some("message") {
                return SocketFrameResult::IgnoredAckSafe {
                    envelope_id,
                    reason: "not a message event",
                };
            }
            // From here on this is an event Little Monkey claims to support,
            // so every missing piece is a failure to surface, not a shrug.
            let Some(envelope_id) = envelope_id else {
                return SocketFrameResult::Reject {
                    error: "a message event arrived without an envelope_id".to_string(),
                };
            };
            match normalize_message_event(account_id, event, our_user_id, our_bot_id, now_ms) {
                Some(envelope) => SocketFrameResult::DurableMessage {
                    envelope: Box::new(envelope),
                    envelope_id,
                },
                // `channel` and `ts` are the durable identity — without them
                // the event cannot be deduplicated, so it must not be ACKed
                // as if it had been recorded.
                None => SocketFrameResult::Reject {
                    error: "a message event is missing its channel or ts".to_string(),
                },
            }
        }
        _ => SocketFrameResult::IgnoredAckSafe {
            envelope_id,
            reason: "an unsupported frame type",
        },
    }
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
                        stored_artifact_id: None,
                        text_excerpt: None,
                        fetch_error: None,
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

async fn auth_test(
    http: &reqwest::Client,
    api_base: &str,
    bot_token: &str,
) -> Result<Identity, String> {
    let request = http
        .post(format!("{api_base}/auth.test"))
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

async fn open_socket_url(
    http: &reqwest::Client,
    api_base: &str,
    app_token: &str,
) -> Result<String, String> {
    let request = http
        .post(format!("{api_base}/apps.connections.open"))
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
    api_base: String,
    http: reqwest::Client,
    tx: mpsc::Sender<ChannelEnvelope>,
    mut ack_rx: mpsc::Receiver<String>,
    shared: Arc<Shared>,
) {
    // Our own identity, retried until the network answers: giving up here
    // would silently disable is_self/mentions_self for the daemon's lifetime,
    // and a rejected token — the one permanent answer — is reported instead.
    let mut identity_backoff = MIN_BACKOFF;
    let identity = loop {
        match auth_test(&http, &api_base, &secret.bot_token).await {
            Ok(identity) if identity.ok => break identity,
            Ok(identity) => {
                shared.status.set(HealthState::Error);
                *shared.permanent_error.lock().await =
                    Some(format!("Slack rejected the bot token: {}", identity.error));
                return;
            }
            Err(_) => {
                shared.status.set(HealthState::Degraded);
                tokio::time::sleep(identity_backoff).await;
                identity_backoff = (identity_backoff * 2).min(MAX_BACKOFF);
            }
        }
        if tx.is_closed() {
            return;
        }
    };
    let our_user_id = (!identity.user_id.is_empty()).then_some(identity.user_id);
    let our_bot_id = (!identity.bot_id.is_empty()).then_some(identity.bot_id);

    let mut backoff = MIN_BACKOFF;
    loop {
        let socket_url = match open_socket_url(&http, &api_base, &secret.app_token).await {
            Ok(url) => url,
            Err(_) => {
                shared.status.set(HealthState::Degraded);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        let mut ws = match tokio_tungstenite::connect_async(&socket_url).await {
            Ok((ws, _)) => ws,
            Err(_) => {
                shared.status.set(HealthState::Degraded);
                tokio::time::sleep(backoff + Duration::from_millis(reconnect_jitter_ms())).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        // One logical consumer across any number of transports: a
        // provider-directed refresh hands the loop a replacement socket and
        // processing continues here, so there is never a second long-lived
        // reader of this account's events.
        let mut established = false;
        loop {
            // Parked ACKs and queued releases belong to the previous
            // transport; their envelope ids mean nothing on this one. Slack
            // redelivers the events themselves, and the event log
            // deduplicates.
            if let Ok(mut pending) = shared.pending_acks.lock() {
                pending.clear();
            }
            while ack_rx.try_recv().is_ok() {}
            let (delivered, end) = run_one_connection(
                &account_id,
                ws,
                our_user_id.as_deref(),
                our_bot_id.as_deref(),
                &tx,
                &mut ack_rx,
                &shared,
                &http,
                &api_base,
                &secret.app_token,
            )
            .await;
            established |= delivered;
            match end {
                // The replacement was live before the old socket was retired,
                // so the refresh costs no receive gap and no backoff.
                ConnectionEnd::Handoff(next) => {
                    ws = *next;
                }
                ConnectionEnd::Shutdown => return,
                ConnectionEnd::Dropped => break,
            }
        }
        // Whatever ended it, nothing arrives on this account until the next
        // connection is up.
        shared.status.set(HealthState::Degraded);
        if tx.is_closed() {
            return;
        }
        // Backoff resets only after a connection that actually delivered a
        // frame — resetting on every attempt turns a flapping socket into a
        // once-a-second hammer.
        backoff = if established {
            MIN_BACKOFF
        } else {
            (backoff * 2).min(MAX_BACKOFF)
        };
        tokio::time::sleep(backoff + Duration::from_millis(reconnect_jitter_ms())).await;
    }
}

/// Sub-second wall-clock nanos folded into 0..500, so a fleet of reconnecting
/// clients does not thunder in step. Not cryptographic; does not need to be.
fn reconnect_jitter_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::from(duration.subsec_nanos()))
        .unwrap_or(0)
        % 500
}

/// The established Socket Mode transport. Plain `ws://` in tests, TLS in
/// production; both come out of `connect_async`.
type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// How one Socket Mode connection ended, as `run_socket_loop` needs to know.
enum ConnectionEnd {
    /// A provider-directed refresh: the replacement connection is already
    /// live, the old socket is closed, processing continues on this one.
    Handoff(Box<WsStream>),
    /// The connection dropped or its replacement could not be established;
    /// the caller mints a fresh URL with backoff.
    Dropped,
    /// The adapter handle is gone; the loop must exit for good.
    Shutdown,
}

/// Connects the replacement socket for a provider-directed refresh.
///
/// The URL Slack named in the disconnect frame is used when present; a
/// refresh that names none gets a freshly minted single-use URL. `None` means
/// the caller falls back to its ordinary reconnect path — a fresh
/// `apps.connections.open` under bounded, jittered backoff.
async fn connect_replacement(
    http: reqwest::Client,
    api_base: String,
    app_token: String,
    replacement_url: Option<String>,
) -> Option<WsStream> {
    let url = match replacement_url {
        Some(url) => url,
        None => open_socket_url(&http, &api_base, &app_token).await.ok()?,
    };
    tokio_tungstenite::connect_async(&url)
        .await
        .ok()
        .map(|(ws, _)| ws)
}

/// Runs one Socket Mode connection until it drops, is replaced, or the
/// adapter is dropped; returns whether it ever delivered a frame, and how it
/// ended.
///
/// `Connected` is written only on Slack's `hello` frame: a completed
/// WebSocket handshake happens before Slack has said anything, and reporting
/// it as connected is exactly the kind of optimism the health column exists
/// to prevent.
#[allow(clippy::too_many_arguments)]
async fn run_one_connection(
    account_id: &str,
    mut ws: WsStream,
    our_user_id: Option<&str>,
    our_bot_id: Option<&str>,
    tx: &mpsc::Sender<ChannelEnvelope>,
    ack_rx: &mut mpsc::Receiver<String>,
    shared: &Shared,
    http: &reqwest::Client,
    api_base: &str,
    app_token: &str,
) -> (bool, ConnectionEnd) {
    let mut got_frame = false;
    // Armed by a disconnect frame. The replacement connects while this socket
    // keeps consuming, so a routine refresh costs no receive gap; if both
    // deliver the same event during the overlap, the durable event log
    // deduplicates.
    let mut replacement: Option<
        std::pin::Pin<Box<dyn std::future::Future<Output = Option<WsStream>> + Send>>,
    > = None;
    loop {
        tokio::select! {
            // The adapter handle was dropped (account disabled, credential
            // rotated). Hang up now rather than lingering as a second socket
            // consumer beside the replacement adapter's connection.
            _ = tx.closed() => {
                let _ = ws.close(None).await;
                return (got_frame, ConnectionEnd::Shutdown);
            }
            next = async { replacement.as_mut().expect("guarded by is_some").as_mut().await },
                if replacement.is_some() =>
            {
                match next {
                    Some(new_ws) => {
                        // Retire the old socket only now that its replacement
                        // is live.
                        let _ = ws.close(None).await;
                        return (got_frame, ConnectionEnd::Handoff(Box::new(new_ws)));
                    }
                    None => return (got_frame, ConnectionEnd::Dropped),
                }
            }
            // A durable receipt released this envelope id; the ACK finally
            // goes on the wire.
            released = ack_rx.recv() => {
                if let Some(envelope_id) = released {
                    let payload = serde_json::json!({ "envelope_id": envelope_id });
                    let _ = ws.send(Message::Text(payload.to_string().into())).await;
                }
            }
            frame = ws.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        got_frame = true;
                        match classify_socket_frame(account_id, &text, our_user_id, our_bot_id, now_ms()) {
                            SocketFrameResult::Hello => {
                                shared.status.set(HealthState::Connected);
                            }
                            SocketFrameResult::DurableMessage { envelope, envelope_id } => {
                                // Park the ACK first, then hand the message
                                // on; the reverse order could commit the
                                // batch before the id is parked.
                                if let Ok(mut pending) = shared.pending_acks.lock() {
                                    pending
                                        .entry(envelope.provider_event_id.clone())
                                        .or_default()
                                        .push(envelope_id);
                                }
                                let _ = tx.send(*envelope).await;
                            }
                            SocketFrameResult::IgnoredAckSafe { envelope_id, reason: _ } => {
                                if let Some(envelope_id) = envelope_id {
                                    let payload = serde_json::json!({ "envelope_id": envelope_id });
                                    let _ = ws.send(Message::Text(payload.to_string().into())).await;
                                }
                            }
                            // No ACK on purpose: Slack will redeliver, and the
                            // failure stays in the log instead of becoming a
                            // silently dropped message.
                            SocketFrameResult::Reject { error } => {
                                eprintln!(
                                    "little monkey: slack[{account_id}] rejected a Socket Mode frame: {error}"
                                );
                            }
                            SocketFrameResult::Reconnect { replacement_url, envelope_id } => {
                                if let Some(envelope_id) = envelope_id {
                                    let payload = serde_json::json!({ "envelope_id": envelope_id });
                                    let _ = ws.send(Message::Text(payload.to_string().into())).await;
                                }
                                if replacement.is_none() {
                                    replacement = Some(Box::pin(connect_replacement(
                                        http.clone(),
                                        api_base.to_string(),
                                        app_token.to_string(),
                                        replacement_url,
                                    )));
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = ws.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(Message::Close(_))) => return (got_frame, ConnectionEnd::Dropped),
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => return (got_frame, ConnectionEnd::Dropped),
                }
            }
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
    fn a_message_envelope_parks_its_ack_for_durable_receipt() {
        let text = serde_json::json!({
            "envelope_id": "env-1",
            "type": "events_api",
            "payload": { "event": message_event_fixture() },
        })
        .to_string();
        // No immediate Ack: the id rides the result so the ACK can wait for
        // the durable insert. Acknowledging here would trade Slack's
        // redelivery guarantee for nothing.
        match classify_socket_frame("acct", &text, Some("BOT1"), None, 500) {
            SocketFrameResult::DurableMessage {
                envelope,
                envelope_id,
            } => {
                assert_eq!(envelope_id, "env-1");
                assert_eq!(envelope.provider_event_id, "cmid-1");
            }
            other => panic!("expected a deferred envelope, got {other:?}"),
        }
    }

    #[test]
    fn an_intentionally_ignored_event_is_acked_immediately() {
        // An events_api envelope whose payload is not a message we ingest:
        // redelivery would never make it actionable, so it is acknowledged now.
        let text = serde_json::json!({
            "envelope_id": "env-2",
            "type": "events_api",
            "payload": { "event": { "type": "reaction_added" } },
        })
        .to_string();
        assert!(matches!(
            classify_socket_frame("acct", &text, None, None, 500),
            SocketFrameResult::IgnoredAckSafe { envelope_id: Some(id), .. } if id == "env-2"
        ));

        // A slash-command envelope this adapter does not handle at all.
        let text = serde_json::json!({
            "envelope_id": "env-3",
            "type": "slash_commands",
            "payload": {},
        })
        .to_string();
        assert!(matches!(
            classify_socket_frame("acct", &text, None, None, 500),
            SocketFrameResult::IgnoredAckSafe { envelope_id: Some(id), .. } if id == "env-3"
        ));
    }

    #[test]
    fn a_malformed_supported_event_is_rejected_not_acked() {
        // Not JSON at all: without a parse there is no envelope_id, no
        // identity, nothing to dedupe. ACK-ing is impossible and silence
        // would hide it; the only honest answer is a surfaced rejection.
        assert!(matches!(
            classify_socket_frame("acct", "not json {", None, None, 500),
            SocketFrameResult::Reject { .. }
        ));

        // A message event — a type this adapter claims to support — missing
        // the channel/ts that form its durable identity. ACKing it would
        // tell Slack the message was handled when it was actually lost.
        let text = serde_json::json!({
            "envelope_id": "env-4",
            "type": "events_api",
            "payload": { "event": { "type": "message", "text": "no channel, no ts" } },
        })
        .to_string();
        assert!(matches!(
            classify_socket_frame("acct", &text, None, None, 500),
            SocketFrameResult::Reject { .. }
        ));

        // A message event with no envelope_id cannot ever be acknowledged,
        // so it cannot be treated as handled either.
        let text = serde_json::json!({
            "type": "events_api",
            "payload": { "event": message_event_fixture() },
        })
        .to_string();
        assert!(matches!(
            classify_socket_frame("acct", &text, None, None, 500),
            SocketFrameResult::Reject { .. }
        ));
    }

    #[test]
    fn hello_is_the_connected_signal() {
        let text = serde_json::json!({ "type": "hello", "num_connections": 1 }).to_string();
        assert!(matches!(
            classify_socket_frame("acct", &text, None, None, 500),
            SocketFrameResult::Hello
        ));
    }

    #[test]
    fn disconnect_carries_the_replacement_url_when_slack_names_one() {
        let text = serde_json::json!({ "type": "disconnect", "reason": "warning" }).to_string();
        assert!(matches!(
            classify_socket_frame("acct", &text, None, None, 500),
            SocketFrameResult::Reconnect {
                replacement_url: None,
                ..
            }
        ));

        let text = serde_json::json!({
            "type": "disconnect",
            "reason": "refresh_requested",
            "envelope_id": "env-5",
            "payload": { "connection_url": "wss://replacement.example/link" },
        })
        .to_string();
        match classify_socket_frame("acct", &text, None, None, 500) {
            SocketFrameResult::Reconnect {
                replacement_url,
                envelope_id,
            } => {
                assert_eq!(
                    replacement_url.as_deref(),
                    Some("wss://replacement.example/link")
                );
                assert_eq!(envelope_id.as_deref(), Some("env-5"));
            }
            other => panic!("expected a reconnect, got {other:?}"),
        }
    }

    fn adapter_with_base(base: &str) -> SlackAdapter {
        let account = account_fixture();
        let config = AdapterConfig {
            account: &account,
            secret: r#"{"bot_token":"xoxb-test","app_token":"xapp-test"}"#.to_string(),
        };
        SlackAdapter::new(&config)
            .expect("adapter")
            .with_base_url(base)
    }

    const AUTH_TEST_OK: &str = r#"{"ok":true,"user_id":"UBOT","bot_id":"B1"}"#;

    // -- health semantics ---------------------------------------------------

    #[tokio::test]
    async fn probe_before_the_socket_starts_is_not_connected() {
        let (base, _requests) =
            crate::daemon::channel_adapter::test_http::serve(vec![(200, AUTH_TEST_OK.to_string())]);
        let adapter = adapter_with_base(&base);
        // auth.test succeeds, but nothing has ever asked the socket to
        // connect. A working token is not a connection.
        let health = adapter.probe().await;
        assert_eq!(
            health.state,
            little_monkey_lib::channels::types::HealthState::Disconnected,
            "{health:?}"
        );
    }

    #[tokio::test]
    async fn probe_while_the_socket_is_down_is_degraded() {
        let (base, _requests) =
            crate::daemon::channel_adapter::test_http::serve(vec![(200, AUTH_TEST_OK.to_string())]);
        let adapter = adapter_with_base(&base);
        let _ = adapter.started.set(());
        let health = adapter.probe().await;
        assert_eq!(
            health.state,
            little_monkey_lib::channels::types::HealthState::Degraded,
            "{health:?}"
        );
    }

    #[tokio::test]
    async fn probe_with_a_live_socket_is_connected() {
        let (base, _requests) =
            crate::daemon::channel_adapter::test_http::serve(vec![(200, AUTH_TEST_OK.to_string())]);
        let adapter = adapter_with_base(&base);
        let _ = adapter.started.set(());
        adapter
            .shared
            .status
            .set(little_monkey_lib::channels::types::HealthState::Connected);
        let health = adapter.probe().await;
        assert_eq!(
            health.state,
            little_monkey_lib::channels::types::HealthState::Connected,
            "{health:?}"
        );
    }

    // -- partial sends and upload classification ------------------------------

    #[tokio::test]
    async fn a_rate_limited_second_chunk_parks_the_message() {
        let (base, _requests) = crate::daemon::channel_adapter::test_http::serve(vec![
            (200, r#"{"ok":true,"ts":"1.1"}"#.to_string()),
            (429, r#"{"ok":false,"error":"ratelimited"}"#.to_string()),
        ]);
        let adapter = adapter_with_base(&base);
        let message = OutboundMessage {
            account_id: "acct".into(),
            kind: ChannelKind::Slack,
            conversation_id: "C1".into(),
            thread_id: None,
            text: "a".repeat(SLACK_MAX_TEXT_CHARS + 10),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            idempotency_key: "k".into(),
        };
        let outcome = adapter.send(&message).await;
        assert!(
            matches!(outcome, SendOutcome::NeedsReconciliation { .. }),
            "{outcome:?}"
        );
    }

    fn message_with_files(count: usize) -> OutboundMessage {
        OutboundMessage {
            account_id: "acct".into(),
            kind: ChannelKind::Slack,
            conversation_id: "C1".into(),
            thread_id: None,
            text: "here".into(),
            attachments: (0..count)
                .map(
                    |index| little_monkey_lib::channels::types::OutboundAttachment {
                        artifact_id: format!("blob-{index}"),
                        filename: Some(format!("file-{index}.txt")),
                        mime_type: Some("text/plain".into()),
                    },
                )
                .collect(),
            reply_to_provider_id: None,
            idempotency_key: "k".into(),
        }
    }

    #[tokio::test]
    async fn a_completion_server_error_parks_the_upload() {
        // The bytes are in Slack's store and the completing call — the one
        // that makes them visible — died mid-flight. Whether a message
        // posted is unknown; a blind retry could post it twice.
        let (upload_base, _uploads) =
            crate::daemon::channel_adapter::test_http::serve(vec![(200, "{}".to_string())]);
        let (base, _requests) = crate::daemon::channel_adapter::test_http::serve(vec![
            (
                200,
                format!(r#"{{"ok":true,"upload_url":"{upload_base}/up1","file_id":"F1"}}"#),
            ),
            (500, r#"{"ok":false}"#.to_string()),
        ]);
        let adapter = adapter_with_base(&base).with_blobs(std::sync::Arc::new(
            crate::daemon::channel_adapter::test_http::FixtureBlobs(b"bytes".to_vec()),
        ));
        let outcome = adapter.send(&message_with_files(1)).await;
        assert!(
            matches!(outcome, SendOutcome::NeedsReconciliation { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_failure_after_the_first_file_landed_parks_the_message() {
        // File 0 is fully in Slack's store; file 1's upload-URL request gets
        // rate limited. Retrying the whole message would re-upload file 0,
        // so the row parks for reconciliation instead.
        let (upload_base, _uploads) =
            crate::daemon::channel_adapter::test_http::serve(vec![(200, "{}".to_string())]);
        let (base, _requests) = crate::daemon::channel_adapter::test_http::serve(vec![
            (
                200,
                format!(r#"{{"ok":true,"upload_url":"{upload_base}/up1","file_id":"F1"}}"#),
            ),
            (429, r#"{"ok":false,"error":"ratelimited"}"#.to_string()),
        ]);
        let adapter = adapter_with_base(&base).with_blobs(std::sync::Arc::new(
            crate::daemon::channel_adapter::test_http::FixtureBlobs(b"bytes".to_vec()),
        ));
        let outcome = adapter.send(&message_with_files(2)).await;
        assert!(
            matches!(outcome, SendOutcome::NeedsReconciliation { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_rate_limit_before_any_byte_landed_is_a_plain_retry() {
        let (base, _requests) = crate::daemon::channel_adapter::test_http::serve(vec![(
            429,
            r#"{"ok":false,"error":"ratelimited"}"#.to_string(),
        )]);
        let adapter = adapter_with_base(&base).with_blobs(std::sync::Arc::new(
            crate::daemon::channel_adapter::test_http::FixtureBlobs(b"bytes".to_vec()),
        ));
        let outcome = adapter.send(&message_with_files(1)).await;
        assert!(
            matches!(outcome, SendOutcome::RetryableFailure { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn completion_failures_split_by_whether_slack_may_have_posted() {
        // 429 in either shape is refused before processing: retry.
        assert!(matches!(
            completion_failure(429, Some(1000), &Value::Null),
            Some(SendOutcome::RetryableFailure { .. })
        ));
        assert!(matches!(
            completion_failure(
                200,
                None,
                &serde_json::json!({"ok": false, "error": "ratelimited"})
            ),
            Some(SendOutcome::RetryableFailure { .. })
        ));
        // A server error may have posted the message first: park.
        assert!(matches!(
            completion_failure(502, None, &Value::Null),
            Some(SendOutcome::NeedsReconciliation { .. })
        ));
        // An explicit refusal repeats forever: permanent.
        assert!(matches!(
            completion_failure(
                200,
                None,
                &serde_json::json!({"ok": false, "error": "invalid_auth"})
            ),
            Some(SendOutcome::PermanentFailure { .. })
        ));
        assert!(completion_failure(200, None, &serde_json::json!({"ok": true})).is_none());
    }

    #[test]
    fn a_retry_becomes_reconciliation_once_bytes_have_landed() {
        let retry = SendOutcome::RetryableFailure {
            error: "x".into(),
            retry_after_ms: None,
        };
        assert!(matches!(
            parked_after_upload(retry.clone(), true),
            SendOutcome::NeedsReconciliation { .. }
        ));
        assert!(matches!(
            parked_after_upload(retry, false),
            SendOutcome::RetryableFailure { .. }
        ));
        // Permanent stays permanent — no retry happens either way.
        let permanent = SendOutcome::PermanentFailure { error: "x".into() };
        assert!(matches!(
            parked_after_upload(permanent, true),
            SendOutcome::PermanentFailure { .. }
        ));
    }

    #[test]
    fn a_rate_limited_upload_step_is_a_retry_not_a_permanent_failure() {
        // HTTP 429 with a Retry-After.
        match upload_step_failure(429, Some(2_000), &Value::Null, "upload URL") {
            Some(SendOutcome::RetryableFailure { retry_after_ms, .. }) => {
                assert_eq!(retry_after_ms, Some(2_000))
            }
            other => panic!("unexpected {other:?}"),
        }
        // Slack's 200-with-ok:false convention for the same thing.
        let body = serde_json::json!({ "ok": false, "error": "ratelimited" });
        assert!(matches!(
            upload_step_failure(200, None, &body, "upload URL"),
            Some(SendOutcome::RetryableFailure { .. })
        ));
        // Server errors retry; explicit refusals stay permanent; success is None.
        assert!(matches!(
            upload_step_failure(503, None, &Value::Null, "upload completion"),
            Some(SendOutcome::RetryableFailure { .. })
        ));
        let refused = serde_json::json!({ "ok": false, "error": "invalid_auth" });
        assert!(matches!(
            upload_step_failure(200, None, &refused, "upload URL"),
            Some(SendOutcome::PermanentFailure { .. })
        ));
        let ok = serde_json::json!({ "ok": true });
        assert!(upload_step_failure(200, None, &ok, "upload URL").is_none());
    }

    #[test]
    fn a_missing_scope_is_named_rather_than_echoed() {
        let error = missing_scope_error("missing_scope");
        assert!(error.contains("files:read"), "{error}");
        // Anything else is passed through as Slack said it, not guessed at.
        assert_eq!(
            missing_scope_error("file_not_found"),
            "Slack refused files.info: file_not_found"
        );
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
