//! Provider-independent durable conversation ingress.
//!
//! Every externally originated turn — a Telegram DM, a phone call transcript, a
//! message a paired phone submits, a task a peer node hands over — becomes one
//! [`ConversationIngress`] record before it becomes a run. That record is what
//! makes the turn durable (it survives a restart), deduplicated (a redelivered
//! webhook collapses onto the existing row) and reproducible (the route it will
//! execute under is frozen onto it, so editing a route mid-flight cannot change
//! a message already in the queue).
//!
//! Nothing here executes anything. Ingress is a description of a turn; the
//! daemon's existing queue is what runs it.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::channels::routing::{ChannelRoute, RouteTarget};
use crate::channels::types::{BoundedMetadata, ChannelAttachment, ChannelEnvelope};

/// Where an externally originated turn came from.
///
/// The wire strings are persisted in `channel_events.source` and in the ingress
/// dedupe key, so they are part of the durable contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationSource {
    /// The desktop app's own chat. Present so the enum describes every producer,
    /// not because the GUI turn loop routes through here today.
    Desktop,
    /// A paired mobile device submitting a turn over the remote protocol.
    Mobile,
    /// A messaging provider adapter (Telegram, Slack, Matrix, …).
    MessagingChannel,
    /// Another Little Monkey node handing over work.
    Peer,
    /// The realtime Talk subsystem.
    Voice,
    /// An inbound phone call handled by the telephony subsystem.
    Telephone,
}

impl ConversationSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ConversationSource::Desktop => "desktop",
            ConversationSource::Mobile => "mobile",
            ConversationSource::MessagingChannel => "messaging_channel",
            ConversationSource::Peer => "peer",
            ConversationSource::Voice => "voice",
            ConversationSource::Telephone => "telephone",
        }
    }

    pub fn parse(value: &str) -> Option<ConversationSource> {
        match value {
            "desktop" => Some(ConversationSource::Desktop),
            "mobile" => Some(ConversationSource::Mobile),
            "messaging_channel" => Some(ConversationSource::MessagingChannel),
            "peer" => Some(ConversationSource::Peer),
            "voice" => Some(ConversationSource::Voice),
            "telephone" => Some(ConversationSource::Telephone),
            _ => None,
        }
    }

    /// Whether the text from this source was authored by the operator.
    ///
    /// The distinction is authentication, not network distance. A paired phone
    /// and the Talk microphone are the operator speaking, and their words are
    /// instructions — wrapping them as untrusted data would make Little Monkey
    /// refuse its own owner. A Telegram sender, a caller, and a peer node are
    /// someone else, and their words are evidence.
    pub fn author_is_operator(self) -> bool {
        matches!(
            self,
            ConversationSource::Desktop | ConversationSource::Mobile | ConversationSource::Voice
        )
    }
}

/// Text supplied by someone other than the operator.
///
/// A newtype rather than a `String` so that every place external text is turned
/// into model input has to name what it is doing: the only way out is
/// [`UntrustedText::as_untrusted_str`], which is greppable, and which callers
/// building a run are expected to feed through the agent's untrusted-content
/// wrapper rather than concatenate into instructions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UntrustedText(String);

impl UntrustedText {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The raw text. Deliberately verbose: reaching for this means the caller is
    /// about to hand provider-controlled bytes to something, and that call site
    /// is the one a reviewer should look at.
    pub fn as_untrusted_str(&self) -> &str {
        &self.0
    }

    pub fn is_blank(&self) -> bool {
        self.0.trim().is_empty()
    }

    pub fn char_count(&self) -> usize {
        self.0.chars().count()
    }
}

/// A durable, deduplicated, route-frozen external turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationIngress {
    pub source: ConversationSource,
    /// Account/device/line the turn arrived on. The channel account id for a
    /// messaging channel, the device id for mobile, the telecom account for a
    /// call. Scopes `source_event_id`, which is only unique within it.
    pub source_account_id: String,
    /// The originating system's own event identifier. A source that has no
    /// stable id must synthesize a deterministic one — never a fresh UUID, or
    /// dedupe silently stops working.
    pub source_event_id: String,
    /// Durable session this turn continues, from the route's [`SessionScope`].
    ///
    /// [`SessionScope`]: crate::channels::routing::SessionScope
    pub session_key: String,
    pub text: UntrustedText,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ChannelAttachment>,
    /// The execution configuration, frozen at accept time.
    pub target: RouteTarget,
    /// Digest of `target` as it was when the turn was accepted, so a run can
    /// prove which configuration produced it after the route row changes.
    pub route_digest: String,
    /// Route row that matched, when one did. Absent for sources that carry their
    /// own target (mobile, peer) instead of resolving a channel route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_id: Option<String>,
    /// How many automated replies deep this turn is. Zero for a turn a human
    /// originated; incremented when an agent's own output triggers another turn.
    #[serde(default)]
    pub reply_depth: u32,
    /// True when this turn was produced by automation rather than by a person.
    /// Carried through to audit state.
    #[serde(default)]
    pub automation_origin: bool,
    pub received_at_ms: i64,
    /// Diagnostic-only. Never model input.
    #[serde(default, skip_serializing_if = "BoundedMetadata::is_empty")]
    pub metadata: BoundedMetadata,
}

impl ConversationIngress {
    /// Build an ingress record from an accepted channel message and the route
    /// that matched it.
    pub fn from_channel(envelope: &ChannelEnvelope, route: &ChannelRoute) -> Self {
        Self {
            source: ConversationSource::MessagingChannel,
            source_account_id: envelope.account_id.clone(),
            source_event_id: envelope.provider_event_id.clone(),
            session_key: route.target.session_scope.session_key(envelope),
            text: UntrustedText::new(envelope.text.clone()),
            attachments: envelope.attachments.clone(),
            route_digest: route.target.digest(),
            target: route.target.clone(),
            route_id: Some(route.route_id.clone()),
            reply_depth: 0,
            automation_origin: false,
            received_at_ms: envelope.received_at_ms,
            metadata: envelope.metadata.clone(),
        }
    }

    /// Build an ingress record for a source that supplies its own target rather
    /// than resolving a channel route: mobile, peer, voice, telephony.
    pub fn direct(
        source: ConversationSource,
        source_account_id: impl Into<String>,
        source_event_id: impl Into<String>,
        session_key: impl Into<String>,
        text: impl Into<String>,
        target: RouteTarget,
        received_at_ms: i64,
    ) -> Self {
        Self {
            source,
            source_account_id: source_account_id.into(),
            source_event_id: source_event_id.into(),
            session_key: session_key.into(),
            text: UntrustedText::new(text),
            attachments: Vec::new(),
            route_digest: target.digest(),
            target,
            route_id: None,
            reply_depth: 0,
            automation_origin: false,
            received_at_ms,
            metadata: BoundedMetadata::new(),
        }
    }

    /// Mark this turn as automation-originated at the given reply depth.
    pub fn with_automation(mut self, reply_depth: u32) -> Self {
        self.automation_origin = true;
        self.reply_depth = reply_depth;
        self
    }

    /// Identity for the durable dedupe: source, account, event id. No timestamp,
    /// so a redelivery or a replayed polling window collapses onto the same row.
    pub fn dedupe_key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.source.as_str(),
            self.source_account_id,
            self.source_event_id
        )
    }

    /// Deterministic job id for the daemon queue, matching the webhook trigger
    /// path's shape. Two submissions of the same turn produce the same id, which
    /// is what makes the queue itself the last line of dedupe defense.
    pub fn deterministic_job_id(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.dedupe_key().as_bytes());
        hasher.update([0]);
        hasher.update(self.route_digest.as_bytes());
        let digest: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("ingress-{}", &digest[..32])
    }

    /// Whether this turn's text must be wrapped as untrusted data before it can
    /// become model input. True for everyone who is not the operator.
    pub fn needs_untrusted_wrapping(&self) -> bool {
        !self.source.author_is_operator()
    }

    /// Whether this turn carries anything worth running.
    pub fn has_content(&self) -> bool {
        !self.text.is_blank() || !self.attachments.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::routing::{ChannelRoute, RouteScope, SessionScope};
    use crate::channels::types::{ChannelConversation, ChannelKind, ChannelSender};

    fn envelope() -> ChannelEnvelope {
        ChannelEnvelope {
            account_id: "acct-1".into(),
            kind: ChannelKind::Telegram,
            provider_event_id: "42".into(),
            conversation: ChannelConversation::direct("chat-7"),
            sender: ChannelSender::new("user-3"),
            text: "ship it".into(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            mentions_self: false,
            received_at_ms: 1_700_000_000_000,
            metadata: BoundedMetadata::new(),
        }
    }

    fn route() -> ChannelRoute {
        ChannelRoute {
            route_id: "route-1".into(),
            scope: RouteScope::account("acct-1"),
            target: RouteTarget::new("chat"),
            enabled: true,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[test]
    fn source_strings_round_trip() {
        for source in [
            ConversationSource::Desktop,
            ConversationSource::Mobile,
            ConversationSource::MessagingChannel,
            ConversationSource::Peer,
            ConversationSource::Voice,
            ConversationSource::Telephone,
        ] {
            assert_eq!(ConversationSource::parse(source.as_str()), Some(source));
        }
        assert_eq!(ConversationSource::parse("carrier pigeon"), None);
    }

    #[test]
    fn the_operators_own_surfaces_are_not_wrapped_as_untrusted() {
        assert!(ConversationSource::Desktop.author_is_operator());
        assert!(ConversationSource::Mobile.author_is_operator());
        assert!(ConversationSource::Voice.author_is_operator());

        assert!(!ConversationSource::MessagingChannel.author_is_operator());
        assert!(!ConversationSource::Telephone.author_is_operator());
        assert!(!ConversationSource::Peer.author_is_operator());

        assert!(ConversationIngress::from_channel(&envelope(), &route()).needs_untrusted_wrapping());
        assert!(!ConversationIngress::direct(
            ConversationSource::Mobile,
            "device-1",
            "mm-1",
            "mobile:s-1",
            "ship it",
            RouteTarget::new("mobile-chat"),
            1,
        )
        .needs_untrusted_wrapping());
    }

    #[test]
    fn channel_ingress_freezes_the_route() {
        let ingress = ConversationIngress::from_channel(&envelope(), &route());

        assert_eq!(ingress.source, ConversationSource::MessagingChannel);
        assert_eq!(ingress.route_id.as_deref(), Some("route-1"));
        assert_eq!(ingress.route_digest, RouteTarget::new("chat").digest());
        assert_eq!(ingress.text.as_untrusted_str(), "ship it");
        assert!(!ingress.automation_origin);
        assert_eq!(ingress.reply_depth, 0);
    }

    #[test]
    fn session_key_follows_the_routes_scope() {
        let mut threaded = envelope();
        threaded.conversation = threaded.conversation.with_thread(Some("t-9".into()));

        let per_thread = ConversationIngress::from_channel(&threaded, &route());
        assert!(per_thread.session_key.ends_with(":t-9"));

        let mut collapsing = route();
        collapsing.target.session_scope = SessionScope::Conversation;
        let per_conversation = ConversationIngress::from_channel(&threaded, &collapsing);
        assert_eq!(
            per_conversation.session_key,
            "channel:telegram:acct-1:chat-7"
        );
    }

    #[test]
    fn dedupe_key_ignores_arrival_time() {
        let first = ConversationIngress::from_channel(&envelope(), &route());
        let mut redelivered = envelope();
        redelivered.received_at_ms += 90_000;
        let second = ConversationIngress::from_channel(&redelivered, &route());

        assert_eq!(first.dedupe_key(), second.dedupe_key());
        assert_eq!(first.deterministic_job_id(), second.deterministic_job_id());
    }

    #[test]
    fn dedupe_key_separates_sources_and_accounts() {
        let channel = ConversationIngress::from_channel(&envelope(), &route());
        let mobile = ConversationIngress::direct(
            ConversationSource::Mobile,
            "acct-1",
            "42",
            "session-1",
            "ship it",
            RouteTarget::new("chat"),
            1,
        );
        assert_ne!(channel.dedupe_key(), mobile.dedupe_key());

        let mut other_account = envelope();
        other_account.account_id = "acct-2".into();
        let mut other_route = route();
        other_route.scope = RouteScope::account("acct-2");
        assert_ne!(
            channel.dedupe_key(),
            ConversationIngress::from_channel(&other_account, &other_route).dedupe_key()
        );
    }

    #[test]
    fn job_id_changes_when_the_frozen_route_changes() {
        let base = ConversationIngress::from_channel(&envelope(), &route());
        let mut rerouted = route();
        rerouted.target = RouteTarget::new("triage");
        let moved = ConversationIngress::from_channel(&envelope(), &rerouted);

        assert_eq!(base.dedupe_key(), moved.dedupe_key());
        assert_ne!(base.deterministic_job_id(), moved.deterministic_job_id());
        assert!(base.deterministic_job_id().starts_with("ingress-"));
        assert_eq!(base.deterministic_job_id().len(), "ingress-".len() + 32);
    }

    #[test]
    fn an_attachment_only_message_still_has_content() {
        let mut silent = envelope();
        silent.text = "   ".into();
        let ingress = ConversationIngress::from_channel(&silent, &route());
        assert!(!ingress.has_content());

        let mut with_file = silent.clone();
        with_file.attachments.push(ChannelAttachment {
            provider_id: Some("file-1".into()),
            kind: crate::channels::types::AttachmentKind::Image,
            filename: Some("shot.png".into()),
            mime_type: Some("image/png".into()),
            declared_size_bytes: Some(1024),
            source: crate::channels::types::AttachmentSource::ProviderHandle {
                handle: "file-1".into(),
            },
        });
        assert!(ConversationIngress::from_channel(&with_file, &route()).has_content());
    }

    #[test]
    fn automation_marker_carries_the_depth() {
        let ingress = ConversationIngress::from_channel(&envelope(), &route()).with_automation(2);
        assert!(ingress.automation_origin);
        assert_eq!(ingress.reply_depth, 2);
    }

    /// The daemon stores this record and reads it back after a restart, which
    /// makes its serialized shape a compatibility surface: a turn accepted by
    /// the build before an upgrade still has to load on the build after it.
    #[test]
    fn a_turn_serialized_before_the_optional_fields_existed_still_loads() {
        let stored = serde_json::json!({
            "source": "messaging_channel",
            "source_account_id": "acct-1",
            "source_event_id": "42",
            "session_key": "channel:telegram:acct-1:chat-7",
            "text": "ship it",
            "target": RouteTarget::new("chat"),
            "route_digest": RouteTarget::new("chat").digest(),
            "received_at_ms": 1_700_000_000_000_i64,
        });

        let restored: ConversationIngress = serde_json::from_value(stored).expect("deserialize");
        assert_eq!(restored.reply_depth, 0);
        assert!(!restored.automation_origin);
        assert!(restored.attachments.is_empty());
        assert!(restored.route_id.is_none());
        // The identity a re-submission deduplicates on has to survive too, or
        // recovery queues a second run for a turn that already has one.
        assert_eq!(restored.dedupe_key(), "messaging_channel:acct-1:42");
        assert_eq!(
            restored.deterministic_job_id(),
            ConversationIngress::from_channel(&envelope(), &route()).deterministic_job_id()
        );
    }

    #[test]
    fn untrusted_text_survives_a_json_round_trip_as_a_plain_string() {
        let ingress = ConversationIngress::from_channel(&envelope(), &route());
        let json = serde_json::to_value(&ingress).expect("serialize");
        assert_eq!(json["text"], "ship it");

        let restored: ConversationIngress = serde_json::from_value(json).expect("deserialize");
        assert_eq!(restored, ingress);
    }
}
