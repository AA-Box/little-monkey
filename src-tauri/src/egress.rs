//! Shared primitives for deciding where this app may send a request.
//!
//! Deliberately small. There are already **four** independent SSRF guards in this
//! tree — `web.rs`, `knowledge_pipeline.rs`, `browser_worker.rs` and
//! `model_sources.rs` — each with its own blocklist, and unifying them is a
//! separate, riskier change: the broadest of the four blocks CGNAT (`100.64/10`),
//! which is Tailscale's default range and live on some consumer ISPs, so adopting
//! it everywhere would newly refuse fetches that work today.
//!
//! What lives here is the narrow subset where all four agreed *and were all
//! wrong the same way*, so a fix belongs in one place rather than four.
//!
//! [`hardened`] is the other half: not *where* a request may go, but what
//! defaults it carries once it goes there. It exists because a default
//! `reqwest::Client` — the spelling ~25 call sites in this tree reached for — has
//! no timeout of any kind and a redirect policy that will follow ten hops to
//! anywhere. See its doc comment for the three concrete holes that closes.
//!
//! [`EgressRule`] is the third piece, and the one the other two were missing: a
//! *name* for the rule that refused a request, so a refusal is a value and not
//! only a sentence. See its doc comment for what could not be done without it.
//!
//! [`send`] and [`metered`] are the fourth, and the only part of this module that
//! *measures* rather than decides: how many bytes a request actually moved, and
//! which process row they belong to. See [`send`] for the byte unit — it is not
//! the obvious one — and `Counted` for why counting a streamed body does not turn
//! it into a buffered one.
//!
//! [`check_run_allowlist`] and `PinnedResolver` are the fifth, and they are the one
//! part that is keyed to a **run** rather than to a URL or an address: the run's
//! frozen host/port/protocol allowlist, enforced where every outbound request
//! already converges, and its names pinned so an allowed one cannot be moved
//! mid-run. See [`check_run_allowlist`] for which absences fail open and which fail
//! closed, and `PinnedResolver` for what pinning costs a rotating CDN.

use std::collections::BTreeMap;
use std::error::Error as _;
use std::fmt;
use std::future::Future;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::body::{Body as HttpBody, Buf, Frame, SizeHint};
use reqwest::Url;

use crate::run_scope::{self, Unattributed};

/// Declares [`EgressRule`] together with its code and summary tables.
///
/// A macro rather than three hand-written lists because those three must not be
/// able to drift apart. Written out by hand, a new variant compiles fine while
/// missing from `ALL`, and `ALL` is what the tests below iterate to prove codes
/// are unique and stable — so the one mistake that matters most (a rule nobody
/// is checking) would be the one mistake the tests could not see. Declared this
/// way, a variant without a code is a compile error and membership in `ALL` is
/// not a thing anyone can forget.
macro_rules! egress_rules {
    ($(
        $(#[doc = $doc:literal])*
        $variant:ident => $code:literal, $summary:literal;
    )+) => {
        /// The rule that refused an outbound request.
        ///
        /// # Why a type, when a sentence was already there
        ///
        /// Every refusal in the four SSRF guards used to be prose built at the
        /// refusal site — `web.rs` returned `Result<(), String>`,
        /// `knowledge_pipeline.rs` wrapped a `String` in `UrlRejected`,
        /// `browser_worker.rs` and `model_sources.rs` returned `Err(String)`.
        /// Three consequences, each observed in this tree rather than imagined:
        ///
        /// 1. **A test could not tell a policy block from a typo.**
        ///    `knowledge_pipeline.rs` maps `Url::parse` failures and its loopback
        ///    block onto the same `UrlRejected` variant, and one of its tests
        ///    asserts five semantically different refusals — loopback, embedded
        ///    credentials, a `file://` scheme, an over-length URL and an `[::1]`
        ///    literal — with the identical `Err(UrlRejected(_))` pattern.
        /// 2. **Neither could an operator.** At the command boundary `web.rs`
        ///    hands the UI a flat `String` in which a policy denial, a DNS
        ///    outage and a TCP reset are the same shape, discriminated only by
        ///    substring-matching prose like `"local/private"`.
        /// 3. **One sentence stood for many rules.** That `"local/private"`
        ///    string is the verdict of ten distinct address predicates.
        ///    `browser_worker.rs` is worse than imprecise: its message names
        ///    four classes ("Private, link-local, multicast, and unspecified")
        ///    while the predicate behind it blocks eleven.
        ///
        /// # What this is not
        ///
        /// Not a unification of the four blocklists. They still disagree, on
        /// purpose and for the reason this module's own doc gives. This is shared
        /// *vocabulary*: where two guards refuse for the same reason they now say
        /// so with the same name, and where only one guard checks a class, that
        /// asymmetry becomes visible instead of hiding inside four different
        /// sentences.
        ///
        /// # Codes are permanent
        ///
        /// [`code`](Self::code) is the machine identity a denial sink will store,
        /// so it outlives the prose and outlives this enum's spelling. A test
        /// below pins every code against a written-out list; changing one has to
        /// be a deliberate edit in two places, because renaming a code orphans
        /// every denial already recorded under the old one.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum EgressRule {
            $($(#[doc = $doc])* $variant,)+
        }

        impl EgressRule {
            /// Every rule, in declaration order.
            ///
            /// Complete by construction — see [`egress_rules`]'s own doc for why
            /// that guarantee is worth a macro.
            pub const ALL: &'static [EgressRule] = &[$(EgressRule::$variant,)+];

            /// The stable machine identity, e.g. `egress.loopback`.
            ///
            /// Never localized, never reworded, and safe to persist, match on, or
            /// grep a log for.
            #[must_use]
            pub fn code(self) -> &'static str {
                match self { $(EgressRule::$variant => $code,)+ }
            }

            /// One short human sentence, with no interpolation.
            ///
            /// Anything request-specific — an address, an origin, a hop count —
            /// belongs in [`EgressDenial`]'s detail, so that the two can be
            /// stored and compared separately.
            #[must_use]
            pub fn summary(self) -> &'static str {
                match self { $(EgressRule::$variant => $summary,)+ }
            }
        }
    };
}

egress_rules! {
    /// The URL did not parse. Not a policy decision — it is here so that a
    /// malformed URL is *distinguishable* from one a rule refused, which is
    /// exactly what `knowledge_pipeline.rs` could not express when both were
    /// `UrlRejected(String)`.
    UrlMalformed => "egress.url-malformed", "the URL could not be parsed";
    /// Longer than the caller's configured maximum.
    UrlTooLong => "egress.url-too-long", "the URL is over its length limit";
    /// Contains control characters. Split from [`UrlTooLong`](EgressRule::UrlTooLong)
    /// even though `knowledge_pipeline.rs` tested both in one branch: a 40 KB URL
    /// and a URL carrying a `\r` are different problems and one of the two is an
    /// injection attempt.
    UrlControlCharacters => "egress.url-control-characters", "the URL contains control characters";
    /// The scheme is not one this caller will send to.
    SchemeNotAllowed => "egress.scheme-not-allowed", "the URL scheme is not allowed here";
    /// `user:password@host`. The only rule whose diagnostics must not name the
    /// target — see [`EgressRule::redacts_target`].
    EmbeddedCredentials => "egress.embedded-credentials", "the URL carries embedded credentials";
    /// A fragment was present where the URL is an identity, not a location.
    FragmentNotAllowed => "egress.fragment-not-allowed", "the URL carries a fragment";
    /// No host at all — `data:`, `file:`, or a relative URL that reached a guard.
    HostMissing => "egress.host-missing", "the URL has no host";
    /// No port, and the scheme has no default to fall back on.
    PortMissing => "egress.port-missing", "the URL has no port";
    /// The origin is outside the set this run or this source was granted.
    OriginNotAllowlisted => "egress.origin-not-allowlisted", "the origin is not on the allowlist";
    /// A redirect or document navigation left the granted origin set. Kept
    /// separate from [`SubresourceLeftGrant`](EgressRule::SubresourceLeftGrant)
    /// because `browser_worker.rs` deliberately distinguishes them and a test
    /// there exists only to hold that line.
    RedirectLeftGrant => "egress.redirect-left-grant", "a redirect left the granted origins";
    /// A page subresource pointed outside the granted origin set.
    SubresourceLeftGrant => "egress.subresource-left-grant", "a subresource left the granted origins";
    /// Plain `http` where TLS is required.
    CleartextNotAllowed => "egress.cleartext-not-allowed", "cleartext HTTP is not allowed here";
    /// `127.0.0.0/8`, `::1`, or a name that resolves there. This machine's own
    /// unauthenticated services live here, which is what makes it the class that
    /// matters most.
    Loopback => "egress.loopback", "the target is a loopback address";
    /// `0.0.0.0` / `::`. Blocked as loopback would be, because the OS routes an
    /// outbound connection to `0.0.0.0` to `127.0.0.1` — verified empirically on
    /// macOS — so it is a live path to a loopback-bound service and not a dead
    /// address.
    Unspecified => "egress.unspecified", "the target is the unspecified address";
    /// RFC 1918: `10/8`, `172.16/12`, `192.168/16`.
    PrivateV4 => "egress.private-v4", "the target is a private IPv4 address";
    /// `169.254/16` or `fe80::/10`. One rule for both families: same policy,
    /// same reason, and no guard distinguishes them.
    LinkLocal => "egress.link-local", "the target is a link-local address";
    /// `fc00::/7`. Its own rule rather than folded into
    /// [`PrivateV4`](EgressRule::PrivateV4) because there is no v4 analogue and a
    /// reader should not have to know that "private-v4" secretly meant v6 too.
    UniqueLocalV6 => "egress.unique-local-v6", "the target is a unique-local IPv6 address";
    /// `224/4` or `ff00::/8`.
    Multicast => "egress.multicast", "the target is a multicast address";
    /// `255.255.255.255`.
    Broadcast => "egress.broadcast", "the target is the broadcast address";
    /// Carrier-grade NAT, `100.64/10`. Only the broadest guard blocks this, and
    /// deliberately so: it is Tailscale's default range and live on some consumer
    /// ISPs, so blocking it everywhere would refuse fetches that work today.
    Cgnat => "egress.cgnat", "the target is a carrier-grade NAT address";
    /// `0.0.0.0/8` — "this network" — other than the unspecified address itself.
    ThisNetwork => "egress.this-network", "the target is in the this-network range";
    /// `192.0.0.0/24`, IETF protocol assignments.
    ProtocolAssignments => "egress.protocol-assignments", "the target is an IETF protocol assignment";
    /// Documentation ranges: `192.0.2/24`, `203.0.113/24`, and (in one guard, wider
    /// than the RFC's `198.51.100/24`) all of `198.51/16`.
    TestNet => "egress.test-net", "the target is in a documentation range";
    /// `198.18/15`, reserved for inter-network benchmarking.
    Benchmarking => "egress.benchmarking", "the target is in the benchmarking range";
    /// `240/4` and above, reserved.
    ReservedRange => "egress.reserved-range", "the target is in a reserved range";
    /// The deprecated IPv4-compatible form `::a.b.c.d`, which walked past all four
    /// guards until [`is_ipv4_compatible`] existed. Rejected as a range rather
    /// than unwrapped — that function's doc says why the unwrap is worse than the
    /// bug.
    Ipv4Compatible => "egress.ipv4-compatible", "the target uses the deprecated IPv4-compatible form";
    /// The name could not be resolved at all. Not a policy decision, and named so
    /// that "the guard blocked it" and "DNS is down" stop being one string.
    DnsResolutionFailed => "egress.dns-resolution-failed", "the host could not be resolved";
    /// The caller must supply the resolved addresses and did not. An interface
    /// contract, not a resolution outcome: `knowledge_pipeline.rs` refuses to
    /// accept bytes for a name whose answers it was never shown.
    DnsAnswersRequired => "egress.dns-answers-required", "the caller supplied no resolved addresses";
    /// Resolution succeeded and returned nothing usable.
    DnsNoAddresses => "egress.dns-no-addresses", "the host resolved to no addresses";
    /// More hops than the caller's cap.
    RedirectHopLimit => "egress.redirect-hop-limit", "the redirect chain is over its hop limit";
    /// A hop changed origin while the request carried a credential. `x-api-key`
    /// is the specific reason: reqwest strips `Authorization` across hosts but
    /// not that one.
    RedirectCrossOrigin => "egress.redirect-cross-origin", "the redirect leaves the origin the request was aimed at";
    /// A hop arrived with no recorded previous URL, so there was no origin to
    /// compare against. Refused rather than assumed, because "follow it anyway"
    /// is the wrong direction to fail in for a client holding an API key.
    RedirectOriginUnknown => "egress.redirect-origin-unknown", "the redirect has no recorded origin to compare against";
    /// The run's own frozen `permission_policy.allow_network` is `false` and the
    /// target is not on this machine. The first rule in this list keyed to a run
    /// rather than to an address: it refuses a destination that every other rule
    /// here would happily allow, because *this run* said it would not use the
    /// network.
    RunNetworkDenied => "egress.run-network-denied", "this run was submitted without network permission";
    /// The host is not on the run's frozen
    /// [`EgressAllowlist`](crate::run_protocol::EgressAllowlist). Deny-by-default
    /// *within* a declaration: a host the spec did not name is refused even though
    /// every address rule above would have allowed it.
    RunHostNotAllowlisted => "egress.run-host-not-allowlisted", "the host is not on this run's egress allowlist";
    /// The effective port — the URL's own, or the scheme's default — is not on the
    /// run's frozen allowlist.
    RunPortNotAllowlisted => "egress.run-port-not-allowlisted", "the port is not on this run's egress allowlist";
    /// The URL scheme is not on the run's frozen allowlist. Distinct from
    /// [`SchemeNotAllowed`](EgressRule::SchemeNotAllowed), which is a *caller's*
    /// fixed set (`web.rs` sends only `http`/`https`); this one is the run's.
    RunProtocolNotAllowlisted => "egress.run-protocol-not-allowlisted", "the protocol is not on this run's egress allowlist";
    /// A run is in scope and its frozen policy could not be read, so whether it
    /// declared an allowlist is unknown. Refused rather than assumed — see
    /// [`run_allowlist_verdict`] for why this one fails closed while an absent
    /// declaration fails open.
    RunPolicyUnavailable => "egress.run-policy-unavailable", "this run's frozen egress policy could not be read";
}

impl EgressRule {
    /// Whether a diagnostic for this rule must **not** name the target URL.
    ///
    /// True for exactly one rule, and the asymmetry is the point:
    /// [`EmbeddedCredentials`](Self::EmbeddedCredentials) fires *because* the URL
    /// contains a secret, so the message that reports it is the one message that
    /// cannot quote it. `web.rs` already had this right by hand — its
    /// credentials refusal is the only one of its seven that omits the URL — and
    /// putting it on the rule is what stops the next guard from getting it wrong.
    /// The same instinct as [`origin_label`], one step further: there, the URL is
    /// reduced to an origin; here, it is dropped.
    #[must_use]
    pub fn redacts_target(self) -> bool {
        matches!(self, EgressRule::EmbeddedCredentials)
    }

    /// Whether a user's opt-in to private networks is meant to cover this rule.
    ///
    /// A setting named for private networks was one boolean over every rule an
    /// address classifier can return, so switching it on to reach a NAS at
    /// `192.168.1.10` also permitted multicast, `255.255.255.255`, the
    /// documentation ranges, `240/4` and the deprecated IPv4-compatible form. None
    /// of those is a thing anybody enables that setting to reach, and the last is
    /// not a destination class at all — it is an alternative *spelling* of one, so
    /// blanketing it turned a private-network grant into a way to launder any
    /// address past the guard.
    ///
    /// The split is "could a host the user actually runs answer here?":
    ///
    /// - **Covered.** Loopback, RFC 1918, link-local, unique-local IPv6, and CGNAT
    ///   — that last because `100.64/10` is Tailscale's default range and live on
    ///   some consumer ISPs, so it is where a real peer lives rather than a
    ///   curiosity. [`Unspecified`](Self::Unspecified) joins them because an
    ///   outbound connection to `0.0.0.0` is routed to `127.0.0.1`: it reaches the
    ///   *same* service the loopback grant already covers, so refusing it while
    ///   permitting loopback would be inconsistent about one destination rather
    ///   than protective of anything.
    /// - **Not covered.** Multicast and broadcast, which HTTP does not use;
    ///   `0/8` past `0.0.0.0`, `192.0.0/24`, the documentation and benchmarking
    ///   ranges and `240/4`, which route nowhere; and the IPv4-compatible form,
    ///   which is a spelling and not a place.
    ///
    /// Written as an exhaustive `match` rather than a `matches!` over the covered
    /// set, so that adding a rule to [`EgressRule`] is a compile error here until
    /// somebody decides which side of this line it falls on. Every rule that is not
    /// an address class answers `false`, which is both correct and moot: this is
    /// only ever consulted with a classifier's verdict.
    #[must_use]
    pub fn covered_by_private_network_grant(self) -> bool {
        match self {
            EgressRule::Loopback
            | EgressRule::Unspecified
            | EgressRule::PrivateV4
            | EgressRule::LinkLocal
            | EgressRule::UniqueLocalV6
            | EgressRule::Cgnat => true,
            EgressRule::Multicast
            | EgressRule::Broadcast
            | EgressRule::ThisNetwork
            | EgressRule::ProtocolAssignments
            | EgressRule::TestNet
            | EgressRule::Benchmarking
            | EgressRule::ReservedRange
            | EgressRule::Ipv4Compatible => false,
            // Not address classes, so no address grant reaches them.
            EgressRule::UrlMalformed
            | EgressRule::UrlTooLong
            | EgressRule::UrlControlCharacters
            | EgressRule::SchemeNotAllowed
            | EgressRule::EmbeddedCredentials
            | EgressRule::FragmentNotAllowed
            | EgressRule::HostMissing
            | EgressRule::PortMissing
            | EgressRule::OriginNotAllowlisted
            | EgressRule::RedirectLeftGrant
            | EgressRule::SubresourceLeftGrant
            | EgressRule::CleartextNotAllowed
            | EgressRule::DnsResolutionFailed
            | EgressRule::DnsAnswersRequired
            | EgressRule::DnsNoAddresses
            | EgressRule::RedirectHopLimit
            | EgressRule::RedirectCrossOrigin
            | EgressRule::RedirectOriginUnknown
            | EgressRule::RunNetworkDenied
            | EgressRule::RunHostNotAllowlisted
            | EgressRule::RunPortNotAllowlisted
            | EgressRule::RunProtocolNotAllowlisted
            | EgressRule::RunPolicyUnavailable => false,
        }
    }
}

/// A refusal: the rule that fired, plus optional request-specific detail.
///
/// Implements [`std::error::Error`], which is not decoration. Two of the places
/// a guard has to hand its verdict to somebody else's signature —
/// `reqwest::dns::Resolve` (`Box<dyn Error + Send + Sync>`) and
/// `redirect::Attempt::error` (via `io::Error::new`) — accept any such error, so
/// a denial can travel through reqwest as itself rather than as a flattened
/// string, and be recovered later with `downcast_ref`. Before this, the only
/// machine-readable signal on that path was an `io::ErrorKind::PermissionDenied`
/// that the caller's `format!` promptly destroyed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressDenial {
    rule: EgressRule,
    detail: Option<String>,
}

impl EgressDenial {
    /// A denial with no detail beyond the rule.
    #[must_use]
    pub fn new(rule: EgressRule) -> Self {
        Self { rule, detail: None }
    }

    /// A denial carrying the specifics — the address that tripped it, the origin
    /// that was not allowlisted, the hop count.
    ///
    /// Detail is prose for a human. It must never be the only place the *reason*
    /// lives: anything a test or a sink needs to branch on belongs in the rule.
    #[must_use]
    pub fn about(rule: EgressRule, detail: impl Into<String>) -> Self {
        Self {
            rule,
            detail: Some(detail.into()),
        }
    }

    #[must_use]
    pub const fn rule(&self) -> EgressRule {
        self.rule
    }

    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

impl From<EgressRule> for EgressDenial {
    fn from(rule: EgressRule) -> Self {
        Self::new(rule)
    }
}

/// The one canonical rendering, so no surface can drop the code.
///
/// `EgressRule` itself deliberately has no `Display`: with two of them, half the
/// call sites would print prose with no code and there would be no way to notice.
impl fmt::Display for EgressDenial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.detail {
            Some(detail) => write!(
                formatter,
                "{}: {detail} [{}]",
                self.rule.summary(),
                self.rule.code()
            ),
            None => write!(formatter, "{} [{}]", self.rule.summary(), self.rule.code()),
        }
    }
}

impl std::error::Error for EgressDenial {}

/// Whether `address` is an IPv4-**compatible** address (`::/96`), the deprecated
/// form `::a.b.c.d` — as distinct from the IPv4-**mapped** form `::ffff:a.b.c.d`.
///
/// # Why this is its own predicate
///
/// All four SSRF guards unwrapped v4-in-v6 with `to_ipv4_mapped()`, which by
/// design matches only `::ffff:0:0/96`. So `http://[::127.0.0.1]/` fell through
/// every branch of every guard: it is not `::1`, not unspecified, not `fc00::/7`,
/// not `fe80::/10`, and `to_ipv4_mapped()` returns `None` for it — leaving it
/// classified as an ordinary public address.
///
/// # Why the obvious one-word fix is worse than the bug
///
/// Swapping `to_ipv4_mapped()` for `to_ipv4()` looks like it closes this, because
/// `to_ipv4()` matches both forms. It does not: `to_ipv4()` maps `::1` to
/// `0.0.0.1`, which is not loopback, not private, not link-local and not
/// unspecified, so it passes every IPv4 predicate. In the two guards where the
/// unwrap branch returns early, that would make **loopback allowed**. This is
/// therefore a rejection of the whole range rather than an unwrap.
///
/// Rejecting `::/96` outright rather than unwrapping and re-checking is safe:
/// RFC 4291 deprecated the format, no modern deployment needs it, and nothing
/// legitimate asks this app to fetch `http://[::10.0.0.1]/`. Treating the range as
/// non-public also means a host that resolves *into* it cannot be used to smuggle
/// any address past a guard, including addresses a v4 blocklist would have
/// permitted.
///
/// `::` and `::1` are excluded because every caller already classifies them —
/// unspecified and loopback respectively — and folding them in here would hide
/// which rule actually fired.
#[must_use]
pub fn is_ipv4_compatible(address: &Ipv6Addr) -> bool {
    let segments = address.segments();
    // The top 96 bits are zero, and the low 32 are not a bare `::`/`::1`.
    segments[0..6].iter().all(|segment| *segment == 0)
        && !address.is_unspecified()
        && !address.is_loopback()
}

/// The IPv4 address a NAT64 prefix (`64:ff9b::/96`) embeds, if `address` is one.
///
/// # Why this is here and not in one guard
///
/// Sibling of [`is_ipv4_compatible`], found the same way and belonging here for the
/// same reason this module's doc gives: the narrow subset where the guards were all
/// wrong the *same* way. `64:ff9b::7f00:1` **is** `127.0.0.1` on any host with a
/// NAT64/CLAT path — which is every modern iOS device and a growing share of mobile
/// networks — and three of the four guards classified it as an ordinary public
/// address. `browser_worker.rs` is the exception, and only since its `2000::/3`
/// allowlist tail landed.
///
/// So this is another *spelling* of the addresses those guards already refuse, not a
/// new class. That distinction is what keeps this inside the shared subset rather
/// than making it the wholesale blocklist unification this module deliberately does
/// not do: nothing here decides whether `127.0.0.1` is refused, only that
/// `64:ff9b::7f00:1` is the same place.
///
/// # Why an unwrap where `::/96` is a rejection
///
/// [`is_ipv4_compatible`] rejects its whole range because RFC 4291 deprecated the
/// format and nothing legitimate uses it. NAT64 is the opposite: RFC 6052 defines it,
/// it is live, and `64:ff9b::` plus a *public* v4 address is a perfectly ordinary way
/// to reach a v4-only server from a v6-only network. Refusing the range outright
/// would break that. So each caller unwraps and re-checks against its **own** v4
/// blocklist, which preserves the divergence the four guards keep on purpose — a
/// guard that permits CGNAT keeps permitting `64:ff9b::64.64.0.1`.
///
/// Only the well-known prefix is recognised. RFC 6052 also allows network-specific
/// prefixes of several lengths, which cannot be detected from an address alone —
/// discovering them needs RFC 7050's DNS lookup, and a guard that consulted the
/// network to decide policy would be taking instructions from the thing it guards
/// against.
#[must_use]
pub fn nat64_embedded_ipv4(address: &Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = address.segments();
    // 64:ff9b:0:0:0:0 — the well-known prefix, with the low 32 bits carrying the
    // address. `0..6` and not `0..2`, because the well-known prefix is /96: bits 48
    // through 95 must be zero too, or this is some other address that merely starts
    // the same way.
    if segments[0] != 0x0064 || segments[1] != 0xff9b {
        return None;
    }
    if segments[2..6].iter().any(|segment| *segment != 0) {
        return None;
    }
    let [.., a, b, c, d] = address.octets();
    Some(Ipv4Addr::new(a, b, c, d))
}

/// The rule that makes `address` non-public, or `None` if it is publicly
/// routable.
///
/// Moved here from `model_sources.rs`, which had the broadest of the four
/// address blocklists and is the nearest sibling of the paths that now share it:
/// a download whose integrity comes from a digest rather than from an origin
/// pin. Not one range is added, removed, widened or narrowed by the move, and
/// the order is preserved for the reason that file gave — none of these ranges
/// overlap today, so order cannot change which rule is reported, and a future
/// range that *does* overlap should inherit the original precedence.
///
/// This is a *spelling* of the shared subset this module exists for, not the
/// wholesale unification its own doc declines: `web.rs` and `browser_worker.rs`
/// keep their own, deliberately different, blocklists.
#[must_use]
pub(crate) fn non_public_ipv4_rule(address: Ipv4Addr) -> Option<EgressRule> {
    if address.is_private() {
        return Some(EgressRule::PrivateV4);
    }
    if address.is_loopback() {
        return Some(EgressRule::Loopback);
    }
    if address.is_link_local() {
        return Some(EgressRule::LinkLocal);
    }
    if address.is_broadcast() {
        return Some(EgressRule::Broadcast);
    }
    if address.is_documentation() {
        return Some(EgressRule::TestNet);
    }
    if address.is_unspecified() {
        return Some(EgressRule::Unspecified);
    }
    if address.is_multicast() {
        return Some(EgressRule::Multicast);
    }
    None
}

/// Additional non-global ranges refused only by public component downloads.
fn non_public_download_ipv4_rule(address: Ipv4Addr) -> Option<EgressRule> {
    let [a, b, c, d] = address.octets();
    if a == 0 && (b != 0 || c != 0 || d != 0) {
        return Some(EgressRule::ThisNetwork);
    }
    if a == 100 && (64..128).contains(&b) {
        return Some(EgressRule::Cgnat);
    }
    if a == 192 && b == 0 && c == 0 {
        return Some(EgressRule::ProtocolAssignments);
    }
    if a == 198 && (18..20).contains(&b) {
        return Some(EgressRule::Benchmarking);
    }
    if a >= 240 && !address.is_broadcast() {
        return Some(EgressRule::ReservedRange);
    }
    None
}

fn non_public_download_ip_rule(address: std::net::IpAddr) -> Option<EgressRule> {
    non_public_ip_rule(address).or_else(|| match address {
        std::net::IpAddr::V4(address) => non_public_download_ipv4_rule(address),
        std::net::IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .or_else(|| nat64_embedded_ipv4(&address))
            .and_then(non_public_download_ipv4_rule),
    })
}

/// The IPv6 counterpart of [`non_public_ipv4_rule`], with the same guarantee: the
/// same addresses are refused as before the move, and only the location is new.
#[must_use]
pub(crate) fn non_public_ipv6_rule(address: Ipv6Addr) -> Option<EgressRule> {
    // Checked before the mapped unwrap: `::a.b.c.d` is not `::ffff:a.b.c.d`, and
    // without this it was reported as public. See [`is_ipv4_compatible`], whose
    // doc also explains why the range is refused outright instead of being
    // unwrapped and re-classified — doing that would turn `::1` into `0.0.0.1`,
    // which every predicate above calls public.
    if is_ipv4_compatible(&address) {
        return Some(EgressRule::Ipv4Compatible);
    }
    if let Some(address) = address.to_ipv4_mapped() {
        // A mapped address keeps whichever v4 rule its inner address reports, so
        // `::ffff:127.0.0.1` is `Loopback` and `::ffff:10.0.0.1` is `PrivateV4`
        // rather than both being some v6-flavoured approximation.
        return non_public_ipv4_rule(address);
    }
    // The third spelling, handled the same way: `64:ff9b::7f00:1` is `127.0.0.1`
    // wherever a NAT64/CLAT path exists. Unwrapped rather than refused as a
    // range, because `64:ff9b::` plus a public v4 address is a legitimate way to
    // reach a v4-only host from a v6-only network. See [`nat64_embedded_ipv4`].
    if let Some(address) = nat64_embedded_ipv4(&address) {
        return non_public_ipv4_rule(address);
    }
    let segments = address.segments();
    if address.is_loopback() {
        return Some(EgressRule::Loopback);
    }
    if address.is_unspecified() {
        return Some(EgressRule::Unspecified);
    }
    if address.is_multicast() {
        return Some(EgressRule::Multicast);
    }
    if address.is_unique_local() {
        return Some(EgressRule::UniqueLocalV6);
    }
    if address.is_unicast_link_local() {
        return Some(EgressRule::LinkLocal);
    }
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return Some(EgressRule::TestNet);
    }
    // Site-local, `fec0::/10`. Reported as `UniqueLocalV6` rather than getting a
    // rule of its own: RFC 3879 deprecated site-local *in favour of* unique-local
    // `fc00::/7`, so the two are the same policy about the same kind of
    // destination, and [`EgressRule`] has no site-local variant to invent one
    // from. The range itself is untouched — this arm still refuses exactly
    // `fec0::/10`.
    if (segments[0] & 0xffc0) == 0xfec0 {
        return Some(EgressRule::UniqueLocalV6);
    }
    None
}

/// [`non_public_ipv4_rule`]/[`non_public_ipv6_rule`] over either family.
///
/// Exists because the resolver in [`public_download_client`] classifies whatever
/// a name answered with, and a `SocketAddr` does not say which family it is until
/// it is matched on.
#[must_use]
pub(crate) fn non_public_ip_rule(address: std::net::IpAddr) -> Option<EgressRule> {
    match address {
        std::net::IpAddr::V4(address) => non_public_ipv4_rule(address),
        std::net::IpAddr::V6(address) => non_public_ipv6_rule(address),
    }
}

/// Whether `address` may be connected to under [`PublicDestinations`].
///
/// A single place for the one difference between the two modes, rather than the
/// two-branch prune it replaces: [`PublicDestinations::LoopbackAllowed`] permits
/// exactly the loopback class and nothing else, so a self-hosted catalog on this
/// machine still cannot be redirected onto the LAN.
fn refused_answer_rule(
    address: std::net::IpAddr,
    destinations: PublicDestinations,
) -> Option<EgressRule> {
    match if destinations == PublicDestinations::Only {
        non_public_download_ip_rule(address)
    } else {
        non_public_ip_rule(address)
    } {
        Some(EgressRule::Loopback) if destinations.allows_loopback() => None,
        other => other,
    }
}

/// Which destinations a component/catalog request may reach.
///
/// Two values rather than a `bool` because the permissive one is not "local
/// network allowed" — it is *loopback* allowed, and only because the endpoint the
/// user configured was itself loopback. `web.rs`'s `allow_local_network` is a
/// user-facing setting covering the whole private network; this is narrower and
/// derived, never configured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublicDestinations {
    /// Only publicly routable HTTPS destinations. The production default for a
    /// published catalog and for every artifact download.
    Only,
    /// Publicly routable HTTPS, plus loopback over `http`, because the request
    /// was aimed at loopback to begin with — a catalog served by something on
    /// this machine, and the shape the fixtures in this tree can produce.
    LoopbackAllowed,
}

impl PublicDestinations {
    const fn allows_loopback(self) -> bool {
        matches!(self, Self::LoopbackAllowed)
    }
}

/// Whether `url` is a destination [`PublicDestinations::Only`] accepts: public
/// HTTPS, no credential in the URL, no fragment, and not an address that points
/// back at this machine or its network.
///
/// Moved here from `model_sources.rs` together with the address rules above, so
/// the component/catalog path reuses the tree's strongest public-URL gate instead
/// of adding a fifth. Every reason names its own [`EgressRule`], which is what
/// lets a refused redirect hop report `egress.loopback` rather than a sentence
/// covering ten classes at once.
///
/// # What this cannot decide
///
/// A hostname. `https://attacker.example/x` passes here and may still resolve to
/// `127.0.0.1`; the name is exactly what a rebind can move. The resolver in
/// [`public_download_client`] is the enforcement point for that, and this stays a
/// layer rather than being folded into it because it is the only gate for a URL
/// that never reaches a resolver at all — every literal-address arm below.
pub(crate) fn classify_public_https_url(url: &Url) -> Result<(), EgressDenial> {
    if url.scheme() != "https" {
        return Err(EgressDenial::about(
            EgressRule::SchemeNotAllowed,
            url.scheme().to_string(),
        ));
    }
    // Username and password are one rule rather than two: both are `userinfo`,
    // and what the rule says is that the URL carries a credential at all.
    if !url.username().is_empty() || url.password().is_some() {
        // Deliberately detail-free. This is the rule `redacts_target` singles
        // out, and the detail field is the one place the secret could reappear.
        return Err(EgressDenial::new(EgressRule::EmbeddedCredentials));
    }
    if url.fragment().is_some() {
        return Err(EgressDenial::new(EgressRule::FragmentNotAllowed));
    }
    // Every `Host` variant is handled and there is no wildcard arm: if the `url`
    // crate ever adds a host kind, that must be a compile error here rather than
    // a new shape of host quietly treated as public.
    match url.host() {
        Some(url::Host::Domain(domain)) => {
            let trimmed = domain.trim_end_matches('.');
            if trimmed.is_empty()
                || trimmed.eq_ignore_ascii_case("localhost")
                || trimmed.to_ascii_lowercase().ends_with(".localhost")
            {
                // One rule for all three spellings: a name that is nothing but
                // dots, `localhost`, and `anything.localhost` are the same
                // destination by policy — this machine — and RFC 6761 reserves
                // the whole `.localhost` tree for loopback. The untrimmed name is
                // the detail so that the `.`-only case still says something.
                return Err(EgressDenial::about(EgressRule::Loopback, domain));
            }
        }
        Some(url::Host::Ipv4(address)) => {
            if let Some(rule) = non_public_ipv4_rule(address) {
                return Err(EgressDenial::about(rule, address.to_string()));
            }
        }
        Some(url::Host::Ipv6(address)) => {
            if let Some(rule) = non_public_ipv6_rule(address) {
                return Err(EgressDenial::about(rule, address.to_string()));
            }
        }
        // Unreachable in practice: `https` is a special scheme, so `Url`
        // guarantees a non-empty host for anything that got past the scheme check
        // above (`https:///x` does not parse at all). Kept refusing rather than
        // deleted or `unwrap`ped, because "no host, so nothing to check" is the
        // wrong direction for a destination gate to fail in.
        None => return Err(EgressDenial::new(EgressRule::HostMissing)),
    }
    Ok(())
}

pub(crate) fn classify_public_download_url(
    url: &Url,
    destinations: PublicDestinations,
) -> Result<(), EgressDenial> {
    classify_public_destination(url, destinations)?;
    if destinations == PublicDestinations::Only {
        let literal = match url.host() {
            Some(url::Host::Ipv4(address)) => Some(std::net::IpAddr::V4(address)),
            Some(url::Host::Ipv6(address)) => Some(std::net::IpAddr::V6(address)),
            _ => None,
        };
        if let Some(address) = literal {
            if let Some(rule) = non_public_download_ip_rule(address) {
                return Err(EgressDenial::about(rule, address.to_string()));
            }
        }
    }
    Ok(())
}

/// [`classify_public_https_url`], plus the loopback exception when the request
/// was aimed at loopback to begin with.
///
/// The exception is scheme-and-class narrow: `http` is accepted only for a
/// destination that *is* loopback. So a catalog served from this machine may
/// redirect within it, while a hop to plain `http` on any other host is still
/// refused — the permission a local endpoint earns is not a licence to walk the
/// request off the machine in cleartext.
fn classify_public_destination(
    url: &Url,
    destinations: PublicDestinations,
) -> Result<(), EgressDenial> {
    match classify_public_https_url(url) {
        Ok(()) => Ok(()),
        Err(denial) if destinations.allows_loopback() => {
            if (url.scheme() == "http" || url.scheme() == "https") && is_loopback_target(url) {
                // Re-checked rather than skipped: a loopback URL can still carry a
                // credential or a fragment, and those two rules are not about
                // where the request goes.
                if !url.username().is_empty() || url.password().is_some() {
                    return Err(EgressDenial::new(EgressRule::EmbeddedCredentials));
                }
                if url.fragment().is_some() {
                    return Err(EgressDenial::new(EgressRule::FragmentNotAllowed));
                }
                return Ok(());
            }
            Err(denial)
        }
        Err(denial) => Err(denial),
    }
}

/// Hop cap for [`public_download_client`]. Three, not [`MAX_REDIRECT_HOPS`]:
/// what this exists for is a release asset answering its stable URL with one
/// signed-CDN hop, and a chain longer than three is not that.
const MAX_PUBLIC_DOWNLOAD_HOPS: usize = 3;

/// A client for fetching public artifacts whose integrity comes from a digest
/// rather than from an origin pin, with every redirect hop and every resolved
/// address held to [`classify_public_https_url`].
///
/// # Why this exists rather than [`hardened`] alone
///
/// [`hardened`]'s [`same_origin_redirect_policy`] is right for a credentialed
/// path and wrong for this one. A published release asset has exactly one stable
/// URL — `releases/download/<tag>/<asset>` — and it answers with a `302` to a
/// signed, expiring URL on a different host. That hop is cross-origin by
/// construction, so a same-origin policy refuses the only way to reach the
/// artifact. Following it is safe *here* and would not be safe as a client
/// default, because of three properties this path has and a credentialed path
/// does not:
///
/// 1. the requests carry no credential of their own, so there is nothing for a
///    hop to forward;
/// 2. every hop is independently re-checked against the same public-destination
///    rule the initial URL had to pass, so a hop cannot lower the bar; and
/// 3. what is downloaded is verified against a SHA-256 the caller already held —
///    and, for a signed component, against a publisher key this app pins — so
///    the origin is not what establishes trust.
///
/// # Why the resolver, and not just the policy
///
/// A URL check decides on a *name*; the connection is opened against an
/// *address*, resolved afterwards. Whoever controls DNS for a name that passed
/// can answer the two lookups differently, so the place that passed the check and
/// the place that gets connected to need not be the same. The resolver installed
/// here removes the second lookup: it resolves through [`resolve_pinned`] — the
/// same per-run pin every [`hardened`] client uses — then hands the connector only
/// the answers that pass [`non_public_ip_rule`]. Those are exactly what reqwest
/// connects to, not a hint compared against a later answer, so there is no second
/// resolution left to race. `web.rs`'s `SsrfGuardedResolver` composes the same two
/// pieces for `web_fetch`; this is its public-only sibling.
///
/// # The one honest gap
///
/// [`hardened`] deliberately still inherits `HTTP(S)_PROXY`, and this does too. A
/// request that goes through a proxy is not resolved by this app at all — the
/// proxy resolves it — so the resolver below is never consulted and the pin does
/// not apply. That is not a second lookup this code could win; it is the whole
/// destination decision moving to the proxy. Kept rather than closed with
/// `no_proxy()`, because a corporate-proxy user losing the ability to install a
/// component is a worse outcome than a threat model (a hostile proxy) that already
/// applies to every other egress path in this tree — and on this path what is
/// downloaded is verified against a digest the caller already held, and for a
/// signed component against a key this app pins, neither of which a proxy can
/// forge.
pub(crate) fn public_download_client(
    destinations: PublicDestinations,
    guard: &'static str,
) -> reqwest::ClientBuilder {
    hardened()
        .redirect(public_redirect_policy(destinations, guard))
        .dns_resolver(std::sync::Arc::new(PublicOnlyResolver {
            destinations,
            guard,
            lookup: system_lookup,
        }))
}

/// Follows a redirect only when its target passes the same check the initial URL
/// had to pass, and only for [`MAX_PUBLIC_DOWNLOAD_HOPS`] hops.
///
/// `reqwest` owns the mechanics this deliberately does not reimplement: resolving
/// a relative `Location` against the URL that answered, re-issuing the request
/// with its headers — `Range` and `If-Range` among them, which a ranged download
/// depends on and which reqwest keeps because they are not in its cross-origin
/// strip list (`Authorization`, `Cookie`, `Proxy-Authorization`,
/// `WWW-Authenticate`) — and the method rules for 301/302/303/307/308, all of
/// which agree for the `GET` and `HEAD` this path sends. What is left, and all
/// that is left, is the verdict on each hop's target.
///
/// A refused hop errors the whole request rather than stopping at the last good
/// URL, for the reason `web.rs` and `model_sources.rs` both give: a caller must
/// not be able to mistake a blocked redirect for a successful fetch of the
/// pre-redirect page.
///
/// A `3xx` reqwest cannot follow — no `Location`, or one that will not parse —
/// never reaches this closure at all; it arrives at the caller as the `3xx`
/// response itself, which every caller on this path already refuses for not being
/// the status it required.
fn public_redirect_policy(
    destinations: PublicDestinations,
    guard: &'static str,
) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_PUBLIC_DOWNLOAD_HOPS {
            return attempt.error(refused(record(
                guard,
                EgressDenial::about(
                    EgressRule::RedirectHopLimit,
                    format!("refusing to follow more than {MAX_PUBLIC_DOWNLOAD_HOPS} redirects"),
                ),
            )));
        }
        // The run's frozen allowlist before the destination rule, in that order and
        // for `web.rs`'s reason: the destination rule can resolve a hostname, and a
        // name the run never allowed must not reach DNS at all. `send` checks the
        // initial URL; an automatic redirect never passes through `send` again.
        if let Err(denial) = check_run_allowlist(attempt.url()) {
            return attempt.error(refused(denial));
        }
        match classify_public_download_url(attempt.url(), destinations) {
            Ok(()) => {
                note_allowed_redirect_destination(attempt.url());
                attempt.follow()
            }
            // The hop's own rule, not a rule about redirects: a hop refused for
            // pointing at loopback says `egress.loopback`, so the reason reads the
            // same whether the address arrived in the request or in a `302`.
            Err(denial) => attempt.error(refused(record(guard, denial))),
        }
    })
}

/// Records a denial and hands it back, so a refusal inside a redirect closure is
/// attributable without the closure growing a second statement per branch.
fn record(guard: &'static str, denial: EgressDenial) -> EgressDenial {
    crate::denial_sink::record(guard, &denial, None);
    denial
}

/// Returns the typed denial embedded in a reqwest failure, when a policy refused
/// the request. Callers use this to preserve the actionable egress code instead
/// of flattening a redirect refusal into reqwest's generic transport wording.
pub(crate) fn denial_from_error(error: &reqwest::Error) -> Option<&EgressDenial> {
    let mut source = error.source();
    while let Some(current) = source {
        if let Some(denial) = current.downcast_ref::<EgressDenial>() {
            return Some(denial);
        }
        if let Some(io) = current.downcast_ref::<std::io::Error>() {
            if let Some(denial) = io
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<EgressDenial>())
            {
                return Some(denial);
            }
        }
        source = current.source();
    }
    None
}

/// The connect-time half of [`public_download_client`]: resolve through the
/// per-run pin, then hand the connector only the answers a public destination rule
/// accepts.
///
/// `lookup` is a field for the reason [`HostLookup`] exists at all — a test that
/// has to prove "a name resolving to `127.0.0.1` is refused" cannot say so through
/// the system resolver without owning a domain that does it. Production always
/// passes [`system_lookup`].
struct PublicOnlyResolver {
    destinations: PublicDestinations,
    guard: &'static str,
    lookup: HostLookup,
}

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let destinations = self.destinations;
        let guard = self.guard;
        let host = name.as_str().to_string();
        // Taken while this task's run scope is active, like `web.rs` does: the pin
        // is keyed to the run, and the future below may be polled elsewhere.
        let pinned = <PinnedResolver as reqwest::dns::Resolve>::resolve(
            &PinnedResolver {
                lookup: self.lookup,
            },
            name,
        );
        Box::pin(async move {
            let resolved = pinned.await?;
            let mut refused_rule = None;
            let allowed: Vec<SocketAddr> = resolved
                .filter(
                    |address| match refused_answer_rule(address.ip(), destinations) {
                        Some(rule) => {
                            refused_rule = Some(rule);
                            false
                        }
                        None => true,
                    },
                )
                .collect();
            if allowed.is_empty() {
                // Two cases and not one: "the lookup came back empty", which no
                // rule refused, and "every answer is refused", which names the rule
                // that accounted for the last one.
                let denial = match refused_rule {
                    Some(rule) => EgressDenial::about(
                        rule,
                        format!("{host} resolves only to addresses this rule refuses"),
                    ),
                    None => EgressDenial::about(EgressRule::DnsNoAddresses, host.clone()),
                };
                crate::denial_sink::record(guard, &denial, None);
                return Err(Box::new(denial) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(allowed.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// How long a connection may take to establish. Matches `model_sources.rs`'s
/// own `connect_timeout` — the nearest sibling, also a credentialed egress
/// path — rather than inventing a new number. Generous on purpose: this is the
/// TCP+TLS handshake (or the proxy `CONNECT` tunnel, since [`hardened`]
/// deliberately still inherits `HTTP(S)_PROXY`), not the response.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a *single* read may stall before the request is abandoned. Resets
/// after every successful read (reqwest's own wording: "applies to each read
/// operation, and resets after a successful read"), so this bounds **silence**,
/// not elapsed time — a streaming SSE response that keeps producing chunks can
/// run for hours under it.
///
/// # Why not `ClientBuilder::timeout`
///
/// That one is documented as "a total deadline", applied "from when the request
/// starts connecting until the response body has finished". On these paths that
/// is not a safety net, it is a truncation bug: `providers.rs` sends
/// `"stream": true` and reads the body incrementally for as long as the model
/// generates, and `m7_companion.rs` already had to hand-write 30- and 60-minute
/// total budgets for exactly that reason. A total deadline that is long enough
/// not to truncate a long generation is far too long to detect a dead peer,
/// which is the whole point.
///
/// # Why ten minutes
///
/// The number only has to be *finite* to close the "hangs forever" hole; what
/// it must not do is preempt a deadline some caller set deliberately. The
/// longest such default on the adopting paths is `mcp.rs`'s per-call
/// `DEFAULT_TIMEOUT_SECS` (60s, on top of a 30s `CONNECT_TIMEOUT_SECS`), so ten
/// minutes sits an order of magnitude clear of it. The residual case is a
/// server entry whose `timeout_secs` is *configured* above this: an MCP tool
/// that computes for >10 minutes and sends nothing at all while it does (no SSE
/// progress notifications, which would each reset the clock) is now cut here
/// rather than at its configured budget. Accepted rather than dropping the read
/// timeout on the MCP path, because that transport is precisely where the OAuth
/// bearer is attached.
pub const READ_TIMEOUT: Duration = Duration::from_secs(600);

/// [`hardened`], but with a caller-chosen silence budget.
///
/// Exists for one specific reason: a caller that already has an explicit,
/// user-configured budget must not have it preempted by the default. An MCP server
/// configured with a 15-minute tool timeout that sends nothing at all while working
/// would otherwise be cut at [`READ_TIMEOUT`], turning a supported configuration
/// into a failure. Such callers pass `max(READ_TIMEOUT, their own budget)`, so the
/// default remains a floor and never a ceiling.
#[allow(clippy::doc_markdown)]
pub fn hardened_with_read_budget(read: Duration) -> reqwest::ClientBuilder {
    hardened_with_timeouts(CONNECT_TIMEOUT, read)
}

/// Hop cap for [`same_origin_redirect_policy`].
///
/// `reqwest::redirect::Policy::custom` does **not** inherit the loop cap that
/// `Policy::default()`/`Policy::limited(10)` provide — its own docs say so, and
/// `web.rs` already had to learn it (see that file's `MAX_REDIRECT_HOPS`). So
/// without this line a same-origin server that redirects to itself forever
/// would be followed forever. Set to reqwest's own default of 10 rather than
/// something tighter purely so `hardened()` never refuses a chain the stock
/// client would have accepted for a reason *other* than the origin rule.
const MAX_REDIRECT_HOPS: usize = 10;

/// The `reqwest::ClientBuilder` every credentialed remote path in this app
/// should start from: a connect timeout, a read timeout, and a redirect policy
/// that will not carry a credential to a host the *response* chose.
///
/// Returns a builder, not a `Client`, so a caller can still add its own
/// per-path options (a user agent, `default_headers`, a longer budget) without
/// having to know which hardening it would be dropping by calling
/// `Client::builder()` itself.
///
/// # The three holes this closes
///
/// 1. **`x-api-key` survived a cross-origin redirect.** reqwest strips only
///    `Authorization`, `Cookie`, `Proxy-Authorization` and `WWW-Authenticate`
///    when a redirect crosses hosts. It does not strip `x-api-key`, which
///    `providers.rs`'s `add_anthropic_headers` sets — and a custom provider's
///    `base_url` is user-configurable, so a `302` could walk that key to a
///    host the redirect picked. That is credential forwarding, not a leak of
///    metadata.
/// 2. **A `302` could reach unauthenticated loopback services.** The default
///    `Policy::limited(10)` follows up to ten hops to arbitrary hosts,
///    including this machine's own `llama-server` and `ollama:11434`, neither
///    of which has any authentication at all.
/// 3. **No timeout whatsoever.** A default `reqwest::Client` sets none, so a
///    peer that completes a TCP handshake and then goes silent holds the
///    caller's task open indefinitely.
///
/// Prose here used to have to say `Client::builder` rather than the bare
/// constructor, because the ratchet at the bottom of this file counted that exact
/// string across the tree and a doc comment naming it registered as a site. Both
/// ratchets now strip comment-only lines before scanning (see `code_only`), so
/// that tax is gone and prose may name whatever it is describing.
///
/// # Two things this deliberately does not do
///
/// - **It does not call `.no_proxy()`.** Forty-plus sites in this tree inherit
///   `HTTP(S)_PROXY` today and only two opt out; a corporate-proxy user losing
///   all outbound access is a worse outcome than the inconsistency.
/// - **It adds no "HTTPS only" or "public addresses only" rule.** A custom
///   provider at `http://127.0.0.1:1234/v1` (LM Studio, vLLM, LiteLLM) is a
///   supported configuration — `providers.rs`'s `validate_base_url` accepts
///   `http://` by design — so either rule would break local inference. Where a
///   public-HTTPS rule *is* wanted, `model_sources.rs` and `web.rs` already
///   have one; this is orthogonal to it.
// No `#[must_use]`: `ClientBuilder` already carries one, and doubling it is a
// clippy warning rather than extra safety.
pub fn hardened() -> reqwest::ClientBuilder {
    hardened_with_timeouts(CONNECT_TIMEOUT, READ_TIMEOUT)
}

/// [`hardened`] with its two budgets injected.
///
/// Exists only so a test can prove the read timeout is really wired up: the
/// production budget is ten minutes, and a test that waited it out would not be
/// a test. Kept private so no production caller can quietly widen a budget by
/// reaching for this instead of [`hardened`].
fn hardened_with_timeouts(connect: Duration, read: Duration) -> reqwest::ClientBuilder {
    hardened_with_lookup(connect, read, system_lookup)
}

/// [`hardened_with_timeouts`] with the name lookup injected, so a test can pin a
/// resolution it controls. Same reason `hardened_with_timeouts` itself exists.
fn hardened_with_lookup(
    connect: Duration,
    read: Duration,
    lookup: HostLookup,
) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(connect)
        .read_timeout(read)
        .redirect(same_origin_redirect_policy())
        .dns_resolver(std::sync::Arc::new(PinnedResolver { lookup }))
}

/// Follows a redirect only when it stays on the origin the request was already
/// aimed at, and only for [`MAX_REDIRECT_HOPS`] hops.
///
/// Modelled on `model_sources.rs`'s validating policy, but the check is
/// *relative* (does this hop stay where it already was?) rather than absolute
/// (is this hop's target a public HTTPS host?). It has to be: the absolute rule
/// would refuse the loopback and plain-`http` provider endpoints this app
/// supports on purpose, whereas the relative rule is exactly the property that
/// makes an un-stripped `x-api-key` harmless.
///
/// A refused hop is an `Err` for the whole request, never a silent stop at the
/// last good URL — `web.rs` documents the same choice, and the reason is the
/// same: a caller must not mistake a blocked redirect for a successful fetch of
/// the pre-redirect page.
fn same_origin_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECT_HOPS {
            return attempt.error(refused(EgressDenial::about(
                EgressRule::RedirectHopLimit,
                format!("refusing to follow more than {MAX_REDIRECT_HOPS} redirects"),
            )));
        }
        // `previous()` cannot actually be empty here — reqwest pushes the
        // requested URL before it ever consults the policy. Treated as a
        // refusal rather than `unwrap`ped because if that ever changed there
        // would be no origin to compare against, and "follow it anyway" is the
        // wrong direction to fail in for a client carrying an API key.
        let Some(previous) = attempt.previous().last() else {
            return attempt.error(refused(EgressDenial::new(
                EgressRule::RedirectOriginUnknown,
            )));
        };
        // The verdict is taken first, and the follow returns immediately, because
        // `Attempt::follow`/`Attempt::error` consume `attempt` while `previous`
        // is still borrowed out of it — that borrow has to end before either
        // call. Returning early also keeps the message off the happy path: it is
        // built from owned `String`s and would otherwise be allocated on every
        // hop that gets followed.
        // The run's allowlist again, on the hop rather than the request. Not
        // redundant with the check in `send`: `may_follow` permits an
        // `http` -> `https` upgrade on the same authority, and that changes the
        // effective port (80 to 443), so a hop can reach a protocol and a port the
        // original request never asked for.
        if let Err(denial) = check_run_allowlist(attempt.url()) {
            return attempt.error(refused(denial));
        }
        if may_follow(previous, attempt.url()) {
            note_allowed_redirect_destination(attempt.url());
            return attempt.follow();
        }
        let refusal = EgressDenial::about(
            EgressRule::RedirectCrossOrigin,
            format!(
                "refusing to carry credentials from {} to {}",
                origin_label(previous),
                origin_label(attempt.url())
            ),
        );
        attempt.error(refused(refusal))
    })
}

/// Wraps a denial for a signature that wants an error rather than a verdict.
///
/// `PermissionDenied` is kept from the hand-written version it replaces, and the
/// denial is passed as itself rather than as `to_string()` so the rule survives
/// the trip: `io::Error::into_inner`/`downcast_ref` can still recover an
/// [`EgressDenial`] on the far side of reqwest.
fn refused(denial: EgressDenial) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, denial)
}

/// Whether a redirect from `previous` to `next` stays on the same origin.
///
/// Split out as a pure function of two `Url`s so the origin rules can be
/// unit-tested without a socket — `reqwest::redirect::Attempt` cannot be
/// constructed outside reqwest, so a policy closure is only reachable through a
/// real request.
///
/// The one exception to strict `(scheme, host, port)` equality is an
/// `http` → `https` upgrade on an otherwise identical authority. Without it,
/// every host that answers plain `http` with `301 https://same-host/...` — the
/// single most common redirect on the web — would break for a user who typed a
/// custom provider `base_url` with the wrong scheme, and that is a regression,
/// not a fix: the destination is the host the request was always aimed at, so
/// nothing is forwarded anywhere new. The inverse (`https` → `http`) is refused,
/// which is why this is written as an explicit upgrade test rather than the
/// tempting "ignore the scheme, compare host and port" shortcut — that shortcut
/// also permits the downgrade, silently turning a TLS-protected key into a
/// cleartext one.
fn may_follow(previous: &Url, next: &Url) -> bool {
    // `None == None` must not count as a match: two hostless URLs (`data:`,
    // `file:`) are not "the same origin", they have no origin.
    let Some(host) = previous.host_str() else {
        return false;
    };
    if next.host_str() != Some(host) {
        return false;
    }
    if previous.scheme() == next.scheme() {
        // Same scheme, so `port_or_known_default` compares like with like and
        // `http://h` == `http://h:80`.
        return previous.port_or_known_default() == next.port_or_known_default();
    }
    // `Url::port()` is `None` whenever the port is the scheme's own default, so
    // this accepts `http://h` -> `https://h` and `http://h:80` -> `https://h:443`
    // (both `None` -> `None`) and `http://h:8080` -> `https://h:8080`, while
    // refusing a hop that also changes the port.
    previous.scheme() == "http" && next.scheme() == "https" && previous.port() == next.port()
}

/// Whether `url` names a peer on this machine.
///
/// # Why a run with no network permission may still reach loopback
///
/// `permission_policy.allow_network` defaults to `false`, and a run targeting the
/// bundled `llama-server` or a local Ollama is submitted with it `false` — quite
/// correctly, because such a run uses no network in the sense the flag means.
/// Reading the flag as "no sockets at all" would therefore refuse every local
/// inference run, which is not a stricter policy, it is a broken one. The flag
/// gates *leaving this machine*.
///
/// Deliberately narrow: literal loopback addresses and the `localhost` names that
/// resolve to them. It does not consult DNS, so a hostname that happens to resolve
/// to `127.0.0.1` is not treated as local here — that is the conservative
/// direction for this predicate, since a name is exactly what a rebind can move.
#[must_use]
pub fn is_loopback_target(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(domain)) => {
            let lowered = domain.to_ascii_lowercase();
            lowered == "localhost" || lowered.ends_with(".localhost")
        }
        None => false,
    }
}

/// `scheme://host:port` for a diagnostic — deliberately **not** the whole URL.
///
/// These messages surface in provider errors the UI shows and the CLI prints,
/// and some of these paths put tokens in a query string (`hosted_oauth`'s relay
/// among them). Naming only the origin says everything a reader needs about a
/// refused hop without copying a credential into a log.
pub(crate) fn origin_label(url: &Url) -> String {
    match (url.host_str(), url.port_or_known_default()) {
        (Some(host), Some(port)) => format!("{}://{host}:{port}", url.scheme()),
        (Some(host), None) => format!("{}://{host}", url.scheme()),
        _ => url.scheme().to_string(),
    }
}

/// What the installed source knows about one run's frozen egress policy.
///
/// Four answers and not two, because the two that look alike are the ones that must
/// not behave alike: [`Unknown`](Self::Unknown) is "no such run", which is ordinary
/// and permitted, while [`Unavailable`](Self::Unavailable) is "the row could not be
/// read", which is refused. Collapsing them would either refuse every run id that is
/// not a ledger entity (`browser_worker` and `m4_runtime` both scope work under ids
/// the run ledger has never seen) or allow a declared policy to be skipped by
/// breaking the ledger.
pub enum RunEgressPolicy {
    /// The run's frozen spec declares an allowlist.
    Declared(std::sync::Arc<crate::run_protocol::EgressAllowlist>),
    /// The run exists and its frozen spec declares nothing.
    Undeclared,
    /// No run by that id. A [`run_scope::RunScope::Run`] id is not necessarily a
    /// row in the run ledger.
    Unknown,
    /// The frozen spec could not be read at all.
    Unavailable,
}

type PolicySource = dyn Fn(&str) -> RunEgressPolicy + Send + Sync;

/// The process-wide way to read a run's frozen policy, or `None` before one is
/// installed.
///
/// A global for the reason `denial_sink`'s recorder is one, and the argument is
/// stronger here: [`send`] is called from 92 sites whose signatures have no
/// `AppHandle`, no `AppState` and no business acquiring either. The run *identity*
/// arrives implicitly through [`run_scope`]; this is how the frozen row behind that
/// identity is fetched, and it is installed once at startup by
/// `run_commands::install_run_egress_policy_source`.
///
/// Unlike the sink, this global does decide something, so the direction of its
/// absence matters: with no source installed there is no policy to enforce and
/// [`send`] behaves exactly as it did before this existed. That is what keeps every
/// unit test, the CLI, and any embedder that never opens a run ledger working, and
/// it is not a hole a run can open — nothing reachable by a model, a skill or a
/// package can uninstall a source, and the app installs one before it serves a
/// window.
static POLICY_SOURCE: OnceLock<std::sync::RwLock<Option<std::sync::Arc<PolicySource>>>> =
    OnceLock::new();

fn policy_source_slot() -> &'static std::sync::RwLock<Option<std::sync::Arc<PolicySource>>> {
    POLICY_SOURCE.get_or_init(|| std::sync::RwLock::new(None))
}

/// Frozen policies already read, so a ledger row is read once per run and not once
/// per request.
///
/// Safe to cache at all only because a run spec is immutable: the row is written once
/// at submission and there is no update path to it, so a cached answer cannot be
/// stale in the direction that matters. Dropping an entry is therefore only ever a
/// cost — the next request re-reads the same frozen answer, or fails closed if it
/// cannot.
///
/// `None` covers both [`RunEgressPolicy::Undeclared`] and
/// [`RunEgressPolicy::Unknown`]: both mean "no allowlist governs this run", and
/// caching `Unknown` is sound because a scope is entered *after* its run row is
/// written, so a real run cannot look unknown on its first request and declared on
/// its second. [`RunEgressPolicy::Unavailable`] is never cached — a transient read
/// failure must not be remembered as an answer.
static POLICY_CACHE: OnceLock<
    Mutex<
        std::collections::HashMap<
            String,
            Option<std::sync::Arc<crate::run_protocol::EgressAllowlist>>,
        >,
    >,
> = OnceLock::new();

/// Cached runs kept. Concurrency here is a handful of runs, not thousands; the bound
/// exists so a long-lived process that has seen ten thousand runs does not keep all
/// of them.
///
/// ponytail: cleared wholesale at the bound rather than evicted per entry, because a
/// dropped entry costs one re-read and can never widen a policy. Upgrade path if the
/// re-read ever matters: an LRU, or eviction when a run reaches a terminal event.
const MAX_CACHED_RUN_POLICIES: usize = 256;

fn policy_cache() -> &'static Mutex<
    std::collections::HashMap<String, Option<std::sync::Arc<crate::run_protocol::EgressAllowlist>>>,
> {
    POLICY_CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Installs the process-wide policy source, replacing any previous one.
///
/// Clears the cache, because a new source may answer differently about the same run
/// id — which in production never happens (it is installed once) and in tests
/// happens constantly.
pub fn install_run_policy_source<F>(source: F)
where
    F: Fn(&str) -> RunEgressPolicy + Send + Sync + 'static,
{
    if let Ok(mut slot) = policy_source_slot().write() {
        *slot = Some(std::sync::Arc::new(source));
    }
    if let Ok(mut cache) = policy_cache().lock() {
        cache.clear();
    }
}

/// Uninstalls the source and forgets every cached answer.
///
/// Tests only, and they must hold `denial_sink::test_lock()` while they use it: the
/// source is process-wide, so a test that left one installed would decide another
/// test's requests.
#[cfg(test)]
pub(crate) fn clear_run_policy_source() {
    if let Ok(mut slot) = policy_source_slot().write() {
        *slot = None;
    }
    if let Ok(mut cache) = policy_cache().lock() {
        cache.clear();
    }
}

/// The allowlist governing `run_id`, `Ok(None)` when none does, and `Err` when the
/// frozen policy could not be read.
fn policy_for_run(
    run_id: &str,
) -> Result<Option<std::sync::Arc<crate::run_protocol::EgressAllowlist>>, EgressDenial> {
    if let Ok(cache) = policy_cache().lock() {
        if let Some(cached) = cache.get(run_id) {
            return Ok(cached.clone());
        }
    }
    let Some(source) = policy_source_slot()
        .read()
        .ok()
        .and_then(|slot| slot.clone())
    else {
        // No source installed: nothing to enforce, and see `POLICY_SOURCE` for why
        // that is the safe direction for this one absence.
        return Ok(None);
    };

    let resolved = match source(run_id) {
        RunEgressPolicy::Declared(allowlist) => Some(allowlist),
        RunEgressPolicy::Undeclared | RunEgressPolicy::Unknown => None,
        RunEgressPolicy::Unavailable => {
            return Err(EgressDenial::about(
                EgressRule::RunPolicyUnavailable,
                format!("run {run_id}"),
            ))
        }
    };
    if let Ok(mut cache) = policy_cache().lock() {
        if cache.len() >= MAX_CACHED_RUN_POLICIES {
            cache.clear();
        }
        cache.insert(run_id.to_string(), resolved.clone());
    }
    Ok(resolved)
}

/// Whether one allowlist host entry matches `host`.
///
/// `host` must already be lowercased; entries are lowercase by validation, so no
/// call site folds anything twice.
///
/// # The boundary is the whole point
///
/// A wildcard entry `*.example.com` matches a name that ends with `.example.com`
/// *including the dot*, so `evil-example.com` does not match — the naive
/// `ends_with("example.com")` would have said it did, and that is the classic
/// suffix-matching bypass. The apex is deliberately excluded too: `*.example.com`
/// does not match `example.com`, which must be named in its own right. Same shape as
/// `http_route_registry`'s `ExactOrDescendant`, which checks for the `/` separator
/// rather than trusting a prefix.
#[must_use]
pub fn allowlist_host_matches(entry: &str, host: &str) -> bool {
    match entry.strip_prefix("*.") {
        Some(suffix) => host
            .strip_suffix(suffix)
            .is_some_and(|label| label.ends_with('.') && label.len() > 1),
        None => entry == host,
    }
}

/// Names this gate in a denial record.
const RUN_ALLOWLIST_GUARD: &str = "egress.run-allowlist";

/// Refuses a request the ambient run's frozen allowlist does not permit, and writes
/// the refusal down.
///
/// # Fail-closed and fail-open are both deliberate, and they are different cases
///
/// - **No run in scope** — behaves exactly as before. Some egress legitimately has
///   no run and never will: timer-driven knowledge refresh, connector verification
///   in Settings, model downloads, update checks, and every inbound request
///   `server.rs` serves. Deny-by-default keyed to a run would silently disable all
///   of them, which is why the run is the key and the absence of one is an answer.
/// - **A run whose policy declares nothing** — also as before. Every run row frozen
///   before this field existed says exactly this, and reading those as deny-all
///   would refuse every in-flight run.
/// - **A run whose policy cannot be read** — refused. This is the one that fails
///   closed, and the cost is bounded by what it means: the frozen spec lives in the
///   same ledger a run appends its events to, so a run whose spec cannot be read
///   cannot record anything either. Refusing its egress is not the failure, it is a
///   symptom of one.
/// - **A declared allowlist** — deny-by-default within it, on all three dimensions.
///
/// Loopback is exempt, for [`is_loopback_target`]'s reason: a local-inference run
/// talks to `127.0.0.1` and refusing that is a broken policy rather than a strict
/// one.
pub(crate) fn check_run_allowlist(url: &Url) -> Result<(), EgressDenial> {
    let verdict = run_allowlist_verdict(url);
    if let Err(denial) = &verdict {
        // Recorded here, at the raise site, because by the time this reaches a
        // command it is a `String` — `denial_sink`'s own finding. The run id comes
        // from the ambient scope, so nothing has to be threaded.
        crate::denial_sink::record(RUN_ALLOWLIST_GUARD, denial, None);
    }
    verdict
}

/// [`check_run_allowlist`] without the recording, so the rules can be tested as a
/// pure function of a URL.
fn run_allowlist_verdict(url: &Url) -> Result<(), EgressDenial> {
    let Some(run_id) = run_scope::current_run_id() else {
        return Ok(());
    };
    // Ahead of the policy read, not after it: the exemption holds whatever the policy
    // says, so a local-inference run never reads a ledger row to be told what it is
    // allowed to do on its own machine.
    if is_loopback_target(url) {
        return Ok(());
    }
    let Some(allowlist) = policy_for_run(&run_id)? else {
        return Ok(());
    };

    let Some(host) = url.host_str() else {
        return Err(EgressDenial::new(EgressRule::HostMissing));
    };
    let host = host.to_ascii_lowercase();
    if !allowlist
        .hosts
        .iter()
        .any(|entry| allowlist_host_matches(entry, &host))
    {
        return Err(EgressDenial::about(
            EgressRule::RunHostNotAllowlisted,
            format!(
                "{host} is not among the {} hosts run {run_id} declared",
                allowlist.hosts.len()
            ),
        ));
    }

    let Some(port) = url.port_or_known_default() else {
        return Err(EgressDenial::new(EgressRule::PortMissing));
    };
    if !allowlist.ports.contains(&port) {
        return Err(EgressDenial::about(
            EgressRule::RunPortNotAllowlisted,
            format!("port {port} is not among the ports run {run_id} declared"),
        ));
    }

    let scheme = url.scheme();
    if !allowlist
        .protocols
        .iter()
        .any(|protocol| protocol == scheme)
    {
        return Err(EgressDenial::about(
            EgressRule::RunProtocolNotAllowlisted,
            format!("{scheme} is not among the protocols run {run_id} declared"),
        ));
    }
    Ok(())
}

/// Host of the throwaway request [`refusal_error`] never sends.
///
/// `.invalid` is reserved by RFC 2606 and is never looked up: the resolver below
/// refuses before any lookup happens, and the client is built `.no_proxy()` so a
/// configured proxy cannot be handed the name instead.
const REFUSAL_HOST: &str = "egress-denied.invalid";

/// Turns a denial into the `reqwest::Error` a caller of [`send`] receives.
///
/// # Why this is a request and not a constructor
///
/// `reqwest::Error` has no public constructor — every one is built inside reqwest
/// from a source it was handed. So a refusal reaches the caller the one way reqwest
/// will carry somebody else's error: through a `dns_resolver` that refuses. The
/// denial arrives on the far side as itself, recoverable with `downcast_ref` and
/// rendered with its code, exactly like the redirect policy's refusals.
///
/// Nothing leaves this process. The throwaway request names [`REFUSAL_HOST`], the
/// resolver refuses it without a lookup, and the denied target is never mentioned to
/// anything outside this function.
///
/// ponytail: builds a client per refusal, because the denial has to be *in* the
/// resolver and a shared client cannot hold a per-request one. A refusal already
/// costs a sink insert, so this is not the expensive half. Upgrade path if it ever
/// matters: one cached client per rule.
async fn refusal_error(denial: EgressDenial) -> reqwest::Error {
    struct AlwaysRefuse(EgressDenial);

    impl reqwest::dns::Resolve for AlwaysRefuse {
        fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
            let denial = self.0.clone();
            Box::pin(
                async move { Err(Box::new(denial) as Box<dyn std::error::Error + Send + Sync>) },
            )
        }
    }

    let client = reqwest::Client::builder()
        .no_proxy()
        .dns_resolver(std::sync::Arc::new(AlwaysRefuse(denial)))
        .build();
    match client {
        Ok(client) => client
            .get(format!("http://{REFUSAL_HOST}/"))
            .send()
            .await
            // A resolver whose every answer is an error cannot produce a response,
            // and the URL is a literal this function wrote.
            .expect_err("a resolver that refuses every name cannot answer a request"),
        // A `ClientBuilder` that will not build is itself a `reqwest::Error`, which
        // is the right thing to return: the request is still refused, and the caller
        // still gets an error rather than a response.
        Err(error) => error,
    }
}

/// Addresses pinned per `(run, host)` for the lifetime of the run.
///
/// See [`PinnedResolver`] for what this closes and what it costs.
static DNS_PINS: OnceLock<Mutex<std::collections::HashMap<(String, String), Vec<SocketAddr>>>> =
    OnceLock::new();

/// Pins kept. A run touching more names than this is not the case this exists for.
///
/// ponytail: cleared wholesale at the bound, like [`POLICY_CACHE`]. Unlike that one
/// a dropped pin is not merely a re-read — it is a re-resolution, which reopens the
/// window this table closes for that name. Stated rather than hidden; upgrade path is
/// eviction keyed to a run reaching a terminal event, which needs a hook this module
/// does not have.
const MAX_DNS_PINS: usize = 512;

fn dns_pins() -> &'static Mutex<std::collections::HashMap<(String, String), Vec<SocketAddr>>> {
    DNS_PINS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// How a name is resolved when no pin exists yet. A function pointer purely so a
/// test can supply a resolver that answers differently on the second call, which is
/// the only way to write a rebinding test that is not a mock of itself.
type HostLookup =
    fn(String) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send>>;

fn system_lookup(
    host: String,
) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send>> {
    Box::pin(async move {
        // Port `0`: reqwest replaces it with the URL's own port (its `Resolve` doc
        // says so), which is also why a pin binds an *address* and not an endpoint.
        // Formatted as an owned string so the returned future borrows nothing.
        tokio::net::lookup_host(format!("{host}:0"))
            .await
            .map(|addresses| addresses.collect())
    })
}

/// Resolves a name once per run and pins the answer for the rest of the run.
///
/// # The gap this closes
///
/// [`check_run_allowlist`] decides on a *name*. The connection is opened against an
/// *address*, resolved afterwards, and an attacker who controls DNS for an
/// allowlisted name (TTL 0, or a rebinding service) can answer the two lookups
/// differently — so the name that passed the check and the address that gets
/// connected to need not be the same place. Pinning removes the second lookup: the
/// addresses handed back here are exactly what reqwest connects to, not a hint
/// compared against a later answer, so there is no second resolution left to race.
/// `web.rs`'s `SsrfGuardedResolver` composes this pin with its own address-class
/// guard, and `browser_worker.rs` does the equivalent per Chromium launch with
/// `--host-resolver-rules`.
///
/// # What it costs, which is real
///
/// A long run against a rotating CDN keeps the address it first saw. If that address
/// is withdrawn mid-run the run's requests fail rather than following the rotation,
/// and the fix is a new run — the pin is per run, so nothing outlives it. That is the
/// deliberate trade: an allowed name that cannot be moved is worth more here than one
/// that follows every answer, and a rebind is indistinguishable from a rotation at
/// the resolver. Two further honest limits:
///
/// - **A pooled connection is not re-resolved at all.** reqwest keys its pool by
///   scheme, host and port, so a request reusing a connection another run opened
///   connects wherever that run's pin pointed. Both runs had to allow the same host
///   for that to happen, and the address was a pinned answer for that name either
///   way, so nothing unallowed is reached — but the pin that applies is the first
///   run's.
/// - **It applies to clients built from [`hardened`].** A caller that reaches for
///   `Client::builder()` itself gets the default resolver and no pin; the ratchet at
///   the bottom of this file is what keeps that set from growing quietly.
struct PinnedResolver {
    lookup: HostLookup,
}

impl reqwest::dns::Resolve for PinnedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        // Read synchronously, here, rather than inside the future: this call happens
        // on the task that is driving the request, which is the task the scope was
        // entered on.
        let run = run_scope::current_run_id();
        let host = name.as_str().to_ascii_lowercase();
        let lookup = self.lookup;
        Box::pin(async move {
            let key = run.map(|run| (run, host.clone()));
            if let Some(key) = &key {
                if let Some(pinned) = dns_pins()
                    .lock()
                    .ok()
                    .and_then(|pins| pins.get(key).cloned())
                {
                    return Ok(Box::new(pinned.into_iter()) as reqwest::dns::Addrs);
                }
            }

            let addresses = lookup(host)
                .await
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?;
            // An empty answer is passed through rather than pinned or renamed: the
            // connector reports it, and pinning "nothing" would make one bad answer
            // permanent for the run.
            if let (Some(key), false) = (key, addresses.is_empty()) {
                if let Ok(mut pins) = dns_pins().lock() {
                    if pins.len() >= MAX_DNS_PINS {
                        pins.clear();
                    }
                    pins.insert(key, addresses.clone());
                }
            }
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Resolve through K5's per-run DNS pin without replacing the caller's own
/// address-class policy.
///
/// Most callers install [`PinnedResolver`] indirectly through [`hardened`].
/// `web_fetch` also has to remove private/loopback answers before reqwest sees
/// them, so its SSRF resolver composes with the same pin here instead of
/// replacing it with a second system lookup or maintaining a second cache.
pub(crate) fn resolve_pinned(name: reqwest::dns::Name) -> reqwest::dns::Resolving {
    use reqwest::dns::Resolve;

    PinnedResolver {
        lookup: system_lookup,
    }
    .resolve(name)
}

/// Bytes counted for work with no process row to charge, one tally per reason.
///
/// # Why these exist rather than a `return`
///
/// The ledger's whole claim is that a number in it was measured and a `NULL` in it
/// says why it was not. Bytes that crossed the wire under no attribution are
/// neither: dropping them on the floor would make the process rows *look*
/// complete while the totals quietly disagreed with the network, and charging them
/// to whichever process happened to be nearby would be worse. So they are counted
/// here, under the reason they could not be attributed, where a support bundle can
/// read them and where the tests below can prove nothing was silently lost.
///
/// Process-lifetime tallies and deliberately not ledger rows: there is no row to
/// write them to, which is the whole point. `Relaxed` for the same reason
/// [`run_scope::ProcessScope::charge_egress`] uses it — each tally has to be right,
/// not ordered against anything else.
///
/// Sized from [`Unattributed::ALL`] plus the two cases that enum does not cover, so
/// a reason added there gets a tally here without anyone remembering to widen this.
static UNATTRIBUTED_EGRESS: [AtomicU64; UNATTRIBUTED_BUCKETS] =
    [const { AtomicU64::new(0) }; UNATTRIBUTED_BUCKETS];

const UNATTRIBUTED_BUCKETS: usize = Unattributed::ALL.len() + 2;

/// A run was in scope but no process row was, so the ledger has nothing to charge.
/// Distinct from every [`Unattributed`] reason, all of which explain the absence of
/// a *run*.
const RUN_WITHOUT_PROCESS: usize = Unattributed::ALL.len();

/// No scope at all: a call site nobody has instrumented yet. Kept apart from
/// `RUN_WITHOUT_PROCESS` for the reason `run_scope` keeps `None` apart from
/// `Unattributed` — "we lost it" and "there is deliberately nothing" cannot be
/// read out of one number.
const NO_SCOPE: usize = Unattributed::ALL.len() + 1;

/// The stable label of each tally, in [`UNATTRIBUTED_EGRESS`] order.
fn unattributed_label(bucket: usize) -> &'static str {
    match bucket {
        RUN_WITHOUT_PROCESS => "egress.run-without-process",
        NO_SCOPE => "egress.no-scope",
        index => Unattributed::ALL[index].code(),
    }
}

/// Every unattributable tally with its label, for a support bundle or a test.
#[must_use]
pub fn unattributed_egress_bytes() -> Vec<(&'static str, u64)> {
    (0..UNATTRIBUTED_BUCKETS)
        .map(|bucket| {
            (
                unattributed_label(bucket),
                UNATTRIBUTED_EGRESS[bucket].load(Ordering::Relaxed),
            )
        })
        .collect()
}

/// Where a counted byte goes.
///
/// Resolved **once per request**, not once per frame, and that is a correctness
/// property as much as a cost one. Reading the task-local per frame would charge a
/// body that is consumed after the scope has been left — a response handed to a
/// detached task, which this tree does routinely — to nobody, or worse to whatever
/// scope that task happens to be in. Binding the destination when the request is
/// made means the bytes follow the request that asked for them.
#[derive(Clone)]
enum Charge {
    /// A process row exists and these are its bytes.
    Process(run_scope::ProcessScope),
    /// One of the [`UNATTRIBUTED_EGRESS`] tallies.
    Unattributed(usize),
}

impl Charge {
    /// Reads the ambient scope and picks the destination.
    fn resolve() -> Self {
        if let Some(process) = run_scope::current_process() {
            return Charge::Process(process);
        }
        Charge::Unattributed(match run_scope::current() {
            None => NO_SCOPE,
            Some(scope) => match scope.unattributed() {
                // `position` cannot miss: `ALL` is complete by construction (see
                // `run_scope`'s own test). `NO_SCOPE` is the fallback purely so
                // this is not an `unwrap` on a hot path.
                Some(reason) => Unattributed::ALL
                    .iter()
                    .position(|candidate| *candidate == reason)
                    .unwrap_or(NO_SCOPE),
                None => RUN_WITHOUT_PROCESS,
            },
        })
    }

    fn add(&self, bytes: u64) {
        match self {
            Charge::Process(process) => process.charge_egress(bytes),
            Charge::Unattributed(bucket) => {
                UNATTRIBUTED_EGRESS[*bucket].fetch_add(bytes, Ordering::Relaxed);
            }
        }
    }

    /// Notes where an allowed request went, so the ledger records destinations
    /// and not only volume.
    ///
    /// Both cases are recorded now. The attributed one hangs off the process row;
    /// the unattributed one goes to [`UNATTRIBUTED_DESTINATIONS`], keyed by the
    /// same reason its bytes are already counted under, so "how much" and "where"
    /// for a given reason are two readings of one vocabulary rather than two
    /// vocabularies.
    ///
    /// # The lock this takes, and why it is affordable
    ///
    /// `UNATTRIBUTED_EGRESS` is a fixed array of atomics precisely because
    /// [`Self::add`] runs **per body frame** — an SSE stream calls it thousands of
    /// times — and a map there would be a global lock on the hottest path in the
    /// app. This is not that path: a destination is noted **once per request**,
    /// beside a DNS lookup and a TLS handshake, and the map is bounded at
    /// [`run_scope::MAX_DESTINATIONS`] so the critical section is a `BTreeMap`
    /// probe over at most 128 entries. The split is deliberate rather than
    /// incidental: volume stays lock-free, destinations pay a lock they can
    /// afford.
    fn note_destination(&self, url: &Url) {
        // Both are absent for the same kind of url — one with no authority, like
        // `data:` — and neither names a destination on its own.
        let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
            return;
        };
        let host = host.to_ascii_lowercase();
        match self {
            Charge::Process(process) => process.note_destination(url.scheme(), &host, port),
            Charge::Unattributed(bucket) => {
                note_unattributed_destination(*bucket, url.scheme(), &host, port)
            }
        }
    }
}

/// Records one redirect hop after the caller's redirect policy has accepted it.
///
/// [`send`] records the initial request before handing it to reqwest, but reqwest
/// follows redirects inside `Client::execute`, where `send` cannot see the later
/// URLs. Redirect policies call this only after every policy guard succeeds, so a
/// refused hop remains a denial record and never appears among allowed
/// destinations.
pub(crate) fn note_allowed_redirect_destination(url: &Url) {
    Charge::resolve().note_destination(url);
}

/// Where egress that belongs to no run went, by the reason it had none.
///
/// One log per [`UNATTRIBUTED_EGRESS`] bucket, so the destination list and the
/// byte tally are indexed the same way and a reader correlating them never has to
/// map between two orders.
///
/// A `Mutex` rather than atomics, unlike the byte tallies beside it, because a set
/// of hosts is not a number. See [`Charge::note_destination`] for why the lock is
/// affordable here and would not be there.
static UNATTRIBUTED_DESTINATIONS: std::sync::Mutex<Option<Box<UnattributedDestinations>>> =
    std::sync::Mutex::new(None);

/// The per-bucket logs, boxed so the static costs one pointer until first use.
///
/// Allocated lazily for that reason and no other: a `monkey` subcommand that makes
/// no unattributed request should not pay for 7 maps to prove it.
struct UnattributedDestinations {
    seen: [BTreeMap<run_scope::Destination, u64>; UNATTRIBUTED_BUCKETS],
    overflowed: [u64; UNATTRIBUTED_BUCKETS],
}

impl Default for UnattributedDestinations {
    fn default() -> Self {
        UnattributedDestinations {
            seen: std::array::from_fn(|_| BTreeMap::new()),
            overflowed: [0; UNATTRIBUTED_BUCKETS],
        }
    }
}

/// Adds one request to `bucket`'s log, or to its overflow once the cap is reached.
///
/// The cap is [`run_scope::MAX_DESTINATIONS`] — the *same* ceiling a process gets,
/// and shared rather than re-picked, because the thing being bounded is the same
/// thing: a caller with no declared allowlist can be walked across arbitrarily many
/// hosts by the content it fetches. Requests past it are counted and not named,
/// never dropped: a truncated list that does not say it is truncated reads as a
/// complete one.
///
/// Poisoning is absorbed with `into_inner`, matching every other lock in this tree:
/// a panic elsewhere must not turn bookkeeping into a second panic.
fn note_unattributed_destination(bucket: usize, scheme: &str, host: &str, port: u16) {
    let Ok(mut guard) = UNATTRIBUTED_DESTINATIONS
        .lock()
        .or_else(|poisoned| Ok::<_, ()>(poisoned.into_inner()))
    else {
        return;
    };
    let logs = guard.get_or_insert_with(Box::<UnattributedDestinations>::default);
    let key = run_scope::Destination {
        scheme: scheme.to_string(),
        host: host.to_string(),
        port,
    };
    if let Some(requests) = logs.seen[bucket].get_mut(&key) {
        *requests += 1;
        return;
    }
    if logs.seen[bucket].len() >= run_scope::MAX_DESTINATIONS {
        logs.overflowed[bucket] += 1;
        return;
    }
    logs.seen[bucket].insert(key, 1);
}

/// Takes every unattributed destination noted since the last call, with its label.
///
/// A drain rather than a read, for [`run_scope::ProcessScope::take_destinations`]'s
/// reason: the ledger writer is additive, so a read that left the counts behind
/// would write them again on the next flush.
///
/// Buckets with nothing to report are absent rather than present and empty, so a
/// caller can skip a transaction entirely on the overwhelmingly common tick where
/// the app talked to nobody outside a run.
#[must_use]
pub fn take_unattributed_destinations() -> Vec<(&'static str, run_scope::DestinationDrain)> {
    let Ok(mut guard) = UNATTRIBUTED_DESTINATIONS
        .lock()
        .or_else(|poisoned| Ok::<_, ()>(poisoned.into_inner()))
    else {
        return Vec::new();
    };
    let Some(logs) = guard.as_mut() else {
        return Vec::new();
    };
    let mut drained = Vec::new();
    for bucket in 0..UNATTRIBUTED_BUCKETS {
        let seen: Vec<_> = std::mem::take(&mut logs.seen[bucket]).into_iter().collect();
        let overflowed = std::mem::replace(&mut logs.overflowed[bucket], 0);
        if seen.is_empty() && overflowed == 0 {
            continue;
        }
        drained.push((
            unattributed_label(bucket),
            run_scope::DestinationDrain { seen, overflowed },
        ));
    }
    drained
}

/// Puts a drain back, for a caller whose write of it failed.
///
/// The mirror of [`run_scope::ProcessScope::return_destinations`] and it inherits
/// that method's rule: a returned destination past the cap becomes overflow rather
/// than being silently dropped, because the cap is the invariant and a returned
/// count must not be able to breach it.
pub fn return_unattributed_destinations(label: &str, drain: run_scope::DestinationDrain) {
    let Some(bucket) = (0..UNATTRIBUTED_BUCKETS).find(|index| unattributed_label(*index) == label)
    else {
        return;
    };
    let Ok(mut guard) = UNATTRIBUTED_DESTINATIONS
        .lock()
        .or_else(|poisoned| Ok::<_, ()>(poisoned.into_inner()))
    else {
        return;
    };
    let logs = guard.get_or_insert_with(Box::<UnattributedDestinations>::default);
    logs.overflowed[bucket] += drain.overflowed;
    for (destination, requests) in drain.seen {
        if let Some(existing) = logs.seen[bucket].get_mut(&destination) {
            *existing += requests;
        } else if logs.seen[bucket].len() < run_scope::MAX_DESTINATIONS {
            logs.seen[bucket].insert(destination, requests);
        } else {
            logs.overflowed[bucket] += requests;
        }
    }
}

/// A body that counts what passes through it and passes it straight on.
///
/// # Why this is not a buffering wrapper
///
/// It would be a great deal easier to read the body to the end, count the length
/// and hand back the bytes — and it would break the two things this app most needs
/// a body for. An SSE stream never ends, so buffering it is a hang, not a delay;
/// and a model download is gigabytes, so buffering it is an OOM. So this is a
/// frame-by-frame passthrough: [`Self::poll_frame`] adds nothing to the pipeline
/// except an addition, retains no data, and forwards `size_hint` and
/// `is_end_stream` unchanged so nothing downstream can tell it is there. A test
/// below asserts the first frame arrives long before the last one is sent, which
/// is a test only a non-buffering implementation can pass.
///
/// # Why the count is of frames *delivered*
///
/// A caller may abandon a response — every byte cap in this tree does exactly that
/// once its ceiling is reached — and the bytes it never polled were never handed
/// over by the transport either. Counting per delivered frame therefore counts what
/// crossed the socket into this process, rather than the `Content-Length` the peer
/// promised or the subset the caller chose to keep.
struct Counted<B> {
    inner: B,
    charge: Charge,
}

impl<B> HttpBody for Counted<B>
where
    B: HttpBody + Unpin,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        // Both fields are `Unpin`, so the inner body can be re-pinned in place and
        // this needs no projection macro (and no new dependency for one).
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_frame(context);
        if let Poll::Ready(Some(Ok(frame))) = &polled {
            // Trailers carry no data and are not counted — see [`send`] on what
            // the unit excludes.
            if let Some(data) = frame.data_ref() {
                this.charge.add(data.remaining() as u64);
            }
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Sends a request and counts the bytes it moves, both ways.
///
/// # The unit, which is not the obvious one
///
/// Every number this module produces is **HTTP entity-body bytes, as they crossed
/// the socket, undecoded**. Precisely:
///
/// - *Counted*: the request body written out and the response body read in, in the
///   content coding the peer actually used.
/// - *Not counted*: request and response headers, chunked-transfer or HTTP/2 frame
///   overhead, TLS records, and TCP/IP. reqwest exposes none of those — its
///   connector's `Conn` is a sealed type, so there is no supported way to wrap the
///   socket itself — and a number that included some of them but not others would
///   be worse than one whose exclusions are written down.
///
/// "Undecoded" needs saying out loud because it is a property of this crate's
/// build rather than of the code here: `Cargo.toml` enables none of reqwest's
/// `gzip`/`brotli`/`zstd`/`deflate` features, so reqwest never decompresses a
/// response and the frames counted below are the compressed-on-the-wire bytes. If
/// one of those features is ever enabled, this count silently becomes *decoded*
/// bytes — larger than what the network carried — so enabling one means revisiting
/// this comment and the tests that pin the unit.
///
/// # Attribution
///
/// The destination is resolved here, under the caller's scope, so a
/// response whose body is read later — in a detached task, after the scope has
/// ended — still charges the process that asked for it. A request made with no
/// process in scope is counted under why it had none, never dropped and never
/// charged to a process that did not make it.
///
/// # What a caller gives up
///
/// Nothing at the call site: `client.get(url).send().await` becomes
/// `egress::send(client.get(url)).await` and the `Response` that comes back is an
/// ordinary one, with its url, status, headers and extensions intact.
///
/// ponytail: an in-memory body replayed across a redirect is counted once, not
/// once per hop. The redirect policies do record every allowed hop's destination,
/// but counting a replay would mean wrapping a reusable body, which makes it
/// unreusable (`reqwest::Body::try_clone` returns `None` for a streaming body) and
/// would break the same-origin redirect this module deliberately follows.
pub async fn send(request: reqwest::RequestBuilder) -> reqwest::Result<reqwest::Response> {
    let charge = Charge::resolve();
    let (client, built) = request.build_split();
    let mut built = built?;
    // Before anything is written, resolved or connected: the run's own frozen
    // allowlist. Checked on the built request's url rather than on whatever the
    // caller believes it asked for, the same reason `provider_endpoint_for_run`
    // reads the frozen endpoint instead of the claimed one.
    if let Err(denial) = check_run_allowlist(built.url()) {
        return Err(refusal_error(denial).await);
    }
    // After the guard, so this records what was *allowed* and never doubles up
    // with `denial_sink`'s record of what was not. Before the send rather than
    // after it, because a request that was permitted and then failed to connect
    // still reached for that host, and a destination list that omitted it would
    // be answering a different question than the one it is asked.
    charge.note_destination(built.url());
    if let Some(body) = built.body_mut().take() {
        // An in-memory body is counted by its length and left alone, so it stays
        // replayable across a redirect; only a streaming body is wrapped, which
        // costs it nothing because a streaming body was never replayable.
        let in_memory = body.as_bytes().map(|bytes| bytes.len() as u64);
        *built.body_mut() = Some(match in_memory {
            Some(length) => {
                charge.add(length);
                body
            }
            None => reqwest::Body::wrap(Counted {
                inner: body,
                charge: charge.clone(),
            }),
        });
    }
    let response = client.execute(built).await?;
    Ok(metered_by(response, charge))
}

/// Counts a response's body without having sent the request through [`send`].
///
/// For a caller that already has a `Response` in hand. Same unit and same
/// attribution rules; the request body is the caller's own to account for.
#[must_use]
pub fn metered(response: reqwest::Response) -> reqwest::Response {
    metered_by(response, Charge::resolve())
}

/// Rebuilds `response` around a [`Counted`] body.
///
/// The round trip through `http::Response` is the only way in: `reqwest::Response`
/// exposes no `body_mut`, so the body cannot be replaced in place. Url, status,
/// version, headers and extensions are all carried across — the url explicitly,
/// because reqwest keeps it beside the response rather than in it and the
/// conversion drops it.
fn metered_by(response: reqwest::Response, charge: Charge) -> reqwest::Response {
    use reqwest::ResponseBuilderExt;

    let url = response.url().clone();
    let (parts, body) = hyper::http::Response::<reqwest::Body>::from(response).into_parts();
    let mut builder = hyper::http::Response::builder()
        .status(parts.status)
        .version(parts.version);
    if let Some(headers) = builder.headers_mut() {
        *headers = parts.headers;
    }
    if let Some(extensions) = builder.extensions_mut() {
        extensions.extend(parts.extensions);
    }
    let rebuilt = builder
        .url(url)
        .body(reqwest::Body::wrap(Counted {
            inner: body,
            charge,
        }))
        // `http::response::Builder` only fails on a status, header or version it
        // could not accept, and every one of those came out of a response reqwest
        // had already parsed — there is nothing here for it to reject.
        .expect("a response reqwest parsed can be rebuilt from its own parts");
    reqwest::Response::from(rebuilt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::str::FromStr;

    /// The two halves of [`public_download_client`]'s guarantee, driven directly.
    ///
    /// # Why not through a fixture server
    ///
    /// Because the properties that matter cannot be shown that way. A fixture
    /// listens on loopback, so a test that drives a real request through the real
    /// client has to run in [`PublicDestinations::LoopbackAllowed`] — which is the
    /// one mode where loopback is *supposed* to be reachable. Such a test proves
    /// that redirects are followed; it says nothing about whether a public request
    /// could be walked onto this machine, which is the actual claim.
    ///
    /// Both enforcement points are reachable as values instead, which is why they
    /// were written as values: [`classify_public_destination`] is the per-hop
    /// verdict the redirect policy returns, and [`PublicOnlyResolver`] is the
    /// object reqwest asks for addresses — with its lookup injected, a name that
    /// answers `127.0.0.1` is something a test can state rather than something it
    /// has to own a domain to demonstrate.
    mod public_destinations {
        use super::*;

        fn hop(url: &str) -> Result<(), EgressDenial> {
            classify_public_destination(&Url::parse(url).expect("test URL parses"), Only)
        }

        use PublicDestinations::{LoopbackAllowed, Only};

        /// A hop out of a public chain must clear the same bar the endpoint did.
        /// Each case here is a way a `302` could have moved a request somewhere the
        /// user's URL could never have named, and the rule reported is the hop's own
        /// so a log says which one fired.
        #[test]
        fn a_public_hop_cannot_reach_anything_a_public_url_could_not() {
            for (url, expected) in [
                // Cleartext, at all, even to a public host: a downgrade is how a
                // network attacker gets to see and rewrite the rest of the chain.
                ("http://public.example/asset", EgressRule::SchemeNotAllowed),
                ("ftp://public.example/asset", EgressRule::SchemeNotAllowed),
                // The four spellings of "this machine".
                ("https://127.0.0.1/asset", EgressRule::Loopback),
                ("https://127.9.9.9/asset", EgressRule::Loopback),
                ("https://[::1]/asset", EgressRule::Loopback),
                ("https://localhost/asset", EgressRule::Loopback),
                ("https://anything.localhost/asset", EgressRule::Loopback),
                // The private network the machine sits on, including the address
                // every cloud metadata service answers on.
                ("https://10.0.0.1/asset", EgressRule::PrivateV4),
                ("https://192.168.1.1/asset", EgressRule::PrivateV4),
                ("https://172.16.0.1/asset", EgressRule::PrivateV4),
                ("https://169.254.169.254/asset", EgressRule::LinkLocal),
                ("https://[fe80::1]/asset", EgressRule::LinkLocal),
                ("https://[fc00::1]/asset", EgressRule::UniqueLocalV6),
                ("https://0.0.0.0/asset", EgressRule::Unspecified),
                ("https://[::]/asset", EgressRule::Unspecified),
                ("https://255.255.255.255/asset", EgressRule::Broadcast),
                ("https://239.0.0.1/asset", EgressRule::Multicast),
                // The two spellings that read as public and are not: a v4-mapped
                // and a NAT64-embedded loopback.
                ("https://[::ffff:127.0.0.1]/asset", EgressRule::Loopback),
                ("https://[64:ff9b::7f00:1]/asset", EgressRule::Loopback),
                ("https://[::127.0.0.1]/asset", EgressRule::Ipv4Compatible),
                // Not a destination rule, but a hop must not be able to introduce
                // either: a credential in the URL would be sent to whoever the
                // response named, and a fragment is never meaningful to a server.
                (
                    "https://user:secret@public.example/asset",
                    EgressRule::EmbeddedCredentials,
                ),
                (
                    "https://public.example/asset#frag",
                    EgressRule::FragmentNotAllowed,
                ),
            ] {
                let denial = hop(url).expect_err("{url} must be refused");
                assert_eq!(denial.rule(), expected, "wrong rule for {url}");
            }
        }

        /// The hop that has to keep working, and the reason the policy is not just
        /// "same origin": a release asset's only stable URL answers with a
        /// cross-origin redirect to a signed CDN URL carrying a query string.
        #[test]
        fn a_public_hop_to_another_public_host_is_followed() {
            hop("https://release-assets.example/github-production-release-asset/1/2?sig=abc&jwt=def")
                .expect("a cross-origin public HTTPS hop is the case this exists for");
            hop("https://public.example:8443/asset").expect("a non-default port is still public");
        }

        /// A catalog served from this machine may redirect within it — otherwise
        /// self-hosting is what broke — but the permission is loopback, not
        /// cleartext-anywhere. This is the composition the old implementation got
        /// right and is the one most easily lost when the flag becomes a bool named
        /// after the network rather than after the address class.
        #[test]
        fn a_loopback_endpoint_earns_loopback_and_not_the_open_network() {
            let hop = |url: &str| {
                classify_public_destination(
                    &Url::parse(url).expect("test URL parses"),
                    LoopbackAllowed,
                )
            };
            hop("http://127.0.0.1:8080/catalog.json").expect("loopback http is the whole point");
            hop("http://localhost:8080/catalog.json").expect("so is the name for it");
            hop("https://public.example/catalog.json").expect("and public HTTPS still passes");
            // The three that must still be refused: cleartext off this machine, the
            // LAN, and a credential.
            assert_eq!(
                hop("http://public.example/catalog.json")
                    .expect_err("a cleartext hop to another host is not loopback")
                    .rule(),
                EgressRule::SchemeNotAllowed
            );
            assert_eq!(
                hop("https://192.168.1.10/catalog.json")
                    .expect_err("the LAN is not this machine")
                    .rule(),
                EgressRule::PrivateV4
            );
            assert_eq!(
                hop("http://user:secret@127.0.0.1:8080/catalog.json")
                    .expect_err("loopback does not excuse a credential")
                    .rule(),
                EgressRule::EmbeddedCredentials
            );
        }

        fn answering(addresses: &'static [&'static str]) -> HostLookup {
            // A `HostLookup` is a plain `fn` pointer, so the answer has to come from
            // a `static` rather than from a captured argument. One indirection buys
            // a resolver that is otherwise exactly the production one.
            static ANSWERS: Mutex<Vec<SocketAddr>> = Mutex::new(Vec::new());
            *ANSWERS.lock().expect("answers lock") = addresses
                .iter()
                .map(|text| SocketAddr::new(text.parse().expect("test address parses"), 0))
                .collect();
            fn lookup(
                _host: String,
            ) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send>>
            {
                let answers = ANSWERS.lock().expect("answers lock").clone();
                Box::pin(async move { Ok(answers) })
            }
            lookup
        }

        async fn resolved(
            addresses: &'static [&'static str],
            destinations: PublicDestinations,
        ) -> Result<Vec<SocketAddr>, EgressRule> {
            let resolver = PublicOnlyResolver {
                destinations,
                guard: "test",
                lookup: answering(addresses),
            };
            match reqwest::dns::Resolve::resolve(
                &resolver,
                reqwest::dns::Name::from_str("catalog.test").expect("test name parses"),
            )
            .await
            {
                Ok(addresses) => Ok(addresses.collect()),
                Err(error) => Err(error
                    .downcast_ref::<EgressDenial>()
                    .expect("a refused answer arrives as an EgressDenial")
                    .rule()),
            }
        }

        /// The gap a URL check cannot close, closed at the only place that can: a
        /// name is what a rebind moves, and the addresses handed back here are
        /// exactly the ones reqwest connects to — not a hint compared against a
        /// second lookup that could answer differently.
        #[tokio::test]
        async fn a_name_that_resolves_into_this_machine_or_its_network_is_refused() {
            for (answers, expected) in [
                (&["127.0.0.1"][..], EgressRule::Loopback),
                (&["::1"][..], EgressRule::Loopback),
                (&["10.1.2.3"][..], EgressRule::PrivateV4),
                (&["192.168.0.5"][..], EgressRule::PrivateV4),
                (&["169.254.169.254"][..], EgressRule::LinkLocal),
                (&["fe80::1"][..], EgressRule::LinkLocal),
                (&["fc00::1"][..], EgressRule::UniqueLocalV6),
                (&["0.0.0.0"][..], EgressRule::Unspecified),
                (&["::ffff:127.0.0.1"][..], EgressRule::Loopback),
                (&["64:ff9b::7f00:1"][..], EgressRule::Loopback),
            ] {
                assert_eq!(
                    resolved(answers, Only).await.expect_err("must be refused"),
                    expected,
                    "wrong rule for {answers:?}"
                );
            }
            // An empty answer is its own fact, and not the same one as "every
            // answer is refused".
            assert_eq!(
                resolved(&[], Only).await.expect_err("nothing answered"),
                EgressRule::DnsNoAddresses
            );
        }

        /// The refusal is a prune, not an all-or-nothing verdict: an ordinary
        /// dual-stack or split-horizon host that answers with one public and one
        /// private address connects through the public one, and the private answer
        /// never reaches the connector.
        #[tokio::test]
        async fn a_mixed_answer_keeps_only_what_may_be_connected_to() {
            let allowed = resolved(&["10.0.0.1", "93.184.216.34"], Only)
                .await
                .expect("a public answer survives beside a private one");
            assert_eq!(
                allowed
                    .iter()
                    .map(|address| address.ip().to_string())
                    .collect::<Vec<_>>(),
                vec!["93.184.216.34"]
            );
        }

        /// The loopback exception reaches the resolver too, or a self-hosted
        /// catalog at `http://localhost:8080` would pass the URL check and then fail
        /// to connect. It reaches *only* loopback: the LAN is still pruned.
        #[tokio::test]
        async fn a_loopback_endpoint_may_resolve_to_loopback_and_nothing_further() {
            assert!(!resolved(&["127.0.0.1"], LoopbackAllowed)
                .await
                .expect("loopback resolves under the loopback exception")
                .is_empty());
            assert_eq!(
                resolved(&["10.0.0.1"], LoopbackAllowed)
                    .await
                    .expect_err("the LAN is not covered by it"),
                EgressRule::PrivateV4
            );
        }
    }

    mod rule {
        use super::*;
        use std::collections::BTreeSet;

        /// Every code, written out.
        ///
        /// # Why duplicate what the macro already declares
        ///
        /// Because a code is a persisted identifier, not an implementation
        /// detail. A denial sink will store these strings; renaming one orphans
        /// every row already written under the old name, and a rename is a
        /// one-character edit that reviews as a typo fix. Repeating the list here
        /// means such an edit fails `cargo test` with the before and after side
        /// by side, and updating it is an explicit statement that the orphaning
        /// is intended. The list is also the readable inventory of what this app
        /// will refuse, in one place, which the macro invocation is not.
        const EXPECTED_CODES: &[&str] = &[
            "egress.url-malformed",
            "egress.url-too-long",
            "egress.url-control-characters",
            "egress.scheme-not-allowed",
            "egress.embedded-credentials",
            "egress.fragment-not-allowed",
            "egress.host-missing",
            "egress.port-missing",
            "egress.origin-not-allowlisted",
            "egress.redirect-left-grant",
            "egress.subresource-left-grant",
            "egress.cleartext-not-allowed",
            "egress.loopback",
            "egress.unspecified",
            "egress.private-v4",
            "egress.link-local",
            "egress.unique-local-v6",
            "egress.multicast",
            "egress.broadcast",
            "egress.cgnat",
            "egress.this-network",
            "egress.protocol-assignments",
            "egress.test-net",
            "egress.benchmarking",
            "egress.reserved-range",
            "egress.ipv4-compatible",
            "egress.dns-resolution-failed",
            "egress.dns-answers-required",
            "egress.dns-no-addresses",
            "egress.redirect-hop-limit",
            "egress.redirect-cross-origin",
            "egress.redirect-origin-unknown",
            "egress.run-network-denied",
            "egress.run-host-not-allowlisted",
            "egress.run-port-not-allowlisted",
            "egress.run-protocol-not-allowlisted",
            "egress.run-policy-unavailable",
        ];

        #[test]
        fn every_code_is_exactly_what_was_published() {
            let actual: Vec<&str> = EgressRule::ALL.iter().map(|rule| rule.code()).collect();
            assert_eq!(
                actual, EXPECTED_CODES,
                "a rule code changed, was added, or was removed. Codes are the \
                 identity a denial sink persists: renaming one orphans every \
                 denial already recorded under the old code. If that is intended, \
                 update this list in the same commit."
            );
        }

        /// Two rules sharing a code would silently merge in any log or sink that
        /// groups by it — and with 32 hand-written strings, a copy-paste is the
        /// likeliest way it happens.
        #[test]
        fn no_two_rules_share_a_code() {
            let unique: BTreeSet<&str> = EgressRule::ALL.iter().map(|rule| rule.code()).collect();
            assert_eq!(
                unique.len(),
                EgressRule::ALL.len(),
                "duplicate rule code among {:?}",
                EgressRule::ALL
            );
        }

        /// Codes end up in logs, config and (eventually) a database column, so
        /// the shape has to be boring: one namespace, lowercase, dash-separated,
        /// nothing that needs quoting.
        #[test]
        fn codes_keep_a_shape_that_survives_a_log_and_a_column() {
            for rule in EgressRule::ALL {
                let code = rule.code();
                let suffix = code
                    .strip_prefix("egress.")
                    .unwrap_or_else(|| panic!("{code} must be namespaced `egress.`"));
                assert!(!suffix.is_empty(), "{code} has an empty suffix");
                assert!(
                    suffix
                        .chars()
                        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-'),
                    "{code} must be lowercase ASCII words joined by dashes"
                );
                assert!(
                    !suffix.starts_with('-') && !suffix.ends_with('-'),
                    "{code} has a dangling dash"
                );
            }
        }

        /// A summary that interpolated a request detail would defeat the split
        /// this type exists to make: the rule is the stable part, the detail is
        /// the per-request part, and a sink stores them in different columns.
        #[test]
        fn summaries_are_static_prose_with_nothing_interpolated() {
            for rule in EgressRule::ALL {
                let summary = rule.summary();
                assert!(!summary.is_empty(), "{} has no summary", rule.code());
                assert!(
                    !summary.contains('{') && !summary.contains('}'),
                    "{}'s summary looks like a format string: {summary}",
                    rule.code()
                );
                assert!(
                    !summary.ends_with('.'),
                    "{}'s summary is a clause, not a sentence: {summary}",
                    rule.code()
                );
            }
        }

        /// The rendering is the only thing an operator sees, so the code has to
        /// be in it — that is the whole difference between this and the prose it
        /// replaced.
        #[test]
        fn a_denial_always_renders_its_code() {
            let bare = EgressDenial::new(EgressRule::Loopback);
            assert_eq!(
                bare.to_string(),
                "the target is a loopback address [egress.loopback]"
            );

            let detailed = EgressDenial::about(EgressRule::Loopback, "127.0.0.1");
            assert_eq!(
                detailed.to_string(),
                "the target is a loopback address: 127.0.0.1 [egress.loopback]"
            );

            for rule in EgressRule::ALL {
                assert!(
                    EgressDenial::new(*rule).to_string().contains(rule.code()),
                    "{} rendered without its code",
                    rule.code()
                );
            }
        }

        #[test]
        fn the_rule_and_the_detail_stay_separable() {
            let denial = EgressDenial::about(EgressRule::OriginNotAllowlisted, "https://evil.test");
            assert_eq!(denial.rule(), EgressRule::OriginNotAllowlisted);
            assert_eq!(denial.detail(), Some("https://evil.test"));
            assert_eq!(EgressDenial::new(EgressRule::Loopback).detail(), None);
        }

        /// Exactly one rule may not name its target, and it is the one whose
        /// target is a secret. Written as an inventory over `ALL` rather than as
        /// a single assertion so that a new rule cannot quietly join the
        /// exemption.
        #[test]
        fn only_the_credentials_rule_hides_the_target_it_refused() {
            let redacting: Vec<&str> = EgressRule::ALL
                .iter()
                .filter(|rule| rule.redacts_target())
                .map(|rule| rule.code())
                .collect();
            assert_eq!(redacting, vec!["egress.embedded-credentials"]);
        }

        /// The set a private-network opt-in covers, written out, for the same reason
        /// the test above is an inventory: the risk is not that this returns the
        /// wrong answer for a rule somebody thought about, it is that a rule added
        /// later lands on the permissive side without anyone deciding it should.
        ///
        /// Pinned by code rather than by variant, because the code is the identity
        /// that outlives the spelling — and pinned as an ordered list over `ALL`, so
        /// the assertion also fails if a rule is *removed* from the enum entirely.
        #[test]
        fn a_private_network_grant_covers_exactly_these_rules() {
            let covered: Vec<&str> = EgressRule::ALL
                .iter()
                .filter(|rule| rule.covered_by_private_network_grant())
                .map(|rule| rule.code())
                .collect();
            assert_eq!(
                covered,
                vec![
                    "egress.loopback",
                    "egress.unspecified",
                    "egress.private-v4",
                    "egress.link-local",
                    "egress.unique-local-v6",
                    "egress.cgnat",
                ]
            );
        }

        /// The other half, and the one that is a security claim rather than a
        /// convenience: these classes stay refused however the setting is set.
        ///
        /// `egress.ipv4-compatible` is the one worth naming out loud. It is not a
        /// class of destination but an alternative *spelling* of one, so covering it
        /// with a private-network grant would not have widened the reachable network
        /// — it would have provided a second way to write any address at all past
        /// the classifier that refuses it.
        #[test]
        fn a_private_network_grant_never_covers_a_range_nothing_routes_to() {
            for rule in [
                EgressRule::Multicast,
                EgressRule::Broadcast,
                EgressRule::ThisNetwork,
                EgressRule::ProtocolAssignments,
                EgressRule::TestNet,
                EgressRule::Benchmarking,
                EgressRule::ReservedRange,
                EgressRule::Ipv4Compatible,
            ] {
                assert!(
                    !rule.covered_by_private_network_grant(),
                    "{} must stay refused whatever the private-network setting says",
                    rule.code()
                );
            }
        }

        /// A denial handed to `io::Error` must arrive as itself, not as a
        /// sentence. This is what lets a caller on the far side of reqwest ask
        /// *which rule* fired instead of substring-matching prose — the defect
        /// this whole type exists to remove.
        #[test]
        fn a_denial_survives_being_boxed_into_an_io_error() {
            let error = refused(EgressDenial::about(
                EgressRule::RedirectCrossOrigin,
                "https://a.test:443 to https://b.test:443",
            ));
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);

            let inner = error
                .into_inner()
                .expect("the denial was passed as an error, not as a string");
            let denial = inner
                .downcast_ref::<EgressDenial>()
                .expect("an `EgressDenial` must be recoverable from the far side");
            assert_eq!(denial.rule(), EgressRule::RedirectCrossOrigin);
            assert_eq!(
                denial.detail(),
                Some("https://a.test:443 to https://b.test:443")
            );
        }
    }

    #[test]
    fn an_ipv4_compatible_address_is_recognised_whatever_it_wraps() {
        for text in [
            // The one that walked past all four guards.
            "::127.0.0.1",
            "::7f00:1",
            // A private v4 and a *public* v4 in the same wrapper: the point is the
            // wrapper, not what it contains, so a v4 blocklist cannot be relied on.
            "::10.0.0.1",
            "::93.184.216.34",
        ] {
            let address = Ipv6Addr::from_str(text).expect("parses");
            assert!(
                is_ipv4_compatible(&address),
                "{text} must be recognised as IPv4-compatible"
            );
        }
    }

    /// The counter-test that matters most: `::` and `::1` are *not* reported here,
    /// because their own rules already name them. Without this, folding them in
    /// would look like a stricter guard while actually hiding which rule fired.
    #[test]
    fn unspecified_and_loopback_are_left_to_their_own_rules() {
        assert!(!is_ipv4_compatible(&Ipv6Addr::UNSPECIFIED));
        assert!(!is_ipv4_compatible(&Ipv6Addr::LOCALHOST));

        // Worth pinning rather than reasoning about: the dotted forms `::0.0.0.0`
        // and `::0.0.0.1` are not separate addresses at all, they ARE `::` and
        // `::1`. A first draft of the test above listed `::0.0.0.1` as a case this
        // should catch, which is self-contradictory.
        assert_eq!(
            Ipv6Addr::from_str("::0.0.0.1").unwrap(),
            Ipv6Addr::LOCALHOST
        );
        assert_eq!(
            Ipv6Addr::from_str("::0.0.0.0").unwrap(),
            Ipv6Addr::UNSPECIFIED
        );
    }

    #[test]
    fn ordinary_addresses_are_not_mistaken_for_the_deprecated_form() {
        for text in [
            // IPv4-*mapped*, which the existing `to_ipv4_mapped()` unwrap handles.
            "::ffff:127.0.0.1",
            "::ffff:93.184.216.34",
            // Real public, unique-local, link-local.
            "2606:2800:220:1:248:1893:25c8:1946",
            "fc00::1",
            "fe80::1",
        ] {
            let address = Ipv6Addr::from_str(text).expect("parses");
            assert!(
                !is_ipv4_compatible(&address),
                "{text} must not be treated as IPv4-compatible"
            );
        }
    }

    /// `64:ff9b::7f00:1` is `127.0.0.1`, and three of the four guards called it public.
    ///
    /// Asserted as an *unwrap* and not as a refusal, which is the difference from
    /// [`is_ipv4_compatible`]: the caller re-checks the extracted address against its
    /// own v4 blocklist, so this predicate must hand back the right v4 address rather
    /// than a verdict. Every embedded form the guards already refuse is listed, because
    /// the claim is that NAT64 is a *spelling* of them and not a new class.
    #[test]
    fn the_nat64_prefix_yields_the_address_it_embeds() {
        for (text, expected) in [
            ("64:ff9b::7f00:1", "127.0.0.1"),
            ("64:ff9b::a00:1", "10.0.0.1"),
            ("64:ff9b::c0a8:101", "192.168.1.1"),
            ("64:ff9b::a9fe:a9fe", "169.254.169.254"),
            ("64:ff9b::", "0.0.0.0"),
            // A public one, which must unwrap just the same — the caller's own v4
            // rule is what decides, and for this address it decides "allowed". If
            // this predicate refused its range instead, this is the case that would
            // break a v6-only network reaching a v4-only host.
            ("64:ff9b::5db8:d822", "93.184.216.34"),
        ] {
            let address = Ipv6Addr::from_str(text).expect("parses");
            assert_eq!(
                nat64_embedded_ipv4(&address),
                Some(Ipv4Addr::from_str(expected).expect("parses")),
                "{text} embeds {expected}"
            );
        }
    }

    /// The counter-test, and the reason the prefix check is a full /96 rather than the
    /// first two segments: "starts with `64:ff9b`" is not the same as "is the
    /// well-known NAT64 prefix", and a predicate that confused them would hand a
    /// caller four bytes taken from the middle of an unrelated global-unicast address
    /// and let its v4 blocklist decide policy on them.
    #[test]
    fn addresses_that_merely_resemble_the_nat64_prefix_are_not_unwrapped() {
        for text in [
            // Right first two segments, non-zero in bits 48..96 — a network-specific
            // prefix shape, which cannot be recognised from the address alone.
            "64:ff9b:0:0:1::7f00:1",
            "64:ff9b::1:7f00:1",
            // One segment off in each direction.
            "64:ff9a::7f00:1",
            "65:ff9b::7f00:1",
            // The forms handled by their own branches, which must not be claimed here.
            "::ffff:127.0.0.1",
            "::127.0.0.1",
            "::1",
            "::",
            // Ordinary addresses.
            "2606:2800:220:1:248:1893:25c8:1946",
            "fc00::1",
            "fe80::1",
        ] {
            let address = Ipv6Addr::from_str(text).expect("parses");
            assert_eq!(
                nat64_embedded_ipv4(&address),
                None,
                "{text} is not the well-known NAT64 prefix"
            );
        }
    }

    /// Pins the trap in the rejected fix, so nobody re-introduces it: `to_ipv4()`
    /// really does turn loopback into an address every IPv4 blocklist permits.
    #[test]
    fn the_rejected_to_ipv4_shortcut_would_have_allowed_loopback() {
        let unwrapped = Ipv6Addr::LOCALHOST
            .to_ipv4()
            .expect("`to_ipv4` matches the compatible range, which is the trap");
        assert_eq!(unwrapped, Ipv4Addr::new(0, 0, 0, 1));
        assert!(
            !unwrapped.is_loopback(),
            "this is why the shortcut is unsafe"
        );
        assert!(!unwrapped.is_private());
        assert!(!unwrapped.is_link_local());
        assert!(!unwrapped.is_unspecified());
    }

    /// The origin rules, driven directly rather than through a socket.
    ///
    /// `reqwest::redirect::Attempt` has no public constructor, so the policy
    /// closure itself is only reachable by making a real request — which is why
    /// [`may_follow`] is a free function. The `hardened_*` tests below prove the
    /// closure is actually wired to it; these prove what it decides.
    mod origin_rules {
        use super::*;

        fn url(text: &str) -> Url {
            Url::parse(text).expect("test URL parses")
        }

        #[test]
        fn a_hop_that_stays_on_the_same_origin_is_followed() {
            for (from, to) in [
                (
                    "https://api.example.com/v1/models",
                    "https://api.example.com/v2/models",
                ),
                // Explicit default port on one side only: `port_or_known_default`
                // is what makes these the same origin.
                ("https://api.example.com/a", "https://api.example.com:443/b"),
                (
                    "http://127.0.0.1:1234/v1",
                    "http://127.0.0.1:1234/v1/models",
                ),
            ] {
                assert!(
                    may_follow(&url(from), &url(to)),
                    "{from} -> {to} is the same origin and must still be followed"
                );
            }
        }

        /// The hole this whole policy exists for: `x-api-key` is not on
        /// reqwest's strip list, so a hop to a new host would carry it.
        #[test]
        fn a_hop_to_a_different_host_or_port_is_refused() {
            for (from, to) in [
                ("https://api.example.com/v1", "https://evil.example.net/v1"),
                // Same registrable domain, different host: still a different
                // server, still a credential leak.
                ("https://api.example.com/v1", "https://logs.example.com/v1"),
                // The loopback case: an unauthenticated local runtime.
                (
                    "https://api.example.com/v1",
                    "http://127.0.0.1:11434/api/tags",
                ),
                ("http://127.0.0.1:1234/v1", "http://127.0.0.1:8080/v1"),
            ] {
                assert!(
                    !may_follow(&url(from), &url(to)),
                    "{from} -> {to} leaves the origin and must be refused"
                );
            }
        }

        #[test]
        fn an_http_to_https_upgrade_on_the_same_authority_is_followed() {
            for (from, to) in [
                ("http://api.example.com/v1", "https://api.example.com/v1"),
                (
                    "http://api.example.com:80/v1",
                    "https://api.example.com:443/v1",
                ),
                (
                    "http://api.example.com:8080/v1",
                    "https://api.example.com:8080/v1",
                ),
            ] {
                assert!(
                    may_follow(&url(from), &url(to)),
                    "{from} -> {to} is the ordinary TLS upgrade and must be followed"
                );
            }
        }

        /// The counter-test for the upgrade allowance, and the reason it is
        /// written as a directional test instead of "compare host and port,
        /// ignore the scheme": that shortcut would pass this case too, quietly
        /// moving an API key from a TLS connection onto a cleartext one.
        #[test]
        fn an_https_to_http_downgrade_is_refused() {
            for (from, to) in [
                ("https://api.example.com/v1", "http://api.example.com/v1"),
                (
                    "https://api.example.com:443/v1",
                    "http://api.example.com:80/v1",
                ),
                (
                    "https://api.example.com:8080/v1",
                    "http://api.example.com:8080/v1",
                ),
            ] {
                assert!(
                    !may_follow(&url(from), &url(to)),
                    "{from} -> {to} is a TLS downgrade and must be refused"
                );
            }
        }

        #[test]
        fn an_upgrade_that_also_changes_the_port_is_refused() {
            assert!(!may_follow(
                &url("http://api.example.com:8080/v1"),
                &url("https://api.example.com:9090/v1")
            ));
        }

        /// Two hostless URLs must not compare equal just because both hosts are
        /// `None`.
        #[test]
        fn a_hostless_url_has_no_origin_to_match() {
            assert!(!may_follow(
                &url("data:text/plain,a"),
                &url("data:text/plain,b")
            ));
            assert!(!may_follow(
                &url("https://api.example.com/v1"),
                &url("data:text/plain,b")
            ));
        }

        /// The diagnostic must name the origin and nothing else — these paths
        /// put tokens in query strings.
        #[test]
        fn the_diagnostic_label_drops_the_path_and_query() {
            assert_eq!(
                origin_label(&url("https://relay.example.com/exchange?handoff=secret")),
                "https://relay.example.com:443"
            );
            assert_eq!(
                origin_label(&url("http://127.0.0.1:1234/v1/chat/completions")),
                "http://127.0.0.1:1234"
            );
        }
    }

    /// Socket-level proof that [`hardened`] wires up what the doc comments claim.
    ///
    /// Offline by construction: every peer here is a `std::net::TcpListener` on
    /// `127.0.0.1:0` in this process. CI has no guaranteed network, and these
    /// tests are about bytes on a socket anyway — a peer that accepts and then
    /// says nothing, and a peer that must never be contacted at all.
    /// The default silence budget must be a floor, never a ceiling. `mcp.rs` has a
    /// per-server tool timeout that users may set above it, and cutting such a
    /// server off at the default would turn a supported configuration into a
    /// failure.
    #[test]
    fn a_callers_own_budget_is_never_tightened_by_the_default() {
        let configured = Duration::from_secs(900);
        assert_eq!(
            configured.max(READ_TIMEOUT),
            configured,
            "a longer explicit budget must win"
        );

        // And the inverse: a shorter configured budget does not weaken the floor,
        // so an MCP server with a 5s tool timeout still gets the full default for
        // its long-lived notification stream.
        assert_eq!(
            Duration::from_secs(5).max(READ_TIMEOUT),
            READ_TIMEOUT,
            "the default is a floor, so a shorter budget must not lower it"
        );
    }

    mod wiring {
        use super::*;
        use crate::run_scope::RunScope;
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Instant;

        /// What a [`FakeHost`] does with the n-th connection it accepts.
        enum Answer {
            /// Read the request head, write these bytes, close.
            Raw(String),
            /// Accept and then hold the socket open, writing nothing. Holding
            /// rather than dropping is the whole point: dropping sends `FIN`,
            /// which the client reports as a connection error, and the test
            /// could then pass without any read timeout existing.
            Silence,
            /// Write the head, then one body byte every `gap` until `chunks` have
            /// gone out. Never silent for longer than `gap`, so a read budget
            /// wider than `gap` must let the whole body through however long the
            /// transfer takes in total — which is precisely what a *total*
            /// deadline does not do.
            Trickle { chunks: usize, gap: Duration },
        }

        /// A scripted loopback HTTP peer plus a count of how many connections it
        /// accepted.
        ///
        /// The accept loop is non-blocking with a deadline rather than a plain
        /// blocking `accept()` — copied from `server.rs`'s `SilentUpstream` for
        /// the same reason it needed it there: one test asserts that *nothing
        /// ever connects*, and a blocking accept would simply never return.
        struct FakeHost {
            origin: String,
            accepted: Arc<AtomicUsize>,
        }

        impl FakeHost {
            /// `answers` is consumed in order; the last entry repeats for any
            /// further connections, so a redirect-loop test needs only one.
            fn start(answers: Vec<Answer>) -> Self {
                let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback peer");
                listener
                    .set_nonblocking(true)
                    .expect("listener goes non-blocking");
                let port = listener.local_addr().expect("peer address").port();
                let accepted = Arc::new(AtomicUsize::new(0));

                let counter = Arc::clone(&accepted);
                // Detached: the harness tears the process down when the run
                // ends, and nothing here needs joining — the assertions read
                // `accepted`, which is shared, not the thread's return value.
                std::thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    let mut parked: Vec<std::net::TcpStream> = Vec::new();
                    let mut served = 0usize;
                    while Instant::now() < deadline {
                        match listener.accept() {
                            Ok((mut stream, _)) => {
                                counter.fetch_add(1, Ordering::SeqCst);
                                let answer = answers.get(served).or_else(|| answers.last());
                                served += 1;
                                match answer {
                                    Some(Answer::Raw(bytes)) => {
                                        // An accepted socket may inherit the
                                        // listener's non-blocking flag, which
                                        // would turn the read below into an
                                        // instant `WouldBlock`.
                                        let _ = stream.set_nonblocking(false);
                                        let _ =
                                            stream.set_read_timeout(Some(Duration::from_secs(2)));
                                        let mut head = [0u8; 2048];
                                        let _ = stream.read(&mut head);
                                        let _ = stream.write_all(bytes.as_bytes());
                                        let _ = stream.flush();
                                    }
                                    Some(Answer::Trickle { chunks, gap }) => {
                                        let _ = stream.set_nonblocking(false);
                                        let _ =
                                            stream.set_read_timeout(Some(Duration::from_secs(2)));
                                        let mut head = [0u8; 2048];
                                        let _ = stream.read(&mut head);
                                        let header = format!(
                                            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {chunks}\r\nConnection: close\r\n\r\n"
                                        );
                                        if stream.write_all(header.as_bytes()).is_ok() {
                                            for _ in 0..*chunks {
                                                std::thread::sleep(*gap);
                                                if stream.write_all(b"x").is_err()
                                                    || stream.flush().is_err()
                                                {
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    Some(Answer::Silence) | None => parked.push(stream),
                                }
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            Err(_) => break,
                        }
                    }
                    drop(parked);
                });

                Self {
                    origin: format!("http://127.0.0.1:{port}"),
                    accepted,
                }
            }

            fn accepted(&self) -> usize {
                self.accepted.load(Ordering::SeqCst)
            }

            /// The port, for a test that needs to aim a *hostname* at this fixture
            /// rather than its literal address — a resolved address's port is
            /// replaced by the URL's own, so the URL has to carry this one.
            fn port(&self) -> u16 {
                self.origin
                    .rsplit(':')
                    .next()
                    .and_then(|port| port.parse().ok())
                    .expect("the fixture origin ends in its port")
            }
        }

        fn redirect_to(location: &str) -> Answer {
            Answer::Raw(format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ))
        }

        fn ok_body(body: &str) -> Answer {
            Answer::Raw(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ))
        }

        /// Hole 3: a peer that completes the handshake and then goes quiet must
        /// not hold the caller open. Driven through
        /// [`hardened_with_timeouts`] because the production budget is ten
        /// minutes.
        #[tokio::test]
        async fn the_read_timeout_ends_a_request_the_peer_never_answers() {
            let host = FakeHost::start(vec![Answer::Silence]);
            let client = hardened_with_timeouts(CONNECT_TIMEOUT, Duration::from_millis(200))
                .build()
                .expect("client builds");

            let started = Instant::now();
            let result = client.get(&host.origin).send().await;

            assert!(
                result.is_err(),
                "a peer that never writes must not be waited on forever"
            );
            // The peer did accept, so this is a read timeout and not a refused
            // connection — an `Err` on its own would not distinguish them.
            assert_eq!(host.accepted(), 1, "the request must have reached the peer");
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "gave up after {:?}, which is not the 200ms read budget",
                started.elapsed()
            );
        }

        /// The distinction three clients in this tree had wrong: a total request
        /// deadline covers the **body**, so it aborts a transfer that is still
        /// making steady progress, while a read budget only ever measures silence.
        ///
        /// Both halves belong in one test, because either alone is misleading. The
        /// read budget letting a slow body through proves nothing on its own — the
        /// same trickle has to be shown failing once that identical duration is
        /// made a deadline for the whole request instead. That contrast is the
        /// entire reason `webdav_client`, `pinned_http_client` and the two model
        /// download clients set `read_timeout` rather than `timeout`.
        #[tokio::test]
        async fn a_total_deadline_aborts_a_trickling_body_where_a_read_budget_does_not() {
            // Every gap is comfortably inside `budget`, while the sum of them is
            // comfortably past it — the two margins the assertions below rest on.
            let gap = Duration::from_millis(50);
            let chunks = 12usize;
            let budget = Duration::from_millis(400);

            let patient = FakeHost::start(vec![Answer::Trickle { chunks, gap }]);
            let body = hardened_with_timeouts(CONNECT_TIMEOUT, budget)
                .build()
                .expect("client builds")
                .get(&patient.origin)
                .send()
                .await
                .expect("a body that keeps arriving is not a silent peer")
                .text()
                .await
                .expect("body reads");

            assert_eq!(
                body.len(),
                chunks,
                "the whole body must arrive: no single gap came near the {budget:?} read budget"
            );

            let truncating = FakeHost::start(vec![Answer::Trickle { chunks, gap }]);
            let result = hardened_with_timeouts(CONNECT_TIMEOUT, budget)
                .timeout(budget)
                .build()
                .expect("client builds")
                .get(&truncating.origin)
                .send()
                .await;
            // The deadline can land either while the headers are outstanding or
            // part-way through the body, so both steps count as the failure.
            let outcome = match result {
                Err(error) => Err(error),
                Ok(response) => response.text().await,
            };

            assert!(
                outcome.is_err(),
                "the identical trickle must fail once {budget:?} becomes a deadline for the whole request"
            );
        }

        /// Counter-test for the read timeout: "always fail" would pass the test
        /// above.
        #[tokio::test]
        async fn a_peer_that_answers_inside_the_budget_still_succeeds() {
            let host = FakeHost::start(vec![ok_body("hello")]);
            let client = hardened_with_timeouts(CONNECT_TIMEOUT, Duration::from_millis(500))
                .build()
                .expect("client builds");

            let body = client
                .get(&host.origin)
                .send()
                .await
                .expect("a prompt answer is not a timeout")
                .text()
                .await
                .expect("body reads");

            assert_eq!(body, "hello");
        }

        /// Holes 1 and 2, and the reason this asserts on a counter rather than
        /// on the `Err`: an `Err` alone cannot tell a policy refusal apart from
        /// a connection that was simply refused. The claim is that the redirect
        /// target is never *contacted*, so that is what is measured.
        #[tokio::test]
        async fn a_cross_origin_hop_is_refused_and_its_target_never_contacted() {
            let target = FakeHost::start(vec![ok_body("credential arrived")]);
            let entry = FakeHost::start(vec![redirect_to(&format!("{}/steal", target.origin))]);

            let client = hardened().build().expect("client builds");
            let result = client
                .get(&entry.origin)
                // The header the whole policy exists for: reqwest strips
                // `Authorization` across hosts but not this one.
                .header("x-api-key", "sk-do-not-forward-me")
                .send()
                .await;

            // Asserted before `result.is_err()` deliberately. Both hold when the
            // policy works, but when it does *not* the request succeeds, and a
            // failure that reads "the target was contacted" names the actual
            // defect where "the request should have failed" only names a symptom.
            assert_eq!(entry.accepted(), 1, "the first hop is legitimate");
            assert_eq!(
                target.accepted(),
                0,
                "the redirect target must never be contacted at all"
            );
            assert!(
                result.is_err(),
                "a refused hop must fail the whole request, not quietly return \
                 the pre-redirect response"
            );
        }

        /// Counter-test for the origin rule: "refuse every redirect" would pass
        /// the test above. Two connections because each answer closes the
        /// socket.
        #[tokio::test]
        async fn a_same_origin_redirect_is_still_followed() {
            let host = FakeHost::start(vec![redirect_to("/landed"), ok_body("landed")]);

            let client = hardened().build().expect("client builds");
            let process = run_scope::ProcessScope::new("p-redirect-destinations");
            let response = run_scope::scoped_with_process(
                RunScope::run("run:redirect-destinations"),
                process.clone(),
                send(client.get(&host.origin)),
            )
            .await
            .expect("a same-origin redirect must be followed");

            assert!(response.url().path().ends_with("/landed"));
            assert_eq!(response.text().await.expect("body reads"), "landed");
            assert_eq!(host.accepted(), 2, "one connection per hop");
            let destinations = process.take_destinations();
            assert_eq!(destinations.seen.len(), 1);
            assert_eq!(
                destinations.seen[0].1, 2,
                "the initial request and followed hop must both be accounted"
            );
        }

        /// The count, against a peer that says exactly how many bytes it wrote.
        ///
        /// A body big enough to arrive in several frames, on purpose: a wrapper
        /// that counted only the first frame, or only the last, passes a one-frame
        /// test and undercounts every real transfer. The frame count is asserted
        /// for the same reason — if the transport ever coalesced this into one
        /// frame the test would stop testing what it claims to.
        #[tokio::test]
        async fn counted_bytes_on_a_streamed_response_are_the_bytes_the_peer_sent() {
            use futures_util::StreamExt;

            const SENT: usize = 40_000;
            let host = FakeHost::start(vec![Answer::Trickle {
                chunks: SENT,
                gap: Duration::ZERO,
            }]);
            let client = hardened().build().expect("client builds");
            let process = run_scope::ProcessScope::new("p-turn-count");

            let (frames, received) = run_scope::scoped_with_process(
                RunScope::run("run:count"),
                process.clone(),
                async {
                    let response = send(client.get(&host.origin))
                        .await
                        .expect("the peer answers");
                    let mut stream = response.bytes_stream();
                    let (mut frames, mut received) = (0usize, 0usize);
                    while let Some(chunk) = stream.next().await {
                        frames += 1;
                        received += chunk.expect("a frame arrives").len();
                    }
                    (frames, received)
                },
            )
            .await;

            assert_eq!(received, SENT, "the peer's whole body must arrive");
            assert!(
                frames > 1,
                "a {SENT}-byte trickle arrived in {frames} frame(s); this test only \
                 means something if the body is delivered in several"
            );
            assert_eq!(
                process.take_egress(),
                SENT as u64,
                "the count must be the bytes the peer actually wrote"
            );
        }

        /// A request that got out names where it went, and a refused one does not.
        ///
        /// Both halves in one test on purpose: the value of the destination list
        /// is that it says what was *allowed*, so a denial leaking into it would
        /// make it answer a different question than the one it is asked.
        // The guard has to span the awaits: it is what serializes this test's
        // global policy source against every other test's, and releasing it
        // before the request is sent is the same as not taking it.
        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn an_allowed_request_names_its_destination_and_a_refused_one_does_not() {
            let _guard = crate::denial_sink::test_lock();
            let host = FakeHost::start(vec![ok_body("first"), ok_body("second")]);
            let client = hardened().build().expect("client builds");
            let process = run_scope::ProcessScope::new("p-turn-destinations");
            let target = Url::parse(&host.origin).expect("the fake host has a url");
            let port = target.port_or_known_default().expect("http has a default");

            run_scope::scoped_with_process(RunScope::run("run:destinations"), process.clone(), {
                let client = client.clone();
                let origin = host.origin.clone();
                async move {
                    for _ in 0..2 {
                        send(client.get(&origin)).await.expect("the peer answers");
                    }
                }
            })
            .await;

            let drain = process.take_destinations();
            assert_eq!(
                drain.seen,
                vec![(
                    run_scope::Destination {
                        scheme: "http".to_string(),
                        host: target.host_str().expect("the url has a host").to_string(),
                        port,
                    },
                    2,
                )],
                "two requests to one host are one destination with a count of two"
            );
            assert_eq!(drain.overflowed, 0);

            // A second run, under a policy that permits nothing this url is.
            // Not the fake host: it is loopback, which `is_loopback_target`
            // exempts ahead of the policy, so a refusal there is unreachable.
            let refused = run_scope::ProcessScope::new("p-turn-refused");
            let allowlist = crate::run_protocol::EgressAllowlist {
                hosts: vec!["nowhere.example.com".to_string()],
                ports: vec![443],
                protocols: vec!["https".to_string()],
            };
            install_run_policy_source(move |_| {
                RunEgressPolicy::Declared(Arc::new(allowlist.clone()))
            });
            run_scope::scoped_with_process(RunScope::run("run:refused"), refused.clone(), async {
                send(client.get("https://api.example.com/v1"))
                    .await
                    .expect_err("the allowlist refuses this host");
            })
            .await;
            clear_run_policy_source();
            assert!(
                refused.take_destinations().is_empty(),
                "a refusal belongs to `denial_sink`, not to the allowed-destination list"
            );
        }

        /// A cap that abandons a response still counts what crossed the wire.
        ///
        /// The claim is not "the whole `Content-Length`" — the peer never got to
        /// send the rest — it is "everything that was handed over", which is what
        /// the caller's own tally proves.
        #[tokio::test]
        async fn an_abandoned_response_still_counts_what_crossed_the_wire() {
            use futures_util::StreamExt;

            let host = FakeHost::start(vec![Answer::Trickle {
                chunks: 20_000,
                gap: Duration::ZERO,
            }]);
            let client = hardened().build().expect("client builds");
            let process = run_scope::ProcessScope::new("p-turn-capped");

            let received = run_scope::scoped_with_process(
                RunScope::run("run:capped"),
                process.clone(),
                async {
                    let response = send(client.get(&host.origin))
                        .await
                        .expect("the peer answers");
                    let mut stream = response.bytes_stream();
                    // One frame, then walk away — exactly what
                    // `knowledge_service.rs` and `mcp_app_core.rs` do at their
                    // ceilings.
                    let first = stream
                        .next()
                        .await
                        .expect("at least one frame")
                        .expect("a frame arrives")
                        .len();
                    drop(stream);
                    first
                },
            )
            .await;

            assert!(
                received > 0,
                "the test needs the peer to have sent something"
            );
            assert_eq!(
                process.take_egress(),
                received as u64,
                "an abandoned body must be counted for what it delivered, neither \
                 zero nor the length the peer promised"
            );
        }

        /// Bytes with no process to charge are recorded under why they had none.
        ///
        /// Three distinct answers, and keeping them apart is the point: an
        /// uninstrumented call site, deliberately unattributed work, and a run
        /// whose process row nobody resolved are three different findings, and one
        /// shared bucket would make all three unreadable.
        #[tokio::test]
        async fn a_request_with_no_process_in_scope_is_counted_under_why_it_had_none() {
            fn tally(label: &str) -> u64 {
                unattributed_egress_bytes()
                    .into_iter()
                    .find(|(name, _)| *name == label)
                    .map(|(_, bytes)| bytes)
                    .expect("every label has a tally")
            }
            async fn fetch(origin: &str) {
                let client = hardened().build().expect("client builds");
                let body = send(client.get(origin))
                    .await
                    .expect("the peer answers")
                    .text()
                    .await
                    .expect("body reads");
                assert_eq!(body, "hello");
            }

            // No scope at all: nobody has instrumented this site.
            let host = FakeHost::start(vec![ok_body("hello")]);
            let before = tally("egress.no-scope");
            fetch(&host.origin).await;
            assert_eq!(
                tally("egress.no-scope") - before,
                5,
                "an uninstrumented request must still be counted somewhere"
            );

            // Deliberately no run, with the reason kept.
            let host = FakeHost::start(vec![ok_body("hello")]);
            let before = tally(Unattributed::Scheduled.code());
            run_scope::scoped(
                RunScope::Unattributed(Unattributed::Scheduled),
                fetch(&host.origin),
            )
            .await;
            assert_eq!(
                tally(Unattributed::Scheduled.code()) - before,
                5,
                "unattributed work keeps its own reason as its tally"
            );

            // A run, but no process row: legitimate, and not the same finding.
            let host = FakeHost::start(vec![ok_body("hello")]);
            let before = tally("egress.run-without-process");
            run_scope::scoped(RunScope::run("run:no-process"), fetch(&host.origin)).await;
            assert_eq!(
                tally("egress.run-without-process") - before,
                5,
                "a run with no process row must not be charged to another process"
            );
        }

        /// The streaming property, as a test that a buffering implementation fails.
        ///
        /// The peer writes its head at once and then one byte per `gap`, so the
        /// first frame is available five gaps before the last one is. A wrapper
        /// that read the body to the end to count it would deliver nothing until
        /// the whole transfer finished, and the first assertion below is what
        /// notices.
        #[tokio::test]
        async fn counting_a_body_does_not_buffer_it() {
            use futures_util::StreamExt;

            let gap = Duration::from_millis(100);
            let chunks = 5usize;
            let host = FakeHost::start(vec![Answer::Trickle { chunks, gap }]);
            let client = hardened().build().expect("client builds");
            let process = run_scope::ProcessScope::new("p-turn-stream");

            let (first_frame_at, whole_body_at) = run_scope::scoped_with_process(
                RunScope::run("run:stream"),
                process.clone(),
                async {
                    let response = send(client.get(&host.origin))
                        .await
                        .expect("the peer answers");
                    let started = Instant::now();
                    let mut stream = response.bytes_stream();
                    let _first = stream
                        .next()
                        .await
                        .expect("a first frame")
                        .expect("a frame arrives");
                    let first_frame_at = started.elapsed();
                    while let Some(chunk) = stream.next().await {
                        chunk.expect("a frame arrives");
                    }
                    (first_frame_at, started.elapsed())
                },
            )
            .await;

            assert!(
                first_frame_at < gap * (chunks as u32 - 1),
                "the first frame took {first_frame_at:?}, which is long enough that \
                 the body was collected before being handed over"
            );
            assert!(
                whole_body_at >= gap * (chunks as u32 - 1),
                "the transfer finished in {whole_body_at:?}, too fast for the peer \
                 to have trickled {chunks} bytes {gap:?} apart — the timing this \
                 test rests on is not what it thinks"
            );
            assert_eq!(process.take_egress(), chunks as u64);
        }

        /// The other direction: a request body is counted as it goes out, and the
        /// two halves add up in one tally.
        #[tokio::test]
        async fn a_request_body_is_counted_on_its_way_out() {
            let host = FakeHost::start(vec![ok_body("ok")]);
            let client = hardened().build().expect("client builds");
            let process = run_scope::ProcessScope::new("p-turn-upload");
            let payload = "x".repeat(1_000);

            run_scope::scoped_with_process(RunScope::run("run:upload"), process.clone(), async {
                let body = send(client.post(&host.origin).body(payload.clone()))
                    .await
                    .expect("the peer answers")
                    .text()
                    .await
                    .expect("body reads");
                assert_eq!(body, "ok");
            })
            .await;

            assert_eq!(
                process.take_egress(),
                payload.len() as u64 + 2,
                "the request body out plus the response body in"
            );
        }

        /// Counting rebuilds the response, so everything a caller reads off one has
        /// to survive the trip. The url is the one that does not come for free —
        /// reqwest keeps it beside the response rather than in it.
        #[tokio::test]
        async fn a_metered_response_keeps_its_url_status_and_headers() {
            let host = FakeHost::start(vec![Answer::Raw(
                "HTTP/1.1 201 Created\r\nContent-Type: text/plain\r\nX-Trace: abc\r\n\
                 Content-Length: 2\r\nConnection: close\r\n\r\nhi"
                    .to_string(),
            )]);
            let client = hardened().build().expect("client builds");
            let process = run_scope::ProcessScope::new("p-turn-shape");

            run_scope::scoped_with_process(RunScope::run("run:shape"), process.clone(), async {
                let response = send(client.get(format!("{}/path?q=1", host.origin)))
                    .await
                    .expect("the peer answers");
                assert_eq!(response.status().as_u16(), 201);
                assert_eq!(
                    response
                        .headers()
                        .get("x-trace")
                        .map(|value| value.as_bytes()),
                    Some(b"abc".as_ref())
                );
                assert_eq!(
                    response.url().as_str(),
                    format!("{}/path?q=1", host.origin),
                    "the url must survive being rebuilt around a counting body"
                );
                assert_eq!(response.text().await.expect("body reads"), "hi");
            })
            .await;

            assert_eq!(process.take_egress(), 2);
        }

        /// Serializes the tests that touch [`UNATTRIBUTED_DESTINATIONS`].
        ///
        /// It is process-wide state by design — unattributed traffic has no scope
        /// to hang a log off, which is the whole reason this exists — so tests
        /// that drain it would otherwise take each other's rows under cargo's
        /// default parallelism. Poisoning is absorbed so one failing test reports
        /// its own assertion rather than poisoning every sibling into a second,
        /// misleading failure.
        static UNATTRIBUTED_LOG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        fn exclusive_log() -> std::sync::MutexGuard<'static, ()> {
            let guard = UNATTRIBUTED_LOG_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Start from a known state rather than from whatever ran before.
            let _ = take_unattributed_destinations();
            guard
        }

        /// The gap this slice closed, end to end: a request made under a *reason*
        /// rather than a run now names where it went.
        ///
        /// Before this, `UNATTRIBUTED_EGRESS` reported "how much, by reason" and
        /// nothing at all about "where", so "which hosts does the app itself
        /// reach outside a run" had no answer. This runs a real request under
        /// `Unattributed::UserAction` and reads the host back out of the same
        /// table the attributed case writes to.
        #[tokio::test]
        async fn a_request_with_no_run_records_its_destination_under_its_reason() {
            use crate::process_table::ProcessTable;
            use crate::run_ledger::RunLedger;

            let _guard = exclusive_log();

            let host = FakeHost::start(vec![ok_body("hi")]);
            let client = hardened().build().expect("client builds");
            run_scope::scoped(RunScope::Unattributed(Unattributed::UserAction), async {
                send(client.get(&host.origin))
                    .await
                    .expect("the peer answers")
                    .text()
                    .await
                    .expect("body reads");
            })
            .await;

            let drained = take_unattributed_destinations();
            let (label, drain) = drained
                .iter()
                .find(|(label, _)| *label == Unattributed::UserAction.code())
                .expect("the request was logged under the reason it had no run");
            // Identified by *this* host's port rather than by being the only
            // entry. `exclusive_log` clears the sink on the way in, but the sink
            // is process-wide and every other test in this binary that makes a
            // scope-less request writes to it while this one runs — the closing
            // comment below already says exactly that. `seen.len() == 1`
            // therefore held only while the suite was small enough that nothing
            // else landed inside the window, and began failing at four entries
            // once the binary gained more parallel tests.
            //
            // The port makes this stricter than the count ever was: a `FakeHost`
            // binds its own, so this now asserts that the destination recorded
            // under the reason is the one *this* request went to, which a
            // neighbouring test's entry could previously have satisfied.
            let (destination, requests) = drain
                .seen
                .iter()
                .find(|(destination, _)| destination.port == host.port())
                .expect("this request's destination is recorded under its reason");
            assert_eq!(destination.host, "127.0.0.1");
            assert_eq!(destination.scheme, "http");
            assert_eq!(*requests, 1);
            assert_eq!(drain.overflowed, 0);

            let ledger = RunLedger::open_in_memory().expect("an in-memory ledger opens");
            let table = ProcessTable::new(ledger.connection());
            table
                .add_unattributed_egress_destinations(label, drain, 2_000)
                .expect("the destination lands");

            let stored = table
                .unattributed_egress_destinations()
                .expect("the ledger reads back");
            let recorded = stored
                .get(Unattributed::UserAction.code())
                .expect("the reason has a destination list");
            let destination = recorded
                .destinations
                .iter()
                .find(|destination| destination.port == host.port())
                .expect("this request's destination round-trips through the ledger");
            assert_eq!(destination.host, "127.0.0.1");
            assert_eq!(destination.scheme, "http");
            assert_eq!(destination.requests, 1);
            assert_eq!(recorded.dropped, 0);

            // The drain is a drain: a second one reports nothing *for this
            // reason*, so the additive writer cannot double-count. Scoped to the
            // reason rather than asserting global emptiness, because other tests
            // in this binary make scope-less requests into the same
            // process-wide log — which is exactly the property that makes it
            // useful in production and awkward in a test.
            assert!(
                take_unattributed_destinations()
                    .iter()
                    .all(|(label, _)| *label != Unattributed::UserAction.code()),
                "a drained reason must not report its destinations twice"
            );
        }

        /// The two vocabularies are one vocabulary. A reader correlating "how
        /// much left under this reason" with "where it went" must be matching on
        /// the same strings, or the correlation is a guess.
        #[test]
        fn destinations_and_byte_tallies_share_the_same_reason_labels() {
            let _guard = exclusive_log();
            for bucket in 0..UNATTRIBUTED_BUCKETS {
                note_unattributed_destination(bucket, "https", "example.test", 443);
            }
            let drained = take_unattributed_destinations();
            let destination_labels: Vec<&str> = drained.iter().map(|(label, _)| *label).collect();
            let tally_labels: Vec<&str> = unattributed_egress_bytes()
                .into_iter()
                .map(|(label, _)| label)
                .collect();
            assert_eq!(
                destination_labels, tally_labels,
                "every tally must have a destination log under the same label, in the same order"
            );
            // And they are the persisted `Unattributed` codes, not a second set
            // invented here.
            for reason in Unattributed::ALL {
                assert!(
                    destination_labels.contains(&reason.code()),
                    "{} is missing from the destination labels",
                    reason.code()
                );
            }
        }

        /// The cap is the same one a process gets, and past it requests are
        /// counted rather than dropped: a truncated list that does not say it is
        /// truncated reads as a complete one.
        #[test]
        fn the_unattributed_log_is_bounded_and_counts_what_it_could_not_name() {
            let _guard = exclusive_log();
            let bucket = 0;
            for index in 0..run_scope::MAX_DESTINATIONS + 7 {
                note_unattributed_destination(bucket, "https", &format!("host-{index}.test"), 443);
            }
            let drained = take_unattributed_destinations();
            let (_, drain) = drained
                .iter()
                .find(|(label, _)| *label == unattributed_label(bucket))
                .expect("the bucket reported");
            assert_eq!(drain.seen.len(), run_scope::MAX_DESTINATIONS);
            assert_eq!(drain.overflowed, 7, "the excess is counted, never dropped");
        }

        /// A failed write must delay the destinations to the next tick rather than
        /// destroy them — the same contract `ProcessScope::return_destinations`
        /// has, including that a return cannot breach the cap.
        #[test]
        fn a_returned_drain_is_recounted_and_still_respects_the_cap() {
            let _guard = exclusive_log();
            let label = Unattributed::Startup.code();
            let bucket = (0..UNATTRIBUTED_BUCKETS)
                .find(|index| unattributed_label(*index) == label)
                .expect("startup has a bucket");

            note_unattributed_destination(bucket, "https", "a.test", 443);
            note_unattributed_destination(bucket, "https", "a.test", 443);
            let drained = take_unattributed_destinations();
            let (_, drain) = drained
                .into_iter()
                .find(|(candidate, _)| *candidate == label)
                .expect("the bucket reported");
            assert_eq!(drain.seen[0].1, 2);

            return_unattributed_destinations(label, drain);
            let again = take_unattributed_destinations();
            let (_, drain) = again
                .into_iter()
                .find(|(candidate, _)| *candidate == label)
                .expect("the returned drain is there to take again");
            assert_eq!(drain.seen[0].1, 2, "a return must not lose the counts");

            // A return into a full log becomes overflow rather than breaching the
            // ceiling the cap exists to hold.
            for index in 0..run_scope::MAX_DESTINATIONS {
                note_unattributed_destination(bucket, "https", &format!("full-{index}.test"), 443);
            }
            return_unattributed_destinations(label, drain);
            let final_drain = take_unattributed_destinations();
            let (_, drain) = final_drain
                .into_iter()
                .find(|(candidate, _)| *candidate == label)
                .expect("the bucket reported");
            assert_eq!(drain.seen.len(), run_scope::MAX_DESTINATIONS);
            assert_eq!(drain.overflowed, 2);
        }

        /// The whole path, ending in the column: bytes counted off a socket reach
        /// `agent_processes.bytes_egressed` through the additive writer, and a row
        /// nobody reported egress for keeps its `NULL`.
        #[tokio::test]
        async fn counted_bytes_reach_the_ledger_row_through_add_egress_bytes() {
            use crate::process_table::{
                AdmitProcess, ProcessKind, ProcessTable, ProcessUsageFilter,
            };
            use crate::run_ledger::RunLedger;

            fn stored_egress(table: &ProcessTable<'_>, process_id: &str) -> Option<u64> {
                table
                    .usage_rows(&ProcessUsageFilter {
                        process_id: Some(process_id.to_string()),
                        ..ProcessUsageFilter::default()
                    })
                    .expect("the ledger row reads")
                    .pop()
                    .expect("the row exists")
                    .usage
                    .measured()
                    .bytes_egressed
            }

            let ledger = RunLedger::open_in_memory().expect("an in-memory ledger opens");
            let table = ProcessTable::new(ledger.connection());
            let record = table
                .admit(
                    &AdmitProcess::new(ProcessKind::BackgroundShell, "bg-egress".to_string()),
                    1_000,
                )
                .expect("a row is admitted");
            let untouched = table
                .admit(
                    &AdmitProcess::new(ProcessKind::BackgroundShell, "bg-quiet".to_string()),
                    1_000,
                )
                .expect("a second row is admitted");

            let host = FakeHost::start(vec![ok_body("hello")]);
            let client = hardened().build().expect("client builds");
            let process = run_scope::ProcessScope::new(record.process_id.clone());

            run_scope::scoped_with_process(RunScope::run("run:ledger"), process.clone(), async {
                send(client.get(&host.origin))
                    .await
                    .expect("the peer answers")
                    .text()
                    .await
                    .expect("body reads");
            })
            .await;

            // What the scope owner does on its own schedule: drain and add. Twice,
            // to prove the drain does not re-report the same bytes.
            for _ in 0..2 {
                let counted = process.take_egress();
                if counted > 0 {
                    table
                        .add_egress_bytes(&record.process_id, counted, 2_000)
                        .expect("the row takes the bytes");
                }
            }

            assert_eq!(
                stored_egress(&table, &record.process_id),
                Some(5),
                "the body's five bytes must land in the column exactly once"
            );
            assert_eq!(
                stored_egress(&table, &untouched.process_id),
                None,
                "a process nobody reported egress for keeps its NULL rather than \
                 being credited with a measured zero"
            );
        }

        /// The hop cap. `Policy::custom` does not inherit reqwest's loop cap, and
        /// since same-origin hops *are* followed, a self-redirect would otherwise
        /// run until the read timeout — ten minutes, in production.
        #[tokio::test]
        async fn a_same_origin_redirect_loop_is_capped_rather_than_followed_forever() {
            let host = FakeHost::start(vec![redirect_to("/again")]);

            let client = hardened().build().expect("client builds");
            let result = client.get(&host.origin).send().await;

            assert!(
                result.is_err(),
                "an endless same-origin redirect must be capped"
            );
            assert!(
                host.accepted() <= MAX_REDIRECT_HOPS + 1,
                "followed {} hops, which is past the {MAX_REDIRECT_HOPS}-hop cap",
                host.accepted()
            );
        }

        /// K5's per-run allowlist: the deny-by-default half, its two deliberate
        /// fail-open cases, and its one fail-closed case.
        ///
        /// Every test here installs a **process-wide** policy source, so they all take
        /// `denial_sink::test_lock()` — the same lock and for the same reason that
        /// module's own installing tests take it — and clear the source afterwards.
        mod allowlist {
            use super::*;
            use crate::run_protocol::EgressAllowlist;
            use std::sync::Arc;

            fn declaring(hosts: &[&str], ports: &[u16], protocols: &[&str]) -> EgressAllowlist {
                let list = EgressAllowlist {
                    hosts: hosts.iter().map(|host| (*host).to_string()).collect(),
                    ports: ports.to_vec(),
                    protocols: protocols.iter().map(|p| (*p).to_string()).collect(),
                };
                list.validate().expect("the fixture must be a legal policy");
                list
            }

            /// A source that answers for exactly one run and `Unknown` for anything
            /// else, so a leaked install cannot decide another test's request.
            fn install_declared(run_id: &str, allowlist: EgressAllowlist) {
                let run_id = run_id.to_string();
                install_run_policy_source(move |asked| {
                    if asked == run_id {
                        RunEgressPolicy::Declared(Arc::new(allowlist.clone()))
                    } else {
                        RunEgressPolicy::Unknown
                    }
                });
            }

            fn verdict_for(run_id: &str, url: &str) -> Result<(), EgressDenial> {
                let url = Url::parse(url).expect("test url parses");
                run_scope::scoped_sync(RunScope::run(run_id), || run_allowlist_verdict(&url))
            }

            fn rule_for(run_id: &str, url: &str) -> Option<EgressRule> {
                verdict_for(run_id, url).err().map(|denial| denial.rule())
            }

            /// The empty declaration is the whole safety property: present-and-empty
            /// denies, where absent permits. Both asserted here so the two cannot
            /// quietly become one behaviour.
            #[test]
            fn an_empty_declaration_denies_and_an_absent_one_does_not() {
                let _guard = crate::denial_sink::test_lock();
                install_declared("run:empty", declaring(&[], &[], &[]));

                assert_eq!(
                    rule_for("run:empty", "https://api.example.com/v1"),
                    Some(EgressRule::RunHostNotAllowlisted),
                    "an empty allowlist must permit nothing"
                );

                install_run_policy_source(|_| RunEgressPolicy::Undeclared);
                verdict_for("run:silent", "https://api.example.com/v1")
                    .expect("a run that declares nothing keeps today's behaviour");

                clear_run_policy_source();
            }

            #[test]
            fn a_host_a_port_and_a_protocol_are_each_refused_by_their_own_rule() {
                let _guard = crate::denial_sink::test_lock();
                install_declared(
                    "run:narrow",
                    declaring(&["api.example.com"], &[443], &["https"]),
                );

                verdict_for("run:narrow", "https://api.example.com/v1/messages")
                    .expect("the declared destination must be permitted");
                assert_eq!(
                    rule_for("run:narrow", "https://other.example.com/v1"),
                    Some(EgressRule::RunHostNotAllowlisted)
                );
                assert_eq!(
                    rule_for("run:narrow", "https://api.example.com:8443/v1"),
                    Some(EgressRule::RunPortNotAllowlisted)
                );
                // Port 80 is not declared either, so the protocol case has to name a
                // declared port explicitly to be a test of the protocol.
                install_declared(
                    "run:narrow",
                    declaring(&["api.example.com"], &[443], &["https"]),
                );
                assert_eq!(
                    rule_for("run:narrow", "http://api.example.com:443/v1"),
                    Some(EgressRule::RunProtocolNotAllowlisted)
                );

                clear_run_policy_source();
            }

            /// **K17 S3's acceptance, at the only layer this repo can prove it.**
            ///
            /// The slice's own wording: *a run placed with a host allowlist is
            /// refused by the node when it reaches outside it, proven by the
            /// node's own denial record — not by the submitter's.* The two
            /// halves that could break it are both here.
            ///
            /// First, the policy has to *survive the trip*. A placed spec
            /// becomes a recipe on the node, and a recipe has nowhere to put an
            /// allowlist — so the snapshot is round-tripped through the exact
            /// JSON the node writes and the child reads, and the allowlist is
            /// taken from the far side of that trip. A conversion that dropped
            /// it would leave `declared` empty here and every assertion below
            /// would pass vacuously against `Undeclared`, which is why the
            /// permitted case is asserted too.
            ///
            /// Second, it has to be *enforced by this process*. Before K17 no
            /// headless `monkey-cli task run` installed a policy source at all,
            /// so a frozen allowlist was enforced in the desktop app and ignored
            /// in the daemon's own children. `task::run_inner` now installs one
            /// from the spec it just froze and runs the turn inside
            /// `RunScope::run`; this reproduces both steps.
            ///
            /// What this is not: proof across two machines. There is one host
            /// here, and the wire is exercised by the API tests in
            /// `daemon/remote/api.rs`. This proves the enforcement that the wire
            /// delivers work to.
            #[test]
            fn a_placed_runs_travelled_allowlist_is_enforced_by_the_node_that_received_it() {
                let _guard = crate::denial_sink::test_lock();

                let allowlist = declaring(&["api.example.com"], &[443], &["https"]);
                let mut spec = crate::node_placement::tests_support::placement_spec("run:placed");
                spec.permission_policy.egress_allowlist = Some(allowlist);

                // The trip: spec -> snapshot -> the recipe JSON the node writes
                // -> the snapshot the executing child reads back.
                let snapshot = crate::node_placement::PlacedRunSnapshot::from_spec(&spec);
                snapshot
                    .validate()
                    .expect("a placed snapshot must validate");
                let wire = serde_json::to_vec(&snapshot).expect("the snapshot serializes");
                let arrived: crate::node_placement::PlacedRunSnapshot =
                    serde_json::from_slice(&wire).expect("the snapshot survives the trip");
                let declared = arrived
                    .permission_policy
                    .egress_allowlist
                    .clone()
                    .expect("the allowlist must survive the conversion the node performs");

                // The install `task::run_inner` performs for its own frozen spec.
                install_declared("run:placed", declared);

                verdict_for("run:placed", "https://api.example.com/v1/messages").expect(
                    "the destination the submitter declared must still be reachable on the node",
                );
                assert_eq!(
                    rule_for("run:placed", "https://exfiltrate.example.net/v1"),
                    Some(EgressRule::RunHostNotAllowlisted),
                    "the node must refuse a destination outside the travelled allowlist"
                );
                // And the refusal is the node's own record, carrying the rule
                // that decided it rather than a generic failure.
                let denial = verdict_for("run:placed", "https://exfiltrate.example.net/v1")
                    .expect_err("the refusal is what is being recorded");
                assert!(
                    denial.rule().summary().len() > 8,
                    "a denial record must say which rule refused it"
                );

                clear_run_policy_source();
            }

            /// The suffix-matching bypass, which is why the matcher checks for the
            /// separator instead of trusting `ends_with`.
            #[test]
            fn a_wildcard_matches_at_a_label_boundary_and_nowhere_else() {
                assert!(allowlist_host_matches("*.example.com", "api.example.com"));
                assert!(allowlist_host_matches("*.example.com", "a.b.example.com"));
                assert!(
                    !allowlist_host_matches("*.example.com", "evil-example.com"),
                    "the classic suffix bypass"
                );
                assert!(
                    !allowlist_host_matches("*.example.com", "example.com"),
                    "the apex must be named in its own right"
                );
                assert!(
                    !allowlist_host_matches("*.example.com", ".example.com"),
                    "an empty label is not a subdomain"
                );
                assert!(allowlist_host_matches("example.com", "example.com"));
                assert!(!allowlist_host_matches("example.com", "api.example.com"));

                // And through the real verdict, since a matcher that is right in
                // isolation can still be called with the wrong argument.
                let _guard = crate::denial_sink::test_lock();
                install_declared(
                    "run:wild",
                    declaring(&["*.example.com"], &[443], &["https"]),
                );
                verdict_for("run:wild", "https://api.example.com/v1").expect("a subdomain");
                assert_eq!(
                    rule_for("run:wild", "https://evil-example.com/v1"),
                    Some(EgressRule::RunHostNotAllowlisted)
                );
                clear_run_policy_source();
            }

            /// Loopback stays reachable under a deny-all declaration, for
            /// [`is_loopback_target`]'s reason: a local-inference run legitimately
            /// talks to this machine.
            #[test]
            fn loopback_survives_a_deny_all_declaration() {
                let _guard = crate::denial_sink::test_lock();
                install_declared("run:local", declaring(&[], &[], &[]));

                for url in [
                    "http://127.0.0.1:11434/api/chat",
                    "http://localhost:8080/v1/chat/completions",
                    "http://[::1]:1234/v1",
                ] {
                    verdict_for("run:local", url)
                        .unwrap_or_else(|denial| panic!("{url} must stay reachable: {denial}"));
                }

                clear_run_policy_source();
            }

            /// Fail-closed, and the only case that does: a run is in scope, a source
            /// is installed, and the frozen policy cannot be read — so whether it
            /// declared anything is unknown and the request is refused.
            #[test]
            fn a_declared_policy_that_cannot_be_read_fails_closed() {
                let _guard = crate::denial_sink::test_lock();
                install_run_policy_source(|_| RunEgressPolicy::Unavailable);

                assert_eq!(
                    rule_for("run:blip", "https://api.example.com/v1"),
                    Some(EgressRule::RunPolicyUnavailable)
                );
                // Not cached: the next read must be attempted again rather than
                // remembering a transient failure as an answer.
                assert_eq!(
                    rule_for("run:blip", "https://api.example.com/v1"),
                    Some(EgressRule::RunPolicyUnavailable)
                );

                clear_run_policy_source();
            }

            /// The two fail-open cases, asserted with a source installed so the pass
            /// is about the *rules* and not about the source being absent.
            #[test]
            fn run_less_egress_behaves_exactly_as_it_did_before() {
                let _guard = crate::denial_sink::test_lock();
                // Deliberately hostile: this source would deny everything if it were
                // ever consulted for work with no run.
                install_run_policy_source(|_| {
                    RunEgressPolicy::Declared(Arc::new(EgressAllowlist::default()))
                });

                let url = Url::parse("https://api.example.com/v1").expect("parses");
                run_allowlist_verdict(&url).expect("no scope at all: today's behaviour");
                run_scope::scoped_sync(
                    RunScope::Unattributed(crate::run_scope::Unattributed::Scheduled),
                    || {
                        run_allowlist_verdict(&url)
                            .expect("deliberately run-less work keeps its egress")
                    },
                );

                clear_run_policy_source();
            }

            /// The frozen spec is immutable, so it is read once per run — a ledger
            /// read on every request's hot path is the thing this cache exists to
            /// avoid.
            #[test]
            fn a_frozen_policy_is_read_once_per_run_and_not_once_per_request() {
                let _guard = crate::denial_sink::test_lock();
                static READS: AtomicUsize = AtomicUsize::new(0);
                READS.store(0, Ordering::SeqCst);
                // Counted for this test's own run ids only. The source is
                // process-wide, so a neighbouring test's run-scoped request would
                // otherwise be counted here as well.
                install_run_policy_source(|asked| {
                    if !asked.starts_with("run:cached") {
                        return RunEgressPolicy::Unknown;
                    }
                    READS.fetch_add(1, Ordering::SeqCst);
                    RunEgressPolicy::Declared(Arc::new(EgressAllowlist {
                        hosts: vec!["api.example.com".to_string()],
                        ports: vec![443],
                        protocols: vec!["https".to_string()],
                    }))
                });

                for _ in 0..5 {
                    verdict_for("run:cached", "https://api.example.com/v1").expect("permitted");
                }
                assert_eq!(READS.load(Ordering::SeqCst), 1);

                // A different run is its own read, so the cache is keyed and not
                // global.
                verdict_for("run:cached-other", "https://api.example.com/v1").expect("permitted");
                assert_eq!(READS.load(Ordering::SeqCst), 2);

                clear_run_policy_source();
            }

            /// The choke point, end to end: a refused request must not reach its
            /// target, and the caller must be able to ask *which rule* refused it
            /// rather than substring-matching prose.
            ///
            /// The fake lookup points every name at the live fixture, so a missing
            /// check would connect — `accepted()` is what proves the refusal happened
            /// before the socket rather than after it.
            #[tokio::test]
            async fn a_denied_request_never_reaches_its_target_and_names_the_rule() {
                fn to_the_fixture(
                    _host: String,
                ) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send>>
                {
                    // Port `0`: reqwest substitutes the URL's own port.
                    Box::pin(async { Ok(vec![SocketAddr::from(([127, 0, 0, 1], 0))]) })
                }

                let _guard = crate::denial_sink::test_lock();
                let host = FakeHost::start(vec![ok_body("reached")]);
                install_declared(
                    "run:choke",
                    declaring(&["allowed.test"], &[host.port()], &["http"]),
                );

                let client = hardened_with_lookup(
                    Duration::from_millis(400),
                    Duration::from_secs(2),
                    to_the_fixture,
                )
                .build()
                .expect("client builds");

                // Entered *with* a process row, and every scope in this module does:
                // a run with no process charges its bytes to a process-wide tally that
                // a neighbouring test asserts exact numbers on, and tests run in
                // parallel.
                let error = run_scope::scoped_with_process(
                    RunScope::run("run:choke"),
                    crate::run_scope::ProcessScope::new("p-choke"),
                    send(client.get(format!("http://blocked.test:{}/", host.port()))),
                )
                .await
                .expect_err("a host the run never declared must be refused");

                assert_eq!(
                    host.accepted(),
                    0,
                    "the refusal must happen before anything is connected"
                );
                assert_eq!(
                    denial_in(&error).map(|denial| denial.rule()),
                    Some(EgressRule::RunHostNotAllowlisted),
                    "the rule must survive the trip through reqwest: {error}"
                );

                // The counter-test: the same client, the same fixture, the declared
                // host. Without it, "refuse everything" would pass the assertions
                // above.
                let response = run_scope::scoped_with_process(
                    RunScope::run("run:choke"),
                    crate::run_scope::ProcessScope::new("p-choke"),
                    send(client.get(format!("http://allowed.test:{}/", host.port()))),
                )
                .await
                .expect("the declared destination must still be reachable");
                assert_eq!(response.text().await.expect("body"), "reached");
                assert_eq!(host.accepted(), 1);

                clear_run_policy_source();
            }

            /// Walks a `reqwest::Error`'s source chain for a denial, the way
            /// `web.rs`'s helper does — `io::Error::source` delegates to its *inner*
            /// error's source, so the inner one has to be unwrapped explicitly.
            fn denial_in(error: &reqwest::Error) -> Option<&EgressDenial> {
                let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
                while let Some(step) = current {
                    if let Some(denial) = step.downcast_ref::<EgressDenial>() {
                        return Some(denial);
                    }
                    if let Some(denial) = step
                        .downcast_ref::<std::io::Error>()
                        .and_then(std::io::Error::get_ref)
                        .and_then(|inner| inner.downcast_ref::<EgressDenial>())
                    {
                        return Some(denial);
                    }
                    current = step.source();
                }
                None
            }

            /// DNS pinning, exercised through a real connection rather than by reading
            /// the pin table back.
            ///
            /// The lookup answers with the live fixture once and with `240.0.0.1` — a
            /// reserved range nothing routes to — every time after, which is a rebind
            /// in the only form a resolver can express one. The fixture answers
            /// `Connection: close`, so the second request cannot reuse the first
            /// connection: it must connect again, and `accepted() == 2` is what proves
            /// it did.
            #[tokio::test]
            async fn a_pinned_name_keeps_its_address_when_resolution_moves() {
                // Nothing here installs a policy source, but this makes a run-scoped
                // request to a *non-loopback* name, so it must not overlap a test that
                // has one installed — the source is process-wide and would decide this
                // request too.
                let _guard = crate::denial_sink::test_lock();
                static CALLS: AtomicUsize = AtomicUsize::new(0);

                fn rebinding(
                    _host: String,
                ) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send>>
                {
                    let first = CALLS.fetch_add(1, Ordering::SeqCst) == 0;
                    Box::pin(async move {
                        let address = if first {
                            [127, 0, 0, 1]
                        } else {
                            [240, 0, 0, 1]
                        };
                        Ok(vec![SocketAddr::from((address, 0))])
                    })
                }

                let host = FakeHost::start(vec![ok_body("pinned"), ok_body("pinned")]);
                let client = hardened_with_lookup(
                    Duration::from_millis(400),
                    Duration::from_secs(2),
                    rebinding,
                )
                .build()
                .expect("client builds");
                let url = format!("http://pinned.test:{}/", host.port());

                let (first, second) = run_scope::scoped_with_process(
                    RunScope::run("run:pin"),
                    crate::run_scope::ProcessScope::new("p-pin"),
                    async {
                        let first = send(client.get(&url)).await.expect("first request");
                        let first = first.text().await.expect("first body");
                        let second = send(client.get(&url)).await.expect("second request");
                        (first, second.text().await.expect("second body"))
                    },
                )
                .await;

                assert_eq!(first, "pinned");
                assert_eq!(
                    second, "pinned",
                    "the second connection must have used the pinned address"
                );
                assert_eq!(
                    host.accepted(),
                    2,
                    "the second request must be a new connection, or this proves nothing"
                );
                assert_eq!(
                    CALLS.load(Ordering::SeqCst),
                    1,
                    "a pinned name must not be resolved a second time at all"
                );
            }

            /// The counter-test that makes the pinning test mean something: with no run
            /// in scope there is nothing to pin to, the second resolution is taken, and
            /// the request lands on the moved address and fails. Same fixture, same
            /// lookup, one difference.
            #[tokio::test]
            async fn without_a_run_the_same_rebind_moves_the_second_request() {
                // Same reason as its sibling above, even though this one has no run: an
                // installed source cannot decide a run-less request, but sharing the
                // lock keeps the pair's timing comparable rather than only one of them
                // serialized.
                let _guard = crate::denial_sink::test_lock();
                static CALLS: AtomicUsize = AtomicUsize::new(0);

                fn rebinding(
                    _host: String,
                ) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send>>
                {
                    let first = CALLS.fetch_add(1, Ordering::SeqCst) == 0;
                    Box::pin(async move {
                        let address = if first {
                            [127, 0, 0, 1]
                        } else {
                            [240, 0, 0, 1]
                        };
                        Ok(vec![SocketAddr::from((address, 0))])
                    })
                }

                let host = FakeHost::start(vec![ok_body("unpinned"), ok_body("unpinned")]);
                let client = hardened_with_lookup(
                    Duration::from_millis(400),
                    Duration::from_secs(2),
                    rebinding,
                )
                .build()
                .expect("client builds");
                let url = format!("http://unpinned.test:{}/", host.port());

                // No *run*, which is the one difference from the test above — and a
                // process row all the same, so these bytes stay out of the
                // process-wide tallies a neighbouring test asserts on.
                let (first, moved) = run_scope::scoped_with_process(
                    RunScope::Unattributed(Unattributed::UserAction),
                    crate::run_scope::ProcessScope::new("p-unpinned"),
                    async {
                        let first = send(client.get(&url))
                            .await
                            .expect("first request")
                            .text()
                            .await
                            .expect("first body");
                        (first, send(client.get(&url)).await)
                    },
                )
                .await;
                assert_eq!(first, "unpinned");
                assert!(
                    moved.is_err(),
                    "with no run there is no pin, so the rebind must be followed"
                );
                assert_eq!(host.accepted(), 1, "the fixture must not be reached twice");
                assert_eq!(CALLS.load(Ordering::SeqCst), 2);
            }
        }
    }

    /// Ratchet: pins every client this tree constructs outside [`hardened`] —
    /// both spellings, `Client::new()` and `Client::builder()` — so a new one
    /// cannot be added without either routing it through [`hardened`] or writing
    /// down here why it does not need to be.
    ///
    /// # Why a source scan
    ///
    /// The defect class is "a new call site that looks fine in isolation", which
    /// no behavioural test can see. `server.rs`'s
    /// `no_serving_path_reaches_a_route_without_admission` is the same technique
    /// for the same reason.
    ///
    /// # Why not `include_str!`, which is what `server.rs` uses
    ///
    /// `server.rs` scans exactly one file — itself. This has to scan the whole
    /// tree, and `include_str!` needs a literal path per file, so the list of
    /// files scanned would be hard-coded. A bare client in a *new* file would
    /// then be invisible, and a new file is the likeliest way one actually
    /// arrives. Walking `CARGO_MANIFEST_DIR` costs a directory traversal and
    /// removes that blind spot. (Not clippy, either: this repo has no
    /// `clippy.toml`, no `[lints]`, and CI never runs it, so a lint would
    /// enforce nothing.)
    ///
    /// # Why both spellings
    ///
    /// This scan used to look for `Client::new()` alone, and pinned **5** bare
    /// production sites in four files while **30** `Client::builder()` chains
    /// stood next to them unseen. Both are the same defect class —
    /// a client built somewhere
    /// other than [`hardened`], with whatever budget and redirect policy its
    /// author happened to think of — so the count that matters is the sum, and a
    /// scanner that reads one spelling only reports a reassuring number rather
    /// than a true one.
    ///
    /// Counting builders does **not** mean every builder is wrong. Most are
    /// deliberate and several are stricter than [`hardened`] (a pinned runner
    /// certificate, `redirect::Policy::none()`). Listing them here pins the set
    /// rather than blessing it: the point is that a new one has to be argued for
    /// in this table instead of arriving silently.
    ///
    /// # What it still does not catch
    ///
    /// Whether a builder chain's budget is *proportionate*. That is a separate
    /// question with a separate ratchet —
    /// `no_new_total_request_deadline_can_be_added_unnoticed`, below — and the
    /// two deliberately do not share a verdict: a site can be legitimately
    /// hand-built and still carry the wrong kind of deadline.
    mod ratchet {
        /// Deliberately written without the `reqwest::` prefix so the
        /// `use reqwest::Client;` spelling cannot dodge the ratchet. The cost is
        /// that some *other* crate's `Client::new()` would also be counted; the
        /// remedy is to add it to `ALLOWED` with a note saying which crate it
        /// is, which is a fair price for closing the alias hole.
        const BARE_CLIENT: &str = "Client::new()";

        /// The other construction spelling, and the one this scan was blind to
        /// until now. Written without the `reqwest::` prefix for the same reason
        /// as [`BARE_CLIENT`], and it also matches this module's own
        /// [`hardened`](super::super::hardened) root, which is why `egress.rs`
        /// appears in [`ALLOWED`] rather than being special-cased out.
        const HAND_BUILT_CLIENT: &str = "Client::builder()";

        /// Production client-construction sites that are staying, and why.
        ///
        /// Each entry is `(path relative to src/, bare `Client::new()` count,
        /// `Client::builder()` count)`, in that order.
        ///
        /// Being in this table is a **pin, not an endorsement**. The bare-client
        /// entries are all loopback-only peers — this machine's own
        /// `llama-server`, `ollama`, or an LM-Studio-style runtime — with no
        /// credential to forward and no third party to forward it to. The builder
        /// entries are a mixed set, and the note on each says which it is:
        /// stricter than [`hardened`] on purpose, loopback-only, bounded at the
        /// application layer instead of at the client, or unable to adopt
        /// [`hardened`] without losing the feature.
        const ALLOWED: &[(&str, usize, usize)] = &[
            // Not a `reqwest` client at all: `matrix_sdk::Client::builder()`,
            // which the unprefixed scan cannot tell apart from one. The HTTP
            // client the SDK actually uses is handed to it by
            // `ClientBuilder::http_client`, and it is `hardened()` — so every
            // Matrix request goes through the same guard as everything else.
            // Two builders because learning which device an access token
            // belongs to needs a restored session, and the real session cannot
            // be restored until that answer is known.
            ("bin/monkey-cli/daemon/adapters/matrix.rs", 0, 2),
            // The readiness probe against the operator's own tunnel client, on
            // the loopback metrics port this daemon told that child to open.
            // Loopback by construction and carrying no credential: the tunnel's
            // token goes to the child in its environment and never near an HTTP
            // request. `hardened()` refuses loopback deliberately, which is why
            // it cannot be used here; the probe's own 3s deadline is audited
            // below.
            ("bin/monkey-cli/daemon/callback_exposure.rs", 0, 1),
            // The opt-in peer live-validation test's client, pinned to the
            // self-signed certificate the test mints for its own loopback
            // listener — the same `tls_certs_only` pin as the client below, and
            // reaching nothing but `127.0.0.1` on a port it just bound.
            ("bin/monkey-cli/daemon/peer_live.rs", 0, 1),
            // The daemon's client for a remote runner: `tls_certs_only` pins the
            // runner's certificate, plus `https_only`, a connect timeout and a
            // silence budget. Stricter than `hardened()`, which has no way to pin
            // a self-signed peer.
            ("bin/monkey-cli/daemon/remote/client.rs", 0, 1),
            // The CLI's local embedding endpoint.
            ("bin/monkey-cli/embed_cli.rs", 1, 0),
            // Favicon fetches for the user's own browsing pane. Its total deadline
            // is the one audited in `TOTAL_TIMEOUT_ALLOWED` below.
            ("browser_pane.rs", 0, 1),
            // Connector verification, `redirect::Policy::none()` and a total
            // deadline against a 64 KiB cap; audited below.
            ("connectors.rs", 0, 1),
            // Loopback `/health` probe; audited below.
            ("diagnostics.rs", 0, 1),
            // This module's own two: the `hardened()` root that every other site
            // is asked to start from, and `refusal_error`'s client, whose resolver
            // answers every name with an error — it exists to mint a
            // `reqwest::Error` and never opens a socket.
            ("egress.rs", 0, 2),
            // The readiness probe against Studio's own `sd-server` child, on a
            // loopback port this process reserved and handed it on its command
            // line. Loopback by construction, and not deadline-free: the probe
            // carries a 2s per-request timeout and the whole wait is bounded by
            // `READY_TIMEOUT`.
            ("generation.rs", 1, 0),
            // Studio's loopback `sd-server` job/cancel/capabilities clients plus
            // the tool-sidecar client, all to children this process spawned on
            // ports it reserved; deadlines audited below.
            ("generation_commands.rs", 0, 4),
            // The hosted image API (total deadline, audited below) and its ComfyUI
            // sibling, which bounds silence with `read_timeout` because its
            // `/history` poll and result download stream.
            ("generation_remote.rs", 0, 2),
            // Local stack runtimes. Lived in `stacks.rs` until the v1 registry and
            // embedding core were extracted for the D2 collapse; the client itself is
            // unchanged and still talks only to the loopback embedding runtime.
            ("knowledge_core.rs", 1, 0),
            // Bundled `llama-server` health/completion probes.
            ("llama.rs", 2, 0),
            // Both talk to `OLLAMA_ENDPOINT`/`LLAMA_ENDPOINT`, which are literals
            // on `127.0.0.1`. No client-level deadline: every call is wrapped in
            // `tokio::time::timeout(context.timeout_ms)` instead, which is the
            // bound the component-hub contract actually specifies.
            ("m3_production.rs", 0, 2),
            // OAuth token/revocation and the workflow client; deadlines audited
            // below.
            ("m4_runtime.rs", 0, 2),
            // One loopback Ollama review; audited below.
            ("m5_delivery/reviewer.rs", 0, 1),
            // Two audited totals below (image edit, transcription) plus the
            // ComfyUI download, which bounds silence rather than elapsed time.
            ("m7_companion.rs", 0, 3),
            // The model-download client, and the one site that **cannot** adopt
            // `hardened()`: `same_origin_redirect_policy` would refuse every
            // Hugging Face and Ollama-registry CDN redirect, which are cross-host
            // by construction. Its integrity guarantee is a SHA-256 check rather
            // than an origin pin, and reqwest strips `Authorization` cross-host
            // anyway. It sets a connect timeout, `egress::READ_TIMEOUT`, a hop cap
            // and its own per-hop SSRF check.
            ("model_sources.rs", 0, 1),
            // `download_to_file`, the same shape and the same reason, likewise on
            // `egress::READ_TIMEOUT`.
            ("models.rs", 0, 1),
            // The loopback Ollama daemon: `/api/version`, `/api/ps`, `/api/embed`;
            // deadlines audited below.
            ("ollama.rs", 0, 3),
            // `EndpointPolicy` gates this between `LoopbackOnly` and
            // `AllowRemoteHttps`. `redirect::Policy::none()`, and each operation is
            // bounded by `tokio::time::timeout` from `RuntimeOperationLimits`.
            ("runtime_adapter.rs", 0, 1),
            // Paged pull-request reads; audited below.
            ("runtime_pr_watcher.rs", 0, 1),
            // `bounded_loopback_client`: `no_proxy`, a connect timeout, a 30-minute
            // silence budget and no redirects. This is the loopback-inference half
            // of the forwarding client the roadmap records splitting — the cloud
            // half went to `hardened()`.
            ("server.rs", 0, 1),
        ];

        /// Drops test code, which is free to build any client it likes — it is
        /// talking to a listener it started itself.
        ///
        /// This used to split at the first `\n#[cfg(test)]` and treat the rest of
        /// the file as tests, on the stated grounds that no file had production
        /// code after its test module. That was false **in this very file**: `egress.rs` has two `#[cfg(test)]`
        /// modules with production code between and after them, so splitting at
        /// the first one hid 3,200 lines from the scan — including
        /// `refusal_error`'s own client, and, in `server.rs`,
        /// `bounded_loopback_client`. Two production sites, invisible, in the two
        /// files most concerned with egress. It cost nothing only because neither
        /// spelled the bare constructor.
        ///
        /// So each `#[cfg(test)]` block is dropped individually now. A block is
        /// recognised only when the attribute sits at column zero and the line
        /// after it opens a brace, which rustfmt guarantees for `mod tests {`.
        /// `#[cfg(test)] use …;`, test-only `static`s and multi-line test fn
        /// signatures are therefore left in — deliberately, because that
        /// over-counts rather than under-counts. An over-count fails loudly and
        /// gets a note in [`ALLOWED`]; an under-count is the exact defect this
        /// module exists to prevent.
        fn production_half(source: &str) -> String {
            let lines: Vec<&str> = source.lines().collect();
            let mut kept: Vec<&str> = Vec::new();
            let mut index = 0;
            while index < lines.len() {
                let opens_test_block = lines[index] == "#[cfg(test)]"
                    && lines
                        .get(index + 1)
                        .is_some_and(|next| next.trim_end().ends_with('{'));
                if opens_test_block {
                    index += 2;
                    while index < lines.len() && lines[index] != "}" {
                        index += 1;
                    }
                    index += 1;
                    continue;
                }
                kept.push(lines[index]);
                index += 1;
            }
            kept.join("\n")
        }

        /// Drops comment-only lines, so prose about a spelling is not counted as a
        /// use of it.
        ///
        /// Learnt twice, both times the hard way. `egress.rs`'s own doc comments
        /// talk around the bare constructor because naming it registered as a site,
        /// and the `ollama.rs` conversion broke this module's other ratchet for
        /// exactly that reason. Then the total-deadline scan below counted
        /// `web.rs`'s `search_client` doc comment — which mentions the builder only
        /// to say it deliberately does *not* use it — as a builder chain, and
        /// picked up a `.timeout(` thirty-odd lines further down as if it belonged
        /// to it.
        ///
        /// A ratchet that greps source text greps the prose explaining it, and the
        /// remedy is here rather than in every future doc comment. Only whole-line
        /// comments are dropped: a trailing `// note` after real code cannot
        /// introduce a false match, since the code on that line is kept either way.
        /// Block comments are not handled — this tree does not use them for API
        /// docs, and a `/* */` naming a constructor would still register.
        fn code_only(source: &str) -> String {
            source
                .lines()
                .map(|line| {
                    if line.trim_start().starts_with("//") {
                        ""
                    } else {
                        line
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        /// A `ClientBuilder` chain's own total-request deadline.
        ///
        /// Matched from either reqwest's builder spelling or this module's standard
        /// hardened builder. `.timeout(..)` on a
        /// *`RequestBuilder`* is a different thing and usually correct — a
        /// per-request deadline on one small buffered call, which `llama.rs` and
        /// `stacks.rs` use exactly right — and there is no way to tell the two
        /// apart from the substring alone. So the scan finds a client-builder root
        /// first and only looks inside the chain that follows it. The separately
        /// tuned `hardened_with_read_budget` root remains outside this narrow
        /// ratchet; the roadmap records that deferred widening.
        const BUILDER_TOTAL_TIMEOUT: &str = ".timeout(";

        fn starts_client_builder(line: &str) -> bool {
            line.contains("Client::builder()") || line.contains("crate::egress::hardened()")
        }

        /// How far past a client-builder root to keep looking for its own
        /// `.timeout(`. Every builder chain in this tree is far shorter than this;
        /// the window exists so the scan cannot run off into an unrelated
        /// function and count its per-request deadline.
        const CHAIN_WINDOW_LINES: usize = 14;

        /// Production client-builder chains that set a total deadline, and
        /// why each is allowed to.
        ///
        /// Paths are relative to `src/`. A total deadline covers the body, so it is
        /// only proportionate when the response is small and buffered. Auditing
        /// every site here turned up **three** distinct ways it goes wrong, not
        /// one, and the notes below say which applies:
        ///
        /// - **(A) A large download.** The truncation this rule is named for. Both
        ///   sites found were converted rather than listed — `remote/client.rs`'s
        ///   artifact fetch and `m7_companion`'s ComfyUI image download.
        /// - **(B) A large upload.** `ClientBuilder::timeout` covers *writing* the
        ///   request body too, which is easy to miss when only the response is
        ///   scored. Two sites carry a `MAX_MEDIA_BYTES` (256 MiB) multipart body
        ///   inside their deadline.
        /// - **(C) Work that is not network at all.** A `"stream": false` request
        ///   to a local model sends nothing until generation finishes, so the
        ///   deadline is a ceiling on *inference* and a slow model looks like a
        ///   transport failure.
        ///
        /// B and C are recorded rather than fixed: both need a product decision
        /// about what the ceiling should be, not a mechanical conversion, and a
        /// `read_timeout` alone would let a wedged local model hang forever.
        const TOTAL_TIMEOUT_ALLOWED: &[(&str, usize)] = &[
            // 3s on the tunnel client's loopback `/ready`, whose body is never
            // read at all, only `status()`. Nothing to truncate, and a probe
            // that hung would stall the supervisor's state reporting.
            ("bin/monkey-cli/daemon/callback_exposure.rs", 1),
            // 8s for two favicon candidates. `MAX_FAVICON_BYTES` (256 KiB) is
            // checked *after* `bytes()` has already buffered the body, so the
            // deadline is doing the byte cap's job — a separate defect from this
            // rule, recorded in the roadmap rather than fixed here.
            ("browser_pane.rs", 1),
            // 15s against `MAX_VERIFY_BYTES` (64 KiB), enforced both by a
            // `Content-Length` pre-check and a running total. 4.4 KB/s — the one
            // site where the cap and the deadline are plainly proportionate.
            ("connectors.rs", 1),
            // 1.5s on a loopback `/health` probe whose body is never read at all,
            // only `status()`. Nothing to truncate.
            ("diagnostics.rs", 1),
            // 120s on Studio's job client, 10s on its cancel and 10s on its
            // capabilities read, all loopback to the `sd-server` child. The
            // generation itself is not under any of them: the API is
            // submit-and-poll, so a deadline here only ever covers one round
            // trip, and the run is bounded by `JOB_STALL_TIMEOUT` (1h without
            // any movement) in the polling loop. The largest body is the terminal poll, which carries the
            // finished media base64 inside the JSON under `MAX_MEDIA_BYTES`
            // (256 MiB) — 2.8 MB/s across a loopback socket, and fully buffered
            // by `json()`, so there is no stream for the deadline to truncate.
            // The capabilities body is the smallest of the three: a few KB of
            // sampler and scheduler names, likewise buffered by `json()`.
            //
            // The fourth is `tool_client`, shared by both calls to a Studio tool
            // sidecar — also loopback, to a child this process spawned on a port
            // it reserved. 30s for the manifest, a page of JSON under
            // `MAX_MANIFEST_BYTES` (256 KiB). `TOOL_RUN_TIMEOUT` (300s) for a
            // run, and that one is (C): the tool contract is synchronous, so the
            // deadline is a ceiling on the *operation* — a face swap, a
            // segmentation — and not on transfer. Deliberately synchronous: these
            // take seconds, and a submit-and-poll protocol to avoid a deadline
            // nobody hits is protocol for its own sake. Neither body can outrun
            // its deadline unnoticed — both are read by `studio_tools::read_capped`,
            // which enforces its ceiling on a running total rather than trusting
            // `Content-Length`.
            ("generation_commands.rs", 4),
            // 1800s on the hosted image API, and it is (B): the body is
            // `MAX_IMAGE_BYTES` (32 MiB) of base64 inside a JSON object — 18 KB/s —
            // and the deadline also covers the provider's own render time, which is
            // what actually justifies the half hour rather than the transfer. Fully
            // buffered by `bytes()`, so there is no stream for it to truncate; but
            // the cap is checked *after* that buffer, so as in `browser_pane.rs` the
            // deadline is doing the byte cap's job. The sibling ComfyUI client is
            // not counted here — it bounds silence with `read_timeout` instead,
            // because its `/history` poll and result download do stream.
            ("generation_remote.rs", 1),
            // 30s for OAuth token/revocation JSON under a 1 MiB cap (35 KB/s), and
            // 120s on the workflow client — the second is (C): `run_model` posts
            // `"stream": false`, so that budget is a cap on how long a local model
            // may think.
            ("m4_runtime.rs", 2),
            // 900s for one loopback Ollama review, `"stream": false` under a 4 MiB
            // cap. (C) again, and the most generous ceiling in the tree — but a
            // ceiling on inference, not on transfer.
            ("m5_delivery/reviewer.rs", 1),
            // (B), both of them: 1800s on the image-edit multipart and 3600s on the
            // transcription upload, each carrying a body bounded only by
            // `MAX_MEDIA_BYTES` (256 MiB) — 149 KB/s and 74 KB/s respectively,
            // *plus* the provider's own render or transcription time.
            ("m7_companion.rs", 2),
            // 1.5s for `/api/version` (a tens-of-bytes object) and 10s for
            // `/api/ps`, both fully buffered. The third is 60s on `/api/embed`,
            // which is (C): `EMBED_BATCH_SIZE` caps the vector *count* at 32, not
            // the bytes and not the work, and a spec may declare up to 65,536 dims.
            ("ollama.rs", 3),
            // 20s per page against `MAX_RESPONSE_BYTES` (2 MiB) and `PER_PAGE` 30
            // pull requests: 105 KB/s, and at most `MAX_PAGES` = 3 requests each
            // with its own deadline.
            ("runtime_pr_watcher.rs", 1),
            // `fetch_impl`'s 30s under `MAX_BODY_BYTES` (5 MiB). Kept as a total
            // deliberately: it is a product ceiling as much as a safety net, since
            // a `web_fetch` the model is waiting on is useless once it is slower
            // than this — which is why the fetch path keeps one and the download
            // paths do not. The chain now inherits K5's connect and read budgets;
            // the remaining caveat is that `search_client`'s own 15s total is
            // **not** counted here because it starts from
            // `hardened_with_read_budget` rather than the builder this scan looks
            // for. That is the documented hole, and it is the one that will let a
            // future total deadline through.
            ("web.rs", 1),
        ];

        #[test]
        fn no_new_total_request_deadline_can_be_added_unnoticed() {
            let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
            let mut found: Vec<(String, usize)> = Vec::new();

            for entry in walkdir::WalkDir::new(&src)
                .into_iter()
                .filter_map(Result::ok)
            {
                if entry.path().extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                let source = std::fs::read_to_string(entry.path()).expect("source file reads");
                let scannable = code_only(&production_half(&source));
                let lines: Vec<&str> = scannable.lines().collect();
                let count = lines
                    .iter()
                    .enumerate()
                    .filter(|(index, line)| {
                        starts_client_builder(line)
                            && lines
                                .iter()
                                .skip(index + 1)
                                .take(CHAIN_WINDOW_LINES)
                                .take_while(|following| !starts_client_builder(following))
                                .any(|following| following.contains(BUILDER_TOTAL_TIMEOUT))
                    })
                    .count();
                if count > 0 {
                    let relative = entry
                        .path()
                        .strip_prefix(&src)
                        .expect("walked path is under src/")
                        .to_string_lossy()
                        .replace('\\', "/");
                    found.push((relative, count));
                }
            }
            found.sort();

            let expected: Vec<(String, usize)> = TOTAL_TIMEOUT_ALLOWED
                .iter()
                .map(|(file, count)| ((*file).to_string(), *count))
                .collect();

            assert_eq!(
                found, expected,
                "the set of client-builder chains setting their own total \
                 request deadline changed.\n\
                 `ClientBuilder::timeout` covers the response body, so on any path \
                 that streams (`bytes_stream()`, a `chunk()` loop) it truncates a \
                 legitimate transfer instead of protecting against a stalled one — \
                 bound silence with `read_timeout` and size with a byte cap \
                 instead, which is what `egress::hardened()` supplies. If the \
                 response really is small and fully buffered, add the site to \
                 `TOTAL_TIMEOUT_ALLOWED` with a comment naming the cap that makes \
                 the deadline proportionate. If a site disappeared, drop its entry."
            );
        }

        #[test]
        fn no_new_unpinned_client_construction_can_be_added_unnoticed() {
            let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
            let mut found: Vec<(String, usize, usize)> = Vec::new();

            for entry in walkdir::WalkDir::new(&src)
                .into_iter()
                .filter_map(Result::ok)
            {
                if entry.path().extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                let source = std::fs::read_to_string(entry.path()).expect("source file reads");
                let scannable = code_only(&production_half(&source));
                let bare = scannable.matches(BARE_CLIENT).count();
                let hand_built = scannable.matches(HAND_BUILT_CLIENT).count();
                if bare > 0 || hand_built > 0 {
                    let relative = entry
                        .path()
                        .strip_prefix(&src)
                        .expect("walked path is under src/")
                        .to_string_lossy()
                        .replace('\\', "/");
                    found.push((relative, bare, hand_built));
                }
            }
            found.sort();

            let expected: Vec<(String, usize, usize)> = ALLOWED
                .iter()
                .map(|(file, bare, hand_built)| ((*file).to_string(), *bare, *hand_built))
                .collect();

            // Name the files that moved, so the failure does not make the reader
            // diff two twenty-two-line vectors by eye to find the one that grew.
            let changed: std::collections::BTreeSet<&str> = found
                .iter()
                .filter(|entry| !expected.contains(entry))
                .chain(expected.iter().filter(|entry| !found.contains(entry)))
                .map(|(file, _, _)| file.as_str())
                .collect();
            let changed = changed.into_iter().collect::<Vec<_>>().join(", ");

            assert_eq!(
                found, expected,
                "client construction outside `egress::hardened()` changed in: \
                 {changed}.\n\
                 Each entry is (file, bare `{BARE_CLIENT}` count, hand-built \
                 `{HAND_BUILT_CLIENT}` count). A new credentialed remote call must \
                 start from `egress::hardened()`, which supplies a connect \
                 timeout, a read timeout, and a redirect policy that will not \
                 carry an `x-api-key` to a host the response picked. If the new \
                 site cannot use it — loopback-only (a local \
                 llama-server/ollama/LM Studio runtime), a pinned peer \
                 certificate, or a cross-host download whose integrity comes from \
                 a checksum rather than an origin pin — add it to `ALLOWED` with a \
                 comment saying which of those it is and what bounds it instead. \
                 If a site disappeared, drop its entry."
            );
        }
    }
}
