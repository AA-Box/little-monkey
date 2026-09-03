//! Email adapter: the operator's own mailbox — IMAP in, SMTP out, implicit TLS
//! on both legs.
//!
//! # Transport
//!
//! Inbound is a poll, not IMAP IDLE: each poll opens one IMAP session, selects
//! one folder, fetches what is new by UID and logs out again, so
//! [`InboundTransport::LongPoll`] is the honest classification. IDLE is a
//! latency optimization over a correct poll and would add a background task, a
//! `TransportStatus` and a reconnect ladder for a handful of seconds; if that
//! ever matters, it goes in beside the polling path rather than replacing it.
//!
//! The worker's own idle tick is two seconds, which for a mailbox would be
//! roughly eighteen hundred logins an hour — enough for a provider to throttle
//! or lock the account. So [`EmailAdapter::poll`] paces itself: it sleeps out
//! the remainder of [`MIN_POLL_INTERVAL`] before touching the network, which is
//! what makes "polled" mean about twice a minute rather than as fast as the
//! loop can go. Outbound is a plain SMTP dialogue, written by hand over the
//! same kind of socket.
//!
//! # TLS is structural, not configured
//!
//! IMAP and SMTP are not HTTP, so neither leg can go through
//! `egress::hardened()` — the socket is a raw `tokio_rustls` client stream,
//! exactly as `irc.rs` opens one, and hostname verification is rustls's rather
//! than anything hand-written here. There is deliberately no STARTTLS path and
//! no cleartext path: [`EmailAdapter::new`] refuses the cleartext ports (IMAP
//! 143, SMTP 25) by number, so a downgrade is not a configuration mistake an
//! operator can make — it is a state this adapter cannot be built in.
//!
//! # Threading and trust
//!
//! A conversation is one correspondent address, lowercased; the thread is the
//! root of `References` (falling back to `In-Reply-To`, then to the message's
//! own `Message-ID`), and a reply carries both headers so it lands in the same
//! thread. A multi-recipient thread is therefore answered to the sender alone —
//! reply-to-list versus reply-to-sender is a policy decision nobody has made,
//! and guessing it is how a private answer reaches everyone on the thread. A
//! mailing-list post is not answered at all; see below.
//!
//! # What is deliberately never answered
//!
//! RFC 3834 says an automatic responder must not answer automatic mail, and a
//! mailbox that answers its own bounces is a loop with a postmaster in it. So
//! [`is_automated`] refuses, before an envelope exists at all, anything
//! carrying `Auto-Submitted:` other than `no`, a `Precedence:` of `bulk`,
//! `list` or `junk`, a `List-Id`/`List-Unsubscribe` header, or the empty
//! `Return-Path: <>` every bounce carries. Those messages are counted past by
//! the cursor and never become a turn.
//!
//! Everything else in a message is untrusted: headers, display names, subject
//! and body are provider payload, normalized into a [`ChannelEnvelope`] and
//! never concatenated into instructions. This adapter grants no access —
//! pairing, routing and whether anything runs at all stay `channel_ingress`'s.
//!
//! # Attachments
//!
//! Both halves are real, which is what makes `sends_attachments(Email)` true.
//! Inbound, the poll already holds the whole message (it refuses to fetch one
//! over [`MAX_MESSAGE_BYTES`] at all), so each attachment part is listed as an
//! [`AttachmentSource::ProviderHandle`] and its decoded bytes are kept in a
//! per-poll cache that [`EmailAdapter::fetch_attachment`] reads back. A second
//! IMAP session per attachment would be the alternative, and with the shared
//! hydration path downloading four at a time that is a fistful of concurrent
//! logins for one poll. The account's own cap is still enforced by
//! `hydrate_attachments`, which refuses an over-cap `declared_size_bytes`
//! before this adapter is asked for the bytes at all.
//!
//! That cache is the boundary, and it is narrower than it looks: hydration
//! runs in a different task from polling, the cursor has already advanced
//! past the UID, and the cache is replaced wholesale by the next poll. So a
//! daemon restart, or a hydration backlog long enough that another poll
//! lands first, loses those bytes permanently — the message text still
//! arrives and the attachment is still listed on the event, but the file is
//! not re-offered. Outbound, the reply is a real MIME multipart built by
//! `mail-builder`.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use futures_util::StreamExt;
use mail_parser::MimeHeaders;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Mutex as AsyncMutex;
use tokio_rustls::TlsConnector;

use little_monkey_lib::channels::types::{
    AttachmentKind, AttachmentSource, BoundedMetadata, ChannelAttachment, ChannelConversation,
    ChannelEnvelope, ChannelHealth, ChannelKind, ChannelSender, InboundTransport, OutboundMessage,
    ProviderCapabilities, SendOutcome,
};

use crate::daemon::channel_adapter::{
    load_attachments, AdapterConfig, AttachmentLimits, BlobSource, ChannelAdapter,
    ConversationReferences, DaemonBlobs, DaemonConversationReferences, InboundBatch,
    LoadedAttachment,
};

/// What one outbound mail may carry. Nothing outside this file reads
/// `ProviderCapabilities::max_text_chars` — there is no worker-side chunker —
/// so this is a declaration to the agent's tool schema and the setup UI, and
/// [`EmailAdapter::send`] is what honours it, by truncating with a visible
/// marker rather than by silently dropping the tail.
const EMAIL_MAX_TEXT_CHARS: usize = 16_384;

/// Ports whose only protocol is cleartext. Refused by number so there is no
/// downgrade to negotiate away.
const IMAP_CLEARTEXT_PORT: u16 = 143;
const SMTP_CLEARTEXT_PORT: u16 = 25;

const DEFAULT_IMAP_PORT: u16 = 993;
const DEFAULT_SMTP_PORT: u16 = 465;

/// The largest message this adapter will pull a body for.
///
/// Checked against `RFC822.SIZE` before the body is fetched, so an oversized
/// message costs one envelope line rather than its own size. A message over
/// the cap is not silently skipped: it becomes a one-line envelope saying so,
/// because a mailbox that quietly ignores mail is worse than one that says it
/// could not read it.
const MAX_MESSAGE_BYTES: u32 = 4 * 1024 * 1024;

/// How many messages one poll will take. The rest wait for the next poll —
/// the cursor only advances over what was actually processed.
const MAX_MESSAGES_PER_POLL: usize = 25;

/// The total body bytes one poll will fetch, and therefore the ceiling on the
/// attachment cache this poll fills. Bounds what one busy mailbox can make
/// this process hold at once.
const MAX_POLL_BYTES: usize = 24 * 1024 * 1024;

/// How much of one message's text is carried into a turn.
const EMAIL_TEXT_LIMIT: usize = 16_384;

/// The shortest gap between two IMAP sessions on one account.
///
/// The worker's idle tick is two seconds and it does not know that a mailbox
/// login is expensive, so the pacing lives here. See the module doc.
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// How long one whole IMAP session — connect, TLS, login, select, fetch,
/// logout — is allowed to take before the poll gives up and reports an error.
const SESSION_TIMEOUT: Duration = Duration::from_secs(120);

/// How long the SMTP dialogue gets.
const SMTP_TIMEOUT: Duration = Duration::from_secs(120);

/// How many `References` ids a reply carries. RFC 5322 permits trimming the
/// chain, every client threads on any shared id, and an unbounded chain is a
/// header a stranger can grow one message at a time.
const MAX_REFERENCES: usize = 10;

/// How long a stored subject may be. It is a stranger's text and it goes back
/// out on the wire.
const MAX_STORED_SUBJECT_CHARS: usize = 200;

pub struct EmailAdapter {
    account_id: String,
    imap: Endpoint,
    smtp: Endpoint,
    username: String,
    from_address: String,
    mailbox: String,
    secrets: EmailSecrets,
    limits: AttachmentLimits,
    refs: Arc<dyn ConversationReferences>,
    blobs: Arc<dyn BlobSource>,
    /// Decoded attachment parts from the most recent poll, keyed by the handle
    /// the envelope carries. Replaced wholesale each poll, so it is bounded by
    /// [`MAX_POLL_BYTES`] and cannot grow across polls.
    parts: AsyncMutex<HashMap<String, Vec<u8>>>,
    /// When the next poll may touch the network. See [`MIN_POLL_INTERVAL`].
    next_poll_at: AsyncMutex<Option<tokio::time::Instant>>,
}

pub(crate) struct Endpoint {
    pub host: String,
    pub port: u16,
}

/// The mailbox passwords, as one keychain bundle.
///
/// A bundle rather than a bare string because two legs authenticate: most
/// providers take the same app password on both, and the ones that do not need
/// somewhere to say so. `smtp_password` falling back to `imap_password` is what
/// the setup guide promises.
/// Deliberately neither `Debug` nor `Clone`: a `{:?}` of this type anywhere —
/// in a future error path, in a panic message — would put the mailbox password
/// in a log, and there is nothing here worth copying.
#[derive(serde::Deserialize)]
pub(crate) struct EmailSecrets {
    imap_password: String,
    #[serde(default)]
    smtp_password: Option<String>,
}

impl EmailSecrets {
    pub(crate) fn parse(secret: &str) -> Result<Self, String> {
        let bundle: Self = serde_json::from_str(secret.trim()).map_err(|_| {
            "Email needs a mailbox password bundle; store it with `monkey channels set-token` as \
             {\"imap_password\":\"...\"}"
                .to_string()
        })?;
        if bundle.imap_password.is_empty() {
            return Err("The email password bundle has an empty 'imap_password'".to_string());
        }
        Ok(bundle)
    }

    fn smtp(&self) -> &str {
        self.smtp_password
            .as_deref()
            .filter(|password| !password.is_empty())
            .unwrap_or(&self.imap_password)
    }
}

fn text(config: &AdapterConfig<'_>, key: &str) -> Option<String> {
    config
        .account
        .non_secret_config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn port(config: &AdapterConfig<'_>, key: &str, default: u16) -> Result<u16, String> {
    match config.account.non_secret_config.get(key) {
        None | Some(serde_json::Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|number| *number > 0 && *number <= u64::from(u16::MAX))
            .map(|number| number as u16)
            .ok_or_else(|| format!("Email '{key}' must be a port number")),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

impl EmailAdapter {
    pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
        let required = |key: &str| {
            text(config, key).ok_or_else(|| format!("Email configuration requires '{key}'"))
        };
        let imap = Endpoint {
            host: required("imap_host")?,
            port: port(config, "imap_port", DEFAULT_IMAP_PORT)?,
        };
        let smtp = Endpoint {
            host: required("smtp_host")?,
            port: port(config, "smtp_port", DEFAULT_SMTP_PORT)?,
        };
        // Refused here rather than checked at connect time: there is no
        // cleartext code path in this adapter to reach, and an account that
        // cannot be built is a mistake an operator sees at once.
        if imap.port == IMAP_CLEARTEXT_PORT {
            return Err(format!(
                "Email imap_port {IMAP_CLEARTEXT_PORT} is the cleartext IMAP port; this adapter \
                 only speaks implicit TLS (usually {DEFAULT_IMAP_PORT})"
            ));
        }
        if smtp.port == SMTP_CLEARTEXT_PORT {
            return Err(format!(
                "Email smtp_port {SMTP_CLEARTEXT_PORT} is the cleartext SMTP port; this adapter \
                 only speaks implicit TLS (usually {DEFAULT_SMTP_PORT})"
            ));
        }
        let from_address = required("from_address")?.to_ascii_lowercase();
        if !is_addr_spec(&from_address) {
            return Err(format!(
                "Email from_address '{from_address}' is not a plain mailbox address"
            ));
        }
        Ok(Self {
            account_id: config.account.account_id.clone(),
            imap,
            smtp,
            username: required("username")?,
            from_address,
            mailbox: text(config, "mailbox").unwrap_or_else(|| "INBOX".to_string()),
            secrets: EmailSecrets::parse(&config.secret)?,
            limits: AttachmentLimits::for_account(&config.account.non_secret_config),
            refs: Arc::new(DaemonConversationReferences::new()),
            blobs: Arc::new(DaemonBlobs),
            parts: AsyncMutex::new(HashMap::new()),
            next_poll_at: AsyncMutex::new(None),
        })
    }

    fn identity(&self) -> EmailIdentity<'_> {
        EmailIdentity {
            account_id: &self.account_id,
            from_address: &self.from_address,
        }
    }

    /// Sleep out whatever is left of [`MIN_POLL_INTERVAL`], then arm the next
    /// one. Held across the sleep on purpose: two concurrent polls of one
    /// account would each be a login.
    async fn pace(&self) {
        let mut next = self.next_poll_at.lock().await;
        if let Some(at) = *next {
            tokio::time::sleep_until(at).await;
        }
        *next = Some(tokio::time::Instant::now() + MIN_POLL_INTERVAL);
    }

    async fn open_session(&self) -> Result<ImapSession, String> {
        let stream = tls_stream(&self.imap.host, self.imap.port).await?;
        let mut client = async_imap::Client::new(stream);
        client
            .read_response()
            .await
            .map_err(|error| format!("The IMAP server sent no greeting: {error}"))?
            .ok_or_else(|| "The IMAP server closed before greeting us".to_string())?;
        // The `(Error, Client)` shape carries the client back so a caller can
        // retry; this one cannot, and the error is rendered without ever
        // touching the password.
        client
            .login(&self.username, &self.secrets.imap_password)
            .await
            .map_err(|(error, _)| format!("IMAP login for {} failed: {error}", self.username))
    }

    async fn poll_once(&self, cursor: Option<&str>) -> Result<(InboundBatch, PollParts), String> {
        let mut session = self.open_session().await?;
        let result = self.poll_session(&mut session, cursor).await;
        // Best effort: a mailbox that will not say goodbye has still answered.
        let _ = session.logout().await;
        result
    }

    async fn poll_session(
        &self,
        session: &mut ImapSession,
        cursor: Option<&str>,
    ) -> Result<(InboundBatch, PollParts), String> {
        let mailbox = session
            .select(&self.mailbox)
            .await
            .map_err(|error| format!("IMAP SELECT {} failed: {error}", self.mailbox))?;
        let uid_validity = mailbox
            .uid_validity
            .ok_or_else(|| format!("Mailbox {} reports no UIDVALIDITY", self.mailbox))?;
        let highest = mailbox.uid_next.unwrap_or(1).saturating_sub(1);

        // No cursor, or a mailbox that has been renumbered under us. Either way
        // the only safe resume point is "from here on": replaying a re-numbered
        // mailbox would re-run every message in it, and a new account must not
        // answer a year of backlog on its first poll.
        let Some(last_uid) = parse_cursor(cursor).filter(|(validity, _)| *validity == uid_validity)
        else {
            return Ok((
                InboundBatch {
                    envelopes: Vec::new(),
                    cursor: Some(format_cursor(uid_validity, highest)),
                },
                PollParts::default(),
            ));
        };
        let last_uid = last_uid.1;
        if highest <= last_uid {
            return Ok((InboundBatch::default(), PollParts::default()));
        }

        // Envelope pass: what is new, and how big. `UID FETCH n:*` always
        // answers with at least the highest message even when its UID is below
        // `n`, so the filter below is load-bearing rather than defensive.
        let mut candidates: Vec<(u32, u32)> = Vec::new();
        {
            let mut stream = session
                .uid_fetch(format!("{}:*", last_uid + 1), "(UID RFC822.SIZE)")
                .await
                .map_err(|error| format!("IMAP UID FETCH failed: {error}"))?;
            while let Some(fetch) = stream.next().await {
                let fetch = fetch.map_err(|error| format!("IMAP fetch stream failed: {error}"))?;
                if let Some(uid) = fetch.uid.filter(|uid| *uid > last_uid) {
                    candidates.push((uid, fetch.size.unwrap_or(0)));
                }
            }
        }
        candidates.sort_unstable();
        candidates.truncate(MAX_MESSAGES_PER_POLL);

        let mut oversized: Vec<(u32, u32)> = Vec::new();
        let mut wanted: Vec<u32> = Vec::new();
        let mut budget = MAX_POLL_BYTES;
        let mut resume_at = last_uid;
        for (uid, size) in candidates {
            if size > MAX_MESSAGE_BYTES {
                oversized.push((uid, size));
                resume_at = uid;
                continue;
            }
            if size as usize > budget {
                // Out of room this poll. Stop here rather than skipping ahead:
                // the cursor has not advanced past this message, so the next
                // poll picks it up.
                break;
            }
            budget -= size as usize;
            wanted.push(uid);
            resume_at = uid;
        }

        let mut bodies: Vec<(u32, i64, Vec<u8>)> = Vec::new();
        if !wanted.is_empty() {
            let set = wanted
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let mut stream = session
                .uid_fetch(set, "(UID INTERNALDATE BODY.PEEK[])")
                .await
                .map_err(|error| format!("IMAP UID FETCH body failed: {error}"))?;
            while let Some(fetch) = stream.next().await {
                let fetch = fetch.map_err(|error| format!("IMAP fetch stream failed: {error}"))?;
                let (Some(uid), Some(body)) = (fetch.uid, fetch.body()) else {
                    continue;
                };
                let received = fetch
                    .internal_date()
                    .map(|date| date.timestamp_millis())
                    .unwrap_or_else(now_ms);
                bodies.push((uid, received, body.to_vec()));
            }
        }
        bodies.sort_by_key(|(uid, _, _)| *uid);

        let mut envelopes = Vec::new();
        let mut parts = PollParts::default();
        for (uid, size) in oversized {
            envelopes.push(oversized_envelope(
                &self.identity(),
                uid,
                uid_validity,
                size,
                now_ms(),
            ));
        }
        for (uid, received, raw) in bodies {
            let Some(normalized) = normalize_message(
                &raw,
                uid,
                uid_validity,
                received,
                &self.identity(),
                self.limits,
            ) else {
                continue;
            };
            // Addressing only, and bounded: this is what a reply threads with.
            let _ = self.refs.put(
                &self.account_id,
                &normalized.envelope.conversation.conversation_id,
                &normalized.reference,
            );
            parts.push_all(normalized.parts);
            envelopes.push(normalized.envelope);
        }

        Ok((
            InboundBatch {
                envelopes,
                cursor: Some(format_cursor(uid_validity, resume_at)),
            },
            parts,
        ))
    }
}

type ImapSession = async_imap::Session<tokio_rustls::client::TlsStream<TcpStream>>;

/// The attachment bytes one poll decoded, ready to replace the adapter's cache.
#[derive(Debug, Default)]
pub(crate) struct PollParts {
    entries: Vec<(String, Vec<u8>)>,
}

impl PollParts {
    fn push_all(&mut self, entries: Vec<(String, Vec<u8>)>) {
        self.entries.extend(entries);
    }
}

#[async_trait]
impl ChannelAdapter for EmailAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Email
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            max_text_chars: EMAIL_MAX_TEXT_CHARS,
            supports_threads: true,
            supports_attachments: true,
            // Mail carries no mention metadata at all; gating falls back to
            // substring matching, which is what `false` tells the ingress.
            supports_mention_metadata: false,
            ..ProviderCapabilities::minimal(ChannelKind::Email, InboundTransport::LongPoll)
        }
    }

    async fn probe(&self) -> ChannelHealth {
        let now = now_ms();
        let probe = async {
            let mut session = self.open_session().await?;
            let mailbox = session
                .select(&self.mailbox)
                .await
                .map_err(|error| format!("IMAP SELECT {} failed: {error}", self.mailbox))?;
            let _ = session.logout().await;
            Ok::<_, String>(mailbox.exists)
        };
        match tokio::time::timeout(SESSION_TIMEOUT, probe).await {
            Ok(Ok(exists)) => ChannelHealth::connected(
                now,
                Some(format!(
                    "{} as {}, {exists} message(s)",
                    self.mailbox, self.username
                )),
            ),
            Ok(Err(error)) => ChannelHealth::error(now, error),
            Err(_) => ChannelHealth::error(
                now,
                format!(
                    "The IMAP server {} did not answer within {}s",
                    self.imap.host,
                    SESSION_TIMEOUT.as_secs()
                ),
            ),
        }
    }

    async fn poll(&self, cursor: Option<&str>) -> Result<InboundBatch, String> {
        self.pace().await;
        let (batch, parts) = tokio::time::timeout(SESSION_TIMEOUT, self.poll_once(cursor))
            .await
            .map_err(|_| {
                format!(
                    "The IMAP session with {} did not finish within {}s",
                    self.imap.host,
                    SESSION_TIMEOUT.as_secs()
                )
            })??;
        // Replaced, never merged: last poll's bytes have already been hydrated
        // or refused, and keeping them is how this cache would stop being
        // bounded by one poll.
        *self.parts.lock().await = parts.entries.into_iter().collect();
        Ok(batch)
    }

    async fn fetch_attachment(
        &self,
        attachment: &ChannelAttachment,
        limits: AttachmentLimits,
    ) -> Result<Vec<u8>, String> {
        let AttachmentSource::ProviderHandle { handle } = &attachment.source else {
            return Err("An email attachment is always a part of its own message".to_string());
        };
        let bytes = self
            .parts
            .lock()
            .await
            .get(handle)
            .cloned()
            .ok_or_else(|| {
                "Those attachment bytes were held only for the poll that read them, and that poll \
             has been replaced; the message is not offered again"
                    .to_string()
            })?;
        // The cap is checked upstream on the declared size too, but the
        // function that hands the bytes back is the honest place for it.
        if bytes.len() as u64 > limits.max_bytes {
            return Err(format!(
                "That attachment decodes to {} bytes, over this account's {} byte cap",
                bytes.len(),
                limits.max_bytes
            ));
        }
        Ok(bytes)
    }

    async fn send(&self, message: &OutboundMessage) -> SendOutcome {
        let recipient = message.conversation_id.trim().to_ascii_lowercase();
        if !is_addr_spec(&recipient) {
            // A conversation id reaches the header builder, so anything that
            // could fold a header or add a second recipient is refused here
            // rather than escaped later.
            return SendOutcome::PermanentFailure {
                error: format!(
                    "'{}' is not a single plain mailbox address, so there is nobody to reply to",
                    message.conversation_id
                ),
            };
        }
        let files = match load_attachments(self.blobs.as_ref(), message) {
            Ok(files) => files,
            Err(outcome) => return outcome,
        };
        let reference = self.refs.get(&self.account_id, &recipient);
        let message_id = mint_message_id(
            &self.account_id,
            &message.idempotency_key,
            &self.from_address,
        );
        let body = match build_mime(
            &bounded_text(&message.text, EMAIL_MAX_TEXT_CHARS),
            &self.from_address,
            &recipient,
            reference.as_ref(),
            message.reply_to_provider_id.as_deref(),
            &files,
            &message_id,
            now_ms() / 1_000,
        ) {
            Ok(body) => body,
            Err(error) => return SendOutcome::PermanentFailure { error },
        };

        let stream = match tls_stream(&self.smtp.host, self.smtp.port).await {
            Ok(stream) => stream,
            // Nothing left the machine: the socket never opened.
            Err(error) => {
                return SendOutcome::RetryableFailure {
                    error,
                    retry_after_ms: Some(5_000),
                }
            }
        };
        let envelope = SmtpEnvelope {
            ehlo_domain: domain_of(&self.from_address),
            username: &self.username,
            password: self.secrets.smtp(),
            from: &self.from_address,
            to: &recipient,
            message: &body,
        };
        match tokio::time::timeout(SMTP_TIMEOUT, smtp_exchange(stream, &envelope)).await {
            Ok(Ok(())) => SendOutcome::Sent {
                provider_message_id: Some(message_id),
            },
            Ok(Err(outcome)) => outcome,
            // The deadline can expire on either side of the terminating dot,
            // and this side cannot tell which. "Unknown" is the only safe
            // answer; a retry could deliver the mail twice.
            Err(_) => SendOutcome::NeedsReconciliation {
                error: format!(
                    "The SMTP dialogue with {} did not finish within {}s",
                    self.smtp.host,
                    SMTP_TIMEOUT.as_secs()
                ),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// TLS
// ---------------------------------------------------------------------------

/// The process-wide TLS client config, built once — the same shape `irc.rs`
/// uses, for the same reason: `rustls::ClientConfig`'s root store is immutable
/// after construction, so there is nothing per-connection to configure.
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

async fn tls_stream(
    host: &str,
    port: u16,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, String> {
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|error| format!("TCP connect to {host}:{port} failed: {error}"))?;
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|error| format!("invalid mail server name {host}: {error}"))?;
    TlsConnector::from(tls_config())
        .connect(name, tcp)
        .await
        .map_err(|error| format!("TLS handshake with {host}:{port} failed: {error}"))
}

// ---------------------------------------------------------------------------
// Cursor
// ---------------------------------------------------------------------------

/// `"<uidvalidity>:<uid>"`. Both halves matter: a UID alone means nothing once
/// the server renumbers the mailbox, and resuming from a stale UID would replay
/// messages that have already been answered.
fn format_cursor(uid_validity: u32, uid: u32) -> String {
    format!("{uid_validity}:{uid}")
}

fn parse_cursor(cursor: Option<&str>) -> Option<(u32, u32)> {
    let (validity, uid) = cursor?.split_once(':')?;
    Some((validity.parse().ok()?, uid.parse().ok()?))
}

// ---------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------

pub(crate) struct EmailIdentity<'a> {
    pub account_id: &'a str,
    /// The account's own address, lowercased. Equality with a message's `From`
    /// is this provider's echo correlation.
    pub from_address: &'a str,
}

pub(crate) struct NormalizedMessage {
    pub envelope: ChannelEnvelope,
    /// Threading addressing for this conversation: subject, message id and the
    /// chain a reply must carry. Bounded, and never anything a decision is made
    /// on.
    pub reference: serde_json::Value,
    /// `(handle, decoded bytes)` for each attachment small enough to keep.
    pub parts: Vec<(String, Vec<u8>)>,
}

/// Whether this message is machine-generated mail that must not be answered.
///
/// RFC 3834's rule, plus the empty `Return-Path` every bounce carries. Reading
/// the raw headers rather than the typed accessors is deliberate: the
/// interesting ones (`Auto-Submitted`, `Precedence`) are not in `mail-parser`'s
/// known-header set, so a case-insensitive scan is the only comparison that is
/// actually right.
pub(crate) fn is_automated(message: &mail_parser::Message<'_>) -> bool {
    for (name, value) in message.headers_raw() {
        let value = value.trim();
        if name.eq_ignore_ascii_case("list-id") || name.eq_ignore_ascii_case("list-unsubscribe") {
            return true;
        }
        if name.eq_ignore_ascii_case("auto-submitted") {
            let kind = value.split(';').next().unwrap_or(value).trim();
            if !kind.eq_ignore_ascii_case("no") {
                return true;
            }
        }
        if name.eq_ignore_ascii_case("precedence")
            && ["bulk", "list", "junk"]
                .iter()
                .any(|marker| value.eq_ignore_ascii_case(marker))
        {
            return true;
        }
        // `Return-Path: <>` is the null reverse path: a bounce, by definition
        // something no reply may be sent to.
        if name.eq_ignore_ascii_case("return-path") && (value == "<>" || value.is_empty()) {
            return true;
        }
    }
    false
}

/// One RFC 5322 message as this project's own envelope, or nothing.
///
/// Pure, and the seam every inbound test drives: it takes bytes and the two
/// numbers IMAP supplies, and reaches no network at all.
pub(crate) fn normalize_message(
    raw: &[u8],
    uid: u32,
    uid_validity: u32,
    internal_date_ms: i64,
    identity: &EmailIdentity<'_>,
    limits: AttachmentLimits,
) -> Option<NormalizedMessage> {
    let message = mail_parser::MessageParser::default().parse(raw)?;
    if is_automated(&message) {
        return None;
    }

    let from = message.from().and_then(mail_parser::Address::first);
    let address = from
        .and_then(mail_parser::Addr::address)
        .map(str::trim)
        .filter(|address| !address.is_empty())?
        .to_ascii_lowercase();
    let display = from
        .and_then(mail_parser::Addr::name)
        .map(|name| bounded_text(name, MAX_STORED_SUBJECT_CHARS));

    let subject = message
        .subject()
        .map(|subject| bounded_text(subject, MAX_STORED_SUBJECT_CHARS))
        .filter(|subject| !subject.is_empty());
    let message_id = message.message_id().map(strip_angles).map(str::to_string);
    let in_reply_to = header_ids(message.in_reply_to()).into_iter().next();
    let references = header_ids(message.references());

    // The root of the chain, which is what makes every message of one
    // exchange share a thread: the first `References` id, else what this
    // message answers, else itself.
    let thread_id = references
        .first()
        .cloned()
        .or_else(|| in_reply_to.clone())
        .or_else(|| message_id.clone());

    let provider_event_id = message_id.clone().unwrap_or_else(|| {
        deterministic_event_id(
            identity.account_id,
            uid_validity,
            uid,
            internal_date_ms,
            &address,
            subject.as_deref().unwrap_or(""),
        )
    });

    let text = message
        .body_text(0)
        .map(|body| bounded_text(&body, EMAIL_TEXT_LIMIT))
        .unwrap_or_default();

    let mut attachments = Vec::new();
    let mut parts = Vec::new();
    let mut budget = MAX_POLL_BYTES;
    for (index, part) in message.attachments().enumerate().take(limits.max_listed) {
        let bytes = part.contents();
        let handle = format!("{uid_validity}:{uid}:{index}");
        let mime = part
            .content_type()
            .map(|content| match content.subtype() {
                Some(subtype) => format!("{}/{subtype}", content.ctype()),
                None => content.ctype().to_string(),
            })
            .unwrap_or_else(|| "application/octet-stream".to_string());
        attachments.push(ChannelAttachment {
            provider_id: Some(handle.clone()),
            kind: AttachmentKind::from_mime(&mime),
            filename: part
                .attachment_name()
                .map(|name| bounded_text(name, MAX_STORED_SUBJECT_CHARS)),
            mime_type: Some(mime),
            declared_size_bytes: Some(bytes.len() as u64),
            stored_size_bytes: None,
            source: AttachmentSource::ProviderHandle {
                handle: handle.clone(),
            },
            stored_artifact_id: None,
            fetch_error: None,
            text_excerpt: None,
        });
        // Bytes are kept only for the parts the shared hydration path will
        // actually ask for: it refuses an over-cap `declared_size_bytes`
        // before calling this adapter at all, so caching one would be paying
        // memory for a refusal.
        if bytes.len() as u64 <= limits.max_bytes && bytes.len() <= budget {
            budget -= bytes.len();
            parts.push((handle, bytes.to_vec()));
        }
    }

    let mut metadata = BoundedMetadata::new();
    metadata.insert("uid", uid.to_string());
    metadata.insert("uidvalidity", uid_validity.to_string());

    let envelope = ChannelEnvelope {
        account_id: identity.account_id.to_string(),
        kind: ChannelKind::Email,
        provider_event_id,
        provider_message_id: message_id,
        conversation: ChannelConversation::direct(address.clone())
            .with_thread(thread_id)
            .with_title(subject.clone()),
        // `is_self` is decided by the account's own configured address, never
        // by anything the message claims about itself.
        sender: ChannelSender {
            is_self: address.eq_ignore_ascii_case(identity.from_address),
            ..ChannelSender::new(address).with_label(display)
        },
        text,
        attachments,
        reply_to_provider_id: in_reply_to.clone(),
        mentions_self: false,
        received_at_ms: internal_date_ms,
        metadata,
    };

    let mut chain = references;
    if let Some(id) = in_reply_to {
        if !chain.contains(&id) {
            chain.push(id);
        }
    }
    if let Some(id) = &envelope.provider_message_id {
        if !chain.contains(id) {
            chain.push(id.clone());
        }
    }
    trim_front(&mut chain, MAX_REFERENCES);

    Some(NormalizedMessage {
        reference: serde_json::json!({
            "subject": subject,
            "message_id": envelope.provider_message_id,
            "references": chain,
        }),
        parts,
        envelope,
    })
}

/// The envelope a message too large to read becomes.
///
/// Not silence: a mailbox that ignores mail without saying so is worse than one
/// that says it could not read it, and the operator is the only person who can
/// raise the cap or go and look.
fn oversized_envelope(
    identity: &EmailIdentity<'_>,
    uid: u32,
    uid_validity: u32,
    size: u32,
    received_at_ms: i64,
) -> ChannelEnvelope {
    ChannelEnvelope {
        account_id: identity.account_id.to_string(),
        kind: ChannelKind::Email,
        provider_event_id: deterministic_event_id(
            identity.account_id,
            uid_validity,
            uid,
            received_at_ms,
            "oversized",
            "",
        ),
        provider_message_id: None,
        conversation: ChannelConversation::direct(identity.from_address.to_string()),
        sender: ChannelSender {
            // A record for the operator, not a message from anybody. Without
            // `is_self` the note is a first-contact message *from the
            // operator's own address*: on a pairing-policy account it mints a
            // challenge, spends one of the account's pending slots and emails
            // the code to the mailbox that could not read the mail. Flagged,
            // `decide_access` ignores it as `OwnMessage`, so it stays a
            // durable event nobody is asked to answer.
            is_self: true,
            ..ChannelSender::new(identity.from_address.to_string())
        },
        text: format!(
            "[a message of {size} bytes was skipped: it is larger than the {MAX_MESSAGE_BYTES}-byte \
             limit this mailbox reads]"
        ),
        attachments: Vec::new(),
        reply_to_provider_id: None,
        mentions_self: false,
        received_at_ms,
        metadata: BoundedMetadata::new(),
    }
}

/// A deterministic, never-random dedupe key for a message with no `Message-ID`.
///
/// `channel_ingress` dedupes on `provider_event_id`, so a random id would
/// defeat dedupe on every redelivery rather than enable it.
fn deterministic_event_id(
    account_id: &str,
    uid_validity: u32,
    uid: u32,
    internal_date_ms: i64,
    from: &str,
    subject: &str,
) -> String {
    let mut hasher = Sha256::new();
    for part in [account_id, from, subject] {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(uid_validity.to_be_bytes());
    hasher.update(uid.to_be_bytes());
    hasher.update(internal_date_ms.to_be_bytes());
    format!("{:x}", hasher.finalize())
}

fn header_ids(value: &mail_parser::HeaderValue<'_>) -> Vec<String> {
    value
        .as_text_list()
        .map(|ids| {
            ids.iter()
                .map(|id| strip_angles(id).to_string())
                .filter(|id| !id.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn strip_angles(id: &str) -> &str {
    id.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim()
}

fn trim_front<T>(items: &mut Vec<T>, keep: usize) {
    if items.len() > keep {
        items.drain(..items.len() - keep);
    }
}

fn bounded_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut bounded: String = text.chars().take(max_chars.saturating_sub(16)).collect();
    bounded.push_str(" […truncated]");
    bounded
}

// ---------------------------------------------------------------------------
// Outbound: MIME
// ---------------------------------------------------------------------------

/// A message id minted from the account and the outbox row's idempotency key.
///
/// Deterministic, so a retry of the same row re-uses the same id — which is
/// what keeps the echo ledger consistent when a send is retried, and what makes
/// `Sent { provider_message_id }` an id the next inbound `References` will
/// actually carry.
fn mint_message_id(account_id: &str, idempotency_key: &str, from_address: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(account_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(idempotency_key.as_bytes());
    format!(
        "{:x}.little-monkey@{}",
        hasher.finalize(),
        domain_of(from_address)
    )
}

fn domain_of(address: &str) -> &str {
    address
        .rsplit_once('@')
        .map(|(_, domain)| domain)
        .unwrap_or("localhost")
}

/// Build the reply, headers and all.
///
/// Pure and injectable so the threading headers are testable without a socket:
/// what a reply carries is the whole of what makes it land in the right thread.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_mime(
    text: &str,
    from: &str,
    to: &str,
    reference: Option<&serde_json::Value>,
    reply_to_provider_id: Option<&str>,
    files: &[LoadedAttachment],
    message_id: &str,
    date_secs: i64,
) -> Result<Vec<u8>, String> {
    if !is_addr_spec(to) || !is_addr_spec(from) {
        return Err("A reply needs one plain mailbox address on each side".to_string());
    }
    let stored_subject = reference
        .and_then(|reference| reference.get("subject"))
        .and_then(serde_json::Value::as_str);
    let stored_id = reference
        .and_then(|reference| reference.get("message_id"))
        .and_then(serde_json::Value::as_str);
    let stored_chain: Vec<String> = reference
        .and_then(|reference| reference.get("references"))
        .and_then(serde_json::Value::as_array)
        .map(|ids| {
            ids.iter()
                .filter_map(serde_json::Value::as_str)
                .map(strip_angles)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let subject = match stored_subject {
        Some(subject) if !subject.trim().is_empty() => reply_subject(subject),
        _ => "Message from Little Monkey".to_string(),
    };
    let in_reply_to = reply_to_provider_id
        .map(strip_angles)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .or_else(|| stored_id.map(strip_angles).map(str::to_string))
        .filter(|id| !id.is_empty());

    let mut chain = stored_chain;
    if let Some(id) = &in_reply_to {
        if !chain.contains(id) {
            chain.push(id.clone());
        }
    }
    trim_front(&mut chain, MAX_REFERENCES);

    let mut builder = mail_builder::MessageBuilder::new()
        .from(from)
        .to(to)
        .subject(subject)
        .date(date_secs)
        .message_id(message_id.to_string())
        .text_body(text.to_string());
    if let Some(id) = in_reply_to {
        builder = builder.in_reply_to(id);
    }
    if !chain.is_empty() {
        builder = builder.references(chain);
    }
    for file in files {
        builder = builder.attachment(
            file.mime_type.clone(),
            file.filename.clone(),
            file.bytes.clone(),
        );
    }
    builder
        .write_to_vec()
        .map_err(|error| format!("Could not build the reply message: {error}"))
}

/// `Re: ` exactly once, however many the stored subject already had.
fn reply_subject(subject: &str) -> String {
    let mut base = subject.trim();
    loop {
        let stripped = base
            .strip_prefix("Re:")
            .or_else(|| base.strip_prefix("RE:"));
        match stripped.or_else(|| base.strip_prefix("re:")) {
            Some(rest) => base = rest.trim_start(),
            None => break,
        }
    }
    format!("Re: {}", bounded_text(base, MAX_STORED_SUBJECT_CHARS))
}

/// One plain `local@domain`, and nothing that could become two.
///
/// A conversation id is a stranger's address that ends up in a header, so a
/// comma (a second recipient), a CR or LF (a folded header, or an injected
/// one), a space or an angle bracket are all refusals rather than things to
/// escape further down.
pub(crate) fn is_addr_spec(address: &str) -> bool {
    if address.is_empty() || address.len() > 254 {
        return false;
    }
    if address.bytes().any(|byte| !(0x21..=0x7e).contains(&byte)) {
        return false;
    }
    if address.contains([',', ';', '<', '>', '"', '\\', '(', ')', '[', ']', ':']) {
        return false;
    }
    let Some((local, domain)) = address.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}

// ---------------------------------------------------------------------------
// Outbound: SMTP
// ---------------------------------------------------------------------------

pub(crate) struct SmtpEnvelope<'a> {
    pub ehlo_domain: &'a str,
    pub username: &'a str,
    pub password: &'a str,
    pub from: &'a str,
    pub to: &'a str,
    pub message: &'a [u8],
}

/// One SMTP delivery over an already-established stream.
///
/// Generic over the socket so the whole dialogue — greeting, EHLO, AUTH,
/// envelope, DATA, dot-stuffing, the terminator — is driven by a test over an
/// in-memory pipe rather than only by a real relay.
///
/// `Err` is the outcome the outbox should record, classified once here:
/// anything that fails **before** the terminating dot provably left nothing
/// behind and is retryable; a `5xx` is permanent whenever it arrives; and an
/// I/O failure at or after the terminator is `NeedsReconciliation`, because the
/// relay may already have queued the message and there is no id to reconcile
/// against.
pub(crate) async fn smtp_exchange<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    envelope: &SmtpEnvelope<'_>,
) -> Result<(), SendOutcome> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    let (code, line) = read_reply(&mut reader).await.map_err(before_data)?;
    check(code, &line, "greeting")?;

    write_line(&mut writer, &format!("EHLO {}", envelope.ehlo_domain))
        .await
        .map_err(before_data)?;
    let (code, greeting) = read_reply(&mut reader).await.map_err(before_data)?;
    check(code, &greeting, "EHLO")?;

    let mechanism = auth_mechanism(&greeting).ok_or_else(|| SendOutcome::PermanentFailure {
        error: "The SMTP server offered no AUTH PLAIN or AUTH LOGIN, and this adapter will not \
                send unauthenticated mail"
            .to_string(),
    })?;
    match mechanism {
        AuthMechanism::Plain => {
            let mut secret = Vec::new();
            secret.push(0u8);
            secret.extend_from_slice(envelope.username.as_bytes());
            secret.push(0u8);
            secret.extend_from_slice(envelope.password.as_bytes());
            write_line(
                &mut writer,
                &format!("AUTH PLAIN {}", BASE64.encode(&secret)),
            )
            .await
            .map_err(before_data)?;
            let (code, line) = read_reply(&mut reader).await.map_err(before_data)?;
            check(code, &line, "AUTH")?;
        }
        AuthMechanism::Login => {
            write_line(&mut writer, "AUTH LOGIN")
                .await
                .map_err(before_data)?;
            let (code, line) = read_reply(&mut reader).await.map_err(before_data)?;
            check(code, &line, "AUTH")?;
            write_line(&mut writer, &BASE64.encode(envelope.username.as_bytes()))
                .await
                .map_err(before_data)?;
            let (code, line) = read_reply(&mut reader).await.map_err(before_data)?;
            check(code, &line, "AUTH")?;
            write_line(&mut writer, &BASE64.encode(envelope.password.as_bytes()))
                .await
                .map_err(before_data)?;
            let (code, line) = read_reply(&mut reader).await.map_err(before_data)?;
            check(code, &line, "AUTH")?;
        }
    }

    write_line(&mut writer, &format!("MAIL FROM:<{}>", envelope.from))
        .await
        .map_err(before_data)?;
    let (code, line) = read_reply(&mut reader).await.map_err(before_data)?;
    check(code, &line, "MAIL FROM")?;

    write_line(&mut writer, &format!("RCPT TO:<{}>", envelope.to))
        .await
        .map_err(before_data)?;
    let (code, line) = read_reply(&mut reader).await.map_err(before_data)?;
    check(code, &line, "RCPT TO")?;

    write_line(&mut writer, "DATA").await.map_err(before_data)?;
    let (code, line) = read_reply(&mut reader).await.map_err(before_data)?;
    check(code, &line, "DATA")?;

    writer
        .write_all(&dot_stuffed(envelope.message))
        .await
        .map_err(before_data)?;
    // From here on nothing is retryable: the relay may have the message.
    writer.write_all(b"\r\n.\r\n").await.map_err(after_data)?;
    writer.flush().await.map_err(after_data)?;
    let (code, line) = read_reply(&mut reader).await.map_err(after_data)?;
    check(code, &line, "message body")?;

    // A relay that will not say goodbye has still accepted the message.
    let _ = write_line(&mut writer, "QUIT").await;
    Ok(())
}

enum AuthMechanism {
    Plain,
    Login,
}

/// What the server's own EHLO said it accepts, preferring `PLAIN`.
///
/// Read from the greeting rather than assumed: offering a mechanism the server
/// never advertised is how an adapter sends a password into a dialogue that
/// cannot use it.
fn auth_mechanism(greeting: &str) -> Option<AuthMechanism> {
    let upper = greeting.to_ascii_uppercase();
    let offered = upper.split_whitespace().collect::<Vec<_>>();
    let has =
        |mechanism: &str| upper.contains("AUTH") && offered.iter().any(|word| *word == mechanism);
    if has("PLAIN") {
        Some(AuthMechanism::Plain)
    } else if has("LOGIN") {
        Some(AuthMechanism::Login)
    } else {
        None
    }
}

/// The one place a response code becomes an outcome.
fn check(code: u16, line: &str, stage: &str) -> Result<(), SendOutcome> {
    if (200..400).contains(&code) {
        return Ok(());
    }
    // Never `line` verbatim in the retryable/permanent split: the code decides,
    // and the text is only ever a diagnostic. A server that echoes a command
    // back is why the AUTH stage's text is dropped entirely.
    let detail = if stage == "AUTH" {
        format!("the SMTP server refused authentication for this mailbox with {code}")
    } else {
        format!(
            "the SMTP server answered {code} at {stage}: {}",
            bounded_text(line, 200)
        )
    };
    if (400..500).contains(&code) {
        Err(SendOutcome::RetryableFailure {
            error: detail,
            retry_after_ms: Some(60_000),
        })
    } else {
        Err(SendOutcome::PermanentFailure { error: detail })
    }
}

fn before_data(error: std::io::Error) -> SendOutcome {
    SendOutcome::RetryableFailure {
        error: format!("The SMTP dialogue failed before the message was sent: {error}"),
        retry_after_ms: Some(30_000),
    }
}

fn after_data(error: std::io::Error) -> SendOutcome {
    SendOutcome::NeedsReconciliation {
        error: format!(
            "The SMTP connection failed after the message body was written, so the relay may \
             already have queued it: {error}"
        ),
    }
}

async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, line: &str) -> std::io::Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\r\n").await?;
    writer.flush().await
}

/// Read one reply, joining the `250-`-continued lines into one string.
async fn read_reply<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<(u16, String)> {
    let mut collected = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the SMTP server closed the connection",
            ));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if !collected.is_empty() {
            collected.push(' ');
        }
        collected.push_str(trimmed);
        let bytes = trimmed.as_bytes();
        if bytes.len() < 4 || bytes[3] != b'-' {
            let code = trimmed
                .get(..3)
                .and_then(|code| code.parse().ok())
                .unwrap_or(0);
            return Ok((code, collected));
        }
    }
}

/// RFC 5321 section 4.5.2: a body line that begins with a period gets a second
/// one, so nothing in the message can terminate it early.
fn dot_stuffed(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + 16);
    let mut at_line_start = true;
    for &byte in message {
        if at_line_start && byte == b'.' {
            out.push(b'.');
        }
        out.push(byte);
        at_line_start = byte == b'\n';
    }
    // Strip a trailing newline: the terminator supplies its own CRLF, and two
    // would add an empty line to every message.
    while out.last() == Some(&b'\n') || out.last() == Some(&b'\r') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::channel_adapter::{
        echo_correlation_for, hydrate_attachments, MemoryConversationReferences,
    };
    use crate::daemon::channel_store::ChannelAccountRecord;
    use little_monkey_lib::channels::policy::{ChannelAccessPolicy, EchoCorrelation};
    use little_monkey_lib::channels::types::{
        ChannelHealth as Health, ConversationKind, HealthState,
    };

    const ACCOUNT: &str = "chan-email";
    const OURS: &str = "you@example.org";

    fn account(settings: serde_json::Value) -> ChannelAccountRecord {
        ChannelAccountRecord {
            account_id: ACCOUNT.into(),
            kind: ChannelKind::Email,
            label: "Mailbox".into(),
            enabled: true,
            non_secret_config: settings,
            credential_ref: Some("channel:chan-email".into()),
            access_policy: ChannelAccessPolicy::default(),
            health: Health {
                state: HealthState::Disconnected,
                detail: None,
                last_error: None,
                probed_at_ms: 1,
            },
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn valid() -> serde_json::Value {
        serde_json::json!({
            "imap_host": "imap.example.org",
            "smtp_host": "smtp.example.org",
            "username": OURS,
            "from_address": OURS,
        })
    }

    fn build_error(settings: serde_json::Value) -> String {
        build(settings).err().expect("expected a refusal")
    }

    fn build(settings: serde_json::Value) -> Result<EmailAdapter, String> {
        build_with(settings, "{\"imap_password\":\"hunter2-imap\"}")
    }

    fn build_with(settings: serde_json::Value, secret: &str) -> Result<EmailAdapter, String> {
        let record = account(settings);
        EmailAdapter::new(&AdapterConfig {
            account: &record,
            secret: secret.to_string(),
        })
    }

    fn identity() -> EmailIdentity<'static> {
        EmailIdentity {
            account_id: ACCOUNT,
            from_address: OURS,
        }
    }

    fn normalize(raw: &str) -> NormalizedMessage {
        normalize_message(
            raw.as_bytes(),
            42,
            7,
            1_700_000_000_000,
            &identity(),
            AttachmentLimits::default(),
        )
        .expect("expected an envelope")
    }

    // ---- recorded fixtures: real RFC 5322 bytes, no network anywhere ----

    const PLAIN: &str = concat!(
        "Return-Path: <ada@example.com>\r\n",
        "From: Ada Lovelace <Ada@Example.COM>\r\n",
        "To: you@example.org\r\n",
        "Subject: Engine notes\r\n",
        "Message-ID: <m2@example.com>\r\n",
        "In-Reply-To: <m1@example.com>\r\n",
        "References: <m0@example.com> <m1@example.com>\r\n",
        "Date: Mon, 4 Dec 2023 09:00:00 +0000\r\n",
        "\r\n",
        "The engine weaves algebraic patterns.\r\n",
    );

    const ENCODED_WORDS: &str = concat!(
        "From: =?utf-8?B?QW5kcsOpIEjDqWxsc3Ryw7Zt?= <andre@example.com>\r\n",
        "To: you@example.org\r\n",
        "Subject: =?utf-8?Q?Caf=C3=A9_r=C3=A9union?=\r\n",
        "Message-ID: <enc@example.com>\r\n",
        "\r\n",
        "bonjour\r\n",
    );

    const FROM_OURSELVES: &str = concat!(
        "From: Little Monkey <YOU@example.org>\r\n",
        "To: ada@example.com\r\n",
        "Subject: Re: Engine notes\r\n",
        "Message-ID: <echo@example.org>\r\n",
        "\r\n",
        "already answered\r\n",
    );

    const NO_MESSAGE_ID: &str = concat!(
        "From: ada@example.com\r\n",
        "To: you@example.org\r\n",
        "Subject: no id here\r\n",
        "\r\n",
        "body\r\n",
    );

    const DEEP_CHAIN: &str = concat!(
        "From: ada@example.com\r\n",
        "To: you@example.org\r\n",
        "Subject: Re: Re: Engine notes\r\n",
        "Message-ID: <m5@example.com>\r\n",
        "In-Reply-To: <m4@example.com>\r\n",
        "References: <m0@example.com> <m1@example.com> <m2@example.com>\r\n",
        " <m3@example.com> <m4@example.com>\r\n",
        "\r\n",
        "still going\r\n",
    );

    /// multipart/mixed with a base64 part, so the decoded length is what the
    /// envelope declares rather than the encoded one.
    fn with_attachment(payload_bytes: usize) -> String {
        let payload = BASE64.encode(vec![b'x'; payload_bytes]);
        let mut wrapped = String::new();
        for chunk in payload.as_bytes().chunks(76) {
            wrapped.push_str(std::str::from_utf8(chunk).expect("base64 is ascii"));
            wrapped.push_str("\r\n");
        }
        format!(
            concat!(
                "From: ada@example.com\r\n",
                "To: you@example.org\r\n",
                "Subject: with a file\r\n",
                "Message-ID: <att@example.com>\r\n",
                "MIME-Version: 1.0\r\n",
                "Content-Type: multipart/mixed; boundary=\"bnd\"\r\n",
                "\r\n",
                "--bnd\r\n",
                "Content-Type: text/plain; charset=utf-8\r\n",
                "\r\n",
                "see attached\r\n",
                "--bnd\r\n",
                "Content-Type: application/pdf; name=\"report.pdf\"\r\n",
                "Content-Disposition: attachment; filename=\"report.pdf\"\r\n",
                "Content-Transfer-Encoding: base64\r\n",
                "\r\n",
                "{}",
                "--bnd--\r\n",
            ),
            wrapped
        )
    }

    // ---- construction ----

    #[test]
    fn a_cleartext_port_is_refused_at_construction() {
        // Not a runtime check that could be bypassed by a server offering
        // STARTTLS: there is no cleartext socket in this adapter to open.
        let mut settings = valid();
        settings["imap_port"] = serde_json::json!(143);
        let error = build_error(settings);
        assert!(error.contains("implicit TLS"), "{error}");

        let mut settings = valid();
        settings["smtp_port"] = serde_json::json!(25);
        let error = build_error(settings);
        assert!(error.contains("implicit TLS"), "{error}");
    }

    #[test]
    fn the_tls_ports_are_the_defaults() {
        let adapter = build(valid()).ok().expect("valid mailbox");
        assert_eq!(adapter.imap.port, DEFAULT_IMAP_PORT);
        assert_eq!(adapter.smtp.port, DEFAULT_SMTP_PORT);
        assert_eq!(adapter.mailbox, "INBOX");
    }

    #[test]
    fn attachments_and_threads_are_declared_because_both_halves_are_the_plan() {
        let capabilities = build(valid()).ok().expect("valid mailbox").capabilities();
        assert!(capabilities.supports_threads);
        assert!(capabilities.supports_attachments);
        assert!(!capabilities.supports_mention_metadata);
        assert_eq!(capabilities.inbound_transport, InboundTransport::LongPoll);
        assert_eq!(capabilities.max_text_chars, EMAIL_MAX_TEXT_CHARS);
    }

    #[test]
    fn a_password_bundle_is_required_and_smtp_falls_back_to_imap() {
        let error = build_with(valid(), "hunter2")
            .err()
            .expect("a bare string is not a bundle");
        assert!(error.contains("set-token"), "{error}");
        assert!(build_with(valid(), "{\"imap_password\":\"\"}").is_err());

        let adapter = build(valid()).ok().expect("valid mailbox");
        assert_eq!(adapter.secrets.smtp(), "hunter2-imap");
        let both = build_with(valid(), "{\"imap_password\":\"a\",\"smtp_password\":\"b\"}")
            .ok()
            .expect("valid mailbox");
        assert_eq!(both.secrets.smtp(), "b");
    }

    // ---- normalization ----

    #[test]
    fn a_plain_message_becomes_the_expected_envelope() {
        let normalized = normalize(PLAIN);
        let envelope = &normalized.envelope;
        assert_eq!(envelope.conversation.conversation_id, "ada@example.com");
        assert_eq!(envelope.conversation.kind, ConversationKind::Direct);
        assert_eq!(envelope.conversation.title.as_deref(), Some("Engine notes"));
        // The root of References, so every message of one exchange shares it.
        assert_eq!(
            envelope.conversation.thread_id.as_deref(),
            Some("m0@example.com")
        );
        assert_eq!(
            envelope.provider_message_id.as_deref(),
            Some("m2@example.com")
        );
        assert_eq!(envelope.provider_event_id, "m2@example.com");
        assert_eq!(
            envelope.reply_to_provider_id.as_deref(),
            Some("m1@example.com")
        );
        assert_eq!(envelope.sender.sender_id, "ada@example.com");
        assert_eq!(
            envelope.sender.display_label.as_deref(),
            Some("Ada Lovelace")
        );
        assert!(!envelope.sender.is_self);
        assert!(envelope.text.contains("algebraic patterns"));
        assert!(envelope.attachments.is_empty());
        assert_eq!(
            normalized.reference["references"],
            serde_json::json!(["m0@example.com", "m1@example.com", "m2@example.com"])
        );
    }

    #[test]
    fn an_encoded_word_subject_and_name_survive_normalization() {
        let normalized = normalize(ENCODED_WORDS);
        assert_eq!(
            normalized.envelope.conversation.title.as_deref(),
            Some("Café réunion")
        );
        assert_eq!(
            normalized.envelope.sender.display_label.as_deref(),
            Some("André Héllström")
        );
    }

    #[test]
    fn a_message_from_our_own_address_is_marked_is_self() {
        // Case-insensitively: the header said YOU@example.org.
        let normalized = normalize(FROM_OURSELVES);
        assert!(normalized.envelope.sender.is_self);
        assert_eq!(
            echo_correlation_for(&account(valid())),
            EchoCorrelation::HostAdapter
        );
    }

    #[test]
    fn a_message_with_no_message_id_gets_a_deterministic_event_id() {
        let first = normalize(NO_MESSAGE_ID).envelope.provider_event_id;
        let again = normalize(NO_MESSAGE_ID).envelope.provider_event_id;
        assert_eq!(first, again);
        assert!(first.len() == 64 && first.chars().all(|c| c.is_ascii_hexdigit()));
        // A UUID has hyphens and this must never be one: a random id defeats
        // dedupe on every redelivery instead of enabling it.
        assert!(!first.contains('-'));

        let different = normalize_message(
            NO_MESSAGE_ID.as_bytes(),
            43,
            7,
            1_700_000_000_000,
            &identity(),
            AttachmentLimits::default(),
        )
        .expect("envelope")
        .envelope
        .provider_event_id;
        assert_ne!(first, different);
        // And it is only ever synthesized where the provider gave nothing.
        assert!(normalize(NO_MESSAGE_ID)
            .envelope
            .provider_message_id
            .is_none());
    }

    #[test]
    fn a_long_reference_chain_is_bounded_but_keeps_its_root_thread() {
        let normalized = normalize(DEEP_CHAIN);
        assert_eq!(
            normalized.envelope.conversation.thread_id.as_deref(),
            Some("m0@example.com")
        );
        let chain = normalized.reference["references"]
            .as_array()
            .expect("a chain");
        assert!(chain.len() <= MAX_REFERENCES);
        assert_eq!(chain.last().unwrap(), "m5@example.com");
    }

    #[test]
    fn automated_mail_is_never_answered() {
        // RFC 3834's rule plus the bounce's null return path. Each of these
        // would otherwise become a turn whose reply goes to a postmaster or a
        // mailing list.
        for header in [
            "Auto-Submitted: auto-replied",
            "Auto-Submitted: auto-generated; owner=vacation",
            "Precedence: bulk",
            "Precedence: list",
            "List-Id: <dev.example.com>",
            "List-Unsubscribe: <mailto:x@example.com>",
            "Return-Path: <>",
        ] {
            let raw = format!("{header}\r\n{PLAIN}");
            assert!(
                normalize_message(
                    raw.as_bytes(),
                    1,
                    1,
                    1,
                    &identity(),
                    AttachmentLimits::default()
                )
                .is_none(),
                "{header} should not have produced an envelope"
            );
        }
        // And the explicit opt-out is still ordinary mail.
        let raw = format!("Auto-Submitted: no\r\n{PLAIN}");
        assert!(normalize_message(
            raw.as_bytes(),
            1,
            1,
            1,
            &identity(),
            AttachmentLimits::default()
        )
        .is_some());
    }

    // ---- attachments ----

    struct MemoryBlobs(std::sync::Mutex<Vec<Vec<u8>>>);

    impl BlobSource for MemoryBlobs {
        fn read(&self, artifact_id: &str) -> Result<Vec<u8>, String> {
            let index: usize = artifact_id
                .parse()
                .map_err(|_| "no such blob".to_string())?;
            self.0
                .lock()
                .unwrap()
                .get(index)
                .cloned()
                .ok_or_else(|| "no such blob".to_string())
        }

        fn write(&self, bytes: &[u8]) -> Result<String, String> {
            let mut blobs = self.0.lock().unwrap();
            blobs.push(bytes.to_vec());
            Ok((blobs.len() - 1).to_string())
        }
    }

    #[tokio::test]
    async fn an_attachment_is_listed_and_hydrated_from_the_polls_own_bytes() {
        let raw = with_attachment(2_048);
        let normalized = normalize(&raw);
        let attachment = &normalized.envelope.attachments[0];
        assert_eq!(attachment.filename.as_deref(), Some("report.pdf"));
        assert_eq!(attachment.mime_type.as_deref(), Some("application/pdf"));
        // The *decoded* length, which is what the shared hydration path then
        // compares the bytes it receives against.
        assert_eq!(attachment.declared_size_bytes, Some(2_048));

        let adapter = build(valid()).ok().expect("valid mailbox");
        *adapter.parts.lock().await = normalized.parts.into_iter().collect();
        let blobs = MemoryBlobs(std::sync::Mutex::new(Vec::new()));
        let mut envelopes = vec![normalized.envelope];
        hydrate_attachments(
            &adapter,
            &blobs,
            AttachmentLimits::default(),
            &mut envelopes,
        )
        .await;
        let stored = &envelopes[0].attachments[0];
        assert_eq!(stored.fetch_error, None);
        assert_eq!(stored.stored_size_bytes, Some(2_048));
        assert!(stored.stored_artifact_id.is_some());
    }

    #[test]
    fn a_note_about_an_unreadable_message_asks_the_operator_for_nothing() {
        let identity = identity();
        let note = oversized_envelope(&identity, 42, 7, 9 * 1024 * 1024, 1_700_000_000_000);
        assert!(note.text.contains("was skipped"));
        assert!(note.sender.is_self, "the note is a record, not a message");

        // The consequence, on the default pairing policy: recorded and
        // ignored, so no challenge, no pending slot and no reply email.
        use little_monkey_lib::channels::policy::{
            AccessContext, AccessDecision, ChannelAccessPolicy, IgnoreReason,
        };
        let policy = ChannelAccessPolicy::default();
        let decision = little_monkey_lib::channels::policy::decide_access(
            &note,
            AccessContext {
                policy: &policy,
                sender: None,
                pending_pairings: 0,
                automated_reply_depth: 0,
                consecutive_machine_messages: 0,
                own_outbound_echo: false,
                now_ms: 1_700_000_000_000,
            },
            || "unused".to_string(),
        );
        assert_eq!(decision, AccessDecision::Ignore(IgnoreReason::OwnMessage));
    }

    #[tokio::test]
    async fn bytes_a_later_poll_replaced_are_reported_as_gone_and_not_promised_again() {
        // The cache is this adapter's one boundary, so the miss path must say
        // plainly that the file is gone rather than imply a retry.
        let raw = with_attachment(2_048);
        let normalized = normalize(&raw);
        let attachment = normalized.envelope.attachments[0].clone();
        let adapter = build(valid()).ok().expect("valid mailbox");
        // A later poll replaced the cache, exactly as `poll` does.
        *adapter.parts.lock().await = HashMap::new();

        let error = adapter
            .fetch_attachment(&attachment, AttachmentLimits::default())
            .await
            .expect_err("no bytes are cached");
        assert!(error.contains("not offered again"), "{error}");
        assert!(!error.contains("next poll"), "{error}");

        // And the shared hydration path records that refusal on the event
        // rather than inventing an empty artifact.
        let blobs = MemoryBlobs(std::sync::Mutex::new(Vec::new()));
        let mut envelopes = vec![normalized.envelope];
        hydrate_attachments(
            &adapter,
            &blobs,
            AttachmentLimits::default(),
            &mut envelopes,
        )
        .await;
        let stored = &envelopes[0].attachments[0];
        assert!(stored.stored_artifact_id.is_none());
        assert!(stored
            .fetch_error
            .as_deref()
            .is_some_and(|error| error.contains("not offered again")));
    }

    #[tokio::test]
    async fn an_attachment_over_the_accounts_cap_arrives_as_a_note_and_is_never_fetched() {
        let raw = with_attachment(4_096);
        let limits = AttachmentLimits {
            max_bytes: 1_024,
            ..AttachmentLimits::default()
        };
        let normalized = normalize_message(
            raw.as_bytes(),
            42,
            7,
            1_700_000_000_000,
            &identity(),
            limits,
        )
        .expect("envelope");
        // Nothing was cached for it: the shared path refuses an over-cap
        // declaration before this adapter is ever asked.
        assert!(normalized.parts.is_empty());

        let adapter = build(valid()).ok().expect("valid mailbox");
        let blobs = MemoryBlobs(std::sync::Mutex::new(Vec::new()));
        let mut envelopes = vec![normalized.envelope];
        hydrate_attachments(&adapter, &blobs, limits, &mut envelopes).await;
        let refused = &envelopes[0].attachments[0];
        assert!(refused.stored_artifact_id.is_none());
        assert!(
            refused
                .fetch_error
                .as_deref()
                .is_some_and(|note| note.contains("over this account's")),
            "{:?}",
            refused.fetch_error
        );
    }

    // ---- outbound headers ----

    fn seeded_refs() -> Arc<MemoryConversationReferences> {
        let refs = Arc::new(MemoryConversationReferences::default());
        refs.put(
            ACCOUNT,
            "ada@example.com",
            &serde_json::json!({
                "subject": "Re: Engine notes",
                "message_id": "m2@example.com",
                "references": ["m0@example.com", "m1@example.com", "m2@example.com"],
            }),
        )
        .expect("seed");
        refs
    }

    fn rendered(files: &[LoadedAttachment], reply_to: Option<&str>) -> String {
        let refs = seeded_refs();
        let reference = refs.get(ACCOUNT, "ada@example.com");
        let bytes = build_mime(
            "here you go",
            OURS,
            "ada@example.com",
            reference.as_ref(),
            reply_to,
            files,
            "abc.little-monkey@example.org",
            1_700_000_000,
        )
        .expect("a reply");
        String::from_utf8(bytes).expect("mail is ascii-safe")
    }

    #[test]
    fn the_reply_carries_in_reply_to_and_references_and_one_re_prefix() {
        let mail = rendered(&[], None);
        assert!(mail.contains("Subject: Re: Engine notes"), "{mail}");
        assert!(!mail.contains("Re: Re:"), "{mail}");
        assert!(mail.contains("In-Reply-To: <m2@example.com>"), "{mail}");
        assert!(
            mail.contains("References: <m0@example.com> <m1@example.com> <m2@example.com>"),
            "{mail}"
        );
        assert!(
            mail.contains("Message-ID: <abc.little-monkey@example.org>"),
            "{mail}"
        );
        assert!(mail.contains("To: <ada@example.com>"), "{mail}");
    }

    #[test]
    fn an_explicit_reply_target_wins_over_the_stored_one() {
        let mail = rendered(&[], Some("<m9@example.com>"));
        assert!(mail.contains("In-Reply-To: <m9@example.com>"), "{mail}");
        assert!(mail.contains("<m9@example.com>"), "{mail}");
    }

    #[test]
    fn a_reply_with_no_stored_thread_still_has_a_subject() {
        let mail = String::from_utf8(
            build_mime(
                "hello",
                OURS,
                "ada@example.com",
                None,
                None,
                &[],
                "x@example.org",
                1_700_000_000,
            )
            .expect("a reply"),
        )
        .expect("ascii");
        assert!(
            mail.contains("Subject: Message from Little Monkey"),
            "{mail}"
        );
        assert!(!mail.contains("In-Reply-To"), "{mail}");
    }

    #[test]
    fn an_outbound_attachment_becomes_a_real_mime_part() {
        let files = vec![LoadedAttachment {
            filename: "notes.txt".into(),
            mime_type: "text/plain".into(),
            bytes: b"hello file".to_vec(),
        }];
        let mail = rendered(&files, None);
        assert!(mail.contains("multipart/mixed"), "{mail}");
        assert!(mail.contains("notes.txt"), "{mail}");
    }

    #[test]
    fn an_address_that_could_inject_a_header_or_a_second_recipient_is_refused() {
        for hostile in [
            "ada@example.com\r\nBcc: evil@example.net",
            "ada@example.com, evil@example.net",
            "ada@example.com\nX: y",
            "<ada@example.com>",
            "ada example.com",
            "ada@localhost",
            "",
            "@example.com",
        ] {
            assert!(!is_addr_spec(hostile), "{hostile:?} should be refused");
            assert!(
                build_mime(
                    "x",
                    OURS,
                    hostile,
                    None,
                    None,
                    &[],
                    "id@example.org",
                    1_700_000_000
                )
                .is_err(),
                "{hostile:?} should not have built a message"
            );
        }
        assert!(is_addr_spec("ada@example.com"));
        assert!(is_addr_spec("ada+tag@mail.example.co.uk"));
    }

    #[test]
    fn a_message_id_is_deterministic_across_retries_of_one_outbox_row() {
        let first = mint_message_id(ACCOUNT, "outbox-1", OURS);
        assert_eq!(first, mint_message_id(ACCOUNT, "outbox-1", OURS));
        assert_ne!(first, mint_message_id(ACCOUNT, "outbox-2", OURS));
        assert!(first.ends_with(".little-monkey@example.org"), "{first}");
        // A digest, never a UUID: a random id would be a fresh Message-ID on
        // every retry of one outbox row, and the echo ledger keys on it.
        let digest = first.split('.').next().expect("a digest");
        assert!(
            digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()),
            "{first}"
        );
    }

    #[test]
    fn dot_stuffing_cannot_end_the_message_early() {
        let stuffed = dot_stuffed(b"line one\r\n.\r\nline two\r\n");
        assert_eq!(stuffed, b"line one\r\n..\r\nline two".to_vec());
    }

    // ---- the SMTP dialogue, over an in-memory pipe ----

    fn envelope<'a>(message: &'a [u8]) -> SmtpEnvelope<'a> {
        SmtpEnvelope {
            ehlo_domain: "example.org",
            username: OURS,
            password: "hunter2-smtp",
            from: OURS,
            to: "ada@example.com",
            message,
        }
    }

    /// A scripted relay: it answers with `replies` in order and records every
    /// line the client sent.
    fn scripted(
        replies: Vec<&'static str>,
        hang_up_after_data: bool,
    ) -> (
        tokio::io::DuplexStream,
        tokio::task::JoinHandle<Vec<String>>,
    ) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let handle = tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(server);
            let mut reader = BufReader::new(reader);
            let mut seen: Vec<String> = Vec::new();
            let mut in_data = false;
            let mut replies = replies.into_iter();
            // The greeting is unprompted.
            if let Some(greeting) = replies.next() {
                let _ = writer.write_all(greeting.as_bytes()).await;
            }
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
                if in_data {
                    seen.push(trimmed.clone());
                    if trimmed == "." {
                        in_data = false;
                        if hang_up_after_data {
                            break;
                        }
                        match replies.next() {
                            Some(reply) => {
                                let _ = writer.write_all(reply.as_bytes()).await;
                            }
                            None => break,
                        }
                    }
                    continue;
                }
                seen.push(trimmed.clone());
                if trimmed.eq_ignore_ascii_case("QUIT") {
                    break;
                }
                match replies.next() {
                    Some(reply) => {
                        let _ = writer.write_all(reply.as_bytes()).await;
                        if trimmed.eq_ignore_ascii_case("DATA") && reply.starts_with('3') {
                            in_data = true;
                        }
                    }
                    None => break,
                }
            }
            seen
        });
        (client, handle)
    }

    const HAPPY: &[&str] = &[
        "220 smtp.example.org ESMTP\r\n",
        "250-smtp.example.org\r\n250-SIZE 35882577\r\n250 AUTH LOGIN PLAIN\r\n",
        "235 2.7.0 Accepted\r\n",
        "250 2.1.0 OK\r\n",
        "250 2.1.5 OK\r\n",
        "354 Go ahead\r\n",
        "250 2.0.0 OK: queued as ABC\r\n",
    ];

    #[tokio::test]
    async fn the_smtp_dialogue_authenticates_and_delivers() {
        let (client, server) = scripted(HAPPY.to_vec(), false);
        let body = b"Subject: hi\r\n\r\n.hidden\r\n";
        let outcome = smtp_exchange(client, &envelope(body)).await;
        assert!(outcome.is_ok(), "{outcome:?}");
        let seen = server.await.expect("server task");
        assert_eq!(seen[0], "EHLO example.org");
        // PLAIN is preferred when both are offered, and the credential is the
        // RFC 4616 \0user\0pass triple.
        let expected = BASE64.encode(format!("\0{OURS}\0hunter2-smtp").as_bytes());
        assert_eq!(seen[1], format!("AUTH PLAIN {expected}"));
        assert_eq!(seen[2], format!("MAIL FROM:<{OURS}>"));
        assert_eq!(seen[3], "RCPT TO:<ada@example.com>");
        assert_eq!(seen[4], "DATA");
        assert!(seen.contains(&"..hidden".to_string()), "{seen:?}");
        assert!(seen.contains(&".".to_string()), "{seen:?}");
    }

    #[tokio::test]
    async fn auth_login_is_used_when_plain_is_not_offered() {
        let mut replies = HAPPY.to_vec();
        replies[1] = "250-smtp.example.org\r\n250 AUTH LOGIN\r\n";
        replies.insert(3, "334 UGFzc3dvcmQ6\r\n");
        replies.insert(2, "334 VXNlcm5hbWU6\r\n");
        let (client, server) = scripted(replies, false);
        let outcome = smtp_exchange(client, &envelope(b"Subject: hi\r\n\r\nbody\r\n")).await;
        assert!(outcome.is_ok(), "{outcome:?}");
        let seen = server.await.expect("server task");
        assert_eq!(seen[1], "AUTH LOGIN");
        assert_eq!(seen[2], BASE64.encode(OURS.as_bytes()));
        assert_eq!(seen[3], BASE64.encode(b"hunter2-smtp"));
    }

    #[tokio::test]
    async fn a_server_offering_no_supported_mechanism_is_refused_before_the_password_moves() {
        let mut replies = HAPPY.to_vec();
        replies[1] = "250-smtp.example.org\r\n250 AUTH GSSAPI\r\n";
        let (client, server) = scripted(replies, false);
        let outcome = smtp_exchange(client, &envelope(b"x")).await;
        assert!(
            matches!(outcome, Err(SendOutcome::PermanentFailure { .. })),
            "{outcome:?}"
        );
        let seen = server.await.expect("server task");
        assert_eq!(seen, vec!["EHLO example.org".to_string()]);
    }

    #[tokio::test]
    async fn a_four_hundred_before_data_is_retryable_and_a_five_hundred_is_permanent() {
        let mut replies = HAPPY.to_vec();
        replies[4] = "451 4.3.0 Try later\r\n";
        let (client, _server) = scripted(replies, false);
        assert!(matches!(
            smtp_exchange(client, &envelope(b"x")).await,
            Err(SendOutcome::RetryableFailure { .. })
        ));

        let mut replies = HAPPY.to_vec();
        replies[4] = "550 5.1.1 No such user\r\n";
        let (client, _server) = scripted(replies, false);
        assert!(matches!(
            smtp_exchange(client, &envelope(b"x")).await,
            Err(SendOutcome::PermanentFailure { .. })
        ));
    }

    #[tokio::test]
    async fn a_failure_after_the_terminating_dot_is_not_retried() {
        // The relay may already have queued the message and there is no id to
        // reconcile against, so retrying could deliver it twice.
        let (client, _server) = scripted(HAPPY.to_vec(), true);
        let outcome = smtp_exchange(client, &envelope(b"Subject: hi\r\n\r\nbody\r\n")).await;
        assert!(
            matches!(outcome, Err(SendOutcome::NeedsReconciliation { .. })),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn no_password_appears_in_any_rendered_smtp_error() {
        let mut replies = HAPPY.to_vec();
        // A hostile relay that echoes the credential back in its refusal.
        replies[2] = "535 5.7.8 rejected AUTH PLAIN AGh1bnRlcjItc210cA== hunter2-smtp\r\n";
        let (client, _server) = scripted(replies, false);
        let outcome = smtp_exchange(client, &envelope(b"x")).await;
        let rendered = format!("{outcome:?}");
        assert!(!rendered.contains("hunter2-smtp"), "{rendered}");
        assert!(rendered.contains("refused authentication"), "{rendered}");
    }

    // ---- cursor ----

    #[test]
    fn a_uidvalidity_change_restarts_the_cursor_without_replaying() {
        assert_eq!(parse_cursor(Some("7:42")), Some((7, 42)));
        assert_eq!(parse_cursor(Some("nonsense")), None);
        assert_eq!(parse_cursor(None), None);
        assert_eq!(format_cursor(7, 42), "7:42");
        // The mailbox was renumbered: the stored uid names a different message
        // now, so it is discarded rather than resumed from.
        assert!(parse_cursor(Some("6:42"))
            .filter(|(validity, _)| *validity == 7)
            .is_none());
    }
}
