//! Telephony: SMS and phone calls, one abstraction over several carriers.
//!
//! Two rules shape this module.
//!
//! **SMS is not a second messaging system.** An inbound text becomes the same
//! [`ChannelEnvelope`] a Telegram message becomes, goes through the same
//! `channel_ingress` gate, and is answered through the same outbox. The only
//! thing telephony adds is the transport.
//!
//! **A call is a mutation with a bill attached.** Answering the phone and
//! placing a call are separate powers: an operator who wants Little Monkey to
//! pick up has not thereby agreed to let it dial out. Outbound calls are
//! external mutations and go through the normal approval policy, never around
//! it.
//!
//! Providers here do exactly what channel adapters do — normalize, send, probe,
//! verify a signature — and never execute an agent.

use async_trait::async_trait;
use little_monkey_lib::channels::types::{ChannelEnvelope, ChannelHealth, SendOutcome};
use serde::{Deserialize, Serialize};

pub mod mock;
pub mod plivo;
pub mod telnyx;
pub mod twilio;

/// Which carrier an account speaks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelecomKind {
    Twilio,
    Telnyx,
    Plivo,
    /// A deterministic in-process carrier. The only one tests ever use, and the
    /// only one that can exist without the operator's own paid account.
    Mock,
}

impl TelecomKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TelecomKind::Twilio => "twilio",
            TelecomKind::Telnyx => "telnyx",
            TelecomKind::Plivo => "plivo",
            TelecomKind::Mock => "mock",
        }
    }

    pub fn parse(value: &str) -> Option<TelecomKind> {
        match value {
            "twilio" => Some(TelecomKind::Twilio),
            "telnyx" => Some(TelecomKind::Telnyx),
            "plivo" => Some(TelecomKind::Plivo),
            "mock" => Some(TelecomKind::Mock),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TelecomKind::Twilio => "Twilio",
            TelecomKind::Telnyx => "Telnyx",
            TelecomKind::Plivo => "Plivo",
            TelecomKind::Mock => "Test carrier",
        }
    }
}

/// What a provider is built with. The credential arrives already resolved, the
/// same way a channel adapter's does, so no provider reads the keychain.
pub struct TelecomConfig {
    pub account_id: String,
    pub kind: TelecomKind,
    /// The account identifier the carrier issues (Twilio Account SID, Telnyx
    /// API user, Plivo Auth ID). Not a secret.
    pub carrier_account_id: String,
    /// The number the operator owns, in E.164.
    pub from_number: String,
    pub secret: String,
    /// The operator's own canonical public URL, when they configured one. Only
    /// this value is ever used to reconstruct a signed URL — never a `Host` or
    /// `X-Forwarded-*` header from the request.
    pub public_base_url: Option<String>,
    /// A carrier-published public key for verifying callbacks, base64, when the
    /// carrier signs with one it does not derive from the API credential
    /// (Telnyx's Ed25519 key). Not a secret — it verifies, it does not sign.
    pub webhook_public_key: Option<String>,
}

/// Build the carrier a telephony account speaks to.
///
/// The one place a [`TelecomKind`] becomes code. A carrier that cannot be built
/// from what the operator configured is an error naming what is missing, so the
/// account simply does not run rather than half-working.
pub fn build_provider(
    config: TelecomConfig,
) -> Result<std::sync::Arc<dyn TelecomProvider>, String> {
    Ok(match config.kind {
        TelecomKind::Twilio => std::sync::Arc::new(twilio::TwilioProvider::new(config)),
        TelecomKind::Plivo => std::sync::Arc::new(plivo::PlivoProvider::new(config)),
        TelecomKind::Mock => std::sync::Arc::new(mock::MockProvider::new(config)),
        TelecomKind::Telnyx => {
            // Telnyx signs callbacks with an Ed25519 key published in the
            // portal, separate from the API key that authenticates our
            // requests. Without it a callback cannot be verified, and an
            // unverifiable callback is not something to accept anyway.
            let key = config.webhook_public_key.clone().ok_or_else(|| {
                "This Telnyx account has no webhook public key configured, so carrier callbacks cannot be verified. Copy it from the Telnyx portal into the account's settings.".to_string()
            })?;
            std::sync::Arc::new(telnyx::TelnyxProvider::new(config, &key)?)
        }
    })
}

/// Build the carrier a stored telephony account speaks to.
///
/// The credential arrives already resolved from the keychain, the same way a
/// channel adapter's does. Every caller that has an account row wants exactly
/// this mapping, so it lives here rather than being spelled out again at each
/// call site.
pub fn provider_for_account(
    account: &super::telecom_store::TelecomAccountRecord,
    secret: String,
) -> Result<std::sync::Arc<dyn TelecomProvider>, String> {
    build_provider(TelecomConfig {
        account_id: account.account_id.clone(),
        kind: account.kind,
        carrier_account_id: account.carrier_account_id.clone(),
        from_number: account.from_number.clone(),
        secret,
        public_base_url: account.public_base_url.clone(),
        webhook_public_key: account
            .non_secret_config
            .get("webhook_public_key")
            .and_then(|value| value.as_str())
            .map(str::to_string),
    })
}

/// Where a call is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    Queued,
    Ringing,
    InProgress,
    Completed,
    Failed,
    /// The carrier may or may not have placed it. Never retried automatically:
    /// a duplicated phone call is not something an apology fixes.
    NeedsReconciliation,
}

impl CallState {
    pub fn as_str(self) -> &'static str {
        match self {
            CallState::Queued => "queued",
            CallState::Ringing => "ringing",
            CallState::InProgress => "in_progress",
            CallState::Completed => "completed",
            CallState::Failed => "failed",
            CallState::NeedsReconciliation => "needs_reconciliation",
        }
    }

    pub fn parse(value: &str) -> Option<CallState> {
        match value {
            "queued" => Some(CallState::Queued),
            "ringing" => Some(CallState::Ringing),
            "in_progress" => Some(CallState::InProgress),
            "completed" => Some(CallState::Completed),
            "failed" => Some(CallState::Failed),
            "needs_reconciliation" => Some(CallState::NeedsReconciliation),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            CallState::Completed | CallState::Failed | CallState::NeedsReconciliation
        )
    }
}

/// A call the carrier accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallHandle {
    pub provider_call_id: String,
    pub state: CallState,
}

/// Everything a carrier can tell us, normalized.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TelecomEvent {
    /// An inbound text. Already a channel envelope, because that is what it
    /// becomes: SMS runs through the messaging subsystem, not beside it.
    InboundSms(Box<ChannelEnvelope>),
    /// Somebody is calling. The answer is decided by the inbound-call policy,
    /// never by the carrier.
    InboundCall {
        provider_call_id: String,
        from_number: String,
        to_number: String,
        received_at_ms: i64,
    },
    /// Progress on a call we know about.
    CallProgress {
        provider_call_id: String,
        state: CallState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// A delivery receipt for a text we sent.
    SmsStatus {
        provider_message_id: String,
        delivered: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Verified, understood, and of no interest — a carrier heartbeat, a
    /// duplicate status. Recorded as nothing rather than guessed at.
    Ignored,
}

/// What a carrier is answered with when it asks how to handle a ringing call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerDocument {
    pub content_type: &'static str,
    pub body: String,
}

/// Mint the token that lets one carrier media socket connect.
///
/// A media stream carries no signature of its own, so the URL is the
/// credential: an HMAC over the account, the call and an expiry, keyed by the
/// account's carrier secret. It is scoped to a single call and short-lived,
/// which bounds what learning one URL is worth.
pub fn media_stream_token(
    secret: &str,
    account_id: &str,
    call_id: &str,
    expires_at_ms: i64,
) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    let signature = ring::hmac::sign(
        &key,
        format!("{account_id}:{call_id}:{expires_at_ms}").as_bytes(),
    );
    signature
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Check a media socket's token. Constant-time, expiry enforced, and bound to
/// the exact call — a token for yesterday's call opens nothing today.
pub fn verify_media_stream_token(
    secret: &str,
    account_id: &str,
    call_id: &str,
    expires_at_ms: i64,
    token: &str,
    now_ms: i64,
) -> Result<(), String> {
    if now_ms > expires_at_ms {
        return Err("This media stream token has expired".to_string());
    }
    let expected = media_stream_token(secret, account_id, call_id, expires_at_ms);
    if expected.len() != token.len()
        || ring::constant_time::verify_slices_are_equal(expected.as_bytes(), token.as_bytes())
            .is_err()
    {
        return Err("This media stream token is not valid for that call".to_string());
    }
    Ok(())
}

/// Mint the token that lets a carrier fetch one attachment.
///
/// MMS is delivered by the carrier fetching a URL, so an outbound attachment
/// has to be reachable from the internet for as long as that takes. This is the
/// same construction as [`media_stream_token`] and for the same reason: the URL
/// is the credential, it names exactly one artifact, and it expires.
pub fn media_file_token(
    secret: &str,
    account_id: &str,
    artifact_id: &str,
    expires_at_ms: i64,
) -> String {
    media_stream_token(
        secret,
        account_id,
        &format!("file:{artifact_id}"),
        expires_at_ms,
    )
}

/// Check an attachment URL's token. Same rules as the media socket's.
pub fn verify_media_file_token(
    secret: &str,
    account_id: &str,
    artifact_id: &str,
    expires_at_ms: i64,
    token: &str,
    now_ms: i64,
) -> Result<(), String> {
    verify_media_stream_token(
        secret,
        account_id,
        &format!("file:{artifact_id}"),
        expires_at_ms,
        token,
        now_ms,
    )
}

/// One carrier.
///
/// Every carrier is also a [`MediaFrameCodec`], because a carrier that cannot
/// say how its own audio frames are spelled cannot carry a conversation — and
/// a default implementation would be a guess that sounds like silence on the
/// line.
#[async_trait]
pub trait TelecomProvider: Send + Sync + super::call_media::MediaFrameCodec {
    fn kind(&self) -> TelecomKind;

    /// Ask the carrier whether the credential works. The only thing that may
    /// report an account as connected.
    async fn probe(&self) -> ChannelHealth;

    /// Send one text. `idempotency_key` is the outbox row's, so a retry after a
    /// crash collapses at the carrier where the carrier supports it.
    ///
    /// `media_urls` makes it an MMS. They are signed, expiring URLs served by
    /// this daemon, because that is how every carrier takes media: it fetches
    /// it. A carrier that cannot send media must refuse rather than drop the
    /// attachment and send the text alone.
    async fn send_sms(
        &self,
        to_number: &str,
        text: &str,
        media_urls: &[String],
        idempotency_key: &str,
    ) -> SendOutcome;

    /// Place a call. The caller has already cleared this with the approval
    /// policy; a provider must never decide for itself that a call is fine.
    ///
    /// `record` is the account's own recording setting. A carrier that cannot
    /// record a call it places must refuse rather than place an unrecorded one:
    /// an operator who turned recording on may be relying on it.
    async fn place_call(
        &self,
        to_number: &str,
        answer_url: &str,
        record: bool,
    ) -> Result<CallHandle, String>;

    /// End a call we placed or answered.
    async fn hangup(&self, provider_call_id: &str) -> Result<(), String>;

    /// How this carrier spells its media-stream frames, when it streams audio
    /// at all. `None` means the carrier can ring and text but cannot hand us the
    /// audio, so a call on it is recorded and never becomes a conversation.
    fn media_stream(&self) -> Option<super::call_media::MediaStreamFormat> {
        None
    }

    /// What to answer the carrier's "somebody is calling, what now?" request
    /// with, given the media socket URL it should connect to.
    ///
    /// Carrier-specific markup, which is why it lives with the carrier. `None`
    /// from a provider that has no such document (the mock) leaves the call
    /// recorded and unanswered rather than inventing one.
    fn answer_instructions(&self, _media_url: &str) -> Option<AnswerDocument> {
        None
    }

    /// Verify a carrier callback over the exact bytes received and normalize
    /// it. An unverified body must return `Err` and leave no trace: it has not
    /// earned a durable row.
    fn verify_webhook(
        &self,
        headers: &[(String, String)],
        body: &[u8],
        now_ms: i64,
    ) -> Result<TelecomEvent, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000_000;

    #[test]
    fn a_media_token_only_opens_the_call_it_was_minted_for() {
        let token = media_stream_token("carrier-secret", "tel-1", "call-1", NOW + 60_000);

        assert!(verify_media_stream_token(
            "carrier-secret",
            "tel-1",
            "call-1",
            NOW + 60_000,
            &token,
            NOW
        )
        .is_ok());
        for (secret, account, call, label) in [
            ("other-secret", "tel-1", "call-1", "a different credential"),
            ("carrier-secret", "tel-2", "call-1", "a different account"),
            ("carrier-secret", "tel-1", "call-2", "a different call"),
        ] {
            assert!(
                verify_media_stream_token(secret, account, call, NOW + 60_000, &token, NOW)
                    .is_err(),
                "{label} must not open this socket"
            );
        }
    }

    #[test]
    fn an_expired_media_token_is_refused_even_though_it_is_authentic() {
        let token = media_stream_token("carrier-secret", "tel-1", "call-1", NOW);

        let error =
            verify_media_stream_token("carrier-secret", "tel-1", "call-1", NOW, &token, NOW + 1)
                .expect_err("refused");

        assert!(error.contains("expired"));
    }

    #[test]
    fn every_streaming_carrier_answers_with_its_own_document() {
        let cases = [
            (TelecomKind::Twilio, "<Connect>", "streamSid"),
            (TelecomKind::Telnyx, "<Connect>", "stream_id"),
            (TelecomKind::Plivo, "<Stream", "streamId"),
        ];
        for (kind, expected_markup, expected_stream_key) in cases {
            let provider = build_provider(config(kind)).expect("provider");

            let format = provider.media_stream().expect("streams audio");
            assert_eq!(
                format.stream_id_path.last().copied(),
                Some(expected_stream_key)
            );
            let document = provider
                .answer_instructions("wss://calls.example.test/v1/telecom/tel-1/media?sig=a&b=1")
                .expect("answers");
            assert!(document.body.contains(expected_markup), "{kind:?}");
            assert!(
                document.body.contains("&amp;b=1"),
                "the URL is escaped for the markup it is placed in: {}",
                document.body
            );
        }
    }

    #[test]
    fn each_carrier_spells_an_outbound_frame_its_own_way() {
        use crate::daemon::call_media::MediaFrameCodec;

        let twilio = build_provider(config(TelecomKind::Twilio)).expect("twilio");
        let frame: serde_json::Value =
            serde_json::from_str(&twilio.encode_media_frame("QUJD", "MZ-stream")).expect("json");
        assert_eq!(frame["event"], "media");
        assert_eq!(
            frame["streamSid"], "MZ-stream",
            "Twilio discards a frame that does not echo its stream id"
        );
        assert_eq!(
            twilio.media_stream().expect("streams").outbound_chunk_ms,
            20
        );

        let plivo = build_provider(config(TelecomKind::Plivo)).expect("plivo");
        let frame: serde_json::Value =
            serde_json::from_str(&plivo.encode_media_frame("QUJD", "ignored")).expect("json");
        assert_eq!(frame["event"], "playAudio");
        assert_eq!(
            frame["media"]["contentType"], "audio/x-mulaw",
            "Plivo refuses audio that does not say what it is"
        );
        assert_eq!(frame["media"]["sampleRate"], 8000);
        assert_eq!(
            plivo.media_stream().expect("streams").stream_id_path,
            ["start", "streamId"],
            "Plivo nests its stream id inside the start event"
        );

        let telnyx = build_provider(config(TelecomKind::Telnyx)).expect("telnyx");
        let frame: serde_json::Value =
            serde_json::from_str(&telnyx.encode_media_frame("QUJD", "tx-stream")).expect("json");
        assert_eq!(frame["event"], "media");
        assert_eq!(frame["stream_id"], "tx-stream");
        assert_eq!(
            telnyx.media_stream().expect("streams").outbound_chunk_ms,
            1_000,
            "Telnyx accepts at most one payload a second, so frames are a second long"
        );
    }

    #[test]
    fn a_media_file_token_is_bound_to_one_artifact_and_expires() {
        let token = media_file_token("carrier-secret", "tel-1", "artifact-1", NOW + 60_000);

        assert!(verify_media_file_token(
            "carrier-secret",
            "tel-1",
            "artifact-1",
            NOW + 60_000,
            &token,
            NOW
        )
        .is_ok());
        assert!(
            verify_media_file_token(
                "carrier-secret",
                "tel-1",
                "artifact-2",
                NOW + 60_000,
                &token,
                NOW
            )
            .is_err(),
            "a URL for one attachment must not fetch another"
        );
        assert!(
            verify_media_file_token(
                "carrier-secret",
                "tel-1",
                "artifact-1",
                NOW,
                &token,
                NOW + 1
            )
            .is_err(),
            "an expired URL fetches nothing"
        );
        // A media-socket token must not open the file route either: the two are
        // separate grants over the same credential.
        let socket_token =
            media_stream_token("carrier-secret", "tel-1", "artifact-1", NOW + 60_000);
        assert!(verify_media_file_token(
            "carrier-secret",
            "tel-1",
            "artifact-1",
            NOW + 60_000,
            &socket_token,
            NOW
        )
        .is_err());
    }

    fn config(kind: TelecomKind) -> TelecomConfig {
        TelecomConfig {
            account_id: "tel-1".into(),
            kind,
            carrier_account_id: "carrier-1".into(),
            from_number: "+15550000000".into(),
            secret: "secret".into(),
            public_base_url: Some("https://calls.example.test".into()),
            webhook_public_key: Some(STANDARD_TEST_KEY.into()),
        }
    }

    /// A syntactically valid Ed25519 key, so the Telnyx provider can be built in
    /// a test that is about answer documents rather than about signatures.
    const STANDARD_TEST_KEY: &str = "MCowBQYDK2VwAyEAGb9ECWmEzf6FQbrBZ9w7lshQhqowtrbLDFw4rXAxZuE=";

    #[test]
    fn carrier_tokens_round_trip() {
        for kind in [
            TelecomKind::Twilio,
            TelecomKind::Telnyx,
            TelecomKind::Plivo,
            TelecomKind::Mock,
        ] {
            assert_eq!(TelecomKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(TelecomKind::parse("carrier-pigeon"), None);
    }

    #[test]
    fn call_states_round_trip_and_know_when_they_are_over() {
        for state in [
            CallState::Queued,
            CallState::Ringing,
            CallState::InProgress,
            CallState::Completed,
            CallState::Failed,
            CallState::NeedsReconciliation,
        ] {
            assert_eq!(CallState::parse(state.as_str()), Some(state));
        }
        assert!(!CallState::Ringing.is_terminal());
        assert!(CallState::Completed.is_terminal());
        // An unprovable call is terminal for the automatic path: nothing may
        // retry it.
        assert!(CallState::NeedsReconciliation.is_terminal());
    }
}
