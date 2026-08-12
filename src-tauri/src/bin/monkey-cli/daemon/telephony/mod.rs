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

/// One carrier.
#[async_trait]
pub trait TelecomProvider: Send + Sync {
    fn kind(&self) -> TelecomKind;

    /// Ask the carrier whether the credential works. The only thing that may
    /// report an account as connected.
    async fn probe(&self) -> ChannelHealth;

    /// Send one text. `idempotency_key` is the outbox row's, so a retry after a
    /// crash collapses at the carrier where the carrier supports it.
    async fn send_sms(&self, to_number: &str, text: &str, idempotency_key: &str) -> SendOutcome;

    /// Place a call. The caller has already cleared this with the approval
    /// policy; a provider must never decide for itself that a call is fine.
    async fn place_call(&self, to_number: &str, answer_url: &str) -> Result<CallHandle, String>;

    /// End a call we placed or answered.
    async fn hangup(&self, provider_call_id: &str) -> Result<(), String>;

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
