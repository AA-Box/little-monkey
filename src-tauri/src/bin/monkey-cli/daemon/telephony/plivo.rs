//! Plivo adapter: SMS + voice over the v1 REST API, webhooks verified with
//! the V3 signature scheme (`X-Plivo-Signature-V3` / `X-Plivo-Signature-V3-Nonce`).
//!
//! # The Auth ID and Auth Token never appear in a diagnostic
//!
//! HTTP Basic auth carries both on every REST call, and the Auth ID is also
//! baked into every request URL (`.../Account/{AuthId}/...`). Same shape as
//! the Twilio adapter's problem and the same fix: [`PlivoProvider::redact`]
//! scrubs both out of anything built from a response or a transport error
//! before it leaves this module.
//!
//! # Reconstructing the signed URL
//!
//! Like Twilio's, Plivo's signature covers a URL this module is never handed
//! directly — `verify_webhook` receives only headers, a body and a clock
//! reading (see `mod.rs`'s trait doc). It is [`super::callback_path`] under
//! [`TelecomConfig::public_base_url`] — the same function the listener routes
//! on and the setup UI tells the operator to paste into their Plivo
//! application, so the three cannot drift apart. The base is never a `Host` or
//! `X-Forwarded-*` header.
//!
//! # Multiple signatures, any one valid
//!
//! Plivo rotates signing keys and, during rotation, sends more than one
//! comma-separated signature so a webhook verifies against either the old or
//! the new key. Each candidate is checked with `ring::hmac::verify`, which is
//! itself constant-time; accepting on the first match does not weaken that,
//! since none of the candidates being tried is the secret.

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

const API_BASE: &str = "https://api.plivo.com/v1";

pub struct PlivoProvider {
    /// This app's own id for the account, which is what its callback path is
    /// keyed by. See the module doc's "Reconstructing the signed URL".
    account_id: String,
    auth_id: String,
    auth_token: String,
    from_number: String,
    public_base_url: Option<String>,
    /// Overridable only by this module's own tests.
    base_url: String,
}

impl PlivoProvider {
    pub fn new(config: TelecomConfig) -> Self {
        Self {
            account_id: config.account_id,
            auth_id: config.carrier_account_id,
            auth_token: config.secret,
            from_number: config.from_number,
            public_base_url: config.public_base_url,
            base_url: API_BASE.to_string(),
        }
    }

    fn client(&self) -> Result<reqwest::Client, String> {
        little_monkey_lib::egress::hardened()
            .build()
            .map_err(|error| self.redact(error.to_string()))
    }

    /// Scrubs the Auth Token and Auth ID out of any string before it becomes
    /// a `ChannelHealth.last_error`, a `SendOutcome::*` error, or a log line.
    fn redact(&self, message: impl Into<String>) -> String {
        let mut message = message.into();
        if !self.auth_token.is_empty() {
            message = message.replace(self.auth_token.as_str(), "<redacted>");
        }
        if !self.auth_id.is_empty() {
            message = message.replace(self.auth_id.as_str(), "<redacted>");
        }
        message
    }

    fn account_url(&self) -> String {
        format!("{}/Account/{}/", self.base_url, self.auth_id)
    }
    fn message_url(&self) -> String {
        format!("{}/Account/{}/Message/", self.base_url, self.auth_id)
    }
    fn call_url(&self) -> String {
        format!("{}/Account/{}/Call/", self.base_url, self.auth_id)
    }
    fn call_status_url(&self, call_uuid: &str) -> String {
        format!(
            "{}/Account/{}/Call/{}/",
            self.base_url, self.auth_id, call_uuid
        )
    }

    /// The URL Plivo signed: the operator's configured base plus the exact
    /// path this daemon served. An answer request and a hangup notice arrive on
    /// different paths and are signed over different URLs.
    fn signed_url(&self, path: &str) -> Result<String, String> {
        let base = self.public_base_url.as_deref().ok_or_else(|| {
            "no public base URL is configured; cannot verify a Plivo webhook signature".to_string()
        })?;
        Ok(format!("{}{path}", base.trim_end_matches('/')))
    }

    /// Where Plivo is told to report delivery reports and call lifecycle.
    fn status_callback(&self) -> Option<String> {
        self.public_base_url
            .as_deref()
            .map(|base| super::status_callback_url(base, &self.account_id))
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
impl TelecomProvider for PlivoProvider {
    fn kind(&self) -> TelecomKind {
        TelecomKind::Plivo
    }

    /// Plivo, like Twilio, serves inbound media from its own API behind the
    /// account's HTTP credential.
    async fn fetch_media(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
        crate::daemon::channel_adapter::fetch_url_basic_auth(
            url,
            &self.auth_id,
            &self.auth_token,
            max_bytes,
        )
        .await
        .map_err(|error| self.redact(error))
    }

    fn media_stream(&self) -> Option<crate::daemon::call_media::MediaStreamFormat> {
        Some(MEDIA_FORMAT)
    }

    /// Plivo XML. `<Stream bidirectional="true">` is the equivalent verb, and
    /// Plivo expects played-back audio under its own `playAudio` event rather
    /// than the inbound `media` one.
    fn answer_instructions(&self, media_url: &str) -> Option<AnswerDocument> {
        Some(AnswerDocument {
            content_type: "text/xml; charset=utf-8",
            body: format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Response><Stream bidirectional=\"true\" keepCallAlive=\"true\" audioTrack=\"inbound\" contentType=\"audio/x-mulaw;rate=8000\">{}</Stream></Response>",
                crate::daemon::service::xml_escape(media_url)
            ),
        })
    }

    async fn probe(&self) -> ChannelHealth {
        let now = now_ms();
        if self.auth_id.trim().is_empty() || self.auth_token.trim().is_empty() {
            return ChannelHealth::error(now, "Plivo requires an Auth ID and Auth Token");
        }
        let client = match self.client() {
            Ok(client) => client,
            Err(error) => return ChannelHealth::error(now, error),
        };
        let request = client
            .get(self.account_url())
            .basic_auth(&self.auth_id, Some(&self.auth_token));
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => {
                return ChannelHealth::error(
                    now,
                    self.redact(format!("Could not reach Plivo: {error}")),
                )
            }
        };
        let status = response.status();
        if status.as_u16() == 401 {
            return ChannelHealth::error(now, "Plivo rejected the Auth ID / Auth Token (401)");
        }
        if !status.is_success() {
            return ChannelHealth::error(
                now,
                self.redact(format!("Plivo returned {status} probing the account")),
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
        let _ = idempotency_key; // no caller-supplied dedupe key on Plivo's Message API
        let client = match self.client() {
            Ok(client) => client,
            Err(error) => return SendOutcome::PermanentFailure { error },
        };
        let mut body = serde_json::json!({
            "src": self.from_number,
            "dst": to_number,
            "text": text,
        });
        if !media_urls.is_empty() {
            body["media_urls"] = serde_json::json!(media_urls);
            body["type"] = serde_json::json!("mms");
        }
        // Plivo sends a delivery report only to the URL the message names.
        if let Some(status) = self.status_callback() {
            body["url"] = serde_json::json!(status);
            body["method"] = serde_json::json!("POST");
        }
        let request = client
            .post(self.message_url())
            .basic_auth(&self.auth_id, Some(&self.auth_token))
            .json(&body);
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => {
                return if error.is_connect() {
                    SendOutcome::RetryableFailure {
                        error: self.redact(format!("Could not connect to Plivo: {error}")),
                        retry_after_ms: None,
                    }
                } else {
                    SendOutcome::NeedsReconciliation {
                        error: self.redact(format!("Plivo send outcome unknown: {error}")),
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
                error: "Plivo rate-limited the request (429)".to_string(),
                retry_after_ms,
            };
        }
        let body_text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return SendOutcome::PermanentFailure {
                error: self.redact(format!("Plivo returned {status} for Message/")),
            };
        }
        match serde_json::from_str::<PlivoMessageResponse>(&body_text) {
            Ok(parsed) => SendOutcome::Sent {
                provider_message_id: parsed.message_uuid.into_iter().next(),
            },
            Err(_) => SendOutcome::NeedsReconciliation {
                error: "Plivo accepted the message but returned an unparseable response"
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
        // No caller-supplied dedupe key on this carrier's call API; the outbox
        // row's own state machine is what stops a second dial.
        let _ = idempotency_key;
        if record {
            // Plivo records with a `<Record>` element in the call flow, which
            // cannot run alongside the bidirectional stream this call needs.
            // Saying so is better than placing a call the operator believes is
            // being recorded and is not.
            return Err(
                "Plivo cannot record a streamed call. Turn recording off for this number, or record at the carrier."
                    .to_string(),
            );
        }
        let client = self.client()?;
        let mut body = serde_json::json!({
            "from": self.from_number,
            "to": to_number,
            "answer_url": answer_url,
            "answer_method": "POST",
        });
        // Plivo reports a call's life only to the URLs the request names.
        // Without these the call store never learns that the phone rang, was
        // answered, or hung up.
        if let Some(status) = self.status_callback() {
            body["ring_url"] = serde_json::json!(status);
            body["ring_method"] = serde_json::json!("POST");
            body["hangup_url"] = serde_json::json!(status);
            body["hangup_method"] = serde_json::json!("POST");
        }
        let request = client
            .post(self.call_url())
            .basic_auth(&self.auth_id, Some(&self.auth_token))
            .json(&body);
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => {
                return if error.is_connect() {
                    Err(self.redact(format!("Could not connect to Plivo: {error}")))
                } else {
                    // The POST may already have reached Plivo before the
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
            return Err(self.redact(format!("Plivo returned {status} placing the call")));
        }
        match serde_json::from_str::<PlivoCallResponse>(&body_text) {
            Ok(parsed) => Ok(CallHandle {
                provider_call_id: parsed.request_uuid,
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
            .delete(self.call_status_url(provider_call_id))
            .basic_auth(&self.auth_id, Some(&self.auth_token));
        let response = little_monkey_lib::egress::send(request)
            .await
            .map_err(|error| self.redact(format!("Plivo hangup outcome unknown: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(self.redact(format!("Plivo returned {status} ending the call")));
        }
        Ok(())
    }

    fn verify_webhook(
        &self,
        path: &str,
        headers: &[(String, String)],
        body: &[u8],
        now_ms: i64,
    ) -> Result<TelecomEvent, String> {
        let url = self.signed_url(path)?;
        // A `BTreeMap` iterates in sorted key order, which is the order V3
        // signs the parameters in.
        let params: std::collections::BTreeMap<String, String> = url::form_urlencoded::parse(body)
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        let signed_message = plivo_signed_message(headers, &url, &params)?;
        plivo_verify_any(
            &self.auth_token,
            &signed_message.message,
            &signed_message.header,
        )?;
        normalize_plivo_params(&params, path.ends_with("/status"), now_ms)
    }
}

/// What a Plivo callback proves knowledge of, and the header carrying the
/// proof.
struct PlivoSignedMessage {
    message: String,
    header: String,
}

/// Build the exact string this callback's signature covers.
///
/// Plivo signs two ways and sends both, on different products. V3 — voice —
/// signs the URL, then every POST parameter as `key` immediately followed by
/// `value` in sorted key order, then the nonce. V2 — messaging — signs the URL
/// and the nonce only. Accepting only one of them meant one of SMS and voice
/// could never verify on the shared endpoint, and computing V3 without the
/// parameters meant a genuine voice callback never verified at all.
fn plivo_signed_message(
    headers: &[(String, String)],
    url: &str,
    params: &std::collections::BTreeMap<String, String>,
) -> Result<PlivoSignedMessage, String> {
    if let (Some(signature), Some(nonce)) = (
        header(headers, "X-Plivo-Signature-V3"),
        header(headers, "X-Plivo-Signature-V3-Nonce"),
    ) {
        let mut message = String::from(url);
        for (key, value) in params {
            message.push_str(key);
            message.push_str(value);
        }
        message.push_str(nonce);
        return Ok(PlivoSignedMessage {
            message,
            header: signature.to_string(),
        });
    }
    if let (Some(signature), Some(nonce)) = (
        header(headers, "X-Plivo-Signature-V2"),
        header(headers, "X-Plivo-Signature-V2-Nonce"),
    ) {
        return Ok(PlivoSignedMessage {
            message: format!("{url}{nonce}"),
            header: signature.to_string(),
        });
    }
    Err("missing X-Plivo-Signature-V3 or X-Plivo-Signature-V2 header".to_string())
}

/// Verifies `signature_header` (possibly several comma-separated base64
/// signatures during a key rotation) against
/// `signed_message` with `auth_token`, accepting if any candidate verifies.
fn plivo_verify_any(
    auth_token: &str,
    signed_message: &str,
    signature_header: &str,
) -> Result<(), String> {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, auth_token.as_bytes());
    for candidate in signature_header.split(',') {
        let candidate = candidate.trim();
        if candidate.is_empty() {
            continue;
        }
        if let Ok(expected) = STANDARD.decode(candidate) {
            if ring::hmac::verify(&key, signed_message.as_bytes(), &expected).is_ok() {
                return Ok(());
            }
        }
    }
    Err("Plivo signature verification failed".to_string())
}

fn plivo_call_status_to_state(status: &str) -> Option<CallState> {
    match status {
        "queued" => Some(CallState::Queued),
        "ringing" => Some(CallState::Ringing),
        "in-progress" => Some(CallState::InProgress),
        "completed" => Some(CallState::Completed),
        "busy" | "failed" | "no-answer" | "timeout" | "canceled" => Some(CallState::Failed),
        _ => None,
    }
}

/// One entry of Plivo's `Media` array on an inbound MMS.
#[derive(Debug, Deserialize)]
struct PlivoMedia {
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    media_url: String,
}

/// Plivo's terminal message states, and whether each means it arrived.
///
/// `queued` and `sent` are Plivo saying it has the message, not that a handset
/// does; only the states below are an answer.
fn plivo_delivery(status: &str) -> Option<bool> {
    match status {
        "delivered" => Some(true),
        "undelivered" | "failed" | "rejected" => Some(false),
        _ => None,
    }
}

fn normalize_plivo_params(
    params: &std::collections::BTreeMap<String, String>,
    on_status_path: bool,
    received_at_ms: i64,
) -> Result<TelecomEvent, String> {
    // A delivery report carries the same `MessageUUID` as the message it is
    // about plus a `Status`. Checked before the inbound arm for the reason
    // Twilio's is: an inbound text has no `Status`, and reading a report as a
    // text would hand the agent an empty message from the person we just
    // texted.
    if let (Some(message_uuid), Some(status)) = (
        params
            .get("MessageUUID")
            .or_else(|| params.get("ParentMessageUUID")),
        params.get("Status"),
    ) {
        let Some(delivered) = plivo_delivery(status) else {
            return Ok(TelecomEvent::Ignored);
        };
        return Ok(TelecomEvent::SmsStatus {
            provider_message_id: message_uuid.clone(),
            delivered,
            error: (!delivered).then(|| match params.get("ErrorCode") {
                Some(code) => format!("Plivo reported {status} (error {code})"),
                None => format!("Plivo reported {status}"),
            }),
        });
    }
    if let Some(message_uuid) = params.get("MessageUUID") {
        let from = super::normalize_e164(
            params
                .get("From")
                .ok_or_else(|| "Plivo inbound SMS is missing From".to_string())?,
        );
        let to = super::normalize_e164(params.get("To").map(String::as_str).unwrap_or_default());
        let text = params.get("Text").cloned().unwrap_or_default();
        // MMS: Plivo posts the media as a JSON array of {content_type, media_url}
        // in `Media`. Metadata only — the bytes are fetched later under the
        // normal egress and artifact limits, like any other provider's.
        let attachments = params
            .get("Media")
            .and_then(|raw| serde_json::from_str::<Vec<PlivoMedia>>(raw).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|media| !media.media_url.is_empty())
            .map(|media| ChannelAttachment {
                provider_id: None,
                kind: media
                    .content_type
                    .as_deref()
                    .map(AttachmentKind::from_mime)
                    .unwrap_or(AttachmentKind::Other),
                filename: None,
                mime_type: media.content_type,
                declared_size_bytes: None,
                stored_size_bytes: None,
                source: AttachmentSource::Url {
                    url: media.media_url,
                },
                // Filled by ingest once the bytes are actually fetched; a
                // webhook carries only the address of the media.
                stored_artifact_id: None,
                fetch_error: None,
                text_excerpt: None,
            })
            .collect();
        let mut metadata = BoundedMetadata::new();
        metadata.insert("to_number", to);
        let envelope = ChannelEnvelope {
            account_id: String::new(),
            kind: ChannelKind::Sms,
            provider_event_id: message_uuid.clone(),
            conversation: ChannelConversation::direct(from.clone()),
            sender: ChannelSender::new(from),
            text,
            attachments,
            reply_to_provider_id: None,
            mentions_self: false,
            received_at_ms,
            metadata,
        };
        return Ok(TelecomEvent::InboundSms(Box::new(envelope)));
    }
    if let Some(call_uuid) = params.get("CallUUID") {
        let inbound = params
            .get("Direction")
            .is_some_and(|direction| direction.starts_with("inbound"));
        // The answer URL asks what to do with a live call; ring and hangup URLs
        // report what became of one. Only the path separates them, and the
        // reply to the first is the markup that connects the audio.
        if !on_status_path {
            return Ok(TelecomEvent::AnswerRequest {
                provider_call_id: call_uuid.clone(),
                // Plivo answered the dial with a `RequestUUID` and identifies
                // the live call by `CallUUID`. The row was written under the
                // first, so the second has to arrive with it or the outbound
                // call can never be found, advanced or hung up.
                request_id: params.get("RequestUUID").cloned(),
                direction: if inbound {
                    CallDirection::Inbound
                } else {
                    CallDirection::Outbound
                },
                from_number: super::normalize_e164(
                    params.get("From").map(String::as_str).unwrap_or_default(),
                ),
                to_number: super::normalize_e164(
                    params.get("To").map(String::as_str).unwrap_or_default(),
                ),
                received_at_ms,
            });
        }
        if let Some(state) = params
            .get("CallStatus")
            .and_then(|status| plivo_call_status_to_state(status))
        {
            return Ok(TelecomEvent::CallProgress {
                provider_call_id: call_uuid.clone(),
                state,
                detail: params.get("HangupCause").cloned(),
            });
        }
    }
    Ok(TelecomEvent::Ignored)
}

#[derive(Debug, Deserialize)]
struct PlivoMessageResponse {
    #[serde(default)]
    message_uuid: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PlivoCallResponse {
    request_uuid: String,
}

#[cfg(test)]
mod tests {
    /// The path a carrier's answer request arrives on, which is what its
    /// signature covers.
    const ANSWER_PATH: &str = "/v1/telecom/acct-1";
    /// Where a carrier reports what became of a message or a call.
    const STATUS_PATH: &str = "/v1/telecom/acct-1/status";

    #[tokio::test]
    async fn plivo_refuses_to_place_a_call_it_cannot_record() {
        // Recording on Plivo needs a `<Record>` element, which cannot run
        // alongside the bidirectional stream a conversation needs. Refusing is
        // the honest answer; placing an unrecorded call while the operator
        // believes it is recorded is not.
        let provider = provider_with_base("http://127.0.0.1:1");

        let error = provider
            .place_call(
                "+15551230000",
                "https://example.test/answer",
                true,
                "outbox-1",
            )
            .await
            .expect_err("refused");

        assert!(error.contains("cannot record a streamed call"));
    }

    use super::*;

    fn config(base_url: &str) -> TelecomConfig {
        TelecomConfig {
            account_id: "acct-1".to_string(),
            kind: TelecomKind::Plivo,
            carrier_account_id: "MAAAAAAAAAAAAAAAAAAA".to_string(),
            from_number: "+15550001111".to_string(),
            secret: "test-auth-token".to_string(),
            public_base_url: Some(base_url.to_string()),
            webhook_public_key: None,
        }
    }

    fn provider(base_url: &str) -> PlivoProvider {
        PlivoProvider::new(config(base_url))
    }

    fn sign(auth_token: &str, message: &str) -> String {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, auth_token.as_bytes());
        STANDARD.encode(ring::hmac::sign(&key, message.as_bytes()).as_ref())
    }

    /// A fresh nonce for one signed callback.
    ///
    /// Plivo chooses the nonce and sends it in a header; this module only
    /// re-signs what arrived, so nothing here depends on the value. It is
    /// generated rather than written down anyway, because a literal reaching a
    /// signing routine is a cryptographic constant baked into the source as far
    /// as a scanner can tell, and telling real ones from fixtures by eye is
    /// exactly the review that stops being done.
    fn fresh_nonce() -> String {
        let bytes: [u8; 16] = ring::rand::generate(&ring::rand::SystemRandom::new())
            .expect("system randomness")
            .expose();
        bytes.iter().fold(String::new(), |mut hex, byte| {
            use std::fmt::Write;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
    }

    /// Sign a callback the way Plivo's Voice V3 scheme does: the URL, then
    /// every POST parameter as key immediately followed by value in sorted key
    /// order, then the nonce.
    ///
    /// Written from Plivo's published algorithm rather than from this module's
    /// verifier on purpose — a test that signs the way the code checks proves
    /// only that the code agrees with itself, which is exactly how a verifier
    /// that ignored the parameters passed its own suite.
    fn signed_v3(auth_token: &str, url: &str, body: &str, nonce: &str) -> Vec<(String, String)> {
        let mut params: Vec<(String, String)> = url::form_urlencoded::parse(body.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        params.sort();
        let mut message = String::from(url);
        for (key, value) in &params {
            message.push_str(key);
            message.push_str(value);
        }
        message.push_str(nonce);
        vec![
            (
                "X-Plivo-Signature-V3".to_string(),
                sign(auth_token, &message),
            ),
            ("X-Plivo-Signature-V3-Nonce".to_string(), nonce.to_string()),
        ]
    }

    /// Sign the way Plivo's Messaging V2 scheme does: URL and nonce only.
    fn signed_v2(auth_token: &str, url: &str, nonce: &str) -> Vec<(String, String)> {
        vec![
            (
                "X-Plivo-Signature-V2".to_string(),
                sign(auth_token, &format!("{url}{nonce}")),
            ),
            ("X-Plivo-Signature-V2-Nonce".to_string(), nonce.to_string()),
        ]
    }

    #[test]
    fn a_correct_signature_verifies() {
        let provider = provider("https://ops.example.com");
        let url = super::super::callback_url("https://ops.example.com", "acct-1");
        let nonce = "nonce-1";
        let signature = sign(&provider.auth_token, &format!("{url}{nonce}"));
        assert!(
            plivo_verify_any(&provider.auth_token, &format!("{url}{nonce}"), &signature).is_ok()
        );
    }

    #[test]
    fn a_tampered_url_fails() {
        let provider = provider("https://ops.example.com");
        let url = super::super::callback_url("https://ops.example.com", "acct-1");
        let nonce = "nonce-1";
        let signature = sign(&provider.auth_token, &format!("{url}{nonce}"));
        assert!(plivo_verify_any(
            &provider.auth_token,
            &format!("{url}other{nonce}"),
            &signature
        )
        .is_err());
    }

    #[test]
    fn a_tampered_nonce_fails() {
        let provider = provider("https://ops.example.com");
        let url = super::super::callback_url("https://ops.example.com", "acct-1");
        let signature = sign(&provider.auth_token, &format!("{url}nonce-1"));
        assert!(
            plivo_verify_any(&provider.auth_token, &format!("{url}nonce-2"), &signature).is_err()
        );
    }

    #[test]
    fn a_wrong_key_fails() {
        let provider = provider("https://ops.example.com");
        let url = super::super::callback_url("https://ops.example.com", "acct-1");
        let signature = sign("some-other-token", &format!("{url}nonce-1"));
        assert!(
            plivo_verify_any(&provider.auth_token, &format!("{url}nonce-1"), &signature).is_err()
        );
    }

    #[test]
    fn any_one_of_several_comma_separated_signatures_may_verify() {
        let provider = provider("https://ops.example.com");
        let url = super::super::callback_url("https://ops.example.com", "acct-1");
        let message = format!("{url}nonce-1");
        let good = sign(&provider.auth_token, &message);
        let bad = sign("stale-rotated-token", &message);
        let header = format!("{bad},{good}");
        assert!(plivo_verify_any(&provider.auth_token, &message, &header).is_ok());
    }

    #[test]
    fn verify_webhook_normalizes_an_inbound_sms() {
        let provider = provider("https://ops.example.com");
        let url = super::super::callback_url("https://ops.example.com", "acct-1");
        let nonce = fresh_nonce();
        let body = "MessageUUID=msg-1&From=%2B15551230000&To=%2B15550001111&Text=hello+there";
        // Plivo Messaging signs with V2 headers, not the V3 ones Voice uses.
        // Requiring V3 for everything on this shared endpoint meant real
        // inbound SMS never verified at all.
        let headers = signed_v2(&provider.auth_token, &url, &nonce);
        let event = provider
            .verify_webhook(ANSWER_PATH, &headers, body.as_bytes(), 1_700_000_000_000)
            .expect("verifies");
        match event {
            TelecomEvent::InboundSms(envelope) => {
                assert_eq!(envelope.provider_event_id, "msg-1");
                assert_eq!(envelope.sender.sender_id, "+15551230000");
                assert_eq!(envelope.text, "hello there");
            }
            other => panic!("expected InboundSms, got {other:?}"),
        }
    }

    #[test]
    fn verify_webhook_normalizes_a_call_status() {
        let provider = provider("https://ops.example.com");
        let url = super::super::status_callback_url("https://ops.example.com", "acct-1");
        let nonce = fresh_nonce();
        let body = "CallUUID=call-1&CallStatus=completed";
        let headers = signed_v3(&provider.auth_token, &url, body, &nonce);
        let event = provider
            .verify_webhook(STATUS_PATH, &headers, body.as_bytes(), 0)
            .expect("verifies");
        assert_eq!(
            event,
            TelecomEvent::CallProgress {
                provider_call_id: "call-1".to_string(),
                state: CallState::Completed,
                detail: None,
            }
        );
    }

    #[test]
    fn a_ringing_inbound_call_is_a_call_to_answer_not_a_status_update() {
        let provider = provider("https://ops.example.com");
        let url = super::super::callback_url("https://ops.example.com", "acct-1");
        let nonce = fresh_nonce();
        // Plivo sends bare digits, which is exactly why the number is
        // normalized before it becomes a conversation id.
        let body =
            "CallUUID=call-9&CallStatus=ringing&Direction=inbound&From=15551230000&To=15550001111";
        let headers = signed_v3(&provider.auth_token, &url, body, &nonce);

        let event = provider
            .verify_webhook(ANSWER_PATH, &headers, body.as_bytes(), 1_700_000_000_000)
            .expect("verifies");

        assert_eq!(
            event,
            TelecomEvent::AnswerRequest {
                request_id: None,
                direction: CallDirection::Inbound,
                provider_call_id: "call-9".to_string(),
                from_number: "+15551230000".to_string(),
                to_number: "+15550001111".to_string(),
                received_at_ms: 1_700_000_000_000,
            },
            "a ringing inbound call has to reach the answering policy; as a \
             CallProgress for an unknown call it would be dropped and the \
             phone would never be picked up"
        );
    }

    #[test]
    fn a_delivery_report_is_not_read_as_an_inbound_text() {
        let provider = provider("https://ops.example.com");
        let url = super::super::status_callback_url("https://ops.example.com", "acct-1");
        let nonce = fresh_nonce();
        let body =
            "MessageUUID=msg-1&From=15550001111&To=15551230000&Status=undelivered&ErrorCode=400";
        let headers = signed_v3(&provider.auth_token, &url, body, &nonce);

        let event = provider
            .verify_webhook(STATUS_PATH, &headers, body.as_bytes(), 0)
            .expect("verifies");

        match event {
            TelecomEvent::SmsStatus {
                provider_message_id,
                delivered,
                error,
            } => {
                assert_eq!(provider_message_id, "msg-1");
                assert!(!delivered);
                assert!(error.expect("a reason").contains("undelivered"));
            }
            other => panic!("a receipt read as {other:?} would deliver an empty text to the agent"),
        }
    }

    #[test]
    fn a_delivered_report_says_so_and_carries_no_error() {
        let provider = provider("https://ops.example.com");
        let url = super::super::status_callback_url("https://ops.example.com", "acct-1");
        let nonce = fresh_nonce();
        let body = "MessageUUID=msg-2&Status=delivered";
        let headers = signed_v3(&provider.auth_token, &url, body, &nonce);

        assert_eq!(
            provider
                .verify_webhook(STATUS_PATH, &headers, body.as_bytes(), 0)
                .expect("verifies"),
            TelecomEvent::SmsStatus {
                provider_message_id: "msg-2".to_string(),
                delivered: true,
                error: None,
            }
        );
    }

    #[test]
    fn an_intermediate_message_state_is_not_a_delivery_answer() {
        let provider = provider("https://ops.example.com");
        let url = super::super::status_callback_url("https://ops.example.com", "acct-1");
        let nonce = fresh_nonce();
        // "queued" is Plivo saying it has the message, not that a handset does.
        let body = "MessageUUID=msg-3&Status=queued";
        let headers = signed_v3(&provider.auth_token, &url, body, &nonce);

        assert_eq!(
            provider
                .verify_webhook(STATUS_PATH, &headers, body.as_bytes(), 0)
                .expect("verifies"),
            TelecomEvent::Ignored
        );
    }

    #[test]
    fn the_signed_url_is_the_one_the_operator_was_told_to_configure() {
        let provider = provider("https://ops.example.com/");

        assert_eq!(
            provider
                .signed_url(ANSWER_PATH)
                .expect("a base is configured"),
            "https://ops.example.com/v1/telecom/acct-1",
            "this is the path the daemon serves and the UI publishes; a \
             verifier that rebuilt any other one would reject every genuine \
             callback"
        );
    }

    #[test]
    fn verify_webhook_ignores_an_uninteresting_callback() {
        let provider = provider("https://ops.example.com");
        let url = super::super::callback_url("https://ops.example.com", "acct-1");
        let nonce = fresh_nonce();
        let headers = signed_v3(&provider.auth_token, &url, "", &nonce);
        let event = provider
            .verify_webhook(ANSWER_PATH, &headers, b"", 0)
            .expect("verifies");
        assert_eq!(event, TelecomEvent::Ignored);
    }

    #[test]
    fn verify_webhook_rejects_a_missing_nonce_header() {
        let provider = provider("https://ops.example.com");
        let headers = vec![("X-Plivo-Signature-V3".to_string(), "sig".to_string())];
        assert!(provider
            .verify_webhook(ANSWER_PATH, &headers, b"", 0)
            .is_err());
    }

    #[test]
    fn redact_scrubs_the_auth_token_and_auth_id() {
        let provider = provider("https://ops.example.com");
        let rendered = provider.redact(format!(
            "https://api.plivo.com/v1/Account/{}/Message/ auth={}",
            provider.auth_id, provider.auth_token
        ));
        assert!(!rendered.contains(provider.auth_token.as_str()));
        assert!(!rendered.contains(provider.auth_id.as_str()));
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

    fn provider_with_base(base_url: &str) -> PlivoProvider {
        let mut provider = PlivoProvider::new(config("https://ops.example.com"));
        provider.base_url = base_url.to_string();
        provider
    }

    #[tokio::test]
    async fn send_sms_maps_a_success_response_to_sent() {
        let base = serve_once("202 Accepted", r#"{"message_uuid":["msg-abc"]}"#);
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
    async fn a_message_asks_plivo_for_the_delivery_report_it_will_be_sent() {
        use crate::daemon::channel_adapter::test_http;
        let (base, requests) =
            test_http::serve(vec![(202, r#"{"message_uuid":["msg-1"]}"#.to_string())]);
        let mut provider = provider("https://ops.example.com");
        provider.base_url = base;

        provider.send_sms("+15551230000", "hi", &[], "idem-1").await;

        let request = String::from_utf8(requests.recv().expect("a request")).expect("utf8");
        assert!(
            request.contains(r#""url":"https://ops.example.com/v1/telecom/acct-1/status""#),
            "Plivo sends a delivery report only to the URL the message names: {request}"
        );
    }

    #[tokio::test]
    async fn a_call_asks_plivo_for_its_ring_and_hangup_callbacks() {
        use crate::daemon::channel_adapter::test_http;
        let (base, requests) =
            test_http::serve(vec![(201, r#"{"request_uuid":"req-1"}"#.to_string())]);
        let mut provider = provider("https://ops.example.com");
        provider.base_url = base;

        provider
            .place_call("+15551230000", "https://ops.example.com/answer", false, "k")
            .await
            .expect("placed");

        let request = String::from_utf8(requests.recv().expect("a request")).expect("utf8");
        // Without these the call store never learns the phone rang or hung up,
        // and a call left open keeps billing.
        assert!(request.contains(r#""ring_url""#), "{request}");
        assert!(request.contains(r#""hangup_url""#), "{request}");
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
    async fn send_sms_maps_401_to_permanent() {
        let base = serve_once("401 Unauthorized", "{}");
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
        stream_id_path: &["start", "streamId"],
        outbound_chunk_ms: 20,
    };

impl crate::daemon::call_media::MediaFrameCodec for PlivoProvider {
    fn format(&self) -> crate::daemon::call_media::MediaStreamFormat {
        MEDIA_FORMAT
    }

    fn encode_clear_frame(&self, stream_id: &str) -> String {
        serde_json::json!({
            "event": "clearAudio",
            "streamId": stream_id,
        })
        .to_string()
    }

    fn encode_media_frame(&self, payload_b64: &str, stream_id: &str) -> String {
        // Plivo plays audio back under its own event, and refuses a frame that
        // does not say what the bytes are. It does not want its stream id back.
        let _ = stream_id;
        serde_json::json!({
            "event": "playAudio",
            "media": {
                "contentType": "audio/x-mulaw",
                "sampleRate": 8000,
                "payload": payload_b64,
            },
        })
        .to_string()
    }
}
