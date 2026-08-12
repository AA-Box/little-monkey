//! Mock adapter: the deterministic, in-process carrier every telephony test
//! runs against.
//!
//! # Why this is what makes "CI never dials anyone" true rather than assumed
//!
//! This provider never opens a socket, never spawns a process, never touches
//! a filesystem — every trait method here reads or mutates an in-memory
//! [`Mutex`] and nothing else. That is not a policy some other test could
//! accidentally violate; it is structural. A CI run that only ever
//! constructs [`MockProvider`] cannot send a real SMS or place a real call no
//! matter what it scripts, because there is no code path in this file that
//! reaches a network.
//!
//! # Verification is real, not skipped
//!
//! [`MockProvider::verify_webhook`] checks an HMAC-SHA256 signature over the
//! body exactly like a real carrier's would, keyed by the shared secret in
//! [`TelecomConfig::secret`]. A test that wants to inject an inbound SMS or a
//! call-progress event goes through [`MockProvider::sign_inbound_sms`] /
//! [`MockProvider::sign_call_progress`] to get a `(headers, body)` pair and
//! then calls `verify_webhook` on it, the same as it would for a real
//! carrier's webhook. There is no second, unverified path to inject an
//! event — if there were, every test built on it would prove nothing about
//! the path production actually uses.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};

use little_monkey_lib::channels::types::{
    BoundedMetadata, ChannelConversation, ChannelEnvelope, ChannelHealth, ChannelKind,
    ChannelSender, SendOutcome,
};

use super::{CallHandle, CallState, TelecomConfig, TelecomEvent, TelecomKind, TelecomProvider};

pub struct MockProvider {
    account_id: String,
    from_number: String,
    /// Shared test secret. Not a real carrier credential — there is nothing
    /// downstream of it to protect — but it is what `verify_webhook` demands
    /// a signed body prove knowledge of, same as a real carrier's would.
    webhook_secret: String,
    state: Mutex<MockState>,
}

#[derive(Default)]
struct MockState {
    sms_outcomes: VecDeque<SendOutcome>,
    call_outcomes: VecDeque<Result<CallHandle, String>>,
    sent_sms: Vec<SentSms>,
    dialed_calls: Vec<DialedCall>,
    next_message_id: u64,
    next_call_id: u64,
}

/// One recorded `send_sms` call, for a test to assert against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentSms {
    pub to_number: String,
    pub text: String,
    /// Signed URLs the carrier would fetch for an MMS.
    pub media_urls: Vec<String>,
    pub idempotency_key: String,
}

/// One recorded `place_call` call, for a test to assert against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialedCall {
    pub to_number: String,
    pub answer_url: String,
    /// Whether the account asked for this call to be recorded, so a test can
    /// prove the setting reached the carrier rather than stopping at the UI.
    pub record: bool,
}

impl MockProvider {
    pub fn new(config: TelecomConfig) -> Self {
        Self {
            account_id: config.account_id,
            from_number: config.from_number,
            webhook_secret: config.secret,
            state: Mutex::new(MockState::default()),
        }
    }

    /// Scripts the outcome of the next `send_sms` call. Consumed
    /// front-to-back; once the queue is empty, `send_sms` falls back to a
    /// deterministic `Sent`.
    pub fn queue_sms_outcome(&self, outcome: SendOutcome) {
        self.state.lock().unwrap().sms_outcomes.push_back(outcome);
    }

    /// Scripts the outcome of the next `place_call` call, the same way
    /// [`Self::queue_sms_outcome`] scripts `send_sms`.
    pub fn queue_call_outcome(&self, outcome: Result<CallHandle, String>) {
        self.state.lock().unwrap().call_outcomes.push_back(outcome);
    }

    pub fn sent_messages(&self) -> Vec<SentSms> {
        self.state.lock().unwrap().sent_sms.clone()
    }

    pub fn dialed_calls(&self) -> Vec<DialedCall> {
        self.state.lock().unwrap().dialed_calls.clone()
    }

    /// A signed `(headers, body)` pair a test can feed straight into
    /// `verify_webhook` to exercise the inbound-SMS path.
    pub fn sign_inbound_sms(
        &self,
        from_number: &str,
        to_number: &str,
        text: &str,
    ) -> (Vec<(String, String)>, Vec<u8>) {
        let mut state = self.state.lock().unwrap();
        state.next_message_id += 1;
        let id = state.next_message_id;
        drop(state);
        self.sign_body(&MockWebhookBody::InboundSms {
            message_id: format!("mock-msg-{id}"),
            from: from_number.to_string(),
            to: to_number.to_string(),
            text: text.to_string(),
        })
    }

    /// A signed `(headers, body)` pair a test can feed into `verify_webhook`
    /// to exercise the call-progress path.
    pub fn sign_call_progress(
        &self,
        provider_call_id: &str,
        state: CallState,
    ) -> (Vec<(String, String)>, Vec<u8>) {
        self.sign_body(&MockWebhookBody::CallProgress {
            provider_call_id: provider_call_id.to_string(),
            state,
        })
    }

    /// A signed `(headers, body)` pair that normalizes to
    /// `TelecomEvent::Ignored` — a carrier heartbeat, in mock form.
    pub fn sign_ignored(&self) -> (Vec<(String, String)>, Vec<u8>) {
        self.sign_body(&MockWebhookBody::Ignored)
    }

    fn sign_body(&self, body: &MockWebhookBody) -> (Vec<(String, String)>, Vec<u8>) {
        let bytes = serde_json::to_vec(body).expect("mock webhook body always serializes");
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, self.webhook_secret.as_bytes());
        let signature = STANDARD.encode(ring::hmac::sign(&key, &bytes).as_ref());
        (vec![("X-Mock-Signature".to_string(), signature)], bytes)
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

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum MockWebhookBody {
    InboundSms {
        message_id: String,
        from: String,
        to: String,
        text: String,
    },
    CallProgress {
        provider_call_id: String,
        state: CallState,
    },
    Ignored,
}

/// The mock streams like Twilio does, which is the shape the media-session
/// tests are written against. Still no IO: frames are strings.
const MEDIA_FORMAT: crate::daemon::call_media::MediaStreamFormat =
    crate::daemon::call_media::MediaStreamFormat {
        stream_id_path: &["streamSid"],
        outbound_chunk_ms: 20,
    };

impl crate::daemon::call_media::MediaFrameCodec for MockProvider {
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
        serde_json::json!({
            "event": "media",
            "streamSid": stream_id,
            "media": { "payload": payload_b64 },
        })
        .to_string()
    }
}

#[async_trait]
impl TelecomProvider for MockProvider {
    fn kind(&self) -> TelecomKind {
        TelecomKind::Mock
    }

    fn media_stream(&self) -> Option<crate::daemon::call_media::MediaStreamFormat> {
        Some(MEDIA_FORMAT)
    }

    async fn probe(&self) -> ChannelHealth {
        ChannelHealth::connected(now_ms(), Some(self.from_number.clone()))
    }

    async fn send_sms(
        &self,
        to_number: &str,
        text: &str,
        media_urls: &[String],
        idempotency_key: &str,
    ) -> SendOutcome {
        let mut state = self.state.lock().unwrap();
        state.sent_sms.push(SentSms {
            to_number: to_number.to_string(),
            text: text.to_string(),
            media_urls: media_urls.to_vec(),
            idempotency_key: idempotency_key.to_string(),
        });
        if let Some(outcome) = state.sms_outcomes.pop_front() {
            return outcome;
        }
        state.next_message_id += 1;
        let id = state.next_message_id;
        SendOutcome::Sent {
            provider_message_id: Some(format!("mock-msg-{id}")),
        }
    }

    async fn place_call(
        &self,
        to_number: &str,
        answer_url: &str,
        record: bool,
    ) -> Result<CallHandle, String> {
        let mut state = self.state.lock().unwrap();
        state.dialed_calls.push(DialedCall {
            to_number: to_number.to_string(),
            answer_url: answer_url.to_string(),
            record,
        });
        if let Some(outcome) = state.call_outcomes.pop_front() {
            return outcome;
        }
        state.next_call_id += 1;
        let id = state.next_call_id;
        Ok(CallHandle {
            provider_call_id: format!("mock-call-{id}"),
            state: CallState::Queued,
        })
    }

    async fn hangup(&self, _provider_call_id: &str) -> Result<(), String> {
        Ok(())
    }

    fn verify_webhook(
        &self,
        headers: &[(String, String)],
        body: &[u8],
        now_ms: i64,
    ) -> Result<TelecomEvent, String> {
        let signature_b64 = header(headers, "X-Mock-Signature")
            .ok_or_else(|| "missing X-Mock-Signature header".to_string())?;
        let expected = STANDARD
            .decode(signature_b64)
            .map_err(|_| "malformed X-Mock-Signature".to_string())?;
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, self.webhook_secret.as_bytes());
        ring::hmac::verify(&key, body, &expected)
            .map_err(|_| "mock signature verification failed".to_string())?;
        let parsed: MockWebhookBody = serde_json::from_slice(body)
            .map_err(|_| "mock webhook body is not valid JSON".to_string())?;
        match parsed {
            MockWebhookBody::InboundSms {
                message_id,
                from,
                to,
                text,
            } => {
                let mut metadata = BoundedMetadata::new();
                metadata.insert("to_number", to);
                Ok(TelecomEvent::InboundSms(Box::new(ChannelEnvelope {
                    account_id: self.account_id.clone(),
                    kind: ChannelKind::Sms,
                    provider_event_id: message_id,
                    conversation: ChannelConversation::direct(from.clone()),
                    sender: ChannelSender::new(from),
                    text,
                    attachments: Vec::new(),
                    reply_to_provider_id: None,
                    mentions_self: false,
                    received_at_ms: now_ms,
                    metadata,
                })))
            }
            MockWebhookBody::CallProgress {
                provider_call_id,
                state,
            } => Ok(TelecomEvent::CallProgress {
                provider_call_id,
                state,
                detail: None,
            }),
            MockWebhookBody::Ignored => Ok(TelecomEvent::Ignored),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> TelecomConfig {
        TelecomConfig {
            account_id: "acct-1".to_string(),
            kind: TelecomKind::Mock,
            carrier_account_id: "mock-account".to_string(),
            from_number: "+15550001111".to_string(),
            secret: "shared-test-secret".to_string(),
            public_base_url: None,
            webhook_public_key: None,
        }
    }

    fn provider() -> MockProvider {
        MockProvider::new(config())
    }

    // --- signature verification -----------------------------------------

    #[test]
    fn a_correctly_signed_body_verifies() {
        let provider = provider();
        let (headers, body) = provider.sign_inbound_sms("+15551230000", "+15550001111", "hi");
        assert!(provider.verify_webhook(&headers, &body, 0).is_ok());
    }

    #[test]
    fn a_tampered_body_fails_and_produces_no_event() {
        let provider = provider();
        let (headers, _) = provider.sign_inbound_sms("+15551230000", "+15550001111", "hi");
        let tampered = br#"{"type":"inbound_sms","message_id":"mock-msg-1","from":"+1evil","to":"+1","text":"hi"}"#;
        assert!(provider.verify_webhook(&headers, tampered, 0).is_err());
    }

    #[test]
    fn a_wrong_key_fails() {
        let provider = provider();
        let other = MockProvider::new(TelecomConfig {
            secret: "different-secret".to_string(),
            ..config()
        });
        let (headers, body) = other.sign_inbound_sms("+15551230000", "+15550001111", "hi");
        assert!(provider.verify_webhook(&headers, &body, 0).is_err());
    }

    #[test]
    fn a_missing_signature_header_is_rejected() {
        let provider = provider();
        let (_, body) = provider.sign_inbound_sms("+15551230000", "+15550001111", "hi");
        assert!(provider.verify_webhook(&[], &body, 0).is_err());
    }

    // --- normalization ----------------------------------------------------

    #[test]
    fn inbound_sms_normalizes_to_a_dm_envelope_with_a_deterministic_id() {
        let provider = provider();
        let (headers, body) = provider.sign_inbound_sms("+15551230000", "+15550001111", "hello");
        let event = provider
            .verify_webhook(&headers, &body, 1_700_000_000_000)
            .unwrap();
        match event {
            TelecomEvent::InboundSms(envelope) => {
                assert_eq!(envelope.provider_event_id, "mock-msg-1");
                assert_eq!(envelope.conversation.conversation_id, "+15551230000");
                assert_eq!(envelope.sender.sender_id, "+15551230000");
                assert_eq!(envelope.text, "hello");
                assert_eq!(envelope.received_at_ms, 1_700_000_000_000);
            }
            other => panic!("expected InboundSms, got {other:?}"),
        }
    }

    #[test]
    fn call_progress_normalizes_to_the_scripted_state() {
        let provider = provider();
        let (headers, body) = provider.sign_call_progress("call-1", CallState::Ringing);
        let event = provider.verify_webhook(&headers, &body, 0).unwrap();
        assert_eq!(
            event,
            TelecomEvent::CallProgress {
                provider_call_id: "call-1".to_string(),
                state: CallState::Ringing,
                detail: None,
            }
        );
    }

    #[test]
    fn an_uninteresting_callback_is_ignored() {
        let provider = provider();
        let (headers, body) = provider.sign_ignored();
        let event = provider.verify_webhook(&headers, &body, 0).unwrap();
        assert_eq!(event, TelecomEvent::Ignored);
    }

    // --- outbound scripting -------------------------------------------

    #[tokio::test]
    async fn send_sms_returns_a_deterministic_default_when_nothing_is_queued() {
        let provider = provider();
        let first = provider.send_sms("+1", "a", &[], "idem-1").await;
        let second = provider.send_sms("+1", "b", &[], "idem-2").await;
        assert_eq!(
            first,
            SendOutcome::Sent {
                provider_message_id: Some("mock-msg-1".to_string())
            }
        );
        assert_eq!(
            second,
            SendOutcome::Sent {
                provider_message_id: Some("mock-msg-2".to_string())
            }
        );
        assert_eq!(provider.sent_messages().len(), 2);
    }

    #[tokio::test]
    async fn send_sms_returns_queued_outcomes_in_order() {
        let provider = provider();
        provider.queue_sms_outcome(SendOutcome::RetryableFailure {
            error: "rate limited".to_string(),
            retry_after_ms: Some(1000),
        });
        provider.queue_sms_outcome(SendOutcome::PermanentFailure {
            error: "bad number".to_string(),
        });
        let first = provider.send_sms("+1", "a", &[], "idem-1").await;
        let second = provider.send_sms("+1", "b", &[], "idem-2").await;
        assert!(matches!(first, SendOutcome::RetryableFailure { .. }));
        assert!(matches!(second, SendOutcome::PermanentFailure { .. }));
    }

    #[tokio::test]
    async fn an_attachment_is_carried_as_a_media_url() {
        let provider = provider();
        let urls = vec!["https://calls.example.test/v1/telecom/tel-1/file?artifact=a".to_string()];

        let _ = provider
            .send_sms("+15551230000", "here", &urls, "idem-1")
            .await;

        assert_eq!(provider.sent_messages()[0].media_urls, urls);
    }

    #[tokio::test]
    async fn the_recording_setting_reaches_the_carrier() {
        let provider = provider();
        let _ = provider
            .place_call("+15551230000", "https://example.com/answer", true)
            .await;
        assert!(
            provider.dialed_calls()[0].record,
            "a stored recording setting that never reaches the carrier records nothing"
        );
    }

    #[tokio::test]
    async fn place_call_records_the_dial_and_returns_queued_outcomes() {
        let provider = provider();
        provider.queue_call_outcome(Ok(CallHandle {
            provider_call_id: "scripted-1".to_string(),
            state: CallState::Queued,
        }));
        let handle = provider
            .place_call("+1", "https://example.com/answer", false)
            .await
            .unwrap();
        assert_eq!(handle.provider_call_id, "scripted-1");
        assert_eq!(
            provider.dialed_calls(),
            vec![DialedCall {
                record: false,
                to_number: "+1".to_string(),
                answer_url: "https://example.com/answer".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn probe_is_always_connected_and_touches_no_network() {
        let provider = provider();
        let health = provider.probe().await;
        assert_eq!(
            health.state,
            little_monkey_lib::channels::types::HealthState::Connected
        );
    }
}
