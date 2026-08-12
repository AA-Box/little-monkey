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
//! reading (see `mod.rs`'s trait doc). [`WEBHOOK_PATH`] is the fixed path
//! this provider assumes the operator's Plivo application points its
//! message/answer URLs at, combined with [`TelecomConfig::public_base_url`]
//! (never a `Host` or `X-Forwarded-*` header).
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
    BoundedMetadata, ChannelConversation, ChannelEnvelope, ChannelHealth, ChannelKind,
    ChannelSender, SendOutcome,
};

use super::{CallHandle, CallState, TelecomConfig, TelecomEvent, TelecomKind, TelecomProvider};

const API_BASE: &str = "https://api.plivo.com/v1";

/// See the module doc's "Reconstructing the signed URL" section.
const WEBHOOK_PATH: &str = "/webhooks/telephony/plivo";

pub struct PlivoProvider {
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

    fn webhook_url(&self) -> Result<String, String> {
        let base = self.public_base_url.as_deref().ok_or_else(|| {
            "no public base URL is configured; cannot verify a Plivo webhook signature".to_string()
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

    async fn send_sms(&self, to_number: &str, text: &str, idempotency_key: &str) -> SendOutcome {
        let _ = idempotency_key; // no caller-supplied dedupe key on Plivo's Message API
        let client = match self.client() {
            Ok(client) => client,
            Err(error) => return SendOutcome::PermanentFailure { error },
        };
        let body = serde_json::json!({
            "src": self.from_number,
            "dst": to_number,
            "text": text,
        });
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

    async fn place_call(&self, to_number: &str, answer_url: &str) -> Result<CallHandle, String> {
        let client = self.client()?;
        let body = serde_json::json!({
            "from": self.from_number,
            "to": to_number,
            "answer_url": answer_url,
            "answer_method": "POST",
        });
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
        headers: &[(String, String)],
        body: &[u8],
        now_ms: i64,
    ) -> Result<TelecomEvent, String> {
        let signature_header = header(headers, "X-Plivo-Signature-V3")
            .ok_or_else(|| "missing X-Plivo-Signature-V3 header".to_string())?;
        let nonce = header(headers, "X-Plivo-Signature-V3-Nonce")
            .ok_or_else(|| "missing X-Plivo-Signature-V3-Nonce header".to_string())?;
        let url = self.webhook_url()?;
        let signed_message = format!("{url}{nonce}");
        plivo_verify_any(&self.auth_token, &signed_message, signature_header)?;
        let params: std::collections::BTreeMap<String, String> = url::form_urlencoded::parse(body)
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        normalize_plivo_params(&params, now_ms)
    }
}

/// Verifies `signature_header` (`X-Plivo-Signature-V3`, possibly several
/// comma-separated base64 signatures during a key rotation) against
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

fn normalize_plivo_params(
    params: &std::collections::BTreeMap<String, String>,
    received_at_ms: i64,
) -> Result<TelecomEvent, String> {
    if let Some(message_uuid) = params.get("MessageUUID") {
        let from = params
            .get("From")
            .cloned()
            .ok_or_else(|| "Plivo inbound SMS is missing From".to_string())?;
        let to = params.get("To").cloned().unwrap_or_default();
        let text = params.get("Text").cloned().unwrap_or_default();
        let mut metadata = BoundedMetadata::new();
        metadata.insert("to_number", to);
        let envelope = ChannelEnvelope {
            account_id: String::new(),
            kind: ChannelKind::Sms,
            provider_event_id: message_uuid.clone(),
            conversation: ChannelConversation::direct(from.clone()),
            sender: ChannelSender::new(from),
            text,
            attachments: Vec::new(),
            reply_to_provider_id: None,
            mentions_self: false,
            received_at_ms,
            metadata,
        };
        return Ok(TelecomEvent::InboundSms(Box::new(envelope)));
    }
    if let (Some(call_uuid), Some(call_status)) = (params.get("CallUUID"), params.get("CallStatus"))
    {
        if let Some(state) = plivo_call_status_to_state(call_status) {
            return Ok(TelecomEvent::CallProgress {
                provider_call_id: call_uuid.clone(),
                state,
                detail: None,
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

    #[test]
    fn a_correct_signature_verifies() {
        let provider = provider("https://ops.example.com");
        let url = format!("https://ops.example.com{WEBHOOK_PATH}");
        let nonce = "nonce-1";
        let signature = sign(&provider.auth_token, &format!("{url}{nonce}"));
        assert!(
            plivo_verify_any(&provider.auth_token, &format!("{url}{nonce}"), &signature).is_ok()
        );
    }

    #[test]
    fn a_tampered_url_fails() {
        let provider = provider("https://ops.example.com");
        let url = format!("https://ops.example.com{WEBHOOK_PATH}");
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
        let url = format!("https://ops.example.com{WEBHOOK_PATH}");
        let signature = sign(&provider.auth_token, &format!("{url}nonce-1"));
        assert!(
            plivo_verify_any(&provider.auth_token, &format!("{url}nonce-2"), &signature).is_err()
        );
    }

    #[test]
    fn a_wrong_key_fails() {
        let provider = provider("https://ops.example.com");
        let url = format!("https://ops.example.com{WEBHOOK_PATH}");
        let signature = sign("some-other-token", &format!("{url}nonce-1"));
        assert!(
            plivo_verify_any(&provider.auth_token, &format!("{url}nonce-1"), &signature).is_err()
        );
    }

    #[test]
    fn any_one_of_several_comma_separated_signatures_may_verify() {
        let provider = provider("https://ops.example.com");
        let url = format!("https://ops.example.com{WEBHOOK_PATH}");
        let message = format!("{url}nonce-1");
        let good = sign(&provider.auth_token, &message);
        let bad = sign("stale-rotated-token", &message);
        let header = format!("{bad},{good}");
        assert!(plivo_verify_any(&provider.auth_token, &message, &header).is_ok());
    }

    #[test]
    fn verify_webhook_normalizes_an_inbound_sms() {
        let provider = provider("https://ops.example.com");
        let url = format!("https://ops.example.com{WEBHOOK_PATH}");
        let nonce = "nonce-9";
        let signature = sign(&provider.auth_token, &format!("{url}{nonce}"));
        let body = "MessageUUID=msg-1&From=%2B15551230000&To=%2B15550001111&Text=hello+there";
        let headers = vec![
            ("X-Plivo-Signature-V3".to_string(), signature),
            ("X-Plivo-Signature-V3-Nonce".to_string(), nonce.to_string()),
        ];
        let event = provider
            .verify_webhook(&headers, body.as_bytes(), 1_700_000_000_000)
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
        let url = format!("https://ops.example.com{WEBHOOK_PATH}");
        let nonce = "nonce-2";
        let body = "CallUUID=call-1&CallStatus=completed";
        let signature = sign(&provider.auth_token, &format!("{url}{nonce}"));
        let headers = vec![
            ("X-Plivo-Signature-V3".to_string(), signature),
            ("X-Plivo-Signature-V3-Nonce".to_string(), nonce.to_string()),
        ];
        let event = provider
            .verify_webhook(&headers, body.as_bytes(), 0)
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
    fn verify_webhook_ignores_an_uninteresting_callback() {
        let provider = provider("https://ops.example.com");
        let url = format!("https://ops.example.com{WEBHOOK_PATH}");
        let nonce = "nonce-3";
        let signature = sign(&provider.auth_token, &format!("{url}{nonce}"));
        let headers = vec![
            ("X-Plivo-Signature-V3".to_string(), signature),
            ("X-Plivo-Signature-V3-Nonce".to_string(), nonce.to_string()),
        ];
        let event = provider.verify_webhook(&headers, b"", 0).expect("verifies");
        assert_eq!(event, TelecomEvent::Ignored);
    }

    #[test]
    fn verify_webhook_rejects_a_missing_nonce_header() {
        let provider = provider("https://ops.example.com");
        let headers = vec![("X-Plivo-Signature-V3".to_string(), "sig".to_string())];
        assert!(provider.verify_webhook(&headers, b"", 0).is_err());
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
    async fn send_sms_maps_401_to_permanent() {
        let base = serve_once("401 Unauthorized", "{}");
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
