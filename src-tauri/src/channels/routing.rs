//! Deterministic routing from an inbound message to the run configuration that
//! will answer it.
//!
//! A route says "messages matching this scope run recipe X in workspace Y under
//! policy Z". Scopes form a specificity ladder, most specific first:
//!
//! ```text
//! account + conversation + thread + sender
//! account + conversation + thread
//! account + conversation
//! account
//! channel default        (every account of one provider)
//! global external default
//! ```
//!
//! Resolution walks the ladder and takes the first rung that matches. Two
//! enabled routes on the *same* rung that both match are a configuration error,
//! not a coin flip: [`resolve_route`] returns [`RouteError::Ambiguous`] so the
//! operator fixes it instead of watching messages land in whichever row the
//! database happened to return first.
//!
//! The resolved [`RouteTarget`] is frozen onto the durable ingress record before
//! the run is queued. Editing a route afterwards changes where *future* messages
//! go; it never retargets a message already in flight.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::types::{ChannelEnvelope, ChannelKind};

/// How much of a message's identity a route pins down.
///
/// Every field is optional, and which ones are set decides the rung. The
/// combinations that do not appear on the ladder (a sender without a
/// conversation, say) are rejected by [`RouteScope::validate`] rather than
/// silently matching nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteScope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ChannelKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<String>,
}

/// Rung on the specificity ladder. Higher wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteSpecificity {
    GlobalDefault = 0,
    ChannelDefault = 1,
    Account = 2,
    Conversation = 3,
    Thread = 4,
    Sender = 5,
}

impl RouteSpecificity {
    pub fn as_str(self) -> &'static str {
        match self {
            RouteSpecificity::GlobalDefault => "global_default",
            RouteSpecificity::ChannelDefault => "channel_default",
            RouteSpecificity::Account => "account",
            RouteSpecificity::Conversation => "conversation",
            RouteSpecificity::Thread => "thread",
            RouteSpecificity::Sender => "sender",
        }
    }
}

/// Why a route scope cannot be stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteScopeError {
    /// A conversation, thread or sender was named without the account it lives
    /// in — such a scope would match messages from unrelated accounts.
    MissingAccount,
    /// A thread or sender was named without a conversation.
    MissingConversation,
    /// A sender-scoped route needs the thread rung populated or absent
    /// consistently; see [`RouteScope::validate`].
    MissingThreadForSender,
    /// A channel-default route names a provider and nothing else; anything more
    /// specific must name the account instead.
    ChannelDefaultWithDetail,
}

impl RouteScopeError {
    pub fn as_str(self) -> &'static str {
        match self {
            RouteScopeError::MissingAccount => "missing_account",
            RouteScopeError::MissingConversation => "missing_conversation",
            RouteScopeError::MissingThreadForSender => "missing_thread_for_sender",
            RouteScopeError::ChannelDefaultWithDetail => "channel_default_with_detail",
        }
    }
}

impl RouteScope {
    /// Route that catches everything not matched by anything more specific.
    pub fn global_default() -> Self {
        Self::default()
    }

    pub fn channel_default(kind: ChannelKind) -> Self {
        Self {
            kind: Some(kind),
            ..Self::default()
        }
    }

    pub fn account(account_id: impl Into<String>) -> Self {
        Self {
            account_id: Some(account_id.into()),
            ..Self::default()
        }
    }

    pub fn conversation(account_id: impl Into<String>, conversation_id: impl Into<String>) -> Self {
        Self {
            account_id: Some(account_id.into()),
            conversation_id: Some(conversation_id.into()),
            ..Self::default()
        }
    }

    pub fn with_thread(mut self, thread_id: impl Into<String>) -> Self {
        self.thread_id = Some(thread_id.into());
        self
    }

    pub fn with_sender(mut self, sender_id: impl Into<String>) -> Self {
        self.sender_id = Some(sender_id.into());
        self
    }

    /// Which rung this scope sits on.
    pub fn specificity(&self) -> RouteSpecificity {
        if self.sender_id.is_some() {
            RouteSpecificity::Sender
        } else if self.thread_id.is_some() {
            RouteSpecificity::Thread
        } else if self.conversation_id.is_some() {
            RouteSpecificity::Conversation
        } else if self.account_id.is_some() {
            RouteSpecificity::Account
        } else if self.kind.is_some() {
            RouteSpecificity::ChannelDefault
        } else {
            RouteSpecificity::GlobalDefault
        }
    }

    /// Reject scopes that are not on the ladder.
    ///
    /// The sender rung is `account + conversation + thread + sender`, but a
    /// provider without threads has no thread id to give, so a sender route in a
    /// thread-less conversation is accepted with `thread_id: None` and matches
    /// only thread-less messages. What is never accepted is a sender or thread
    /// floating free of its conversation.
    pub fn validate(&self) -> Result<(), RouteScopeError> {
        let needs_account =
            self.conversation_id.is_some() || self.thread_id.is_some() || self.sender_id.is_some();
        if needs_account && self.account_id.is_none() {
            return Err(RouteScopeError::MissingAccount);
        }
        if (self.thread_id.is_some() || self.sender_id.is_some()) && self.conversation_id.is_none()
        {
            return Err(RouteScopeError::MissingConversation);
        }
        if self.kind.is_some() && self.account_id.is_some() {
            // A kind-scoped route is the provider-wide default. Once an account
            // is named the account itself already implies the provider, and
            // storing both invites a scope whose two halves disagree.
            return Err(RouteScopeError::ChannelDefaultWithDetail);
        }
        Ok(())
    }

    /// Does this scope match the message?
    pub fn matches(&self, envelope: &ChannelEnvelope) -> bool {
        if let Some(kind) = self.kind {
            if kind != envelope.kind {
                return false;
            }
        }
        if let Some(account_id) = &self.account_id {
            if account_id != &envelope.account_id {
                return false;
            }
        }
        if let Some(conversation_id) = &self.conversation_id {
            if conversation_id != &envelope.conversation.conversation_id {
                return false;
            }
        }
        if let Some(thread_id) = &self.thread_id {
            if Some(thread_id) != envelope.conversation.thread_id.as_ref() {
                return false;
            }
        }
        if let Some(sender_id) = &self.sender_id {
            if sender_id != &envelope.sender.sender_id {
                return false;
            }
        }
        true
    }
}

/// Which durable session an external conversation maps onto.
///
/// The default keeps channel, account, conversation and thread apart, so two
/// threads in one Slack channel are two conversations with their own history,
/// the way a human would expect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionScope {
    /// One session per channel/account/conversation/thread. Default.
    #[default]
    Thread,
    /// Collapse all threads of a conversation into one session.
    Conversation,
    /// One session per sender, across conversations in the account.
    Sender,
    /// One session for the whole account.
    Account,
}

impl SessionScope {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionScope::Thread => "thread",
            SessionScope::Conversation => "conversation",
            SessionScope::Sender => "sender",
            SessionScope::Account => "account",
        }
    }

    pub fn parse(value: &str) -> Option<SessionScope> {
        match value {
            "thread" => Some(SessionScope::Thread),
            "conversation" => Some(SessionScope::Conversation),
            "sender" => Some(SessionScope::Sender),
            "account" => Some(SessionScope::Account),
            _ => None,
        }
    }

    /// Build the durable session key for a message under this scope.
    pub fn session_key(self, envelope: &ChannelEnvelope) -> String {
        match self {
            SessionScope::Thread => envelope.default_session_key(),
            SessionScope::Conversation => format!(
                "channel:{}:{}:{}",
                envelope.kind.as_str(),
                envelope.account_id,
                envelope.conversation.conversation_id
            ),
            SessionScope::Sender => format!(
                "channel:{}:{}:sender:{}",
                envelope.kind.as_str(),
                envelope.account_id,
                envelope.sender.sender_id
            ),
            SessionScope::Account => {
                format!("channel:{}:{}", envelope.kind.as_str(), envelope.account_id)
            }
        }
    }
}

/// The execution configuration a route selects.
///
/// This is Little Monkey's existing run vocabulary, not a second one: `recipe`
/// plus `params` is exactly what the daemon's queue takes, and the recipe is
/// what carries the model target, system prompt and knowledge configuration.
/// Freezing this struct onto the ingress record is what makes the route
/// immutable for a message already in flight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteTarget {
    /// Recipe the message runs as.
    pub recipe: String,
    /// Recipe parameters, merged with the ingress-supplied message parameter.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
    /// Repository/workspace root the run gets, when the route pins one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default)]
    pub session_scope: SessionScope,
    /// Queue priority, matching the daemon's own scale.
    #[serde(default)]
    pub priority: i32,
    /// Whether a reply is sent back to the originating conversation when the run
    /// finishes. Off for routes that only file work for a human to look at.
    #[serde(default = "default_true")]
    pub reply_to_conversation: bool,
}

fn default_true() -> bool {
    true
}

impl RouteTarget {
    pub fn new(recipe: impl Into<String>) -> Self {
        Self {
            recipe: recipe.into(),
            params: BTreeMap::new(),
            repository: None,
            session_scope: SessionScope::default(),
            priority: 0,
            reply_to_conversation: true,
        }
    }

    /// Stable digest of the frozen configuration.
    ///
    /// Stored alongside the ingress record so a run can prove which route
    /// produced it even after the route row is edited or deleted.
    pub fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.recipe.as_bytes());
        hasher.update([0]);
        for (key, value) in &self.params {
            hasher.update(key.as_bytes());
            hasher.update([1]);
            hasher.update(value.as_bytes());
            hasher.update([2]);
        }
        hasher.update(self.repository.as_deref().unwrap_or_default().as_bytes());
        hasher.update([3]);
        hasher.update(self.session_scope.as_str().as_bytes());
        hasher.update([4]);
        hasher.update(self.priority.to_le_bytes());
        hasher.update([u8::from(self.reply_to_conversation)]);
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// A stored route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelRoute {
    pub route_id: String,
    pub scope: RouteScope,
    pub target: RouteTarget,
    pub enabled: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Why routing failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteError {
    /// Nothing matched, not even a global default.
    NoRoute,
    /// Two or more equally specific routes matched. Reported with the offending
    /// ids so the operator can see which rows to fix.
    Ambiguous {
        specificity: RouteSpecificity,
        route_ids: Vec<String>,
    },
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteError::NoRoute => write!(
                formatter,
                "No channel route matched this message and no default route is configured"
            ),
            RouteError::Ambiguous {
                specificity,
                route_ids,
            } => write!(
                formatter,
                "Channel routes {} are equally specific ({}) and all match this message; make one more specific or disable the others",
                route_ids.join(", "),
                specificity.as_str()
            ),
        }
    }
}

impl std::error::Error for RouteError {}

/// Pick the route for a message.
pub fn resolve_route<'a>(
    routes: &'a [ChannelRoute],
    envelope: &ChannelEnvelope,
) -> Result<&'a ChannelRoute, RouteError> {
    let mut best: Option<RouteSpecificity> = None;
    let mut winners: Vec<&ChannelRoute> = Vec::new();

    for route in routes {
        if !route.enabled || !route.scope.matches(envelope) {
            continue;
        }
        let specificity = route.scope.specificity();
        match best {
            Some(current) if specificity < current => continue,
            Some(current) if specificity == current => winners.push(route),
            _ => {
                best = Some(specificity);
                winners.clear();
                winners.push(route);
            }
        }
    }

    match winners.len() {
        0 => Err(RouteError::NoRoute),
        1 => Ok(winners[0]),
        _ => {
            let mut route_ids: Vec<String> =
                winners.iter().map(|route| route.route_id.clone()).collect();
            route_ids.sort();
            Err(RouteError::Ambiguous {
                specificity: best.expect("winners implies a best rung"),
                route_ids,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::types::{
        BoundedMetadata, ChannelConversation, ChannelSender, ConversationKind,
    };

    fn envelope() -> ChannelEnvelope {
        ChannelEnvelope {
            account_id: "acct-1".into(),
            kind: ChannelKind::Slack,
            provider_event_id: "evt".into(),
            conversation: ChannelConversation {
                conversation_id: "C1".into(),
                kind: ConversationKind::Channel,
                thread_id: Some("T1".into()),
                title: None,
            },
            sender: ChannelSender::new("U1"),
            text: "hi".into(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            mentions_self: true,
            received_at_ms: 5,
            metadata: BoundedMetadata::new(),
        }
    }

    fn route(id: &str, scope: RouteScope) -> ChannelRoute {
        ChannelRoute {
            route_id: id.into(),
            scope,
            target: RouteTarget::new(format!("recipe-{id}")),
            enabled: true,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[test]
    fn specificity_ladder_is_ordered() {
        assert!(RouteSpecificity::Sender > RouteSpecificity::Thread);
        assert!(RouteSpecificity::Thread > RouteSpecificity::Conversation);
        assert!(RouteSpecificity::Conversation > RouteSpecificity::Account);
        assert!(RouteSpecificity::Account > RouteSpecificity::ChannelDefault);
        assert!(RouteSpecificity::ChannelDefault > RouteSpecificity::GlobalDefault);
    }

    #[test]
    fn the_most_specific_matching_route_wins() {
        let routes = vec![
            route("global", RouteScope::global_default()),
            route("kind", RouteScope::channel_default(ChannelKind::Slack)),
            route("account", RouteScope::account("acct-1")),
            route("conv", RouteScope::conversation("acct-1", "C1")),
            route(
                "thread",
                RouteScope::conversation("acct-1", "C1").with_thread("T1"),
            ),
            route(
                "sender",
                RouteScope::conversation("acct-1", "C1")
                    .with_thread("T1")
                    .with_sender("U1"),
            ),
        ];
        let resolved = resolve_route(&routes, &envelope()).expect("a route");
        assert_eq!(resolved.route_id, "sender");
    }

    #[test]
    fn resolution_falls_back_down_the_ladder() {
        let routes = vec![
            route("global", RouteScope::global_default()),
            route("account", RouteScope::account("acct-1")),
        ];
        assert_eq!(
            resolve_route(&routes, &envelope())
                .expect("a route")
                .route_id,
            "account"
        );

        let only_global = vec![route("global", RouteScope::global_default())];
        assert_eq!(
            resolve_route(&only_global, &envelope())
                .expect("a route")
                .route_id,
            "global"
        );
    }

    #[test]
    fn a_disabled_route_is_skipped() {
        let mut specific = route("conv", RouteScope::conversation("acct-1", "C1"));
        specific.enabled = false;
        let routes = vec![route("global", RouteScope::global_default()), specific];
        assert_eq!(
            resolve_route(&routes, &envelope())
                .expect("a route")
                .route_id,
            "global"
        );
    }

    #[test]
    fn equally_specific_matches_are_rejected_not_guessed() {
        let routes = vec![
            route("b", RouteScope::account("acct-1")),
            route("a", RouteScope::account("acct-1")),
        ];
        match resolve_route(&routes, &envelope()) {
            Err(RouteError::Ambiguous {
                specificity,
                route_ids,
            }) => {
                assert_eq!(specificity, RouteSpecificity::Account);
                assert_eq!(route_ids, vec!["a".to_string(), "b".to_string()]);
            }
            other => panic!("expected ambiguity, got {other:?}"),
        }
    }

    #[test]
    fn ambiguity_at_a_lower_rung_does_not_matter_when_a_specific_route_wins() {
        let routes = vec![
            route("a", RouteScope::account("acct-1")),
            route("b", RouteScope::account("acct-1")),
            route("conv", RouteScope::conversation("acct-1", "C1")),
        ];
        assert_eq!(
            resolve_route(&routes, &envelope())
                .expect("a route")
                .route_id,
            "conv"
        );
    }

    #[test]
    fn no_matching_route_is_an_error() {
        let routes = vec![route("other", RouteScope::account("acct-2"))];
        assert_eq!(
            resolve_route(&routes, &envelope()).unwrap_err(),
            RouteError::NoRoute
        );
    }

    #[test]
    fn a_thread_scoped_route_does_not_match_the_threadless_message() {
        let mut message = envelope();
        message.conversation.thread_id = None;
        let routes = vec![route(
            "thread",
            RouteScope::conversation("acct-1", "C1").with_thread("T1"),
        )];
        assert_eq!(
            resolve_route(&routes, &message).unwrap_err(),
            RouteError::NoRoute
        );
    }

    #[test]
    fn scope_validation_rejects_off_ladder_combinations() {
        let orphan_conversation = RouteScope {
            conversation_id: Some("C1".into()),
            ..RouteScope::default()
        };
        assert_eq!(
            orphan_conversation.validate(),
            Err(RouteScopeError::MissingAccount)
        );

        let orphan_sender = RouteScope {
            account_id: Some("acct-1".into()),
            sender_id: Some("U1".into()),
            ..RouteScope::default()
        };
        assert_eq!(
            orphan_sender.validate(),
            Err(RouteScopeError::MissingConversation)
        );

        let kind_and_account = RouteScope {
            kind: Some(ChannelKind::Slack),
            account_id: Some("acct-1".into()),
            ..RouteScope::default()
        };
        assert_eq!(
            kind_and_account.validate(),
            Err(RouteScopeError::ChannelDefaultWithDetail)
        );

        assert!(RouteScope::global_default().validate().is_ok());
        assert!(RouteScope::channel_default(ChannelKind::Irc)
            .validate()
            .is_ok());
        assert!(RouteScope::conversation("a", "c")
            .with_thread("t")
            .with_sender("s")
            .validate()
            .is_ok());
    }

    #[test]
    fn session_scopes_produce_distinct_stable_keys() {
        let message = envelope();
        let thread = SessionScope::Thread.session_key(&message);
        let conversation = SessionScope::Conversation.session_key(&message);
        let sender = SessionScope::Sender.session_key(&message);
        let account = SessionScope::Account.session_key(&message);

        let unique: std::collections::BTreeSet<&String> =
            [&thread, &conversation, &sender, &account]
                .into_iter()
                .collect();
        assert_eq!(unique.len(), 4, "scopes must not collide");
        assert_eq!(thread, SessionScope::Thread.session_key(&message));
    }

    #[test]
    fn target_digest_changes_with_configuration() {
        let base = RouteTarget::new("assistant");
        let mut renamed = base.clone();
        renamed.recipe = "other".into();
        let mut reparameterized = base.clone();
        reparameterized.params.insert("tone".into(), "terse".into());
        let mut rescoped = base.clone();
        rescoped.session_scope = SessionScope::Account;

        assert_eq!(base.digest(), RouteTarget::new("assistant").digest());
        assert_ne!(base.digest(), renamed.digest());
        assert_ne!(base.digest(), reparameterized.digest());
        assert_ne!(base.digest(), rescoped.digest());
    }
}
