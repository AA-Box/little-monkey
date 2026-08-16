//! Messages and task requests exchanged between two paired Little Monkey
//! installations.
//!
//! A peer is another install the operator paired deliberately, not a service
//! and not a federation network: there is no directory, no relay and no
//! central identity. Everything here rides the remote trust primitives that
//! already carry a paired phone — the same TLS pin, the same HMAC signature,
//! the same replay window — and adds only what peer-to-peer traffic needs that
//! a phone does not: a hop limit and an origin chain, so a message cannot
//! circulate between three installs forever.
//!
//! # What a peer is not
//!
//! Peer standing grants nothing local. A peer can ask; the receiving install
//! decides, under its own routes, its own permission policy and its own
//! approvals. The sender cannot name a workspace, a tool, a model or a device
//! — nothing in [`PeerEnvelope`] carries any of those, which is the point of
//! it being this small.
//!
//! # Untrusted
//!
//! Peer text is someone else's words, exactly like a message from a stranger
//! on a messaging channel. It reaches the model wrapped as untrusted data,
//! never as instructions. See [`crate::channels::ingress::ConversationSource`].

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Envelope version. Bumped when a field's meaning changes; a receiver refuses
/// what it does not understand rather than guessing.
pub const PEER_ENVELOPE_VERSION: u32 = 1;

/// Default hops a message may still make. Four is enough for an operator's own
/// small mesh (laptop → desktop → server) with room to spare.
pub const DEFAULT_HOP_LIMIT: u8 = 4;

/// Hard ceiling on the hop limit, whatever a sender asks for. A sender that
/// wants more than this is not describing a personal mesh any more.
pub const MAX_HOP_LIMIT: u8 = 8;

/// Longest body a peer may send. Big enough for a real request, small enough
/// that an unattended install cannot be filled up by one.
pub const MAX_BODY_BYTES: usize = 16 * 1024;

/// Most artifact references one envelope may carry.
pub const MAX_ARTIFACT_REFS: usize = 8;

/// Artifact metadata is display data, not a path. Keep each field bounded even
/// though only a handful of references fit in an envelope.
pub const MAX_ARTIFACT_FILENAME_BYTES: usize = 255;
pub const MAX_ARTIFACT_MEDIA_TYPE_BYTES: usize = 255;

/// A peer reference may never advertise content larger than the remote
/// artifact ceiling. The content store applies the same limit before a local
/// id is offered by the agent tool.
pub const MAX_PEER_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;

/// Longest an envelope may claim to live. Past this a sender is asking a
/// receiver to hold state indefinitely.
pub const MAX_TTL_MS: i64 = 24 * 60 * 60 * 1_000;

/// A signed request already has a five-minute clock-skew window. The envelope
/// gets the same allowance, rather than accepting work authored arbitrarily
/// far in the future.
pub const MAX_FUTURE_SKEW_MS: i64 = 5 * 60 * 1_000;

/// Longest identifier accepted anywhere in an envelope.
pub const MAX_ID_LEN: usize = 128;

/// Most installs an origin chain may name before it stops being a personal
/// mesh and starts being a relay network.
pub const MAX_ORIGIN_CHAIN: usize = MAX_HOP_LIMIT as usize;

/// What a peer is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerMessageKind {
    /// Say something in a thread. It becomes a conversation turn.
    Message,
    /// Ask the receiver to do something. Also a conversation turn — the
    /// difference is that the sender expects a result to correlate against,
    /// not that it runs under different authority.
    TaskRequest,
    /// Hand over artifact references belonging to an existing thread.
    Artifact,
}

impl PeerMessageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PeerMessageKind::Message => "message",
            PeerMessageKind::TaskRequest => "task_request",
            PeerMessageKind::Artifact => "artifact",
        }
    }

    pub fn parse(value: &str) -> Option<PeerMessageKind> {
        match value {
            "message" => Some(PeerMessageKind::Message),
            "task_request" => Some(PeerMessageKind::TaskRequest),
            "artifact" => Some(PeerMessageKind::Artifact),
            _ => None,
        }
    }

    /// Whether this kind expects the receiver to run something and report back.
    pub fn expects_result(self) -> bool {
        matches!(self, PeerMessageKind::TaskRequest)
    }
}

/// A reference to an artifact the sender already holds.
///
/// An id and a digest, never a path and never bytes: the receiver fetches it
/// through the artifact mechanisms it already has, so a peer cannot name a
/// file on this machine and cannot push one into it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerArtifactRef {
    pub artifact_id: String,
    /// SHA-256 of the content, so the receiver can prove what it fetched is
    /// what was offered.
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

/// One bounded thing one install says to another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerEnvelope {
    pub version: u32,
    /// Unique per sender. The receiver's dedupe key, so a retried delivery
    /// collapses instead of running twice.
    pub message_id: String,
    /// Conversation this belongs to. Minted by whoever starts the exchange and
    /// echoed by both sides.
    pub thread_id: String,
    /// The sender's handle for this specific request, returned with the result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    pub kind: PeerMessageKind,
    /// Instance that authored this envelope.
    pub sender_instance_id: String,
    /// Every install this envelope has passed through, oldest first, including
    /// the sender. A receiver that finds itself here drops the message: that is
    /// a loop, and a hop limit alone would only make it a slow one.
    #[serde(default)]
    pub origin_chain: Vec<String>,
    /// Hops still allowed. Decremented on each forward; zero cannot travel.
    pub hop_limit: u8,
    pub created_at_ms: i64,
    /// When this stops being worth acting on. A receiver refuses an expired
    /// envelope rather than running stale work.
    pub expires_at_ms: i64,
    /// The text. Untrusted, bounded, and never instructions.
    #[serde(default)]
    pub body: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<PeerArtifactRef>,
}

/// Why an envelope was refused.
///
/// Every variant is a rejection the receiver can state without revealing
/// anything about its configuration: a peer learns that its message was not
/// accepted and why in its own terms, never what routes or policies exist here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRejection {
    UnsupportedVersion,
    MalformedId,
    BodyTooLarge,
    TooManyArtifacts,
    MissingBody,
    HopLimitExceeded,
    ZeroHops,
    OriginLoop,
    InvalidOriginChain,
    OriginChainTooLong,
    InvalidTimestamp,
    CreatedInFuture,
    Expired,
    ExpiryTooFar,
    ArtifactMetadataTooLarge,
    ArtifactTooLarge,
    /// The envelope names content this installation was never handed. A sender
    /// uploads before it references; anything else would be a reference to
    /// bytes nobody here can read.
    ArtifactUnavailable,
    Duplicate,
    MissingCapability,
    PeerRevoked,
    LocalUnavailable,
}

impl PeerRejection {
    pub fn as_str(self) -> &'static str {
        match self {
            PeerRejection::UnsupportedVersion => "unsupported_version",
            PeerRejection::MalformedId => "malformed_id",
            PeerRejection::BodyTooLarge => "body_too_large",
            PeerRejection::TooManyArtifacts => "too_many_artifacts",
            PeerRejection::MissingBody => "missing_body",
            PeerRejection::HopLimitExceeded => "hop_limit_exceeded",
            PeerRejection::ZeroHops => "zero_hops",
            PeerRejection::OriginLoop => "origin_loop",
            PeerRejection::InvalidOriginChain => "invalid_origin_chain",
            PeerRejection::OriginChainTooLong => "origin_chain_too_long",
            PeerRejection::InvalidTimestamp => "invalid_timestamp",
            PeerRejection::CreatedInFuture => "created_in_future",
            PeerRejection::Expired => "expired",
            PeerRejection::ExpiryTooFar => "expiry_too_far",
            PeerRejection::ArtifactMetadataTooLarge => "artifact_metadata_too_large",
            PeerRejection::ArtifactTooLarge => "artifact_too_large",
            PeerRejection::ArtifactUnavailable => "artifact_unavailable",
            PeerRejection::Duplicate => "duplicate",
            PeerRejection::MissingCapability => "missing_capability",
            PeerRejection::PeerRevoked => "peer_revoked",
            PeerRejection::LocalUnavailable => "local_unavailable",
        }
    }

    /// What the sender is told. Deliberately the same string an operator sees:
    /// there is nothing here worth hiding, and nothing that describes the
    /// receiver's setup.
    pub fn message(self) -> &'static str {
        match self {
            PeerRejection::UnsupportedVersion => "This peer envelope version is not supported",
            PeerRejection::MalformedId => "An identifier in the envelope is malformed",
            PeerRejection::BodyTooLarge => "The message body is larger than a peer may send",
            PeerRejection::TooManyArtifacts => "The envelope references too many artifacts",
            PeerRejection::MissingBody => "The envelope carries neither text nor artifacts",
            PeerRejection::HopLimitExceeded => "The hop limit is above the maximum",
            PeerRejection::ZeroHops => "The envelope has no hops left",
            PeerRejection::OriginLoop => "This installation is already in the origin chain",
            PeerRejection::InvalidOriginChain => {
                "The origin chain does not describe one unambiguous route"
            }
            PeerRejection::OriginChainTooLong => "The origin chain is longer than allowed",
            PeerRejection::InvalidTimestamp => "The envelope timestamps are malformed",
            PeerRejection::CreatedInFuture => "The envelope was created too far in the future",
            PeerRejection::Expired => "The envelope expired before it arrived",
            PeerRejection::ExpiryTooFar => "The envelope asks to stay valid for too long",
            PeerRejection::ArtifactMetadataTooLarge => {
                "An artifact reference carries invalid or oversized metadata"
            }
            PeerRejection::ArtifactTooLarge => {
                "An artifact reference is larger than a peer may offer"
            }
            PeerRejection::ArtifactUnavailable => {
                "An artifact was referenced before its content was handed over"
            }
            PeerRejection::Duplicate => "This message was delivered before",
            PeerRejection::MissingCapability => "This peer was not granted that capability",
            PeerRejection::PeerRevoked => "This peer's pairing was revoked",
            PeerRejection::LocalUnavailable => "This installation cannot route peer work right now",
        }
    }
}

impl PeerEnvelope {
    /// A minimal well-formed envelope. The caller sets what it means.
    pub fn new(
        message_id: impl Into<String>,
        thread_id: impl Into<String>,
        kind: PeerMessageKind,
        sender_instance_id: impl Into<String>,
        body: impl Into<String>,
        created_at_ms: i64,
        ttl_ms: i64,
    ) -> Self {
        let sender_instance_id = sender_instance_id.into();
        Self {
            version: PEER_ENVELOPE_VERSION,
            message_id: message_id.into(),
            thread_id: thread_id.into(),
            correlation_id: None,
            kind,
            origin_chain: vec![sender_instance_id.clone()],
            sender_instance_id,
            hop_limit: DEFAULT_HOP_LIMIT,
            created_at_ms,
            expires_at_ms: created_at_ms.saturating_add(ttl_ms.clamp(1, MAX_TTL_MS)),
            body: body.into(),
            artifacts: Vec::new(),
        }
    }

    /// Check everything a sender can prove before the envelope leaves.
    pub fn validate_for_send(&self, now_ms: i64) -> Result<(), PeerRejection> {
        self.validate_common(now_ms)
    }

    /// Check the envelope plus the receiver-specific origin-loop rule.
    pub fn validate_for_receive(
        &self,
        local_instance_id: &str,
        now_ms: i64,
    ) -> Result<(), PeerRejection> {
        self.validate_common(now_ms)?;
        if self.origin_chain.iter().any(|hop| hop == local_instance_id) {
            return Err(PeerRejection::OriginLoop);
        }
        Ok(())
    }

    /// Backward-compatible receiver validation entry point.
    ///
    /// Deliberately does not take a store: the parts that need one — dedupe,
    /// revocation, capability — are the receiver's, and keeping them out means
    /// the loop, bound and expiry rules are provable without a database.
    pub fn validate(&self, local_instance_id: &str, now_ms: i64) -> Result<(), PeerRejection> {
        self.validate_for_receive(local_instance_id, now_ms)
    }

    fn validate_common(&self, now_ms: i64) -> Result<(), PeerRejection> {
        if self.version != PEER_ENVELOPE_VERSION {
            return Err(PeerRejection::UnsupportedVersion);
        }
        for id in [&self.message_id, &self.thread_id, &self.sender_instance_id] {
            check_id(id)?;
        }
        if let Some(correlation_id) = &self.correlation_id {
            check_id(correlation_id)?;
        }
        if self.hop_limit > MAX_HOP_LIMIT {
            return Err(PeerRejection::HopLimitExceeded);
        }
        if self.hop_limit == 0 {
            return Err(PeerRejection::ZeroHops);
        }
        if self.origin_chain.len() > MAX_ORIGIN_CHAIN {
            return Err(PeerRejection::OriginChainTooLong);
        }
        if self.origin_chain.first() != Some(&self.sender_instance_id) {
            return Err(PeerRejection::InvalidOriginChain);
        }
        let mut unique_hops = BTreeSet::new();
        for hop in &self.origin_chain {
            check_id(hop)?;
            if !unique_hops.insert(hop.as_str()) {
                return Err(PeerRejection::InvalidOriginChain);
            }
        }
        if self.body.len() > MAX_BODY_BYTES {
            return Err(PeerRejection::BodyTooLarge);
        }
        if self.artifacts.len() > MAX_ARTIFACT_REFS {
            return Err(PeerRejection::TooManyArtifacts);
        }
        for artifact in &self.artifacts {
            check_id(&artifact.artifact_id)?;
            if artifact.sha256.len() != 64
                || !artifact.sha256.chars().all(|c| c.is_ascii_hexdigit())
            {
                return Err(PeerRejection::MalformedId);
            }
            if artifact
                .size_bytes
                .is_some_and(|size| size > MAX_PEER_ARTIFACT_BYTES)
            {
                return Err(PeerRejection::ArtifactTooLarge);
            }
            if artifact
                .filename
                .as_deref()
                .is_some_and(|value| !valid_filename(value))
                || artifact
                    .media_type
                    .as_deref()
                    .is_some_and(|value| !valid_media_type(value))
            {
                return Err(PeerRejection::ArtifactMetadataTooLarge);
            }
        }
        if self.body.trim().is_empty() && self.artifacts.is_empty() {
            return Err(PeerRejection::MissingBody);
        }
        if self.created_at_ms <= 0 || self.expires_at_ms <= self.created_at_ms {
            return Err(PeerRejection::InvalidTimestamp);
        }
        if self.created_at_ms > now_ms.saturating_add(MAX_FUTURE_SKEW_MS) {
            return Err(PeerRejection::CreatedInFuture);
        }
        if self.expires_at_ms <= now_ms {
            return Err(PeerRejection::Expired);
        }
        if self.expires_at_ms.saturating_sub(self.created_at_ms) > MAX_TTL_MS {
            return Err(PeerRejection::ExpiryTooFar);
        }
        Ok(())
    }

    /// The capability a receiver must have granted this peer for this envelope.
    ///
    /// Artifact references are their own grant even on a plain message: handing
    /// over content is not the same act as saying something, and an operator
    /// who allowed one has not agreed to the other.
    pub fn required_capabilities(&self) -> BTreeSet<PeerCapability> {
        let mut required = BTreeSet::new();
        match self.kind {
            PeerMessageKind::Message => {
                required.insert(PeerCapability::Message);
            }
            PeerMessageKind::TaskRequest => {
                required.insert(PeerCapability::TaskRequest);
            }
            PeerMessageKind::Artifact => {
                required.insert(PeerCapability::Artifact);
            }
        }
        if !self.artifacts.is_empty() {
            required.insert(PeerCapability::Artifact);
        }
        required
    }

    /// Identity a receiver deduplicates on: the sender plus its own message id.
    /// No timestamp, so a retry collapses.
    pub fn dedupe_key(&self) -> String {
        format!("{}:{}", self.sender_instance_id, self.message_id)
    }

    /// This envelope, prepared to travel one more hop from this installation.
    ///
    /// Forwarding is not implemented anywhere yet; this exists so the hop
    /// accounting has exactly one definition when it is.
    pub fn forwarded_from(&self, local_instance_id: &str) -> Result<PeerEnvelope, PeerRejection> {
        if self.hop_limit <= 1 {
            return Err(PeerRejection::ZeroHops);
        }
        if self.origin_chain.len() >= MAX_ORIGIN_CHAIN {
            return Err(PeerRejection::OriginChainTooLong);
        }
        if self.origin_chain.iter().any(|hop| hop == local_instance_id) {
            return Err(PeerRejection::OriginLoop);
        }
        check_id(local_instance_id)?;
        let mut forwarded = self.clone();
        forwarded.hop_limit -= 1;
        forwarded.origin_chain.push(local_instance_id.to_string());
        Ok(forwarded)
    }
}

/// The three things a peer may be allowed to do, named on their own rather
/// than folded into the device capability list's meaning.
///
/// These map onto the remote protocol's capability grants; keeping a small
/// enum here lets the envelope state its own requirement without the pure
/// types depending on the daemon's protocol module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerCapability {
    Message,
    TaskRequest,
    Artifact,
}

impl PeerCapability {
    pub fn as_str(self) -> &'static str {
        match self {
            PeerCapability::Message => "peer_message",
            PeerCapability::TaskRequest => "peer_task_request",
            PeerCapability::Artifact => "peer_artifact",
        }
    }

    pub fn parse(value: &str) -> Option<PeerCapability> {
        match value {
            "peer_message" => Some(PeerCapability::Message),
            "peer_task_request" => Some(PeerCapability::TaskRequest),
            "peer_artifact" => Some(PeerCapability::Artifact),
            _ => None,
        }
    }
}

fn check_id(value: &str) -> Result<(), PeerRejection> {
    if value.is_empty() || value.len() > MAX_ID_LEN {
        return Err(PeerRejection::MalformedId);
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
    {
        return Err(PeerRejection::MalformedId);
    }
    Ok(())
}

fn valid_filename(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= MAX_ARTIFACT_FILENAME_BYTES
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\'])
        && !value.chars().any(char::is_control)
}

fn valid_media_type(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= MAX_ARTIFACT_MEDIA_TYPE_BYTES
        && value.is_ascii()
        && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000_000;
    const LOCAL: &str = "instance-local";

    fn envelope(kind: PeerMessageKind) -> PeerEnvelope {
        PeerEnvelope::new(
            "msg-1",
            "thread-1",
            kind,
            "instance-remote",
            "look at the failing test",
            NOW,
            60_000,
        )
    }

    #[test]
    fn a_well_formed_envelope_is_accepted() {
        assert_eq!(
            envelope(PeerMessageKind::Message).validate(LOCAL, NOW + 1),
            Ok(())
        );
    }

    #[test]
    fn an_envelope_that_has_already_been_here_is_a_loop() {
        let mut looped = envelope(PeerMessageKind::Message);
        looped.origin_chain.push(LOCAL.to_string());
        assert_eq!(
            looped.validate(LOCAL, NOW + 1),
            Err(PeerRejection::OriginLoop)
        );
    }

    #[test]
    fn hops_are_bounded_at_both_ends() {
        let mut greedy = envelope(PeerMessageKind::Message);
        greedy.hop_limit = MAX_HOP_LIMIT + 1;
        assert_eq!(
            greedy.validate(LOCAL, NOW + 1),
            Err(PeerRejection::HopLimitExceeded)
        );

        let mut spent = envelope(PeerMessageKind::Message);
        spent.hop_limit = 0;
        assert_eq!(spent.validate(LOCAL, NOW + 1), Err(PeerRejection::ZeroHops));
    }

    #[test]
    fn forwarding_spends_a_hop_and_records_the_stop() {
        let received = envelope(PeerMessageKind::Message);
        let forwarded = received.forwarded_from(LOCAL).expect("forwardable");

        assert_eq!(forwarded.hop_limit, received.hop_limit - 1);
        assert_eq!(
            forwarded.origin_chain.last().map(String::as_str),
            Some(LOCAL)
        );
        // And the next install refuses to send it back to us.
        assert_eq!(
            forwarded.validate(LOCAL, NOW + 1),
            Err(PeerRejection::OriginLoop)
        );

        let mut last_hop = envelope(PeerMessageKind::Message);
        last_hop.hop_limit = 1;
        assert_eq!(last_hop.forwarded_from(LOCAL), Err(PeerRejection::ZeroHops));
    }

    #[test]
    fn an_expired_envelope_is_refused_rather_than_run_late() {
        let stale = envelope(PeerMessageKind::TaskRequest);
        assert_eq!(
            stale.validate(LOCAL, stale.expires_at_ms),
            Err(PeerRejection::Expired)
        );

        let mut immortal = envelope(PeerMessageKind::Message);
        immortal.expires_at_ms = NOW + MAX_TTL_MS + 1;
        assert_eq!(
            immortal.validate(LOCAL, NOW + 1),
            Err(PeerRejection::ExpiryTooFar)
        );
    }

    #[test]
    fn a_body_is_bounded_and_something_has_to_be_in_it() {
        let mut huge = envelope(PeerMessageKind::Message);
        huge.body = "x".repeat(MAX_BODY_BYTES + 1);
        assert_eq!(
            huge.validate(LOCAL, NOW + 1),
            Err(PeerRejection::BodyTooLarge)
        );

        let mut empty = envelope(PeerMessageKind::Message);
        empty.body = "   ".into();
        assert_eq!(
            empty.validate(LOCAL, NOW + 1),
            Err(PeerRejection::MissingBody)
        );

        // Artifacts alone are content enough.
        let mut only_files = empty.clone();
        only_files.artifacts.push(PeerArtifactRef {
            artifact_id: "art-1".into(),
            sha256: "a".repeat(64),
            filename: Some("log.txt".into()),
            media_type: Some("text/plain".into()),
            size_bytes: Some(12),
        });
        assert_eq!(only_files.validate(LOCAL, NOW + 1), Ok(()));
    }

    #[test]
    fn an_artifact_reference_carries_a_digest_not_a_path() {
        let mut forged = envelope(PeerMessageKind::Artifact);
        forged.artifacts.push(PeerArtifactRef {
            artifact_id: "../../etc/passwd".into(),
            sha256: "b".repeat(64),
            filename: None,
            media_type: None,
            size_bytes: None,
        });
        assert_eq!(
            forged.validate(LOCAL, NOW + 1),
            Err(PeerRejection::MalformedId)
        );

        let mut unproven = envelope(PeerMessageKind::Artifact);
        unproven.artifacts.push(PeerArtifactRef {
            artifact_id: "art-1".into(),
            sha256: "not-a-digest".into(),
            filename: None,
            media_type: None,
            size_bytes: None,
        });
        assert_eq!(
            unproven.validate(LOCAL, NOW + 1),
            Err(PeerRejection::MalformedId)
        );
    }

    #[test]
    fn attaching_a_file_needs_the_artifact_grant_on_top() {
        let plain = envelope(PeerMessageKind::Message);
        assert_eq!(
            plain.required_capabilities(),
            BTreeSet::from([PeerCapability::Message])
        );

        let mut with_file = plain.clone();
        with_file.artifacts.push(PeerArtifactRef {
            artifact_id: "art-1".into(),
            sha256: "c".repeat(64),
            filename: None,
            media_type: None,
            size_bytes: None,
        });
        assert_eq!(
            with_file.required_capabilities(),
            BTreeSet::from([PeerCapability::Message, PeerCapability::Artifact])
        );

        assert_eq!(
            envelope(PeerMessageKind::TaskRequest).required_capabilities(),
            BTreeSet::from([PeerCapability::TaskRequest])
        );
    }

    #[test]
    fn the_origin_chain_has_to_describe_one_route() {
        // A chain that does not start with the author is a chain that has been
        // edited; there is no legitimate way to produce one.
        let mut disowned = envelope(PeerMessageKind::Message);
        disowned.origin_chain = vec!["instance-someone-else".into()];
        assert_eq!(
            disowned.validate(LOCAL, NOW + 1),
            Err(PeerRejection::InvalidOriginChain)
        );

        let mut empty = envelope(PeerMessageKind::Message);
        empty.origin_chain.clear();
        assert_eq!(
            empty.validate(LOCAL, NOW + 1),
            Err(PeerRejection::InvalidOriginChain)
        );

        // A repeated hop would let a forwarder hide a loop inside the chain.
        let mut doubled = envelope(PeerMessageKind::Message);
        doubled.origin_chain.push("instance-middle".into());
        doubled.origin_chain.push("instance-middle".into());
        assert_eq!(
            doubled.validate(LOCAL, NOW + 1),
            Err(PeerRejection::InvalidOriginChain)
        );

        // And forwarding refuses rather than producing one of the above.
        let received = envelope(PeerMessageKind::Message);
        assert_eq!(
            received.forwarded_from("instance-remote"),
            Err(PeerRejection::OriginLoop)
        );
    }

    #[test]
    fn timestamps_have_to_make_sense_before_anything_else_is_believed() {
        let mut backwards = envelope(PeerMessageKind::Message);
        backwards.expires_at_ms = backwards.created_at_ms;
        assert_eq!(
            backwards.validate(LOCAL, NOW - 1),
            Err(PeerRejection::InvalidTimestamp)
        );

        let mut unborn = envelope(PeerMessageKind::Message);
        unborn.created_at_ms = 0;
        assert_eq!(
            unborn.validate(LOCAL, NOW + 1),
            Err(PeerRejection::InvalidTimestamp)
        );

        // Dating an envelope into the future would otherwise buy a sender an
        // arbitrarily long life for it.
        let mut ahead = envelope(PeerMessageKind::Message);
        ahead.created_at_ms = NOW + MAX_FUTURE_SKEW_MS + 1;
        ahead.expires_at_ms = ahead.created_at_ms + 60_000;
        assert_eq!(
            ahead.validate(LOCAL, NOW),
            Err(PeerRejection::CreatedInFuture)
        );

        // Ordinary clock skew inside the allowance is fine.
        let mut skewed = envelope(PeerMessageKind::Message);
        skewed.created_at_ms = NOW + MAX_FUTURE_SKEW_MS - 1;
        skewed.expires_at_ms = skewed.created_at_ms + 60_000;
        assert_eq!(skewed.validate(LOCAL, NOW), Ok(()));
    }

    #[test]
    fn artifact_metadata_is_display_data_and_is_bounded_like_it() {
        let with_ref = |filename: Option<&str>, media_type: Option<&str>, size: Option<u64>| {
            let mut envelope = envelope(PeerMessageKind::Artifact);
            envelope.artifacts.push(PeerArtifactRef {
                artifact_id: "art-1".into(),
                sha256: "d".repeat(64),
                filename: filename.map(str::to_string),
                media_type: media_type.map(str::to_string),
                size_bytes: size,
            });
            envelope.validate(LOCAL, NOW + 1)
        };

        // A filename is a label, never a path — and never a way to climb out.
        assert_eq!(
            with_ref(Some("../../etc/passwd"), None, None),
            Err(PeerRejection::ArtifactMetadataTooLarge)
        );
        assert_eq!(
            with_ref(Some(".."), None, None),
            Err(PeerRejection::ArtifactMetadataTooLarge)
        );
        assert_eq!(
            with_ref(Some("build\nlog.txt"), None, None),
            Err(PeerRejection::ArtifactMetadataTooLarge)
        );
        assert_eq!(
            with_ref(
                Some(&"x".repeat(MAX_ARTIFACT_FILENAME_BYTES + 1)),
                None,
                None
            ),
            Err(PeerRejection::ArtifactMetadataTooLarge)
        );
        assert_eq!(
            with_ref(
                None,
                Some(&"t".repeat(MAX_ARTIFACT_MEDIA_TYPE_BYTES + 1)),
                None
            ),
            Err(PeerRejection::ArtifactMetadataTooLarge)
        );
        assert_eq!(
            with_ref(None, None, Some(MAX_PEER_ARTIFACT_BYTES + 1)),
            Err(PeerRejection::ArtifactTooLarge)
        );
        assert_eq!(
            with_ref(Some("build.log"), Some("text/plain"), Some(2_048)),
            Ok(())
        );
    }

    #[test]
    fn a_sender_can_validate_everything_except_the_receivers_own_loop_rule() {
        // Its own id in the chain is normal for a sender and a loop for a
        // receiver, which is exactly why the two entry points differ.
        let outgoing = envelope(PeerMessageKind::Message);
        assert_eq!(outgoing.validate_for_send(NOW + 1), Ok(()));
        assert_eq!(
            outgoing.validate_for_receive("instance-remote", NOW + 1),
            Err(PeerRejection::OriginLoop)
        );
        assert_eq!(outgoing.validate_for_receive(LOCAL, NOW + 1), Ok(()));

        // Everything else is refused on both sides alike.
        let mut expired = outgoing.clone();
        expired.expires_at_ms = NOW + 1;
        assert_eq!(
            expired.validate_for_send(NOW + 2),
            Err(PeerRejection::Expired)
        );
    }

    #[test]
    fn every_rejection_has_a_distinct_token_and_a_sentence() {
        let all = [
            PeerRejection::UnsupportedVersion,
            PeerRejection::MalformedId,
            PeerRejection::BodyTooLarge,
            PeerRejection::TooManyArtifacts,
            PeerRejection::MissingBody,
            PeerRejection::HopLimitExceeded,
            PeerRejection::ZeroHops,
            PeerRejection::OriginLoop,
            PeerRejection::InvalidOriginChain,
            PeerRejection::OriginChainTooLong,
            PeerRejection::InvalidTimestamp,
            PeerRejection::CreatedInFuture,
            PeerRejection::Expired,
            PeerRejection::ExpiryTooFar,
            PeerRejection::ArtifactMetadataTooLarge,
            PeerRejection::ArtifactTooLarge,
            PeerRejection::ArtifactUnavailable,
            PeerRejection::Duplicate,
            PeerRejection::MissingCapability,
            PeerRejection::PeerRevoked,
            PeerRejection::LocalUnavailable,
        ];
        let tokens: BTreeSet<&str> = all.iter().map(|reason| reason.as_str()).collect();
        assert_eq!(tokens.len(), all.len(), "two rejections share a token");
        for reason in all {
            // The token is what a store column and an audit row hold; the
            // sentence is what a peer is told. Neither may be empty.
            assert!(!reason.as_str().is_empty());
            assert!(!reason.message().is_empty());
        }
    }

    #[test]
    fn dedupe_is_per_sender() {
        let mine = envelope(PeerMessageKind::Message);
        let mut theirs = mine.clone();
        theirs.sender_instance_id = "instance-other".into();

        assert_eq!(mine.dedupe_key(), "instance-remote:msg-1");
        assert_ne!(mine.dedupe_key(), theirs.dedupe_key());
    }

    #[test]
    fn capability_tokens_round_trip() {
        for capability in [
            PeerCapability::Message,
            PeerCapability::TaskRequest,
            PeerCapability::Artifact,
        ] {
            assert_eq!(PeerCapability::parse(capability.as_str()), Some(capability));
        }
        assert_eq!(PeerCapability::parse("peer_admin"), None);
    }

    #[test]
    fn kinds_round_trip_and_only_a_task_expects_a_result() {
        for kind in [
            PeerMessageKind::Message,
            PeerMessageKind::TaskRequest,
            PeerMessageKind::Artifact,
        ] {
            assert_eq!(PeerMessageKind::parse(kind.as_str()), Some(kind));
        }
        assert!(PeerMessageKind::TaskRequest.expects_result());
        assert!(!PeerMessageKind::Message.expects_result());
        assert!(!PeerMessageKind::Artifact.expects_result());
    }

    #[test]
    fn an_envelope_survives_a_json_round_trip() {
        let sent = envelope(PeerMessageKind::TaskRequest);
        let json = serde_json::to_string(&sent).expect("serialize");
        let received: PeerEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(received, sent);

        // A future version is refused rather than read with today's meanings.
        let mut newer = sent.clone();
        newer.version = PEER_ENVELOPE_VERSION + 1;
        assert_eq!(
            newer.validate(LOCAL, NOW + 1),
            Err(PeerRejection::UnsupportedVersion)
        );
    }
}
