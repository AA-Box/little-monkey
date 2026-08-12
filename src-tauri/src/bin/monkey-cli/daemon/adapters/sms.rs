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
/// Carriers fetch promptly; the window only has to cover a retry or two. The
/// whole query string is a bearer capability for that window — worth keeping
/// out of proxy logs, and worth expiring quickly.
const MEDIA_FILE_TTL_MS: i64 = 10 * 60 * 1_000;

/// One media item per message. Carriers accept more, and every one of them
/// bills per segment for it; a reply that quietly sends six pictures is a bill
/// nobody agreed to.
const MAX_ATTACHMENTS_PER_MESSAGE: usize = 1;

/// What a carrier will accept, and how big.
///
/// Photographs get the larger allowance because that is what people send;
/// everything else is capped an order of magnitude lower, because a large
/// non-image attachment on MMS is nearly always a mistake and always a bill.
/// A type not on this list is refused rather than sent as bytes with no name:
/// a carrier that cannot tell what it received delivers nothing useful.
const MEDIA_TYPES: &[(&str, usize)] = &[
    ("image/jpeg", 5_000_000),
    ("image/png", 5_000_000),
    ("image/gif", 5_000_000),
    ("image/webp", 500_000),
    ("audio/mpeg", 500_000),
    ("audio/wav", 500_000),
    ("video/mp4", 500_000),
    ("application/pdf", 500_000),
];

/// Identify an attachment from its own first bytes.
///
/// The declared type is whatever produced the artifact said it was, which is
/// not the same as what the bytes are — and it is the bytes a carrier fetches.
/// Sniffing is the only claim this process can actually stand behind, and it is
/// also what the file route serves the attachment as.
pub(crate) fn sniff_media_type(bytes: &[u8]) -> Option<&'static str> {
    let starts = |prefix: &[u8]| bytes.starts_with(prefix);
    if starts(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if starts(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return Some("image/gif");
    }
    if starts(b"RIFF") && bytes.len() > 12 && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if starts(b"RIFF") && bytes.len() > 12 && &bytes[8..12] == b"WAVE" {
        return Some("audio/wav");
    }
    if starts(b"ID3") || starts(&[0xFF, 0xFB]) || starts(&[0xFF, 0xF3]) {
        return Some("audio/mpeg");
    }
    if bytes.len() > 12 && &bytes[4..8] == b"ftyp" {
        return Some("video/mp4");
    }
    if starts(b"%PDF-") {
        return Some("application/pdf");
    }
    None
}

/// The cap for one media type, or `None` when a carrier will not take it.
pub(crate) fn media_limit(media_type: &str) -> Option<usize> {
    MEDIA_TYPES
        .iter()
        .find(|(known, _)| *known == media_type)
        .map(|(_, limit)| *limit)
}

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
        if message.attachments.len() > MAX_ATTACHMENTS_PER_MESSAGE {
            return Err(format!(
                "A text can carry {MAX_ATTACHMENTS_PER_MESSAGE} attachment; this reply has {}.",
                message.attachments.len()
            ));
        }
        let largest = MEDIA_TYPES
            .iter()
            .map(|(_, limit)| *limit)
            .max()
            .unwrap_or_default();
        let store = little_monkey_lib::artifact_store::ArtifactStore::with_max_blob_size(
            media.app_data_dir.join("content-v1"),
            largest as u64,
        )
        .map_err(|error| error.to_string())?;
        let expires_at_ms = now_ms()? + MEDIA_FILE_TTL_MS;
        let base = media.public_base_url.trim_end_matches('/');
        let mut urls = Vec::new();
        for attachment in &message.attachments {
            let bytes = store
                .read(&attachment.artifact_id)
                .map_err(|error| format!("That attachment could not be read: {error}"))?;
            let Some(media_type) = sniff_media_type(&bytes) else {
                return Err(
                    "That attachment is not a type a carrier will deliver, so it was not sent."
                        .to_string(),
                );
            };
            let limit = media_limit(media_type).unwrap_or_default();
            if bytes.len() > limit {
                return Err(format!(
                    "That {media_type} attachment is {} bytes, over the {limit}-byte limit for MMS.",
                    bytes.len()
                ));
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
        // A real PNG header, because the pipeline identifies media by its bytes.
        let blob = store
            .put(b"\x89PNG\r\n\x1a\nand then some pixels")
            .expect("put");
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

    #[test]
    fn an_attachment_is_identified_by_its_own_bytes() {
        assert_eq!(
            sniff_media_type(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
        assert_eq!(
            sniff_media_type(b"\x89PNG\r\n\x1a\nrest"),
            Some("image/png")
        );
        assert_eq!(sniff_media_type(b"GIF89a..."), Some("image/gif"));
        assert_eq!(sniff_media_type(b"%PDF-1.7"), Some("application/pdf"));
        // A file that says it is a PNG and is not gets no claim made for it.
        assert_eq!(sniff_media_type(b"this is just text"), None);
    }

    #[test]
    fn a_photograph_gets_more_room_than_everything_else() {
        assert_eq!(media_limit("image/jpeg"), Some(5_000_000));
        assert_eq!(media_limit("application/pdf"), Some(500_000));
        assert_eq!(media_limit("application/x-msdownload"), None);
    }

    #[tokio::test]
    async fn an_attachment_a_carrier_cannot_identify_is_refused() {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-mms-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let store = little_monkey_lib::artifact_store::ArtifactStore::new(root.join("content-v1"))
            .expect("store");
        let blob = store.put(b"not a media file at all").expect("put");
        let mut with_url = account();
        with_url.public_base_url = Some("https://calls.example.test".into());
        let adapter =
            SmsAdapter::new(&with_url, "carrier-secret".into(), root.clone()).expect("adapter");
        let mut message = reply("look at this");
        message
            .attachments
            .push(little_monkey_lib::channels::types::OutboundAttachment {
                artifact_id: blob.id,
                filename: Some("payload.png".into()),
                mime_type: Some("image/png".into()),
            });

        let error = adapter.media_urls(&message).expect_err("refused");

        assert!(
            error.contains("not a type a carrier will deliver"),
            "the declared type is not evidence: {error}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_reply_carrying_several_attachments_is_refused_rather_than_billed() {
        let mut with_url = account();
        with_url.public_base_url = Some("https://calls.example.test".into());
        let adapter =
            SmsAdapter::new(&with_url, "secret".into(), std::env::temp_dir()).expect("adapter");
        let mut message = reply("two things");
        for _ in 0..2 {
            message
                .attachments
                .push(little_monkey_lib::channels::types::OutboundAttachment {
                    artifact_id: "artifact-1".into(),
                    filename: None,
                    mime_type: None,
                });
        }

        let error = adapter.media_urls(&message).expect_err("refused");

        assert!(error.contains("1 attachment"), "{error}");
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
