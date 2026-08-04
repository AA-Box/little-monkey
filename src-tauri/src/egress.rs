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

use std::net::Ipv6Addr;

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
}
