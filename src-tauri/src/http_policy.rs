//! Shared policy for the unified HTTP service.
//!
//! Legacy OpenAI-compatible routes and M3 compatibility routes now share one
//! route authority, lifecycle, admission domain, and response-body contract.
//! A distinct LAN/TLS socket is still possible, but it is an endpoint of the
//! same logical service rather than an independently-started listener.

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Bytes, Frame, SizeHint};
use hyper::{header, Response, StatusCode};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

/// Error erased by both listeners' buffered and streaming HTTP bodies.
pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// One response-body type shared by the legacy and compatibility listeners.
///
/// Both listeners return a mix of fully buffered JSON and streaming SSE. Boxing
/// those bodies here lets admission ownership be attached once at the HTTP
/// boundary, independent of which router produced the response.
pub(crate) type ResponseBody = BoxBody<Bytes, BoxError>;

/// Buffered response body shared by both routers.
///
/// `Full<Bytes>`'s `Error` is `Infallible` — mapping it into `BoxError` is what
/// lets every response path on either listener share one concrete body type.
pub(crate) fn full_body(bytes: impl Into<Bytes>) -> ResponseBody {
    Full::new(bytes.into())
        .map_err(|never: std::convert::Infallible| match never {})
        .boxed()
}

/// One buffered JSON response for both routers.
///
/// Deduplicated because it is byte-identical on both sides: same status, the
/// same single `Content-Type: application/json` header, and `Value::to_string`
/// for the body. Only the *envelope inside* `value` differs between the two
/// families, and that stays with each router's own `error_response` — see the
/// note there.
pub(crate) fn json_response(
    status: StatusCode,
    value: serde_json::Value,
) -> Response<ResponseBody> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(full_body(Bytes::from(value.to_string())))
        .expect("building a response from a fixed status + static header never fails")
}

/// Why a capped body read refused to produce bytes.
///
/// The read *semantics* are shared (see [`read_capped_body`]); the wire
/// rendering is not, because the two families answer in deliberately different
/// envelopes and CORS regimes. Returning a typed rejection keeps the one
/// security-relevant loop single-sourced while leaving each router to render
/// its own bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CappedBodyRejection {
    /// The request (or the whole server) was cancelled mid-read.
    Cancelled,
    /// The running total would have exceeded `limit`.
    TooLarge { limit: usize },
    /// A frame read failed — client disconnect, malformed chunked encoding.
    ReadFailed,
}

/// Streams a request body in frame by frame, rejecting it the moment the
/// running total would exceed `limit`.
///
/// Unlike `Incoming::collect()`, this never buffers past the cap, so an
/// oversized body can't force an unbounded allocation before it's rejected
/// (the security-review finding this addresses: `collect()` used to buffer the
/// *entire* body — no matter how large — before the Authorization header had
/// even been looked at). A read that fails partway through is reported as its
/// own [`CappedBodyRejection::ReadFailed`], rather than silently substituting
/// an empty body and letting it fail later as a confusing generic "invalid
/// JSON" — a second, independently-reported review finding.
///
/// **`Content-Length` is deliberately not consulted.** Only bytes actually
/// delivered in data frames count, so a header that lies in either direction
/// changes nothing: an understated length on an oversized body is still
/// refused at the frame that crosses the cap, and an overstated length on a
/// small body is still accepted. A chunked body that overruns is the same
/// case as many small frames that overrun — the running total catches both.
///
/// Nothing is drained after a rejection: the remaining frames are dropped with
/// the body, which is the point — draining would hand the caller back exactly
/// the unbounded read the cap exists to prevent.
///
/// Generic over the body type (rather than hardcoded to `Incoming`) purely so
/// unit tests can drive it with a synthetic `StreamBody` instead of a real
/// hyper connection.
pub(crate) async fn read_capped_body<B>(
    mut body: B,
    limit: usize,
    cancellation: &CancellationToken,
) -> Result<Bytes, CappedBodyRejection>
where
    B: Body<Data = Bytes> + Unpin,
{
    let mut collected: Vec<u8> = Vec::new();
    loop {
        let frame = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(CappedBodyRejection::Cancelled),
            frame = body.frame() => frame,
        };
        match frame {
            Some(Ok(frame)) => {
                let Some(data) = frame.data_ref() else {
                    continue;
                };
                if collected.len().saturating_add(data.len()) > limit {
                    return Err(CappedBodyRejection::TooLarge { limit });
                }
                collected.extend_from_slice(data);
            }
            Some(Err(_)) => return Err(CappedBodyRejection::ReadFailed),
            None => break,
        }
    }
    Ok(Bytes::from(collected))
}

/// Default port of the unified loopback HTTP endpoint.
///
/// Changing it is user-visible because published Local Apps bake the port into
/// their HTML (see `local_apps.rs`).
pub const DEFAULT_HTTP_PORT: u16 = 1234;

/// Maximum requests in flight across one logical server generation.
pub const MAX_ACTIVE_REQUESTS: usize = 64;

/// Largest request body either listener will read before answering 413.
///
/// Enforced by [`read_capped_body`] as it streams frames in, never after the
/// fact — buffering the whole thing first and checking the length would defeat
/// the point. Chat-completion payloads can legitimately run to several MB
/// (inline base64 image content for multimodal messages), so this is set
/// generously rather than to a tight "small JSON payload" bound: it exists to
/// put a ceiling on a malicious or mistaken caller's memory impact, the same
/// "streamed cap regardless of what Content-Length claims" stance
/// `web.rs::MAX_BODY_BYTES` takes for fetched page bodies.
///
/// Lives here rather than once per listener because it is a shared
/// *mechanism*, which is what this module owns — and because the K21
/// attestation publishes it as a limit clients may rely on. A value a client
/// is told and a value a listener enforces must be one constant.
pub const MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;

pub const LEGACY_TOKEN_RATE_WINDOW_MS: u64 = 60_000;
pub const LEGACY_TOKEN_MAX_REQUESTS: u64 = 60;
pub const LEGACY_TOKEN_MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LEGACY_RATE_KEYS: usize = 4_096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TokenRateWindow {
    started_at_ms: u64,
    requests: u64,
    input_bytes: u64,
}

/// Compares two byte strings in time that does not depend on where they first
/// differ.
///
/// One implementation, in the module both listeners already share, because this
/// is a security primitive and two copies is one copy that can be fixed while
/// the other keeps the bug. `server.rs` and `compatibility_hub.rs` each had a
/// byte-identical private version; they now both call this.
///
/// Every caller compares fixed-length SHA-256 hex digests, so the length-based
/// early return leaks nothing a caller could not compute itself. What would
/// matter for real is if this ever compared *unhashed* tokens directly — it must
/// not start doing that.
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

/// In-memory migration limiter for legacy `lmk-*` tokens.
///
/// Pairing tokens keep their durable limiter in `LanAccessController`; legacy
/// tokens cannot be migrated into that store because only their digests remain
/// and published Local Apps still contain their plaintexts. The unified HTTP
/// service therefore applies this bounded limiter to the fallback branch until
/// those tokens naturally age out. It is keyed by the persisted token id—not
/// plaintext or digest—and shared by every endpoint in the logical server.
#[derive(Default)]
pub struct LegacyTokenRateLimiter {
    windows: Mutex<BTreeMap<String, TokenRateWindow>>,
}

impl LegacyTokenRateLimiter {
    pub fn check_and_debit(
        &self,
        token_id: &str,
        input_bytes: u64,
        now_ms: u64,
    ) -> Result<(), u64> {
        let mut windows = self
            .windows
            .lock()
            .map_err(|_| LEGACY_TOKEN_RATE_WINDOW_MS)?;
        if windows.len() >= MAX_LEGACY_RATE_KEYS && !windows.contains_key(token_id) {
            windows.retain(|_, window| {
                now_ms.saturating_sub(window.started_at_ms) < LEGACY_TOKEN_RATE_WINDOW_MS
            });
            if windows.len() >= MAX_LEGACY_RATE_KEYS {
                return Err(LEGACY_TOKEN_RATE_WINDOW_MS);
            }
        }
        let window = windows
            .entry(token_id.to_string())
            .or_insert(TokenRateWindow {
                started_at_ms: now_ms,
                requests: 0,
                input_bytes: 0,
            });
        if now_ms.saturating_sub(window.started_at_ms) >= LEGACY_TOKEN_RATE_WINDOW_MS {
            *window = TokenRateWindow {
                started_at_ms: now_ms,
                requests: 0,
                input_bytes: 0,
            };
        }
        let next_requests = window.requests.saturating_add(1);
        let next_bytes = window.input_bytes.saturating_add(input_bytes);
        if next_requests > LEGACY_TOKEN_MAX_REQUESTS || next_bytes > LEGACY_TOKEN_MAX_INPUT_BYTES {
            return Err(LEGACY_TOKEN_RATE_WINDOW_MS
                .saturating_sub(now_ms.saturating_sub(window.started_at_ms)));
        }
        window.requests = next_requests;
        window.input_bytes = next_bytes;
        Ok(())
    }
}

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
/// The two route families no longer own independent listeners, so an
/// `AddrInUse` failure cannot honestly blame the other internal surface. The
/// default-port message names likely external owners and retains the exact
/// settings panel where the endpoint can be changed.
pub fn describe_bind_error(
    role: ListenerRole,
    bind_address: &str,
    port: u16,
    error: &io::Error,
) -> String {
    if error.kind() == io::ErrorKind::AddrInUse {
        if port == DEFAULT_HTTP_PORT {
            return format!(
                "Could not bind {bind_address}:{port} for {}: the port is already in use. \
                 Another Little Monkey instance, `monkey-cli api-serve`, or another process \
                 may own it. Stop that process or choose a different port in {}.",
                role.label(),
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
/// the body not yet read. Both listeners therefore pass every admitted response
/// through [`hold_admission_until_response_ends`], so the guard is released only
/// when the body finishes or the client drops it.
///
/// **What the token is for is server shutdown**, which is worth stating because an
/// earlier version of this comment claimed it covered a client going away, and
/// that part was never the gap: a disconnecting client is already handled by drop,
/// since hyper drops the service future and the in-flight `reqwest` future with
/// it. Stopping the server was the hole. `server.rs`'s `stop_server_core` awaits
/// only the accept loop's task, while every connection is a separate
/// `tokio::spawn` that nothing joins — so requests it had already accepted kept
/// streaming from upstream after the UI reported "stopped".
///
/// Both listeners now honour it. The compatibility listener threads it into
/// `M3OperationContext`; the legacy listener carries it on `ServerDeps` and races
/// every upstream call against it. The shared wrapper ends a body cut short by
/// shutdown in an **error** rather than a clean close, because a truncated SSE
/// stream that closes successfully is indistinguishable to a client from a
/// complete one that happens to lack `[DONE]`.
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
        self.counters
            .active_requests
            .fetch_sub(1, Ordering::Relaxed);
        self.counters.request_count.fetch_add(1, Ordering::Relaxed);
        self.counters
            .last_request_at_ms
            .store(unix_time_ms(), Ordering::Relaxed);
    }
}

/// Attaches admission ownership to a response's entire body lifetime.
///
/// The returned response preserves the original status, headers, frames and
/// trailers byte-for-byte. Its body owns `guard`, so permits and counters remain
/// in flight after the handler returns and are released only when the body ends
/// or is dropped. The guard's cancellation token is also a child of the server
/// token; a shutdown therefore interrupts a body already being streamed.
pub(crate) fn hold_admission_until_response_ends(
    response: Response<ResponseBody>,
    guard: AdmissionGuard,
) -> Response<ResponseBody> {
    response.map(|body| BodyExt::boxed(AdmissionBody::new(body, guard)))
}

/// Body adapter that owns admission without changing HTTP body semantics.
///
/// A stream-based adapter would forward frames, but it would lose the wrapped
/// body's `is_end_stream` and `size_hint`. Hyper uses the latter to choose
/// `Content-Length` for buffered bodies, so losing it would silently change M3
/// JSON responses to chunked transfer encoding. Delegating the [`Body`] contract
/// directly preserves both framing metadata and trailers.
struct AdmissionBody {
    inner: ResponseBody,
    guard: Option<AdmissionGuard>,
    cancelled: Pin<Box<WaitForCancellationFutureOwned>>,
    cancellation_emitted: bool,
    polled_once: bool,
}

impl AdmissionBody {
    fn new(inner: ResponseBody, guard: AdmissionGuard) -> Self {
        let cancelled = Box::pin(guard.cancellation().cancelled_owned());
        Self {
            inner,
            guard: Some(guard),
            cancelled,
            cancellation_emitted: false,
            polled_once: false,
        }
    }

    fn release(&mut self) {
        self.guard.take();
    }
}

impl Body for AdmissionBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.as_mut().get_mut();
        if this.cancellation_emitted {
            this.release();
            return Poll::Ready(None);
        }

        // A `Full` body reports end-of-stream after its one frame. Let that
        // already-complete response finish cleanly even if shutdown raced it.
        if this.inner.is_end_stream() {
            this.release();
            return Poll::Ready(None);
        }

        // After the first poll, cancellation gets priority over another ready
        // streaming frame. Otherwise an always-ready upstream could starve
        // shutdown forever. The first poll is body-first so an already-built
        // buffered response deterministically keeps its bytes.
        if this.polled_once && Future::poll(this.cancelled.as_mut(), cx).is_ready() {
            this.cancellation_emitted = true;
            this.release();
            let error: BoxError = Box::new(std::io::Error::other(
                "The API server stopped while this response was streaming",
            ));
            return Poll::Ready(Some(Err(error)));
        }
        this.polled_once = true;

        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(frame)) => return Poll::Ready(Some(frame)),
            Poll::Ready(None) => {
                this.release();
                return Poll::Ready(None);
            }
            Poll::Pending => {}
        }

        if Future::poll(this.cancelled.as_mut(), cx).is_ready() {
            this.cancellation_emitted = true;
            this.release();
            let error: BoxError = Box::new(std::io::Error::other(
                "The API server stopped while this response was streaming",
            ));
            return Poll::Ready(Some(Err(error)));
        }
        Poll::Pending
    }

    fn is_end_stream(&self) -> bool {
        self.guard.is_none() || self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        if self.cancellation_emitted {
            SizeHint::with_exact(0)
        } else {
            self.inner.size_hint()
        }
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

/// Wall-clock milliseconds since the Unix epoch, for both routers' rate
/// windows, `last_used_at` bumps and admission counters.
///
/// There used to be three of these (`server.rs::now_ms`,
/// `m3_http_server.rs::now_ms`, and this one). They agreed except on the
/// `u128 -> u64` narrowing, where two wrapped (`as u64`) and one saturated;
/// the saturating form is kept because a wrapped clock reads as a time far in
/// the past, which silently resets every rate-limit window.
pub(crate) fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use sha2::Digest as _;

    /// Moved here with the primitive it pins. It lived in `server.rs` next to
    /// that file's private copy, which is exactly the arrangement that lets a
    /// second copy go untested.
    #[test]
    fn constant_time_eq_matches_equal_digests_and_rejects_unequal_ones() {
        let digest = |input: &str| format!("{:x}", sha2::Sha256::digest(input.as_bytes()));
        let base = digest("lmk-abc");
        assert!(constant_time_eq(
            base.as_bytes(),
            digest("lmk-abc").as_bytes()
        ));
        assert!(!constant_time_eq(
            base.as_bytes(),
            digest("lmk-different").as_bytes()
        ));
        assert!(!constant_time_eq(b"short", base.as_bytes()));
    }

    /// The function must reject a mismatch regardless of *where* the difference
    /// falls. A naive early-exit `==` would be identical in correctness here —
    /// this pins the behaviour, since real timing cannot be asserted in a unit
    /// test.
    #[test]
    fn constant_time_eq_rejects_mismatches_at_every_position() {
        let base = format!("{:x}", sha2::Sha256::digest(b"lmk-fixed-value"));
        let flip = |index: usize| {
            let mut flipped = base.clone();
            let replacement = if &base[index..index + 1] == "0" {
                "1"
            } else {
                "0"
            };
            flipped.replace_range(index..index + 1, replacement);
            flipped
        };

        assert!(!constant_time_eq(base.as_bytes(), flip(0).as_bytes()));
        assert!(!constant_time_eq(
            base.as_bytes(),
            flip(base.len() - 1).as_bytes()
        ));
        assert!(constant_time_eq(base.as_bytes(), base.as_bytes()));
    }

    use std::convert::Infallible;

    use futures_util::stream;
    use http_body_util::{Full, StreamBody};
    use hyper::header::HeaderValue;
    use hyper::{HeaderMap, StatusCode};

    use super::*;

    fn test_body(bytes: &'static [u8]) -> ResponseBody {
        Full::new(Bytes::from_static(bytes))
            .map_err(|never: Infallible| -> BoxError { match never {} })
            .boxed()
    }

    #[test]
    fn unified_service_has_one_pinned_default_port() {
        assert_eq!(DEFAULT_HTTP_PORT, 1234);
    }

    #[test]
    fn legacy_token_limiter_debits_once_resets_and_bounds_bytes() {
        let limiter = LegacyTokenRateLimiter::default();
        for index in 0..LEGACY_TOKEN_MAX_REQUESTS {
            limiter
                .check_and_debit("token-a", 1, 1_000 + index)
                .expect("request inside window");
        }
        assert!(limiter.check_and_debit("token-a", 1, 2_000).is_err());
        limiter
            .check_and_debit("token-a", 1, 61_001)
            .expect("new window");

        assert!(limiter
            .check_and_debit("token-b", LEGACY_TOKEN_MAX_INPUT_BYTES + 1, 1_000)
            .is_err());
        limiter
            .check_and_debit("token-c", LEGACY_TOKEN_MAX_INPUT_BYTES, 1_000)
            .expect("exact byte budget");
    }

    #[test]
    fn a_conflict_on_the_default_port_names_external_owners_and_where_to_fix_it() {
        let error = io::Error::new(io::ErrorKind::AddrInUse, "address already in use");
        let message = describe_bind_error(
            ListenerRole::LegacyProxy,
            "127.0.0.1",
            DEFAULT_HTTP_PORT,
            &error,
        );
        assert!(message.contains("127.0.0.1:1234"), "{message}");
        assert!(
            message.contains("monkey-cli api-serve"),
            "the message must name a real competing process: {message}"
        );
        assert!(message.contains("Settings → API Hub"), "{message}");

        let reverse = describe_bind_error(
            ListenerRole::CompatibilityListener,
            "0.0.0.0",
            DEFAULT_HTTP_PORT,
            &error,
        );
        assert!(reverse.contains("compatibility listener"), "{reverse}");
        assert!(reverse.contains("another process"), "{reverse}");
        assert!(reverse.contains("Runtime Hub → Compatibility"), "{reverse}");
    }

    #[test]
    fn a_conflict_on_a_custom_port_does_not_guess_the_owner() {
        let error = io::Error::new(io::ErrorKind::AddrInUse, "address already in use");
        let message = describe_bind_error(ListenerRole::LegacyProxy, "127.0.0.1", 8123, &error);
        assert!(message.contains("8123"), "{message}");
        assert!(!message.contains("monkey-cli api-serve"), "{message}");
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
        assert_eq!(
            admission.request_count(),
            1,
            "a completed request is counted"
        );
        admission
            .try_admit(&shutdown)
            .expect("a freed permit admits again");

        drop(second);
    }

    #[tokio::test]
    async fn response_wrapper_preserves_wire_parts_and_holds_admission_until_body_end() {
        let admission = RequestAdmission::new(1);
        let shutdown = CancellationToken::new();
        let guard = admission.try_admit(&shutdown).expect("request admits");
        let response = Response::builder()
            .status(StatusCode::CREATED)
            .header("x-test-header", "unchanged")
            .body(test_body(br#"{"ok":true}"#))
            .expect("test response");

        let response = hold_admission_until_response_ends(response, guard);
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers()["x-test-header"], "unchanged");
        assert_eq!(response.body().size_hint().exact(), Some(11));
        assert_eq!(admission.active_requests(), 1);
        assert_eq!(admission.request_count(), 0);

        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("wrapped body collects")
            .to_bytes();
        assert_eq!(&bytes[..], br#"{"ok":true}"#);
        assert_eq!(admission.active_requests(), 0);
        assert_eq!(admission.request_count(), 1);
    }

    #[tokio::test]
    async fn response_wrapper_preserves_trailers() {
        let admission = RequestAdmission::new(1);
        let shutdown = CancellationToken::new();
        let guard = admission.try_admit(&shutdown).expect("request admits");
        let mut trailers = HeaderMap::new();
        trailers.insert("x-checksum", HeaderValue::from_static("kept"));
        let frames = stream::iter(vec![
            Ok::<_, BoxError>(Frame::data(Bytes::from_static(b"payload"))),
            Ok(Frame::trailers(trailers)),
        ]);
        let body = StreamBody::new(frames).boxed();
        let response = hold_admission_until_response_ends(Response::new(body), guard);

        let collected = response
            .into_body()
            .collect()
            .await
            .expect("body and trailers collect");
        assert_eq!(
            collected.trailers().expect("trailers")["x-checksum"],
            "kept"
        );
        assert_eq!(collected.to_bytes(), Bytes::from_static(b"payload"));
        assert_eq!(admission.active_requests(), 0);
    }

    #[tokio::test]
    async fn completed_buffered_body_wins_a_shutdown_race_without_transport_error() {
        let admission = RequestAdmission::new(1);
        let shutdown = CancellationToken::new();
        let guard = admission.try_admit(&shutdown).expect("request admits");
        let response =
            hold_admission_until_response_ends(Response::new(test_body(b"complete")), guard);
        shutdown.cancel();

        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("an already-complete buffered response stays successful")
            .to_bytes();
        assert_eq!(&bytes[..], b"complete");
        assert_eq!(admission.active_requests(), 0);
    }

    #[tokio::test]
    async fn an_always_ready_stream_cannot_starve_shutdown() {
        let admission = RequestAdmission::new(1);
        let shutdown = CancellationToken::new();
        let guard = admission.try_admit(&shutdown).expect("request admits");
        let frames = stream::repeat_with(|| {
            Ok::<_, BoxError>(Frame::data(Bytes::from_static(b"still streaming")))
        });
        let response = hold_admission_until_response_ends(
            Response::new(StreamBody::new(frames).boxed()),
            guard,
        );
        shutdown.cancel();

        let mut body = response.into_body();
        let first = tokio::time::timeout(std::time::Duration::from_secs(1), body.frame())
            .await
            .expect("first poll completes")
            .expect("stream has a first frame")
            .expect("the body-first poll may publish one ready frame");
        assert!(first.is_data());
        let second = tokio::time::timeout(std::time::Duration::from_secs(1), body.frame())
            .await
            .expect("shutdown wakes the body")
            .expect("shutdown emits one terminal frame");
        assert!(
            second.is_err(),
            "a hot upstream must not keep winning polls after shutdown"
        );
        assert_eq!(admission.active_requests(), 0);
    }

    #[tokio::test]
    async fn dropping_an_unread_wrapped_response_releases_admission() {
        let admission = RequestAdmission::new(1);
        let shutdown = CancellationToken::new();
        let guard = admission.try_admit(&shutdown).expect("request admits");
        let response = Response::new(test_body(b"unread"));
        let response = hold_admission_until_response_ends(response, guard);

        assert_eq!(admission.active_requests(), 1);
        drop(response);
        assert_eq!(admission.active_requests(), 0);
        assert_eq!(admission.request_count(), 1);
        assert!(
            admission.try_admit(&shutdown).is_some(),
            "dropping a client response must return its permit"
        );
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
        assert!(
            token.is_cancelled(),
            "dropping the guard must cancel the request"
        );
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

    /// Cap semantics, moved here with the implementation. Each router keeps its
    /// own test for the *bytes* it renders from these rejections.
    #[tokio::test]
    async fn a_body_well_within_the_limit_is_returned_whole() {
        let stream = stream::iter(vec![
            Ok::<_, BoxError>(Frame::data(Bytes::from_static(b"hello "))),
            Ok(Frame::data(Bytes::from_static(b"world"))),
        ]);
        let bytes = read_capped_body(StreamBody::new(stream), 1024, &CancellationToken::new())
            .await
            .expect("a small body reads");
        assert_eq!(&bytes[..], b"hello world");
    }

    #[tokio::test]
    async fn a_single_oversized_frame_is_refused_before_it_is_buffered() {
        let stream = stream::iter(vec![Ok::<_, BoxError>(Frame::data(Bytes::from_static(
            b"0123456789",
        )))]);
        assert_eq!(
            read_capped_body(StreamBody::new(stream), 4, &CancellationToken::new()).await,
            Err(CappedBodyRejection::TooLarge { limit: 4 })
        );
    }

    /// The running total, not just any single frame in isolation, must be
    /// checked against the limit — otherwise a caller could smuggle an
    /// arbitrarily large body past the cap by splitting it into many
    /// small-enough frames (exactly what a real chunked-encoded upload from an
    /// oversized client looks like at the hyper frame level).
    #[tokio::test]
    async fn an_oversized_body_split_across_many_small_frames_is_still_refused() {
        let stream = stream::iter(vec![
            Ok::<_, BoxError>(Frame::data(Bytes::from_static(b"12345"))),
            Ok(Frame::data(Bytes::from_static(b"67890"))),
        ]);
        assert_eq!(
            read_capped_body(StreamBody::new(stream), 6, &CancellationToken::new()).await,
            Err(CappedBodyRejection::TooLarge { limit: 6 })
        );
    }

    /// The core regression for the "partial read silently becomes an empty
    /// body" finding: a failed frame read must surface as its own distinct
    /// rejection, never as `Ok(Bytes::new())`.
    #[tokio::test]
    async fn a_failed_frame_read_is_distinct_from_an_empty_body() {
        let stream = stream::iter(vec![Err::<Frame<Bytes>, BoxError>(
            "simulated connection drop".into(),
        )]);
        assert_eq!(
            read_capped_body(StreamBody::new(stream), 1024, &CancellationToken::new()).await,
            Err(CappedBodyRejection::ReadFailed)
        );
    }

    #[tokio::test]
    async fn a_cancelled_request_stops_reading_its_body() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let stream = stream::repeat_with(|| {
            Ok::<_, BoxError>(Frame::data(Bytes::from_static(b"still uploading")))
        });
        assert_eq!(
            read_capped_body(StreamBody::new(stream), usize::MAX, &cancellation).await,
            Err(CappedBodyRejection::Cancelled)
        );
    }

    #[test]
    fn a_non_conflict_error_keeps_what_the_os_said() {
        let error = io::Error::new(io::ErrorKind::PermissionDenied, "permission denied");
        let message = describe_bind_error(ListenerRole::LegacyProxy, "127.0.0.1", 80, &error);
        assert!(message.contains("permission denied"), "{message}");
        assert!(!message.contains("already in use"), "{message}");
    }
}
