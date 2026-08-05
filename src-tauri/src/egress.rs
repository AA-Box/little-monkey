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

use std::net::Ipv6Addr;
use std::time::Duration;

use reqwest::Url;

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
/// Prose here says `Client::builder` rather than the bare constructor on
/// purpose: the ratchet test at the bottom of this file counts that exact
/// string across the tree, and a doc comment naming it would count as a site.
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
    reqwest::Client::builder()
        .connect_timeout(connect)
        .read_timeout(read)
        .redirect(same_origin_redirect_policy())
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
            return attempt.error(std::io::Error::other(format!(
                "Refusing to follow more than {MAX_REDIRECT_HOPS} redirects"
            )));
        }
        // `previous()` cannot actually be empty here — reqwest pushes the
        // requested URL before it ever consults the policy. Treated as a
        // refusal rather than `unwrap`ped because if that ever changed there
        // would be no origin to compare against, and "follow it anyway" is the
        // wrong direction to fail in for a client carrying an API key.
        let Some(previous) = attempt.previous().last() else {
            return attempt.error(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Refusing a redirect with no recorded origin to compare against",
            ));
        };
        // The verdict is taken first, and the follow returns immediately, because
        // `Attempt::follow`/`Attempt::error` consume `attempt` while `previous`
        // is still borrowed out of it — that borrow has to end before either
        // call. Returning early also keeps the message off the happy path: it is
        // built from owned `String`s and would otherwise be allocated on every
        // hop that gets followed.
        if may_follow(previous, attempt.url()) {
            return attempt.follow();
        }
        let refusal = format!(
            "Refusing to carry credentials across a redirect from {} to {}",
            origin_label(previous),
            origin_label(attempt.url())
        );
        attempt.error(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            refusal,
        ))
    })
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

/// `scheme://host:port` for a diagnostic — deliberately **not** the whole URL.
///
/// These messages surface in provider errors the UI shows and the CLI prints,
/// and some of these paths put tokens in a query string (`hosted_oauth`'s relay
/// among them). Naming only the origin says everything a reader needs about a
/// refused hop without copying a credential into a log.
fn origin_label(url: &Url) -> String {
    match (url.host_str(), url.port_or_known_default()) {
        (Some(host), Some(port)) => format!("{}://{host}:{port}", url.scheme()),
        (Some(host), None) => format!("{}://{host}", url.scheme()),
        _ => url.scheme().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::str::FromStr;

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
        /// style runtime — except `server.rs`, which is owned by a different
        /// change. They are not egress targets: there is no credential to
        /// forward and no third party to forward it to, and the counts are
        /// pinned here rather than converted so that a *new* site has to be
        /// justified.
        const ALLOWED: &[(&str, usize)] = &[
            // The CLI's local embedding endpoint.
            ("bin/monkey-cli/embed_cli.rs", 1),
            // Bundled `llama-server` health/completion probes.
            ("llama.rs", 2),
            // The local Ollama daemon on `11434`.
            ("ollama.rs", 2),
            // Out of scope for this change by ownership, not by argument.
            ("server.rs", 2),
            // Local stack runtimes.
            ("stacks.rs", 1),
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
                let count = production_half(&source).matches(BARE_CLIENT).count();
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
