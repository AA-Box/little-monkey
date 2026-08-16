//! Discord adapter: gateway WebSocket inbound, REST outbound.
//!
//! One long-lived task owns the gateway socket for the lifetime of the
//! adapter. It is spawned lazily, on the first [`ChannelAdapter::probe`] or
//! [`ChannelAdapter::poll`] call rather than in [`DiscordAdapter::new`] —
//! `new` is a plain, synchronous constructor with no side effects, so
//! building an adapter for an account that turns out to be disabled never
//! opens a socket. The task normalizes `MESSAGE_CREATE` dispatches and pushes
//! them into a bounded channel; `poll` only drains that channel. The
//! gateway's own framing (HELLO, heartbeat, IDENTIFY/RESUME, DISPATCH) is
//! handled by [`handle_frame`] and [`handle_close`], both pure functions of a
//! [`GatewayState`] so the protocol logic is testable without a socket.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use little_monkey_lib::channels::types::{
    AttachmentKind, AttachmentSource, ChannelAttachment, ChannelConversation, ChannelEnvelope,
    ChannelHealth, ChannelKind, ChannelSender, HealthState, InboundTransport, OutboundMessage,
    ProviderCapabilities, SendOutcome,
};
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

use crate::daemon::channel_adapter::{
    load_attachments, AdapterConfig, BlobSource, ChannelAdapter, DaemonBlobs, InboundBatch,
    LoadedAttachment,
};

const API_BASE: &str = "https://discord.com/api/v10";
const GATEWAY_VERSION: &str = "10";
const DISCORD_MAX_TEXT_CHARS: usize = 2000;
/// Guilds (1<<0, carries THREAD_CREATE/THREAD_UPDATE — without it the
/// thread-to-parent map never populates) + guild messages (1<<9) + direct
/// messages (1<<12) + message content (1<<15). Exactly the set the implemented
/// feature surface reads, nothing speculative.
const DISCORD_INTENTS: u64 = 1 | (1 << 9) | (1 << 12) | (1 << 15);
const INBOUND_CHANNEL_CAPACITY: usize = 256;
/// How long [`DiscordAdapter::poll`] blocks for a first envelope before
/// returning an empty batch. Bounded so the daemon's poll loop keeps ticking
/// even when Discord is quiet.
const POLL_WAIT: Duration = Duration::from_secs(20);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Discord allows one IDENTIFY per five seconds per concurrency bucket;
/// sending them faster earns a 4008/invalid-session spiral rather than a
/// connection. A single-account client is one shard in bucket zero, so this
/// spacing is exactly the `max_concurrency` semantics for it.
const IDENTIFY_SPACING: Duration = Duration::from_secs(5);
/// The longest an open socket is held waiting for session-start allowance
/// before hanging up and waiting outside instead. Discord tolerates a short
/// pause between HELLO and IDENTIFY but not minutes of one.
const MAX_IDENTIFY_HOLD: Duration = Duration::from_secs(10);
/// The longest rate-limit wait served in place between chunks of one logical
/// message. Anything longer parks the row for reconciliation instead of
/// pinning an outbox worker to a sleep.
const MAX_INTRA_MESSAGE_WAIT_MS: u64 = 10_000;
/// How many thread-to-parent mappings ride the persisted cursor. The cursor
/// column caps its value at 4096 bytes, and 64 snowflake pairs plus the
/// resume fields stay safely under that; threads evicted past the cap are
/// re-resolved through the REST API on their next message.
const MAX_PERSISTED_THREADS: usize = 64;

/// What a RESUME needs to survive a daemon restart: the session, where Discord
/// said to resume it, and the last sequence number seen. Serialized as the
/// account's inbound cursor, which is persisted only after the batch it covers
/// is durably recorded — so the stored `seq` can lag reality but never lead
/// it, and a lagging resume merely replays events the event log deduplicates.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ResumeState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_gateway_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// Our own user id, learned from READY. Carried here because a session
    /// that RESUMEs never sees a second READY, and without it `is_self` is
    /// false for our own messages — which for Discord, where a bot is
    /// dispatched its own `MESSAGE_CREATE`, is how a reply loop starts. Not a
    /// secret: a bot's user id is visible to everyone it talks to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_user_id: Option<String>,
    /// Thread channel id -> parent channel id, carried here because thread
    /// topology otherwise lives only in gateway dispatches: a message in a
    /// thread the restarted process never saw a THREAD_CREATE for would
    /// normalize as a plain channel. Bounded to [`MAX_PERSISTED_THREADS`]
    /// entries (the cursor column the state is stored in caps its value at
    /// 4096 bytes); the oldest snowflake is evicted first, and an evicted
    /// thread is simply re-resolved through the REST API on its next message.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub thread_parents: std::collections::BTreeMap<String, String>,
}

/// Insert a thread mapping, evicting the oldest thread (smallest snowflake —
/// they are ordered by creation time) once the bound is reached. Numeric
/// order for equal-length strings is lexicographic order, so the comparison
/// key is (length, string).
fn insert_thread_parent(
    map: &mut std::collections::BTreeMap<String, String>,
    thread_id: String,
    parent_id: String,
) {
    map.insert(thread_id, parent_id);
    while map.len() > MAX_PERSISTED_THREADS {
        let Some(oldest) = map
            .keys()
            .min_by_key(|key| (key.len(), key.as_str().to_string()))
            .cloned()
        else {
            break;
        };
        map.remove(&oldest);
    }
}

impl ResumeState {
    fn parse(cursor: Option<&str>) -> Self {
        cursor
            .and_then(|value| serde_json::from_str(value).ok())
            .unwrap_or_default()
    }

    fn resumable(&self) -> bool {
        self.session_id.is_some() && self.seq.is_some()
    }
}

/// Gateway connection status, for an honest probe: `Connected` is written only
/// by a READY/RESUMED on a live socket, never by saved configuration.
const GATEWAY_NOT_STARTED: u8 = 0;
const GATEWAY_RECONNECTING: u8 = 1;
const GATEWAY_CONNECTED: u8 = 2;
/// The gateway task exited for good — bad token or disallowed intent.
/// Backoff does not fix it, and health must say so rather than "reconnecting".
const GATEWAY_FAILED: u8 = 3;

/// State shared between the gateway task and the adapter handle. Everything
/// here is either non-secret or already redacted before it lands — the token
/// itself lives only in the task's local variables and the request headers it
/// builds, never in a field another caller can read back.
#[derive(Default)]
struct Shared {
    /// Set once, by a non-resumable close code (4004, 4014). Once set the
    /// gateway task has exited and will not reconnect — a bad token or a
    /// disallowed intent is not something backoff fixes.
    permanent_error: Mutex<Option<String>>,
    /// The current resume state, mirrored out of the gateway task after every
    /// frame so `poll` can snapshot it into the durable cursor.
    resume: std::sync::Mutex<ResumeState>,
    /// One of the `GATEWAY_*` constants above. This is what the daemon's
    /// health loop reads through `live_transport`: `poll` returns an empty
    /// batch whether the socket is live or dropped, so it cannot answer this.
    gateway_status: std::sync::atomic::AtomicU8,
}

impl Shared {
    fn snapshot_resume_json(&self) -> Option<String> {
        if self
            .gateway_status
            .load(std::sync::atomic::Ordering::SeqCst)
            == GATEWAY_NOT_STARTED
        {
            return None;
        }
        let state = self.resume.lock().ok()?.clone();
        serde_json::to_string(&state).ok()
    }

    /// The current state, but only once it names no session to resume.
    ///
    /// The ordinary snapshot is taken before a poll waits, so the sequence it
    /// publishes can only lag the dispatches still in flight. A *cleared*
    /// state has no such hazard — it leads nothing and asks for nothing — so
    /// it is safe to publish at the end of a poll instead. That matters:
    /// clearing is how a session the provider discarded stops being resumed
    /// again after every restart.
    fn snapshot_discarded_resume_json(&self) -> Option<String> {
        let json = self.snapshot_resume_json()?;
        let state = self.resume.lock().ok()?.clone();
        (!state.resumable()).then_some(json)
    }
}

pub struct DiscordAdapter {
    account_id: String,
    token: String,
    http: reqwest::Client,
    /// Each envelope travels with the resume state as of its own dispatch, so
    /// the cursor persisted for a batch is exactly the state at its last
    /// message — never a sequence that leads an envelope still in this queue.
    inbound_tx: mpsc::Sender<(ChannelEnvelope, ResumeState)>,
    inbound_rx: Mutex<mpsc::Receiver<(ChannelEnvelope, ResumeState)>>,
    shared: Arc<Shared>,
    /// Guards the one-time spawn of the gateway task. See the module doc for
    /// why construction itself must stay side-effect-free.
    started: tokio::sync::OnceCell<()>,
    blobs: Arc<dyn BlobSource>,
    /// The REST origin. Always [`API_BASE`] in production; swappable in tests
    /// so the whole gateway handshake can run against a loopback fixture.
    api_base: String,
    /// Provider-side rate accounting, consulted before every REST send and
    /// fed by every REST response. One coordinator per adapter because the
    /// limits it mirrors are per bot account, not per call site.
    rate_limiter: std::sync::Mutex<RateLimiter>,
}

/// The provider accounts for rate limits the way it says it does, not per
/// channel: each response names the bucket the request drew from, and several
/// routes can share one bucket — so spending it through one channel must
/// gate the others too. A global 429 is wider still: it closes the whole
/// account until its retry window passes. Per-channel cooldowns capture
/// neither, which is why this coordinator replaces them.
///
/// Time is always passed in rather than read here, so tests can drive the
/// clock with constructed instants instead of sleeping.
#[derive(Default)]
struct RateLimiter {
    /// Route key ("POST:{channel_id}") -> the bucket id the provider named in
    /// X-RateLimit-Bucket for it. Learned from every response, so the mapping
    /// tracks the provider's own (re)grouping of routes.
    route_buckets: std::collections::HashMap<String, String>,
    /// Gate key -> when sends through it may resume. The key is the bucket id
    /// once one has been learned for the route, and the bare route key until
    /// then — an exhaustion recorded before the bucket was known is migrated
    /// forward rather than lost.
    gates: std::collections::HashMap<String, std::time::Instant>,
    /// Set by a global 429. Every route on this account waits it out,
    /// whatever bucket it maps to.
    global_until: Option<std::time::Instant>,
}

/// Hard cap on each limiter map. An adapter talks to a bounded set of live
/// channels, so the cap is never hit in practice; it exists so a pathological
/// spread of one-off routes cannot grow the state without bound.
const RATE_LIMIT_MAP_CAP: usize = 256;

/// Whole milliseconds until `until`, rounded up to at least one so a gate
/// that is still in the future never reports a zero wait.
fn ms_until(until: std::time::Instant, now: std::time::Instant) -> Option<u64> {
    let remaining = until.saturating_duration_since(now);
    (remaining > Duration::ZERO).then(|| (remaining.as_millis() as u64).max(1))
}

impl RateLimiter {
    /// The gate this route currently answers to.
    fn gate_key<'a>(&'a self, route: &'a str) -> &'a str {
        self.route_buckets
            .get(route)
            .map(String::as_str)
            .unwrap_or(route)
    }

    /// How long this route must wait before its next request, `None` when it
    /// is clear. The account-wide gate is checked first — it outranks any
    /// bucket — and the longer of the two waits is reported.
    fn wait_ms(&self, route: &str, now: std::time::Instant) -> Option<u64> {
        let global = self.global_until.and_then(|until| ms_until(until, now));
        let bucket = self
            .gates
            .get(self.gate_key(route))
            .and_then(|until| ms_until(*until, now));
        match (global, bucket) {
            (None, None) => None,
            (global, bucket) => Some(global.unwrap_or(0).max(bucket.unwrap_or(0))),
        }
    }

    /// Fold one response's verdict into the state: remember which bucket the
    /// route draws from, close the gate when the bucket is spent or the
    /// request was refused, and close the whole account on a global 429.
    #[allow(clippy::too_many_arguments)]
    fn on_response(
        &mut self,
        route: &str,
        status: u16,
        bucket: Option<&str>,
        remaining: Option<u64>,
        reset_after_ms: Option<u64>,
        retry_after_ms: Option<u64>,
        is_global: bool,
        now: std::time::Instant,
    ) {
        if let Some(bucket) = bucket {
            if self.route_buckets.get(route).map(String::as_str) != Some(bucket) {
                // An exhaustion recorded while the bucket was still unknown
                // lives under the bare route key; carry it to the bucket key
                // so learning the mapping cannot reopen a closed gate.
                if let Some(until) = self.gates.remove(route) {
                    let slot = self.gates.entry(bucket.to_string()).or_insert(until);
                    if *slot < until {
                        *slot = until;
                    }
                }
                self.insert_route_bucket(route, bucket, now);
            }
        }
        if remaining == Some(0) {
            if let Some(reset_after_ms) = reset_after_ms {
                self.insert_gate(
                    self.gate_key(route).to_string(),
                    now + Duration::from_millis(reset_after_ms),
                    now,
                );
            }
        }
        if status == 429 {
            if let Some(retry_after_ms) = retry_after_ms {
                let until = now + Duration::from_millis(retry_after_ms);
                if is_global {
                    self.global_until = Some(until);
                } else {
                    self.insert_gate(self.gate_key(route).to_string(), until, now);
                }
            }
        }
        // A passed global gate is dead state; drop it while we hold the
        // write lock anyway.
        if self.global_until.is_some_and(|until| until <= now) {
            self.global_until = None;
        }
    }

    fn insert_gate(&mut self, key: String, until: std::time::Instant, now: std::time::Instant) {
        // Passed gates are inert; sweeping them on write keeps the map sized
        // to what is actually still closed.
        self.gates.retain(|_, gate| *gate > now);
        if self.gates.len() >= RATE_LIMIT_MAP_CAP && !self.gates.contains_key(&key) {
            // Sacrifice the gate that opens soonest: losing it risks at most
            // one premature request near its own reset.
            if let Some(soonest) = self
                .gates
                .iter()
                .min_by_key(|(_, gate)| **gate)
                .map(|(key, _)| key.clone())
            {
                self.gates.remove(&soonest);
            }
        }
        self.gates.insert(key, until);
    }

    fn insert_route_bucket(&mut self, route: &str, bucket: &str, now: std::time::Instant) {
        if self.route_buckets.len() >= RATE_LIMIT_MAP_CAP && !self.route_buckets.contains_key(route)
        {
            // A dropped mapping is the cheapest loss here — the very next
            // response on that route teaches it again — so evict the one
            // whose gate opens soonest (or is already open).
            let victim = self
                .route_buckets
                .iter()
                .min_by_key(|(_, bucket)| self.gates.get(bucket.as_str()).copied().unwrap_or(now))
                .map(|(route, _)| route.clone());
            if let Some(victim) = victim {
                self.route_buckets.remove(&victim);
            }
        }
        self.route_buckets
            .insert(route.to_string(), bucket.to_string());
    }
}

/// What one REST response said about rate limiting, in one place so both
/// send paths feed the limiter identically.
#[derive(Default)]
struct RateLimitInfo {
    bucket: Option<String>,
    remaining: Option<u64>,
    reset_after_ms: Option<u64>,
    retry_after_ms: Option<u64>,
    is_global: bool,
}

/// Read the rate-limit headers off a response. Reset and retry windows come
/// as fractional seconds and are widened to milliseconds; the global flag
/// counts as set by presence, since it only ever accompanies a global 429.
fn parse_rate_limit_headers(headers: &reqwest::header::HeaderMap) -> RateLimitInfo {
    let text = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    RateLimitInfo {
        bucket: text("X-RateLimit-Bucket").map(str::to_string),
        remaining: text("X-RateLimit-Remaining").and_then(|value| value.parse().ok()),
        reset_after_ms: text("X-RateLimit-Reset-After")
            .and_then(|value| value.parse::<f64>().ok())
            .map(|seconds| (seconds * 1000.0).max(0.0) as u64),
        retry_after_ms: text("Retry-After")
            .and_then(|value| value.parse::<f64>().ok())
            .map(|seconds| (seconds * 1000.0).round().max(0.0) as u64),
        is_global: text("X-RateLimit-Global")
            .map(|value| !value.eq_ignore_ascii_case("false"))
            .unwrap_or(false),
    }
}

impl DiscordAdapter {
    pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
        if config.secret.is_empty() {
            return Err("This Discord account has no bot token configured".to_string());
        }
        let http = little_monkey_lib::egress::hardened()
            .build()
            .map_err(|error| format!("Failed to build the Discord HTTP client: {error}"))?;
        let (tx, rx) = mpsc::channel(INBOUND_CHANNEL_CAPACITY);
        Ok(Self {
            account_id: config.account.account_id.clone(),
            token: config.secret.clone(),
            http,
            inbound_tx: tx,
            inbound_rx: Mutex::new(rx),
            shared: Arc::new(Shared::default()),
            started: tokio::sync::OnceCell::new(),
            blobs: Arc::new(DaemonBlobs),
            api_base: API_BASE.to_string(),
            rate_limiter: std::sync::Mutex::new(RateLimiter::default()),
        })
    }

    /// Consult the limiter before one HTTP request goes out. `None` means
    /// clear to send. While nothing has been sent, a gated route is a clean
    /// retry carrying the provider's own wait. Once a chunk of this message
    /// has landed, a retry would deliver that chunk twice — so a short wait
    /// is served in place and a longer one parks the row as partially
    /// delivered.
    async fn gate_or_wait(&self, route: &str, any_sent: bool) -> Option<SendOutcome> {
        let wait_ms = {
            let limiter = self.rate_limiter.lock().ok()?;
            limiter.wait_ms(route, std::time::Instant::now())?
        };
        if !any_sent {
            return Some(SendOutcome::RetryableFailure {
                error: "Discord's rate-limit window for this route has not reset yet".to_string(),
                retry_after_ms: Some(wait_ms as i64),
            });
        }
        if wait_ms <= MAX_INTRA_MESSAGE_WAIT_MS {
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;
            return None;
        }
        Some(SendOutcome::NeedsReconciliation {
            error: format!(
                "Discord's rate limit holds the rest of the message for {wait_ms}ms \
                 after part of it was delivered"
            ),
        })
    }

    /// Feed one response's rate-limit verdict to the limiter and classify its
    /// status. Success hands the response back so the caller can read the
    /// created message out of it; any failure is returned as the mapped
    /// outcome. A 429's body is read here — it repeats `retry_after` and
    /// carries `global` when the headers do not, and a 429 never has a
    /// message id to preserve.
    async fn observe_response(
        &self,
        route: &str,
        any_sent: bool,
        response: reqwest::Response,
    ) -> Result<reqwest::Response, SendOutcome> {
        let status = response.status().as_u16();
        let mut info = parse_rate_limit_headers(response.headers());
        let response = if status == 429 {
            if let Ok(body) = response.json::<Value>().await {
                if info.retry_after_ms.is_none() {
                    info.retry_after_ms = body
                        .get("retry_after")
                        .and_then(Value::as_f64)
                        .map(|seconds| (seconds * 1000.0).round().max(0.0) as u64);
                }
                info.is_global =
                    info.is_global || body.get("global").and_then(Value::as_bool).unwrap_or(false);
            }
            None
        } else {
            Some(response)
        };
        if let Ok(mut limiter) = self.rate_limiter.lock() {
            limiter.on_response(
                route,
                status,
                info.bucket.as_deref(),
                info.remaining,
                info.reset_after_ms,
                info.retry_after_ms,
                info.is_global,
                std::time::Instant::now(),
            );
        }
        match map_send_status(status, any_sent, info.retry_after_ms.map(|ms| ms as i64)) {
            None => Ok(response.expect("only a 2xx maps to None, and a 2xx keeps its response")),
            Some(outcome) => Err(outcome),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_base_url(mut self, base: &str) -> Self {
        self.api_base = base.to_string();
        self
    }

    /// Start the gateway task once, seeded with the resume state persisted as
    /// this account's cursor — which is why only `poll`, which is handed the
    /// cursor, ever starts it. A RESUME with the stored session and sequence is
    /// what turns a daemon restart into a gap-free continuation instead of a
    /// fresh session that never sees what arrived while the process was down.
    async fn ensure_started(&self, cursor: Option<&str>) {
        self.started
            .get_or_init(|| async {
                let initial = ResumeState::parse(cursor);
                if let Ok(mut resume) = self.shared.resume.lock() {
                    *resume = initial;
                }
                self.shared
                    .gateway_status
                    .store(GATEWAY_RECONNECTING, std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(run_gateway_loop(
                    self.account_id.clone(),
                    self.token.clone(),
                    self.api_base.clone(),
                    self.http.clone(),
                    self.inbound_tx.clone(),
                    self.shared.clone(),
                ));
            })
            .await;
    }
}

#[async_trait]
impl ChannelAdapter for DiscordAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Discord
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            max_text_chars: DISCORD_MAX_TEXT_CHARS,
            supports_threads: true,
            supports_attachments: true,
            supports_mention_metadata: true,
            supports_idempotency_key: false,
            supports_delivery_receipts: false,
            ..ProviderCapabilities::minimal(ChannelKind::Discord, InboundTransport::Socket)
        }
    }

    /// The gateway's own state. Discord is a socket transport, so a poll
    /// coming back empty says nothing about whether the connection is live.
    /// `Connected` is reported for exactly one state: a live READY/RESUMED
    /// session. A never-started gateway is `Disconnected`, a dead one is
    /// `Error`, and anything in between is `Degraded`.
    fn live_transport(&self) -> Option<HealthState> {
        Some(
            match self
                .shared
                .gateway_status
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                GATEWAY_CONNECTED => HealthState::Connected,
                GATEWAY_NOT_STARTED => HealthState::Disconnected,
                GATEWAY_FAILED => HealthState::Error,
                _ => HealthState::Degraded,
            },
        )
    }

    async fn probe(&self) -> ChannelHealth {
        // Deliberately does not start the gateway: only `poll` holds the
        // persisted resume cursor, and a probe-started task would burn the
        // stored session by identifying fresh.
        let now = now_ms();
        if let Some(error) = self.shared.permanent_error.lock().await.clone() {
            return ChannelHealth::error(now, error);
        }
        let request = self
            .http
            .get(format!("{}/users/@me", self.api_base))
            .header("Authorization", format!("Bot {}", self.token));
        match little_monkey_lib::egress::send(request).await {
            Ok(response) if response.status().is_success() => {
                let body: Value = response.json().await.unwrap_or(Value::Null);
                let username = body
                    .get("username")
                    .and_then(Value::as_str)
                    .unwrap_or("bot");
                // The REST identity proves the token; the gateway status is
                // what proves messages can actually arrive. Both are reported,
                // and a live token with a down socket is degraded, not
                // connected — saved credentials are not a connection. The
                // match is exhaustive on purpose: `Connected` is written for
                // exactly one status, a live READY/RESUMED session, and every
                // other value — including one this code has never heard of —
                // reports the transport as not fully up.
                match self
                    .shared
                    .gateway_status
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    GATEWAY_CONNECTED => ChannelHealth::connected(
                        now,
                        Some(format!("Connected to Discord as {username}")),
                    ),
                    GATEWAY_NOT_STARTED => ChannelHealth {
                        state: little_monkey_lib::channels::types::HealthState::Disconnected,
                        detail: Some(format!(
                            "Authenticated to Discord as {username}; the gateway session has not started yet"
                        )),
                        last_error: None,
                        probed_at_ms: now,
                    },
                    _ => ChannelHealth {
                        state: little_monkey_lib::channels::types::HealthState::Degraded,
                        detail: Some(format!(
                            "Authenticated to Discord as {username}; the gateway socket is reconnecting"
                        )),
                        last_error: None,
                        probed_at_ms: now,
                    },
                }
            }
            Ok(response) => ChannelHealth::error(
                now,
                scrub(
                    &format!("Discord probe failed: HTTP {}", response.status()),
                    &self.token,
                ),
            ),
            Err(error) => ChannelHealth::error(
                now,
                scrub(&format!("Discord probe failed: {error}"), &self.token),
            ),
        }
    }

    async fn poll(&self, cursor: Option<&str>) -> Result<InboundBatch, String> {
        // The cursor is this account's persisted RESUME state; the first poll
        // after a restart seeds the gateway task with it.
        self.ensure_started(cursor).await;
        if let Some(error) = self.shared.permanent_error.lock().await.clone() {
            return Err(error);
        }
        // For an empty batch the persisted state is a snapshot taken BEFORE
        // the wait, which can only lag reality — safe, since a lagging RESUME
        // merely replays what the event log deduplicates. A non-empty batch
        // uses the state that traveled with its last envelope, which matches
        // that envelope's dispatch exactly.
        let early_snapshot = self.shared.snapshot_resume_json();
        let mut rx = self.inbound_rx.lock().await;
        let mut envelopes = Vec::new();
        let mut last_state: Option<ResumeState> = None;
        match tokio::time::timeout(POLL_WAIT, rx.recv()).await {
            Ok(Some((envelope, state))) => {
                envelopes.push(envelope);
                last_state = Some(state);
                while let Ok((envelope, state)) = rx.try_recv() {
                    envelopes.push(envelope);
                    last_state = Some(state);
                }
            }
            Ok(None) => {
                if let Some(error) = self.shared.permanent_error.lock().await.clone() {
                    return Err(error);
                }
            }
            Err(_) => {}
        }
        let cursor = match last_state {
            Some(state) => serde_json::to_string(&state).ok(),
            // A session discarded during this very wait is published now
            // rather than next time: the pre-wait snapshot still names it, and
            // persisting that would have the next restart resume a session
            // the provider has already refused.
            None => self
                .shared
                .snapshot_discarded_resume_json()
                .or(early_snapshot),
        };
        Ok(InboundBatch { envelopes, cursor })
    }

    async fn send(&self, message: &OutboundMessage) -> SendOutcome {
        // The limiter already knows when this route's bucket — or the whole
        // account — reopens; going to the network before then would spend the
        // request on a certain 429. Nothing has been sent yet, so a retry
        // with the provider's own wait is the honest outcome.
        let route = format!("POST:{}", target_channel(message));
        if let Some(outcome) = self.gate_or_wait(&route, false).await {
            return outcome;
        }
        if !message.attachments.is_empty() {
            let files = match load_attachments(self.blobs.as_ref(), message) {
                Ok(files) => files,
                Err(outcome) => return outcome,
            };
            return self.send_with_attachments(message, &files).await;
        }
        let mut chunks = split_message(&message.text, DISCORD_MAX_TEXT_CHARS);
        if chunks.is_empty() {
            chunks.push(String::new());
        }
        let mut any_sent = false;
        let mut last_id = None;
        for (index, chunk) in chunks.iter().enumerate() {
            // The earlier chunks may have spent the bucket; ask again before
            // each one rather than walking into the 429.
            if index > 0 {
                if let Some(outcome) = self.gate_or_wait(&route, any_sent).await {
                    return outcome;
                }
            }
            let mut body = serde_json::json!({ "content": chunk });
            if index == 0 {
                if let Some(reply_to) = &message.reply_to_provider_id {
                    body["message_reference"] = serde_json::json!({ "message_id": reply_to });
                }
            }
            let request = self
                .http
                .post(format!(
                    "{}/channels/{}/messages",
                    self.api_base,
                    target_channel(message)
                ))
                .header("Authorization", format!("Bot {}", self.token))
                .json(&body);
            let response = match little_monkey_lib::egress::send(request).await {
                Ok(response) => response,
                Err(error) => {
                    // Only a connect failure proves the request never left
                    // this machine. A reset mid-response may have delivered
                    // the message, and retrying that — or anything after a
                    // chunk already landed — posts it twice.
                    let is_connect = error.is_connect();
                    let error = scrub(&error.to_string(), &self.token);
                    return if is_connect && !any_sent {
                        SendOutcome::RetryableFailure {
                            error,
                            retry_after_ms: None,
                        }
                    } else {
                        SendOutcome::NeedsReconciliation { error }
                    };
                }
            };
            // Whatever the response said about the bucket lands in the
            // limiter before the status is classified, so even a failed
            // request teaches the next one when to go.
            let response = match self.observe_response(&route, any_sent, response).await {
                Ok(response) => response,
                Err(outcome) => return outcome,
            };
            any_sent = true;
            last_id = response
                .json::<Value>()
                .await
                .ok()
                .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_string));
        }
        SendOutcome::Sent {
            provider_message_id: last_id,
        }
    }
}

/// Where a message posts: the thread when one is targeted, else the channel.
/// A Discord thread is itself a channel id, so thread targeting is channel
/// selection — posting a thread reply to the parent channel would leak the
/// conversation out of its thread.
fn target_channel(message: &OutboundMessage) -> &str {
    message
        .thread_id
        .as_deref()
        .unwrap_or(&message.conversation_id)
}

impl DiscordAdapter {
    /// Discord takes files on the same message-create endpoint, as multipart
    /// with the JSON body in `payload_json` and each file in `files[n]`. One
    /// request carries the text and every file, so a caller never sees a
    /// caption arrive separately from what it captions.
    async fn send_with_attachments(
        &self,
        message: &OutboundMessage,
        files: &[LoadedAttachment],
    ) -> SendOutcome {
        // Text longer than one Discord message is sent first, exactly as the
        // text-only path splits it; the files then ride the final message.
        let mut leading = split_message(&message.text, DISCORD_MAX_TEXT_CHARS);
        let last = leading.pop().unwrap_or_default();
        let mut any_sent = false;
        if !leading.is_empty() {
            let head = OutboundMessage {
                text: leading.join(""),
                attachments: Vec::new(),
                ..message.clone()
            };
            match self.send(&head).await {
                SendOutcome::Sent { .. } => any_sent = true,
                other => return other,
            }
        }
        // The head chunks above may have spent the bucket, and this multipart
        // request is a REST send like any other — it must not skip the gate
        // just because it carries files.
        let route = format!("POST:{}", target_channel(message));
        if let Some(outcome) = self.gate_or_wait(&route, any_sent).await {
            return outcome;
        }
        let mut body = serde_json::json!({ "content": last });
        if !any_sent {
            if let Some(reply_to) = &message.reply_to_provider_id {
                body["message_reference"] = serde_json::json!({ "message_id": reply_to });
            }
        }
        let mut form = reqwest::multipart::Form::new().text("payload_json", body.to_string());
        for (index, file) in files.iter().enumerate() {
            let part = reqwest::multipart::Part::bytes(file.bytes.clone())
                .file_name(file.filename.clone())
                .mime_str(&file.mime_type)
                .unwrap_or_else(|_| {
                    reqwest::multipart::Part::bytes(file.bytes.clone())
                        .file_name(file.filename.clone())
                });
            form = form.part(format!("files[{index}]"), part);
        }
        let request = self
            .http
            .post(format!(
                "{}/channels/{}/messages",
                self.api_base,
                target_channel(message)
            ))
            .header("Authorization", format!("Bot {}", self.token))
            .multipart(form);
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => {
                // A connect failure provably never left this machine, so it is
                // safe to retry. Anything later may have uploaded the file
                // already, and a blind retry would post it twice.
                let is_connect = error.is_connect();
                let error = scrub(&error.to_string(), &self.token);
                return if is_connect && !any_sent {
                    SendOutcome::RetryableFailure {
                        error,
                        retry_after_ms: None,
                    }
                } else {
                    SendOutcome::NeedsReconciliation { error }
                };
            }
        };
        let response = match self.observe_response(&route, any_sent, response).await {
            Ok(response) => response,
            Err(outcome) => return outcome,
        };
        SendOutcome::Sent {
            provider_message_id: response
                .json::<Value>()
                .await
                .ok()
                .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_string)),
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Removes an exact-match token from a string before it is allowed anywhere
/// near a health record, a log, or an error surfaced to the caller.
fn scrub(text: &str, token: &str) -> String {
    if token.is_empty() {
        text.to_string()
    } else {
        text.replace(token, "[redacted]")
    }
}

/// Splits `text` into chunks of at most `limit` characters, on char
/// boundaries. Simple and predictable rather than word-aware: Discord's limit
/// is generous enough that mid-word splits are a rare, cosmetic cost.
fn split_message(text: &str, limit: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(limit.max(1))
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// Maps an HTTP status from `POST .../messages` to a terminal [`SendOutcome`],
/// or `None` when the caller should treat it as success and continue (the next
/// chunk, or done). Pulled out of [`DiscordAdapter::send`] so the mapping is
/// testable without a socket or an HTTP server.
fn map_send_status(
    status: u16,
    any_sent_before: bool,
    retry_after_ms: Option<i64>,
) -> Option<SendOutcome> {
    // Once any chunk of this logical message has been delivered, every
    // failure — including a rate limit that would otherwise be a plain
    // retry — parks the row instead: the outbox retries a whole message
    // from its first chunk, and a blind retry would deliver that chunk
    // twice. A permanent rejection after a delivered chunk is parked for
    // the same reason, so the half-delivered message is visible to an
    // operator rather than silently truncated.
    if any_sent_before && !(200..=299).contains(&status) {
        return Some(SendOutcome::NeedsReconciliation {
            error: format!("Discord returned HTTP {status} after part of the message was sent"),
        });
    }
    match status {
        200..=299 => None,
        429 => Some(SendOutcome::RetryableFailure {
            error: "Discord rate limited the request".to_string(),
            retry_after_ms,
        }),
        401 | 403 => Some(SendOutcome::PermanentFailure {
            error: format!("Discord rejected the request: HTTP {status}"),
        }),
        500..=599 => Some(SendOutcome::RetryableFailure {
            error: format!("Discord returned HTTP {status}"),
            retry_after_ms: None,
        }),
        _ => Some(SendOutcome::PermanentFailure {
            error: format!("Discord rejected the message: HTTP {status}"),
        }),
    }
}

// ---------------------------------------------------------------------------
// Gateway state machine (pure)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
struct GatewayState {
    seq: Option<u64>,
    session_id: Option<String>,
    /// Where Discord said this session must be resumed, from READY. RESUMEs
    /// sent to the general gateway endpoint may be rejected outright.
    resume_gateway_url: Option<String>,
    bot_user_id: Option<String>,
    /// Set by a resumable close/RECONNECT/INVALID_SESSION(resumable=true); read
    /// by the next HELLO to decide RESUME vs a fresh IDENTIFY.
    pending_resume: bool,
    /// Thread channel id -> parent channel id: learned from THREAD_CREATE /
    /// THREAD_UPDATE dispatches, seeded from the persisted [`ResumeState`]
    /// after a restart, and filled by a REST lookup when a message arrives in
    /// a guild channel this map has never heard of. Persisted as part of the
    /// resume cursor, so a thread known before a restart is known after it.
    thread_parents: std::collections::BTreeMap<String, String>,
    /// Guild channels the REST API confirmed are not threads, so one plain
    /// channel does not cost a lookup per message. Memory-only on purpose: a
    /// wrong entry costs one extra REST call after a restart, never a
    /// misrouted message.
    known_non_threads: std::collections::HashSet<String>,
}

impl GatewayState {
    /// A state seeded from what the last run of this daemon persisted. The
    /// stored sequence may lag by whatever was in flight around the crash,
    /// which is safe: a lagging RESUME replays events the durable event log
    /// deduplicates, where a fresh IDENTIFY would never see them at all.
    fn from_resume(resume: &ResumeState) -> Self {
        Self {
            seq: resume.seq,
            session_id: resume.session_id.clone(),
            resume_gateway_url: resume.resume_gateway_url.clone(),
            bot_user_id: resume.bot_user_id.clone(),
            pending_resume: resume.resumable(),
            thread_parents: resume.thread_parents.clone(),
            ..Self::default()
        }
    }

    fn resume_snapshot(&self) -> ResumeState {
        ResumeState {
            session_id: self.session_id.clone(),
            resume_gateway_url: self.resume_gateway_url.clone(),
            seq: self.seq,
            bot_user_id: self.bot_user_id.clone(),
            thread_parents: self.thread_parents.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Action {
    SendJson(Value),
    /// An IDENTIFY, separate from `SendJson` because the I/O loop must space
    /// these five seconds apart per Discord's identify rate limit.
    Identify(Value),
    StartHeartbeat(u64),
    /// Discord acknowledged our heartbeat; the I/O loop clears its
    /// missed-ACK flag.
    HeartbeatAck,
    /// READY or RESUMED arrived: the session is live, backoff may reset.
    Established,
    SetBotUserId(String),
    Envelope(Box<ChannelEnvelope>),
    /// A message arrived in a guild channel the thread map has never heard
    /// of. The I/O loop resolves the channel through the REST API — it might
    /// be a thread that existed before this process did — records the answer
    /// in the state, and only then normalizes the carried dispatch.
    ResolveChannelThenNormalize {
        channel_id: String,
        data: Value,
    },
    PermanentError(String),
    /// Tear the socket down and reconnect after at least `delay_ms` —
    /// non-zero only for INVALID_SESSION, where Discord asks for a short
    /// randomized wait before the next attempt.
    Reconnect {
        delay_ms: u64,
    },
}

fn identify_payload(token: &str, intents: u64) -> Value {
    serde_json::json!({
        "op": 2,
        "d": {
            "token": token,
            "intents": intents,
            "properties": {
                "os": "linux",
                "browser": "little-monkey",
                "device": "little-monkey",
            },
        }
    })
}

fn resume_payload(token: &str, session_id: &str, seq: u64) -> Value {
    serde_json::json!({
        "op": 6,
        "d": { "token": token, "session_id": session_id, "seq": seq },
    })
}

/// Handles one gateway text frame, mutating `state` and returning what the
/// caller should do about it. Never touches a socket, a clock, or a channel —
/// every side effect is expressed as a returned [`Action`].
fn handle_frame(
    state: &mut GatewayState,
    account_id: &str,
    frame: &str,
    token: &str,
) -> Vec<Action> {
    let Ok(value) = serde_json::from_str::<Value>(frame) else {
        return Vec::new();
    };
    if let Some(seq) = value.get("s").and_then(Value::as_u64) {
        state.seq = Some(seq);
    }
    match value.get("op").and_then(Value::as_i64).unwrap_or(-1) {
        // HELLO
        10 => {
            let interval = value["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250);
            let mut actions = vec![Action::StartHeartbeat(interval)];
            if state.pending_resume {
                if let (Some(session_id), Some(seq)) = (state.session_id.clone(), state.seq) {
                    actions.push(Action::SendJson(resume_payload(token, &session_id, seq)));
                    return actions;
                }
            }
            state.session_id = None;
            actions.push(Action::Identify(identify_payload(token, DISCORD_INTENTS)));
            actions
        }
        // DISPATCH
        0 => {
            let event_type = value.get("t").and_then(Value::as_str).unwrap_or_default();
            let data = value.get("d").cloned().unwrap_or(Value::Null);
            let mut actions = Vec::new();
            match event_type {
                "READY" => {
                    state.pending_resume = false;
                    if let Some(session_id) = data.get("session_id").and_then(Value::as_str) {
                        state.session_id = Some(session_id.to_string());
                    }
                    // Where THIS session must be resumed. Discord may reject a
                    // RESUME aimed at the general gateway endpoint.
                    if let Some(url) = data.get("resume_gateway_url").and_then(Value::as_str) {
                        state.resume_gateway_url = Some(url.to_string());
                    }
                    if let Some(id) = data
                        .get("user")
                        .and_then(|user| user.get("id"))
                        .and_then(Value::as_str)
                    {
                        state.bot_user_id = Some(id.to_string());
                        actions.push(Action::SetBotUserId(id.to_string()));
                    }
                    actions.push(Action::Established);
                }
                "RESUMED" => {
                    state.pending_resume = false;
                    actions.push(Action::Established);
                }
                "THREAD_CREATE" | "THREAD_UPDATE" => {
                    if let (Some(id), Some(parent)) = (
                        data.get("id").and_then(Value::as_str),
                        data.get("parent_id").and_then(Value::as_str),
                    ) {
                        insert_thread_parent(
                            &mut state.thread_parents,
                            id.to_string(),
                            parent.to_string(),
                        );
                    }
                }
                "THREAD_DELETE" => {
                    if let Some(id) = data.get("id").and_then(Value::as_str) {
                        state.thread_parents.remove(id);
                    }
                }
                "MESSAGE_CREATE" => {
                    // A guild channel this process has never classified could
                    // be a thread that predates it — only the REST API can
                    // say, and the I/O loop is where a REST call can happen.
                    // DM channels are never threads, and a channel already in
                    // either map is already answered.
                    let channel_id = data.get("channel_id").and_then(Value::as_str);
                    let needs_resolution = data.get("guild_id").is_some()
                        && channel_id.is_some_and(|id| {
                            !state.thread_parents.contains_key(id)
                                && !state.known_non_threads.contains(id)
                        });
                    if needs_resolution {
                        actions.push(Action::ResolveChannelThenNormalize {
                            channel_id: channel_id.unwrap_or_default().to_string(),
                            data,
                        });
                    } else if let Some(envelope) = normalize_message_create(
                        account_id,
                        &data,
                        state.bot_user_id.as_deref(),
                        &state.thread_parents,
                        now_ms(),
                    ) {
                        actions.push(Action::Envelope(Box::new(envelope)));
                    }
                }
                _ => {}
            }
            actions
        }
        // Gateway-requested heartbeat
        1 => vec![Action::SendJson(
            serde_json::json!({ "op": 1, "d": state.seq }),
        )],
        // RECONNECT: always resumable.
        7 => {
            state.pending_resume = true;
            vec![Action::Reconnect { delay_ms: 0 }]
        }
        // INVALID_SESSION: `d` says whether a resume is worth attempting.
        // Either way Discord asks for a short randomized wait before the next
        // attempt, so a burst of invalid sessions cannot become an
        // identify storm.
        9 => {
            let resumable = value.get("d").and_then(Value::as_bool).unwrap_or(false);
            state.pending_resume = resumable;
            if !resumable {
                state.session_id = None;
                state.seq = None;
                state.resume_gateway_url = None;
            }
            vec![Action::Reconnect {
                delay_ms: 1_000 + jitter_ms(4_000),
            }]
        }
        // Heartbeat ACK: clears the I/O loop's missed-ACK flag. A connection
        // whose ACKs stop arriving is a zombie — TCP-alive, Discord-dead —
        // and only this opcode can prove liveness.
        11 => vec![Action::HeartbeatAck],
        _ => Vec::new(),
    }
}

/// A cheap jitter source: the sub-second nanos of the wall clock, folded into
/// `0..range_ms`. Not cryptographic and does not need to be — it only has to
/// keep a fleet of reconnecting clients from thundering in step.
fn jitter_ms(range_ms: u64) -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::from(duration.subsec_nanos()))
        .unwrap_or(0)
        % range_ms.max(1)
}

/// What one Gateway close code means for the connection after it.
///
/// Three outcomes, because the provider's close-code table has exactly three
/// kinds of entry, and collapsing any two of them costs something real:
/// resuming a session the provider has already discarded burns a reconnect and
/// earns the same close again, and reconnecting at all after a configuration
/// rejection spends session-start allowance on an attempt that cannot succeed.
///
/// Decided here, away from any socket, so every code can be asserted directly.
#[derive(Debug, Clone, PartialEq)]
enum CloseDisposition {
    /// The session outlives the drop: reconnect to its resume URL and RESUME.
    Resume,
    /// The session is gone but the credential is fine: discard the stored
    /// session and sequence, then IDENTIFY a fresh one.
    Reidentify,
    /// Nothing about this account can connect until an operator changes
    /// something. The gateway task stops and health says why.
    Fatal { reason: String },
}

/// The provider's close-code table, as a decision.
///
/// An ordinary network drop never arrives here as a close frame at all — the
/// caller treats a read error or a stream end as [`CloseDisposition::Resume`],
/// which is what those are.
fn close_disposition(code: u16) -> CloseDisposition {
    let fatal = |reason: &str| CloseDisposition::Fatal {
        reason: reason.to_string(),
    };
    match code {
        // The session's own identity is what the provider rejected: its
        // sequence is unusable, or it has already been reaped. Resuming it
        // again would earn the same close, forever.
        4007 => CloseDisposition::Reidentify,
        4009 => CloseDisposition::Reidentify,
        // Sent before IDENTIFY was accepted, so there is no session to resume.
        4003 => CloseDisposition::Reidentify,
        4004 => fatal(
            "Discord rejected the bot token (close code 4004: authentication failed). Set a \
             valid bot token for this account and re-enable it",
        ),
        4010 => fatal(
            "Discord refused the shard this connection identified with (close code 4010: \
             invalid shard). This adapter connects one unsharded session per account",
        ),
        4011 => fatal(
            "This bot is on too many guilds for a single gateway connection and Discord now \
             requires it to shard (close code 4011). Sharding is not supported for a channel \
             account; use a bot in fewer guilds",
        ),
        4012 => fatal(
            "Discord refused the gateway API version this build requests (close code 4012: \
             invalid API version). Update Little Monkey",
        ),
        4013 => fatal(
            "Discord refused the gateway intents this build requests as malformed (close code \
             4013: invalid intents). Update Little Monkey",
        ),
        4014 => fatal(
            "Discord refused the requested gateway intents (close code 4014). If the bot should \
             read message text, enable the Message Content intent for it in the Discord \
             developer portal under Bot → Privileged Gateway Intents, then re-enable this account",
        ),
        _ => CloseDisposition::Resume,
    }
}

/// Applies a close code to the state and says what the I/O loop should do.
///
/// A `Reidentify` clears the stored session here, in the state the caller
/// mirrors into the durable cursor — so a session the provider has discarded
/// cannot come back after a restart and ask to be resumed again.
fn handle_close(state: &mut GatewayState, code: u16) -> Vec<Action> {
    match close_disposition(code) {
        CloseDisposition::Fatal { reason } => vec![Action::PermanentError(reason)],
        CloseDisposition::Reidentify => {
            state.pending_resume = false;
            state.session_id = None;
            state.seq = None;
            state.resume_gateway_url = None;
            vec![Action::Reconnect { delay_ms: 0 }]
        }
        CloseDisposition::Resume => {
            state.pending_resume = true;
            vec![Action::Reconnect { delay_ms: 0 }]
        }
    }
}

fn normalize_message_create(
    account_id: &str,
    data: &Value,
    bot_user_id: Option<&str>,
    thread_parents: &std::collections::BTreeMap<String, String>,
    now_ms: i64,
) -> Option<ChannelEnvelope> {
    let id = data.get("id")?.as_str()?.to_string();
    let channel_id = data.get("channel_id")?.as_str()?.to_string();
    let author = data.get("author")?;
    let author_id = author.get("id")?.as_str()?.to_string();
    let username = author
        .get("username")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.is_empty());
    let is_bot = author.get("bot").and_then(Value::as_bool).unwrap_or(false);
    let is_self = bot_user_id.is_some_and(|bot_id| bot_id == author_id);

    let (conversation_id, thread_id) = match thread_parents.get(&channel_id) {
        Some(parent) => (parent.clone(), Some(channel_id.clone())),
        None => (channel_id.clone(), None),
    };
    let is_dm = data.get("guild_id").is_none();
    let conversation = if is_dm {
        ChannelConversation::direct(conversation_id)
    } else {
        ChannelConversation::group(conversation_id)
    }
    .with_thread(thread_id);

    let mentions_self = bot_user_id.is_some_and(|bot_id| {
        data.get("mentions")
            .and_then(Value::as_array)
            .is_some_and(|mentions| {
                mentions
                    .iter()
                    .any(|mention| mention.get("id").and_then(Value::as_str) == Some(bot_id))
            })
    });

    let text = data
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let attachments = data
        .get("attachments")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|attachment| {
                    let url = attachment.get("url")?.as_str()?.to_string();
                    let mime_type = attachment
                        .get("content_type")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let kind = mime_type
                        .as_deref()
                        .map(AttachmentKind::from_mime)
                        .unwrap_or(AttachmentKind::Other);
                    Some(ChannelAttachment {
                        stored_artifact_id: None,
                        text_excerpt: None,
                        fetch_error: None,
                        provider_id: attachment
                            .get("id")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        kind,
                        filename: attachment
                            .get("filename")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        mime_type,
                        declared_size_bytes: attachment.get("size").and_then(Value::as_u64),
                        stored_size_bytes: None,
                        source: AttachmentSource::Url { url },
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let reply_to_provider_id = data
        .get("message_reference")
        .and_then(|reference| reference.get("message_id"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut metadata = little_monkey_lib::channels::types::BoundedMetadata::new();
    if let Some(guild_id) = data.get("guild_id").and_then(Value::as_str) {
        metadata.insert("guild_id", guild_id);
    }

    Some(ChannelEnvelope {
        account_id: account_id.to_string(),
        kind: ChannelKind::Discord,
        provider_event_id: id,
        conversation,
        sender: ChannelSender {
            sender_id: author_id,
            display_label: username,
            is_self,
            is_bot,
        },
        text,
        attachments,
        reply_to_provider_id,
        mentions_self,
        received_at_ms: now_ms,
        metadata,
    })
}

// ---------------------------------------------------------------------------
// Gateway I/O loop
// ---------------------------------------------------------------------------

enum FetchError {
    Retryable(String),
    Permanent(String),
}

/// Discord's session-start allowance, from `/gateway/bot`'s
/// `session_start_limit`. Only IDENTIFYs spend it — a RESUME continues an
/// existing session and costs nothing, which is why the resume path skips
/// the lookup entirely. `total` and `max_concurrency` are parsed because
/// they are part of the provider's contract (`max_concurrency` is honored by
/// [`IDENTIFY_SPACING`]: a single-account client is one shard in bucket
/// zero); `remaining`/`reset_at` are what admission actually gates on.
#[derive(Debug, Clone, PartialEq)]
struct SessionStartBudget {
    #[allow(dead_code)]
    total: u64,
    remaining: u64,
    reset_at: std::time::Instant,
    #[allow(dead_code)]
    max_concurrency: u64,
}

impl SessionStartBudget {
    fn parse(body: &Value, now: std::time::Instant) -> Option<Self> {
        let limit = body.get("session_start_limit")?;
        Some(Self {
            total: limit.get("total").and_then(Value::as_u64)?,
            remaining: limit.get("remaining").and_then(Value::as_u64)?,
            reset_at: now
                + Duration::from_millis(limit.get("reset_after").and_then(Value::as_u64)?),
            max_concurrency: limit
                .get("max_concurrency")
                .and_then(Value::as_u64)
                .unwrap_or(1),
        })
    }

    /// How long an IDENTIFY must wait for the allowance. Zero while starts
    /// remain, zero once Discord's own reset time has passed (the next
    /// `/gateway/bot` fetch re-reads the real numbers), else the time left
    /// until that reset.
    fn wait_before_identify(&self, now: std::time::Instant) -> Duration {
        if self.remaining > 0 {
            return Duration::ZERO;
        }
        self.reset_at.saturating_duration_since(now)
    }

    /// One IDENTIFY went on the wire; the local mirror of the budget drops
    /// with it so a reconnect storm cannot outspend what Discord granted
    /// between two `/gateway/bot` fetches.
    fn note_identify(&mut self) {
        self.remaining = self.remaining.saturating_sub(1);
    }
}

async fn fetch_gateway_url(
    http: &reqwest::Client,
    api_base: &str,
    token: &str,
) -> Result<(String, Option<SessionStartBudget>), FetchError> {
    let request = http
        .get(format!("{api_base}/gateway/bot"))
        .header("Authorization", format!("Bot {token}"));
    let response = little_monkey_lib::egress::send(request)
        .await
        .map_err(|error| FetchError::Retryable(scrub(&error.to_string(), token)))?;
    let status = response.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(FetchError::Permanent(
            "Discord rejected the bot token while resolving the gateway URL".to_string(),
        ));
    }
    if !status.is_success() {
        return Err(FetchError::Retryable(format!(
            "Discord gateway lookup failed: HTTP {status}"
        )));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|error| FetchError::Retryable(scrub(&error.to_string(), token)))?;
    let budget = SessionStartBudget::parse(&body, std::time::Instant::now());
    body.get("url")
        .and_then(Value::as_str)
        .map(|url| (url.to_string(), budget))
        .ok_or_else(|| FetchError::Retryable("Discord gateway response had no url".to_string()))
}

/// Ask the REST API what a channel is: `Some(parent)` for a thread (types
/// 10/11/12 all carry their parent channel), `None` for anything else, `Err`
/// when Discord did not answer — in which case nothing is cached and the
/// message at hand is treated as a plain channel message.
async fn resolve_thread_parent(
    http: &reqwest::Client,
    api_base: &str,
    token: &str,
    channel_id: &str,
) -> Result<Option<String>, String> {
    let request = http
        .get(format!("{api_base}/channels/{channel_id}"))
        .header("Authorization", format!("Bot {token}"));
    let response = little_monkey_lib::egress::send(request)
        .await
        .map_err(|error| scrub(&error.to_string(), token))?;
    if !response.status().is_success() {
        return Err(format!(
            "Discord returned HTTP {} for the channel lookup",
            response.status()
        ));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|error| scrub(&error.to_string(), token))?;
    let is_thread = matches!(body.get("type").and_then(Value::as_i64), Some(10..=12));
    if !is_thread {
        return Ok(None);
    }
    Ok(body
        .get("parent_id")
        .and_then(Value::as_str)
        .map(str::to_string))
}

enum ConnectionOutcome {
    Permanent(String),
    /// Reconnect, saying whether this connection ever reached READY/RESUMED
    /// (a connection that never established must keep growing the backoff —
    /// resetting on every attempt is how a flapping socket becomes a 1-second
    /// hammer) and how long Discord asked us to wait first.
    Reconnect {
        established: bool,
        delay_ms: u64,
    },
    /// The adapter was dropped; the task must exit, not reconnect.
    Shutdown,
}

#[allow(clippy::too_many_arguments)]
async fn run_one_connection(
    account_id: &str,
    token: &str,
    gateway_url: &str,
    http: &reqwest::Client,
    api_base: &str,
    tx: &mpsc::Sender<(ChannelEnvelope, ResumeState)>,
    state: &mut GatewayState,
    shared: &Shared,
    last_identify: &mut Option<std::time::Instant>,
    budget: &mut Option<SessionStartBudget>,
) -> ConnectionOutcome {
    let reconnect = |established: bool| ConnectionOutcome::Reconnect {
        established,
        delay_ms: 0,
    };
    let full_url = format!(
        "{}/?v={GATEWAY_VERSION}&encoding=json",
        gateway_url.trim_end_matches('/')
    );
    let (mut ws, _) = match tokio_tungstenite::connect_async(full_url).await {
        Ok(pair) => pair,
        Err(_) => return reconnect(false),
    };

    let mut heartbeat_armed = false;
    let mut awaiting_ack = false;
    let mut established = false;
    // Placeholder ticker; replaced once HELLO tells us the real interval. Never
    // fires before then because nothing selects on it until `heartbeat_armed`.
    let mut ticker = tokio::time::interval(Duration::from_secs(3600));
    ticker.tick().await;

    loop {
        tokio::select! {
            // The adapter handle was dropped (account disabled, credential
            // rotated). Hang up now rather than lingering as a second socket
            // owner beside the replacement adapter's session.
            _ = tx.closed() => {
                let _ = ws.close(None).await;
                return ConnectionOutcome::Shutdown;
            }
            frame = ws.next() => {
                let text = match frame {
                    Some(Ok(Message::Text(text))) => text.to_string(),
                    Some(Ok(Message::Close(close_frame))) => {
                        let code = close_frame.map(|f| f.code.into()).unwrap_or(1000u16);
                        let actions = handle_close(state, code);
                        // A close that discarded the session cleared it in
                        // `state`; mirroring here is what carries that to the
                        // durable cursor, so a restart identifies fresh
                        // instead of resuming what the provider threw away.
                        if let Ok(mut resume) = shared.resume.lock() {
                            *resume = state.resume_snapshot();
                        }
                        return match actions.into_iter().next() {
                            Some(Action::PermanentError(error)) => ConnectionOutcome::Permanent(error),
                            _ => reconnect(established),
                        };
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = ws.send(Message::Pong(payload)).await;
                        continue;
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => return reconnect(established),
                };
                let actions = handle_frame(state, account_id, &text, token);
                // Mirror the resume-relevant state out after every frame, so a
                // concurrent `poll` snapshots a sequence that lags what it is
                // about to drain rather than leading it.
                if let Ok(mut resume) = shared.resume.lock() {
                    *resume = state.resume_snapshot();
                }
                for action in actions {
                    match action {
                        Action::SendJson(payload) => {
                            let _ = ws.send(Message::Text(payload.to_string().into())).await;
                        }
                        Action::Identify(payload) => {
                            // Session-start admission first: an IDENTIFY when
                            // `session_start_limit.remaining` is spent earns a
                            // rejected session AND burns the next allowance.
                            // A short wait is served on the open socket; a
                            // long one hangs up and waits outside, where the
                            // next `/gateway/bot` fetch re-reads the budget.
                            let budget_wait = budget
                                .as_ref()
                                .map(|budget| {
                                    budget.wait_before_identify(std::time::Instant::now())
                                })
                                .unwrap_or(Duration::ZERO);
                            if budget_wait > MAX_IDENTIFY_HOLD {
                                let _ = ws.close(None).await;
                                return ConnectionOutcome::Reconnect {
                                    established,
                                    delay_ms: budget_wait.as_millis() as u64,
                                };
                            }
                            if budget_wait > Duration::ZERO {
                                tokio::time::sleep(budget_wait).await;
                            }
                            // At most one IDENTIFY per five seconds, measured
                            // across reconnects: violating it earns an invalid
                            // session and another reconnect, forever.
                            if let Some(previous) = *last_identify {
                                let since = previous.elapsed();
                                if since < IDENTIFY_SPACING {
                                    tokio::time::sleep(IDENTIFY_SPACING - since).await;
                                }
                            }
                            *last_identify = Some(std::time::Instant::now());
                            if let Some(budget) = budget.as_mut() {
                                budget.note_identify();
                            }
                            let _ = ws.send(Message::Text(payload.to_string().into())).await;
                        }
                        Action::StartHeartbeat(interval_ms) => {
                            let interval = Duration::from_millis(interval_ms.max(1));
                            // First beat after a random fraction of the
                            // interval, as the gateway spec asks, so a fleet
                            // of clients does not heartbeat in step.
                            ticker = tokio::time::interval_at(
                                tokio::time::Instant::now()
                                    + Duration::from_millis(jitter_ms(interval_ms.max(1))),
                                interval,
                            );
                            heartbeat_armed = true;
                            awaiting_ack = false;
                        }
                        Action::HeartbeatAck => awaiting_ack = false,
                        Action::Established => {
                            established = true;
                            shared.gateway_status.store(
                                GATEWAY_CONNECTED,
                                std::sync::atomic::Ordering::SeqCst,
                            );
                        }
                        // The id itself is captured into GatewayState by
                        // handle_frame; liveness is Action::Established's job.
                        Action::SetBotUserId(_id) => {}
                        Action::Envelope(envelope) => {
                            // The state rides along, frozen at this dispatch.
                            let _ = tx.send((*envelope, state.resume_snapshot())).await;
                        }
                        Action::ResolveChannelThenNormalize { channel_id, data } => {
                            // The channel might be a thread older than this
                            // process. Ask once, cache the answer (threads in
                            // the persisted map, plain channels in memory),
                            // and treat a lookup failure as a plain channel
                            // for this message only — uncached, so the next
                            // message asks again. This await runs inside the
                            // select loop, so heartbeats stall until it
                            // returns; the explicit timeout here is the real
                            // bound on that stall — the HTTP client's own
                            // read timeout is minutes, far past the ~41s
                            // heartbeat interval.
                            match tokio::time::timeout(
                                Duration::from_secs(5),
                                resolve_thread_parent(http, api_base, token, &channel_id),
                            )
                            .await
                            .unwrap_or_else(|_| {
                                Err("the channel lookup timed out".to_string())
                            }) {
                                Ok(Some(parent)) => {
                                    insert_thread_parent(
                                        &mut state.thread_parents,
                                        channel_id,
                                        parent,
                                    );
                                }
                                Ok(None) => {
                                    state.known_non_threads.insert(channel_id);
                                }
                                Err(error) => {
                                    eprintln!(
                                        "little monkey: discord[{account_id}] channel lookup: {error}"
                                    );
                                }
                            }
                            if let Ok(mut resume) = shared.resume.lock() {
                                *resume = state.resume_snapshot();
                            }
                            if let Some(envelope) = normalize_message_create(
                                account_id,
                                &data,
                                state.bot_user_id.as_deref(),
                                &state.thread_parents,
                                now_ms(),
                            ) {
                                let _ = tx.send((envelope, state.resume_snapshot())).await;
                            }
                        }
                        Action::PermanentError(error) => return ConnectionOutcome::Permanent(error),
                        Action::Reconnect { delay_ms } => {
                            return ConnectionOutcome::Reconnect { established, delay_ms };
                        }
                    }
                }
            }
            _ = ticker.tick(), if heartbeat_armed => {
                if awaiting_ack {
                    // A full interval passed with no ACK: the connection is a
                    // zombie. Close and resume — the sequence is still good.
                    state.pending_resume = true;
                    let _ = ws.close(None).await;
                    return reconnect(established);
                }
                awaiting_ack = true;
                let payload = serde_json::json!({ "op": 1, "d": state.seq });
                let _ = ws.send(Message::Text(payload.to_string().into())).await;
            }
        }
    }
}

async fn run_gateway_loop(
    account_id: String,
    token: String,
    api_base: String,
    http: reqwest::Client,
    tx: mpsc::Sender<(ChannelEnvelope, ResumeState)>,
    shared: Arc<Shared>,
) {
    let mut backoff = MIN_BACKOFF;
    // Seeded from the persisted cursor, so the first connection after a daemon
    // restart RESUMEs the stored session instead of identifying fresh.
    let mut state =
        GatewayState::from_resume(&shared.resume.lock().map(|r| r.clone()).unwrap_or_default());
    let mut last_identify: Option<std::time::Instant> = None;
    // The session-start allowance, refreshed by every `/gateway/bot` fetch.
    // A RESUME skips both the fetch and the spend — continuing a session
    // costs no allowance, which is the whole point of preferring it.
    let mut budget: Option<SessionStartBudget> = None;
    loop {
        // A RESUME must go to the URL Discord named for this session; only a
        // fresh IDENTIFY goes through the general gateway lookup.
        let resume_url = (state.pending_resume && state.session_id.is_some())
            .then(|| state.resume_gateway_url.clone())
            .flatten();
        let will_identify = resume_url.is_none();
        let url = match resume_url {
            Some(url) => Ok(url),
            None => match fetch_gateway_url(&http, &api_base, &token).await {
                Ok((url, fresh_budget)) => {
                    // The fetch is authoritative; a missing block keeps the
                    // last known numbers rather than forgetting them.
                    if fresh_budget.is_some() {
                        budget = fresh_budget;
                    }
                    Ok(url)
                }
                Err(error) => Err(error),
            },
        };
        // Admission before the socket ever opens: a connection whose HELLO
        // cannot be answered with an IDENTIFY is a connection Discord will
        // tear down, and opening it anyway is how a reconnect storm spends
        // the whole session-start budget in one bad minute.
        if will_identify && url.is_ok() {
            if let Some(wait) = budget
                .as_ref()
                .map(|budget| budget.wait_before_identify(std::time::Instant::now()))
                .filter(|wait| *wait > Duration::ZERO)
            {
                eprintln!(
                    "little monkey: discord[{account_id}] session-start allowance exhausted; \
                     waiting {}s before the next IDENTIFY",
                    wait.as_secs().max(1)
                );
                tokio::time::sleep(wait + Duration::from_millis(jitter_ms(500))).await;
                if tx.is_closed() {
                    return;
                }
            }
        }
        let mut delay_hint_ms = 0;
        match url {
            Ok(url) => {
                match run_one_connection(
                    &account_id,
                    &token,
                    &url,
                    &http,
                    &api_base,
                    &tx,
                    &mut state,
                    &shared,
                    &mut last_identify,
                    &mut budget,
                )
                .await
                {
                    ConnectionOutcome::Permanent(error) => {
                        *shared.permanent_error.lock().await = Some(error);
                        shared
                            .gateway_status
                            .store(GATEWAY_FAILED, std::sync::atomic::Ordering::SeqCst);
                        return;
                    }
                    ConnectionOutcome::Shutdown => return,
                    ConnectionOutcome::Reconnect {
                        established,
                        delay_ms,
                    } => {
                        delay_hint_ms = delay_ms;
                        backoff = if established {
                            MIN_BACKOFF
                        } else {
                            (backoff * 2).min(MAX_BACKOFF)
                        };
                    }
                }
            }
            Err(FetchError::Permanent(error)) => {
                shared
                    .gateway_status
                    .store(GATEWAY_FAILED, std::sync::atomic::Ordering::SeqCst);
                *shared.permanent_error.lock().await = Some(error);
                return;
            }
            Err(FetchError::Retryable(error)) => {
                eprintln!("little monkey: discord[{account_id}] gateway lookup: {error}");
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
        shared
            .gateway_status
            .store(GATEWAY_RECONNECTING, std::sync::atomic::Ordering::SeqCst);
        if tx.is_closed() {
            return;
        }
        // Jittered, and never shorter than what Discord explicitly asked for.
        let wait = backoff + Duration::from_millis(jitter_ms(500));
        let wait = wait.max(Duration::from_millis(delay_hint_ms));
        tokio::time::sleep(wait).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn account_fixture(
        credential_ref: Option<&str>,
    ) -> crate::daemon::channel_store::ChannelAccountRecord {
        crate::daemon::channel_store::ChannelAccountRecord {
            account_id: "acct-1".to_string(),
            kind: ChannelKind::Discord,
            label: "Bot".to_string(),
            enabled: true,
            non_secret_config: serde_json::json!({}),
            credential_ref: credential_ref.map(str::to_string),
            access_policy: Default::default(),
            health: ChannelHealth::error(0, "unused"),
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    // -- construction ---------------------------------------------------------

    #[test]
    fn rejects_an_empty_token() {
        let account = account_fixture(None);
        let config = AdapterConfig {
            account: &account,
            secret: String::new(),
        };
        assert!(DiscordAdapter::new(&config).is_err());
    }

    #[test]
    fn construction_does_not_start_the_gateway_task() {
        // No #[tokio::test] runtime here on purpose: if `new` spawned
        // anything it would panic outside a runtime, so a plain #[test]
        // passing is itself proof construction has no side effects.
        let account = account_fixture(Some("discord/acct-1"));
        let config = AdapterConfig {
            account: &account,
            secret: "bot-token".to_string(),
        };
        let adapter = DiscordAdapter::new(&config).expect("adapter");
        assert_eq!(adapter.token, "bot-token");
    }

    // -- normalization ----------------------------------------------------

    fn message_create_fixture() -> Value {
        serde_json::json!({
            "id": "1001",
            "channel_id": "chan-1",
            "guild_id": "guild-1",
            "content": "hello @bot",
            "author": { "id": "author-1", "username": "alice", "bot": false },
            "mentions": [{ "id": "bot-id" }],
            "attachments": [{
                "id": "att-1",
                "url": "https://cdn.discordapp.com/a.png",
                "filename": "a.png",
                "content_type": "image/png",
                "size": 42,
            }],
        })
    }

    #[test]
    fn normalizes_guild_message_with_mention_and_attachment() {
        let empty = std::collections::BTreeMap::new();
        let envelope = normalize_message_create(
            "acct",
            &message_create_fixture(),
            Some("bot-id"),
            &empty,
            1000,
        )
        .expect("envelope");
        assert_eq!(envelope.provider_event_id, "1001");
        assert_eq!(
            envelope.conversation.kind,
            little_monkey_lib::channels::types::ConversationKind::Group
        );
        assert!(envelope.mentions_self);
        assert!(!envelope.sender.is_self);
        assert_eq!(envelope.attachments.len(), 1);
        assert_eq!(
            envelope.attachments[0].source,
            AttachmentSource::Url {
                url: "https://cdn.discordapp.com/a.png".to_string()
            }
        );
    }

    #[test]
    fn dm_message_has_no_guild_id_and_is_direct() {
        let mut fixture = message_create_fixture();
        fixture.as_object_mut().unwrap().remove("guild_id");
        let empty = std::collections::BTreeMap::new();
        let envelope = normalize_message_create("acct", &fixture, Some("bot-id"), &empty, 1000)
            .expect("envelope");
        assert_eq!(
            envelope.conversation.kind,
            little_monkey_lib::channels::types::ConversationKind::Direct
        );
    }

    #[test]
    fn thread_message_carries_parent_as_conversation() {
        let mut fixture = message_create_fixture();
        fixture["channel_id"] = Value::String("thread-9".to_string());
        let mut thread_parents = std::collections::BTreeMap::new();
        thread_parents.insert("thread-9".to_string(), "chan-1".to_string());
        let envelope =
            normalize_message_create("acct", &fixture, Some("bot-id"), &thread_parents, 1000)
                .expect("envelope");
        assert_eq!(envelope.conversation.conversation_id, "chan-1");
        assert_eq!(envelope.conversation.thread_id.as_deref(), Some("thread-9"));
    }

    #[test]
    fn self_authored_message_is_flagged_not_dropped() {
        let mut fixture = message_create_fixture();
        fixture["author"]["id"] = Value::String("bot-id".to_string());
        let empty = std::collections::BTreeMap::new();
        let envelope = normalize_message_create("acct", &fixture, Some("bot-id"), &empty, 1000)
            .expect("envelope");
        assert!(envelope.sender.is_self);
    }

    #[test]
    fn provider_event_id_is_deterministic() {
        let empty = std::collections::BTreeMap::new();
        let first = normalize_message_create("acct", &message_create_fixture(), None, &empty, 1000)
            .unwrap();
        let second =
            normalize_message_create("acct", &message_create_fixture(), None, &empty, 9999)
                .unwrap();
        assert_eq!(first.provider_event_id, second.provider_event_id);
    }

    // -- gateway state machine --------------------------------------------

    #[test]
    fn hello_without_prior_session_sends_identify() {
        let mut state = GatewayState::default();
        let actions = handle_frame(
            &mut state,
            "acct",
            r#"{"op":10,"d":{"heartbeat_interval":45000}}"#,
            "tok",
        );
        assert!(matches!(actions[0], Action::StartHeartbeat(45000)));
        match &actions[1] {
            Action::Identify(value) => assert_eq!(value["op"], 2),
            other => panic!("expected IDENTIFY, got {other:?}"),
        }
    }

    #[test]
    fn hello_after_resumable_close_sends_resume() {
        let mut state = GatewayState {
            session_id: Some("sess-1".to_string()),
            seq: Some(7),
            pending_resume: true,
            ..GatewayState::default()
        };
        let actions = handle_frame(
            &mut state,
            "acct",
            r#"{"op":10,"d":{"heartbeat_interval":45000}}"#,
            "tok",
        );
        match &actions[1] {
            Action::SendJson(value) => {
                assert_eq!(value["op"], 6);
                assert_eq!(value["d"]["session_id"], "sess-1");
                assert_eq!(value["d"]["seq"], 7);
            }
            other => panic!("expected RESUME, got {other:?}"),
        }
    }

    #[test]
    fn ready_captures_session_and_bot_id() {
        let mut state = GatewayState::default();
        let actions = handle_frame(
            &mut state,
            "acct",
            r#"{"op":0,"t":"READY","s":1,"d":{"session_id":"sess-2","user":{"id":"bot-9"}}}"#,
            "tok",
        );
        assert_eq!(state.session_id.as_deref(), Some("sess-2"));
        assert_eq!(state.bot_user_id.as_deref(), Some("bot-9"));
        assert_eq!(state.seq, Some(1));
        assert!(matches!(&actions[0], Action::SetBotUserId(id) if id == "bot-9"));
    }

    #[test]
    fn heartbeat_request_replies_with_current_sequence() {
        let mut state = GatewayState {
            seq: Some(42),
            ..GatewayState::default()
        };
        let actions = handle_frame(&mut state, "acct", r#"{"op":1}"#, "tok");
        match &actions[0] {
            Action::SendJson(value) => assert_eq!(value["d"], 42),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// A state that looks like a healthy live session, so a close code's
    /// effect on it is visible.
    fn live_session() -> GatewayState {
        GatewayState {
            seq: Some(77),
            session_id: Some("sess-live".into()),
            resume_gateway_url: Some("wss://resume.example".into()),
            pending_resume: false,
            ..GatewayState::default()
        }
    }

    #[test]
    fn every_unrecoverable_close_code_stops_the_gateway_with_a_reason() {
        // The provider's whole permanent column. Each must stop the loop
        // rather than spend another reconnect and another session start.
        for code in [4004u16, 4010, 4011, 4012, 4013, 4014] {
            let mut state = live_session();
            let actions = handle_close(&mut state, code);
            match &actions[0] {
                Action::PermanentError(reason) => assert!(
                    !reason.trim().is_empty(),
                    "close {code} stopped the gateway without saying why"
                ),
                other => panic!("close {code} must be permanent, got {other:?}"),
            }
            assert_eq!(
                actions.len(),
                1,
                "close {code} asked for something besides stopping"
            );
        }
    }

    #[test]
    fn a_discarded_session_is_forgotten_rather_than_resumed_again() {
        // 4007 (invalid seq) and 4009 (session timed out) both mean the
        // provider no longer has the session. Asking to resume it again earns
        // the same close, so the stored identity must go.
        for code in [4007u16, 4009] {
            let mut state = live_session();
            let actions = handle_close(&mut state, code);
            assert!(
                matches!(&actions[0], Action::Reconnect { .. }),
                "close {code} must reconnect, got {:?}",
                actions[0]
            );
            assert!(
                !state.pending_resume,
                "close {code} left the connection asking to RESUME"
            );
            assert_eq!(state.session_id, None, "close {code} kept the session id");
            assert_eq!(state.seq, None, "close {code} kept the sequence");
            assert_eq!(
                state.resume_gateway_url, None,
                "close {code} kept the resume URL"
            );
            // And the state that gets persisted says the same thing.
            assert!(
                !state.resume_snapshot().resumable(),
                "close {code} would still be resumed after a restart"
            );
        }
    }

    #[test]
    fn a_cleared_session_identifies_on_the_next_hello_instead_of_resuming() {
        let mut state = live_session();
        handle_close(&mut state, 4007);
        let actions = handle_frame(
            &mut state,
            "acct",
            r#"{"op":10,"d":{"heartbeat_interval":41250}}"#,
            "tok",
        );
        // The frame that actually goes out, not a flag: an IDENTIFY, and no
        // RESUME carrying the sequence the provider just rejected.
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, Action::Identify(_))),
            "{actions:?}"
        );
        assert!(
            !actions.iter().any(|action| matches!(
                action,
                Action::SendJson(value) if value["op"] == 6
            )),
            "a RESUME was sent for a session the provider discarded: {actions:?}"
        );
    }

    #[test]
    fn other_close_codes_reconnect() {
        let mut state = live_session();
        let actions = handle_close(&mut state, 1006);
        assert!(matches!(&actions[0], Action::Reconnect { .. }));
        assert!(state.pending_resume);
        // A resumable drop keeps everything a RESUME needs.
        assert!(state.resume_snapshot().resumable());
    }

    #[test]
    fn ready_captures_the_resume_url_and_establishes() {
        let mut state = GatewayState::default();
        let actions = handle_frame(
            &mut state,
            "acct",
            r#"{"op":0,"t":"READY","s":1,"d":{"session_id":"sess-2","resume_gateway_url":"wss://resume.example","user":{"id":"bot-9"}}}"#,
            "tok",
        );
        assert_eq!(
            state.resume_gateway_url.as_deref(),
            Some("wss://resume.example")
        );
        assert!(actions.contains(&Action::Established));
        let snapshot = state.resume_snapshot();
        assert_eq!(snapshot.session_id.as_deref(), Some("sess-2"));
        assert_eq!(snapshot.seq, Some(1));
        assert!(snapshot.resumable());
    }

    #[test]
    fn a_persisted_resume_state_round_trips_through_the_cursor() {
        let state = ResumeState {
            session_id: Some("sess-9".into()),
            resume_gateway_url: Some("wss://resume.example".into()),
            seq: Some(41),
            bot_user_id: Some("bot-9".into()),
            thread_parents: std::collections::BTreeMap::from([(
                "thread-9".to_string(),
                "chan-1".to_string(),
            )]),
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let parsed = ResumeState::parse(Some(&json));
        assert_eq!(parsed, state);
        // Garbage or absence degrades to a fresh IDENTIFY, never a panic.
        assert_eq!(ResumeState::parse(Some("not json")), ResumeState::default());
        assert_eq!(ResumeState::parse(None), ResumeState::default());
        assert!(!ResumeState::parse(None).resumable());
    }

    #[test]
    fn a_seeded_state_asks_to_resume_and_a_blank_one_identifies() {
        let seeded = GatewayState::from_resume(&ResumeState {
            session_id: Some("sess-9".into()),
            resume_gateway_url: Some("wss://resume.example".into()),
            seq: Some(41),
            bot_user_id: None,
            thread_parents: Default::default(),
        });
        assert!(seeded.pending_resume);
        // HELLO on a seeded state sends RESUME with the stored ids.
        let mut state = seeded;
        let actions = handle_frame(
            &mut state,
            "acct",
            r#"{"op":10,"d":{"heartbeat_interval":45000}}"#,
            "tok",
        );
        match &actions[1] {
            Action::SendJson(value) => {
                assert_eq!(value["op"], 6);
                assert_eq!(value["d"]["session_id"], "sess-9");
                assert_eq!(value["d"]["seq"], 41);
            }
            other => panic!("expected RESUME, got {other:?}"),
        }

        let blank = GatewayState::from_resume(&ResumeState::default());
        assert!(!blank.pending_resume);
    }

    #[test]
    fn a_resumed_session_still_recognizes_its_own_messages() {
        // A RESUME is answered with RESUMED, never a second READY, so the only
        // place the bot's own id can come from is what the last session
        // persisted. Without it every self-authored MESSAGE_CREATE — which
        // Discord does dispatch back to its author — looks like a stranger's,
        // and the reply to it starts a loop.
        let mut state = GatewayState::from_resume(&ResumeState {
            session_id: Some("sess-9".into()),
            resume_gateway_url: Some("wss://resume.example".into()),
            seq: Some(41),
            bot_user_id: Some("bot-9".into()),
            thread_parents: Default::default(),
        });
        // Already classified as a plain channel, so the frame normalizes
        // inline instead of asking the REST API first.
        state.known_non_threads.insert("chan-1".to_string());
        let actions = handle_frame(
            &mut state,
            "acct",
            r#"{"op":0,"t":"MESSAGE_CREATE","s":42,"d":{
                "id":"msg-1","channel_id":"chan-1","guild_id":"guild-1",
                "content":"our own reply",
                "author":{"id":"bot-9","username":"little-monkey","bot":true}
            }}"#,
            "tok",
        );
        match actions.first() {
            Some(Action::Envelope(envelope)) => assert!(
                envelope.sender.is_self,
                "a resumed session failed to recognize its own message"
            ),
            other => panic!("expected an envelope, got {other:?}"),
        }
    }

    #[test]
    fn ready_puts_the_bot_id_into_the_state_that_gets_persisted() {
        let mut state = GatewayState::default();
        handle_frame(
            &mut state,
            "acct",
            r#"{"op":0,"t":"READY","s":1,"d":{"session_id":"sess-2","user":{"id":"bot-9"}}}"#,
            "tok",
        );
        assert_eq!(
            state.resume_snapshot().bot_user_id.as_deref(),
            Some("bot-9")
        );
        // A dead session loses its sequence, never its identity.
        handle_frame(&mut state, "acct", r#"{"op":9,"d":false}"#, "tok");
        assert_eq!(state.seq, None);
        assert_eq!(state.bot_user_id.as_deref(), Some("bot-9"));
    }

    #[test]
    fn heartbeat_ack_is_surfaced_to_the_io_loop() {
        let mut state = GatewayState::default();
        let actions = handle_frame(&mut state, "acct", r#"{"op":11}"#, "tok");
        assert_eq!(actions, vec![Action::HeartbeatAck]);
    }

    #[test]
    fn invalid_session_asks_for_a_randomized_wait() {
        let mut state = GatewayState {
            session_id: Some("sess".into()),
            seq: Some(5),
            ..GatewayState::default()
        };
        let actions = handle_frame(&mut state, "acct", r#"{"op":9,"d":false}"#, "tok");
        match &actions[0] {
            Action::Reconnect { delay_ms } => {
                assert!((1_000..5_000).contains(delay_ms), "{delay_ms}")
            }
            other => panic!("unexpected {other:?}"),
        }
        // Non-resumable: the dead session must not be resumed or persisted.
        assert!(!state.pending_resume);
        assert_eq!(state.session_id, None);
        assert_eq!(state.seq, None);
    }

    #[test]
    fn the_guilds_intent_rides_along_for_thread_dispatches() {
        assert_eq!(DISCORD_INTENTS & 1, 1, "GUILDS intent missing");
    }

    #[test]
    fn a_reply_targets_the_thread_when_one_is_named() {
        let message = OutboundMessage {
            account_id: "acct".into(),
            kind: ChannelKind::Discord,
            conversation_id: "chan-1".into(),
            thread_id: Some("thread-9".into()),
            text: "hi".into(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            idempotency_key: "k".into(),
        };
        assert_eq!(target_channel(&message), "thread-9");
        let plain = OutboundMessage {
            thread_id: None,
            ..message
        };
        assert_eq!(target_channel(&plain), "chan-1");
    }

    // -- outbound mapping ---------------------------------------------------

    #[test]
    fn rate_limit_maps_to_retryable_with_ms() {
        let outcome = map_send_status(429, false, Some(2500)).unwrap();
        match outcome {
            SendOutcome::RetryableFailure { retry_after_ms, .. } => {
                assert_eq!(retry_after_ms, Some(2500))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rate_limit_headers_parse_fractional_seconds_into_milliseconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("Retry-After", "1.5".parse().unwrap());
        headers.insert("X-RateLimit-Bucket", "b1".parse().unwrap());
        headers.insert("X-RateLimit-Remaining", "0".parse().unwrap());
        headers.insert("X-RateLimit-Reset-After", "2.25".parse().unwrap());
        headers.insert("X-RateLimit-Global", "true".parse().unwrap());
        let info = parse_rate_limit_headers(&headers);
        assert_eq!(info.retry_after_ms, Some(1500));
        assert_eq!(info.bucket.as_deref(), Some("b1"));
        assert_eq!(info.remaining, Some(0));
        assert_eq!(info.reset_after_ms, Some(2250));
        assert!(info.is_global);
        // Absent or garbage headers degrade to nothing, never a panic.
        let mut garbage = reqwest::header::HeaderMap::new();
        garbage.insert("Retry-After", "soon".parse().unwrap());
        let info = parse_rate_limit_headers(&garbage);
        assert_eq!(info.retry_after_ms, None);
        assert!(!info.is_global);
        assert_eq!(
            parse_rate_limit_headers(&Default::default()).remaining,
            None
        );
    }

    #[test]
    fn auth_failure_is_permanent() {
        assert!(matches!(
            map_send_status(401, false, None),
            Some(SendOutcome::PermanentFailure { .. })
        ));
        assert!(matches!(
            map_send_status(403, false, None),
            Some(SendOutcome::PermanentFailure { .. })
        ));
    }

    #[test]
    fn server_error_after_partial_send_needs_reconciliation() {
        assert!(matches!(
            map_send_status(500, true, None),
            Some(SendOutcome::NeedsReconciliation { .. })
        ));
        assert!(matches!(
            map_send_status(500, false, None),
            Some(SendOutcome::RetryableFailure { .. })
        ));
    }

    #[test]
    fn success_status_continues() {
        assert!(map_send_status(204, false, None).is_none());
    }

    #[test]
    fn message_splitting_respects_the_limit() {
        let text = "a".repeat(4500);
        let chunks = split_message(&text, 2000);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].chars().count(), 2000);
        assert_eq!(chunks[2].chars().count(), 500);
        assert!(split_message("", 2000).is_empty());
    }

    // -- token hygiene -------------------------------------------------------

    #[test]
    fn scrub_removes_the_token_from_any_rendered_error() {
        let rendered = scrub(
            "request to https://x failed carrying secret-token-abc",
            "secret-token-abc",
        );
        assert!(!rendered.contains("secret-token-abc"));
    }

    #[test]
    fn identify_and_resume_payloads_carry_the_credential_only_where_intended() {
        let identify = identify_payload("tok-123", DISCORD_INTENTS);
        assert_eq!(identify["d"]["token"], "tok-123");
        let resume = resume_payload("tok-123", "sess", 3);
        assert_eq!(resume["op"], 6);
    }

    // -- HTTP fixture (probe) -------------------------------------------------

    async fn fixture_server(status: &str, body: String) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let address = listener.local_addr().expect("fixture address");
        let status = status.to_string();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept fixture request");
            let mut request = vec![0u8; 8 * 1024];
            let _ = stream.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        format!("http://{address}")
    }

    fn adapter_with_base(base: &str) -> DiscordAdapter {
        let account = account_fixture(Some("discord/acct-1"));
        let config = AdapterConfig {
            account: &account,
            secret: "bot-token".to_string(),
        };
        DiscordAdapter::new(&config)
            .expect("adapter")
            .with_base_url(base)
    }

    fn outbound_to(channel: &str) -> OutboundMessage {
        OutboundMessage {
            account_id: "acct".into(),
            kind: ChannelKind::Discord,
            conversation_id: channel.into(),
            thread_id: None,
            text: "hi".into(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            idempotency_key: "k".into(),
        }
    }

    /// Serve `responses` in order, one connection each, carrying the given
    /// extra headers — the shared test_http helper cannot speak rate-limit
    /// headers — and count how many requests actually reached the wire.
    fn serve_with_headers(
        responses: Vec<(u16, Vec<(&'static str, &'static str)>, String)>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        std::thread::spawn(move || {
            for (status, headers, body) in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                // Every request here is a JSON POST, so the body length is
                // always announced; read until it has arrived in full.
                let mut received = Vec::new();
                let mut scratch = [0u8; 4096];
                loop {
                    match stream.read(&mut scratch) {
                        Ok(0) | Err(_) => break,
                        Ok(count) => {
                            received.extend_from_slice(&scratch[..count]);
                            let text = String::from_utf8_lossy(&received).to_string();
                            if let Some(index) = text.find("\r\n\r\n") {
                                let content_length = text[..index]
                                    .lines()
                                    .find_map(|line| {
                                        line.to_ascii_lowercase()
                                            .strip_prefix("content-length:")
                                            .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                                    })
                                    .unwrap_or(0);
                                if received.len() >= index + 4 + content_length {
                                    break;
                                }
                            }
                        }
                    }
                }
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let extra: String = headers
                    .iter()
                    .map(|(name, value)| format!("{name}: {value}\r\n"))
                    .collect();
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        (format!("http://127.0.0.1:{port}"), hits)
    }

    // -- health semantics ------------------------------------------------

    #[tokio::test]
    async fn probe_before_the_gateway_starts_is_not_connected() {
        let base = fixture_server("200 OK", r#"{"username":"little-monkey"}"#.to_string()).await;
        let adapter = adapter_with_base(&base);
        // Never polled: the gateway task has not started. A valid REST token
        // alone must not read as a live connection.
        let health = adapter.probe().await;
        assert_eq!(
            health.state,
            little_monkey_lib::channels::types::HealthState::Disconnected,
            "{health:?}"
        );
    }

    #[tokio::test]
    async fn probe_while_the_gateway_reconnects_is_degraded() {
        let base = fixture_server("200 OK", r#"{"username":"little-monkey"}"#.to_string()).await;
        let adapter = adapter_with_base(&base);
        adapter
            .shared
            .gateway_status
            .store(GATEWAY_RECONNECTING, std::sync::atomic::Ordering::SeqCst);
        let health = adapter.probe().await;
        assert_eq!(
            health.state,
            little_monkey_lib::channels::types::HealthState::Degraded,
            "{health:?}"
        );
    }

    #[tokio::test]
    async fn probe_with_a_live_session_is_connected() {
        let base = fixture_server("200 OK", r#"{"username":"little-monkey"}"#.to_string()).await;
        let adapter = adapter_with_base(&base);
        adapter
            .shared
            .gateway_status
            .store(GATEWAY_CONNECTED, std::sync::atomic::Ordering::SeqCst);
        let health = adapter.probe().await;
        assert_eq!(
            health.state,
            little_monkey_lib::channels::types::HealthState::Connected,
            "{health:?}"
        );
    }

    // -- partial-send correctness ------------------------------------------

    #[test]
    fn any_failure_after_a_delivered_chunk_needs_reconciliation() {
        // A retry of the whole message would deliver chunk 1 twice; a
        // permanent failure would silently truncate it. Both park instead.
        for status in [429, 400, 401, 500, 502] {
            assert!(
                matches!(
                    map_send_status(status, true, None),
                    Some(SendOutcome::NeedsReconciliation { .. })
                ),
                "HTTP {status} after a delivered chunk must park the row"
            );
        }
    }

    #[tokio::test]
    async fn a_rate_limited_second_chunk_parks_the_message() {
        let (base, _requests) = crate::daemon::channel_adapter::test_http::serve(vec![
            (200, r#"{"id":"m1"}"#.to_string()),
            (429, r#"{"message":"rate limited"}"#.to_string()),
        ]);
        let adapter = adapter_with_base(&base);
        let message = OutboundMessage {
            account_id: "acct".into(),
            kind: ChannelKind::Discord,
            conversation_id: "chan-1".into(),
            thread_id: None,
            text: "a".repeat(2500),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            idempotency_key: "k".into(),
        };
        let outcome = adapter.send(&message).await;
        assert!(
            matches!(outcome, SendOutcome::NeedsReconciliation { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_rate_limit_before_anything_was_sent_is_a_plain_retry() {
        let (base, _requests) = crate::daemon::channel_adapter::test_http::serve(vec![(
            429,
            r#"{"message":"rate limited"}"#.to_string(),
        )]);
        let adapter = adapter_with_base(&base);
        let message = OutboundMessage {
            account_id: "acct".into(),
            kind: ChannelKind::Discord,
            conversation_id: "chan-1".into(),
            thread_id: None,
            text: "hi".into(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            idempotency_key: "k".into(),
        };
        let outcome = adapter.send(&message).await;
        assert!(
            matches!(outcome, SendOutcome::RetryableFailure { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_route_discord_said_was_spent_is_not_asked_again() {
        // No fixture server at all: the limiter must answer before any
        // request is built, and nothing has been sent so a retry is safe.
        let adapter = adapter_with_base("http://127.0.0.1:9");
        adapter.rate_limiter.lock().unwrap().on_response(
            "POST:chan-1",
            429,
            None,
            None,
            None,
            Some(30_000),
            false,
            std::time::Instant::now(),
        );
        match adapter.send(&outbound_to("chan-1")).await {
            SendOutcome::RetryableFailure { retry_after_ms, .. } => {
                let wait = retry_after_ms.expect("carries the remaining wait");
                assert!((1..=30_000).contains(&wait), "{wait}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    // -- rate limiting ---------------------------------------------------

    #[test]
    fn an_exhausted_bucket_gates_every_route_that_shares_it() {
        let mut limiter = RateLimiter::default();
        let now = std::time::Instant::now();
        // Both routes have been taught the same bucket; only one response
        // reported the bucket as spent.
        limiter.on_response(
            "POST:chan-2",
            200,
            Some("b1"),
            Some(5),
            Some(60_000),
            None,
            false,
            now,
        );
        limiter.on_response(
            "POST:chan-1",
            200,
            Some("b1"),
            Some(0),
            Some(5_000),
            None,
            false,
            now,
        );
        let wait = limiter
            .wait_ms("POST:chan-2", now)
            .expect("the sibling route shares the spent bucket");
        assert!((1..=5_000).contains(&wait), "{wait}");
    }

    #[test]
    fn a_global_rate_limit_blocks_an_unrelated_route() {
        let mut limiter = RateLimiter::default();
        let now = std::time::Instant::now();
        limiter.on_response("POST:chan-1", 429, None, None, None, Some(3_000), true, now);
        let wait = limiter
            .wait_ms("POST:elsewhere", now)
            .expect("a global 429 gates every route");
        assert!((1..=3_000).contains(&wait), "{wait}");
    }

    #[test]
    fn gates_reopen_once_the_reset_has_passed() {
        let mut limiter = RateLimiter::default();
        let now = std::time::Instant::now();
        limiter.on_response(
            "POST:chan-2",
            200,
            Some("b1"),
            Some(5),
            Some(60_000),
            None,
            false,
            now,
        );
        limiter.on_response(
            "POST:chan-1",
            200,
            Some("b1"),
            Some(0),
            Some(5_000),
            None,
            false,
            now,
        );
        limiter.on_response("POST:chan-3", 429, None, None, None, Some(3_000), true, now);
        let later = now + Duration::from_millis(5_001);
        assert_eq!(limiter.wait_ms("POST:chan-1", later), None);
        assert_eq!(limiter.wait_ms("POST:chan-2", later), None);
        assert_eq!(limiter.wait_ms("POST:chan-3", later), None);
    }

    #[test]
    fn a_gate_learned_before_the_bucket_id_survives_the_mapping() {
        let mut limiter = RateLimiter::default();
        let now = std::time::Instant::now();
        // A 429 with no bucket header gates the bare route key.
        limiter.on_response(
            "POST:chan-1",
            429,
            None,
            None,
            None,
            Some(10_000),
            false,
            now,
        );
        assert!(limiter.wait_ms("POST:chan-1", now).is_some());
        // The next response names the bucket; the recorded exhaustion must
        // move with the route, not vanish under the old key.
        limiter.on_response(
            "POST:chan-1",
            200,
            Some("b1"),
            Some(3),
            None,
            None,
            false,
            now,
        );
        let wait = limiter
            .wait_ms("POST:chan-1", now)
            .expect("the gate migrated to the bucket key");
        assert!((1..=10_000).contains(&wait), "{wait}");
    }

    #[test]
    fn the_limiter_maps_never_outgrow_their_cap() {
        let mut limiter = RateLimiter::default();
        let now = std::time::Instant::now();
        for index in 0..(RATE_LIMIT_MAP_CAP * 2) {
            let bucket = format!("b{index}");
            limiter.on_response(
                &format!("POST:chan-{index}"),
                200,
                Some(bucket.as_str()),
                Some(0),
                Some(600_000),
                None,
                false,
                now,
            );
        }
        assert!(limiter.route_buckets.len() <= RATE_LIMIT_MAP_CAP);
        assert!(limiter.gates.len() <= RATE_LIMIT_MAP_CAP);
    }

    #[tokio::test]
    async fn an_exhausted_shared_bucket_refuses_the_sibling_route_before_the_wire() {
        let (base, hits) = serve_with_headers(vec![
            (
                200,
                vec![
                    ("X-RateLimit-Bucket", "b1"),
                    ("X-RateLimit-Remaining", "5"),
                    ("X-RateLimit-Reset-After", "60"),
                ],
                r#"{"id":"m1"}"#.to_string(),
            ),
            (
                200,
                vec![
                    ("X-RateLimit-Bucket", "b1"),
                    ("X-RateLimit-Remaining", "0"),
                    ("X-RateLimit-Reset-After", "5"),
                ],
                r#"{"id":"m2"}"#.to_string(),
            ),
        ]);
        let adapter = adapter_with_base(&base);
        // The first response teaches the limiter that chan-2 draws from b1;
        // the second spends b1 through chan-1.
        assert!(matches!(
            adapter.send(&outbound_to("chan-2")).await,
            SendOutcome::Sent { .. }
        ));
        assert!(matches!(
            adapter.send(&outbound_to("chan-1")).await,
            SendOutcome::Sent { .. }
        ));
        match adapter.send(&outbound_to("chan-2")).await {
            SendOutcome::RetryableFailure { retry_after_ms, .. } => {
                let wait = retry_after_ms.expect("carries the bucket's remaining wait");
                assert!((1..=5_000).contains(&wait), "{wait}");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the gated send must never reach the wire"
        );
    }

    #[tokio::test]
    async fn a_global_429_blocks_a_send_to_another_channel_without_a_request() {
        let (base, hits) = serve_with_headers(vec![(
            429,
            vec![("Retry-After", "3"), ("X-RateLimit-Global", "true")],
            r#"{"message":"you are being rate limited","retry_after":3.0,"global":true}"#
                .to_string(),
        )]);
        let adapter = adapter_with_base(&base);
        match adapter.send(&outbound_to("chan-1")).await {
            SendOutcome::RetryableFailure { retry_after_ms, .. } => {
                assert_eq!(retry_after_ms, Some(3_000));
            }
            other => panic!("unexpected {other:?}"),
        }
        match adapter.send(&outbound_to("chan-2")).await {
            SendOutcome::RetryableFailure { retry_after_ms, .. } => {
                let wait = retry_after_ms.expect("carries the global wait");
                assert!((1..=3_000).contains(&wait), "{wait}");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the globally gated send must never reach the wire"
        );
    }

    // -- session-start budget ------------------------------------------------

    #[test]
    fn the_session_start_limit_is_parsed_from_gateway_bot() {
        let now = std::time::Instant::now();
        let body: Value = serde_json::json!({
            "url": "wss://gateway.example",
            "session_start_limit": {
                "total": 1000, "remaining": 3, "reset_after": 14_400_000, "max_concurrency": 1
            }
        });
        let budget = SessionStartBudget::parse(&body, now).expect("parsed");
        assert_eq!(budget.total, 1000);
        assert_eq!(budget.remaining, 3);
        assert_eq!(budget.max_concurrency, 1);
        assert_eq!(budget.wait_before_identify(now), Duration::ZERO);
        assert!(SessionStartBudget::parse(&serde_json::json!({"url":"wss://x"}), now).is_none());
    }

    #[test]
    fn an_exhausted_budget_waits_out_the_reset_before_identifying() {
        let now = std::time::Instant::now();
        let body: Value = serde_json::json!({
            "session_start_limit": {
                "total": 1000, "remaining": 0, "reset_after": 60_000, "max_concurrency": 1
            }
        });
        let budget = SessionStartBudget::parse(&body, now).expect("parsed");
        let wait = budget.wait_before_identify(now);
        assert!(
            wait > Duration::from_secs(59) && wait <= Duration::from_secs(60),
            "{wait:?}"
        );
        // Once Discord's own reset time has passed the wait collapses to
        // zero — the next /gateway/bot fetch re-reads the real numbers.
        assert_eq!(
            budget.wait_before_identify(now + Duration::from_secs(61)),
            Duration::ZERO
        );
    }

    #[test]
    fn each_identify_spends_the_local_budget_mirror() {
        let now = std::time::Instant::now();
        let mut budget = SessionStartBudget {
            total: 1000,
            remaining: 2,
            reset_at: now + Duration::from_secs(3600),
            max_concurrency: 1,
        };
        budget.note_identify();
        assert_eq!(budget.wait_before_identify(now), Duration::ZERO);
        budget.note_identify();
        // Spent: a reconnect storm between two /gateway/bot fetches cannot
        // identify past what Discord granted.
        assert!(budget.wait_before_identify(now) > Duration::ZERO);
        budget.note_identify();
        assert_eq!(budget.remaining, 0, "never underflows");
    }

    // -- thread topology -----------------------------------------------------

    #[test]
    fn thread_topology_rides_the_persisted_cursor_across_a_restart() {
        // The daemon that saw THREAD_CREATE is gone; the new process is
        // seeded only with what the cursor stored. A message in that thread —
        // with no fresh THREAD_CREATE — must still resolve its parent.
        let stored = ResumeState {
            session_id: Some("sess-9".into()),
            resume_gateway_url: Some("wss://resume.example".into()),
            seq: Some(41),
            bot_user_id: Some("bot-9".into()),
            thread_parents: std::collections::BTreeMap::from([(
                "thread-9".to_string(),
                "chan-1".to_string(),
            )]),
        };
        let json = serde_json::to_string(&stored).expect("serialize");
        let mut state = GatewayState::from_resume(&ResumeState::parse(Some(&json)));
        let actions = handle_frame(
            &mut state,
            "acct",
            r#"{"op":0,"t":"MESSAGE_CREATE","s":42,"d":{
                "id":"msg-1","channel_id":"thread-9","guild_id":"guild-1",
                "content":"still threaded",
                "author":{"id":"user-1","username":"ada","bot":false}
            }}"#,
            "tok",
        );
        match actions.first() {
            Some(Action::Envelope(envelope)) => {
                assert_eq!(envelope.conversation.conversation_id, "chan-1");
                assert_eq!(envelope.conversation.thread_id.as_deref(), Some("thread-9"));
            }
            other => panic!("expected an envelope, got {other:?}"),
        }
        // And the snapshot that will be persisted next still carries the map.
        assert_eq!(
            state.resume_snapshot().thread_parents.get("thread-9"),
            Some(&"chan-1".to_string())
        );
    }

    #[test]
    fn a_deleted_thread_leaves_the_map() {
        let mut state = GatewayState::default();
        handle_frame(
            &mut state,
            "acct",
            r#"{"op":0,"t":"THREAD_CREATE","s":1,"d":{"id":"thread-9","parent_id":"chan-1"}}"#,
            "tok",
        );
        assert_eq!(state.thread_parents.len(), 1);
        handle_frame(
            &mut state,
            "acct",
            r#"{"op":0,"t":"THREAD_DELETE","s":2,"d":{"id":"thread-9"}}"#,
            "tok",
        );
        assert!(state.thread_parents.is_empty());
    }

    #[test]
    fn the_persisted_thread_map_evicts_its_oldest_entry_at_the_bound() {
        let mut map = std::collections::BTreeMap::new();
        // Snowflakes grow over time; "100" is older than "99" is false — the
        // comparison is numeric via (length, string).
        for index in 0..MAX_PERSISTED_THREADS {
            insert_thread_parent(&mut map, format!("{}", 1000 + index), "parent".to_string());
        }
        assert_eq!(map.len(), MAX_PERSISTED_THREADS);
        insert_thread_parent(&mut map, "99999".to_string(), "parent".to_string());
        assert_eq!(map.len(), MAX_PERSISTED_THREADS);
        assert!(!map.contains_key("1000"), "oldest snowflake evicted");
        assert!(map.contains_key("99999"));
        // The bounded map must serialize comfortably under the cursor
        // column's 4096-byte cap even with realistic 19-digit snowflakes.
        let mut full = std::collections::BTreeMap::new();
        for index in 0..MAX_PERSISTED_THREADS {
            insert_thread_parent(
                &mut full,
                format!("11223344556677{index:05}"),
                "99887766554433221100".to_string(),
            );
        }
        let state = ResumeState {
            session_id: Some("s".repeat(40)),
            resume_gateway_url: Some(format!("wss://{}.example", "g".repeat(60))),
            seq: Some(u64::MAX),
            bot_user_id: Some("1".repeat(20)),
            thread_parents: full,
        };
        let json = serde_json::to_string(&state).expect("serialize");
        assert!(json.len() <= 4096, "cursor value too large: {}", json.len());
    }

    #[test]
    fn an_unknown_guild_channel_is_resolved_before_it_is_normalized() {
        let mut state = GatewayState::default();
        let frame = r#"{"op":0,"t":"MESSAGE_CREATE","s":1,"d":{
            "id":"msg-1","channel_id":"mystery-7","guild_id":"guild-1",
            "content":"hi","author":{"id":"user-1","username":"ada","bot":false}
        }}"#;
        match handle_frame(&mut state, "acct", frame, "tok").first() {
            Some(Action::ResolveChannelThenNormalize { channel_id, .. }) => {
                assert_eq!(channel_id, "mystery-7");
            }
            other => panic!("expected a resolution request, got {other:?}"),
        }
        // Once classified as a plain channel, the same frame normalizes
        // inline — one lookup per channel, not one per message.
        state.known_non_threads.insert("mystery-7".to_string());
        match handle_frame(&mut state, "acct", frame, "tok").first() {
            Some(Action::Envelope(envelope)) => {
                assert_eq!(envelope.conversation.conversation_id, "mystery-7");
                assert_eq!(envelope.conversation.thread_id, None);
            }
            other => panic!("expected an envelope, got {other:?}"),
        }
    }

    #[test]
    fn a_dm_channel_is_never_sent_for_thread_resolution() {
        let mut state = GatewayState::default();
        let frame = r#"{"op":0,"t":"MESSAGE_CREATE","s":1,"d":{
            "id":"msg-1","channel_id":"dm-7",
            "content":"hi","author":{"id":"user-1","username":"ada","bot":false}
        }}"#;
        match handle_frame(&mut state, "acct", frame, "tok").first() {
            Some(Action::Envelope(envelope)) => {
                assert_eq!(
                    envelope.conversation.kind,
                    little_monkey_lib::channels::types::ConversationKind::Direct
                );
            }
            other => panic!("expected an envelope, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_thread_channel_lookup_reads_the_parent_from_the_rest_api() {
        let base = fixture_server(
            "200 OK",
            r#"{"id":"thread-9","type":11,"parent_id":"chan-1"}"#.to_string(),
        )
        .await;
        let client = little_monkey_lib::egress::hardened().build().unwrap();
        let parent = resolve_thread_parent(&client, &base, "tok", "thread-9")
            .await
            .expect("resolved");
        assert_eq!(parent.as_deref(), Some("chan-1"));
    }

    #[tokio::test]
    async fn a_plain_channel_lookup_answers_not_a_thread() {
        let base = fixture_server("200 OK", r#"{"id":"chan-1","type":0}"#.to_string()).await;
        let client = little_monkey_lib::egress::hardened().build().unwrap();
        let parent = resolve_thread_parent(&client, &base, "tok", "chan-1")
            .await
            .expect("resolved");
        assert_eq!(parent, None);
    }

    #[tokio::test]
    async fn a_failed_channel_lookup_is_an_error_not_a_cached_answer() {
        let base = fixture_server("404 Not Found", r#"{"message":"gone"}"#.to_string()).await;
        let client = little_monkey_lib::egress::hardened().build().unwrap();
        assert!(resolve_thread_parent(&client, &base, "tok", "gone-1")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn probe_reads_username_from_a_fixture_server() {
        let base = fixture_server("200 OK", r#"{"username":"little-monkey"}"#.to_string()).await;
        let client = little_monkey_lib::egress::hardened().build().unwrap();
        let request = client
            .get(format!("{base}/users/@me"))
            .header("Authorization", "Bot tok");
        let response = little_monkey_lib::egress::send(request).await.unwrap();
        assert!(response.status().is_success());
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["username"], "little-monkey");
    }
}
