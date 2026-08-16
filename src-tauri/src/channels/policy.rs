//! Who is allowed to make Little Monkey do something over a messaging channel,
//! and when a message in a busy room should be answered at all.
//!
//! Two independent gates, in this order:
//!
//! 1. **Access** — is this sender allowed to submit messages to this account?
//!    Answered from the account's [`AccessPolicy`] plus the durable sender
//!    authorization row.
//! 2. **Activation** — should this particular message wake the agent? Group and
//!    channel conversations default to mention-only so joining a busy room does
//!    not turn every line into a run.
//!
//! Neither gate grants authority to *do* anything. An approved sender can submit
//! a message; the permission system still decides whether the resulting run may
//! touch files, run shell commands, reach the network, or send anything back
//! out. That separation is the whole point: message-submission rights and tool
//! rights are different things and are never conflated here.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::types::{ChannelEnvelope, ConversationKind};

/// Pairing codes live for an hour. Long enough for someone to walk to the
/// desktop and approve, short enough that a code leaked in a group log is dead
/// before it is useful.
pub const PAIRING_CODE_TTL_MS: i64 = 60 * 60 * 1000;

/// Cap on simultaneously pending pairing requests for one account. Beyond this
/// an unknown sender is ignored outright, so an open DM inbox cannot be used to
/// flood the operator's approval queue.
pub const MAX_PENDING_PAIRING_PER_ACCOUNT: usize = 16;

/// How many automated replies may chain before the conversation is treated as a
/// loop. Counted per conversation over the reply window, not globally.
pub const MAX_AUTOMATED_REPLY_DEPTH: u32 = 3;

/// Number of characters in a generated pairing code.
pub const PAIRING_CODE_LEN: usize = 8;

/// Unambiguous alphabet: no `0`/`O`, no `1`/`I`/`l`. Codes get read aloud and
/// retyped, and a code that cannot be transcribed is a support ticket.
const PAIRING_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// How an account decides whether an unknown sender may talk to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessPolicy {
    /// Nothing inbound is accepted. Messages are recorded and dropped.
    Disabled,
    /// Only senders explicitly approved by the operator may submit.
    AllowList,
    /// Unknown senders get a one-time code and wait for operator approval.
    Pairing,
    /// Anyone may submit. Never a default — the operator has to choose it, and
    /// Security Doctor flags it.
    Open,
}

impl AccessPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            AccessPolicy::Disabled => "disabled",
            AccessPolicy::AllowList => "allow_list",
            AccessPolicy::Pairing => "pairing",
            AccessPolicy::Open => "open",
        }
    }

    pub fn parse(value: &str) -> Option<AccessPolicy> {
        match value {
            "disabled" => Some(AccessPolicy::Disabled),
            "allow_list" => Some(AccessPolicy::AllowList),
            "pairing" => Some(AccessPolicy::Pairing),
            "open" => Some(AccessPolicy::Open),
            _ => None,
        }
    }
}

/// When a message in a multi-party conversation should wake the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupActivation {
    /// Every message runs. Expensive and loud; opt-in only.
    Always,
    /// Only messages that mention or reply to the configured bot identity run.
    MentionOnly,
    /// Multi-party conversations are ignored entirely.
    Disabled,
}

impl GroupActivation {
    pub fn as_str(self) -> &'static str {
        match self {
            GroupActivation::Always => "always",
            GroupActivation::MentionOnly => "mention_only",
            GroupActivation::Disabled => "disabled",
        }
    }

    pub fn parse(value: &str) -> Option<GroupActivation> {
        match value {
            "always" => Some(GroupActivation::Always),
            "mention_only" => Some(GroupActivation::MentionOnly),
            "disabled" => Some(GroupActivation::Disabled),
            _ => None,
        }
    }
}

/// The per-account access configuration.
///
/// The defaults are the conservative pair the product ships with: a DM from a
/// stranger starts a pairing handshake, and a group needs both an approved
/// sender and an explicit mention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelAccessPolicy {
    pub direct: AccessPolicy,
    pub group: AccessPolicy,
    pub group_activation: GroupActivation,
}

impl Default for ChannelAccessPolicy {
    fn default() -> Self {
        Self {
            direct: AccessPolicy::Pairing,
            group: AccessPolicy::AllowList,
            group_activation: GroupActivation::MentionOnly,
        }
    }
}

impl ChannelAccessPolicy {
    pub fn policy_for(&self, kind: ConversationKind) -> AccessPolicy {
        match kind {
            ConversationKind::Direct => self.direct,
            ConversationKind::Group | ConversationKind::Channel => self.group,
        }
    }
}

/// Durable authorization state for one sender on one account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderState {
    /// A pairing code was issued and the operator has not decided yet.
    Pending,
    /// The operator approved this sender. Grants message submission only.
    Approved,
    /// The operator blocked this sender. Sticky: blocked beats every policy,
    /// including `Open`.
    Blocked,
}

impl SenderState {
    pub fn as_str(self) -> &'static str {
        match self {
            SenderState::Pending => "pending",
            SenderState::Approved => "approved",
            SenderState::Blocked => "blocked",
        }
    }

    pub fn parse(value: &str) -> Option<SenderState> {
        match value {
            "pending" => Some(SenderState::Pending),
            "approved" => Some(SenderState::Approved),
            "blocked" => Some(SenderState::Blocked),
            _ => None,
        }
    }
}

/// What the access gate knows about a sender when it decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderAuthorization {
    pub state: SenderState,
    /// Expiry of the outstanding pairing code, when one is live.
    pub pairing_expires_at_ms: Option<i64>,
}

impl SenderAuthorization {
    pub fn approved() -> Self {
        Self {
            state: SenderState::Approved,
            pairing_expires_at_ms: None,
        }
    }

    pub fn blocked() -> Self {
        Self {
            state: SenderState::Blocked,
            pairing_expires_at_ms: None,
        }
    }

    pub fn pending(expires_at_ms: i64) -> Self {
        Self {
            state: SenderState::Pending,
            pairing_expires_at_ms: Some(expires_at_ms),
        }
    }

    fn pairing_is_live(&self, now_ms: i64) -> bool {
        matches!(self.state, SenderState::Pending)
            && self
                .pairing_expires_at_ms
                .is_some_and(|expires| expires > now_ms)
    }
}

/// Why a message was not run. Recorded on the durable event so the operator can
/// see what happened without turning on debug logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IgnoreReason {
    /// The message came from the account's own bot identity.
    OwnMessage,
    /// Inbound is switched off for this conversation kind.
    PolicyDisabled,
    /// Sender is not on the allow list and this policy does not offer pairing.
    SenderNotAllowed,
    /// Sender is explicitly blocked.
    SenderBlocked,
    /// A pairing code is already outstanding for this sender.
    PairingPending,
    /// Too many senders are already waiting for approval on this account.
    PairingQueueFull,
    /// A group message that did not mention us.
    NotMentioned,
    /// Nothing to act on — no text and no attachments.
    EmptyMessage,
    /// Automated replies have chained too deep in this conversation.
    ReplyDepthExceeded,
}

impl IgnoreReason {
    pub fn as_str(self) -> &'static str {
        match self {
            IgnoreReason::OwnMessage => "own_message",
            IgnoreReason::PolicyDisabled => "policy_disabled",
            IgnoreReason::SenderNotAllowed => "sender_not_allowed",
            IgnoreReason::SenderBlocked => "sender_blocked",
            IgnoreReason::PairingPending => "pairing_pending",
            IgnoreReason::PairingQueueFull => "pairing_queue_full",
            IgnoreReason::NotMentioned => "not_mentioned",
            IgnoreReason::EmptyMessage => "empty_message",
            IgnoreReason::ReplyDepthExceeded => "reply_depth_exceeded",
        }
    }
}

/// Outcome of the two gates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessDecision {
    /// Run the message as a normal durable turn.
    Accept,
    /// Issue a pairing challenge. The original message is *not* run — the code
    /// is the only thing that goes back out, and the sender has to wait for the
    /// operator.
    Challenge(PairingChallenge),
    /// Record and drop.
    Ignore(IgnoreReason),
}

/// A freshly minted pairing challenge.
///
/// The plaintext `code` exists only long enough to be put in the outbound reply;
/// only `code_digest` is ever persisted, so a stolen database does not hand
/// anyone a working code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingChallenge {
    pub code: String,
    pub code_digest: String,
    pub expires_at_ms: i64,
}

/// Everything the access gate needs that is not on the envelope itself.
#[derive(Debug, Clone, Copy)]
pub struct AccessContext<'a> {
    pub policy: &'a ChannelAccessPolicy,
    pub sender: Option<SenderAuthorization>,
    /// How many senders currently sit in `pending` for this account.
    pub pending_pairings: usize,
    /// Consecutive automated replies already made in this conversation.
    pub automated_reply_depth: u32,
    /// How many messages in a row this conversation has taken from a machine,
    /// with no human message in between.
    ///
    /// A different measurement from `automated_reply_depth`, and it exists
    /// because that one only sees a chain the *other* side threads.
    /// `reply_to_provider_id` is how a reply is linked to the message it
    /// answers, and a bot on the far end is under no obligation to set it — so
    /// two bots talking in a group with `Always` activation produce a chain of
    /// depth zero, forever. Counted from the durable event log rather than
    /// inferred, because the only honest source for "did a person say anything
    /// recently" is what actually arrived.
    pub consecutive_machine_messages: u32,
    pub now_ms: i64,
}

/// Decide whether an inbound message may become a durable turn.
///
/// `mint_code` supplies the challenge code. It is a parameter so tests can be
/// deterministic; production passes [`generate_pairing_code`].
pub fn decide_access(
    envelope: &ChannelEnvelope,
    context: AccessContext<'_>,
    mint_code: impl FnOnce() -> String,
) -> AccessDecision {
    // Loop prevention comes first. Our own message can never be worth running,
    // whatever the policy says, and checking it here means every provider gets
    // the protection whether or not its adapter remembered to filter.
    if envelope.sender.is_self {
        return AccessDecision::Ignore(IgnoreReason::OwnMessage);
    }

    if let Some(authorization) = context.sender {
        if matches!(authorization.state, SenderState::Blocked) {
            return AccessDecision::Ignore(IgnoreReason::SenderBlocked);
        }
    }

    let policy = context.policy.policy_for(envelope.conversation.kind);
    if matches!(policy, AccessPolicy::Disabled) {
        return AccessDecision::Ignore(IgnoreReason::PolicyDisabled);
    }

    let approved = context
        .sender
        .is_some_and(|authorization| matches!(authorization.state, SenderState::Approved));

    if !approved {
        match policy {
            AccessPolicy::Open => {}
            AccessPolicy::AllowList => {
                return AccessDecision::Ignore(IgnoreReason::SenderNotAllowed);
            }
            AccessPolicy::Pairing => {
                // Pairing is a DM handshake. In a group there is no private
                // channel to deliver a code over, so an unknown sender in a
                // group is simply not allowed.
                if envelope.conversation.kind.is_multi_party() {
                    return AccessDecision::Ignore(IgnoreReason::SenderNotAllowed);
                }
                if let Some(authorization) = context.sender {
                    if authorization.pairing_is_live(context.now_ms) {
                        // A code is already out. Re-issuing on every message
                        // would let a stranger mint unlimited codes.
                        return AccessDecision::Ignore(IgnoreReason::PairingPending);
                    }
                }
                if context.pending_pairings >= MAX_PENDING_PAIRING_PER_ACCOUNT {
                    return AccessDecision::Ignore(IgnoreReason::PairingQueueFull);
                }
                let code = mint_code();
                return AccessDecision::Challenge(PairingChallenge {
                    code_digest: pairing_code_digest(&code),
                    code,
                    expires_at_ms: context.now_ms.saturating_add(PAIRING_CODE_TTL_MS),
                });
            }
            AccessPolicy::Disabled => unreachable!("handled above"),
        }
    }

    // Activation gate. Only reached by a sender who is allowed to talk to us.
    if envelope.conversation.kind.is_multi_party() {
        match context.policy.group_activation {
            GroupActivation::Disabled => {
                return AccessDecision::Ignore(IgnoreReason::PolicyDisabled);
            }
            GroupActivation::MentionOnly => {
                if !envelope.mentions_self {
                    return AccessDecision::Ignore(IgnoreReason::NotMentioned);
                }
            }
            GroupActivation::Always => {}
        }
    }

    if context.automated_reply_depth >= MAX_AUTOMATED_REPLY_DEPTH {
        return AccessDecision::Ignore(IgnoreReason::ReplyDepthExceeded);
    }

    // The same bound, measured the other way. Telegram, Discord, Slack and an
    // extension provider all report whether a sender is a bot, and until now
    // nothing read it: an exchange between two bots that do not thread their
    // replies inherits a depth of zero on every message and never converges.
    //
    // A person speaking resets the count, so this costs a human conversation
    // nothing however long it runs — the budget is spent only by a stretch of
    // machine messages with nobody in it.
    if envelope.sender.is_bot && context.consecutive_machine_messages >= MAX_AUTOMATED_REPLY_DEPTH {
        return AccessDecision::Ignore(IgnoreReason::ReplyDepthExceeded);
    }

    if !envelope.has_text() && envelope.attachments.is_empty() {
        return AccessDecision::Ignore(IgnoreReason::EmptyMessage);
    }

    AccessDecision::Accept
}

/// Generate a pairing code from the OS CSPRNG.
///
/// Rejection sampling keeps the alphabet uniform; a modulo would bias the first
/// few characters, which is exactly the kind of small mistake that makes a short
/// code guessable.
pub fn generate_pairing_code() -> Result<String, String> {
    use ring::rand::SecureRandom as _;

    let rng = ring::rand::SystemRandom::new();
    let mut code = String::with_capacity(PAIRING_CODE_LEN);
    let limit = (256 / PAIRING_ALPHABET.len()) * PAIRING_ALPHABET.len();
    let mut buffer = [0_u8; 32];
    while code.len() < PAIRING_CODE_LEN {
        rng.fill(&mut buffer)
            .map_err(|_| "Failed to read system randomness for a pairing code".to_string())?;
        for byte in buffer {
            if code.len() == PAIRING_CODE_LEN {
                break;
            }
            let value = usize::from(byte);
            if value >= limit {
                continue;
            }
            code.push(char::from(PAIRING_ALPHABET[value % PAIRING_ALPHABET.len()]));
        }
    }
    Ok(code)
}

/// Digest a pairing code for storage. Plaintext codes are never persisted.
pub fn pairing_code_digest(code: &str) -> String {
    let digest = Sha256::digest(code.trim().to_ascii_uppercase().as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Constant-time comparison of a submitted code against a stored digest.
pub fn pairing_code_matches(submitted: &str, stored_digest: &str) -> bool {
    let candidate = pairing_code_digest(submitted);
    if candidate.len() != stored_digest.len() {
        return false;
    }
    candidate
        .as_bytes()
        .iter()
        .zip(stored_digest.as_bytes())
        .fold(0_u8, |accumulator, (left, right)| {
            accumulator | (left ^ right)
        })
        == 0
}

/// The message sent back to a stranger who triggered a pairing challenge.
///
/// Deliberately says nothing about the host, the operator, or what Little Monkey
/// can do — an unpaired sender has not earned any of that.
pub fn pairing_challenge_reply(code: &str) -> String {
    format!(
        "This assistant is not paired with you yet. Give this code to its operator to approve you: {code}\n\
         The code expires in 1 hour. Your message was not processed."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::types::{
        BoundedMetadata, ChannelConversation, ChannelKind, ChannelSender,
    };

    fn envelope(kind: ConversationKind, mentions_self: bool) -> ChannelEnvelope {
        ChannelEnvelope {
            account_id: "acct".into(),
            kind: ChannelKind::Telegram,
            provider_event_id: "1".into(),
            conversation: ChannelConversation {
                conversation_id: "c1".into(),
                kind,
                thread_id: None,
                title: None,
            },
            sender: ChannelSender::new("sender-1"),
            text: "hello".into(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            mentions_self,
            received_at_ms: 1_000,
            metadata: BoundedMetadata::new(),
        }
    }

    fn context<'a>(
        policy: &'a ChannelAccessPolicy,
        sender: Option<SenderAuthorization>,
    ) -> AccessContext<'a> {
        AccessContext {
            policy,
            sender,
            pending_pairings: 0,
            automated_reply_depth: 0,
            consecutive_machine_messages: 0,
            now_ms: 1_000,
        }
    }

    fn fixed_code() -> String {
        "ABCD2345".to_string()
    }

    #[test]
    fn own_messages_never_run_even_under_open_policy() {
        let policy = ChannelAccessPolicy {
            direct: AccessPolicy::Open,
            group: AccessPolicy::Open,
            group_activation: GroupActivation::Always,
        };
        let mut message = envelope(ConversationKind::Direct, true);
        message.sender.is_self = true;
        assert_eq!(
            decide_access(&message, context(&policy, None), fixed_code),
            AccessDecision::Ignore(IgnoreReason::OwnMessage)
        );
    }

    #[test]
    fn blocked_sender_beats_open_policy() {
        let policy = ChannelAccessPolicy {
            direct: AccessPolicy::Open,
            ..ChannelAccessPolicy::default()
        };
        let message = envelope(ConversationKind::Direct, false);
        assert_eq!(
            decide_access(
                &message,
                context(&policy, Some(SenderAuthorization::blocked())),
                fixed_code
            ),
            AccessDecision::Ignore(IgnoreReason::SenderBlocked)
        );
    }

    #[test]
    fn unknown_dm_under_pairing_gets_a_challenge_and_does_not_run() {
        let policy = ChannelAccessPolicy::default();
        let message = envelope(ConversationKind::Direct, false);
        match decide_access(&message, context(&policy, None), fixed_code) {
            AccessDecision::Challenge(challenge) => {
                assert_eq!(challenge.code, "ABCD2345");
                assert_eq!(challenge.code_digest, pairing_code_digest("ABCD2345"));
                assert_eq!(challenge.expires_at_ms, 1_000 + PAIRING_CODE_TTL_MS);
            }
            other => panic!("expected a pairing challenge, got {other:?}"),
        }
    }

    #[test]
    fn a_live_pairing_code_is_not_reissued() {
        let policy = ChannelAccessPolicy::default();
        let message = envelope(ConversationKind::Direct, false);
        let pending = SenderAuthorization::pending(1_000 + PAIRING_CODE_TTL_MS);
        assert_eq!(
            decide_access(&message, context(&policy, Some(pending)), fixed_code),
            AccessDecision::Ignore(IgnoreReason::PairingPending)
        );
    }

    #[test]
    fn an_expired_pairing_code_can_be_reissued() {
        let policy = ChannelAccessPolicy::default();
        let message = envelope(ConversationKind::Direct, false);
        let expired = SenderAuthorization::pending(999);
        assert!(matches!(
            decide_access(&message, context(&policy, Some(expired)), fixed_code),
            AccessDecision::Challenge(_)
        ));
    }

    #[test]
    fn the_pending_pairing_queue_is_capped() {
        let policy = ChannelAccessPolicy::default();
        let message = envelope(ConversationKind::Direct, false);
        let mut ctx = context(&policy, None);
        ctx.pending_pairings = MAX_PENDING_PAIRING_PER_ACCOUNT;
        assert_eq!(
            decide_access(&message, ctx, fixed_code),
            AccessDecision::Ignore(IgnoreReason::PairingQueueFull)
        );
    }

    #[test]
    fn pairing_offers_no_challenge_in_a_group() {
        let policy = ChannelAccessPolicy {
            group: AccessPolicy::Pairing,
            ..ChannelAccessPolicy::default()
        };
        let message = envelope(ConversationKind::Group, true);
        assert_eq!(
            decide_access(&message, context(&policy, None), fixed_code),
            AccessDecision::Ignore(IgnoreReason::SenderNotAllowed)
        );
    }

    #[test]
    fn approved_group_sender_still_needs_a_mention_by_default() {
        let policy = ChannelAccessPolicy::default();
        let approved = Some(SenderAuthorization::approved());

        let unmentioned = envelope(ConversationKind::Group, false);
        assert_eq!(
            decide_access(&unmentioned, context(&policy, approved), fixed_code),
            AccessDecision::Ignore(IgnoreReason::NotMentioned)
        );

        let mentioned = envelope(ConversationKind::Group, true);
        assert_eq!(
            decide_access(&mentioned, context(&policy, approved), fixed_code),
            AccessDecision::Accept
        );
    }

    #[test]
    fn always_activation_runs_unmentioned_group_messages() {
        let policy = ChannelAccessPolicy {
            group_activation: GroupActivation::Always,
            ..ChannelAccessPolicy::default()
        };
        let message = envelope(ConversationKind::Group, false);
        assert_eq!(
            decide_access(
                &message,
                context(&policy, Some(SenderAuthorization::approved())),
                fixed_code
            ),
            AccessDecision::Accept
        );
    }

    #[test]
    fn reply_depth_stops_a_bot_loop() {
        let policy = ChannelAccessPolicy::default();
        let message = envelope(ConversationKind::Direct, false);
        let mut ctx = context(&policy, Some(SenderAuthorization::approved()));
        ctx.automated_reply_depth = MAX_AUTOMATED_REPLY_DEPTH;
        assert_eq!(
            decide_access(&message, ctx, fixed_code),
            AccessDecision::Ignore(IgnoreReason::ReplyDepthExceeded)
        );
    }

    /// The loop the reply-depth chain cannot see.
    ///
    /// `automated_reply_depth` is inherited through `reply_to_provider_id`, and
    /// a bot on the far end need not thread anything — so an unthreaded
    /// exchange between two bots arrives at depth zero on every message and
    /// runs forever. The streak of machine messages is what bounds it, and it
    /// is only consulted for a sender the provider itself calls a bot.
    #[test]
    fn an_unthreaded_bot_exchange_is_bounded_by_the_machine_streak() {
        let policy = ChannelAccessPolicy {
            group_activation: GroupActivation::Always,
            ..ChannelAccessPolicy::default()
        };
        let mut message = envelope(ConversationKind::Group, false);
        message.sender.is_bot = true;

        // Under the ceiling, with nothing threaded, it still runs.
        let mut ctx = context(&policy, Some(SenderAuthorization::approved()));
        ctx.consecutive_machine_messages = MAX_AUTOMATED_REPLY_DEPTH - 1;
        assert_eq!(ctx.automated_reply_depth, 0);
        assert_eq!(
            decide_access(&message, ctx, fixed_code),
            AccessDecision::Accept
        );

        ctx.consecutive_machine_messages = MAX_AUTOMATED_REPLY_DEPTH;
        assert_eq!(
            decide_access(&message, ctx, fixed_code),
            AccessDecision::Ignore(IgnoreReason::ReplyDepthExceeded)
        );
    }

    /// A person is never rate-limited by a machine's streak.
    ///
    /// The budget exists to stop two programs talking to each other. Spending
    /// it on somebody in a busy group -- where the count could be high for
    /// reasons that have nothing to do with them -- would turn a loop guard
    /// into a silent outage.
    #[test]
    fn a_person_is_never_refused_for_a_machine_streak() {
        let policy = ChannelAccessPolicy {
            group_activation: GroupActivation::Always,
            ..ChannelAccessPolicy::default()
        };
        let message = envelope(ConversationKind::Group, false);
        assert!(!message.sender.is_bot);
        let mut ctx = context(&policy, Some(SenderAuthorization::approved()));
        ctx.consecutive_machine_messages = MAX_AUTOMATED_REPLY_DEPTH * 10;
        assert_eq!(
            decide_access(&message, ctx, fixed_code),
            AccessDecision::Accept
        );
    }

    #[test]
    fn an_empty_message_with_no_attachments_is_ignored() {
        let policy = ChannelAccessPolicy::default();
        let mut message = envelope(ConversationKind::Direct, false);
        message.text = "   ".into();
        assert_eq!(
            decide_access(
                &message,
                context(&policy, Some(SenderAuthorization::approved())),
                fixed_code
            ),
            AccessDecision::Ignore(IgnoreReason::EmptyMessage)
        );
    }

    #[test]
    fn an_attachment_only_message_still_runs() {
        use crate::channels::types::{AttachmentKind, AttachmentSource, ChannelAttachment};

        let policy = ChannelAccessPolicy::default();
        let mut message = envelope(ConversationKind::Direct, false);
        message.text = String::new();
        message.attachments.push(ChannelAttachment {
            stored_artifact_id: None,
            text_excerpt: None,
            fetch_error: None,
            provider_id: Some("file-1".into()),
            kind: AttachmentKind::Image,
            filename: None,
            mime_type: None,
            declared_size_bytes: Some(10),
            stored_size_bytes: None,
            source: AttachmentSource::ProviderHandle {
                handle: "file-1".into(),
            },
        });
        assert_eq!(
            decide_access(
                &message,
                context(&policy, Some(SenderAuthorization::approved())),
                fixed_code
            ),
            AccessDecision::Accept
        );
    }

    #[test]
    fn generated_codes_use_the_unambiguous_alphabet_and_vary() {
        let first = generate_pairing_code().expect("code");
        let second = generate_pairing_code().expect("code");
        assert_eq!(first.len(), PAIRING_CODE_LEN);
        for character in first.chars() {
            assert!(
                PAIRING_ALPHABET.contains(&(character as u8)),
                "unexpected character {character}"
            );
        }
        assert_ne!(first, second, "two codes in a row must not collide");
    }

    #[test]
    fn code_matching_is_case_insensitive_and_rejects_wrong_codes() {
        let digest = pairing_code_digest("ABCD2345");
        assert!(pairing_code_matches("abcd2345", &digest));
        assert!(pairing_code_matches(" ABCD2345 ", &digest));
        assert!(!pairing_code_matches("ABCD2346", &digest));
        assert!(!pairing_code_matches("", &digest));
    }

    #[test]
    fn the_challenge_reply_carries_the_code_and_no_host_detail() {
        let reply = pairing_challenge_reply("ABCD2345");
        assert!(reply.contains("ABCD2345"));
        assert!(reply.contains("was not processed"));
    }
}
