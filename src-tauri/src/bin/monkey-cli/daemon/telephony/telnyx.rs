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

use super::{CallHandle, CallState, TelecomConfig, TelecomEvent, TelecomKind, TelecomProvider};

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

    async fn send_sms(&self, to_number: &str, text: &str, idempotency_key: &str) -> SendOutcome {
        let _ = idempotency_key; // no caller-supplied dedupe key on /v2/messages
        let client = match self.client() {
            Ok(client) => client,
            Err(error) => return SendOutcome::PermanentFailure { error },
        };
        let body = serde_json::json!({
            "from": self.from_number,
            "to": to_number,
            "text": text,
        });
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

    async fn place_call(&self, to_number: &str, answer_url: &str) -> Result<CallHandle, String> {
        let client = self.client()?;
        let body = serde_json::json!({
            "connection_id": self.connection_id,
            "to": to_number,
            "from": self.from_number,
            "webhook_url": answer_url,
        });
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
            .json(&serde_json::json!({}));
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
        headers: &[(String, String)],
        body: &[u8],
        now_ms: i64,
    ) -> Result<TelecomEvent, String> {
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

fn normalize_telnyx_event(data: &TelnyxData, received_at_ms: i64) -> Result<TelecomEvent, String> {
    match data.event_type.as_str() {
        "message.received" => {
            let from = data
                .payload
                .from
                .as_ref()
                .map(|address| address.phone_number.clone())
                .ok_or_else(|| "Telnyx inbound message is missing from".to_string())?;
            let to = data
                .payload
                .to
                .as_ref()
                .and_then(|list| list.first())
                .map(|address| address.phone_number.clone())
                .unwrap_or_default();
            let mut metadata = BoundedMetadata::new();
            metadata.insert("to_number", to);
            let attachments = data
                .payload
                .media
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .map(|media| ChannelAttachment {
                    provider_id: None,
                    kind: media
                        .content_type
                        .as_deref()
                        .map(AttachmentKind::from_mime)
                        .unwrap_or(AttachmentKind::Other),
                    filename: None,
                    mime_type: media.content_type.clone(),
                    declared_size_bytes: None,
                    source: AttachmentSource::Url {
                        url: media.url.clone(),
                    },
                })
                .collect();
            let envelope = ChannelEnvelope {
                account_id: String::new(),
                kind: ChannelKind::Sms,
                provider_event_id: data.payload.id.clone(),
                conversation: ChannelConversation::direct(from.clone()),
                sender: ChannelSender::new(from),
                text: data.payload.text.clone().unwrap_or_default(),
                attachments,
                reply_to_provider_id: None,
                mentions_self: false,
                received_at_ms,
                metadata,
            };
            Ok(TelecomEvent::InboundSms(Box::new(envelope)))
        }
        "call.initiated" if data.payload.direction.as_deref() == Some("incoming") => {
            Ok(TelecomEvent::InboundCall {
                provider_call_id: data.payload.id.clone(),
                from_number: data
                    .payload
                    .from
                    .as_ref()
                    .map(|address| address.phone_number.clone())
                    .unwrap_or_default(),
                to_number: data
                    .payload
                    .to
                    .as_ref()
                    .and_then(|list| list.first())
                    .map(|address| address.phone_number.clone())
                    .unwrap_or_default(),
                received_at_ms,
            })
        }
        "call.initiated" => Ok(TelecomEvent::CallProgress {
            provider_call_id: data.payload.id.clone(),
            state: CallState::Queued,
            detail: None,
        }),
        "call.answered" => Ok(TelecomEvent::CallProgress {
            provider_call_id: data.payload.id.clone(),
            state: CallState::InProgress,
            detail: None,
        }),
        "call.hangup" => {
            let state = match data.payload.hangup_cause.as_deref() {
                Some("normal_clearing") | None => CallState::Completed,
                Some(_) => CallState::Failed,
            };
            Ok(TelecomEvent::CallProgress {
                provider_call_id: data.payload.id.clone(),
                state,
                detail: data.payload.hangup_cause.clone(),
            })
        }
        "message.finalized" => {
            let failed = data
                .payload
                .to
                .as_ref()
                .map(|list| {
                    list.iter()
                        .any(|address| address.status.as_deref() == Some("delivery_failed"))
                })
                .unwrap_or(false);
            Ok(TelecomEvent::SmsStatus {
                provider_message_id: data.payload.id.clone(),
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
    direction: Option<String>,
    #[serde(default)]
    hangup_cause: Option<String>,
    #[serde(default)]
    media: Option<Vec<TelnyxMedia>>,
}

#[derive(Debug, Deserialize)]
struct TelnyxData {
    event_type: String,
    payload: TelnyxPayload,
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
            .verify_webhook(&headers, body, now)
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
        assert!(provider.verify_webhook(&headers, tampered, now).is_err());
    }

    #[test]
    fn a_wrong_key_fails_verification() {
        let pair = keypair();
        let other_pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(&[9u8; 32]).unwrap();
        let provider = provider(&other_pair);
        let body = br#"{"data":{"event_type":"message.received","payload":{"id":"msg-1","from":{"phone_number":"+1"}}}}"#;
        let now = 1_700_000_000_000;
        let headers = signed_headers(&pair, now / 1000, body);
        assert!(provider.verify_webhook(&headers, body, now).is_err());
    }

    #[test]
    fn a_stale_timestamp_fails_even_with_a_correct_signature() {
        let pair = keypair();
        let provider = provider(&pair);
        let body = br#"{"data":{"event_type":"message.received","payload":{"id":"msg-1","from":{"phone_number":"+1"}}}}"#;
        let now = 1_700_000_000_000;
        let stale_timestamp = now / 1000 - (MAX_TIMESTAMP_SKEW_SECS + 60);
        let headers = signed_headers(&pair, stale_timestamp, body);
        let error = provider.verify_webhook(&headers, body, now).unwrap_err();
        assert!(error.contains("stale"), "{error}");
    }

    #[test]
    fn a_missing_signature_header_is_rejected() {
        let pair = keypair();
        let provider = provider(&pair);
        let headers = vec![("telnyx-timestamp".to_string(), "1700000000".to_string())];
        assert!(provider
            .verify_webhook(&headers, b"{}", 1_700_000_000_000)
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
            .verify_webhook(&headers, b"{}", 1_700_000_000_000)
            .is_err());
    }

    #[test]
    fn call_status_vocabulary_maps_to_call_states() {
        let pair = keypair();
        let provider = provider(&pair);
        let now = 1_700_000_000_000;
        for (event_type, hangup_cause, expected) in [
            ("call.answered", None, CallState::InProgress),
            ("call.hangup", Some("normal_clearing"), CallState::Completed),
            ("call.hangup", Some("call_rejected"), CallState::Failed),
        ] {
            let body = if let Some(cause) = hangup_cause {
                format!(
                    r#"{{"data":{{"event_type":"{event_type}","payload":{{"id":"call-1","hangup_cause":"{cause}"}}}}}}"#
                )
            } else {
                format!(r#"{{"data":{{"event_type":"{event_type}","payload":{{"id":"call-1"}}}}}}"#)
            };
            let headers = signed_headers(&pair, now / 1000, body.as_bytes());
            let event = provider
                .verify_webhook(&headers, body.as_bytes(), now)
                .expect("verifies");
            assert_eq!(
                event,
                TelecomEvent::CallProgress {
                    provider_call_id: "call-1".to_string(),
                    state: expected,
                    detail: hangup_cause.map(|value| value.to_string()),
                }
            );
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
            .verify_webhook(&headers, body, now)
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
        let outcome = provider.send_sms("+15551230000", "hi", "idem-1").await;
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
        match provider.send_sms("+15551230000", "hi", "idem-1").await {
            SendOutcome::RetryableFailure { .. } => {}
            other => panic!("expected RetryableFailure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_sms_maps_403_to_permanent() {
        let base = serve_once("403 Forbidden", "{}");
        let provider = provider_with_base(&base);
        match provider.send_sms("+15551230000", "hi", "idem-1").await {
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
        match provider.send_sms("+15551230000", "hi", "idem-1").await {
            SendOutcome::NeedsReconciliation { .. } => {}
            other => panic!("expected NeedsReconciliation, got {other:?}"),
        }
    }
}
