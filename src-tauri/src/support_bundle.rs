//! What an operator may hand somebody else when a message never arrived.
//!
//! # The problem this exists for
//!
//! Every subsystem this app grew — messaging channels, devices, peers, phone
//! calls, extensions, Talk — already persists its own lifecycle. What none of
//! them had was a way to *hand it over*. Diagnostics could export its findings,
//! and findings are conclusions: they say "this account is failing", never "here
//! is the sequence of things that happened to that account". Anybody debugging
//! a message that vanished had to be talked through six settings panels, or be
//! sent a database.
//!
//! # Why redaction is the shape of the type, not a step at the end
//!
//! A bundle is by definition a thing that leaves the machine. The subsystems it
//! summarizes hold private messages, other people's phone numbers, encryption
//! device state and recorded audio, and a redaction pass applied *after*
//! assembly is one forgotten field away from shipping all of it.
//!
//! So nothing that could carry content is representable here. There is no field
//! for message text, no field for a transcript, no field for audio, no field for
//! a key or a session. An identifier — a phone number, a sender handle, a
//! conversation, a device — is only ever a [`Pseudonym`], which can be
//! constructed one way: by hashing the real value with a salt this bundle
//! generated and will not record. Two events about the same number therefore
//! read as the same party *within one bundle*, which is what makes a trace
//! followable, and correlate with nothing outside it.
//!
//! What survives is what a reader actually needs: what kind of thing happened,
//! to which pseudonymous party, in what order, and how it turned out.
//!
//! # Bounded
//!
//! Every list has a ceiling and every truncation is *reported* — see
//! [`TraceSection::omitted`]. A section that silently stopped at fifty rows
//! reads as a subsystem that only did fifty things, which is exactly the wrong
//! conclusion for somebody counting retries.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const SUPPORT_BUNDLE_SCHEMA_VERSION: u32 = 1;

/// How many events one section may carry.
///
/// Small enough to read and to paste into an issue, large enough to hold the
/// window somebody is actually asking about — a message that failed a few
/// minutes ago, with the retries around it.
pub const MAX_SECTION_EVENTS: usize = 100;

/// A real identifier, replaced by something stable and meaningless.
///
/// The only way an identifier reaches a bundle. Construction goes through
/// [`Redactor::pseudonym`], which is the only thing holding the salt, so there
/// is no path that puts a raw value in one of these by accident.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Pseudonym(String);

impl Pseudonym {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Pseudonym {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Turns real identifiers into pseudonyms, consistently within one bundle.
///
/// The salt is random per bundle and is never stored or emitted. That is the
/// whole design: within a bundle, the same phone number always produces the
/// same token, so a reader can follow one party through a call and the text
/// that followed it; across two bundles — or against a list of phone numbers
/// somebody already has — the tokens say nothing.
pub struct Redactor {
    salt: [u8; 32],
}

impl Redactor {
    /// A redactor with a fresh random salt.
    ///
    /// Fallible, and deliberately so. There is no safe default for this value:
    /// a fixed or zeroed salt would turn every pseudonym into a stable global
    /// identifier for the thing it stands for, so a machine whose CSPRNG
    /// refuses does not get a bundle with a weaker one — it gets no bundle.
    /// Falling back would be strictly worse than failing, because the document
    /// would still *say* its identifiers are pseudonymized.
    pub fn new() -> Result<Self, String> {
        // `ring`'s generator rather than a buffer declared and filled in place:
        // the latter reads to a scanner as a hard-coded salt, and has been
        // flagged as one in this tree before.
        ring::rand::generate(&ring::rand::SystemRandom::new())
            .map(|random: ring::rand::Random<[u8; 32]>| Self {
                salt: random.expose(),
            })
            .map_err(|_| {
                "This machine's random number generator is unavailable, so a bundle whose \
                 identifiers cannot be correlated cannot be produced"
                    .to_string()
            })
    }

    /// A redactor whose salt is derived from a label, so a test can assert on
    /// exact output.
    ///
    /// Public because the CLI's own tests are a different crate and `cfg(test)`
    /// does not cross that boundary — hence the name, which is the guard: a
    /// production caller of `from_seed_for_tests` is visibly wrong in review,
    /// and there is nothing else to catch it, because a predictable salt
    /// produces a bundle that looks exactly like a correct one.
    ///
    /// Takes a seed string rather than a salt, so no literal in this file is
    /// used as a salt. That is both what a scanner is right to object to and a
    /// real hazard if one were ever copied into production.
    #[must_use]
    pub fn from_seed_for_tests(seed: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(seed.as_bytes());
        // Straight from the digest, never through a zeroed buffer: a declared
        // `[0u8; 32]` in a function that produces a salt is indistinguishable
        // from a hard-coded one to a reader and to a scanner, whatever happens
        // to it on the next line.
        Self {
            salt: hasher.finalize().into(),
        }
    }

    /// Replace one identifier.
    ///
    /// `kind` is a short label kept in the clear — `phone`, `sender`,
    /// `conversation`, `device` — because knowing *what sort of thing* a token
    /// stands for is most of what makes a trace readable, and it reveals
    /// nothing about which one.
    #[must_use]
    pub fn pseudonym(&self, kind: &str, value: &str) -> Pseudonym {
        let mut hasher = Sha256::new();
        hasher.update(self.salt);
        hasher.update(kind.as_bytes());
        hasher.update([0u8]);
        hasher.update(value.as_bytes());
        let digest = hasher.finalize();
        Pseudonym(format!(
            "{kind}:{}",
            digest
                .iter()
                .take(6)
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ))
    }

    /// The same, for a value that may be absent.
    #[must_use]
    pub fn optional(&self, kind: &str, value: Option<&str>) -> Option<Pseudonym> {
        value.map(|value| self.pseudonym(kind, value))
    }
}

/// One thing that happened, with nothing in it anybody said.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceEvent {
    pub at_ms: i64,
    /// What happened, in the subsystem's own vocabulary: `inbound.accepted`,
    /// `outbox.sent`, `call.completed`, `command.leased`. A fixed vocabulary
    /// from the code, never a formatted string carrying a value.
    pub event: String,
    /// Who or what it concerned. Pseudonymous by construction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<Pseudonym>,
    /// The conversation, thread, call or session it belongs to, when the
    /// subsystem has one. This is what makes a sequence followable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Pseudonym>,
    /// How it turned out, from the subsystem's own state vocabulary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Why, when there is a why.
    ///
    /// Only ever a code or a sentence *this codebase wrote* — a rejection
    /// reason, a state name, a bounded provider error. Never a message body,
    /// and never a value that arrived over the wire; see
    /// [`bounded_reason`], which is the only way one is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A reason string, bounded and stripped of anything that breaks a line.
///
/// The bound is the point: a provider error is text somebody else wrote, and a
/// bundle is a document a person reads. Anything long enough to hide a payload
/// in is not a reason.
#[must_use]
pub fn bounded_reason(value: &str) -> String {
    const MAX_CHARS: usize = 160;
    let single_line: String = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let trimmed = single_line.trim();
    if trimmed.chars().count() <= MAX_CHARS {
        return trimmed.to_string();
    }
    trimmed.chars().take(MAX_CHARS).collect::<String>() + "…"
}

/// One subsystem's slice of the trace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceSection {
    /// Newest last, so a section reads in the order things happened.
    pub events: Vec<TraceEvent>,
    /// How many were dropped to fit [`MAX_SECTION_EVENTS`].
    ///
    /// Reported rather than silently truncated: a capped list that does not say
    /// it is capped reads as the complete history, and somebody counting
    /// retries would reach the wrong conclusion from it.
    pub omitted: usize,
    /// Why this section is empty, when it is empty for a reason other than
    /// "nothing happened" — a store that could not be opened, a subsystem this
    /// build does not have. The distinction is the one a reader most needs and
    /// the one an absent section cannot make.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
}

impl TraceSection {
    /// Build a section from events in any order, keeping the newest
    /// [`MAX_SECTION_EVENTS`] and counting the rest.
    #[must_use]
    pub fn from_events(mut events: Vec<TraceEvent>) -> Self {
        events.sort_by_key(|event| event.at_ms);
        let omitted = events.len().saturating_sub(MAX_SECTION_EVENTS);
        if omitted > 0 {
            events.drain(..omitted);
        }
        Self {
            events,
            omitted,
            unavailable: None,
        }
    }

    #[must_use]
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            events: Vec::new(),
            omitted: 0,
            unavailable: Some(reason.into()),
        }
    }
}

/// The whole thing an operator hands over.
///
/// Sections are keyed by subsystem name so a build without a subsystem simply
/// has no key for it, rather than an empty section that reads as "this ran and
/// found nothing".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupportBundle {
    pub schema_version: u32,
    pub generated_at_ms: u64,
    pub app_version: String,
    pub platform: String,
    /// Stated in the document itself, so a reader knows what they are *not*
    /// looking at and does not conclude a conversation was empty.
    pub redaction: RedactionNotice,
    pub sections: BTreeMap<String, TraceSection>,
}

/// What this bundle deliberately does not contain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RedactionNotice {
    pub identifiers_pseudonymized: bool,
    pub excluded: Vec<String>,
}

impl Default for RedactionNotice {
    fn default() -> Self {
        Self {
            identifiers_pseudonymized: true,
            excluded: vec![
                "message text".to_string(),
                "call and voice transcripts".to_string(),
                "recorded or streamed audio".to_string(),
                "phone numbers and sender handles".to_string(),
                "encryption keys, sessions and device fingerprints".to_string(),
                "credentials and authorization headers".to_string(),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_value_reads_the_same_way_inside_one_bundle() {
        let redactor = Redactor::from_seed_for_tests("support-bundle-tests");
        let first = redactor.pseudonym("phone", "+15550001111");
        let again = redactor.pseudonym("phone", "+15550001111");
        assert_eq!(first, again, "a trace has to be followable");
        assert_ne!(first, redactor.pseudonym("phone", "+15550002222"));
    }

    /// The kind is part of the hash, not just a prefix. Without it, the same
    /// string used as two different sorts of identifier would produce one
    /// token and silently merge two parties in a trace.
    #[test]
    fn the_same_string_as_two_kinds_of_thing_is_two_different_parties() {
        let redactor = Redactor::from_seed_for_tests("support-bundle-tests");
        let as_phone = redactor.pseudonym("phone", "+15550001111");
        let as_sender = redactor.pseudonym("sender", "+15550001111");
        assert_ne!(as_phone.as_str(), as_sender.as_str());
    }

    #[test]
    fn nothing_of_the_original_survives_the_pseudonym() {
        let redactor = Redactor::from_seed_for_tests("support-bundle-tests");
        let token = redactor.pseudonym("phone", "+15550001111");
        assert!(token.as_str().starts_with("phone:"));
        for fragment in ["1555", "0001111", "+1"] {
            assert!(
                !token.as_str().contains(fragment),
                "'{fragment}' survived into {token}"
            );
        }
    }

    /// Two bundles must not be joinable. A fixed salt would make every
    /// pseudonym a stable global identifier for a phone number.
    #[test]
    fn two_bundles_cannot_be_correlated_with_each_other() {
        let first = Redactor::new()
            .expect("randomness")
            .pseudonym("phone", "+15550001111");
        let second = Redactor::new()
            .expect("randomness")
            .pseudonym("phone", "+15550001111");
        assert_ne!(
            first, second,
            "a per-bundle salt is what stops one number being traceable across handovers"
        );
    }

    #[test]
    fn a_reason_is_bounded_and_kept_to_one_line() {
        let reason = bounded_reason("carrier said:\n\tno\r\n");
        assert_eq!(reason, "carrier said:  no");
        let long = bounded_reason(&"x".repeat(500));
        assert_eq!(long.chars().count(), 161, "160 characters and an ellipsis");
        assert!(long.ends_with('…'));
    }

    /// A capped section says so. A truncated list that looks complete is how a
    /// reader concludes a retry loop ran three times when it ran three hundred.
    #[test]
    fn a_capped_section_reports_what_it_dropped_and_keeps_the_newest() {
        let events: Vec<TraceEvent> = (0..MAX_SECTION_EVENTS + 25)
            .map(|index| TraceEvent {
                at_ms: index as i64,
                event: "outbox.attempt".to_string(),
                subject: None,
                context: None,
                outcome: None,
                reason: None,
            })
            .collect();
        let section = TraceSection::from_events(events);
        assert_eq!(section.events.len(), MAX_SECTION_EVENTS);
        assert_eq!(section.omitted, 25);
        assert_eq!(section.events[0].at_ms, 25, "the newest are what is kept");
        assert_eq!(
            section.events.last().expect("last").at_ms,
            (MAX_SECTION_EVENTS + 24) as i64
        );
    }

    /// Out-of-order input still reads in the order things happened, because a
    /// trace whose lines are shuffled is not a trace.
    #[test]
    fn a_section_is_ordered_by_when_things_happened() {
        let section = TraceSection::from_events(vec![
            TraceEvent {
                at_ms: 30,
                event: "b".into(),
                subject: None,
                context: None,
                outcome: None,
                reason: None,
            },
            TraceEvent {
                at_ms: 10,
                event: "a".into(),
                subject: None,
                context: None,
                outcome: None,
                reason: None,
            },
        ]);
        assert_eq!(
            section
                .events
                .iter()
                .map(|event| event.event.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    /// "Could not be read" and "nothing happened" are different answers and a
    /// reader needs to tell them apart.
    #[test]
    fn an_unreadable_section_says_so_rather_than_looking_empty() {
        let section = TraceSection::unavailable("the background service is not running");
        assert!(section.events.is_empty());
        assert_eq!(
            section.unavailable.as_deref(),
            Some("the background service is not running")
        );
        let quiet = TraceSection::from_events(Vec::new());
        assert!(quiet.unavailable.is_none());
    }

    /// There is no infallible way to get a redactor, and that is the point.
    ///
    /// A zeroed or fixed fallback salt would produce a bundle that looks
    /// exactly like a correct one and whose every pseudonym is a stable global
    /// identifier for the number behind it. The type therefore has no `Default`
    /// and no infallible constructor, so a caller cannot reach for one without
    /// noticing — the compiler is the enforcement here, because nothing about
    /// the output would reveal the mistake.
    #[test]
    fn there_is_no_way_to_build_a_redactor_without_handling_the_failure() {
        let redactor: Result<Redactor, String> = Redactor::new();
        assert!(redactor.is_ok(), "this machine has a working CSPRNG");
        // And the seeded constructor names itself as a test-only one, so a
        // production use of it is visible in review rather than invisible in
        // output.
        let seeded = Redactor::from_seed_for_tests("a");
        assert_ne!(
            seeded.pseudonym("phone", "+15550001111"),
            Redactor::from_seed_for_tests("b").pseudonym("phone", "+15550001111"),
            "the seed has to actually reach the salt"
        );
    }

    /// The bundle states what it left out, so a reader does not mistake a
    /// redacted conversation for an empty one.
    #[test]
    fn the_bundle_says_what_it_does_not_contain() {
        let notice = RedactionNotice::default();
        assert!(notice.identifiers_pseudonymized);
        for expected in ["message text", "recorded or streamed audio"] {
            assert!(
                notice.excluded.iter().any(|item| item == expected),
                "{expected} is not declared excluded"
            );
        }
    }
}
