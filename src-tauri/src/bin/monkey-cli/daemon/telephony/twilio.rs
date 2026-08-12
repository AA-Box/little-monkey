//! Twilio adapter: SMS and voice over the 2010-04-01 REST API, webhooks
//! verified with `X-Twilio-Signature`.
//!
//! # The auth token — and the account SID — never appear in a diagnostic
//!
//! HTTP Basic auth carries the Account SID and Auth Token on every REST call,
//! and the Account SID is also baked into every request URL
//! (`.../Accounts/{sid}/...`). A naive `format!("... {error}")` around a
//! `reqwest::Error` — whose `Display` prints the request URL — would put the
//! SID straight into a `ChannelHealth.last_error`, a `SendOutcome::*` error,
//! or a log line, and the auth token would follow it out of the `basic_auth`
//! header the moment anyone dumped a request for debugging. [`TwilioProvider::redact`]
//! is applied to every string this module builds from a response or a
//! transport error before it leaves the module.
//!
//! # No idempotency key on `Messages.json`
//!
//! Twilio's Messages API has no caller-supplied dedupe key (unlike, say,
//! Stripe). `send_sms`'s `idempotency_key` argument is accepted, per the
//! trait, and otherwise unused here — a retry after a crash relies entirely
//! on the outbox's own state machine deciding whether to resend, not on
//! Twilio collapsing a duplicate for us.
//!
//! # Reconstructing the signed URL
//!
//! `verify_webhook` receives only headers, a body and a clock reading (see
//! `mod.rs`'s trait doc) — never the request path — so the URL Twilio signed
//! has to be reconstructed rather than observed. [`WEBHOOK_PATH`] is the
//! fixed path this provider assumes the operator's Twilio console points both
//! the incoming-message and call-status-callback URLs at; combined with
//! [`TelecomConfig::public_base_url`] (never a `Host` or `X-Forwarded-*`
//! header — those are attacker-controlled on an unauthenticated request) it
//! is the whole of the URL Twilio signed over.

use std::collections::BTreeMap;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::Deserialize;

use little_monkey_lib::channels::types::{
    AttachmentKind, AttachmentSource, BoundedMetadata, ChannelAttachment, ChannelConversation,
    ChannelEnvelope, ChannelHealth, ChannelKind, ChannelSender, SendOutcome,
};

use super::{
    AnswerDocument, CallHandle, CallState, TelecomConfig, TelecomEvent, TelecomKind,
    TelecomProvider,
};

const API_BASE: &str = "https://api.twilio.com/2010-04-01";

/// See the module doc's "Reconstructing the signed URL" section.
const WEBHOOK_PATH: &str = "/webhooks/telephony/twilio";

pub struct TwilioProvider {
    account_sid: String,
    auth_token: String,
    from_number: String,
    public_base_url: Option<String>,
    /// Overridable only by this module's own tests, which point it at a
    /// loopback fixture; production always leaves it at [`API_BASE`].
    base_url: String,
}

impl TwilioProvider {
    pub fn new(config: TelecomConfig) -> Self {
        Self {
            account_sid: config.carrier_account_id,
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

    /// Scrubs the Auth Token and Account SID out of any string before it
    /// becomes a `ChannelHealth.last_error`, a `SendOutcome::*` error, or a
    /// log line. The SID is not a bearer credential, but it identifies the
    /// account and sits in every URL this module builds, so a `reqwest::Error`
    /// (whose `Display` prints the request URL) would leak it right alongside
    /// the token if only the token were guarded.
    fn redact(&self, message: impl Into<String>) -> String {
        let mut message = message.into();
        if !self.auth_token.is_empty() {
            message = message.replace(self.auth_token.as_str(), "<redacted>");
        }
        if !self.account_sid.is_empty() {
            message = message.replace(self.account_sid.as_str(), "<redacted>");
        }
        message
    }

    fn account_url(&self) -> String {
        format!("{}/Accounts/{}.json", self.base_url, self.account_sid)
    }

    fn messages_url(&self) -> String {
        format!(
            "{}/Accounts/{}/Messages.json",
            self.base_url, self.account_sid
        )
    }

    fn calls_url(&self) -> String {
        format!("{}/Accounts/{}/Calls.json", self.base_url, self.account_sid)
    }

    fn call_status_url(&self, call_sid: &str) -> String {
        format!(
            "{}/Accounts/{}/Calls/{}.json",
            self.base_url, self.account_sid, call_sid
        )
    }

    fn webhook_url(&self) -> Result<String, String> {
        let base = self.public_base_url.as_deref().ok_or_else(|| {
            "no public base URL is configured; cannot verify a Twilio webhook signature".to_string()
        })?;
        Ok(format!("{}{WEBHOOK_PATH}", base.trim_end_matches('/')))
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn retry_after_ms(response: &reqwest::Response) -> Option<i64> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .map(|seconds| seconds * 1000)
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

#[async_trait]
impl TelecomProvider for TwilioProvider {
    fn kind(&self) -> TelecomKind {
        TelecomKind::Twilio
    }

    fn media_stream(&self) -> Option<crate::daemon::call_media::MediaStreamFormat> {
        Some(MEDIA_FORMAT)
    }

    /// TwiML. `<Connect><Stream>` is bidirectional: Twilio streams the caller's
    /// audio to the socket and plays back whatever we write to it, which is the
    /// only Twilio verb that gives both halves of a conversation.
    fn answer_instructions(&self, media_url: &str) -> Option<AnswerDocument> {
        Some(AnswerDocument {
            content_type: "text/xml; charset=utf-8",
            body: format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Response><Connect><Stream url=\"{}\"/></Connect></Response>",
                crate::daemon::service::xml_escape(media_url)
            ),
        })
    }

    async fn probe(&self) -> ChannelHealth {
        let now = now_ms();
        if self.account_sid.trim().is_empty() || self.auth_token.trim().is_empty() {
            return ChannelHealth::error(now, "Twilio requires an Account SID and Auth Token");
        }
        let client = match self.client() {
            Ok(client) => client,
            Err(error) => return ChannelHealth::error(now, error),
        };
        let request = client
            .get(self.account_url())
            .basic_auth(&self.account_sid, Some(&self.auth_token));
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => {
                return ChannelHealth::error(
                    now,
                    self.redact(format!("Could not reach Twilio: {error}")),
                )
            }
        };
        let status = response.status();
        if status.as_u16() == 401 {
            return ChannelHealth::error(now, "Twilio rejected the Account SID / Auth Token (401)");
        }
        if !status.is_success() {
            return ChannelHealth::error(
                now,
                self.redact(format!("Twilio returned {status} probing the account")),
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
        // No caller-supplied idempotency key exists on Messages.json; see the
        // module doc. Accepted per the trait, unused here on purpose.
        let _ = idempotency_key;
        let client = match self.client() {
            Ok(client) => client,
            Err(error) => return SendOutcome::PermanentFailure { error },
        };
        let mut params = vec![
            ("To", to_number),
            ("From", self.from_number.as_str()),
            ("Body", text),
        ];
        // Twilio fetches each MediaUrl itself, which is why the URLs are signed
        // and short-lived rather than public.
        for url in media_urls {
            params.push(("MediaUrl", url.as_str()));
        }
        let request = client
            .post(self.messages_url())
            .basic_auth(&self.account_sid, Some(&self.auth_token))
            .form(&params);
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => {
                return if error.is_connect() {
                    // The TCP/TLS handshake itself failed: the request
                    // provably never left this machine.
                    SendOutcome::RetryableFailure {
                        error: self.redact(format!("Could not connect to Twilio: {error}")),
                        retry_after_ms: None,
                    }
                } else {
                    // A stalled read or reset mid-response may have happened
                    // after Twilio already accepted the message; whether it
                    // was sent is unknown, so this is never treated as safe
                    // to retry.
                    SendOutcome::NeedsReconciliation {
                        error: self.redact(format!("Twilio send outcome unknown: {error}")),
                    }
                };
            }
        };
        let status = response.status();
        if status.as_u16() == 429 {
            let retry_after = retry_after_ms(&response);
            return SendOutcome::RetryableFailure {
                error: "Twilio rate-limited the request (429)".to_string(),
                retry_after_ms: retry_after,
            };
        }
        let body_text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return SendOutcome::PermanentFailure {
                error: self.redact(format!("Twilio returned {status} for Messages.json")),
            };
        }
        match serde_json::from_str::<TwilioMessageResponse>(&body_text) {
            Ok(parsed) => SendOutcome::Sent {
                provider_message_id: Some(parsed.sid),
            },
            Err(_) => SendOutcome::NeedsReconciliation {
                error: "Twilio accepted the message but returned an unparseable response"
                    .to_string(),
            },
        }
    }

    async fn place_call(
        &self,
        to_number: &str,
        answer_url: &str,
        record: bool,
    ) -> Result<CallHandle, String> {
        let client = self.client()?;
        let mut params = vec![
            ("To", to_number),
            ("From", self.from_number.as_str()),
            ("Url", answer_url),
        ];
        if record {
            // Twilio records the whole call from answer and stores it under the
            // operator's own account, where their retention settings apply.
            params.push(("Record", "true"));
        }
        let request = client
            .post(self.calls_url())
            .basic_auth(&self.account_sid, Some(&self.auth_token))
            .form(&params);
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => {
                return if error.is_connect() {
                    Err(self.redact(format!("Could not connect to Twilio: {error}")))
                } else {
                    // The POST may already have reached Twilio before the
                    // transport failed. A duplicated phone call cannot be
                    // undone, so this is reported as a handle that needs
                    // reconciliation rather than a plain, retryable error.
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
            return Err(self.redact(format!("Twilio returned {status} placing the call")));
        }
        match serde_json::from_str::<TwilioCallResponse>(&body_text) {
            Ok(parsed) => {
                let state =
                    twilio_call_status_to_state(&parsed.status).unwrap_or(CallState::Queued);
                Ok(CallHandle {
                    provider_call_id: parsed.sid,
                    state,
                })
            }
            Err(_) => Ok(CallHandle {
                provider_call_id: String::new(),
                state: CallState::NeedsReconciliation,
            }),
        }
    }

    async fn hangup(&self, provider_call_id: &str) -> Result<(), String> {
        let client = self.client()?;
        let params = [("Status", "completed")];
        let request = client
            .post(self.call_status_url(provider_call_id))
            .basic_auth(&self.account_sid, Some(&self.auth_token))
            .form(&params);
        let response = little_monkey_lib::egress::send(request)
            .await
            .map_err(|error| self.redact(format!("Twilio hangup outcome unknown: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(self.redact(format!("Twilio returned {status} ending the call")));
        }
        Ok(())
    }

    fn verify_webhook(
        &self,
        headers: &[(String, String)],
        body: &[u8],
        now_ms: i64,
    ) -> Result<TelecomEvent, String> {
        let signature_b64 = header(headers, "X-Twilio-Signature")
            .ok_or_else(|| "missing X-Twilio-Signature header".to_string())?;
        let url = self.webhook_url()?;
        let params = decode_form_urlencoded(body)?;
        twilio_verify_signature(&self.auth_token, &url, &params, signature_b64)?;
        normalize_twilio_params(&params, now_ms)
    }
}

fn twilio_call_status_to_state(status: &str) -> Option<CallState> {
    match status {
        "queued" => Some(CallState::Queued),
        "ringing" => Some(CallState::Ringing),
        "in-progress" => Some(CallState::InProgress),
        "completed" => Some(CallState::Completed),
        "busy" | "failed" | "no-answer" => Some(CallState::Failed),
        _ => None,
    }
}

fn normalize_twilio_params(
    params: &BTreeMap<String, String>,
    received_at_ms: i64,
) -> Result<TelecomEvent, String> {
    if let Some(message_sid) = params.get("MessageSid") {
        let from = params
            .get("From")
            .cloned()
            .ok_or_else(|| "Twilio inbound SMS is missing From".to_string())?;
        let to = params.get("To").cloned().unwrap_or_default();
        let text = params.get("Body").cloned().unwrap_or_default();
        let num_media: usize = params
            .get("NumMedia")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let mut attachments = Vec::new();
        for index in 0..num_media {
            if let Some(url) = params.get(&format!("MediaUrl{index}")) {
                let mime = params.get(&format!("MediaContentType{index}")).cloned();
                let kind = mime
                    .as_deref()
                    .map(AttachmentKind::from_mime)
                    .unwrap_or(AttachmentKind::Other);
                attachments.push(ChannelAttachment {
                    provider_id: None,
                    kind,
                    filename: None,
                    mime_type: mime,
                    declared_size_bytes: None,
                    source: AttachmentSource::Url { url: url.clone() },
                });
            }
        }
        let mut metadata = BoundedMetadata::new();
        metadata.insert("to_number", to);
        let envelope = ChannelEnvelope {
            account_id: String::new(),
            kind: ChannelKind::Sms,
            provider_event_id: message_sid.clone(),
            conversation: ChannelConversation::direct(from.clone()),
            sender: ChannelSender::new(from),
            text,
            attachments,
            reply_to_provider_id: None,
            mentions_self: false,
            // Twilio's inbound-message webhook carries no timestamp of its
            // own; the moment this app verified the callback is the least-
            // wrong arrival time available.
            received_at_ms,
            metadata,
        };
        return Ok(TelecomEvent::InboundSms(Box::new(envelope)));
    }
    if let (Some(call_sid), Some(call_status)) = (params.get("CallSid"), params.get("CallStatus")) {
        if let Some(state) = twilio_call_status_to_state(call_status) {
            return Ok(TelecomEvent::CallProgress {
                provider_call_id: call_sid.clone(),
                state,
                detail: None,
            });
        }
    }
    Ok(TelecomEvent::Ignored)
}

/// The exact byte string Twilio signed: the full URL immediately followed by
/// every POST parameter's key and value concatenated in ascending key order
/// (a `BTreeMap` already iterates that way, so no separate sort is needed).
fn twilio_signing_string(url: &str, params: &BTreeMap<String, String>) -> String {
    let mut signed = String::from(url);
    for (key, value) in params {
        signed.push_str(key);
        signed.push_str(value);
    }
    signed
}

/// Verifies `signature_b64` (the `X-Twilio-Signature` header value) over
/// `url`/`params` with `auth_token`, in constant time — `ring::hmac::verify`
/// does the constant-time comparison itself, so no comparison is written by
/// hand here.
fn twilio_verify_signature(
    auth_token: &str,
    url: &str,
    params: &BTreeMap<String, String>,
    signature_b64: &str,
) -> Result<(), String> {
    let expected = STANDARD
        .decode(signature_b64)
        .map_err(|_| "malformed X-Twilio-Signature".to_string())?;
    let signing_string = twilio_signing_string(url, params);
    let key = ring::hmac::Key::new(
        ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
        auth_token.as_bytes(),
    );
    ring::hmac::verify(&key, signing_string.as_bytes(), &expected)
        .map_err(|_| "Twilio signature verification failed".to_string())
}

/// `application/x-www-form-urlencoded` decoder: `+` becomes a space, `%XX`
/// becomes the raw byte, and the decoded bytes for one value are joined
/// *before* the UTF-8 conversion runs — so a multi-byte UTF-8 sequence split
/// across consecutive `%XX` escapes decodes correctly, which converting each
/// escape to a `char` on its own would not. No percent-encoding crate is
/// available in this tree, hence the hand-rolled decoder.
fn decode_form_urlencoded(body: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let text = std::str::from_utf8(body).map_err(|_| "webhook body is not UTF-8".to_string())?;
    let mut params = BTreeMap::new();
    for pair in text.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut parts = pair.splitn(2, '=');
        let raw_key = parts.next().unwrap_or_default();
        let raw_value = parts.next().unwrap_or_default();
        params.insert(decode_component(raw_key)?, decode_component(raw_value)?);
    }
    Ok(params)
}

fn decode_component(raw: &str) -> Result<String, String> {
    let mut bytes = Vec::with_capacity(raw.len());
    let mut iter = raw.bytes();
    while let Some(byte) = iter.next() {
        match byte {
            b'+' => bytes.push(b' '),
            b'%' => {
                let hi = iter
                    .next()
                    .ok_or_else(|| "truncated percent-escape".to_string())?;
                let lo = iter
                    .next()
                    .ok_or_else(|| "truncated percent-escape".to_string())?;
                let hex_bytes = [hi, lo];
                let hex = std::str::from_utf8(&hex_bytes)
                    .map_err(|_| "invalid percent-escape".to_string())?;
                let value = u8::from_str_radix(hex, 16)
                    .map_err(|_| "invalid percent-escape".to_string())?;
                bytes.push(value);
            }
            other => bytes.push(other),
        }
    }
    String::from_utf8(bytes).map_err(|_| "percent-decoded body is not valid UTF-8".to_string())
}

#[derive(Debug, Deserialize)]
struct TwilioMessageResponse {
    sid: String,
}

#[derive(Debug, Deserialize)]
struct TwilioCallResponse {
    sid: String,
    #[serde(default)]
    status: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(base_url: &str) -> TelecomConfig {
        TelecomConfig {
            account_id: "acct-1".to_string(),
            kind: TelecomKind::Twilio,
            carrier_account_id: "AC00000000000000000000000000000000".to_string(),
            from_number: "+15550001111".to_string(),
            secret: "test-auth-token".to_string(),
            public_base_url: Some(base_url.to_string()),
            webhook_public_key: None,
        }
    }

    fn provider(base_url: &str) -> TwilioProvider {
        TwilioProvider::new(config(base_url))
    }

    fn sign(auth_token: &str, url: &str, params: &BTreeMap<String, String>) -> String {
        let signing_string = twilio_signing_string(url, params);
        let key = ring::hmac::Key::new(
            ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
            auth_token.as_bytes(),
        );
        STANDARD.encode(ring::hmac::sign(&key, signing_string.as_bytes()).as_ref())
    }

    // --- form decoder -------------------------------------------------

    #[test]
    fn decodes_plus_as_space() {
        let params = decode_form_urlencoded(b"Body=hello+world").unwrap();
        assert_eq!(params.get("Body").unwrap(), "hello world");
    }

    #[test]
    fn decodes_percent_20_as_space() {
        let params = decode_form_urlencoded(b"Body=hello%20world").unwrap();
        assert_eq!(params.get("Body").unwrap(), "hello world");
    }

    #[test]
    fn decodes_percent_2b_as_a_literal_plus() {
        let params = decode_form_urlencoded(b"Body=1%2B1").unwrap();
        assert_eq!(params.get("Body").unwrap(), "1+1");
    }

    #[test]
    fn decodes_a_multi_byte_utf8_sequence_across_escapes() {
        // U+2603 SNOWMAN, percent-encoded byte by byte.
        let params = decode_form_urlencoded(b"Body=%E2%98%83").unwrap();
        assert_eq!(params.get("Body").unwrap(), "\u{2603}");
    }

    // --- signing string / signature, against a hand-computed fixture --

    #[test]
    fn twilio_signature_matches_a_hand_computed_fixture() {
        // Independently computed with Python's hmac/hashlib over the same
        // inputs Twilio's own signature-validation docs use as an example,
        // not with this module's own signing code.
        let mut params = BTreeMap::new();
        params.insert(
            "CallSid".to_string(),
            "CA1234567890ABCDE1234567890ABCDE".to_string(),
        );
        params.insert("Caller".to_string(), "+14158675310".to_string());
        params.insert("Digits".to_string(), "1234".to_string());
        params.insert("From".to_string(), "+14158675310".to_string());
        params.insert("To".to_string(), "+18005551212".to_string());
        let url = "https://mycompany.com/myapp.php?foo=1&bar=2";
        let signature = "HKH1PCdmw1YvcFsuJxOIA8Dzg2k=";
        assert!(twilio_verify_signature("12345", url, &params, signature).is_ok());
    }

    #[test]
    fn tampered_body_fails_verification() {
        let mut params = BTreeMap::new();
        params.insert("Body".to_string(), "hi".to_string());
        let signature = sign("12345", "https://example.com/hook", &params);
        params.insert("Body".to_string(), "hi!".to_string());
        assert!(
            twilio_verify_signature("12345", "https://example.com/hook", &params, &signature)
                .is_err()
        );
    }

    #[test]
    fn tampered_url_fails_verification() {
        let mut params = BTreeMap::new();
        params.insert("Body".to_string(), "hi".to_string());
        let signature = sign("12345", "https://example.com/hook", &params);
        assert!(
            twilio_verify_signature("12345", "https://example.com/other", &params, &signature)
                .is_err()
        );
    }

    #[test]
    fn wrong_key_fails_verification() {
        let mut params = BTreeMap::new();
        params.insert("Body".to_string(), "hi".to_string());
        let signature = sign("12345", "https://example.com/hook", &params);
        assert!(twilio_verify_signature(
            "wrong-token",
            "https://example.com/hook",
            &params,
            &signature
        )
        .is_err());
    }

    // --- full verify_webhook path (URL reconstruction + normalization) --

    #[test]
    fn verify_webhook_normalizes_an_inbound_sms() {
        let provider = provider("https://ops.example.com");
        let url = format!("https://ops.example.com{WEBHOOK_PATH}");
        let mut params = BTreeMap::new();
        params.insert("MessageSid".to_string(), "SM123".to_string());
        params.insert("From".to_string(), "+15551230000".to_string());
        params.insert("To".to_string(), "+15550001111".to_string());
        params.insert("Body".to_string(), "hello there".to_string());
        let signature = sign(&provider.auth_token, &url, &params);
        let body = "MessageSid=SM123&From=%2B15551230000&To=%2B15550001111&Body=hello+there";
        let headers = vec![("X-Twilio-Signature".to_string(), signature)];
        let event = provider
            .verify_webhook(&headers, body.as_bytes(), 1_700_000_000_000)
            .expect("verifies");
        match event {
            TelecomEvent::InboundSms(envelope) => {
                assert_eq!(envelope.provider_event_id, "SM123");
                assert_eq!(envelope.conversation.conversation_id, "+15551230000");
                assert_eq!(envelope.sender.sender_id, "+15551230000");
                assert_eq!(envelope.text, "hello there");
                assert_eq!(envelope.received_at_ms, 1_700_000_000_000);
            }
            other => panic!("expected InboundSms, got {other:?}"),
        }
    }

    #[test]
    fn verify_webhook_normalizes_a_call_status() {
        let provider = provider("https://ops.example.com");
        let url = format!("https://ops.example.com{WEBHOOK_PATH}");
        let mut params = BTreeMap::new();
        params.insert("CallSid".to_string(), "CA999".to_string());
        params.insert("CallStatus".to_string(), "in-progress".to_string());
        let signature = sign(&provider.auth_token, &url, &params);
        let body = "CallSid=CA999&CallStatus=in-progress";
        let headers = vec![("X-Twilio-Signature".to_string(), signature)];
        let event = provider
            .verify_webhook(&headers, body.as_bytes(), 0)
            .expect("verifies");
        assert_eq!(
            event,
            TelecomEvent::CallProgress {
                provider_call_id: "CA999".to_string(),
                state: CallState::InProgress,
                detail: None,
            }
        );
    }

    #[test]
    fn verify_webhook_ignores_an_uninteresting_callback() {
        let provider = provider("https://ops.example.com");
        let url = format!("https://ops.example.com{WEBHOOK_PATH}");
        let params: BTreeMap<String, String> = BTreeMap::new();
        let signature = sign(&provider.auth_token, &url, &params);
        let headers = vec![("X-Twilio-Signature".to_string(), signature)];
        let event = provider.verify_webhook(&headers, b"", 0).expect("verifies");
        assert_eq!(event, TelecomEvent::Ignored);
    }

    #[test]
    fn verify_webhook_rejects_a_missing_signature_header() {
        let provider = provider("https://ops.example.com");
        let error = provider.verify_webhook(&[], b"Body=hi", 0).unwrap_err();
        assert!(error.contains("X-Twilio-Signature"));
    }

    #[test]
    fn verify_webhook_produces_no_event_on_a_bad_signature() {
        let provider = provider("https://ops.example.com");
        let headers = vec![(
            "X-Twilio-Signature".to_string(),
            STANDARD.encode(b"nonsense"),
        )];
        assert!(provider
            .verify_webhook(&headers, b"MessageSid=SM1&From=%2B1&Body=hi", 0)
            .is_err());
    }

    // --- credential redaction ------------------------------------------

    #[test]
    fn redact_scrubs_the_auth_token_and_account_sid() {
        let provider = provider("https://ops.example.com");
        let rendered = provider.redact(format!(
            "error sending request for url (https://api.twilio.com/2010-04-01/Accounts/{}/Messages.json): auth={}",
            provider.account_sid, provider.auth_token
        ));
        assert!(!rendered.contains(provider.auth_token.as_str()));
        assert!(!rendered.contains(provider.account_sid.as_str()));
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

    fn provider_with_base(base_url: &str) -> TwilioProvider {
        let mut provider = TwilioProvider::new(config("https://ops.example.com"));
        provider.base_url = base_url.to_string();
        provider
    }

    #[tokio::test]
    async fn send_sms_maps_a_success_response_to_sent() {
        let base = serve_once("200 OK", r#"{"sid":"SM_ABC"}"#);
        let provider = provider_with_base(&base);
        let outcome = provider.send_sms("+15551230000", "hi", &[], "idem-1").await;
        assert_eq!(
            outcome,
            SendOutcome::Sent {
                provider_message_id: Some("SM_ABC".to_string())
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
        let base = serve_once("200 OK", r#"{"sid":"CA1"}"#);
        let provider = provider_with_base(&base);
        assert!(provider.hangup("CA1").await.is_ok());
    }

    #[tokio::test]
    async fn hangup_reports_a_non_success_status() {
        let base = serve_once("404 Not Found", "{}");
        let provider = provider_with_base(&base);
        assert!(provider.hangup("CA1").await.is_err());
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
        stream_id_path: &["streamSid"],
        outbound_chunk_ms: 20,
    };

impl crate::daemon::call_media::MediaFrameCodec for TwilioProvider {
    fn format(&self) -> crate::daemon::call_media::MediaStreamFormat {
        MEDIA_FORMAT
    }

    fn encode_clear_frame(&self, stream_id: &str) -> String {
        serde_json::json!({
            "event": "clear",
            "streamSid": stream_id,
        })
        .to_string()
    }

    fn encode_media_frame(&self, payload_b64: &str, stream_id: &str) -> String {
        // Twilio requires its own stream id echoed on every outbound frame;
        // a frame without it is discarded silently.
        serde_json::json!({
            "event": "media",
            "streamSid": stream_id,
            "media": { "payload": payload_b64 },
        })
        .to_string()
    }
}
