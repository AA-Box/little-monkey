//! Telnyx adapter: SMS + Call Control over the v2 REST API, webhooks verified
//! with Ed25519 (`telnyx-signature-ed25519` / `telnyx-timestamp`).
//!
//! # Two keys, not one
//!
//! `TelecomConfig` carries exactly one secret slot (`secret`), which this
//! provider spends on the bearer API key `send_sms`/`place_call`/`hangup`/
//! `probe` need. Webhook verification needs a *different* value — the
//! account's Ed25519 **public** key, which Telnyx issues separately from the
//! API key and which does not belong in the same field: it is not secret (it
//! can only verify, never forge or authenticate a REST call), and packing it
//! into `secret` would make `redact` guess which of two unrelated values a
//! string might contain. It arrives as its own constructor argument instead —
//! see [`TelnyxProvider::new`].
//!
//! # The API key never appears in a diagnostic
//!
//! Every REST call carries `Authorization: Bearer <api key>`. A `reqwest::Error`'s
//! `Display` does not print headers, only the request URL, so the API key
//! cannot leak that way — but nothing stops a future edit from formatting it
//! into an error by hand, so [`TelnyxProvider::redact`] scrubs it anyway, the
//! same discipline every other adapter in this tree uses.
//!
//! # Call Control is commands, not markup
//!
//! Telnyx Voice is not a TwiML-shaped carrier. A ringing call is answered by
//! `POST /v2/calls/{call_control_id}/actions/answer`, and the media stream is
//! an argument of that command (`stream_url`); a call already up is streamed
//! with `actions/streaming_start`. Returning XML to the webhook answers
//! nothing at all — Telnyx reads the response body of a webhook only for its
//! status code. So [`TelnyxProvider::answer_instructions`] is deliberately
//! `None` and [`TelnyxProvider::connect_media`] carries the whole of it.
//!
//! The call this module can act on is identified by `payload.call_control_id`.
//! `data.id` is the id of the *webhook*, and `payload.id` does not exist on a
//! Voice event — using either as the call id leaves every command addressed to
//! nothing.
//!
//! # Call Control's `connection_id`
//!
//! Placing a Call Control call requires a `connection_id` naming which Telnyx
//! application owns the call — a value `TelecomConfig` has no dedicated field
//! for. [`TelecomConfig::carrier_account_id`] ("the account identifier the
//! carrier issues") is reused for it: an operator's Telnyx Call Control
//! Connection ID is exactly that, the carrier-issued identifier this account
//! acts as.

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::Deserialize;

use little_monkey_lib::channels::types::{
    AttachmentKind, AttachmentSource, BoundedMetadata, ChannelAttachment, ChannelConversation,
    ChannelEnvelope, ChannelHealth, ChannelKind, ChannelSender, SendOutcome,
};

use super::super::telecom_store::CallDirection;
use super::{
    AnswerDocument, CallHandle, CallState, TelecomConfig, TelecomEvent, TelecomKind,
    TelecomProvider,
};

const API_BASE: &str = "https://api.telnyx.com/v2";

/// Signatures older than this are refused even if otherwise valid — bounds
/// how long a captured, still-correctly-signed webhook replay stays usable.
const MAX_TIMESTAMP_SKEW_SECS: i64 = 300;

pub struct TelnyxProvider {
    api_key: String,
    from_number: String,
    /// Reused as the Call Control `connection_id`; see the module doc.
    connection_id: String,
    webhook_public_key: Vec<u8>,
    /// Overridable only by this module's own tests.
    base_url: String,
}

impl TelnyxProvider {
    /// `webhook_public_key_b64` is the account's Ed25519 public key exactly
    /// as Telnyx's portal displays it (base64) — see the module doc for why
    /// it is a separate argument rather than a `TelecomConfig` field.
    pub fn new(config: TelecomConfig, webhook_public_key_b64: &str) -> Result<Self, String> {
        let webhook_public_key = STANDARD
            .decode(webhook_public_key_b64)
            .map_err(|_| "malformed Telnyx webhook public key".to_string())?;
        Ok(Self {
            api_key: config.secret,
            from_number: config.from_number,
            connection_id: config.carrier_account_id,
            webhook_public_key,
            base_url: API_BASE.to_string(),
        })
    }

    fn client(&self) -> Result<reqwest::Client, String> {
        little_monkey_lib::egress::hardened()
            .build()
            .map_err(|error| self.redact(error.to_string()))
    }

    /// Scrubs the bearer API key out of any string before it becomes a
    /// `ChannelHealth.last_error`, a `SendOutcome::*` error, or a log line.
    fn redact(&self, message: impl Into<String>) -> String {
        let message = message.into();
        if self.api_key.is_empty() {
            message
        } else {
            message.replace(self.api_key.as_str(), "<redacted>")
        }
    }

    fn messages_url(&self) -> String {
        format!("{}/messages", self.base_url)
    }
    fn calls_url(&self) -> String {
        format!("{}/calls", self.base_url)
    }
    fn hangup_url(&self, call_control_id: &str) -> String {
        format!("{}/calls/{call_control_id}/actions/hangup", self.base_url)
    }
    fn answer_url(&self, call_control_id: &str) -> String {
        format!("{}/calls/{call_control_id}/actions/answer", self.base_url)
    }
    fn streaming_start_url(&self, call_control_id: &str) -> String {
        format!(
            "{}/calls/{call_control_id}/actions/streaming_start",
            self.base_url
        )
    }
    fn phone_numbers_url(&self) -> String {
        format!("{}/phone_numbers?page[size]=1", self.base_url)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

#[async_trait]
impl TelecomProvider for TelnyxProvider {
    fn kind(&self) -> TelecomKind {
        TelecomKind::Telnyx
    }

    /// Telnyx hands out a media URL that needs no credential of ours, so this
    /// is the plain hardened download -- same cap, same redirect policy, no
    /// header. Implemented rather than left to the refusing default: "this
    /// carrier cannot download attachments" would be false.
    async fn fetch_media(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
        crate::daemon::channel_adapter::fetch_url(url, None, max_bytes)
            .await
            .map_err(|error| self.redact(error))
    }

    fn media_stream(&self) -> Option<crate::daemon::call_media::MediaStreamFormat> {
        Some(MEDIA_FORMAT)
    }

    /// TeXML, Telnyx's TwiML-compatible document. Same shape, same verb names,
    /// different host — which is why the two are written out separately rather
    /// than shared: a compatibility layer that silently drifts is worse than two
    /// short strings.
    /// None: see the module doc. Telnyx ignores a webhook's response body, so
    /// a document here would be markup nobody reads and a caller connected to
    /// silence.
    fn answer_instructions(&self, _media_url: &str) -> Option<AnswerDocument> {
        None
    }

    /// Answer a ringing call with the stream attached, or start a stream on a
    /// call that is already up.
    ///
    /// Two commands because Telnyx rejects the wrong one: `answer` on an
    /// answered call and `streaming_start` on a ringing one are both errors,
    /// and an error here is a call nobody can hear.
    async fn connect_media(
        &self,
        provider_call_id: &str,
        media_url: &str,
        answered_already: bool,
        record: bool,
    ) -> Result<(), String> {
        let client = self.client()?;
        let (url, mut body) = if answered_already {
            (
                self.streaming_start_url(provider_call_id),
                serde_json::json!({
                    "stream_url": media_url,
                    "stream_track": "both_tracks",
                    "stream_bidirectional_mode": "rtp",
                    "stream_bidirectional_codec": "PCMU",
                }),
            )
        } else {
            (
                self.answer_url(provider_call_id),
                serde_json::json!({
                    "stream_url": media_url,
                    "stream_track": "both_tracks",
                    // Audio only flows both ways on an RTP bidirectional
                    // stream, and only µ-law matches what the rest of this
                    // pipeline speaks.
                    "stream_bidirectional_mode": "rtp",
                    "stream_bidirectional_codec": "PCMU",
                }),
            )
        };
        // A command id makes the retry of a dropped request a no-op at Telnyx
        // rather than a second answer.
        body["command_id"] = serde_json::json!(telnyx_command_id(
            if answered_already { "stream" } else { "answer" },
            provider_call_id
        ));
        if record && !answered_already {
            body["record"] = serde_json::json!("record-from-answer");
            body["record_channels"] = serde_json::json!("single");
        }
        let request = client.post(url).bearer_auth(&self.api_key).json(&body);
        let response = little_monkey_lib::egress::send(request)
            .await
            .map_err(|error| self.redact(format!("Could not reach Telnyx: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(self.redact(format!("Telnyx returned {status} connecting the call")));
        }
        Ok(())
    }

    async fn probe(&self) -> ChannelHealth {
        let now = now_ms();
        if self.api_key.trim().is_empty() {
            return ChannelHealth::error(now, "Telnyx requires an API key");
        }
        let client = match self.client() {
            Ok(client) => client,
            Err(error) => return ChannelHealth::error(now, error),
        };
        let request = client
            .get(self.phone_numbers_url())
            .bearer_auth(&self.api_key);
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => {
                return ChannelHealth::error(
                    now,
                    self.redact(format!("Could not reach Telnyx: {error}")),
                )
            }
        };
        let status = response.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return ChannelHealth::error(now, "Telnyx rejected the API key");
        }
        if !status.is_success() {
            return ChannelHealth::error(
                now,
                self.redact(format!("Telnyx returned {status} probing phone numbers")),
            );
        }
        ChannelHealth::connected(now, Some(self.from_number.clone()))
    }

    async fn send_sms(
        &self,
        to_number: &str,
        text: &str,
        media_urls: &[String],
        idempotency_key: &str,
    ) -> SendOutcome {
        let _ = idempotency_key; // no caller-supplied dedupe key on /v2/messages
        let client = match self.client() {
            Ok(client) => client,
            Err(error) => return SendOutcome::PermanentFailure { error },
        };
        let mut body = serde_json::json!({
            "from": self.from_number,
            "to": to_number,
            "text": text,
        });
        if !media_urls.is_empty() {
            body["media_urls"] = serde_json::json!(media_urls);
        }
        let request = client
            .post(self.messages_url())
            .bearer_auth(&self.api_key)
            .json(&body);
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => {
                return if error.is_connect() {
                    SendOutcome::RetryableFailure {
                        error: self.redact(format!("Could not connect to Telnyx: {error}")),
                        retry_after_ms: None,
                    }
                } else {
                    SendOutcome::NeedsReconciliation {
                        error: self.redact(format!("Telnyx send outcome unknown: {error}")),
                    }
                };
            }
        };
        let status = response.status();
        if status.as_u16() == 429 {
            let retry_after_ms = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<i64>().ok())
                .map(|seconds| seconds * 1000);
            return SendOutcome::RetryableFailure {
                error: "Telnyx rate-limited the request (429)".to_string(),
                retry_after_ms,
            };
        }
        let body_text = response.text().await.unwrap_or_default();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return SendOutcome::PermanentFailure {
                error: self.redact(format!("Telnyx returned {status} for /v2/messages")),
            };
        }
        if !status.is_success() {
            return SendOutcome::PermanentFailure {
                error: self.redact(format!("Telnyx returned {status} for /v2/messages")),
            };
        }
        match serde_json::from_str::<TelnyxMessageEnvelope>(&body_text) {
            Ok(parsed) => SendOutcome::Sent {
                provider_message_id: Some(parsed.data.id),
            },
            Err(_) => SendOutcome::NeedsReconciliation {
                error: "Telnyx accepted the message but returned an unparseable response"
                    .to_string(),
            },
        }
    }

    async fn place_call(
        &self,
        to_number: &str,
        answer_url: &str,
        record: bool,
        idempotency_key: &str,
    ) -> Result<CallHandle, String> {
        let client = self.client()?;
        let mut body = serde_json::json!({
            "connection_id": self.connection_id,
            // Telnyx ignores a second command carrying a `command_id` it has
            // already seen. That is the difference between a retried run
            // reaching the carrier twice and it ringing somebody's phone
            // twice, so the key is the call row's own — stable across the
            // retry, unique across calls.
            "command_id": telnyx_command_id("dial", idempotency_key),
            "to": to_number,
            "from": self.from_number,
            // Every event for this call comes back here. Telnyx has one
            // webhook URL per call rather than a separate status URL, which is
            // why its normalizer reads the event type rather than the path.
            //
            // No stream fields here: `stream_url` names a socket that is
            // scoped to one call, and the call does not exist yet. The stream
            // is attached by `connect_media` when Telnyx says the far end
            // answered.
            "webhook_url": answer_url,
            "webhook_url_method": "POST",
        });
        if record {
            body["record"] = serde_json::json!("record-from-answer");
            body["record_channels"] = serde_json::json!("single");
        }
        let request = client
            .post(self.calls_url())
            .bearer_auth(&self.api_key)
            .json(&body);
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => {
                return if error.is_connect() {
                    Err(self.redact(format!("Could not connect to Telnyx: {error}")))
                } else {
                    // The POST may already have reached Telnyx before the
                    // transport failed; a duplicated outbound call cannot be
                    // undone by a retry.
                    Ok(CallHandle {
                        provider_call_id: String::new(),
                        state: CallState::NeedsReconciliation,
                    })
                };
            }
        };
        let status = response.status();
        let body_text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(self.redact(format!("Telnyx returned {status} placing the call")));
        }
        match serde_json::from_str::<TelnyxCallEnvelope>(&body_text) {
            Ok(parsed) => Ok(CallHandle {
                provider_call_id: parsed.data.call_control_id,
                state: CallState::Queued,
            }),
            Err(_) => Ok(CallHandle {
                provider_call_id: String::new(),
                state: CallState::NeedsReconciliation,
            }),
        }
    }

    async fn hangup(&self, provider_call_id: &str) -> Result<(), String> {
        let client = self.client()?;
        let request = client
            .post(self.hangup_url(provider_call_id))
            .bearer_auth(&self.api_key)
            .json(&serde_json::json!({
                "command_id": telnyx_command_id("hangup", provider_call_id),
            }));
        let response = little_monkey_lib::egress::send(request)
            .await
            .map_err(|error| self.redact(format!("Telnyx hangup outcome unknown: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(self.redact(format!("Telnyx returned {status} ending the call")));
        }
        Ok(())
    }

    fn verify_webhook(
        &self,
        _path: &str,
        headers: &[(String, String)],
        body: &[u8],
        now_ms: i64,
    ) -> Result<TelecomEvent, String> {
        // Telnyx has one webhook URL per call rather than a separate status
        // URL, and its signature covers `timestamp|body` rather than a URL, so
        // the path decides nothing here: the event type does.
        let signature_b64 = header(headers, "telnyx-signature-ed25519")
            .ok_or_else(|| "missing telnyx-signature-ed25519 header".to_string())?;
        let timestamp_str = header(headers, "telnyx-timestamp")
            .ok_or_else(|| "missing telnyx-timestamp header".to_string())?;
        let timestamp: i64 = timestamp_str
            .parse()
            .map_err(|_| "malformed telnyx-timestamp header".to_string())?;
        let skew = (now_ms / 1000 - timestamp).abs();
        if skew > MAX_TIMESTAMP_SKEW_SECS {
            return Err("Telnyx webhook timestamp is stale".to_string());
        }
        let signature = STANDARD
            .decode(signature_b64)
            .map_err(|_| "malformed telnyx-signature-ed25519 header".to_string())?;
        let mut signed_message = Vec::with_capacity(timestamp_str.len() + 1 + body.len());
        signed_message.extend_from_slice(timestamp_str.as_bytes());
        signed_message.push(b'|');
        signed_message.extend_from_slice(body);
        let public_key = ring::signature::UnparsedPublicKey::new(
            &ring::signature::ED25519,
            &self.webhook_public_key,
        );
        public_key
            .verify(&signed_message, &signature)
            .map_err(|_| "Telnyx signature verification failed".to_string())?;
        let parsed: TelnyxWebhookBody = serde_json::from_slice(body)
            .map_err(|_| "Telnyx webhook body is not valid JSON".to_string())?;
        normalize_telnyx_event(&parsed.data, now_ms)
    }
}

/// A stable `command_id` for one Telnyx call command.
///
/// Telnyx wants a UUID, and the values this derives from are not — so they are
/// hashed into one. Deterministic, so the retry of a command produces the id
/// the first attempt used and Telnyx drops it; namespaced by `verb`, so a dial
/// and the hangup that ends it never collide.
fn telnyx_command_id(verb: &str, key: &str) -> String {
    let digest = ring::digest::digest(
        &ring::digest::SHA256,
        format!("little-monkey:{verb}:{key}").as_bytes(),
    );
    let hex: String = digest
        .as_ref()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn normalize_telnyx_event(data: &TelnyxData, received_at_ms: i64) -> Result<TelecomEvent, String> {
    // Voice and messaging events disagree about the shape of `payload`, so the
    // event type decides which one to read. A voice payload parsed as a
    // messaging one yields an empty call id, and a command addressed to an
    // empty id reaches nothing.
    let voice = || -> Result<TelnyxCallPayload, String> {
        serde_json::from_value::<TelnyxCallPayload>(data.payload.clone())
            .map_err(|error| format!("Telnyx voice payload could not be read: {error}"))
    };
    match data.event_type.as_str() {
        "message.received" => {
            let payload: TelnyxPayload = serde_json::from_value(data.payload.clone())
                .map_err(|error| format!("Telnyx message payload could not be read: {error}"))?;
            let from = super::normalize_e164(
                payload
                    .from
                    .as_ref()
                    .map(|address| address.phone_number.as_str())
                    .ok_or_else(|| "Telnyx inbound message is missing from".to_string())?,
            );
            let to = super::normalize_e164(
                payload
                    .to
                    .as_ref()
                    .and_then(|list| list.first())
                    .map(|address| address.phone_number.as_str())
                    .unwrap_or_default(),
            );
            let mut metadata = BoundedMetadata::new();
            metadata.insert("to_number", to);
            let attachments = payload
                .media
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .map(|media| ChannelAttachment {
                    stored_artifact_id: None,
                    text_excerpt: None,
                    fetch_error: None,
                    provider_id: None,
                    kind: media
                        .content_type
                        .as_deref()
                        .map(AttachmentKind::from_mime)
                        .unwrap_or(AttachmentKind::Other),
                    filename: None,
                    mime_type: media.content_type.clone(),
                    declared_size_bytes: None,
                    stored_size_bytes: None,
                    source: AttachmentSource::Url {
                        url: media.url.clone(),
                    },
                })
                .collect();
            let envelope = ChannelEnvelope {
                account_id: String::new(),
                kind: ChannelKind::Sms,
                provider_event_id: payload.id.clone(),
                provider_message_id: None,
                conversation: ChannelConversation::direct(from.clone()),
                sender: ChannelSender::new(from),
                text: payload.text.clone().unwrap_or_default(),
                attachments,
                reply_to_provider_id: None,
                mentions_self: false,
                received_at_ms,
                metadata,
            };
            Ok(TelecomEvent::InboundSms(Box::new(envelope)))
        }
        // A call arriving at the operator's number. The answering policy
        // decides what happens; Telnyx is answered with a command, not with
        // this webhook's response body.
        "call.initiated" if voice()?.direction.as_deref() == Some("incoming") => {
            let payload = voice()?;
            Ok(TelecomEvent::AnswerRequest {
                provider_call_id: payload.call_control_id.clone(),
                request_id: None,
                direction: CallDirection::Inbound,
                from_number: super::normalize_e164(payload.from.as_deref().unwrap_or_default()),
                to_number: super::normalize_e164(payload.to.as_deref().unwrap_or_default()),
                received_at_ms,
            })
        }
        "call.initiated" => Ok(TelecomEvent::CallProgress {
            provider_call_id: voice()?.call_control_id,
            state: CallState::Queued,
            detail: None,
        }),
        // The far end picked up. On an outbound call this is the moment the
        // stream has to be attached, so it is an answer request rather than
        // plain progress; the worker connects it and marks the call up.
        "call.answered" => {
            let payload = voice()?;
            if payload.direction.as_deref() == Some("incoming") {
                return Ok(TelecomEvent::CallProgress {
                    provider_call_id: payload.call_control_id,
                    state: CallState::InProgress,
                    detail: None,
                });
            }
            Ok(TelecomEvent::AnswerRequest {
                provider_call_id: payload.call_control_id.clone(),
                request_id: None,
                direction: CallDirection::Outbound,
                from_number: super::normalize_e164(payload.from.as_deref().unwrap_or_default()),
                to_number: super::normalize_e164(payload.to.as_deref().unwrap_or_default()),
                received_at_ms,
            })
        }
        "call.hangup" => {
            let payload = voice()?;
            let state = match payload.hangup_cause.as_deref() {
                Some("normal_clearing") | None => CallState::Completed,
                Some(_) => CallState::Failed,
            };
            Ok(TelecomEvent::CallProgress {
                provider_call_id: payload.call_control_id,
                state,
                detail: payload.hangup_cause,
            })
        }
        "message.finalized" => {
            let payload: TelnyxPayload = serde_json::from_value(data.payload.clone())
                .map_err(|error| format!("Telnyx message payload could not be read: {error}"))?;
            let failed = payload
                .to
                .as_ref()
                .map(|list| {
                    list.iter()
                        .any(|address| address.status.as_deref() == Some("delivery_failed"))
                })
                .unwrap_or(false);
            Ok(TelecomEvent::SmsStatus {
                provider_message_id: payload.id.clone(),
                delivered: !failed,
                error: if failed {
                    Some("Telnyx reported delivery_failed".to_string())
                } else {
                    None
                },
            })
        }
        _ => Ok(TelecomEvent::Ignored),
    }
}

#[derive(Debug, Deserialize)]
struct TelnyxAddress {
    phone_number: String,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelnyxMedia {
    url: String,
    #[serde(default)]
    content_type: Option<String>,
}

/// A Voice event's payload.
///
/// Deliberately separate from the messaging payload above: on a Voice webhook
/// `from` and `to` are plain strings, there is no `payload.id`, and the only
/// identifier any command can be addressed to is `call_control_id`. Sharing one
/// struct with messaging is what made Voice events deserialize into an empty
/// call id — and the tests did not catch it because they were written from the
/// struct rather than from a real payload.
#[derive(Debug, Deserialize)]
struct TelnyxCallPayload {
    #[serde(default)]
    call_control_id: String,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    /// `incoming` for a call arriving at the operator's number, `outgoing` for
    /// one this machine placed.
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    hangup_cause: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelnyxPayload {
    #[serde(default)]
    id: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    from: Option<TelnyxAddress>,
    #[serde(default)]
    to: Option<Vec<TelnyxAddress>>,
    #[serde(default)]
    media: Option<Vec<TelnyxMedia>>,
}

#[derive(Debug, Deserialize)]
struct TelnyxData {
    event_type: String,
    /// Kept unparsed so one webhook body can carry either shape: messaging and
    /// voice disagree about what `payload` is, and only `event_type` says
    /// which to expect.
    payload: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct TelnyxWebhookBody {
    data: TelnyxData,
}

#[derive(Debug, Deserialize)]
struct TelnyxMessageData {
    id: String,
}

#[derive(Debug, Deserialize)]
struct TelnyxMessageEnvelope {
    data: TelnyxMessageData,
}

#[derive(Debug, Deserialize)]
struct TelnyxCallData {
    call_control_id: String,
}

#[derive(Debug, Deserialize)]
struct TelnyxCallEnvelope {
    data: TelnyxCallData,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The path a carrier's answer request arrives on, which is what its
    /// signature covers. Built from the shared function rather than typed, so
    /// a test cannot agree with a verifier that has drifted from the route.
    const ANSWER_PATH: &str = "/v1/telecom/acct-1";
    /// Where a carrier reports what became of a message or a call.
    #[allow(dead_code)]
    const STATUS_PATH: &str = "/v1/telecom/acct-1/status";

    /// Deterministic Ed25519 keypair: a fixed 32-byte seed rather than
    /// `SystemRandom`, so a signature fixture computed once in a test stays
    /// reproducible.
    fn keypair() -> ring::signature::Ed25519KeyPair {
        ring::signature::Ed25519KeyPair::from_seed_unchecked(&[7u8; 32]).unwrap()
    }

    fn public_key_b64(pair: &ring::signature::Ed25519KeyPair) -> String {
        use ring::signature::KeyPair as _;
        STANDARD.encode(pair.public_key().as_ref())
    }

    fn config() -> TelecomConfig {
        TelecomConfig {
            account_id: "acct-1".to_string(),
            kind: TelecomKind::Telnyx,
            carrier_account_id: "conn-123".to_string(),
            from_number: "+15550001111".to_string(),
            secret: "test-api-key".to_string(),
            public_base_url: None,
            webhook_public_key: None,
        }
    }

    fn provider(pair: &ring::signature::Ed25519KeyPair) -> TelnyxProvider {
        TelnyxProvider::new(config(), &public_key_b64(pair)).unwrap()
    }

    fn signed_headers(
        pair: &ring::signature::Ed25519KeyPair,
        timestamp: i64,
        body: &[u8],
    ) -> Vec<(String, String)> {
        let mut message = Vec::new();
        message.extend_from_slice(timestamp.to_string().as_bytes());
        message.push(b'|');
        message.extend_from_slice(body);
        let signature = pair.sign(&message);
        vec![
            (
                "telnyx-signature-ed25519".to_string(),
                STANDARD.encode(signature.as_ref()),
            ),
            ("telnyx-timestamp".to_string(), timestamp.to_string()),
        ]
    }

    #[test]
    fn a_correct_signature_verifies_and_normalizes_an_inbound_sms() {
        let pair = keypair();
        let provider = provider(&pair);
        let body = br#"{"data":{"event_type":"message.received","payload":{"id":"msg-1","text":"hi","from":{"phone_number":"+15551230000"},"to":[{"phone_number":"+15550001111"}]}}}"#;
        let now = 1_700_000_000_000;
        let headers = signed_headers(&pair, now / 1000, body);
        let event = provider
            .verify_webhook(ANSWER_PATH, &headers, body, now)
            .expect("verifies");
        match event {
            TelecomEvent::InboundSms(envelope) => {
                assert_eq!(envelope.provider_event_id, "msg-1");
                assert_eq!(envelope.sender.sender_id, "+15551230000");
                assert_eq!(envelope.text, "hi");
            }
            other => panic!("expected InboundSms, got {other:?}"),
        }
    }

    #[test]
    fn a_tampered_body_fails_verification() {
        let pair = keypair();
        let provider = provider(&pair);
        let body = br#"{"data":{"event_type":"message.received","payload":{"id":"msg-1","from":{"phone_number":"+1"}}}}"#;
        let now = 1_700_000_000_000;
        let headers = signed_headers(&pair, now / 1000, body);
        let tampered = br#"{"data":{"event_type":"message.received","payload":{"id":"msg-2","from":{"phone_number":"+1"}}}}"#;
        assert!(provider
            .verify_webhook(ANSWER_PATH, &headers, tampered, now)
            .is_err());
    }

    #[test]
    fn a_wrong_key_fails_verification() {
        let pair = keypair();
        let other_pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(&[9u8; 32]).unwrap();
        let provider = provider(&other_pair);
        let body = br#"{"data":{"event_type":"message.received","payload":{"id":"msg-1","from":{"phone_number":"+1"}}}}"#;
        let now = 1_700_000_000_000;
        let headers = signed_headers(&pair, now / 1000, body);
        assert!(provider
            .verify_webhook(ANSWER_PATH, &headers, body, now)
            .is_err());
    }

    #[test]
    fn a_stale_timestamp_fails_even_with_a_correct_signature() {
        let pair = keypair();
        let provider = provider(&pair);
        let body = br#"{"data":{"event_type":"message.received","payload":{"id":"msg-1","from":{"phone_number":"+1"}}}}"#;
        let now = 1_700_000_000_000;
        let stale_timestamp = now / 1000 - (MAX_TIMESTAMP_SKEW_SECS + 60);
        let headers = signed_headers(&pair, stale_timestamp, body);
        let error = provider
            .verify_webhook(ANSWER_PATH, &headers, body, now)
            .unwrap_err();
        assert!(error.contains("stale"), "{error}");
    }

    #[test]
    fn a_missing_signature_header_is_rejected() {
        let pair = keypair();
        let provider = provider(&pair);
        let headers = vec![("telnyx-timestamp".to_string(), "1700000000".to_string())];
        assert!(provider
            .verify_webhook(ANSWER_PATH, &headers, b"{}", 1_700_000_000_000)
            .is_err());
    }

    #[test]
    fn a_missing_timestamp_header_is_rejected() {
        let pair = keypair();
        let provider = provider(&pair);
        let headers = vec![(
            "telnyx-signature-ed25519".to_string(),
            STANDARD.encode([0u8; 64]),
        )];
        assert!(provider
            .verify_webhook(ANSWER_PATH, &headers, b"{}", 1_700_000_000_000)
            .is_err());
    }

    /// A Telnyx Voice webhook, written from Telnyx's own published shape
    /// rather than from this module's structs: `payload.call_control_id` is
    /// the only id a command can be addressed to, `from` and `to` are plain
    /// strings, and `data.id` identifies the webhook, not the call.
    fn voice_body(event_type: &str, extra: &str) -> String {
        let payload = format!(
            r#"{{"call_control_id":"v3:ccid-1","call_leg_id":"leg-1","from":"+15551230000","to":"+15550001111"{extra}}}"#
        );
        format!(
            r#"{{"data":{{"event_type":"{event_type}","id":"webhook-evt-1","payload":{payload}}}}}"#
        )
    }

    #[test]
    fn call_status_vocabulary_maps_to_call_states() {
        let pair = keypair();
        let provider = provider(&pair);
        let now = 1_700_000_000_000;
        for (event_type, extra, hangup_cause, expected) in [
            (
                "call.answered",
                r#","direction":"incoming""#,
                None,
                CallState::InProgress,
            ),
            (
                "call.hangup",
                r#","hangup_cause":"normal_clearing""#,
                Some("normal_clearing"),
                CallState::Completed,
            ),
            (
                "call.hangup",
                r#","hangup_cause":"call_rejected""#,
                Some("call_rejected"),
                CallState::Failed,
            ),
        ] {
            let body = voice_body(event_type, extra);
            let headers = signed_headers(&pair, now / 1000, body.as_bytes());

            let event = provider
                .verify_webhook(ANSWER_PATH, &headers, body.as_bytes(), now)
                .expect("verifies");

            assert_eq!(
                event,
                TelecomEvent::CallProgress {
                    // The controllable id, not `data.id` and not a `payload.id`
                    // that Voice events do not carry at all.
                    provider_call_id: "v3:ccid-1".to_string(),
                    state: expected,
                    detail: hangup_cause.map(|value| value.to_string()),
                }
            );
        }
    }

    #[test]
    fn an_incoming_call_is_an_answer_request_addressed_to_its_control_id() {
        let pair = keypair();
        let provider = provider(&pair);
        let now = 1_700_000_000_000;
        let body = voice_body(
            "call.initiated",
            r#","direction":"incoming","state":"parked""#,
        );
        let headers = signed_headers(&pair, now / 1000, body.as_bytes());

        let event = provider
            .verify_webhook(ANSWER_PATH, &headers, body.as_bytes(), now)
            .expect("verifies");

        assert_eq!(
            event,
            TelecomEvent::AnswerRequest {
                provider_call_id: "v3:ccid-1".to_string(),
                request_id: None,
                direction: CallDirection::Inbound,
                from_number: "+15551230000".to_string(),
                to_number: "+15550001111".to_string(),
                received_at_ms: now,
            }
        );
    }

    #[test]
    fn an_outbound_call_being_picked_up_is_an_answer_request_not_plain_progress() {
        let pair = keypair();
        let provider = provider(&pair);
        let now = 1_700_000_000_000;
        let body = voice_body("call.answered", r#","direction":"outgoing""#);
        let headers = signed_headers(&pair, now / 1000, body.as_bytes());

        let event = provider
            .verify_webhook(ANSWER_PATH, &headers, body.as_bytes(), now)
            .expect("verifies");

        assert!(
            matches!(
                event,
                TelecomEvent::AnswerRequest {
                    ref provider_call_id,
                    direction: CallDirection::Outbound,
                    ..
                } if provider_call_id == "v3:ccid-1"
            ),
            "as plain progress the stream is never started and whoever picked \
             up hears nothing: {event:?}"
        );
    }

    #[test]
    fn a_voice_payload_is_not_read_as_a_messaging_one() {
        let pair = keypair();
        let provider = provider(&pair);
        let now = 1_700_000_000_000;
        // The real shape: `from` is a string here and an object on a messaging
        // event. Read through the messaging struct this deserializes to an
        // empty call id, and a command addressed to an empty id reaches
        // nothing.
        let body = voice_body("call.hangup", r#","hangup_cause":"normal_clearing""#);
        let headers = signed_headers(&pair, now / 1000, body.as_bytes());

        let event = provider
            .verify_webhook(ANSWER_PATH, &headers, body.as_bytes(), now)
            .expect("verifies");

        match event {
            TelecomEvent::CallProgress {
                provider_call_id, ..
            } => assert_eq!(provider_call_id, "v3:ccid-1"),
            other => panic!("expected progress, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_ringing_call_is_answered_by_command_with_the_stream_attached() {
        use crate::daemon::channel_adapter::test_http;
        let (base, requests) = test_http::serve(vec![(200, r#"{"data":{}}"#.to_string())]);
        let provider = provider_with_base(&base);

        provider
            .connect_media("v3:ccid-1", "wss://ops.example.com/media", false, false)
            .await
            .expect("answered");

        let request = String::from_utf8(requests.recv().expect("a request")).expect("utf8");
        // Returning XML to a Telnyx webhook answers nothing: the call is
        // answered by this command, and the stream is an argument of it.
        assert!(
            request.contains("/calls/v3:ccid-1/actions/answer"),
            "{request}"
        );
        assert!(
            request.contains(r#""stream_url":"wss://ops.example.com/media""#),
            "`webhook_url` is where events go; `stream_url` is where the audio goes: {request}"
        );
        assert!(
            request.contains(r#""stream_bidirectional_mode":"rtp""#),
            "{request}"
        );
    }

    #[tokio::test]
    async fn a_call_already_up_gets_a_stream_started_rather_than_answered_again() {
        use crate::daemon::channel_adapter::test_http;
        let (base, requests) = test_http::serve(vec![(200, r#"{"data":{}}"#.to_string())]);
        let provider = provider_with_base(&base);

        provider
            .connect_media("v3:ccid-2", "wss://ops.example.com/media", true, false)
            .await
            .expect("streaming");

        let request = String::from_utf8(requests.recv().expect("a request")).expect("utf8");
        // Answering an answered call is an error at Telnyx, and an error here
        // is an outbound call nobody can hear.
        assert!(
            request.contains("/calls/v3:ccid-2/actions/streaming_start"),
            "{request}"
        );
        assert!(request.contains(r#""stream_url""#), "{request}");
    }

    #[tokio::test]
    async fn a_dial_carries_no_stream_url_because_the_call_does_not_exist_yet() {
        use crate::daemon::channel_adapter::test_http;
        let (base, requests) = test_http::serve(vec![(
            200,
            r#"{"data":{"call_control_id":"v3:ccid-3"}}"#.to_string(),
        )]);
        let provider = provider_with_base(&base);

        provider
            .place_call(
                "+15551230000",
                "https://ops.example.com/v1/telecom/acct-1",
                false,
                "k",
            )
            .await
            .expect("placed");

        let request = String::from_utf8(requests.recv().expect("a request")).expect("utf8");
        assert!(request.contains(r#""webhook_url""#), "{request}");
        // The media socket's token is scoped to one call, and the call has no
        // id until Telnyx answers this request. The stream is attached on
        // `call.answered` instead.
        assert!(
            !request.contains(r#""stream_url""#),
            "a stream_url here would name a socket for a call that does not exist: {request}"
        );
    }

    #[test]
    fn a_retried_dial_carries_the_command_id_the_first_attempt_used() {
        // Telnyx drops a command whose `command_id` it has already seen, which
        // is the only thing between a retried run and a second ring at
        // somebody's phone. Same call row, same id; a different call, a
        // different id.
        let first = telnyx_command_id("dial", "outbound:job-1:+15551230000");
        assert_eq!(
            first,
            telnyx_command_id("dial", "outbound:job-1:+15551230000")
        );
        assert_ne!(
            first,
            telnyx_command_id("dial", "outbound:job-2:+15551230000")
        );
        // The hangup that ends this call must not be mistaken for the dial.
        assert_ne!(
            first,
            telnyx_command_id("hangup", "outbound:job-1:+15551230000")
        );
        // Telnyx wants a UUID shape.
        let parts: Vec<usize> = first.split('-').map(str::len).collect();
        assert_eq!(parts, vec![8, 4, 4, 4, 12], "{first}");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    #[test]
    fn a_finalized_message_that_never_arrived_says_so() {
        let pair = keypair();
        let provider = provider(&pair);
        let body = br#"{"data":{"event_type":"message.finalized","payload":{"id":"msg-9","to":[{"phone_number":"+15551230000","status":"delivery_failed"}]}}}"#;
        let now = 1_700_000_000_000;
        let headers = signed_headers(&pair, now / 1000, body);

        let event = provider
            .verify_webhook(ANSWER_PATH, &headers, body, now)
            .expect("verifies");

        match event {
            TelecomEvent::SmsStatus {
                provider_message_id,
                delivered,
                error,
            } => {
                assert_eq!(provider_message_id, "msg-9");
                assert!(!delivered);
                assert!(error.is_some());
            }
            other => panic!("expected a delivery receipt, got {other:?}"),
        }
    }

    #[test]
    fn an_uninteresting_event_is_ignored() {
        let pair = keypair();
        let provider = provider(&pair);
        let body =
            br#"{"data":{"event_type":"call.machine.detection.ended","payload":{"id":"call-1"}}}"#;
        let now = 1_700_000_000_000;
        let headers = signed_headers(&pair, now / 1000, body);
        let event = provider
            .verify_webhook(ANSWER_PATH, &headers, body, now)
            .expect("verifies");
        assert_eq!(event, TelecomEvent::Ignored);
    }

    #[test]
    fn redact_scrubs_the_api_key() {
        let pair = keypair();
        let provider = provider(&pair);
        let rendered = provider.redact(format!("Bearer {}", provider.api_key));
        assert!(!rendered.contains(provider.api_key.as_str()));
        assert!(rendered.contains("<redacted>"));
    }

    // --- outbound mapping over a real loopback fixture ------------------

    fn serve_once(status: &str, body: &str) -> String {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut stream, _)) = listener.accept() {
                let mut scratch = [0u8; 4096];
                let _ = stream.read(&mut scratch);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn provider_with_base(base_url: &str) -> TelnyxProvider {
        let pair = keypair();
        let mut provider = provider(&pair);
        provider.base_url = base_url.to_string();
        provider
    }

    #[tokio::test]
    async fn send_sms_maps_a_success_response_to_sent() {
        let base = serve_once("200 OK", r#"{"data":{"id":"msg-abc"}}"#);
        let provider = provider_with_base(&base);
        let outcome = provider.send_sms("+15551230000", "hi", &[], "idem-1").await;
        assert_eq!(
            outcome,
            SendOutcome::Sent {
                provider_message_id: Some("msg-abc".to_string())
            }
        );
    }

    #[tokio::test]
    async fn send_sms_maps_429_to_retryable() {
        let base = serve_once("429 Too Many Requests", "{}");
        let provider = provider_with_base(&base);
        match provider.send_sms("+15551230000", "hi", &[], "idem-1").await {
            SendOutcome::RetryableFailure { .. } => {}
            other => panic!("expected RetryableFailure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_sms_maps_403_to_permanent() {
        let base = serve_once("403 Forbidden", "{}");
        let provider = provider_with_base(&base);
        match provider.send_sms("+15551230000", "hi", &[], "idem-1").await {
            SendOutcome::PermanentFailure { .. } => {}
            other => panic!("expected PermanentFailure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn hangup_succeeds_on_a_200_response() {
        let base = serve_once("200 OK", "{}");
        let provider = provider_with_base(&base);
        assert!(provider.hangup("call-1").await.is_ok());
    }

    #[tokio::test]
    async fn hangup_reports_a_non_success_status() {
        let base = serve_once("404 Not Found", "{}");
        let provider = provider_with_base(&base);
        assert!(provider.hangup("call-1").await.is_err());
    }

    #[tokio::test]
    async fn send_sms_maps_a_dropped_connection_to_needs_reconciliation() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                drop(stream);
            }
        });
        let provider = provider_with_base(&format!("http://127.0.0.1:{port}"));
        match provider.send_sms("+15551230000", "hi", &[], "idem-1").await {
            SendOutcome::NeedsReconciliation { .. } => {}
            other => panic!("expected NeedsReconciliation, got {other:?}"),
        }
    }
}

/// How this carrier's media stream is shaped. See
/// [`crate::daemon::call_media::MediaStreamFormat`] for why each field exists.
const MEDIA_FORMAT: crate::daemon::call_media::MediaStreamFormat =
    crate::daemon::call_media::MediaStreamFormat {
        stream_id_path: &["stream_id"],
        outbound_chunk_ms: 1_000,
    };

impl crate::daemon::call_media::MediaFrameCodec for TelnyxProvider {
    fn format(&self) -> crate::daemon::call_media::MediaStreamFormat {
        MEDIA_FORMAT
    }

    fn encode_clear_frame(&self, stream_id: &str) -> String {
        serde_json::json!({
            "event": "clear",
            "stream_id": stream_id,
        })
        .to_string()
    }

    fn encode_media_frame(&self, payload_b64: &str, stream_id: &str) -> String {
        // Telnyx accepts at most one payload per second on a bidirectional
        // RTP stream, so frames here are a second of audio rather than 20 ms.
        serde_json::json!({
            "event": "media",
            "stream_id": stream_id,
            "media": { "payload": payload_b64 },
        })
        .to_string()
    }
}
