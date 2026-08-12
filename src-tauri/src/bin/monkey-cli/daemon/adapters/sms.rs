//! SMS: the messaging side of a telephony account.
//!
//! Texts arrive through the carrier's callback route, not here — `telecom_worker`
//! normalizes them and hands them to the same `channel_ingress` gate every other
//! provider uses. What this adapter exists for is the other direction: when a run
//! answers a text, the reply is an ordinary outbox row, and the outbox needs
//! something to hand it to.
//!
//! So this is a thin skin over the account's carrier. It owns no credential, no
//! signature scheme and no normalization; all three already live in
//! `telephony::`, and duplicating any of them here would give SMS a second
//! security posture, which is exactly what the adapter seam exists to prevent.

use async_trait::async_trait;

use little_monkey_lib::channels::types::{
    ChannelHealth, ChannelKind, InboundTransport, OutboundMessage, ProviderCapabilities,
    SendOutcome,
};

use super::super::channel_adapter::{ChannelAdapter, InboundBatch};
use super::super::telecom_store::TelecomAccountRecord;
use super::super::telephony::{provider_for_account, TelecomProvider};

/// Two concatenated GSM-7 segments. Carriers accept far more and silently bill
/// per segment; a long agent reply is worth splitting deliberately upstream
/// rather than discovering on an invoice.
const MAX_TEXT_CHARS: usize = 306;

pub(crate) struct SmsAdapter {
    carrier: std::sync::Arc<dyn TelecomProvider>,
}

impl SmsAdapter {
    /// Build the adapter for a telephony account. `secret` is that account's
    /// carrier credential, already read from the keychain by the caller.
    pub(crate) fn new(account: &TelecomAccountRecord, secret: String) -> Result<Self, String> {
        Ok(Self::with_carrier(provider_for_account(account, secret)?))
    }

    fn with_carrier(carrier: std::sync::Arc<dyn TelecomProvider>) -> Self {
        Self { carrier }
    }
}

#[async_trait]
impl ChannelAdapter for SmsAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Sms
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            max_text_chars: MAX_TEXT_CHARS,
            supports_threads: false,
            // MMS is a different endpoint on every carrier and a different
            // billing line; text only until one is actually wired.
            supports_attachments: false,
            supports_mention_metadata: false,
            supports_idempotency_key: true,
            supports_delivery_receipts: true,
            ..ProviderCapabilities::minimal(ChannelKind::Sms, InboundTransport::Webhook)
        }
    }

    async fn probe(&self) -> ChannelHealth {
        // One account, one credential: whether the carrier answers is the same
        // question for texts and for calls.
        self.carrier.probe().await
    }

    async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
        // Carriers post to us — see the module doc.
        Ok(InboundBatch::default())
    }

    async fn send(&self, message: &OutboundMessage) -> SendOutcome {
        // An SMS conversation is identified by the peer's number, which is what
        // `telecom_worker` put in the envelope's conversation id.
        self.carrier
            .send_sms(
                &message.conversation_id,
                &message.text,
                &message.idempotency_key,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::channels::types::HealthState;

    use super::super::super::telecom_store::{CallLimits, InboundCallPolicy, OutboundCallApproval};
    use super::super::super::telephony::{mock::MockProvider, TelecomConfig, TelecomKind};

    const NOW: i64 = 1_700_000_000_000;

    fn account() -> TelecomAccountRecord {
        TelecomAccountRecord {
            account_id: "tel-1".into(),
            kind: TelecomKind::Mock,
            label: "Support line".into(),
            enabled: true,
            carrier_account_id: "carrier-1".into(),
            from_number: "+15550000000".into(),
            credential_ref: Some("telecom:tel-1".into()),
            public_base_url: None,
            non_secret_config: serde_json::json!({}),
            inbound_policy: InboundCallPolicy::Reject,
            outbound_approval: OutboundCallApproval::Never,
            limits: CallLimits::default(),
            health: ChannelHealth {
                state: HealthState::Disconnected,
                detail: None,
                last_error: None,
                probed_at_ms: NOW,
            },
            created_at_ms: NOW,
            updated_at_ms: NOW,
        }
    }

    fn reply(text: &str) -> OutboundMessage {
        OutboundMessage {
            account_id: "tel-1".into(),
            kind: ChannelKind::Sms,
            conversation_id: "+15551234567".into(),
            thread_id: None,
            text: text.into(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            idempotency_key: "outbox-1".into(),
        }
    }

    fn mock() -> (SmsAdapter, std::sync::Arc<MockProvider>) {
        let carrier = std::sync::Arc::new(MockProvider::new(TelecomConfig {
            account_id: "tel-1".into(),
            kind: TelecomKind::Mock,
            carrier_account_id: "carrier-1".into(),
            from_number: "+15550000000".into(),
            secret: "shared-secret".into(),
            public_base_url: None,
            webhook_public_key: None,
        }));
        (SmsAdapter::with_carrier(carrier.clone()), carrier)
    }

    #[tokio::test]
    async fn a_reply_goes_out_through_the_account_s_carrier() {
        let (adapter, carrier) = mock();

        let outcome = adapter.send(&reply("on my way")).await;

        assert!(
            matches!(outcome, SendOutcome::Sent { .. }),
            "the mock carrier accepts a text: {outcome:?}"
        );
        let sent = carrier.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].to_number, "+15551234567");
        assert_eq!(sent[0].text, "on my way");
    }

    #[tokio::test]
    async fn the_outbox_row_id_is_what_the_carrier_deduplicates_on() {
        let (adapter, carrier) = mock();

        adapter.send(&reply("first")).await;

        // A crash between send and completion means the outbox retries the same
        // row, and the carrier is what collapses the two attempts — so the key
        // has to be the outbox's own id and not a fresh one per attempt.
        assert_eq!(carrier.sent_messages()[0].idempotency_key, "outbox-1");
    }

    #[tokio::test]
    async fn a_real_account_builds_its_configured_carrier() {
        let adapter = SmsAdapter::new(&account(), "shared-secret".into()).expect("adapter");

        assert_eq!(adapter.kind(), ChannelKind::Sms);
        assert!(adapter.capabilities().supports_idempotency_key);
    }

    #[tokio::test]
    async fn nothing_is_ever_polled() {
        let adapter = SmsAdapter::new(&account(), "shared-secret".into()).expect("adapter");

        let batch = adapter.poll(None).await.expect("poll");

        assert!(batch.envelopes.is_empty());
        assert!(batch.cursor.is_none(), "a webhook provider has no cursor");
    }
}
