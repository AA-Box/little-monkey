//! IRC adapter: TLS socket, SASL PLAIN, CAP negotiation, reconnect.
//!
//! Unlike Telegram's long poll, IRC is a persistent connection: this module
//! owns a background task (spawned once per adapter, on first use) that holds
//! the socket, drives registration, answers `PING`, and forwards `PRIVMSG`
//! into a bounded channel that [`IrcAdapter::poll`] drains. `poll`'s `cursor`
//! is unused — see its doc for why there is nothing for it to resume from.
//!
//! # The nick we asked for is not always the nick we have
//!
//! A nick is first-come-first-served per network, so the configured one may
//! already be taken. The server answers `433 ERR_NICKNAMEINUSE` and waits — it
//! does not pick one for us, and re-sending the same `NICK` forever is how a
//! connection hangs before `001` ever arrives. [`fallback_nick`] walks a
//! deterministic, bounded ladder instead (`monkey`, `monkey_`, `monkey_2`, …),
//! honouring the server's own `NICKLEN` once `005` has told us what it is, and
//! gives up with a real health error rather than looping.
//!
//! Everything downstream then has to use the nick we actually got:
//! [`Shared::active_nick`] is that nick, set from `001`'s own first parameter
//! — the server's answer, not our guess — and it is what mention detection and
//! self-filtering read. It is runtime state and never written back into the
//! account's configuration: the preferred nick is what the operator asked for,
//! and a reconnect asks for it again.
//!
//! SASL is deliberately *not* tied to it. The account being authenticated is
//! `sasl_username` (defaulting to the preferred nick), because an account name
//! and a display nick are different things on every network that has both —
//! authenticating as a collision-generated `monkey_2` would simply fail.
//!
//! # Event ids have no provider equivalent
//!
//! IRC assigns no id to a message. `provider_event_id` is therefore
//! synthesized as a SHA-256 digest of `(server, target, sender, raw line,
//! connection message counter)` — deterministic *for one connection's stream
//! of lines*, and never a random UUID, because dedupe in `channel_ingress`
//! keys on this value: a random id would defeat dedupe on every redelivery
//! rather than enable it. The message counter is what keeps two literally
//! identical lines (the same user pasting "lol" twice) from colliding — a
//! digest over the line's own bytes alone cannot tell them apart.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex as AsyncMutex, OnceCell};
use tokio_rustls::TlsConnector;

use crate::daemon::channel_adapter::{
    AdapterConfig, ChannelAdapter, InboundBatch, TransportStatus,
};
use little_monkey_lib::channels::policy::EchoCorrelation;
use little_monkey_lib::channels::types::{
    BoundedMetadata, ChannelConversation, ChannelEnvelope, ChannelHealth, ChannelKind,
    ChannelSender, HealthState, InboundTransport, OutboundMessage, ProviderCapabilities,
    SendOutcome,
};

/// IRC's own per-line limit (RFC 1459 section 2.3 / RFC 2812 section 2.3):
/// 512 bytes total, including the trailing CRLF. There is no analogous
/// provider constant for the *usable text* portion — it depends on the
/// target name — so [`split_privmsg_chunks`] computes the real budget per
/// call instead of guessing one number.
const MAX_LINE_BYTES: usize = 512;

/// How many inbound `PRIVMSG` envelopes to buffer between one `poll` call and
/// the next. Generous relative to any plausible IRC traffic rate; a consumer
/// that falls this far behind is not going to be helped by a bigger number,
/// only by polling faster.
const INBOUND_CAPACITY: usize = 1024;

/// How long [`IrcAdapter::poll`] will wait for at least one message before
/// returning an empty batch. Bounded, per the trait's contract for a
/// long-lived transport, rather than blocking forever.
const POLL_WAIT: Duration = Duration::from_secs(20);

/// How many nicks to try before giving up on registering at all.
///
/// Bounded on purpose: a network where the first five are all taken is a
/// network an operator has to look at, and walking the ladder forever would
/// hammer the server while reporting nothing.
const MAX_NICK_ATTEMPTS: usize = 5;

/// How long a SASL exchange gets from the first line read to `903`.
///
/// A server that acknowledges the capability and then goes quiet would
/// otherwise hold a connection open that can never authenticate and can never
/// register, which reads as "connecting" forever.
#[cfg(not(test))]
const SASL_TIMEOUT: Duration = Duration::from_secs(30);
/// The same deadline, shortened so the timeout arm is provable in CI. Every
/// test exchange runs over an in-memory pipe and answers in microseconds, so
/// this is only ever reached by a server that deliberately says nothing.
#[cfg(test)]
const SASL_TIMEOUT: Duration = Duration::from_millis(150);

const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(60);
/// A connection that stayed registered at least this long is treated as
/// having recovered, so a later drop starts backing off from the beginning
/// again instead of inheriting the previous outage's backoff.
const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(30);

pub struct IrcAdapter {
    server: String,
    port: u16,
    nick: String,
    /// The SASL account name, which is not necessarily the nick — see the
    /// module doc. Defaults to the preferred nick so an account configured
    /// before this existed keeps authenticating exactly as it did.
    sasl_username: String,
    channels: Vec<String>,
    use_sasl: bool,
    password: String,
    shared: Arc<Shared>,
}

/// State reachable from every `&self` trait method, none of which can take
/// `&mut self`. Interior mutability is therefore load-bearing here, not a
/// style choice.
struct Shared {
    /// Set only when `001 RPL_WELCOME` is seen — see [`IrcAdapter::probe`]'s
    /// doc for why configuration or a live socket must not be enough.
    registered: AtomicBool,
    /// The same fact as `registered`, in the shape the daemon's health loop
    /// reads: connecting before the first registration, degraded once a
    /// connection has dropped.
    status: TransportStatus,
    last_error: AsyncMutex<Option<String>>,
    /// The socket's write half, type-erased.
    ///
    /// Boxed rather than `WriteHalf<TlsStream<TcpStream>>` so the registration
    /// state machine below is exercised over an in-memory pipe in tests — the
    /// production code path, without needing a TLS server and a trusted
    /// certificate to prove that `433` is handled.
    write_half: AsyncMutex<Option<Box<dyn AsyncWrite + Send + Unpin>>>,
    inbound_tx: mpsc::Sender<ChannelEnvelope>,
    inbound_rx: AsyncMutex<mpsc::Receiver<ChannelEnvelope>>,
    message_counter: AtomicU64,
    /// The nick the server actually gave us, which is what mention detection
    /// and self-filtering must use. Runtime state only: the account's own
    /// configured nick is never rewritten, so a reconnect asks for it again.
    active_nick: AsyncMutex<String>,
    /// `NICKLEN` from the server's `005 ISUPPORT`, or zero while unknown. Only
    /// applied when known — guessing a limit would truncate a nick the server
    /// would have accepted.
    nick_len: AtomicUsize,
    /// Spawns the connection task exactly once, on whichever trait method
    /// (`probe`, `poll`, or `send`) is called first — not in `new`, so
    /// constructing an adapter never requires an async runtime to be current.
    started: OnceCell<()>,
}

impl IrcAdapter {
    pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
        let raw = &config.account.non_secret_config;
        let server = raw
            .get("server")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .ok_or("IRC requires a server hostname")?
            .to_string();
        let port = raw
            .get("port")
            .and_then(|value| value.as_u64())
            .filter(|value| *value > 0 && *value <= u64::from(u16::MAX))
            .unwrap_or(6697) as u16;
        let nick = raw
            .get("nick")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .ok_or("IRC requires a nick")?
            .to_string();
        let channels = raw
            .get("channels")
            .and_then(|value| value.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let use_sasl = raw
            .get("use_sasl")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if use_sasl && config.secret.trim().is_empty() {
            return Err("IRC SASL is enabled but no password is configured".to_string());
        }
        let sasl_username = raw
            .get("sasl_username")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(&nick)
            .to_string();

        let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_CAPACITY);
        Ok(Self {
            server,
            port,
            channels,
            nick: nick.clone(),
            sasl_username,
            use_sasl,
            password: config.secret.clone(),
            shared: Arc::new(Shared {
                active_nick: AsyncMutex::new(nick.clone()),
                nick_len: AtomicUsize::new(0),
                registered: AtomicBool::new(false),
                status: TransportStatus::default(),
                last_error: AsyncMutex::new(None),
                write_half: AsyncMutex::new(None),
                inbound_tx,
                inbound_rx: AsyncMutex::new(inbound_rx),
                message_counter: AtomicU64::new(0),
                started: OnceCell::new(),
            }),
        })
    }

    /// Spawns the background connection task on first call and does nothing
    /// on every call after — `OnceCell::get_or_init` makes concurrent callers
    /// (a `probe` racing a `poll`, say) share one spawn rather than one each.
    async fn ensure_started(&self) {
        let identity = Identity {
            preferred_nick: self.nick.clone(),
            sasl_username: self.sasl_username.clone(),
            use_sasl: self.use_sasl,
            password: self.password.clone(),
        };
        let shared = self.shared.clone();
        let server = self.server.clone();
        let port = self.port;
        let channels = self.channels.clone();
        self.shared
            .started
            .get_or_init(|| async move {
                tokio::spawn(connection_loop(shared, server, port, identity, channels));
            })
            .await;
    }
}

#[async_trait]
impl ChannelAdapter for IrcAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Irc
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            kind: ChannelKind::Irc,
            inbound_transport: InboundTransport::Socket,
            // A conservative advertised cap: the true per-message budget
            // depends on the target name (see `split_privmsg_chunks`, which
            // always computes the exact one), so this is headroom for a UI
            // or outbox sizing a draft, not the number `send` actually
            // enforces.
            max_text_chars: 400,
            supports_threads: false,
            supports_attachments: false,
            // IRC carries no structured mention data — `mentions_self` here
            // is our own word-boundary scan over the text, not provider
            // metadata, so callers must not treat this as authoritative the
            // way Telegram's entities are.
            supports_mention_metadata: false,
            supports_idempotency_key: false,
            supports_delivery_receipts: false,
            echo_correlation: EchoCorrelation::HostAdapter,
        }
    }

    /// Registration state, which is the only thing that means "connected"
    /// on IRC — an open TCP socket is not one.
    fn live_transport(&self) -> Option<HealthState> {
        Some(self.shared.status.get())
    }

    async fn probe(&self) -> ChannelHealth {
        self.ensure_started().await;
        let now = now_ms();
        if self.shared.registered.load(Ordering::SeqCst) {
            // The nick the server gave us, not the one we asked for: after a
            // collision those differ, and reporting the configured one would
            // tell an operator their bot answers to a name it does not have.
            let active = self.shared.active_nick.lock().await.clone();
            let detail = if active == self.nick {
                format!("{active} on {}", self.server)
            } else {
                format!(
                    "{active} on {} (the configured nick '{}' was taken)",
                    self.server, self.nick
                )
            };
            return ChannelHealth::connected(now, Some(detail));
        }
        // Connected-but-not-registered and never-connected look the same
        // from here on purpose: configuration existing, or a TCP socket
        // being open, is not a connection — only `001 RPL_WELCOME` is.
        let detail = self
            .shared
            .last_error
            .lock()
            .await
            .clone()
            .unwrap_or_else(|| "Not yet registered with the IRC server".to_string());
        ChannelHealth::error(now, detail)
    }

    async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
        // IRC has no addressable history for a cursor to resume from: a
        // reconnect rejoins live, it does not replay what was missed while
        // disconnected. `cursor` is therefore always ignored and this always
        // returns `None`, which `InboundBatch::cursor`'s own doc says leaves
        // the stored cursor alone — the correct answer for "no resume
        // concept" rather than persisting a value nothing will ever read.
        self.ensure_started().await;
        let mut envelopes = Vec::new();
        let mut receiver = self.shared.inbound_rx.lock().await;
        match tokio::time::timeout(POLL_WAIT, receiver.recv()).await {
            Ok(Some(envelope)) => envelopes.push(envelope),
            Ok(None) => return Err("IRC inbound channel closed".to_string()),
            Err(_) => {} // Nothing arrived within the bounded wait.
        }
        // Drain whatever else is already queued without waiting further.
        while let Ok(envelope) = receiver.try_recv() {
            envelopes.push(envelope);
        }
        Ok(InboundBatch {
            envelopes,
            cursor: None,
        })
    }

    async fn send(&self, message: &OutboundMessage) -> SendOutcome {
        self.ensure_started().await;
        if !self.shared.registered.load(Ordering::SeqCst) {
            // Never touched the socket for this message at all.
            return SendOutcome::RetryableFailure {
                error: "Not connected to the IRC server yet".to_string(),
                retry_after_ms: Some(2_000),
            };
        }
        let target = &message.conversation_id;
        let chunks = split_privmsg_chunks(target, &message.text);
        for (index, chunk) in chunks.iter().enumerate() {
            let line = format!("PRIVMSG {target} :{chunk}");
            match write_line(&self.shared, &line).await {
                Ok(()) => {}
                // The socket went away between the registration check above and
                // this write, and no part of this message has gone out yet:
                // nothing was sent, so a retry cannot duplicate anything.
                Err(WriteError::NotConnected) if index == 0 => {
                    return SendOutcome::RetryableFailure {
                        error: "Lost the IRC connection before the message was written".to_string(),
                        retry_after_ms: Some(2_000),
                    }
                }
                // IRC gives no delivery acknowledgement. A write that failed
                // part-way, or one that follows a chunk already on the wire,
                // may already have been delivered — "unknown" is the only safe
                // answer, never "retry".
                Err(error) => {
                    return SendOutcome::NeedsReconciliation {
                        error: String::from(error),
                    }
                }
            }
        }
        SendOutcome::Sent {
            provider_message_id: None,
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// The process-wide TLS client config, built once. `rustls::ClientConfig`'s
/// own root store is immutable after construction, so there is nothing
/// per-connection to configure here — every IRC adapter in this process
/// shares one. An IRC account cannot name extra trust anchors the way an email
/// account can; when one needs to, `email.rs`'s `trust_roots` belongs in
/// `channel_adapter.rs` rather than copied here.
fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            // `ClientConfig::builder()` *panics* when it cannot pick a crypto
            // provider, and this build compiles rustls with both `ring` and
            // `aws-lc-rs`, so it never can. `daemon/remote/server.rs` installs
            // one for the same reason, but a daemon with an IRC account and no
            // remote host never reaches that code — this connect would abort
            // the process. Idempotent, and any installed provider will do.
            let _ = rustls::crypto::ring::default_provider().install_default();
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            Arc::new(
                rustls::ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            )
        })
        .clone()
}

/// Why one line did not go out.
///
/// The distinction is the outbound-certainty one: with no socket at all
/// nothing was written and a retry is provably safe, while a write that
/// failed part-way may already have put bytes on the wire — and IRC
/// acknowledges nothing, so those bytes can never be un-sent.
#[derive(Debug)]
enum WriteError {
    NotConnected,
    Failed(String),
}

impl From<WriteError> for String {
    fn from(error: WriteError) -> String {
        match error {
            WriteError::NotConnected => "not connected to the IRC server".to_string(),
            WriteError::Failed(message) => message,
        }
    }
}

/// Writes one line plus `\r\n` to the current socket, or fails if there is
/// none. Shared by the connection task (registration, `PONG`) and
/// [`IrcAdapter::send`], so both go through the same `Mutex` and neither can
/// interleave a half-written line with the other.
async fn write_line(shared: &Shared, line: &str) -> Result<(), WriteError> {
    let mut guard = shared.write_half.lock().await;
    let Some(write_half) = guard.as_mut() else {
        return Err(WriteError::NotConnected);
    };
    write_half
        .write_all(line.as_bytes())
        .await
        .map_err(|error| WriteError::Failed(error.to_string()))?;
    write_half
        .write_all(b"\r\n")
        .await
        .map_err(|error| WriteError::Failed(error.to_string()))?;
    write_half
        .flush()
        .await
        .map_err(|error| WriteError::Failed(error.to_string()))
}

/// Connects, registers, and reads forever, reconnecting with backoff whenever
/// the socket drops. Never returns while the adapter is alive — it is the
/// body of the task [`IrcAdapter::ensure_started`] spawns once.
async fn connection_loop(
    shared: Arc<Shared>,
    server: String,
    port: u16,
    identity: Identity,
    channels: Vec<String>,
) {
    let mut backoff = INITIAL_RECONNECT_BACKOFF;
    loop {
        begin_attempt(&shared, &identity).await;
        let connected_at = std::time::Instant::now();
        let result = connect_and_register(&shared, &server, port, &identity, &channels).await;
        shared.registered.store(false, Ordering::SeqCst);
        shared.status.set(HealthState::Degraded);
        *shared.write_half.lock().await = None;
        if let Err(error) = result {
            *shared.last_error.lock().await = Some(error);
        }
        if connected_at.elapsed() >= BACKOFF_RESET_AFTER {
            backoff = INITIAL_RECONNECT_BACKOFF;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
    }
}

/// Reset what belongs to one connection before the next one starts.
///
/// Every attempt asks for the nick the operator configured. Whoever held it may
/// well have left by now, and the collision-generated one was never their
/// choice — carrying it across a reconnect would make a temporary clash
/// permanent.
async fn begin_attempt(shared: &Arc<Shared>, identity: &Identity) {
    *shared.active_nick.lock().await = identity.preferred_nick.clone();
}

/// One connection attempt: TCP connect, TLS handshake, CAP/SASL/registration,
/// then the read loop for as long as the socket stays open. Returns (always
/// with an `Err`, describing why) once the connection ends, so the caller can
/// log it and reconnect.
async fn connect_and_register(
    shared: &Arc<Shared>,
    server: &str,
    port: u16,
    identity: &Identity,
    channels: &[String],
) -> Result<(), String> {
    let tcp = TcpStream::connect((server, port))
        .await
        .map_err(|error| format!("TCP connect to {server}:{port} failed: {error}"))?;
    let connector = TlsConnector::from(tls_config());
    let server_name = rustls::pki_types::ServerName::try_from(server.to_string())
        .map_err(|error| format!("invalid IRC server name {server}: {error}"))?;
    let tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|error| format!("TLS handshake with {server} failed: {error}"))?;
    let (read_half, write_half) = tokio::io::split(tls);
    *shared.write_half.lock().await = Some(Box::new(write_half));

    register_and_read(shared, server, identity, channels, read_half).await
}

/// The registration state machine and the read loop, over whatever the socket
/// turned out to be.
///
/// Generic over the reader so a test can drive the whole of it — CAP, SASL,
/// `433` collisions, `001`, `JOIN`, `PRIVMSG` — across an in-memory pipe.
async fn register_and_read<R: AsyncRead + Unpin>(
    shared: &Arc<Shared>,
    server: &str,
    identity: &Identity,
    channels: &[String],
    read_half: R,
) -> Result<(), String> {
    let Identity {
        preferred_nick,
        sasl_username,
        use_sasl,
        password,
    } = identity;
    let use_sasl = *use_sasl;

    write_line(shared, "CAP LS 302").await?;
    if use_sasl {
        write_line(shared, "CAP REQ :sasl").await?;
    }
    // `USER` names the account behind the connection and is never re-sent, so
    // it stays the preferred nick even when `NICK` has to change.
    let mut attempted_nick = preferred_nick.clone();
    let mut nick_attempts = 1usize;
    write_line(shared, &format!("NICK {attempted_nick}")).await?;
    write_line(
        shared,
        &format!("USER {preferred_nick} 0 * :{preferred_nick}"),
    )
    .await?;

    let mut state = RegistrationState::NegotiatingCapabilities;
    // Only meaningful when `use_sasl`; the whole point is that `001` may not be
    // believed until this is true.
    let mut sasl_succeeded = false;
    // A server that acknowledges `sasl` and then says nothing must not hold the
    // connection open forever pretending to authenticate.
    let sasl_deadline = tokio::time::Instant::now() + SASL_TIMEOUT;

    let mut lines = BufReader::new(read_half).lines();
    loop {
        let next = lines.next_line();
        let read = if use_sasl && !sasl_succeeded {
            match tokio::time::timeout_at(sasl_deadline, next).await {
                Ok(read) => read,
                Err(_) => return Err(sasl_failure(sasl_username, "it did not answer in time")),
            }
        } else {
            next.await
        };
        let raw_line = match read.map_err(|error| format!("read failed: {error}"))? {
            Some(line) => line,
            None if use_sasl && !sasl_succeeded => {
                return Err(sasl_failure(
                    sasl_username,
                    "the server closed the connection during authentication",
                ))
            }
            None => return Err("connection closed by server".to_string()),
        };
        let Some(parsed) = parse_line(&raw_line) else {
            continue;
        };
        match parsed.command {
            "PING" => {
                let token = parsed.params.first().copied().unwrap_or("");
                write_line(shared, &format!("PONG :{token}")).await?;
            }
            // The `CAP LS` listing. Without SASL nothing was ever requested, so
            // negotiation is over the moment the listing is complete — and it
            // has to be ended explicitly, because a server that saw `CAP LS`
            // holds registration until `CAP END` and would otherwise never send
            // `001`. A `*` in the third parameter marks a continuation line.
            "CAP"
                if !use_sasl
                    && parsed.params.get(1).copied() == Some("LS")
                    && parsed.params.get(2).copied() != Some("*") =>
            {
                write_line(shared, "CAP END").await?;
                state = RegistrationState::Registering;
            }
            "CAP" if use_sasl && parsed.params.get(1).copied() == Some("ACK") => {
                state = RegistrationState::Authenticating;
                write_line(shared, "AUTHENTICATE PLAIN").await?;
            }
            "CAP" if parsed.params.get(1).copied() == Some("NAK") => {
                if use_sasl {
                    // The operator asked for SASL and this server will not do
                    // it. Continuing would register anonymously — an
                    // unauthenticated connection wearing a healthy badge — so
                    // the attempt fails instead.
                    return Err(sasl_failure(
                        sasl_username,
                        "the server refused the SASL capability",
                    ));
                }
                write_line(shared, "CAP END").await?;
                state = RegistrationState::Registering;
            }
            "AUTHENTICATE"
                if state == RegistrationState::Authenticating
                    && parsed.params.first().copied() == Some("+") =>
            {
                // `\0<authzid>\0<authcid>\0<password>` with an empty
                // authorization identity, per SASL PLAIN (RFC 4616). The
                // authentication identity is the configured *account*, never
                // `attempted_nick`: a collision changes the display nick, and
                // authenticating as `monkey_2` would simply be rejected.
                let payload = format!("\0{sasl_username}\0{password}");
                let encoded = BASE64.encode(payload.as_bytes());
                write_line(shared, &format!("AUTHENTICATE {encoded}")).await?;
            }
            // RPL_LOGGEDIN / RPL_SASLSUCCESS. Only believed while an
            // authentication this connection started is actually in flight.
            "900" | "903" if state == RegistrationState::Authenticating => {
                sasl_succeeded = true;
                state = RegistrationState::Registering;
                write_line(shared, "CAP END").await?;
            }
            // ERR_SASLFAIL / ERR_SASLTOOLONG / ERR_SASLABORTED /
            // ERR_SASLALREADY. With SASL explicitly enabled every one of these
            // ends the attempt: the alternative is an anonymous registration
            // the operator never asked for.
            "904" | "905" | "906" | "907" => {
                if use_sasl {
                    return Err(sasl_failure(
                        sasl_username,
                        "the server rejected the credentials",
                    ));
                }
                write_line(shared, "CAP END").await?;
            }
            "433" => {
                // ERR_NICKNAMEINUSE. The server will not choose for us and
                // will not proceed, so re-sending the same NICK is how this
                // connection hangs before `001` ever arrives.
                if nick_attempts >= MAX_NICK_ATTEMPTS {
                    return Err(format!(
                        "the IRC server refused {nick_attempts} nicknames starting from \
                         '{preferred_nick}'; every one was already in use"
                    ));
                }
                attempted_nick =
                    fallback_nick(preferred_nick, nick_attempts, known_nick_len(shared));
                nick_attempts += 1;
                write_line(shared, &format!("NICK {attempted_nick}")).await?;
            }
            "005" => {
                // RPL_ISUPPORT. `NICKLEN=<n>` is the only token read here, and
                // only so a later fallback stays inside a limit the server
                // itself named rather than one we guessed.
                if let Some(limit) = parse_nick_len(&parsed.params) {
                    shared.nick_len.store(limit, Ordering::SeqCst);
                }
            }
            "NICK" => {
                // A server-forced rename (services collides us, or a network
                // policy renames us). The new nick is the trailing parameter.
                let previous = parsed.prefix.and_then(|prefix| prefix.split('!').next());
                let current = shared.active_nick.lock().await.clone();
                if previous == Some(current.as_str()) {
                    if let Some(renamed) = parsed.params.first() {
                        *shared.active_nick.lock().await = (*renamed).to_string();
                    }
                }
            }
            "001" => {
                // RPL_WELCOME: the server says registration is complete. When
                // SASL was asked for, that is not enough on its own — a server
                // will happily register an unauthenticated connection, and
                // treating this as success is exactly the silent downgrade this
                // state machine exists to prevent.
                if use_sasl && !sasl_succeeded {
                    return Err(sasl_failure(
                        sasl_username,
                        "the server completed registration without authenticating",
                    ));
                }
                // This is the one and only place `registered` becomes true —
                // see `probe`'s doc for why nothing earlier may set it.
                //
                // The nick recorded is `001`'s own first parameter, which is
                // the server's answer to what we asked for. Trusting our own
                // last `NICK` instead would be wrong exactly when it matters:
                // a server that truncated or rewrote it says so only here.
                let assigned = parsed
                    .params
                    .first()
                    .copied()
                    .filter(|value| !value.is_empty())
                    .unwrap_or(attempted_nick.as_str());
                *shared.active_nick.lock().await = assigned.to_string();
                state = RegistrationState::Registered;
                shared.registered.store(true, Ordering::SeqCst);
                shared.status.set(HealthState::Connected);
                *shared.last_error.lock().await = None;
                for channel in channels {
                    write_line(shared, &format!("JOIN {channel}")).await?;
                }
            }
            "PRIVMSG" if parsed.params.len() >= 2 => {
                if let Some(prefix) = parsed.prefix {
                    let target = parsed.params[0];
                    let text = parsed.params[1];
                    let counter = shared.message_counter.fetch_add(1, Ordering::SeqCst);
                    // The nick we actually have, not the one we asked for: a
                    // collision would otherwise make every mention of us
                    // invisible and every echo of our own line look like
                    // somebody else's.
                    let own_nick = shared.active_nick.lock().await.clone();
                    let envelope = normalize_privmsg(
                        server, prefix, target, text, &raw_line, counter, &own_nick,
                    );
                    // `try_send`, deliberately: a full inbound channel must
                    // never block the reader, or this task stops answering
                    // `PING` and the server disconnects it. Dropping the
                    // newest message under sustained overflow is the
                    // ponytail-acceptable cost — see `INBOUND_CAPACITY`.
                    let _ = shared.inbound_tx.try_send(envelope);
                }
            }
            _ => {}
        }
    }
}

/// How far one connection attempt has got.
///
/// Tracked explicitly rather than inferred from what has been written, because
/// the question "may `001` be believed?" has no correct answer without it: a
/// server registers an unauthenticated connection just as happily as an
/// authenticated one, and the difference is only visible in this state plus
/// whether SASL actually succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistrationState {
    /// Capabilities requested, nothing settled yet.
    NegotiatingCapabilities,
    /// SASL is in flight. Only reachable when the account enabled it.
    Authenticating,
    /// Negotiation is over (`CAP END` sent); waiting for `001`.
    Registering,
    /// `001` arrived and, if SASL was required, it succeeded first.
    Registered,
}

/// The health sentence for a SASL failure.
///
/// Names the account so an operator knows *which* credential to fix, and never
/// the password — this string is written into the account's health, which is
/// shown in the UI and returned by the CLI.
fn sasl_failure(sasl_username: &str, reason: &str) -> String {
    format!("IRC SASL authentication failed for account {sasl_username}: {reason}")
}

/// Who this connection is, in the two senses IRC keeps separate: the nick it
/// would like to be called, and the account it authenticates as.
struct Identity {
    preferred_nick: String,
    sasl_username: String,
    use_sasl: bool,
    password: String,
}

/// The server's own `NICKLEN`, or `None` while it has not said.
fn known_nick_len(shared: &Arc<Shared>) -> Option<usize> {
    match shared.nick_len.load(Ordering::SeqCst) {
        0 => None,
        limit => Some(limit),
    }
}

/// Reads `NICKLEN=<n>` out of one `005 RPL_ISUPPORT` line's parameters.
///
/// Every other token is ignored: this is not an ISUPPORT parser, it is one
/// lookup, and a limit nobody stated is better left unknown than guessed.
fn parse_nick_len(params: &[&str]) -> Option<usize> {
    params
        .iter()
        .find_map(|token| token.strip_prefix("NICKLEN=")?.parse::<usize>().ok())
        .filter(|limit| *limit > 0)
}

/// The nick to try after `attempt` collisions, as a deterministic ladder:
/// `monkey`, `monkey_`, `monkey_2`, `monkey_3`, …
///
/// Deterministic and never random, so the same server refusing the same nick
/// twice produces the same second choice — an operator who sees `monkey_2` in
/// health once sees it again rather than a new name every reconnect.
///
/// `nick_len` truncates the *base*, never the suffix: the suffix is the only
/// part that makes this attempt different from the last one, and trimming it
/// would ask for a nick already known to be taken.
fn fallback_nick(preferred: &str, attempt: usize, nick_len: Option<usize>) -> String {
    let suffix = if attempt <= 1 {
        "_".to_string()
    } else {
        format!("_{attempt}")
    };
    let Some(limit) = nick_len else {
        return format!("{preferred}{suffix}");
    };
    let suffix_len = suffix.chars().count();
    if preferred.chars().count() + suffix_len <= limit {
        return format!("{preferred}{suffix}");
    }
    let base: String = preferred
        .chars()
        .take(limit.saturating_sub(suffix_len))
        .collect();
    format!("{base}{suffix}")
}

struct ParsedLine<'a> {
    prefix: Option<&'a str>,
    command: &'a str,
    params: Vec<&'a str>,
}

/// Parses one IRC protocol line (already stripped of its trailing newline by
/// `AsyncBufReadExt::lines`) into `[:prefix] COMMAND [params...] [:trailing]`
/// per RFC 1459/2812 section 2.3.1. Returns `None` for a blank line, which a
/// keep-alive or a stray CRLF can produce.
fn parse_line(line: &str) -> Option<ParsedLine<'_>> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() {
        return None;
    }
    let mut rest = line;
    let mut prefix = None;
    if let Some(stripped) = rest.strip_prefix(':') {
        let (found_prefix, remainder) = stripped.split_once(' ')?;
        prefix = Some(found_prefix);
        rest = remainder;
    }
    let (command, mut rest) = match rest.split_once(' ') {
        Some((command, remainder)) => (command, remainder),
        None => (rest, ""),
    };
    let mut params = Vec::new();
    loop {
        if let Some(trailing) = rest.strip_prefix(':') {
            params.push(trailing);
            break;
        }
        match rest.split_once(' ') {
            Some((param, remainder)) => {
                params.push(param);
                rest = remainder;
            }
            None => {
                if !rest.is_empty() {
                    params.push(rest);
                }
                break;
            }
        }
    }
    Some(ParsedLine {
        prefix,
        command,
        params,
    })
}

fn normalize_privmsg(
    server: &str,
    prefix: &str,
    target: &str,
    text: &str,
    raw_line: &str,
    counter: u64,
    own_nick: &str,
) -> ChannelEnvelope {
    let sender_nick = prefix.split('!').next().unwrap_or(prefix);
    let is_group = matches!(target.chars().next(), Some('#' | '&' | '+' | '!'));
    let conversation = if is_group {
        ChannelConversation::group(target.to_string())
    } else {
        // No provider conversation id exists for a DM; the sender's own nick
        // is the only stable key IRC offers for "this direct conversation".
        ChannelConversation::direct(sender_nick.to_string())
    };
    let mut metadata = BoundedMetadata::new();
    metadata.insert("target", target);
    metadata.insert("kind", if is_group { "channel" } else { "direct" });

    ChannelEnvelope {
        account_id: String::new(),
        kind: ChannelKind::Irc,
        provider_event_id: deterministic_event_id(server, target, sender_nick, raw_line, counter),
        provider_message_id: None,
        conversation,
        sender: ChannelSender {
            sender_id: sender_nick.to_string(),
            display_label: Some(sender_nick.to_string()),
            // Compared against the nick we actually hold: with `echo-message`
            // negotiated, or a bouncer replaying what we said, our own line
            // comes back down the socket and must not read as somebody
            // else's.
            is_self: sender_nick.eq_ignore_ascii_case(own_nick),
            is_bot: false,
        },
        text: text.to_string(),
        attachments: Vec::new(),
        reply_to_provider_id: None,
        mentions_self: mentions_nick(text, own_nick),
        received_at_ms: now_ms(),
        metadata,
    }
}

/// A deterministic, never-random dedupe key. See this module's own doc for
/// why: `channel_ingress` dedupes on `provider_event_id`, and IRC assigns
/// none, so one has to be synthesized here from everything that makes one
/// line on one connection unique — including the counter, since two users
/// sending the identical text on the same connection must not collide.
fn deterministic_event_id(
    server: &str,
    target: &str,
    sender: &str,
    raw_line: &str,
    counter: u64,
) -> String {
    let mut hasher = Sha256::new();
    for part in [server, target, sender, raw_line] {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(counter.to_be_bytes());
    format!("{:x}", hasher.finalize())
}

/// IRC nick-name characters (RFC 2812 section 2.3.1), used to decide whether
/// an occurrence of the nick in text is a whole word or a substring of a
/// longer token (`bob` inside `bobby` must not count).
fn is_nick_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'_' | b'[' | b']' | b'\\' | b'`' | b'^' | b'{' | b'}' | b'|'
        )
}

/// True when `nick` appears in `text` at a word boundary, case-insensitively.
/// Byte-indexed rather than char-indexed: every character in
/// [`is_nick_char`]'s set is ASCII, so a non-ASCII byte (any continuation
/// byte of a multi-byte UTF-8 character included) always reads as "not a
/// nick character" and therefore always counts as a boundary — which is the
/// conservative direction to be wrong in for a gate that only ever adds a
/// mention, never removes text.
fn mentions_nick(text: &str, nick: &str) -> bool {
    if nick.is_empty() {
        return false;
    }
    let haystack = text.to_ascii_lowercase();
    let needle = nick.to_ascii_lowercase();
    let bytes = haystack.as_bytes();
    let mut start = 0usize;
    while let Some(offset) = haystack[start..].find(&needle) {
        let match_start = start + offset;
        let match_end = match_start + needle.len();
        let before_ok = match_start == 0 || !is_nick_char(bytes[match_start - 1]);
        let after_ok = match_end >= bytes.len() || !is_nick_char(bytes[match_end]);
        if before_ok && after_ok {
            return true;
        }
        start = match_start + 1;
        if start >= bytes.len() {
            break;
        }
    }
    false
}

/// Splits `text` into `PRIVMSG` payload chunks that keep the *whole* line —
/// `PRIVMSG <target> :<chunk>\r\n` — at or under [`MAX_LINE_BYTES`]. The
/// budget is computed from the real `target`, not guessed, because a long
/// channel name eats directly into how much text fits; never splits inside a
/// UTF-8 character.
fn split_privmsg_chunks(target: &str, text: &str) -> Vec<String> {
    let overhead = "PRIVMSG ".len() + target.len() + " :".len() + "\r\n".len();
    let budget = MAX_LINE_BYTES.saturating_sub(overhead).max(1);
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;
    for ch in text.chars() {
        let ch_len = ch.len_utf8();
        if current_len + ch_len > budget && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_len = 0;
        }
        current.push(ch);
        current_len += ch_len;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_privmsg_to_a_channel() {
        let parsed = parse_line(":alice!a@host PRIVMSG #general :hello world").unwrap();
        assert_eq!(parsed.prefix, Some("alice!a@host"));
        assert_eq!(parsed.command, "PRIVMSG");
        assert_eq!(parsed.params, vec!["#general", "hello world"]);
    }

    #[test]
    fn parses_a_privmsg_with_no_trailing_colon_needed_for_the_target() {
        let parsed = parse_line(":bob!b@host PRIVMSG alice :hi there").unwrap();
        assert_eq!(parsed.params, vec!["alice", "hi there"]);
    }

    #[test]
    fn parses_ping_with_no_prefix() {
        let parsed = parse_line("PING :irc.example.org").unwrap();
        assert_eq!(parsed.prefix, None);
        assert_eq!(parsed.command, "PING");
        assert_eq!(parsed.params, vec!["irc.example.org"]);
    }

    #[test]
    fn blank_line_parses_to_none() {
        assert!(parse_line("").is_none());
        assert!(parse_line("\r\n").is_none());
    }

    #[test]
    fn normalizes_a_channel_message_as_a_group_conversation() {
        let envelope = normalize_privmsg(
            "irc.example.org",
            "alice!a@host",
            "#general",
            "hello",
            ":alice!a@host PRIVMSG #general :hello",
            0,
            "little_monkey",
        );
        assert_eq!(envelope.conversation.conversation_id, "#general");
        assert_eq!(envelope.sender.sender_id, "alice");
    }

    #[test]
    fn normalizes_a_direct_message_keyed_on_the_sender_nick() {
        let envelope = normalize_privmsg(
            "irc.example.org",
            "alice!a@host",
            "little_monkey",
            "hi",
            ":alice!a@host PRIVMSG little_monkey :hi",
            0,
            "little_monkey",
        );
        assert_eq!(envelope.conversation.conversation_id, "alice");
    }

    #[test]
    fn mentions_self_at_a_word_boundary_only() {
        assert!(mentions_nick("hey little_monkey, ping", "little_monkey"));
        assert!(mentions_nick("LITTLE_MONKEY!!", "little_monkey"));
        assert!(!mentions_nick("little_monkeys are cute", "little_monkey"));
        assert!(!mentions_nick("not_little_monkey either", "little_monkey"));
    }

    #[test]
    fn deterministic_event_ids_repeat_for_identical_inputs() {
        let first = deterministic_event_id("irc.example.org", "#general", "alice", "line", 3);
        let second = deterministic_event_id("irc.example.org", "#general", "alice", "line", 3);
        assert_eq!(first, second);
    }

    #[test]
    fn deterministic_event_ids_differ_when_the_counter_differs() {
        let first = deterministic_event_id("irc.example.org", "#general", "alice", "line", 3);
        let second = deterministic_event_id("irc.example.org", "#general", "alice", "line", 4);
        assert_ne!(first, second);
    }

    #[test]
    fn deterministic_event_ids_differ_across_distinct_lines_from_the_same_sender() {
        let first = deterministic_event_id("irc.example.org", "#general", "alice", "hello", 0);
        let second = deterministic_event_id("irc.example.org", "#general", "alice", "world", 1);
        assert_ne!(first, second);
    }

    #[test]
    fn send_splitting_respects_the_512_byte_line_budget() {
        let long = "a".repeat(2000);
        let chunks = split_privmsg_chunks("#general", &long);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            let line_len = "PRIVMSG ".len() + "#general".len() + " :".len() + chunk.len() + 2;
            assert!(line_len <= MAX_LINE_BYTES);
        }
        assert_eq!(chunks.concat(), long);
    }

    #[test]
    fn send_splitting_accounts_for_a_long_target_name() {
        let target = format!("#{}", "x".repeat(100));
        let text = "y".repeat(500);
        let chunks = split_privmsg_chunks(&target, &text);
        for chunk in &chunks {
            let line_len = "PRIVMSG ".len() + target.len() + " :".len() + chunk.len() + 2;
            assert!(line_len <= MAX_LINE_BYTES);
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn short_text_is_a_single_send_chunk() {
        assert_eq!(split_privmsg_chunks("#general", "hi"), vec!["hi"]);
    }

    #[test]
    fn sasl_plain_payload_is_nul_separated_nick_and_password() {
        let nick = "little_monkey";
        let password = "hunter2";
        let payload = format!("\0{nick}\0{password}");
        let encoded = BASE64.encode(payload.as_bytes());
        let decoded = BASE64.decode(encoded).unwrap();
        assert_eq!(decoded, format!("\0{nick}\0{password}").into_bytes());
    }

    #[test]
    fn config_requires_a_server_and_nick() {
        let account = super::super::super::channel_store::ChannelAccountRecord {
            account_id: "acct-1".to_string(),
            kind: ChannelKind::Irc,
            label: "IRC".to_string(),
            enabled: true,
            non_secret_config: serde_json::json!({}),
            credential_ref: None,
            access_policy: Default::default(),
            health: ChannelHealth::error(0, "unused"),
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let config = AdapterConfig {
            account: &account,
            secret: String::new(),
        };
        assert!(IrcAdapter::new(&config).is_err());
    }

    #[test]
    fn config_requires_a_password_when_sasl_is_enabled() {
        let account = super::super::super::channel_store::ChannelAccountRecord {
            account_id: "acct-1".to_string(),
            kind: ChannelKind::Irc,
            label: "IRC".to_string(),
            enabled: true,
            non_secret_config: serde_json::json!({
                "server": "irc.example.org",
                "nick": "little_monkey",
                "use_sasl": true,
            }),
            credential_ref: Some("irc/acct-1".to_string()),
            access_policy: Default::default(),
            health: ChannelHealth::error(0, "unused"),
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let config = AdapterConfig {
            account: &account,
            secret: String::new(),
        };
        assert!(IrcAdapter::new(&config).is_err());
    }

    #[test]
    fn config_parses_a_full_account() {
        let account = super::super::super::channel_store::ChannelAccountRecord {
            account_id: "acct-1".to_string(),
            kind: ChannelKind::Irc,
            label: "IRC".to_string(),
            enabled: true,
            non_secret_config: serde_json::json!({
                "server": "irc.example.org",
                "port": 6697,
                "nick": "little_monkey",
                "channels": ["#general", "#ops"],
                "use_sasl": true,
            }),
            credential_ref: Some("irc/acct-1".to_string()),
            access_policy: Default::default(),
            health: ChannelHealth::error(0, "unused"),
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let config = AdapterConfig {
            account: &account,
            secret: "hunter2".to_string(),
        };
        let adapter = IrcAdapter::new(&config).expect("adapter");
        assert_eq!(adapter.server, "irc.example.org");
        assert_eq!(adapter.port, 6697);
        assert_eq!(adapter.nick, "little_monkey");
        assert_eq!(adapter.channels, vec!["#general", "#ops"]);
        assert!(adapter.use_sasl);
        // Not configured, so the account name is the nick — which is how every
        // IRC account that existed before `sasl_username` did keeps working.
        assert_eq!(adapter.sasl_username, "little_monkey");
    }

    #[test]
    fn a_sasl_username_is_kept_separate_from_the_nick() {
        let account = super::super::super::channel_store::ChannelAccountRecord {
            account_id: "acct-1".to_string(),
            kind: ChannelKind::Irc,
            label: "IRC".to_string(),
            enabled: true,
            non_secret_config: serde_json::json!({
                "server": "irc.example.org",
                "nick": "little_monkey",
                "use_sasl": true,
                "sasl_username": "monkey-account",
            }),
            credential_ref: Some("irc/acct-1".to_string()),
            access_policy: Default::default(),
            health: ChannelHealth::error(0, "unused"),
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let adapter = IrcAdapter::new(&AdapterConfig {
            account: &account,
            secret: "hunter2".to_string(),
        })
        .expect("adapter");
        assert_eq!(adapter.nick, "little_monkey");
        assert_eq!(adapter.sasl_username, "monkey-account");
    }

    #[test]
    fn the_fallback_ladder_is_deterministic() {
        assert_eq!(fallback_nick("monkey", 1, None), "monkey_");
        assert_eq!(fallback_nick("monkey", 2, None), "monkey_2");
        assert_eq!(fallback_nick("monkey", 3, None), "monkey_3");
        // The same collision twice gives the same second choice, so an
        // operator who saw `monkey_2` in health sees it again.
        assert_eq!(
            fallback_nick("monkey", 2, None),
            fallback_nick("monkey", 2, None)
        );
    }

    #[test]
    fn a_known_nick_length_trims_the_base_and_never_the_suffix() {
        // The suffix is the only thing that makes this attempt different from
        // the last, so trimming it would re-ask for a nick already refused.
        assert_eq!(fallback_nick("littlemonkey", 2, Some(9)), "littlem_2");
        assert_eq!(fallback_nick("littlemonkey", 1, Some(9)), "littlemo_");
        // Comfortably inside the limit: nothing is trimmed.
        assert_eq!(fallback_nick("mk", 2, Some(30)), "mk_2");
    }

    #[test]
    fn an_unknown_nick_length_truncates_nothing() {
        // Guessing a limit would refuse a nick the server would have taken.
        let long = "a".repeat(40);
        assert_eq!(fallback_nick(&long, 1, None), format!("{long}_"));
    }

    #[test]
    fn nick_len_is_read_out_of_isupport_and_nothing_else_is() {
        assert_eq!(
            parse_nick_len(&["monkey", "CHANTYPES=#", "NICKLEN=30", "AWAYLEN=200"]),
            Some(30)
        );
        assert_eq!(parse_nick_len(&["monkey", "CHANTYPES=#"]), None);
        assert_eq!(parse_nick_len(&["monkey", "NICKLEN=oops"]), None);
        assert_eq!(parse_nick_len(&["monkey", "NICKLEN=0"]), None);
    }

    /// An opt-in round trip against an IRC network the *operator* names.
    ///
    /// Never runs unless the server, nick and target are all set, so CI and
    /// every contributor's `cargo test` skip it. No network, no nickname and no
    /// channel is bundled anywhere in this tree: the destination is one the
    /// person running it chose, and it is the only place a message is sent.
    /// SASL is exercised only when a username and password are given too.
    ///
    /// ```text
    /// LM_IRC_LIVE_SERVER=irc.libera.chat \
    /// LM_IRC_LIVE_NICK=littlemonkey \
    /// LM_IRC_LIVE_TARGET='#your-own-channel' \
    /// LM_IRC_LIVE_SASL_USERNAME=littlemonkey \
    /// LM_IRC_LIVE_SASL_PASSWORD=… \
    ///   cargo test --bin monkey-cli a_live_irc_round_trip -- --nocapture
    /// ```
    #[tokio::test]
    async fn a_live_irc_round_trip() {
        let (Ok(server), Ok(nick), Ok(target)) = (
            std::env::var("LM_IRC_LIVE_SERVER"),
            std::env::var("LM_IRC_LIVE_NICK"),
            std::env::var("LM_IRC_LIVE_TARGET"),
        ) else {
            return;
        };
        let sasl_username = std::env::var("LM_IRC_LIVE_SASL_USERNAME").ok();
        let password = std::env::var("LM_IRC_LIVE_SASL_PASSWORD").unwrap_or_default();
        let mut config = serde_json::json!({
            "server": server,
            "port": std::env::var("LM_IRC_LIVE_PORT")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(6697),
            "nick": nick,
            "channels": [target.clone()],
            "use_sasl": !password.is_empty(),
        });
        if let Some(username) = sasl_username {
            config["sasl_username"] = serde_json::Value::String(username);
        }
        let account = super::super::super::channel_store::ChannelAccountRecord {
            account_id: "acct-1".to_string(),
            kind: ChannelKind::Irc,
            label: "IRC".to_string(),
            enabled: true,
            non_secret_config: config,
            credential_ref: Some("irc/acct-1".to_string()),
            access_policy: Default::default(),
            health: ChannelHealth::error(0, "unused"),
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let adapter = IrcAdapter::new(&AdapterConfig {
            account: &account,
            secret: password,
        })
        .expect("adapter");

        adapter.ensure_started().await;
        for _ in 0..120 {
            if adapter.shared.registered.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let health = adapter.probe().await;
        assert_eq!(
            health.state,
            HealthState::Connected,
            "{:?}{:?}",
            health.detail,
            health.last_error
        );

        let marker = uuid::Uuid::new_v4().simple().to_string();
        let outcome = adapter
            .send(&OutboundMessage {
                account_id: "acct-1".to_string(),
                kind: ChannelKind::Irc,
                conversation_id: target,
                thread_id: None,
                text: format!("little-monkey live smoke test {marker}"),
                attachments: Vec::new(),
                reply_to_provider_id: None,
                idempotency_key: format!("live-{marker}"),
            })
            .await;
        assert!(matches!(outcome, SendOutcome::Sent { .. }), "{outcome:?}");
    }

    /// Everything below drives the *production* registration state machine over
    /// an in-memory pipe: the same `register_and_read` a TLS socket reaches,
    /// with a scripted server on the other end. No TLS, no certificate, no
    /// network — which is what makes `433` handling provable in CI at all.
    mod against_a_test_server {
        use super::*;

        fn identity(use_sasl: bool) -> Identity {
            Identity {
                preferred_nick: "monkey".to_string(),
                sasl_username: "monkey-account".to_string(),
                use_sasl,
                password: "hunter2".to_string(),
            }
        }

        fn shared() -> Arc<Shared> {
            let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_CAPACITY);
            Arc::new(Shared {
                active_nick: AsyncMutex::new("monkey".to_string()),
                nick_len: AtomicUsize::new(0),
                registered: AtomicBool::new(false),
                status: TransportStatus::default(),
                last_error: AsyncMutex::new(None),
                write_half: AsyncMutex::new(None),
                inbound_tx,
                inbound_rx: AsyncMutex::new(inbound_rx),
                message_counter: AtomicU64::new(0),
                started: OnceCell::new(),
            })
        }

        /// Run one connection against a server that answers each line the
        /// adapter writes. Returning `None` closes the socket, which is how a
        /// session ends.
        ///
        /// Hands back the registration result and every line the adapter sent,
        /// in order — the two things a protocol assertion needs.
        async fn session(
            shared: &Arc<Shared>,
            identity: &Identity,
            channels: &[String],
            respond: impl Fn(&str) -> Option<Vec<String>> + Send + 'static,
        ) -> (Result<(), String>, Vec<String>) {
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, client_write) = tokio::io::split(client);
            *shared.write_half.lock().await = Some(Box::new(client_write));

            let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
            let recorder = sent.clone();
            let server_task = tokio::spawn(async move {
                let (server_read, mut server_write) = tokio::io::split(server);
                let mut lines = BufReader::new(server_read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    recorder.lock().expect("recorder").push(line.clone());
                    let Some(replies) = respond(&line) else {
                        return;
                    };
                    for reply in replies {
                        if server_write
                            .write_all(format!("{reply}\r\n").as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            });

            // Bounded, because a session that registered successfully is
            // *supposed* to keep reading forever — that is what a live IRC
            // connection does. Stopping the watch is not a failure; a real
            // failure resolves long before this.
            let result = match tokio::time::timeout(
                Duration::from_secs(1),
                register_and_read(shared, "irc.test", identity, channels, client_read),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Ok(()),
            };
            // Dropped before the server is awaited: while this end of the pipe
            // is open the server's own read never sees EOF.
            *shared.write_half.lock().await = None;
            server_task.abort();
            let lines = sent.lock().expect("recorder").clone();
            (result, lines)
        }

        /// A server that welcomes whatever nick it is finally asked for, after
        /// refusing the first `collisions` of them.
        fn welcome_after(collisions: usize) -> impl Fn(&str) -> Option<Vec<String>> + Send {
            let refused = std::sync::Mutex::new(0usize);
            move |line: &str| {
                if let Some(nick) = line.strip_prefix("NICK ") {
                    let mut refused = refused.lock().expect("counter");
                    if *refused < collisions {
                        *refused += 1;
                        return Some(vec![format!(
                            ":irc.test 433 * {nick} :Nickname is already in use"
                        )]);
                    }
                    return Some(vec![
                        format!(":irc.test 001 {nick} :Welcome to the test network"),
                        ":irc.test 005 CHANTYPES=# NICKLEN=30 :are supported".to_string(),
                    ]);
                }
                Some(Vec::new())
            }
        }

        #[tokio::test]
        async fn a_taken_nick_walks_the_ladder_rather_than_hanging() {
            let shared = shared();
            let (_result, sent) = session(&shared, &identity(false), &[], welcome_after(2)).await;

            let nicks: Vec<&String> = sent
                .iter()
                .filter(|line| line.starts_with("NICK "))
                .collect();
            assert_eq!(
                nicks,
                vec!["NICK monkey", "NICK monkey_", "NICK monkey_2"],
                "the ladder must move on rather than re-send the same nick"
            );
            assert!(shared.registered.load(Ordering::SeqCst));
            assert_eq!(*shared.active_nick.lock().await, "monkey_2");
        }

        #[tokio::test]
        async fn registration_ends_with_a_real_error_once_the_attempts_run_out() {
            let shared = shared();
            let (result, sent) = session(&shared, &identity(false), &[], |line: &str| {
                line.strip_prefix("NICK ")
                    .map(|nick| {
                        vec![format!(
                            ":irc.test 433 * {nick} :Nickname is already in use"
                        )]
                    })
                    .or(Some(Vec::new()))
            })
            .await;

            let error = result.expect_err("a server that refuses every nick is not a connection");
            assert!(error.contains("already in use"), "{error}");
            assert!(
                !shared.registered.load(Ordering::SeqCst),
                "never registered, so never connected"
            );
            assert_eq!(
                sent.iter().filter(|line| line.starts_with("NICK ")).count(),
                MAX_NICK_ATTEMPTS,
                "the ladder is bounded, not endless"
            );
        }

        #[tokio::test]
        async fn sasl_authenticates_as_the_account_even_after_the_nick_changed() {
            let shared = shared();
            // A server that holds registration until `CAP END`, which is what
            // the protocol says and what makes `001` mean anything: welcoming
            // a connection mid-authentication would be the downgrade.
            let state = std::sync::Mutex::new((false, "monkey".to_string()));
            let (_result, sent) = session(&shared, &identity(true), &[], move |line: &str| {
                if line == "CAP REQ :sasl" {
                    return Some(vec![":irc.test CAP * ACK :sasl".to_string()]);
                }
                if line == "AUTHENTICATE PLAIN" {
                    return Some(vec!["AUTHENTICATE +".to_string()]);
                }
                if line.starts_with("AUTHENTICATE ") {
                    return Some(vec![":irc.test 903 monkey :SASL successful".to_string()]);
                }
                if line == "CAP END" {
                    let nick = state.lock().expect("state").1.clone();
                    return Some(vec![format!(":irc.test 001 {nick} :Welcome")]);
                }
                if let Some(nick) = line.strip_prefix("NICK ") {
                    let mut state = state.lock().expect("state");
                    state.1 = nick.to_string();
                    if !state.0 {
                        state.0 = true;
                        return Some(vec![format!(
                            ":irc.test 433 * {nick} :Nickname is already in use"
                        )]);
                    }
                    return Some(Vec::new());
                }
                Some(Vec::new())
            })
            .await;

            // The nick did change...
            assert_eq!(*shared.active_nick.lock().await, "monkey_");
            // ...and the credential presented did not.
            let payload = sent
                .iter()
                .filter_map(|line| line.strip_prefix("AUTHENTICATE "))
                .find(|value| *value != "PLAIN")
                .expect("a SASL payload was sent");
            let decoded = String::from_utf8(BASE64.decode(payload).expect("base64")).expect("utf8");
            assert_eq!(decoded, "\0monkey-account\0hunter2");
            assert!(
                !decoded.contains("monkey_"),
                "authenticating as the collision nick would simply be rejected"
            );
        }

        #[tokio::test]
        async fn the_active_nick_is_the_servers_answer_not_our_last_guess() {
            let shared = shared();
            // A server that truncates rather than refusing: `001` is the only
            // place that says what we actually got.
            let _ = session(&shared, &identity(false), &[], |line: &str| {
                if line.starts_with("NICK ") {
                    return Some(vec![":irc.test 001 mnky :Welcome".to_string()]);
                }
                Some(Vec::new())
            })
            .await;
            assert_eq!(*shared.active_nick.lock().await, "mnky");
        }

        #[tokio::test]
        async fn configured_channels_are_joined_once_registration_completes() {
            let shared = shared();
            let channels = vec!["#general".to_string(), "#ops".to_string()];
            let (_result, sent) =
                session(&shared, &identity(false), &channels, welcome_after(0)).await;
            let joins: Vec<&String> = sent
                .iter()
                .filter(|line| line.starts_with("JOIN "))
                .collect();
            assert_eq!(joins, vec!["JOIN #general", "JOIN #ops"]);
        }

        #[tokio::test]
        async fn a_ping_is_answered_so_the_server_keeps_the_connection() {
            let shared = shared();
            let (_result, sent) = session(&shared, &identity(false), &[], |line: &str| {
                if line.starts_with("NICK ") {
                    return Some(vec![
                        ":irc.test 001 monkey :Welcome".to_string(),
                        "PING :token-1".to_string(),
                    ]);
                }
                Some(Vec::new())
            })
            .await;
            assert!(sent.iter().any(|line| line == "PONG :token-1"), "{sent:?}");
        }

        #[tokio::test]
        async fn channel_and_direct_messages_normalize_against_the_active_nick() {
            let shared = shared();
            let _ = session(&shared, &identity(false), &[], |line: &str| {
                if let Some(nick) = line.strip_prefix("NICK ") {
                    if nick == "monkey" {
                        return Some(vec![
                            ":irc.test 433 * monkey :Nickname is already in use".to_string()
                        ]);
                    }
                    return Some(vec![
                        ":irc.test 001 monkey_ :Welcome".to_string(),
                        ":alice!a@host PRIVMSG #general :hey monkey_ look at this".to_string(),
                        ":alice!a@host PRIVMSG monkey_ :and privately".to_string(),
                        ":monkey_!m@host PRIVMSG #general :our own line coming back".to_string(),
                    ]);
                }
                Some(Vec::new())
            })
            .await;

            let mut receiver = shared.inbound_rx.lock().await;
            let channel_message = receiver.try_recv().expect("a channel message");
            assert_eq!(channel_message.conversation.conversation_id, "#general");
            assert!(
                channel_message.mentions_self,
                "the mention is of the nick we actually have, not the one we asked for"
            );
            assert!(!channel_message.sender.is_self);

            let direct = receiver.try_recv().expect("a direct message");
            assert_eq!(
                direct.conversation.kind,
                little_monkey_lib::channels::types::ConversationKind::Direct
            );
            assert_eq!(direct.conversation.conversation_id, "alice");

            let echo = receiver.try_recv().expect("our own line");
            assert!(
                echo.sender.is_self,
                "our own line must not read as somebody else's"
            );
        }

        #[tokio::test]
        async fn a_reconnect_asks_for_the_preferred_nick_again() {
            let shared = shared();
            let identity = identity(false);
            let _ = session(&shared, &identity, &[], welcome_after(1)).await;
            assert_eq!(*shared.active_nick.lock().await, "monkey_");

            // What `connection_loop` does between attempts. The collision was
            // somebody else being online, not a decision the operator made.
            begin_attempt(&shared, &identity).await;
            assert_eq!(*shared.active_nick.lock().await, "monkey");

            let (_result, sent) = session(&shared, &identity, &[], welcome_after(0)).await;
            assert_eq!(
                sent.iter().find(|line| line.starts_with("NICK ")),
                Some(&"NICK monkey".to_string())
            );
            assert_eq!(*shared.active_nick.lock().await, "monkey");
        }

        // -- SASL fails closed ------------------------------------------------
        //
        // The failure class: SASL was explicitly enabled, authentication did
        // not happen, and the connection registered anyway. That is an
        // anonymous IRC session flying a healthy badge — and on most networks
        // it silently loses every channel that requires an account.

        /// A server that welcomes any nick it is asked for, and answers the
        /// SASL exchange with `refusal` at the point named by `at`.
        fn welcome_but_refuse_sasl(
            at: &'static str,
            refusal: &'static str,
        ) -> impl Fn(&str) -> Option<Vec<String>> + Send {
            move |line: &str| {
                if line == at {
                    return Some(vec![refusal.to_string()]);
                }
                if line == "CAP REQ :sasl" {
                    return Some(vec![":irc.test CAP * ACK :sasl".to_string()]);
                }
                if line == "AUTHENTICATE PLAIN" {
                    return Some(vec!["AUTHENTICATE +".to_string()]);
                }
                if let Some(nick) = line.strip_prefix("NICK ") {
                    return Some(vec![format!(":irc.test 001 {nick} :Welcome")]);
                }
                Some(Vec::new())
            }
        }

        /// Every SASL failure asserts the same three things, so they are
        /// asserted in one place: the attempt fails, nothing is Connected, and
        /// the reason names the account rather than the password.
        async fn assert_failed_closed(result: Result<(), String>, shared: &Arc<Shared>) {
            let error = result.expect_err("an unauthenticated session is not a connection");
            assert!(
                error.contains("SASL authentication failed"),
                "the reason has to be actionable: {error}"
            );
            assert!(
                error.contains("monkey-account"),
                "an operator needs to know which account to fix: {error}"
            );
            assert!(!error.contains("hunter2"), "the password leaked: {error}");
            assert!(
                !shared.registered.load(Ordering::SeqCst),
                "registered without authenticating"
            );
            assert_ne!(shared.status.get(), HealthState::Connected);
        }

        #[tokio::test]
        async fn a_refused_sasl_capability_fails_the_connection() {
            let shared = shared();
            let (result, sent) = session(
                &shared,
                &identity(true),
                &[],
                welcome_but_refuse_sasl("CAP REQ :sasl", ":irc.test CAP * NAK :sasl"),
            )
            .await;
            assert_failed_closed(result, &shared).await;
            assert!(
                !sent.iter().any(|line| line == "CAP END"),
                "ending negotiation here is what continues anonymously: {sent:?}"
            );
        }

        #[tokio::test]
        async fn a_rejected_credential_fails_the_connection() {
            for code in ["904", "905", "906", "907"] {
                let shared = shared();
                let refusal: &'static str = match code {
                    "904" => ":irc.test 904 * :SASL authentication failed",
                    "905" => ":irc.test 905 * :SASL message too long",
                    "906" => ":irc.test 906 * :SASL aborted",
                    _ => ":irc.test 907 * :Already authenticated",
                };
                let (result, _) = session(
                    &shared,
                    &identity(true),
                    &[],
                    welcome_but_refuse_sasl("AUTHENTICATE PLAIN", refusal),
                )
                .await;
                assert_failed_closed(result, &shared).await;
            }
        }

        #[tokio::test]
        async fn a_welcome_without_authentication_is_not_a_connection() {
            // The exact silent downgrade: the server never mentions SASL at
            // all and goes straight to `001`. Believing it would report an
            // anonymous session as Connected.
            let shared = shared();
            let (result, _) = session(&shared, &identity(true), &[], |line: &str| {
                if let Some(nick) = line.strip_prefix("NICK ") {
                    return Some(vec![format!(":irc.test 001 {nick} :Welcome")]);
                }
                Some(Vec::new())
            })
            .await;
            assert_failed_closed(result, &shared).await;
        }

        #[tokio::test]
        async fn a_sasl_exchange_that_never_answers_fails_rather_than_hanging() {
            // The server acknowledges the capability and then says nothing at
            // all — no `903`, no `904`, no close. Without a deadline this holds
            // a connection open that can never register.
            let shared = shared();
            let (result, _) = session(&shared, &identity(true), &[], |line: &str| {
                if line == "CAP REQ :sasl" {
                    return Some(vec![":irc.test CAP * ACK :sasl".to_string()]);
                }
                Some(Vec::new())
            })
            .await;
            assert_failed_closed(result, &shared).await;
        }

        #[tokio::test]
        async fn a_successful_sasl_session_registers_and_reports_the_active_nick() {
            let shared = shared();
            let (_result, sent) = session(&shared, &identity(true), &[], |line: &str| {
                if line == "CAP REQ :sasl" {
                    return Some(vec![":irc.test CAP * ACK :sasl".to_string()]);
                }
                if line == "AUTHENTICATE PLAIN" {
                    return Some(vec!["AUTHENTICATE +".to_string()]);
                }
                if line.starts_with("AUTHENTICATE ") {
                    return Some(vec![":irc.test 903 monkey :SASL successful".to_string()]);
                }
                if line == "CAP END" {
                    return Some(vec![":irc.test 001 monkey :Welcome".to_string()]);
                }
                Some(Vec::new())
            })
            .await;
            assert!(shared.registered.load(Ordering::SeqCst));
            assert_eq!(shared.status.get(), HealthState::Connected);
            assert_eq!(*shared.active_nick.lock().await, "monkey");
            assert!(sent.iter().any(|line| line == "CAP END"));
            assert!(
                !sent.iter().any(|line| line.contains("hunter2")),
                "the password is base64 in the payload and plaintext nowhere: {sent:?}"
            );
        }

        #[tokio::test]
        async fn without_sasl_capability_negotiation_is_ended_explicitly() {
            // `CAP LS` suspends registration until `CAP END`. Nothing is ever
            // requested without SASL, so the listing has to be answered or the
            // server never sends `001`.
            let shared = shared();
            let (_result, sent) = session(&shared, &identity(false), &[], |line: &str| {
                if line == "CAP LS 302" {
                    return Some(vec![
                        ":irc.test CAP * LS :multi-prefix sasl account-tag".to_string()
                    ]);
                }
                if line == "CAP END" {
                    return Some(vec![":irc.test 001 monkey :Welcome".to_string()]);
                }
                Some(Vec::new())
            })
            .await;
            assert!(sent.iter().any(|line| line == "CAP END"), "{sent:?}");
            assert!(shared.registered.load(Ordering::SeqCst));
        }

        #[tokio::test]
        async fn health_reports_the_nick_the_server_gave_and_never_the_password() {
            let account = super::super::super::super::channel_store::ChannelAccountRecord {
                account_id: "acct-1".to_string(),
                kind: ChannelKind::Irc,
                label: "IRC".to_string(),
                enabled: true,
                non_secret_config: serde_json::json!({
                    "server": "irc.test",
                    "nick": "monkey",
                    "use_sasl": true,
                    "sasl_username": "monkey-account",
                }),
                credential_ref: Some("irc/acct-1".to_string()),
                access_policy: Default::default(),
                health: ChannelHealth::error(0, "unused"),
                created_at_ms: 0,
                updated_at_ms: 0,
            };
            let adapter = IrcAdapter::new(&AdapterConfig {
                account: &account,
                secret: "hunter2".to_string(),
            })
            .expect("adapter");
            // Stand in for a completed registration without opening a socket:
            // this is the state `001` leaves behind.
            adapter.shared.registered.store(true, Ordering::SeqCst);
            *adapter.shared.active_nick.lock().await = "monkey_2".to_string();

            let health = adapter.probe().await;
            assert_eq!(health.state, HealthState::Connected);
            let detail = health.detail.unwrap_or_default();
            assert!(detail.contains("monkey_2"), "{detail}");
            assert!(!detail.contains("hunter2"), "{detail}");
        }

        #[tokio::test]
        async fn a_sasl_failure_is_what_health_reports() {
            let shared = shared();
            let (result, _) = session(
                &shared,
                &identity(true),
                &[],
                welcome_but_refuse_sasl("CAP REQ :sasl", ":irc.test CAP * NAK :sasl"),
            )
            .await;
            // What `connection_loop` does with the failure.
            *shared.last_error.lock().await = result.err();
            let reported = shared.last_error.lock().await.clone().expect("a reason");
            assert!(
                reported.contains("SASL authentication failed"),
                "{reported}"
            );
            assert!(!reported.contains("hunter2"), "{reported}");
        }

        #[tokio::test]
        async fn a_server_forced_rename_moves_the_active_nick_with_it() {
            let shared = shared();
            let _ = session(&shared, &identity(false), &[], |line: &str| {
                if line.starts_with("NICK ") {
                    return Some(vec![
                        ":irc.test 001 monkey :Welcome".to_string(),
                        ":monkey!m@host NICK :monkey-renamed".to_string(),
                    ]);
                }
                Some(Vec::new())
            })
            .await;
            assert_eq!(*shared.active_nick.lock().await, "monkey-renamed");
        }
    }
}
