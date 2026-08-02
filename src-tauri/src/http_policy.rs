//! Policy shared by this app's two HTTP listeners.
//!
//! There are two: `server.rs` (the legacy OpenAI-compatible reverse proxy) and
//! `m3_http_server.rs` (the M3 compatibility listener). Collapsing them into one
//! is D1 in `docs/agent-os-roadmap.md`; this module is where the shared pieces
//! accumulate as that work proceeds, so each one stops being implemented twice.
//!
//! Starting content is the port-conflict diagnosis, because the two listeners
//! **default to the same port** — `server.rs`'s `DEFAULT_PORT` and the persisted
//! `LanServerPolicy`'s default are both 1234, both bind loopback, and both
//! autostart independently from `lib.rs`'s `setup` with no ordering and no
//! cross-check. A user with `autostart` enabled *and* a persisted LAN policy on
//! the default port has two tasks racing for one socket, and whichever loses
//! reports a bare "address already in use" naming neither the winner nor the
//! reason.

use std::io;

/// The port both listeners default to.
///
/// Shared so the collision is visible in one place rather than being a
/// coincidence between two unrelated constants. Changing either default is a
/// user-visible config change (published Local Apps bake the port into their
/// HTML — see `local_apps.rs`), so this records the overlap rather than
/// silently resolving it.
pub const DEFAULT_HTTP_PORT: u16 = 1234;

/// Which listener is reporting a bind failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerRole {
    /// `server.rs` — the OpenAI-compatible reverse proxy, loopback only.
    LegacyProxy,
    /// `m3_http_server.rs` — the M3 compatibility listener, binds the persisted
    /// `LanServerPolicy`.
    CompatibilityListener,
}

impl ListenerRole {
    pub fn label(self) -> &'static str {
        match self {
            ListenerRole::LegacyProxy => "the API server",
            ListenerRole::CompatibilityListener => "the compatibility listener",
        }
    }

    /// The other listener — the one most likely already holding the port.
    pub fn other(self) -> ListenerRole {
        match self {
            ListenerRole::LegacyProxy => ListenerRole::CompatibilityListener,
            ListenerRole::CompatibilityListener => ListenerRole::LegacyProxy,
        }
    }

    /// Where a user actually resolves the conflict for this listener.
    pub fn settings_hint(self) -> &'static str {
        match self {
            ListenerRole::LegacyProxy => "Settings → API Hub",
            ListenerRole::CompatibilityListener => "Runtime Hub → Compatibility",
        }
    }
}

/// Turns a bind failure into a message that names the likely cause.
///
/// `AddrInUse` on [`DEFAULT_HTTP_PORT`] is overwhelmingly this app conflicting
/// with itself, so say that and point at the panel that fixes it. Any other
/// error, or a non-default port, keeps the underlying message — guessing at a
/// cause we have not established would be worse than reporting what the OS
/// said.
pub fn describe_bind_error(
    role: ListenerRole,
    bind_address: &str,
    port: u16,
    error: &io::Error,
) -> String {
    if error.kind() == io::ErrorKind::AddrInUse {
        let other = role.other();
        if port == DEFAULT_HTTP_PORT {
            return format!(
                "Could not bind {bind_address}:{port} for {}: the port is already in use. \
                 {} defaults to this port too — if it is running, give one of them a \
                 different port in {} or stop the other one.",
                role.label(),
                capitalize(other.label()),
                role.settings_hint(),
            );
        }
        return format!(
            "Could not bind {bind_address}:{port} for {}: the port is already in use. \
             Choose a different port in {}.",
            role.label(),
            role.settings_hint(),
        );
    }
    format!(
        "Could not bind {bind_address}:{port} for {}: {error}",
        role.label()
    )
}

fn capitalize(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_listeners_share_one_default_port_constant() {
        // The point of the constant: the overlap is stated, not accidental.
        assert_eq!(DEFAULT_HTTP_PORT, 1234);
        assert_eq!(
            ListenerRole::LegacyProxy.other(),
            ListenerRole::CompatibilityListener
        );
        assert_eq!(
            ListenerRole::CompatibilityListener.other(),
            ListenerRole::LegacyProxy
        );
    }

    #[test]
    fn a_conflict_on_the_default_port_names_the_other_listener_and_where_to_fix_it() {
        let error = io::Error::new(io::ErrorKind::AddrInUse, "address already in use");
        let message = describe_bind_error(
            ListenerRole::LegacyProxy,
            "127.0.0.1",
            DEFAULT_HTTP_PORT,
            &error,
        );
        assert!(message.contains("127.0.0.1:1234"), "{message}");
        assert!(
            message.contains("compatibility listener"),
            "the message must name the other listener: {message}"
        );
        assert!(message.contains("Settings → API Hub"), "{message}");

        let reverse = describe_bind_error(
            ListenerRole::CompatibilityListener,
            "0.0.0.0",
            DEFAULT_HTTP_PORT,
            &error,
        );
        assert!(reverse.contains("The API server"), "{reverse}");
        assert!(reverse.contains("Runtime Hub → Compatibility"), "{reverse}");
    }

    #[test]
    fn a_conflict_on_a_custom_port_does_not_blame_the_other_listener() {
        // On a non-default port the other listener is not the likely culprit, so
        // claiming it would send the user to the wrong panel.
        let error = io::Error::new(io::ErrorKind::AddrInUse, "address already in use");
        let message = describe_bind_error(ListenerRole::LegacyProxy, "127.0.0.1", 8123, &error);
        assert!(message.contains("8123"), "{message}");
        assert!(!message.contains("defaults to this port too"), "{message}");
        assert!(message.contains("Choose a different port"), "{message}");
    }

    #[test]
    fn a_non_conflict_error_keeps_what_the_os_said() {
        let error = io::Error::new(io::ErrorKind::PermissionDenied, "permission denied");
        let message = describe_bind_error(ListenerRole::LegacyProxy, "127.0.0.1", 80, &error);
        assert!(message.contains("permission denied"), "{message}");
        assert!(!message.contains("already in use"), "{message}");
    }
}
