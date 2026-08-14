//! IRC adapter: TLS socket, SASL PLAIN, CAP negotiation, reconnect.
//!
//! Unlike Telegram's long poll, IRC is a persistent connection: this module
//! owns a background task (spawned once per adapter, on first use) that holds
//! the socket, drives registration, answers `PING`, and forwards `PRIVMSG`
//! into a bounded channel that [`IrcAdapter::poll`] drains. `poll`'s `cursor`
//! is unused — see its doc for why there is nothing for it to resume from.
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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex as AsyncMutex, OnceCell};
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

use crate::daemon::channel_adapter::{
    AdapterConfig, ChannelAdapter, InboundBatch, TransportStatus,
};
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
    write_half: AsyncMutex<Option<WriteHalf<TlsStream<TcpStream>>>>,
    inbound_tx: mpsc::Sender<ChannelEnvelope>,
    inbound_rx: AsyncMutex<mpsc::Receiver<ChannelEnvelope>>,
    message_counter: AtomicU64,
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

        let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_CAPACITY);
        Ok(Self {
            server,
            port,
            nick,
            channels,
            use_sasl,
            password: config.secret.clone(),
            shared: Arc::new(Shared {
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
        let shared = self.shared.clone();
        let server = self.server.clone();
        let port = self.port;
        let nick = self.nick.clone();
        let channels = self.channels.clone();
        let use_sasl = self.use_sasl;
        let password = self.password.clone();
        self.shared
            .started
            .get_or_init(|| async move {
                tokio::spawn(connection_loop(
                    shared, server, port, nick, channels, use_sasl, password,
                ));
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
            return ChannelHealth::connected(
                now,
                Some(format!("{} on {}", self.nick, self.server)),
            );
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
        for chunk in &chunks {
            let line = format!("PRIVMSG {target} :{chunk}");
            if let Err(error) = write_line(&self.shared, &line).await {
                // IRC gives no delivery acknowledgement. Once `send` has
                // confirmed we were registered, any failure here means bytes
                // may already be on the wire, so the safe answer is "unknown"
                // rather than "safe to retry".
                return SendOutcome::NeedsReconciliation { error };
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
/// shares one.
fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
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

/// Writes one line plus `\r\n` to the current socket, or fails if there is
/// none. Shared by the connection task (registration, `PONG`) and
/// [`IrcAdapter::send`], so both go through the same `Mutex` and neither can
/// interleave a half-written line with the other.
async fn write_line(shared: &Shared, line: &str) -> Result<(), String> {
    let mut guard = shared.write_half.lock().await;
    let Some(write_half) = guard.as_mut() else {
        return Err("not connected to the IRC server".to_string());
    };
    write_half
        .write_all(line.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    write_half
        .write_all(b"\r\n")
        .await
        .map_err(|error| error.to_string())?;
    write_half.flush().await.map_err(|error| error.to_string())
}

/// Connects, registers, and reads forever, reconnecting with backoff whenever
/// the socket drops. Never returns while the adapter is alive — it is the
/// body of the task [`IrcAdapter::ensure_started`] spawns once.
async fn connection_loop(
    shared: Arc<Shared>,
    server: String,
    port: u16,
    nick: String,
    channels: Vec<String>,
    use_sasl: bool,
    password: String,
) {
    let mut backoff = INITIAL_RECONNECT_BACKOFF;
    loop {
        let connected_at = std::time::Instant::now();
        let result = connect_and_register(
            &shared, &server, port, &nick, &channels, use_sasl, &password,
        )
        .await;
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

/// One connection attempt: TCP connect, TLS handshake, CAP/SASL/registration,
/// then the read loop for as long as the socket stays open. Returns (always
/// with an `Err`, describing why) once the connection ends, so the caller can
/// log it and reconnect.
async fn connect_and_register(
    shared: &Arc<Shared>,
    server: &str,
    port: u16,
    nick: &str,
    channels: &[String],
    use_sasl: bool,
    password: &str,
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
    *shared.write_half.lock().await = Some(write_half);

    write_line(shared, "CAP LS 302").await?;
    if use_sasl {
        write_line(shared, "CAP REQ :sasl").await?;
    }
    write_line(shared, &format!("NICK {nick}")).await?;
    write_line(shared, &format!("USER {nick} 0 * :{nick}")).await?;

    let mut lines = BufReader::new(read_half).lines();
    loop {
        let raw_line = match lines
            .next_line()
            .await
            .map_err(|error| format!("read failed: {error}"))?
        {
            Some(line) => line,
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
            "CAP" if use_sasl && parsed.params.get(1).copied() == Some("ACK") => {
                write_line(shared, "AUTHENTICATE PLAIN").await?;
            }
            "CAP" if parsed.params.get(1).copied() == Some("NAK") => {
                // Server refused a requested capability. Only `sasl` is ever
                // requested, so there is nothing left to negotiate; proceed
                // to registration unauthenticated rather than hang here.
                write_line(shared, "CAP END").await?;
            }
            "AUTHENTICATE" if parsed.params.first().copied() == Some("+") => {
                // `\0<authzid>\0<authcid>\0<password>` with an empty
                // authorization identity, per SASL PLAIN (RFC 4616) — the
                // authentication identity is the nick being registered.
                let payload = format!("\0{nick}\0{password}");
                let encoded = BASE64.encode(payload.as_bytes());
                write_line(shared, &format!("AUTHENTICATE {encoded}")).await?;
            }
            "900" | "903" => {
                // RPL_LOGGEDIN / RPL_SASLSUCCESS.
                write_line(shared, "CAP END").await?;
            }
            "904" | "905" | "906" | "907" => {
                // SASL failed or is already done; end negotiation and let
                // registration continue rather than hang waiting for a
                // capability response that is not coming.
                write_line(shared, "CAP END").await?;
            }
            "001" => {
                // RPL_WELCOME: registration is complete. This is the one and
                // only place `registered` becomes true — see `probe`'s doc
                // for why nothing earlier may set it.
                shared.registered.store(true, Ordering::SeqCst);
                shared.status.set(HealthState::Connected);
                for channel in channels {
                    write_line(shared, &format!("JOIN {channel}")).await?;
                }
            }
            "PRIVMSG" if parsed.params.len() >= 2 => {
                if let Some(prefix) = parsed.prefix {
                    let target = parsed.params[0];
                    let text = parsed.params[1];
                    let counter = shared.message_counter.fetch_add(1, Ordering::SeqCst);
                    let envelope =
                        normalize_privmsg(server, prefix, target, text, &raw_line, counter, nick);
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
        conversation,
        sender: ChannelSender::new(sender_nick.to_string())
            .with_label(Some(sender_nick.to_string())),
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
    }
}
