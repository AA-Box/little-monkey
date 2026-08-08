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
/// `web.rs`'s `SsrfGuardedResolver` closes the same gap for its own guard, and
/// `browser_worker.rs` does it per Chromium launch with `--host-resolver-rules`.
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
    /// Only the attributed case is recorded, and that is the same split
    /// [`Self::add`] makes: an unattributed byte has a *reason* to be charged to
    /// but no row to hang a destination list off. `UNATTRIBUTED_EGRESS` is a
    /// fixed array of counters precisely because it must not allocate per
    /// request; a per-reason destination map would be a global lock on the hot
    /// path, which is what `run_scope` put the counter in the scope to avoid.
    ///
    /// ponytail: so unattributed egress still reports volume by reason and no
    /// destinations. Upgrade path is a bounded global map behind the same cap, if
    /// "which hosts does the app itself reach outside a run" turns out to be a
    /// question anyone asks.
    fn note_destination(&self, url: &Url) {
        let Charge::Process(process) = self else {
            return;
        };
        // Both are absent for the same kind of url — one with no authority, like
        // `data:` — and neither names a destination on its own.
        let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
            return;
        };
        process.note_destination(url.scheme(), &host.to_ascii_lowercase(), port);
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
/// once per hop. Counting the replay would mean wrapping a reusable body, which
/// makes it unreusable (`reqwest::Body::try_clone` returns `None` for a streaming
/// body) and would break the same-origin redirect this module deliberately
/// follows. Upgrade path is counting in the redirect policy, which sees each hop.
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
            let response = client
                .get(&host.origin)
                .send()
                .await
                .expect("a same-origin redirect must be followed");

            assert!(response.url().path().ends_with("/landed"));
            assert_eq!(response.text().await.expect("body reads"), "landed");
            assert_eq!(host.accepted(), 2, "one connection per hop");
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

    /// Ratchet: pins every remaining bare `Client::new()` in the tree, so a new
    /// one cannot be added without either routing it through [`hardened`] or
    /// writing down here why it does not need to be.
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
    /// # What it does not catch
    ///
    /// A hand-rolled `Client::builder()` that happens to set no timeout. Counting
    /// those too would flag the dozen sites that legitimately build their own
    /// client with their own budget (`m7_companion`'s 30- and 60-minute totals,
    /// `web.rs`'s SSRF-guarded resolver), so this pins the one spelling that is
    /// *never* right on a credentialed path instead of trying to police every
    /// builder.
    mod ratchet {
        /// Deliberately written without the `reqwest::` prefix so the
        /// `use reqwest::Client;` spelling cannot dodge the ratchet. The cost is
        /// that some *other* crate's `Client::new()` would also be counted; the
        /// remedy is to add it to `ALLOWED` with a note saying which crate it
        /// is, which is a fair price for closing the alias hole.
        const BARE_CLIENT: &str = "Client::new()";

        /// Production bare-client sites that are staying, and why.
        ///
        /// Paths are relative to `src/`. Every one of these is a **loopback-only**
        /// peer — this machine's own `llama-server`, `ollama`, or an LM-Studio-
        /// style runtime. They are not egress targets: there is no credential to
        /// forward and no third party to forward it to, and the counts are
        /// pinned here rather than converted so that a *new* site has to be
        /// justified.
        const ALLOWED: &[(&str, usize)] = &[
            // The CLI's local embedding endpoint.
            ("bin/monkey-cli/embed_cli.rs", 1),
            // The readiness probe against Studio's own `sd-server` child, on a
            // loopback port this process reserved and handed it on its command
            // line. Loopback by construction, and not deadline-free: the probe
            // carries a 2s per-request timeout and the whole wait is bounded by
            // `READY_TIMEOUT`.
            ("generation.rs", 1),
            // Local stack runtimes. Lived in `stacks.rs` until the v1 registry and
            // embedding core were extracted for the D2 collapse; the client itself is
            // unchanged and still talks only to the loopback embedding runtime.
            ("knowledge_core.rs", 1),
            // Bundled `llama-server` health/completion probes.
            ("llama.rs", 2),
        ];

        /// Everything after the first `#[cfg(test)]` is test code, and a test is
        /// free to use a bare client — it is talking to a listener it started
        /// itself. Verified at the time of writing that every file in the tree
        /// containing a bare client has at most one such attribute and no
        /// production code after it, so a single split is sound.
        fn production_half(source: &str) -> &str {
            source
                .split_once("\n#[cfg(test)]")
                .map_or(source, |(before, _)| before)
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
        /// Matched as the builder spelling only. `.timeout(..)` on a
        /// *`RequestBuilder`* is a different thing and usually correct — a
        /// per-request deadline on one small buffered call, which `llama.rs` and
        /// `stacks.rs` use exactly right — and there is no way to tell the two
        /// apart from the substring alone. So the scan finds `Client::builder()`
        /// first and only looks inside the chain that follows it.
        const BUILDER_TOTAL_TIMEOUT: &str = ".timeout(";

        /// How far past a `Client::builder()` to keep looking for its own
        /// `.timeout(`. Every builder chain in this tree is far shorter than this;
        /// the window exists so the scan cannot run off into an unrelated
        /// function and count its per-request deadline.
        const CHAIN_WINDOW_LINES: usize = 14;

        /// Production `Client::builder()` chains that set a total deadline, and
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
        /// `docs/agent-os-roadmap.md` carries the detail.
        const TOTAL_TIMEOUT_ALLOWED: &[(&str, usize)] = &[
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
            // trip, and the run is bounded by `JOB_TIMEOUT` (2h) in the polling
            // loop. The largest body is the terminal poll, which carries the
            // finished media base64 inside the JSON under `MAX_MEDIA_BYTES`
            // (256 MiB) — 2.8 MB/s across a loopback socket, and fully buffered
            // by `json()`, so there is no stream for the deadline to truncate.
            // The capabilities body is the smallest of the three: a few KB of
            // sampler and scheduler names, likewise buffered by `json()`.
            ("generation_commands.rs", 3),
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
            // paths do not. Two caveats in the roadmap: the chain sets no
            // `connect_timeout`, so those 30s also cover DNS, TLS and up to
            // `MAX_REDIRECT_HOPS` hops; and `search_client`'s own 15s total is
            // **not** counted here, because it starts from
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
                let scannable = code_only(production_half(&source));
                let lines: Vec<&str> = scannable.lines().collect();
                let count = lines
                    .iter()
                    .enumerate()
                    .filter(|(index, line)| {
                        line.contains("Client::builder()")
                            && lines
                                .iter()
                                .skip(index + 1)
                                .take(CHAIN_WINDOW_LINES)
                                .take_while(|following| !following.contains("Client::builder()"))
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
                "the set of `Client::builder()` chains setting their own total \
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
        fn no_new_bare_reqwest_client_can_be_added_unnoticed() {
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
                let count = code_only(production_half(&source))
                    .matches(BARE_CLIENT)
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

            let expected: Vec<(String, usize)> = ALLOWED
                .iter()
                .map(|(file, count)| ((*file).to_string(), *count))
                .collect();

            assert_eq!(
                found, expected,
                "the set of bare `{BARE_CLIENT}` sites in production code changed.\n\
                 A new credentialed remote call must start from \
                 `egress::hardened()`, which supplies a connect timeout, a read \
                 timeout, and a redirect policy that will not carry an \
                 `x-api-key` to a host the response picked. If the new site is \
                 loopback-only (a local llama-server/ollama/LM Studio runtime), \
                 add it to `ALLOWED` with a comment saying which peer it talks \
                 to. If a site disappeared, drop its entry."
            );
        }
    }
}
