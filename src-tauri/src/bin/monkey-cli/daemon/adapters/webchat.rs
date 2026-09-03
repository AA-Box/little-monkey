//! Web chat adapter: a browser chat page the resident daemon serves itself, on
//! its own already-configured TLS listener.
//!
//! **This file is a stub.** The kind, the transport classification and the
//! capability declaration are already the real ones; normalization, the served
//! routes and the probe are not implemented yet and say so rather than
//! reporting a health this adapter has not earned.
//!
//! # There is no provider here
//!
//! Every other kind in this directory talks to somebody else's service. This
//! one does not: the page, the POST it makes and the replies it polls for are
//! all served by the remote listener in `daemon/remote/` — the same one that
//! already serves the controller UI, under the same pinned certificate. That is
//! why the transport is [`InboundTransport::Served`] and not `Webhook`: there
//! is no provider to register a callback with and no public URL an operator
//! owes anyone. It is also why this kind stays in
//! [`build_webhook_adapter`](super::build_webhook_adapter)'s refusal arm — the
//! generic `POST /v1/channels/<account>` listener is the one operators are told
//! to publish through a proxy or a tunnel, and admitting web chat there would
//! be a second, unguarded way in.
//!
//! # A visitor is a name, not a permission
//!
//! The daemon mints the identifier — not the page — and the browser keeps it in
//! its own storage. It grants no authority to *run* anything: who may cause a
//! run is decided where it always is —
//! `channel_ingress::accept_channel_envelope`, under the account's ordinary
//! sender policy, which for a direct conversation defaults to pairing. A
//! first-time visitor is therefore answered with a pairing code through this
//! same account's outbox, exactly like a stranger on any other provider, and
//! the operator approves them with `monkey channels approve <account> <sender>`
//! after reading the pending list. There is no second pairing store and no
//! credential of our own, which is why
//! [`credential_required`](crate::daemon::channel_adapter::credential_required)
//! answers `false` for this kind.
//!
//! It is not nothing, though, and the doc that says so would be wrong: the
//! identifier *addresses* one conversation's transcript, which the page reads
//! back, so it is that conversation's bearer and is minted here rather than
//! chosen by the browser. It is hashed at the trust boundary before it becomes
//! a conversation or sender id, so the durable database never holds the bearer
//! a browser presents and a copied database stays useless.
//!
//! # What it deliberately does not do
//!
//! Nothing is uploaded or downloaded in either direction, so
//! `sends_attachments(WebChat)` is `false`. Loopback is the default, and a
//! listener that binds a wildcard or multicast address, or port zero, is
//! refused rather than served.

use async_trait::async_trait;
use little_monkey_lib::channels::types::{
    ChannelHealth, ChannelKind, DeliveryReceipt, InboundTransport, OutboundMessage,
    ProviderCapabilities, SendOutcome,
};

use crate::daemon::channel_adapter::{
    AdapterConfig, ChannelAdapter, InboundBatch, VerifiedWebhookDelivery, WebhookAck,
    WebhookChannelAdapter,
};

/// What one browser message may carry. The route caps the request body too.
/// Nothing outside this file reads `ProviderCapabilities::max_text_chars` —
/// there is no worker-side chunker — so this is a declaration to the agent's
/// tool schema and the setup UI, and this adapter is what has to honour it.
const WEBCHAT_MAX_TEXT_CHARS: usize = 4_000;

pub struct WebChatAdapter {
    account_id: String,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

impl WebChatAdapter {
    /// Nothing to read: this kind has no configuration and no credential. The
    /// listener it is served on is configured once, under the remote host, for
    /// every served surface at once.
    pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
        Ok(Self {
            account_id: config.account.account_id.clone(),
        })
    }
}

#[async_trait]
impl ChannelAdapter for WebChatAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::WebChat
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            max_text_chars: WEBCHAT_MAX_TEXT_CHARS,
            ..ProviderCapabilities::minimal(ChannelKind::WebChat, InboundTransport::Served)
        }
    }

    async fn probe(&self) -> ChannelHealth {
        ChannelHealth::error(
            now_ms(),
            format!(
                "The web chat surface is not implemented yet, so account {} serves no page.",
                self.account_id
            ),
        )
    }

    /// A served surface is never polled: messages arrive at the listener's own
    /// route. `health_after_poll` knows this and moves health nowhere off the
    /// back of an empty batch.
    async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
        Ok(InboundBatch::default())
    }

    async fn send(&self, _message: &OutboundMessage) -> SendOutcome {
        SendOutcome::PermanentFailure {
            error: "The web chat surface is not implemented yet".to_string(),
        }
    }
}

impl WebhookChannelAdapter for WebChatAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::WebChat
    }

    fn account_id(&self) -> &str {
        &self.account_id
    }

    /// The page reads the body, so it gets JSON rather than an empty 200.
    fn ack(&self) -> WebhookAck {
        WebhookAck::json_ok()
    }

    fn verify_and_normalize(
        &self,
        _headers: &[(String, String)],
        _body: &[u8],
        _public_base_url: Option<&str>,
        _now_ms: i64,
    ) -> Result<VerifiedWebhookDelivery, String> {
        // `Err` on purpose rather than an empty delivery: an unimplemented
        // verifier must not let a body earn a durable row.
        Err("The web chat surface is not implemented yet".to_string())
    }

    fn delivery_receipts(&self, _body: &[u8], _now_ms: i64) -> Vec<DeliveryReceipt> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::channel_adapter::credential_required;
    use crate::daemon::channel_store::ChannelAccountRecord;
    use little_monkey_lib::channels::policy::ChannelAccessPolicy;
    use little_monkey_lib::channels::types::HealthState;

    fn account() -> ChannelAccountRecord {
        ChannelAccountRecord {
            account_id: "chan-web".into(),
            kind: ChannelKind::WebChat,
            label: "Web chat".into(),
            enabled: true,
            non_secret_config: serde_json::json!({}),
            credential_ref: None,
            access_policy: ChannelAccessPolicy::default(),
            health: ChannelHealth {
                state: HealthState::Disconnected,
                detail: None,
                last_error: None,
                probed_at_ms: 1,
            },
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[test]
    fn an_account_with_no_credential_is_still_a_usable_one() {
        // There is no provider to hold a token for. Demanding one would make
        // the account impossible to enable, and Security Doctor would warn
        // about it forever.
        let record = account();
        assert!(!credential_required(&record));
        let adapter = WebChatAdapter::new(&AdapterConfig {
            account: &record,
            secret: String::new(),
        })
        .expect("no configuration and no credential is the whole point");
        assert_eq!(ChannelAdapter::kind(&adapter), ChannelKind::WebChat);
    }

    #[test]
    fn nothing_is_claimed_that_the_surface_does_not_do() {
        let record = account();
        let adapter = WebChatAdapter::new(&AdapterConfig {
            account: &record,
            secret: String::new(),
        })
        .expect("build");
        let capabilities = adapter.capabilities();
        assert_eq!(capabilities.inbound_transport, InboundTransport::Served);
        assert!(!capabilities.supports_attachments);
        assert!(!capabilities.supports_threads);
        assert!(!capabilities.supports_mention_metadata);
    }

    #[test]
    fn an_unverified_body_earns_no_durable_row() {
        let record = account();
        let adapter = WebChatAdapter::new(&AdapterConfig {
            account: &record,
            secret: String::new(),
        })
        .expect("build");
        assert!(adapter.verify_and_normalize(&[], b"{}", None, 1).is_err());
    }
}
