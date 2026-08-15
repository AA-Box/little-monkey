//! Provider-independent messaging types.
//!
//! Every messaging provider Little Monkey speaks to is reduced to the shapes in
//! this module before anything else in the app sees it. Adapters own exactly two
//! translations — `provider wire format -> ChannelEnvelope` on the way in and
//! `OutboundMessage -> provider wire format` on the way out — and nothing else.
//! They never execute an agent, never touch the run ledger, and never decide
//! whether a message is allowed to run.
//!
//! The types are deliberately narrow. Provider payloads carry far more than this
//! (Discord alone sends dozens of fields per message), and everything that does
//! not survive normalization is either dropped or squeezed into
//! [`BoundedMetadata`], which is capped, diagnostic-only, and never handed to a
//! model as instructions.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Which messaging provider an account speaks to.
///
/// The wire strings are persisted in `channel_accounts.kind` and appear in
/// routes, so they are part of the durable contract: rename one and existing
/// accounts stop resolving.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Telegram,
    Discord,
    Slack,
    WhatsApp,
    Signal,
    Teams,
    GoogleChat,
    Matrix,
    Mattermost,
    Line,
    IMessage,
    Irc,
    Sms,
}

impl ChannelKind {
    /// Every kind, in a stable order — used by settings UI enumeration and by
    /// the registry's exhaustiveness test.
    pub const ALL: &'static [ChannelKind] = &[
        ChannelKind::Telegram,
        ChannelKind::Discord,
        ChannelKind::Slack,
        ChannelKind::WhatsApp,
        ChannelKind::Signal,
        ChannelKind::Teams,
        ChannelKind::GoogleChat,
        ChannelKind::Matrix,
        ChannelKind::Mattermost,
        ChannelKind::Line,
        ChannelKind::IMessage,
        ChannelKind::Irc,
        ChannelKind::Sms,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ChannelKind::Telegram => "telegram",
            ChannelKind::Discord => "discord",
            ChannelKind::Slack => "slack",
            ChannelKind::WhatsApp => "whatsapp",
            ChannelKind::Signal => "signal",
            ChannelKind::Teams => "teams",
            ChannelKind::GoogleChat => "google_chat",
            ChannelKind::Matrix => "matrix",
            ChannelKind::Mattermost => "mattermost",
            ChannelKind::Line => "line",
            ChannelKind::IMessage => "imessage",
            ChannelKind::Irc => "irc",
            ChannelKind::Sms => "sms",
        }
    }

    pub fn parse(value: &str) -> Option<ChannelKind> {
        ChannelKind::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == value)
    }

    /// Human-facing label. Kept here rather than in the frontend so the daemon,
    /// the CLI and the desktop app all name a provider the same way.
    pub fn label(self) -> &'static str {
        match self {
            ChannelKind::Telegram => "Telegram",
            ChannelKind::Discord => "Discord",
            ChannelKind::Slack => "Slack",
            ChannelKind::WhatsApp => "WhatsApp",
            ChannelKind::Signal => "Signal",
            ChannelKind::Teams => "Microsoft Teams",
            ChannelKind::GoogleChat => "Google Chat",
            ChannelKind::Matrix => "Matrix",
            ChannelKind::Mattermost => "Mattermost",
            ChannelKind::Line => "LINE",
            ChannelKind::IMessage => "iMessage",
            ChannelKind::Irc => "IRC",
            ChannelKind::Sms => "SMS",
        }
    }
}

/// How an adapter receives inbound traffic. Surfaced in the UI because the
/// operator's setup obligations differ wildly: a polling transport needs no
/// public URL, a webhook transport does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboundTransport {
    /// The adapter asks the provider for updates on a loop (Telegram).
    LongPoll,
    /// The adapter holds a socket open (Discord, Slack, Mattermost, IRC, Matrix).
    Socket,
    /// The provider posts to the daemon's ingress (WhatsApp, Teams, LINE, SMS).
    Webhook,
    /// A supervised local helper process streams events (Signal, iMessage).
    Helper,
}

impl InboundTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            InboundTransport::LongPoll => "long_poll",
            InboundTransport::Socket => "socket",
            InboundTransport::Webhook => "webhook",
            InboundTransport::Helper => "helper",
        }
    }
}

/// Shape of the conversation a message arrived in.
///
/// The distinction drives access policy: `Direct` defaults to pairing, the two
/// multi-party kinds default to an allow list plus mention gating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationKind {
    Direct,
    Group,
    Channel,
}

impl ConversationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ConversationKind::Direct => "direct",
            ConversationKind::Group => "group",
            ConversationKind::Channel => "channel",
        }
    }

    pub fn parse(value: &str) -> Option<ConversationKind> {
        match value {
            "direct" => Some(ConversationKind::Direct),
            "group" => Some(ConversationKind::Group),
            "channel" => Some(ConversationKind::Channel),
            _ => None,
        }
    }

    /// Group and channel conversations share every policy default; only DMs are
    /// treated as a one-to-one relationship with a single sender.
    pub fn is_multi_party(self) -> bool {
        !matches!(self, ConversationKind::Direct)
    }
}

/// Where a conversation lives inside an account.
///
/// `conversation_id` is the provider's own identifier (chat id, channel id, room
/// id). `thread_id` is set when the provider models threads separately — Slack
/// thread_ts, Discord thread channel, Telegram forum topic. Keeping them apart
/// matters because routes and session identity can be scoped to a thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelConversation {
    pub conversation_id: String,
    pub kind: ConversationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Display title when the provider supplies one. Never used for routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl ChannelConversation {
    pub fn direct(conversation_id: impl Into<String>) -> Self {
        Self {
            conversation_id: conversation_id.into(),
            kind: ConversationKind::Direct,
            thread_id: None,
            title: None,
        }
    }

    pub fn group(conversation_id: impl Into<String>) -> Self {
        Self {
            conversation_id: conversation_id.into(),
            kind: ConversationKind::Group,
            thread_id: None,
            title: None,
        }
    }

    pub fn with_thread(mut self, thread_id: Option<String>) -> Self {
        self.thread_id = thread_id.filter(|value| !value.is_empty());
        self
    }

    pub fn with_title(mut self, title: Option<String>) -> Self {
        self.title = title.filter(|value| !value.is_empty());
        self
    }
}

/// Who sent an inbound message.
///
/// `sender_id` is the provider's stable user identifier and is the key sender
/// authorization is recorded against — display names are mutable and
/// impersonable, so they never participate in an authorization decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelSender {
    pub sender_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_label: Option<String>,
    /// True when the provider says this message came from the configured bot
    /// identity itself. Set by the adapter; the ingress path drops these before
    /// anything else looks at them.
    #[serde(default)]
    pub is_self: bool,
    /// True when the provider marks the sender as a bot other than us. Used by
    /// loop prevention, not by authorization.
    #[serde(default)]
    pub is_bot: bool,
}

impl ChannelSender {
    pub fn new(sender_id: impl Into<String>) -> Self {
        Self {
            sender_id: sender_id.into(),
            display_label: None,
            is_self: false,
            is_bot: false,
        }
    }

    pub fn with_label(mut self, label: Option<String>) -> Self {
        self.display_label = label.filter(|value| !value.is_empty());
        self
    }
}

/// Kind of an attachment as far as Little Monkey cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Image,
    Audio,
    Video,
    Document,
    Other,
}

impl AttachmentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AttachmentKind::Image => "image",
            AttachmentKind::Audio => "audio",
            AttachmentKind::Video => "video",
            AttachmentKind::Document => "document",
            AttachmentKind::Other => "other",
        }
    }

    /// Best-effort classification from a MIME type. Providers that already say
    /// what a thing is should not call this.
    pub fn from_mime(mime: &str) -> AttachmentKind {
        let mime = mime.to_ascii_lowercase();
        if mime.starts_with("image/") {
            AttachmentKind::Image
        } else if mime.starts_with("audio/") {
            AttachmentKind::Audio
        } else if mime.starts_with("video/") {
            AttachmentKind::Video
        } else if mime.starts_with("text/") || mime.starts_with("application/") {
            AttachmentKind::Document
        } else {
            AttachmentKind::Other
        }
    }
}

/// An inbound attachment as the provider describes it.
///
/// Nothing here is trusted: `declared_size_bytes` is what the provider claims,
/// and `source` is a provider-controlled URL or opaque handle. Fetching is a
/// separate, bounded step (`channels::attachments`) that re-verifies size and
/// stores through the artifact store — an envelope carrying an attachment does
/// not mean any bytes were downloaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelAttachment {
    /// Provider identifier for the attachment, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    pub kind: AttachmentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_size_bytes: Option<u64>,
    /// How the bytes are obtained. Either an https URL or a provider handle the
    /// adapter knows how to resolve.
    pub source: AttachmentSource,
    /// Content-store id once the bytes have actually been fetched.
    ///
    /// Absent until ingest downloads them, and absent forever if the download
    /// was refused or failed — which is why it is an `Option` rather than a
    /// flag: an id that exists is a promise the bytes are on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stored_artifact_id: Option<String>,
    /// Why the bytes are not here, when they are not.
    ///
    /// Recorded rather than swallowed: an attachment that was too large or
    /// whose download failed is something the person who sent it should be
    /// told about, not something to silently ignore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_error: Option<String>,
    /// The beginning of the file's own text, when it is text at all.
    ///
    /// Filled at ingest for attachments that decode as UTF-8, which is what
    /// lets an agent answer a question about a log or a CSV somebody sent
    /// instead of being told a file exists. Bounded, and untrusted exactly like
    /// the message body — a file's contents are the sender's words too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_excerpt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachmentSource {
    /// Directly fetchable URL. Still subject to egress policy and size caps.
    Url { url: String },
    /// Opaque provider handle the owning adapter resolves (Telegram file_id,
    /// Slack file id, a Signal helper attachment path).
    ProviderHandle { handle: String },
}

/// Bounded, diagnostic-only provider metadata.
///
/// Adapters put things here that are useful when debugging an account (guild id,
/// team id, message subtype) and useless — or dangerous — as model input. The
/// caps are enforced on construction so an oversized provider payload cannot
/// bloat the durable event log, and the whole map is kept out of prompts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BoundedMetadata(BTreeMap<String, String>);

impl BoundedMetadata {
    pub const MAX_ENTRIES: usize = 16;
    pub const MAX_KEY_LEN: usize = 64;
    pub const MAX_VALUE_LEN: usize = 256;

    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Insert a key, truncating the value and dropping the entry entirely once
    /// the map is full. Silently bounded on purpose: metadata is never worth
    /// failing an inbound message over.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        let mut key = key.into();
        let mut value = value.into();
        if key.is_empty() || value.is_empty() {
            return self;
        }
        truncate_chars(&mut key, Self::MAX_KEY_LEN);
        truncate_chars(&mut value, Self::MAX_VALUE_LEN);
        if self.0.len() >= Self::MAX_ENTRIES && !self.0.contains_key(&key) {
            return self;
        }
        self.0.insert(key, value);
        self
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// Re-apply the caps to a map that arrived from storage or from a provider
    /// adapter that built one by hand.
    pub fn sanitized(entries: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut bounded = Self::new();
        for (key, value) in entries {
            bounded.insert(key, value);
        }
        bounded
    }
}

fn truncate_chars(value: &mut String, max_chars: usize) {
    if value.chars().count() <= max_chars {
        return;
    }
    let end = value
        .char_indices()
        .nth(max_chars)
        .map(|(index, _)| index)
        .unwrap_or(value.len());
    value.truncate(end);
}

/// A normalized inbound message.
///
/// This is the only shape the rest of the app sees for external messaging
/// traffic — channel ingress, SMS from the telephony subsystem, and messages
/// contributed by an extension-provided channel all arrive as one of these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelEnvelope {
    pub account_id: String,
    pub kind: ChannelKind,
    /// Provider's own event/message identifier. Combined with the account it
    /// forms the dedupe key, so an adapter that cannot supply a stable one must
    /// synthesize a deterministic value (never a random id).
    pub provider_event_id: String,
    pub conversation: ChannelConversation,
    pub sender: ChannelSender,
    /// Message text as the provider delivered it. Untrusted. Callers must not
    /// concatenate this into system instructions.
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ChannelAttachment>,
    /// Provider message id this message replies to, when the provider says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_provider_id: Option<String>,
    /// True when the provider's own mention metadata names the configured bot
    /// identity. Mention gating prefers this over scanning text.
    #[serde(default)]
    pub mentions_self: bool,
    pub received_at_ms: i64,
    #[serde(default, skip_serializing_if = "BoundedMetadata::is_empty")]
    pub metadata: BoundedMetadata,
}

impl ChannelEnvelope {
    /// Dedupe key for the durable event log: an account plus the provider's own
    /// event id. Never includes a timestamp, so a redelivered webhook or a
    /// replayed polling window collapses onto the same row.
    pub fn dedupe_key(&self) -> String {
        format!("{}:{}", self.account_id, self.provider_event_id)
    }

    /// Default session identity for this envelope: channel, account,
    /// conversation and thread stay separate so two Slack channels, or two
    /// threads in one channel, never share a session.
    pub fn default_session_key(&self) -> String {
        let mut key = format!(
            "channel:{}:{}:{}",
            self.kind.as_str(),
            self.account_id,
            self.conversation.conversation_id
        );
        if let Some(thread) = &self.conversation.thread_id {
            key.push(':');
            key.push_str(thread);
        }
        key
    }

    pub fn has_text(&self) -> bool {
        !self.text.trim().is_empty()
    }
}

/// What a provider adapter can actually do.
///
/// Used by the outbound path (to reject or split a message before it reaches the
/// provider) and by the UI (to hide controls a provider cannot honor).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub kind: ChannelKind,
    pub inbound_transport: InboundTransport,
    /// Provider-enforced maximum characters in one outbound message.
    pub max_text_chars: usize,
    pub supports_threads: bool,
    /// This adapter can both fetch an inbound attachment and upload an
    /// outbound one. Declared `false` unless *both* halves are implemented:
    /// a provider that claims files and then quietly sends a message without
    /// one is worse than a provider that says it cannot.
    pub supports_attachments: bool,
    /// Provider exposes explicit mention metadata, so mention gating does not
    /// have to fall back to substring matching.
    pub supports_mention_metadata: bool,
    /// Provider accepts a caller-supplied idempotency/transaction id, which is
    /// what lets a crashed send be retried safely.
    pub supports_idempotency_key: bool,
    /// Provider reports delivery/read receipts asynchronously.
    pub supports_delivery_receipts: bool,
}

impl ProviderCapabilities {
    /// Conservative defaults: everything unsupported except plain text. Adapters
    /// opt into what they can prove they do.
    pub fn minimal(kind: ChannelKind, inbound_transport: InboundTransport) -> Self {
        Self {
            kind,
            inbound_transport,
            max_text_chars: 4000,
            supports_threads: false,
            supports_attachments: false,
            supports_mention_metadata: false,
            supports_idempotency_key: false,
            supports_delivery_receipts: false,
        }
    }
}

/// Verified connection state for an account.
///
/// `Connected` is only ever written after a probe or a live inbound event —
/// never because configuration exists. The UI renders this value directly, which
/// is what keeps "configured" from being displayed as "connected".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    /// Required configuration is missing.
    Unconfigured,
    /// Configured, enabled, but no successful probe or connection yet.
    Disconnected,
    /// Handshake or first probe in flight.
    Connecting,
    /// Provider answered and the transport is live.
    Connected,
    /// Live but impaired — reconnecting, rate limited, partial failure.
    Degraded,
    /// The provider cannot run on this OS or build.
    Unsupported,
    /// Last attempt failed; `last_error` explains.
    Error,
}

impl HealthState {
    pub fn as_str(self) -> &'static str {
        match self {
            HealthState::Unconfigured => "unconfigured",
            HealthState::Disconnected => "disconnected",
            HealthState::Connecting => "connecting",
            HealthState::Connected => "connected",
            HealthState::Degraded => "degraded",
            HealthState::Unsupported => "unsupported",
            HealthState::Error => "error",
        }
    }

    pub fn parse(value: &str) -> Option<HealthState> {
        match value {
            "unconfigured" => Some(HealthState::Unconfigured),
            "disconnected" => Some(HealthState::Disconnected),
            "connecting" => Some(HealthState::Connecting),
            "connected" => Some(HealthState::Connected),
            "degraded" => Some(HealthState::Degraded),
            "unsupported" => Some(HealthState::Unsupported),
            "error" => Some(HealthState::Error),
            _ => None,
        }
    }
}

/// Result of a health probe against a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelHealth {
    pub state: HealthState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub probed_at_ms: i64,
}

impl ChannelHealth {
    pub fn connected(probed_at_ms: i64, detail: Option<String>) -> Self {
        Self {
            state: HealthState::Connected,
            detail,
            last_error: None,
            probed_at_ms,
        }
    }

    /// The transport is on its way up and has not proved itself yet.
    ///
    /// Distinct from `connected`, which claims a working capability, and from
    /// `degraded`, which claims a connection that is losing something: this is
    /// the honest answer while a socket is still being opened.
    pub fn connecting(probed_at_ms: i64, detail: Option<String>) -> Self {
        Self {
            state: HealthState::Connecting,
            detail,
            last_error: None,
            probed_at_ms,
        }
    }

    pub fn error(probed_at_ms: i64, error: impl Into<String>) -> Self {
        Self {
            state: HealthState::Error,
            detail: None,
            last_error: Some(error.into()),
            probed_at_ms,
        }
    }

    pub fn unsupported(probed_at_ms: i64, detail: impl Into<String>) -> Self {
        Self {
            state: HealthState::Unsupported,
            detail: Some(detail.into()),
            last_error: None,
            probed_at_ms,
        }
    }

    /// Working, but not fully: the connection is up and something measurable is
    /// being lost.
    ///
    /// The detail carries the count, because "degraded" on its own tells an
    /// operator nothing they can act on. Distinct from `error` — messages are
    /// still flowing — and distinct from `connected`, which would claim
    /// everything arrived.
    pub fn degraded(probed_at_ms: i64, detail: impl Into<String>) -> Self {
        Self {
            state: HealthState::Degraded,
            detail: Some(detail.into()),
            last_error: None,
            probed_at_ms,
        }
    }
}

/// An outbound attachment, referenced by artifact so the send path never carries
/// bytes through the agent or the tool boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundAttachment {
    pub artifact_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// A message Little Monkey wants to send.
///
/// Built by the outbox, not by adapters. `idempotency_key` is stable across
/// retries of the same queued row, which is what lets a provider that supports
/// idempotency collapse a duplicated send after a crash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub account_id: String,
    pub kind: ChannelKind,
    pub conversation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<OutboundAttachment>,
    /// Provider message id being replied to, when the caller asked for a reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_provider_id: Option<String>,
    pub idempotency_key: String,
}

/// Terminal-ish outcome of one send attempt.
///
/// `NeedsReconciliation` is the important one: it means the request may have
/// reached the provider before the failure, so retrying could duplicate an
/// external effect. The outbox parks those rows for an operator instead of
/// retrying them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum SendOutcome {
    Sent {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_message_id: Option<String>,
    },
    /// Failed in a way that is provably safe to retry (connection refused,
    /// request never left, provider returned a retryable status).
    RetryableFailure {
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<i64>,
    },
    /// Failed permanently — bad configuration, rejected content, revoked token.
    PermanentFailure { error: String },
    /// Outcome unknown after the request was already in flight.
    NeedsReconciliation { error: String },
}

/// Provider-reported delivery progress for an already-sent message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Queued,
    Sent,
    Delivered,
    Read,
    Failed,
}

impl DeliveryState {
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryState::Queued => "queued",
            DeliveryState::Sent => "sent",
            DeliveryState::Delivered => "delivered",
            DeliveryState::Read => "read",
            DeliveryState::Failed => "failed",
        }
    }
}

/// An asynchronous delivery update from a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryReceipt {
    pub account_id: String,
    pub provider_message_id: String,
    pub state: DeliveryState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub observed_at_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_kind_wire_strings_round_trip() {
        for kind in ChannelKind::ALL {
            assert_eq!(ChannelKind::parse(kind.as_str()), Some(*kind));
        }
    }

    #[test]
    fn channel_kind_wire_strings_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for kind in ChannelKind::ALL {
            assert!(seen.insert(kind.as_str()), "duplicate wire string");
        }
        assert_eq!(seen.len(), ChannelKind::ALL.len());
    }

    #[test]
    fn bounded_metadata_caps_entries_keys_and_values() {
        let mut metadata = BoundedMetadata::new();
        for index in 0..(BoundedMetadata::MAX_ENTRIES + 10) {
            metadata.insert(format!("key{index}"), "value");
        }
        assert_eq!(metadata.len(), BoundedMetadata::MAX_ENTRIES);

        let mut long = BoundedMetadata::new();
        long.insert("k".repeat(500), "v".repeat(5000));
        let (key, value) = long.iter().next().expect("one entry");
        assert_eq!(key.chars().count(), BoundedMetadata::MAX_KEY_LEN);
        assert_eq!(value.chars().count(), BoundedMetadata::MAX_VALUE_LEN);
    }

    #[test]
    fn bounded_metadata_truncates_on_char_boundaries() {
        let mut metadata = BoundedMetadata::new();
        metadata.insert("k", "é".repeat(400));
        let (_, value) = metadata.iter().next().expect("one entry");
        assert_eq!(value.chars().count(), BoundedMetadata::MAX_VALUE_LEN);
    }

    #[test]
    fn session_key_separates_threads_and_conversations() {
        let base = ChannelEnvelope {
            account_id: "acct".into(),
            kind: ChannelKind::Slack,
            provider_event_id: "evt-1".into(),
            conversation: ChannelConversation::group("C1"),
            sender: ChannelSender::new("U1"),
            text: "hi".into(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            mentions_self: false,
            received_at_ms: 1,
            metadata: BoundedMetadata::new(),
        };
        let threaded = ChannelEnvelope {
            conversation: ChannelConversation::group("C1").with_thread(Some("T9".into())),
            ..base.clone()
        };
        let other_conversation = ChannelEnvelope {
            conversation: ChannelConversation::group("C2"),
            ..base.clone()
        };

        assert_ne!(base.default_session_key(), threaded.default_session_key());
        assert_ne!(
            base.default_session_key(),
            other_conversation.default_session_key()
        );
        assert_eq!(base.default_session_key(), base.default_session_key());
    }

    #[test]
    fn dedupe_key_ignores_arrival_time() {
        let first = ChannelEnvelope {
            account_id: "acct".into(),
            kind: ChannelKind::Telegram,
            provider_event_id: "42".into(),
            conversation: ChannelConversation::direct("chat"),
            sender: ChannelSender::new("U1"),
            text: "hello".into(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            mentions_self: false,
            received_at_ms: 100,
            metadata: BoundedMetadata::new(),
        };
        let redelivered = ChannelEnvelope {
            received_at_ms: 900,
            ..first.clone()
        };
        assert_eq!(first.dedupe_key(), redelivered.dedupe_key());
    }
}
