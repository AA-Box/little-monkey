//! Web chat adapter: a browser chat page the resident daemon serves itself, on
//! its own already-configured TLS listener.
//!
//! The routes live in [`crate::daemon::remote::webchat`] — this file is the
//! adapter half: what one posted body normalizes to, what a queued reply means
//! here, and what health this account may honestly claim.
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
//! `sends_attachments(WebChat)` is `false`, and a queued message carrying one
//! is refused rather than sent without it. The page is served wherever the
//! operator already configured the remote listener — loopback by default,
//! anything wider being one decision they take once for every served surface
//! at once — so this adapter adds no bind rule of its own.
//!
//! # What `Sent` means here
//!
//! There is nobody to hand the reply to. It is already durable in
//! `channel_outbox` by the time the drain calls [`ChannelAdapter::send`], and
//! the page reads those rows back, so `Sent` says the reply is *readable by
//! that conversation's page* — not that a browser was open to read it.

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use little_monkey_lib::channels::types::{
    ChannelConversation, ChannelEnvelope, ChannelHealth, ChannelKind, ChannelSender,
    DeliveryReceipt, InboundTransport, OutboundMessage, ProviderCapabilities, SendOutcome,
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

/// The length and alphabet of what
/// [`crate::daemon::remote::webchat::mint_visitor`] produces: 32 bytes of
/// nonce-and-tag in unpadded base64url. The tag itself is verified by the
/// route, which is the half that holds the key.
fn is_minted_shape(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// A visitor's conversation, as the durable store sees it.
///
/// The hash is the point: the bearer the browser presents never reaches the
/// database, so a copied database is no way to read a transcript or send as
/// anybody. Public because the route derives the same id when it reads a
/// visitor's own messages back.
pub(crate) fn visitor_conversation_id(account_id: &str, visitor_id: &str) -> String {
    format!("web-{}", &digest(&[account_id, visitor_id])[..32])
}

/// SHA-256 over null-separated parts, hex. Separated so no concatenation of
/// two different part lists can produce the same input.
fn digest(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    format!("{:x}", hasher.finalize())
}

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

    /// Whether there is a listener to serve the page on, and where.
    ///
    /// The only thing that can be false here is the operator's own remote host
    /// configuration: there is no provider to reach and no credential to check.
    /// `connected` therefore carries the page's real URL, which is also why
    /// this kind needs no setup screen of its own.
    async fn probe(&self) -> ChannelHealth {
        let now = now_ms();
        let page = crate::daemon::store::DaemonPaths::resolve()
            .and_then(|paths| crate::daemon::remote::webchat::page_url(&paths, &self.account_id));
        match page {
            Ok(url) => ChannelHealth::connected(now, Some(format!("Chat page at {url}"))),
            Err(error) => ChannelHealth::error(now, error),
        }
    }

    /// A served surface is never polled: messages arrive at the listener's own
    /// route. `health_after_poll` knows this and moves health nowhere off the
    /// back of an empty batch.
    async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
        Ok(InboundBatch::default())
    }

    /// The reply is already where the page reads from, so this only refuses
    /// what this surface cannot carry and mints the id the echo ledger needs.
    ///
    /// Derived from the idempotency key rather than randomly, so a retry
    /// re-uses the id it already reported instead of leaving the ledger with
    /// two entries for one reply.
    async fn send(&self, message: &OutboundMessage) -> SendOutcome {
        if !message.attachments.is_empty() {
            return SendOutcome::PermanentFailure {
                error: "The web chat page carries text only; it has no file transfer in either direction".to_string(),
            };
        }
        SendOutcome::Sent {
            provider_message_id: Some(digest(&["webchat-reply", &message.idempotency_key])),
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

    /// One posted body, from a request the route already authenticated.
    ///
    /// **There is no signature to check, and that is the design rather than a
    /// gap.** What proves this request came from a browser the operator's own
    /// listener served is that it reached that listener, over its pinned TLS,
    /// carrying a visitor identifier this daemon minted and
    /// [`crate::daemon::remote::webchat`] verified against its own key before
    /// calling here. `headers` is deliberately empty on this surface: nothing
    /// in a request's headers authenticates anything, so nothing in them may
    /// name a visitor either.
    ///
    /// What is left for this function is the part that is genuinely about the
    /// message: that the body is exactly the two fields this page sends, that
    /// the identifier still has the shape the route mints, and that the text is
    /// something rather than nothing. Anything else is `Err`, which leaves no
    /// durable row.
    fn verify_and_normalize(
        &self,
        _headers: &[(String, String)],
        body: &[u8],
        _public_base_url: Option<&str>,
        now_ms: i64,
    ) -> Result<VerifiedWebhookDelivery, String> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Posted {
            visitor_id: String,
            text: String,
        }

        let posted: Posted = serde_json::from_slice(body)
            .map_err(|error| format!("That is not a web chat message: {error}"))?;
        if !is_minted_shape(&posted.visitor_id) {
            return Err("That visitor identifier was not minted by this daemon".to_string());
        }
        let text = posted.text.trim();
        if text.is_empty() {
            return Err("A web chat message carries text".to_string());
        }
        if text.chars().count() > WEBCHAT_MAX_TEXT_CHARS {
            return Err(format!(
                "A web chat message is at most {WEBCHAT_MAX_TEXT_CHARS} characters"
            ));
        }
        // The account is the adapter's own, taken from the record the route
        // matched — never from the request — so one visitor's identifier can
        // only ever address one account's conversation.
        let conversation_id = visitor_conversation_id(&self.account_id, &posted.visitor_id);
        Ok(VerifiedWebhookDelivery::messages_only(vec![
            ChannelEnvelope {
                account_id: self.account_id.clone(),
                kind: ChannelKind::WebChat,
                // Deterministic, never a UUID: a retried POST of the same message
                // in the same millisecond collapses onto the row it already wrote.
                provider_event_id: digest(&[
                    "webchat-event",
                    &self.account_id,
                    &conversation_id,
                    text,
                    &now_ms.to_string(),
                ]),
                provider_message_id: None,
                conversation: ChannelConversation::direct(conversation_id.clone()),
                sender: ChannelSender {
                    sender_id: conversation_id,
                    display_label: None,
                    // Hard-coded. A payload may not claim to be us, whatever it
                    // puts in its fields — and `deny_unknown_fields` means it
                    // cannot even put a field there to try.
                    is_self: false,
                    is_bot: false,
                },
                text: text.to_string(),
                attachments: Vec::new(),
                reply_to_provider_id: None,
                mentions_self: false,
                received_at_ms: now_ms.max(1),
                metadata: Default::default(),
            },
        ]))
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

    fn adapter() -> WebChatAdapter {
        WebChatAdapter::new(&AdapterConfig {
            account: &account(),
            secret: String::new(),
        })
        .expect("build")
    }

    /// A well-formed identifier of the shape the route mints. The tag is not
    /// checked here — that is the route's half, and it has its own tests.
    const VISITOR: &str = "AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHHIIIIJJJJKKK";

    fn post(visitor: &str, text: &str) -> Vec<u8> {
        serde_json::json!({ "visitor_id": visitor, "text": text })
            .to_string()
            .into_bytes()
    }

    #[test]
    fn an_unverified_body_earns_no_durable_row() {
        let adapter = adapter();
        for body in [
            &b"{}"[..],
            b"not json",
            // Empty and blank text: nothing was said.
            &post(VISITOR, "")[..],
            &post(VISITOR, "   ")[..],
            // An identifier the route could not have minted.
            &post("nope", "hello")[..],
            &post(&"A".repeat(44), "hello")[..],
            &post(&"AAAA/BBBCCCCDDDDEEEEFFFFGGGGHHHHIIIIJJJJKKK", "hello")[..],
        ] {
            assert!(
                adapter.verify_and_normalize(&[], body, None, 1).is_err(),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        // And a message longer than this surface declares it carries.
        let long = post(VISITOR, &"x".repeat(WEBCHAT_MAX_TEXT_CHARS + 1));
        assert!(adapter.verify_and_normalize(&[], &long, None, 1).is_err());
    }

    #[test]
    fn a_body_claiming_to_be_us_is_still_not_us() {
        // `deny_unknown_fields` is what makes this true rather than a filter
        // that has to remember every field: a payload cannot even name
        // `is_self`, `sender_id` or `account_id`, so it cannot claim one.
        let adapter = adapter();
        for extra in ["is_self", "sender_id", "account_id", "visitor_hash", "kind"] {
            let body = serde_json::json!({
                "visitor_id": VISITOR,
                "text": "hello",
                extra: "chan-web",
            })
            .to_string()
            .into_bytes();
            assert!(
                adapter.verify_and_normalize(&[], &body, None, 1).is_err(),
                "an extra {extra} field must be refused"
            );
        }
        let verified = adapter
            .verify_and_normalize(&[], &post(VISITOR, "hello"), None, 5)
            .expect("a plain message");
        let envelope = &verified.envelopes[0];
        assert!(!envelope.sender.is_self);
        assert!(!envelope.mentions_self);
        assert_eq!(envelope.account_id, "chan-web");
    }

    #[test]
    fn a_message_normalizes_to_this_visitors_own_direct_conversation() {
        let adapter = adapter();
        let verified = adapter
            .verify_and_normalize(&[], &post(VISITOR, "  hello there  "), None, 42)
            .expect("a plain message");
        assert_eq!(verified.envelopes.len(), 1);
        assert!(verified.durable_addressing.is_empty());
        let envelope = &verified.envelopes[0];
        assert_eq!(envelope.text, "hello there");
        assert_eq!(envelope.kind, ChannelKind::WebChat);
        assert_eq!(
            envelope.conversation.kind,
            little_monkey_lib::channels::types::ConversationKind::Direct
        );
        // Sender and conversation are the same hashed visitor, and the raw
        // identifier appears in neither: the database never holds the bearer.
        let expected = visitor_conversation_id("chan-web", VISITOR);
        assert_eq!(envelope.conversation.conversation_id, expected);
        assert_eq!(envelope.sender.sender_id, expected);
        assert!(expected.starts_with("web-"));
        assert!(!expected.contains(VISITOR));
        assert!(envelope.attachments.is_empty());
    }

    #[test]
    fn one_visitor_identifier_addresses_exactly_one_accounts_conversation() {
        assert_ne!(
            visitor_conversation_id("chan-web", VISITOR),
            visitor_conversation_id("chan-other", VISITOR)
        );
        assert_ne!(
            visitor_conversation_id("chan-web", VISITOR),
            visitor_conversation_id("chan-web", &VISITOR.replace('K', "L"))
        );
    }

    #[test]
    fn a_deterministic_event_id_is_stable_and_not_a_uuid() {
        let adapter = adapter();
        let first = adapter
            .verify_and_normalize(&[], &post(VISITOR, "hello"), None, 7)
            .expect("first");
        let again = adapter
            .verify_and_normalize(&[], &post(VISITOR, "hello"), None, 7)
            .expect("again");
        let later = adapter
            .verify_and_normalize(&[], &post(VISITOR, "hello"), None, 8)
            .expect("later");
        let id = |value: &VerifiedWebhookDelivery| value.envelopes[0].provider_event_id.clone();
        assert_eq!(id(&first), id(&again));
        assert_ne!(id(&first), id(&later));
        assert_eq!(id(&first).len(), 64);
        assert!(id(&first).chars().all(|c| c.is_ascii_hexdigit()));
        assert!(uuid::Uuid::parse_str(&id(&first)).is_err());
    }

    #[tokio::test]
    async fn a_reply_is_sent_with_a_stable_id_and_a_file_is_refused() {
        let adapter = adapter();
        let mut message = OutboundMessage {
            account_id: "chan-web".into(),
            kind: ChannelKind::WebChat,
            conversation_id: visitor_conversation_id("chan-web", VISITOR),
            thread_id: None,
            text: "an answer".into(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            idempotency_key: "job-1:0".into(),
        };
        let SendOutcome::Sent {
            provider_message_id,
        } = adapter.send(&message).await
        else {
            panic!("a queued reply is readable by the page");
        };
        let id = provider_message_id.expect("an id for the echo ledger");
        // A retry must not mint a second id for one reply.
        let SendOutcome::Sent {
            provider_message_id: again,
        } = adapter.send(&message).await
        else {
            panic!("retry");
        };
        assert_eq!(again.as_deref(), Some(id.as_str()));

        message.attachments = vec![little_monkey_lib::channels::types::OutboundAttachment {
            artifact_id: "blob-1".into(),
            filename: Some("photo.png".into()),
            mime_type: Some("image/png".into()),
        }];
        assert!(matches!(
            adapter.send(&message).await,
            SendOutcome::PermanentFailure { .. }
        ));
    }
}
