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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

/// The port both listeners default to.
///
/// Shared so the collision is visible in one place rather than being a
/// coincidence between two unrelated constants. Changing either default is a
/// user-visible config change (published Local Apps bake the port into their
/// HTML — see `local_apps.rs`), so this records the overlap rather than
/// silently resolving it.
pub const DEFAULT_HTTP_PORT: u16 = 1234;

/// Maximum requests in flight per listener.
///
/// Shared so both listeners are bounded by the same number rather than one being
/// bounded and the other unbounded — which is what they were.
pub const MAX_ACTIVE_REQUESTS: usize = 64;

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

/// Live request counters for one listener.
#[derive(Default)]
pub struct ServerCounters {
    pub request_count: AtomicU64,
    pub active_requests: AtomicUsize,
    pub last_request_at_ms: AtomicU64,
}

/// Held for the lifetime of one in-flight request.
///
/// Owns the concurrency permit (so the listener has a bounded number of requests
/// in flight), the active/total counters, and a per-request
/// [`CancellationToken`] derived from the server's shutdown token.
///
/// **Lifetime is the whole point and is easy to get wrong.** Releasing the permit
/// when the *handler returns* rather than when the *response body ends* bounds
/// time-to-first-header instead of time in flight, which for a streaming route
/// bounds nothing: the handler returns as soon as upstream headers arrive, with
/// the body not yet read. Both listeners now hold the guard until the body is
/// finished or dropped — `m3_http_server.rs` by moving it into `sse_body`'s
/// unfold state, `server.rs` by wrapping the response body in
/// `hold_permit_until_body_ends`. The legacy listener did the wrong one until the
/// guard-lifetime fix.
///
/// **What the token does *not* yet do**, stated because the earlier version of
/// this comment claimed otherwise: on the compatibility listener it reaches the
/// work, via `RequestGuard::context` into `M3OperationContext`. On the legacy
/// listener nothing reads it — `AdmissionGuard::cancellation` has no caller in
/// `server.rs`, and the upstream calls there are bare `reqwest::Client`s with no
/// timeout — so a legacy client that goes away still leaves its upstream request
/// running to completion. Wiring that is its own change; the type carrying a
/// token is not the same as a route honouring it.
pub struct AdmissionGuard {
    cancellation: CancellationToken,
    counters: Arc<ServerCounters>,
    _permit: OwnedSemaphorePermit,
}

impl AdmissionGuard {
    pub fn new(
        counters: Arc<ServerCounters>,
        permit: OwnedSemaphorePermit,
        server_shutdown: &CancellationToken,
    ) -> Self {
        counters.active_requests.fetch_add(1, Ordering::Relaxed);
        Self {
            cancellation: server_shutdown.child_token(),
            counters,
            _permit: permit,
        }
    }

    /// This request's cancellation token. Cancelled when the guard drops, and
    /// when the server's shutdown token fires.
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.counters.active_requests.fetch_sub(1, Ordering::Relaxed);
        self.counters.request_count.fetch_add(1, Ordering::Relaxed);
        self.counters
            .last_request_at_ms
            .store(unix_time_ms(), Ordering::Relaxed);
    }
}

/// Bounded admission for one listener: a permit pool plus the counters.
pub struct RequestAdmission {
    limit: Arc<Semaphore>,
    counters: Arc<ServerCounters>,
}

impl RequestAdmission {
    pub fn new(max_concurrent: usize) -> Self {
        RequestAdmission {
            limit: Arc::new(Semaphore::new(max_concurrent.max(1))),
            counters: Arc::new(ServerCounters::default()),
        }
    }

    /// Admits a request, or `None` when the quota is exhausted — the caller then
    /// owes the client a 503 rather than queueing without bound.
    pub fn try_admit(&self, server_shutdown: &CancellationToken) -> Option<AdmissionGuard> {
        let permit = self.limit.clone().try_acquire_owned().ok()?;
        Some(AdmissionGuard::new(
            self.counters.clone(),
            permit,
            server_shutdown,
        ))
    }

    pub fn counters(&self) -> Arc<ServerCounters> {
        self.counters.clone()
    }

    pub fn active_requests(&self) -> usize {
        self.counters.active_requests.load(Ordering::Relaxed)
    }

    pub fn request_count(&self) -> u64 {
        self.counters.request_count.load(Ordering::Relaxed)
    }
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
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

    #[tokio::test]
    async fn admission_bounds_concurrency_and_returns_the_permit_on_drop() {
        let admission = RequestAdmission::new(2);
        let shutdown = CancellationToken::new();

        let first = admission.try_admit(&shutdown).expect("first admits");
        let second = admission.try_admit(&shutdown).expect("second admits");
        assert_eq!(admission.active_requests(), 2);
        assert!(
            admission.try_admit(&shutdown).is_none(),
            "a third request must be refused, not queued without bound"
        );

        drop(first);
        assert_eq!(admission.active_requests(), 1);
        assert_eq!(admission.request_count(), 1, "a completed request is counted");
        admission
            .try_admit(&shutdown)
            .expect("a freed permit admits again");

        drop(second);
    }

    #[tokio::test]
    async fn a_guard_cancels_its_request_when_dropped() {
        // What makes an abandoned client actually stop upstream work.
        let admission = RequestAdmission::new(1);
        let shutdown = CancellationToken::new();
        let guard = admission.try_admit(&shutdown).expect("admits");
        let token = guard.cancellation();
        assert!(!token.is_cancelled());
        drop(guard);
        assert!(token.is_cancelled(), "dropping the guard must cancel the request");
    }

    #[tokio::test]
    async fn server_shutdown_cancels_every_in_flight_request() {
        let admission = RequestAdmission::new(4);
        let shutdown = CancellationToken::new();
        let one = admission.try_admit(&shutdown).expect("admits");
        let two = admission.try_admit(&shutdown).expect("admits");
        let tokens = [one.cancellation(), two.cancellation()];

        shutdown.cancel();
        for token in &tokens {
            assert!(
                token.is_cancelled(),
                "stopping the server must cancel work already in flight"
            );
        }
    }

    #[tokio::test]
    async fn a_zero_limit_still_admits_one_request() {
        // A misconfigured zero would otherwise wedge the listener into refusing
        // everything, which is worse than ignoring the value.
        let admission = RequestAdmission::new(0);
        let shutdown = CancellationToken::new();
        assert!(admission.try_admit(&shutdown).is_some());
    }

    #[test]
    fn a_non_conflict_error_keeps_what_the_os_said() {
        let error = io::Error::new(io::ErrorKind::PermissionDenied, "permission denied");
        let message = describe_bind_error(ListenerRole::LegacyProxy, "127.0.0.1", 80, &error);
        assert!(message.contains("permission denied"), "{message}");
        assert!(!message.contains("already in use"), "{message}");
    }
}
