//! The seam every messaging provider plugs into.
//!
//! An adapter does exactly two translations — provider wire format into
//! [`ChannelEnvelope`] on the way in, [`OutboundMessage`] into provider wire
//! format on the way out — plus a health probe. It never executes an agent,
//! never resolves a route, never decides who may talk, and never touches the
//! run ledger. Everything downstream of `poll` goes through
//! `channel_ingress::plan_channel_ingress`, which is the one gate.
//!
//! Two shapes of provider exist and both live behind this trait:
//!
//! - **Polling / socket** providers own their own inbound loop and hand batches
//!   to [`ChannelAdapter::poll`]. Their resume state is a bounded cursor stored
//!   in `channel_cursors` — never a credential.
//! - **Webhook** providers cannot be polled. They implement
//!   [`WebhookChannelAdapter`] instead, verify their own signature over the raw
//!   body, and are driven by the daemon's existing webhook listener.
//!
//! # Credentials
//!
//! An adapter is constructed with the secret already resolved by
//! [`ChannelSecrets`], so no adapter reads the keychain itself and no adapter
//! can be built with a secret it was not handed. `ChannelAccountRecord` carries
//! only a `credential_ref` — the keychain account name — which is what keeps a
//! copied database useless.

use super::channel_store::ChannelAccountRecord;
use async_trait::async_trait;
use little_monkey_lib::channels::types::{
    ChannelEnvelope, ChannelHealth, ChannelKind, DeliveryReceipt, OutboundMessage,
    ProviderCapabilities, SendOutcome,
};

/// One batch of inbound events plus the cursor to resume from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InboundBatch {
    pub envelopes: Vec<ChannelEnvelope>,
    /// Bounded resume token to persist. `None` leaves the stored cursor alone,
    /// which is what a provider with no resume concept wants.
    pub cursor: Option<String>,
}

/// What an adapter needs to exist: the account row plus its resolved secret.
pub struct AdapterConfig<'a> {
    pub account: &'a ChannelAccountRecord,
    /// The credential, already read from the keychain. Empty when the account
    /// has no `credential_ref`, which every adapter must reject in `probe`
    /// rather than by panicking.
    pub secret: String,
}

#[async_trait]
pub trait ChannelAdapter: Send + Sync {
    fn kind(&self) -> ChannelKind;

    fn capabilities(&self) -> ProviderCapabilities;

    /// Ask the provider who we are. This — and only this — is what may write
    /// `HealthState::Connected`: saved configuration is not a connection.
    async fn probe(&self) -> ChannelHealth;

    /// Fetch the next batch of inbound events, resuming from `cursor`.
    ///
    /// Long-polling adapters block here for their own bounded interval. A
    /// webhook-driven adapter returns an empty batch forever and is not
    /// scheduled by the poll loop.
    async fn poll(&self, cursor: Option<&str>) -> Result<InboundBatch, String>;

    /// Send one message. The returned [`SendOutcome`] is what decides whether
    /// the outbox retries, gives up, or parks the row for reconciliation —
    /// an adapter that cannot prove a request never left the machine must say
    /// `NeedsReconciliation` rather than `RetryableFailure`.
    async fn send(&self, message: &OutboundMessage) -> SendOutcome;
}

/// Providers that are delivered to rather than polled.
///
/// Signature verification happens here, over the exact bytes received, because
/// only the adapter knows the provider's canonicalization. The daemon's
/// listener never reconstructs a URL from `Host` or `X-Forwarded-*` headers for
/// this purpose — those are attacker-controlled — and passes the configured
/// public base URL instead when a provider's signature covers it.
pub trait WebhookChannelAdapter: Send + Sync {
    fn kind(&self) -> ChannelKind;

    /// Verify and normalize one delivery. `headers` are lowercase-keyed.
    ///
    /// Returning `Err` rejects the delivery without recording anything, which
    /// is the correct answer for a bad signature: an unverified body has not
    /// earned a row in the durable event log.
    fn verify_and_normalize(
        &self,
        headers: &[(String, String)],
        body: &[u8],
        public_base_url: Option<&str>,
        now_ms: i64,
    ) -> Result<Vec<ChannelEnvelope>, String>;

    /// Delivery progress this same body reports for messages already sent.
    ///
    /// Separate from the envelopes because a receipt is not a turn: nobody is
    /// speaking, so nothing runs. It is only ever called on a body whose
    /// signature already verified, and the default is empty for providers that
    /// report nothing.
    fn delivery_receipts(&self, _body: &[u8], _now_ms: i64) -> Vec<DeliveryReceipt> {
        Vec::new()
    }

    /// Answer this provider's webhook-registration handshake, if it has one.
    ///
    /// Meta will not save a callback URL until the endpoint echoes the
    /// `hub.challenge` it sends, so without an answer here an operator cannot
    /// finish WhatsApp setup at all. `query` is the raw query string of a GET
    /// to the account's callback path.
    ///
    /// `None` refuses, which is both the default and the right answer for
    /// every provider that has no such handshake — the route turns it into a
    /// flat 403 rather than a hint about what would have worked.
    fn verification_challenge(&self, _query: &str) -> Option<String> {
        None
    }
}

/// Keychain-backed credential storage for messaging accounts.
///
/// A trait so tests never touch the real keychain — the CI machines have none —
/// and so a distributor can substitute a different store without every adapter
/// learning about it.
pub trait ChannelSecrets: Send + Sync {
    fn put(&self, credential_ref: &str, secret: &str) -> Result<(), String>;
    fn get(&self, credential_ref: &str) -> Result<String, String>;
    fn delete(&self, credential_ref: &str) -> Result<(), String>;
}

pub struct KeyringChannelSecrets;

impl KeyringChannelSecrets {
    fn entry(credential_ref: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(
            &little_monkey_lib::channels::KEYCHAIN_SERVICE,
            credential_ref,
        )
        .map_err(|error| format!("Failed to open the messaging keychain entry: {error}"))
    }
}

impl ChannelSecrets for KeyringChannelSecrets {
    fn put(&self, credential_ref: &str, secret: &str) -> Result<(), String> {
        if secret.is_empty() || secret.len() > 8192 {
            return Err("A messaging credential must contain 1-8192 bytes".to_string());
        }
        Self::entry(credential_ref)?
            .set_password(secret)
            .map_err(|error| format!("Failed to save the messaging credential: {error}"))
    }

    fn get(&self, credential_ref: &str) -> Result<String, String> {
        Self::entry(credential_ref)?
            .get_password()
            .map_err(|error| format!("Failed to read the messaging credential: {error}"))
    }

    fn delete(&self, credential_ref: &str) -> Result<(), String> {
        match Self::entry(credential_ref)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(format!(
                "Failed to delete the messaging credential: {error}"
            )),
        }
    }
}

/// An in-memory secret store for tests and for a build with no keychain.
#[derive(Default)]
pub struct MemoryChannelSecrets {
    entries: std::sync::Mutex<std::collections::BTreeMap<String, String>>,
}

impl ChannelSecrets for MemoryChannelSecrets {
    fn put(&self, credential_ref: &str, secret: &str) -> Result<(), String> {
        self.entries
            .lock()
            .map_err(|_| "channel secret store poisoned".to_string())?
            .insert(credential_ref.to_string(), secret.to_string());
        Ok(())
    }

    fn get(&self, credential_ref: &str) -> Result<String, String> {
        self.entries
            .lock()
            .map_err(|_| "channel secret store poisoned".to_string())?
            .get(credential_ref)
            .cloned()
            .ok_or_else(|| format!("No stored credential for '{credential_ref}'"))
    }

    fn delete(&self, credential_ref: &str) -> Result<(), String> {
        self.entries
            .lock()
            .map_err(|_| "channel secret store poisoned".to_string())?
            .remove(credential_ref);
        Ok(())
    }
}
