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
    /// What an outbound attachment needs to become a URL a carrier can fetch:
    /// the account it is signed for, that account's own credential, and the
    /// operator's public base. Absent means this account cannot send media.
    media: Option<MediaSigning>,
}

/// Everything needed to hand a carrier a fetchable attachment URL.
struct MediaSigning {
    account_id: String,
    secret: String,
    public_base_url: String,
    app_data_dir: std::path::PathBuf,
}

/// How long a carrier has to fetch an attachment before its URL stops working.
///
/// Carriers fetch promptly; the window only has to cover a retry or two. Every
/// minute past that is a minute the URL is worth stealing.
const MEDIA_FILE_TTL_MS: i64 = 15 * 60 * 1_000;

/// The largest attachment this pipeline will hand a carrier. MMS is capped far
/// below this by every carrier anyway; the point is to refuse before reading a
/// large blob into memory.
const MAX_MMS_BYTES: usize = 5 * 1024 * 1024;

impl SmsAdapter {
    /// Build the adapter for a telephony account. `secret` is that account's
    /// carrier credential, already read from the keychain by the caller.
    pub(crate) fn new(
        account: &TelecomAccountRecord,
        secret: String,
        app_data_dir: std::path::PathBuf,
    ) -> Result<Self, String> {
        let media = account
            .public_base_url
            .clone()
            .map(|public_base_url| MediaSigning {
                account_id: account.account_id.clone(),
                secret: secret.clone(),
                public_base_url,
                app_data_dir,
            });
        Ok(Self {
            carrier: provider_for_account(account, secret)?,
            media,
        })
    }

    fn with_carrier(carrier: std::sync::Arc<dyn TelecomProvider>) -> Self {
        Self {
            carrier,
            media: None,
        }
    }

    /// Turn this message's attachments into URLs the carrier can fetch.
    ///
    /// The artifact is read here only to confirm it exists and fits: the bytes
    /// themselves are served later, by the daemon's own listener, to whoever
    /// presents the signed URL.
    fn media_urls(&self, message: &OutboundMessage) -> Result<Vec<String>, String> {
        if message.attachments.is_empty() {
            return Ok(Vec::new());
        }
        let Some(media) = &self.media else {
            return Err(
                "This number has no public URL configured, so a carrier has nowhere to fetch an attachment from."
                    .to_string(),
            );
        };
        let store = little_monkey_lib::artifact_store::ArtifactStore::with_max_blob_size(
            media.app_data_dir.join("content-v1"),
            MAX_MMS_BYTES as u64,
        )
        .map_err(|error| error.to_string())?;
        let expires_at_ms = now_ms()? + MEDIA_FILE_TTL_MS;
        let base = media.public_base_url.trim_end_matches('/');
        let mut urls = Vec::new();
        for attachment in &message.attachments {
            let bytes = store
                .read(&attachment.artifact_id)
                .map_err(|error| format!("That attachment could not be read: {error}"))?;
            if bytes.len() > MAX_MMS_BYTES {
                return Err("That attachment is too large to send as MMS.".to_string());
            }
            let signature = super::super::telephony::media_file_token(
                &media.secret,
                &media.account_id,
                &attachment.artifact_id,
                expires_at_ms,
            );
            urls.push(format!(
                "{base}/v1/telecom/{}/file?artifact={}&exp={expires_at_ms}&sig={signature}",
                media.account_id, attachment.artifact_id
            ));
        }
        Ok(urls)
    }
}

fn now_ms() -> Result<i64, String> {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "System clock is before the Unix epoch".to_string())?
            .as_millis(),
    )
    .map_err(|_| "System clock is beyond the supported range".to_string())
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
            // MMS, when the operator configured a public URL for the carrier to
            // fetch the attachment from.
            supports_attachments: self.media.is_some(),
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
        // A reply to a phone call is spoken, not texted. Call conversations are
        // `call:<call_id>`, and the line is either still up — in which case the
        // conversation loop synthesizes this — or it is not, which no amount of
        // retrying fixes.
        if let Some(call_id) = message.conversation_id.strip_prefix("call:") {
            return match super::super::call_media::speak_on_call(call_id, &message.text) {
                Ok(()) => SendOutcome::Sent {
                    provider_message_id: None,
                },
                Err(error) => SendOutcome::PermanentFailure { error },
            };
        }
        // An attachment becomes a signed, expiring URL this daemon serves; every
        // carrier sends media by fetching one.
        let media_urls = match self.media_urls(message) {
            Ok(urls) => urls,
            Err(error) => return SendOutcome::PermanentFailure { error },
        };
        // An SMS conversation is identified by the peer's number, which is what
        // `telecom_worker` put in the envelope's conversation id.
        self.carrier
            .send_sms(
                &message.conversation_id,
                &message.text,
                &media_urls,
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
        let adapter = SmsAdapter::new(&account(), "shared-secret".into(), std::env::temp_dir())
            .expect("adapter");

        assert_eq!(adapter.kind(), ChannelKind::Sms);
        assert!(adapter.capabilities().supports_idempotency_key);
    }

    #[tokio::test]
    async fn an_attachment_becomes_a_signed_url_the_carrier_can_fetch() {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-mms-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let store = little_monkey_lib::artifact_store::ArtifactStore::new(root.join("content-v1"))
            .expect("store");
        let blob = store.put(b"a tiny png").expect("put");
        let mut with_url = account();
        with_url.public_base_url = Some("https://calls.example.test".into());
        let adapter =
            SmsAdapter::new(&with_url, "carrier-secret".into(), root.clone()).expect("adapter");
        let mut message = reply("here it is");
        message
            .attachments
            .push(little_monkey_lib::channels::types::OutboundAttachment {
                artifact_id: blob.id.clone(),
                filename: Some("photo.png".into()),
                mime_type: Some("image/png".into()),
            });

        let urls = adapter.media_urls(&message).expect("urls");

        assert_eq!(urls.len(), 1);
        let url = &urls[0];
        assert!(url.starts_with("https://calls.example.test/v1/telecom/tel-1/file?"));
        assert!(url.contains(&format!("artifact={}", blob.id)));
        // The signature must be the account's own, over this artifact: another
        // artifact's URL is not accepted by the route that serves them.
        let expires: i64 = url
            .split("exp=")
            .nth(1)
            .and_then(|rest| rest.split('&').next())
            .and_then(|value| value.parse().ok())
            .expect("expiry");
        let signature = url.split("sig=").nth(1).expect("signature");
        assert!(crate::daemon::telephony::verify_media_file_token(
            "carrier-secret",
            "tel-1",
            &blob.id,
            expires,
            signature,
            expires - 1,
        )
        .is_ok());
        assert!(crate::daemon::telephony::verify_media_file_token(
            "carrier-secret",
            "tel-1",
            "some-other-artifact",
            expires,
            signature,
            expires - 1,
        )
        .is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_number_with_no_public_url_refuses_the_attachment_rather_than_dropping_it() {
        let mut without_url = account();
        without_url.public_base_url = None;
        let adapter =
            SmsAdapter::new(&without_url, "secret".into(), std::env::temp_dir()).expect("adapter");
        let mut message = reply("here it is");
        message
            .attachments
            .push(little_monkey_lib::channels::types::OutboundAttachment {
                artifact_id: "artifact-1".into(),
                filename: None,
                mime_type: None,
            });

        let outcome = adapter.send(&message).await;

        assert!(
            matches!(outcome, SendOutcome::PermanentFailure { ref error } if error.contains("nowhere to fetch")),
            "{outcome:?}"
        );
        assert!(!adapter.capabilities().supports_attachments);
    }

    #[tokio::test]
    async fn nothing_is_ever_polled() {
        let adapter = SmsAdapter::new(&account(), "shared-secret".into(), std::env::temp_dir())
            .expect("adapter");

        let batch = adapter.poll(None).await.expect("poll");

        assert!(batch.envelopes.is_empty());
        assert!(batch.cursor.is_none(), "a webhook provider has no cursor");
    }
}
