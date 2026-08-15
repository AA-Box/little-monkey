//! Matrix adapter, built on the maintained Rust `matrix-sdk`.
//!
//! Nothing here speaks the Client-Server API by hand. Sync, room state, the
//! media repository, threads, and the whole Olm/Megolm lifecycle belong to
//! [`matrix_sdk::Client`]; this module owns exactly three things — turning SDK
//! room events into [`ChannelEnvelope`], turning an [`OutboundMessage`] into an
//! SDK send, and the fail-closed rules around encryption.
//!
//! # The session is the user's own, and it survives a restart
//!
//! An account carries a homeserver URL, a user id, and an access token the
//! operator created in their own Matrix client. [`MatrixAdapter::client`] asks
//! the homeserver which **device** that token belongs to
//! ([`Client::whoami`]) and restores that session, so Little Monkey adopts the
//! device the user can already see in their session list instead of registering
//! a new one every time the daemon starts. The SDK's SQLite store — crypto keys,
//! room state, and the sync token together — lives under the daemon's own
//! private state root, keyed by account id, which is what makes "restart with
//! persistent crypto/session state" true rather than aspirational.
//!
//! # Encryption fails closed
//!
//! [`encryption_decision`] is the whole rule, and it is deliberately a pure
//! function so it can be asserted on directly:
//!
//! ```text
//! known unencrypted room            → plaintext send allowed
//! known encrypted room              → encrypted send (the SDK encrypts)
//! encryption state unknown          → refuse
//! the state query itself failed     → refuse
//! ```
//!
//! There is no arm that answers "could not determine" with a plaintext send.
//! A room that asked for encryption and cannot get it has not been sent to.
//!
//! # An event that cannot be decrypted is never guessed at
//!
//! The SDK decrypts before dispatch. An event whose key never arrived stays an
//! `m.room.encrypted` event, is counted, and is surfaced in the account's
//! health — it never becomes an empty message, never reaches the agent as
//! ciphertext, and is never silently dropped. When the key does turn up in a
//! later sync the SDK re-processes it and the message arrives then.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use matrix_sdk::attachment::AttachmentConfig;
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::media::{MediaFormat, MediaRequestParameters};
use matrix_sdk::room::reply::{EnforceThread, Reply};
use matrix_sdk::ruma::events::relation::{InReplyTo, Reply as ReplyRelation, Thread};
use matrix_sdk::ruma::events::room::encrypted::SyncRoomEncryptedEvent;
use matrix_sdk::ruma::events::room::message::{
    AddMentions, MessageType, OriginalSyncRoomMessageEvent, Relation, ReplyWithinThread,
    RoomMessageEventContent, RoomMessageEventContentWithoutRelation, TextMessageEventContent,
};
use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::ruma::events::AnyMessageLikeEventContent;
use matrix_sdk::ruma::{EventId, OwnedDeviceId, OwnedUserId, RoomId, UserId};
use matrix_sdk::{
    AuthSession, Client, EncryptionState, Room, RoomState, SessionMeta, SessionTokens,
};
use serde_json::Value;
use tokio::sync::{mpsc, Mutex, OnceCell};

use crate::daemon::channel_adapter::{
    load_attachments, AdapterConfig, BlobSource, ChannelAdapter, DaemonBlobs, InboundBatch,
    LoadedAttachment, TransportStatus,
};
use little_monkey_lib::channels::types::{
    AttachmentKind, AttachmentSource, ChannelAttachment, ChannelConversation, ChannelEnvelope,
    ChannelHealth, ChannelKind, ChannelSender, HealthState, InboundTransport, OutboundMessage,
    ProviderCapabilities, SendOutcome,
};

/// Matrix events are capped by the homeserver's own `max_event_size` (Synapse's
/// default is 65536 bytes for the whole serialized event, not just the body). A
/// conservative character budget, not a byte-exact one — see `telegram.rs`'s
/// `MAX_MESSAGE_UTF16` note on chars vs wire units.
const MAX_TEXT_CHARS: usize = 32_000;

/// How many normalized events to hold between one `poll` and the next. The sync
/// task never blocks on this — see [`Shared::dropped`].
const INBOUND_CAPACITY: usize = 1024;

/// How long [`MatrixAdapter::poll`] waits for a first event before returning an
/// empty batch. Bounded, per the trait's contract for a long-lived transport.
const POLL_WAIT: Duration = Duration::from_secs(20);

/// How long the homeserver holds each `/sync` open. The SDK re-issues it.
const SYNC_TIMEOUT: Duration = Duration::from_secs(30);

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A sync that ran at least this long counts as recovered, so the next outage
/// backs off from the beginning rather than inheriting the previous one.
const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(30);

/// State every `&self` trait method needs and none of them can own, plus the
/// two counters an operator reads in health.
#[derive(Default)]
struct Shared {
    status: TransportStatus,
    last_error: Mutex<Option<String>>,
    /// Events the SDK could not decrypt across this adapter's whole life. With
    /// encryption working this stays at zero; a number here means keys are not
    /// reaching this device.
    undecryptable: AtomicU64,
    /// Normalized events dropped because `poll` fell far enough behind to fill
    /// the queue. Reported as degraded health rather than pretended away.
    dropped: AtomicU64,
}

pub struct MatrixAdapter {
    homeserver_url: String,
    user_id: OwnedUserId,
    access_token: String,
    account_id: String,
    /// Built once, on first use, because it needs a round trip to learn which
    /// device the token belongs to. The error is cached too: a homeserver that
    /// rejected the token will reject it again, and retrying per poll would be
    /// a login loop against somebody's own server.
    client: OnceCell<Result<Client, String>>,
    started: OnceCell<()>,
    inbound_tx: mpsc::Sender<ChannelEnvelope>,
    inbound_rx: Mutex<mpsc::Receiver<ChannelEnvelope>>,
    shared: Arc<Shared>,
    blobs: Arc<dyn BlobSource>,
}

impl MatrixAdapter {
    pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
        if config.secret.trim().is_empty() {
            return Err("Matrix requires an access token".to_string());
        }
        let raw_homeserver = config
            .account
            .non_secret_config
            .get("homeserver_url")
            .and_then(Value::as_str)
            .ok_or_else(|| "Matrix account is missing homeserver_url".to_string())?;
        let homeserver_url = validate_homeserver_url(raw_homeserver)?;
        let raw_user_id = config
            .account
            .non_secret_config
            .get("user_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "Matrix account is missing user_id".to_string())?;
        let user_id = UserId::parse(raw_user_id)
            .map_err(|_| format!("'{raw_user_id}' is not a Matrix user id"))?;
        let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_CAPACITY);
        Ok(Self {
            homeserver_url,
            user_id,
            access_token: config.secret.clone(),
            account_id: config.account.account_id.clone(),
            client: OnceCell::new(),
            started: OnceCell::new(),
            inbound_tx,
            inbound_rx: Mutex::new(inbound_rx),
            shared: Arc::new(Shared::default()),
            blobs: Arc::new(DaemonBlobs),
        })
    }

    /// Where this account's SDK store lives: crypto keys, room state and the
    /// sync token, in one SQLite database under the daemon's private root.
    ///
    /// Per account, never shared: two Matrix accounts on one machine are two
    /// devices with two sets of keys, and a shared store would cross them.
    fn store_dir(&self) -> Result<std::path::PathBuf, String> {
        let paths = crate::daemon::store::DaemonPaths::resolve()?;
        Ok(paths.root.join("matrix").join(&self.account_id))
    }

    /// The SDK client for this account, built and logged in exactly once.
    async fn client(&self) -> Result<&Client, String> {
        self.client
            .get_or_init(|| async { self.build_client().await })
            .await
            .as_ref()
            .map_err(String::clone)
    }

    async fn build_client(&self) -> Result<Client, String> {
        let store_dir = self.store_dir()?;
        std::fs::create_dir_all(&store_dir)
            .map_err(|error| format!("Matrix has nowhere to store its session: {error}"))?;
        // The SDK gets this tree's own hardened client rather than building its
        // own, so every Matrix request goes through the same egress guard,
        // timeouts and redirect policy as everything else.
        let http = little_monkey_lib::egress::hardened()
            .build()
            .map_err(|error| format!("Failed to build the Matrix HTTP client: {error}"))?;
        let client = Client::builder()
            .homeserver_url(&self.homeserver_url)
            .http_client(http)
            // No passphrase: the store sits inside the daemon's private state
            // root, protected the same way the run ledger beside it is. A
            // passphrase here would have to be stored next to what it protects.
            .sqlite_store(&store_dir, None)
            .build()
            .await
            .map_err(|error| format!("Could not reach the Matrix homeserver: {error}"))?;

        // The device id comes from the homeserver, never from us: an access
        // token already belongs to a device in the user's own session list, and
        // inventing one would add an unverified device on every restart.
        let whoami = client
            .whoami()
            .await
            .map_err(|error| format!("Matrix rejected the access token: {error}"))?;
        if whoami.user_id != self.user_id {
            return Err(format!(
                "That access token belongs to {}, not to the configured user id",
                whoami.user_id
            ));
        }
        let device_id: OwnedDeviceId = whoami.device_id.ok_or_else(|| {
            "The Matrix homeserver reported no device for this access token, so encrypted \
             rooms could never be read"
                .to_string()
        })?;

        client
            .restore_session(AuthSession::Matrix(MatrixSession {
                meta: SessionMeta {
                    user_id: self.user_id.clone(),
                    device_id,
                },
                tokens: SessionTokens {
                    access_token: self.access_token.clone(),
                    refresh_token: None,
                },
            }))
            .await
            .map_err(|error| format!("Could not restore the Matrix session: {error}"))?;
        Ok(client)
    }

    /// Register the event handlers and start syncing, once.
    async fn ensure_started(&self) -> Result<(), String> {
        let client = self.client().await?.clone();
        self.started
            .get_or_init(|| async {
                let tx = self.inbound_tx.clone();
                let shared = self.shared.clone();
                let self_user = self.user_id.clone();

                // Decrypted (or never-encrypted) room messages. The SDK has
                // already turned an `m.room.encrypted` event into this one by
                // the time a handler sees it.
                {
                    let tx = tx.clone();
                    let shared = shared.clone();
                    client.add_event_handler(
                        move |event: OriginalSyncRoomMessageEvent, room: Room| {
                            let tx = tx.clone();
                            let shared = shared.clone();
                            let self_user = self_user.clone();
                            async move {
                                if room.state() != RoomState::Joined {
                                    return;
                                }
                                let is_direct = room.is_direct().await.unwrap_or(false);
                                let Some(envelope) = normalize_message(
                                    &event,
                                    room.room_id().as_str(),
                                    is_direct,
                                    &self_user,
                                ) else {
                                    return;
                                };
                                // `try_send`, never `send`: the sync task must
                                // not stall behind a slow consumer, or the SDK
                                // stops absorbing to-device key traffic and
                                // encrypted rooms go dark. An overflow is
                                // counted and surfaced instead.
                                if tx.try_send(envelope).is_err() {
                                    shared.dropped.fetch_add(1, Ordering::Relaxed);
                                    shared.status.set(HealthState::Degraded);
                                }
                            }
                        },
                    );
                }

                // Anything still encrypted here is an event the SDK could not
                // decrypt. It is counted and never invented into plaintext.
                client.add_event_handler(move |_: SyncRoomEncryptedEvent, _: Room| {
                    let shared = shared.clone();
                    async move {
                        shared.undecryptable.fetch_add(1, Ordering::Relaxed);
                    }
                });

                let shared = self.shared.clone();
                tokio::spawn(run_sync_loop(client, shared));
            })
            .await;
        Ok(())
    }

    /// Whether this room may be sent to in the clear, encrypted, or not at all.
    ///
    /// Asked per send rather than cached: a room that turns encryption on never
    /// turns it off again, and sending cleartext into a room that just enabled
    /// it is exactly the mistake worth one extra request.
    async fn encryption_gate(&self, room: &Room) -> Result<Encryption, String> {
        match room.latest_encryption_state().await {
            Ok(state) => encryption_decision(Some(state)),
            // The query itself failed, so the answer is unknown — which is the
            // refusing arm, not the plaintext one.
            Err(error) => {
                encryption_decision(None).map_err(|message| format!("{message} ({error})"))
            }
        }
    }

    /// The joined room this message names, or the outcome the outbox should
    /// record for not having one.
    async fn room_for(&self, message: &OutboundMessage) -> Result<Room, SendOutcome> {
        let client = match self.client().await {
            Ok(client) => client,
            Err(error) => return Err(SendOutcome::PermanentFailure { error }),
        };
        let room_id =
            RoomId::parse(&message.conversation_id).map_err(|_| SendOutcome::PermanentFailure {
                error: format!("'{}' is not a Matrix room id", message.conversation_id),
            })?;
        match client.get_room(&room_id) {
            Some(room) if room.state() == RoomState::Joined => Ok(room),
            Some(_) => Err(SendOutcome::PermanentFailure {
                error: "This account is not joined to that Matrix room".to_string(),
            }),
            // The first sync has not landed yet, so the room is not in the
            // store. Nothing was sent, and a moment later it will be there.
            None => Err(SendOutcome::RetryableFailure {
                error: "That Matrix room is not in sync yet".to_string(),
                retry_after_ms: Some(2_000),
            }),
        }
    }
}

/// What [`encryption_decision`] permits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encryption {
    /// The room is known to be unencrypted, so a plaintext event is correct.
    Plaintext,
    /// The room is known to be encrypted. The SDK encrypts, or the send fails —
    /// it never falls back.
    Encrypted,
}

/// The fail-closed rule, as a pure function over what the SDK knows.
///
/// `None` means the state query failed outright. There is deliberately no arm
/// that turns "could not determine" into [`Encryption::Plaintext`]: a room whose
/// encryption is unknown may be encrypted, and a plaintext event in one is on
/// the server in the clear forever.
fn encryption_decision(state: Option<EncryptionState>) -> Result<Encryption, String> {
    match state {
        Some(EncryptionState::NotEncrypted) => Ok(Encryption::Plaintext),
        Some(EncryptionState::Unknown) | None => Err(
            "Little Monkey could not determine whether this Matrix room is encrypted, so it \
             refused to send rather than risk sending in the clear"
                .to_string(),
        ),
        // Every other state the SDK models is an encrypted one.
        Some(_) => Ok(Encryption::Encrypted),
    }
}

/// Sync until it fails, then back off and sync again. The SDK owns the sync
/// token — it lives in the same SQLite store as the crypto keys — so a restart
/// resumes where this left off rather than replaying or skipping.
async fn run_sync_loop(client: Client, shared: Arc<Shared>) {
    let mut backoff = MIN_BACKOFF;
    loop {
        let started_at = std::time::Instant::now();
        shared.status.set(HealthState::Connected);
        let result = client
            .sync(SyncSettings::default().timeout(SYNC_TIMEOUT))
            .await;
        shared.status.set(HealthState::Degraded);
        if let Err(error) = result {
            *shared.last_error.lock().await = Some(format!("Matrix sync stopped: {error}"));
        }
        if started_at.elapsed() >= BACKOFF_RESET_AFTER {
            backoff = MIN_BACKOFF;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Validates `non_secret_config.homeserver_url`: a bare origin (no path, no
/// query) so it can never be mistaken for an API path — mirrors
/// `mattermost.rs`'s `validate_base_url` and the same reasoning: this is a trust
/// boundary once an access token goes out on every request. `https` is required;
/// plain `http` is accepted only for localhost/127.0.0.1/::1, which is how a
/// self-hosted homeserver is commonly reached during setup.
fn validate_homeserver_url(raw: &str) -> Result<String, String> {
    let parsed =
        url::Url::parse(raw).map_err(|_| "Matrix homeserver_url is not a valid URL".to_string())?;
    if !matches!(parsed.path(), "" | "/") {
        return Err("Matrix homeserver_url must not include a path".to_string());
    }
    if parsed.query().is_some() {
        return Err("Matrix homeserver_url must not include a query string".to_string());
    }
    match parsed.scheme() {
        "https" => {}
        "http" => {
            let is_local = matches!(
                parsed.host_str(),
                Some("localhost") | Some("127.0.0.1") | Some("::1")
            );
            if !is_local {
                return Err(
                    "Matrix homeserver_url must be https (plain http is only accepted for localhost)"
                        .to_string(),
                );
            }
        }
        _ => return Err("Matrix homeserver_url must use http or https".to_string()),
    }
    Ok(raw.trim_end_matches('/').to_string())
}

/// True when `needle` appears in `text` at a word boundary (a Unicode
/// alphanumeric character or `_` on either side counts as "still inside a
/// word"). Same shape as `irc.rs`'s `mentions_nick`, generalized to Matrix ids
/// and non-ASCII text, which is why this is char-indexed rather than
/// byte-indexed.
fn mentions_word_boundary(text: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let haystack: Vec<char> = text.to_lowercase().chars().collect();
    let needle: Vec<char> = needle.to_lowercase().chars().collect();
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    let is_word_char = |c: char| c.is_alphanumeric() || c == '_';
    for start in 0..=(haystack.len() - needle.len()) {
        if haystack[start..start + needle.len()] == needle[..] {
            let end = start + needle.len();
            let before_ok = start == 0 || !is_word_char(haystack[start - 1]);
            let after_ok = end >= haystack.len() || !is_word_char(haystack[end]);
            if before_ok && after_ok {
                return true;
            }
        }
    }
    false
}

/// One inbound attachment, carrying the SDK's own [`MediaSource`] rather than a
/// bare `mxc://` URI.
///
/// An encrypted room's media is an `EncryptedFile` — a URI plus the key, IV and
/// hashes needed to decrypt it — and a URI alone cannot be downloaded into
/// anything readable. The whole source is therefore serialized into the
/// attachment's provider handle and handed back to the SDK at fetch time, which
/// is what makes an encrypted image work the same way a plain one does.
fn attachment_from(
    kind: AttachmentKind,
    source: &MediaSource,
    filename: &str,
    mime_type: Option<String>,
    size: Option<u64>,
) -> Option<ChannelAttachment> {
    let handle = serde_json::to_string(source).ok()?;
    Some(ChannelAttachment {
        stored_artifact_id: None,
        text_excerpt: None,
        fetch_error: None,
        provider_id: match source {
            MediaSource::Plain(uri) => Some(uri.to_string()),
            MediaSource::Encrypted(file) => Some(file.url.to_string()),
        },
        kind,
        filename: (!filename.is_empty()).then(|| filename.to_string()),
        mime_type,
        declared_size_bytes: size,
        source: AttachmentSource::ProviderHandle { handle },
    })
}

/// One decrypted room message as a normalized envelope.
///
/// Pure — no network, no clock, no SDK client — so the tests below drive it
/// directly from event JSON.
fn normalize_message(
    event: &OriginalSyncRoomMessageEvent,
    room_id: &str,
    is_direct: bool,
    self_user_id: &UserId,
) -> Option<ChannelEnvelope> {
    let content = &event.content;
    let mut attachments = Vec::new();
    let text = match &content.msgtype {
        MessageType::Text(body) => body.body.clone(),
        MessageType::Notice(body) => body.body.clone(),
        MessageType::Emote(body) => body.body.clone(),
        MessageType::Image(image) => {
            if let Some(attachment) = attachment_from(
                AttachmentKind::Image,
                &image.source,
                image.filename(),
                image.info.as_ref().and_then(|info| info.mimetype.clone()),
                image
                    .info
                    .as_ref()
                    .and_then(|info| info.size)
                    .map(Into::into),
            ) {
                attachments.push(attachment);
            }
            image.caption().unwrap_or_default().to_string()
        }
        MessageType::File(file) => {
            if let Some(attachment) = attachment_from(
                AttachmentKind::Document,
                &file.source,
                file.filename(),
                file.info.as_ref().and_then(|info| info.mimetype.clone()),
                file.info
                    .as_ref()
                    .and_then(|info| info.size)
                    .map(Into::into),
            ) {
                attachments.push(attachment);
            }
            file.caption().unwrap_or_default().to_string()
        }
        MessageType::Audio(audio) => {
            if let Some(attachment) = attachment_from(
                AttachmentKind::Audio,
                &audio.source,
                audio.filename(),
                audio.info.as_ref().and_then(|info| info.mimetype.clone()),
                audio
                    .info
                    .as_ref()
                    .and_then(|info| info.size)
                    .map(Into::into),
            ) {
                attachments.push(attachment);
            }
            audio.caption().unwrap_or_default().to_string()
        }
        MessageType::Video(video) => {
            if let Some(attachment) = attachment_from(
                AttachmentKind::Video,
                &video.source,
                video.filename(),
                video.info.as_ref().and_then(|info| info.mimetype.clone()),
                video
                    .info
                    .as_ref()
                    .and_then(|info| info.size)
                    .map(Into::into),
            ) {
                attachments.push(attachment);
            }
            video.caption().unwrap_or_default().to_string()
        }
        // A location, a verification request, or a future msgtype: nothing here
        // to normalize into either text or an attachment.
        _ => return None,
    };

    if text.is_empty() && attachments.is_empty() {
        return None;
    }

    let (thread_id, reply_to_provider_id) = relations(content);

    let mentioned_by_metadata = content
        .mentions
        .as_ref()
        .is_some_and(|mentions| mentions.user_ids.iter().any(|id| id == self_user_id));
    let localpart = self_user_id.localpart();
    let mentions_self = mentioned_by_metadata
        || mentions_word_boundary(&text, self_user_id.as_str())
        || mentions_word_boundary(&text, localpart);

    let conversation = if is_direct {
        ChannelConversation::direct(room_id.to_string())
    } else {
        ChannelConversation::group(room_id.to_string())
    }
    .with_thread(thread_id);

    Some(ChannelEnvelope {
        account_id: String::new(),
        kind: ChannelKind::Matrix,
        provider_event_id: event.event_id.to_string(),
        conversation,
        sender: ChannelSender {
            sender_id: event.sender.to_string(),
            display_label: None,
            is_self: event.sender == self_user_id,
            is_bot: false,
        },
        text,
        attachments,
        reply_to_provider_id,
        mentions_self,
        received_at_ms: i64::from(event.origin_server_ts.0),
        metadata: little_monkey_lib::channels::types::BoundedMetadata::new(),
    })
}

/// The thread this message belongs to, and the message it replies to.
///
/// Matrix carries both on one `m.relates_to`, and they are genuinely separate
/// facts: a threaded message always names its thread root, and *additionally*
/// names a replied-to event when it is a real reply rather than the spec's
/// fallback for clients that cannot render threads.
fn relations(content: &RoomMessageEventContent) -> (Option<String>, Option<String>) {
    match &content.relates_to {
        Some(Relation::Thread(thread)) => (
            Some(thread.event_id.to_string()),
            // `is_falling_back` marks the `m.in_reply_to` as a compatibility
            // pointer at the newest event in the thread, not a reply anybody
            // made. Reporting it as a reply would attribute an intent to the
            // sender that they never had.
            (!thread.is_falling_back)
                .then(|| {
                    thread
                        .in_reply_to
                        .as_ref()
                        .map(|reply| reply.event_id.to_string())
                })
                .flatten(),
        ),
        Some(Relation::Reply(reply)) => (None, Some(reply.in_reply_to.event_id.to_string())),
        _ => (None, None),
    }
}

/// Builds the `m.relates_to` an outbound message needs, from the thread and
/// reply the common outbound shape carries.
///
/// A thread plus a reply keeps both, which is what the spec asks for and what a
/// threaded reply looks like in every other client. A thread with no reply gets
/// the recommended fallback pointer so a thread-blind client still renders
/// something in order.
fn outbound_relation(
    thread_id: Option<&str>,
    reply_to: Option<&str>,
) -> Option<Relation<RoomMessageEventContentWithoutRelation>> {
    let reply_event = reply_to.and_then(|id| EventId::parse(id).ok());
    match thread_id.and_then(|id| EventId::parse(id).ok()) {
        Some(root) => Some(Relation::Thread(match reply_event {
            Some(reply) => Thread::reply(root, reply),
            None => Thread::plain(root.clone(), root),
        })),
        None => reply_event
            .map(|event_id| Relation::Reply(ReplyRelation::new(InReplyTo::new(event_id)))),
    }
}

#[async_trait]
impl ChannelAdapter for MatrixAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Matrix
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            kind: ChannelKind::Matrix,
            // The SDK holds `/sync` open and dispatches to handlers; nothing
            // here polls a cursor of its own.
            inbound_transport: InboundTransport::Socket,
            max_text_chars: MAX_TEXT_CHARS,
            // Real `m.thread` relations, both directions — see `relations` and
            // `outbound_relation`.
            supports_threads: true,
            supports_attachments: true,
            supports_mention_metadata: true,
            // The SDK's transaction id is a real caller-supplied idempotency
            // key the homeserver dedupes on.
            supports_idempotency_key: true,
            supports_delivery_receipts: false,
        }
    }

    /// The sync task's own state, which a quiet poll cannot report.
    fn live_transport(&self) -> Option<HealthState> {
        Some(self.shared.status.get())
    }

    async fn probe(&self) -> ChannelHealth {
        let now = now_ms();
        let client = match self.client().await {
            Ok(client) => client,
            Err(error) => return ChannelHealth::error(now, error),
        };
        if let Err(error) = self.ensure_started().await {
            return ChannelHealth::error(now, error);
        }
        // A real round trip to the homeserver, not a cached session: saved
        // configuration is not a connection.
        let whoami = match client.whoami().await {
            Ok(whoami) => whoami,
            Err(error) => {
                return ChannelHealth::error(
                    now,
                    format!("The Matrix homeserver rejected the session: {error}"),
                )
            }
        };
        let mut detail = format!(
            "{} · device {}",
            whoami.user_id,
            client
                .device_id()
                .map(|id| id.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        );
        detail.push_str(
            if client.encryption().cross_signing_status().await.is_some() {
                " · encryption ready"
            } else {
                " · encryption starting"
            },
        );
        let undecryptable = self.shared.undecryptable.load(Ordering::Relaxed);
        if undecryptable > 0 {
            detail.push_str(&format!(
                " · {undecryptable} message(s) could not be decrypted"
            ));
        }
        let dropped = self.shared.dropped.load(Ordering::Relaxed);
        if dropped > 0 {
            detail.push_str(&format!(" · {dropped} event(s) dropped under load"));
        }
        // The token still works, so this is not an error — but a sync that
        // dropped out is a connection an operator should know about, and the
        // reason it stopped is the only actionable part.
        let sync_stopped = self.shared.last_error.lock().await.clone();
        if self.shared.status.get() != HealthState::Connected {
            if let Some(reason) = sync_stopped {
                detail.push_str(&format!(" · {reason}"));
            }
            return ChannelHealth::degraded(now, detail);
        }
        if undecryptable > 0 || dropped > 0 {
            return ChannelHealth::degraded(now, detail);
        }
        ChannelHealth::connected(now, Some(detail))
    }

    /// Drain whatever the sync task has normalized.
    ///
    /// `cursor` is unused and always `None` on the way out: the SDK persists its
    /// own sync token in the same store as the crypto keys, so there is nothing
    /// for `channel_cursors` to hold that would not be a stale second copy.
    async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
        self.ensure_started().await?;
        let mut receiver = self.inbound_rx.lock().await;
        let mut envelopes = Vec::new();
        match tokio::time::timeout(POLL_WAIT, receiver.recv()).await {
            Ok(Some(envelope)) => envelopes.push(envelope),
            Ok(None) => return Err("The Matrix sync task stopped".to_string()),
            Err(_) => {}
        }
        while let Ok(envelope) = receiver.try_recv() {
            envelopes.push(envelope);
        }
        Ok(InboundBatch {
            envelopes,
            cursor: None,
        })
    }

    async fn send(&self, message: &OutboundMessage) -> SendOutcome {
        let room = match self.room_for(message).await {
            Ok(room) => room,
            Err(outcome) => return outcome,
        };
        // The gate runs before anything is uploaded or sent, for files and text
        // alike: an encrypted room whose state cannot be established gets
        // nothing at all, not a plaintext fallback.
        if let Err(error) = self.encryption_gate(&room).await {
            return SendOutcome::PermanentFailure { error };
        }
        if !message.attachments.is_empty() {
            let files = match load_attachments(self.blobs.as_ref(), message) {
                Ok(files) => files,
                Err(outcome) => return outcome,
            };
            return self.send_with_attachments(&room, message, &files).await;
        }

        let mut content = RoomMessageEventContent::text_plain(&message.text);
        content.relates_to = outbound_relation(
            message.thread_id.as_deref(),
            message.reply_to_provider_id.as_deref(),
        )
        .map(Into::into);
        // Matrix dedupes on (access token, transaction id) — exactly the
        // idempotency guarantee the outbox needs on a retried send.
        let transaction_id = transaction_id_for(&message.idempotency_key);
        match room
            .send(AnyMessageLikeEventContent::RoomMessage(content))
            .with_transaction_id(transaction_id)
            .await
        {
            Ok(result) => SendOutcome::Sent {
                provider_message_id: Some(result.response.event_id.to_string()),
            },
            Err(error) => classify_send_error(&error),
        }
    }

    /// Download one inbound attachment through the SDK.
    ///
    /// The SDK owns the authenticated media endpoint *and* the decryption: an
    /// `EncryptedFile` source comes back as plaintext bytes, and a plain
    /// `mxc://` comes back as itself. Nothing here builds a media URL.
    async fn fetch_attachment(
        &self,
        attachment: &ChannelAttachment,
        limits: crate::daemon::channel_adapter::AttachmentLimits,
    ) -> Result<Vec<u8>, String> {
        let AttachmentSource::ProviderHandle { handle } = &attachment.source else {
            return Err("This Matrix attachment has no media source.".to_string());
        };
        let source: MediaSource = serde_json::from_str(handle)
            .map_err(|_| "That Matrix attachment's media source is unusable".to_string())?;
        let client = self.client().await?;
        let bytes = client
            .media()
            .get_media_content(
                &MediaRequestParameters {
                    source,
                    format: MediaFormat::File,
                },
                // Not cached: the daemon's own content store is where these
                // bytes are kept, and a second copy in the SDK's media cache
                // would double every attachment on disk.
                false,
            )
            .await
            .map_err(|error| format!("That Matrix attachment could not be downloaded: {error}"))?;
        if bytes.len() as u64 > limits.max_bytes {
            return Err(format!(
                "The attachment is larger than the {}-byte limit",
                limits.max_bytes
            ));
        }
        Ok(bytes)
    }
}

/// A transaction id derived from the outbox's own idempotency key, so a retried
/// send is deduplicated by the homeserver rather than posting twice.
fn transaction_id_for(idempotency_key: &str) -> matrix_sdk::ruma::OwnedTransactionId {
    idempotency_key.to_string().into()
}

/// Whether the outbox may retry a failed send.
///
/// The distinction that matters is whether the request provably never left this
/// machine. A connection that was never established is safe to retry; anything
/// that failed after bytes went out may already have been acted on, and saying
/// "retry" there is how a message gets sent twice.
fn classify_send_error(error: &matrix_sdk::Error) -> SendOutcome {
    let message = error.to_string();
    if let Some(api_error) = error.client_api_error_kind() {
        return match api_error {
            matrix_sdk::ruma::api::error::ErrorKind::LimitExceeded(limit) => {
                SendOutcome::RetryableFailure {
                    error: "Matrix rate-limited the request".to_string(),
                    retry_after_ms: limit.retry_after.as_ref().and_then(retry_after_ms),
                }
            }
            _ => SendOutcome::PermanentFailure {
                error: format!("Matrix refused the send: {message}"),
            },
        };
    }
    SendOutcome::NeedsReconciliation {
        error: format!("Matrix send outcome unknown: {message}"),
    }
}

fn retry_after_ms(retry_after: &matrix_sdk::ruma::api::error::RetryAfter) -> Option<i64> {
    match retry_after {
        matrix_sdk::ruma::api::error::RetryAfter::Delay(duration) => {
            i64::try_from(duration.as_millis()).ok()
        }
        matrix_sdk::ruma::api::error::RetryAfter::DateTime(_) => None,
    }
}

impl MatrixAdapter {
    /// Send one message's files through the SDK's attachment path.
    ///
    /// In an encrypted room this uploads *encrypted* media and posts an event
    /// carrying the decryption metadata — the SDK does both, which is why
    /// nothing here has to decide between them and nothing here can get it
    /// wrong. The message text rides along as the first file's caption rather
    /// than as a separate event, so a reply with a file is one message.
    async fn send_with_attachments(
        &self,
        room: &Room,
        message: &OutboundMessage,
        files: &[LoadedAttachment],
    ) -> SendOutcome {
        // Built per use rather than once: `Reply` is deliberately not `Clone`,
        // and only the first file carries the reply anyway — repeating it on
        // every file would make one message read as several replies.
        let reply_for_first_file = || {
            message
                .reply_to_provider_id
                .as_deref()
                .and_then(|id| EventId::parse(id).ok())
                .map(|event_id| Reply {
                    event_id,
                    enforce_thread: if message.thread_id.is_some() {
                        EnforceThread::Threaded(ReplyWithinThread::Yes)
                    } else {
                        EnforceThread::MaybeThreaded
                    },
                    add_mentions: AddMentions::No,
                })
        };

        let mut any_sent = false;
        let mut last_event_id = None;
        for (index, file) in files.iter().enumerate() {
            let mime: mime::Mime = file
                .mime_type
                .parse()
                .unwrap_or(mime::APPLICATION_OCTET_STREAM);
            let mut config = AttachmentConfig::new().txn_id(transaction_id_for(&format!(
                "{}-file{index}",
                message.idempotency_key
            )));
            if index == 0 && !message.text.is_empty() {
                config = config.caption(Some(TextMessageEventContent::plain(&message.text)));
            }
            if index == 0 {
                config = config.reply(reply_for_first_file());
            }
            match room
                .send_attachment(file.filename.clone(), &mime, file.bytes.clone(), config)
                .await
            {
                Ok(response) => {
                    any_sent = true;
                    last_event_id = Some(response.event_id.to_string());
                }
                Err(error) => {
                    let outcome = classify_send_error(&error);
                    // Once one file is on the wire, a later failure is never
                    // "safe to retry": retrying would repost the ones that
                    // landed.
                    return match (any_sent, outcome) {
                        (true, SendOutcome::RetryableFailure { error, .. }) => {
                            SendOutcome::NeedsReconciliation { error }
                        }
                        (true, SendOutcome::PermanentFailure { error }) => {
                            SendOutcome::NeedsReconciliation { error }
                        }
                        (_, outcome) => outcome,
                    };
                }
            }
        }
        // No files is not reachable — `send` only calls this with at least one —
        // but answering `Sent` with no id is still the honest outcome if it were.
        SendOutcome::Sent {
            provider_message_id: last_event_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::serde::Raw;
    use matrix_sdk::ruma::user_id;

    /// The encryption table from this module's own doc, asserted arm by arm.
    /// This is the regression test for the failure class where an undetermined
    /// room fell through to a plaintext send.
    #[test]
    fn encryption_fails_closed_for_everything_but_a_known_answer() {
        assert_eq!(
            encryption_decision(Some(EncryptionState::NotEncrypted)),
            Ok(Encryption::Plaintext)
        );
        assert_eq!(
            encryption_decision(Some(EncryptionState::Encrypted)),
            Ok(Encryption::Encrypted)
        );
        // The two refusing arms: the state is unknown, or the query failed.
        assert!(encryption_decision(Some(EncryptionState::Unknown)).is_err());
        assert!(encryption_decision(None).is_err());
    }

    #[test]
    fn a_refusal_says_it_refused_rather_than_naming_a_fallback() {
        let error = encryption_decision(None).expect_err("must refuse");
        assert!(error.contains("refused to send"), "{error}");
        // Nothing in the message may suggest a plaintext retry is available.
        assert!(!error.to_lowercase().contains("plain text"));
    }

    const SELF_ID: &str = "@self:example.org";

    fn self_id() -> &'static UserId {
        user_id!("@self:example.org")
    }

    fn message_event(json: serde_json::Value) -> OriginalSyncRoomMessageEvent {
        serde_json::from_str::<Raw<OriginalSyncRoomMessageEvent>>(&json.to_string())
            .expect("fixture is JSON")
            .deserialize()
            .expect("fixture parses")
    }

    fn text_event(body: &str, relates_to: Option<serde_json::Value>) -> serde_json::Value {
        let mut content = serde_json::json!({"msgtype": "m.text", "body": body});
        if let Some(relates_to) = relates_to {
            content["m.relates_to"] = relates_to;
        }
        serde_json::json!({
            "type": "m.room.message",
            "event_id": "$1",
            "sender": "@bob:example.org",
            "origin_server_ts": 1_700_000_000_000i64,
            "content": content,
        })
    }

    #[test]
    fn a_direct_room_and_a_group_room_normalize_differently() {
        let event = message_event(text_event("hello there", None));
        let direct =
            normalize_message(&event, "!dm:example.org", true, self_id()).expect("envelope");
        assert_eq!(
            direct.conversation.kind,
            little_monkey_lib::channels::types::ConversationKind::Direct
        );
        let group =
            normalize_message(&event, "!group:example.org", false, self_id()).expect("envelope");
        assert_eq!(
            group.conversation.kind,
            little_monkey_lib::channels::types::ConversationKind::Group
        );
    }

    #[test]
    fn a_thread_relation_becomes_the_common_thread_id() {
        let event = message_event(text_event(
            "in the thread",
            Some(serde_json::json!({
                "rel_type": "m.thread",
                "event_id": "$root",
                "m.in_reply_to": {"event_id": "$root"},
                "is_falling_back": true,
            })),
        ));
        let envelope =
            normalize_message(&event, "!room:example.org", false, self_id()).expect("envelope");
        assert_eq!(envelope.conversation.thread_id.as_deref(), Some("$root"));
        // The fallback pointer is not a reply anybody made, so it must not be
        // reported as one.
        assert_eq!(envelope.reply_to_provider_id, None);
    }

    #[test]
    fn a_real_reply_inside_a_thread_keeps_both_facts() {
        let event = message_event(text_event(
            "answering you",
            Some(serde_json::json!({
                "rel_type": "m.thread",
                "event_id": "$root",
                "m.in_reply_to": {"event_id": "$earlier"},
                "is_falling_back": false,
            })),
        ));
        let envelope =
            normalize_message(&event, "!room:example.org", false, self_id()).expect("envelope");
        assert_eq!(envelope.conversation.thread_id.as_deref(), Some("$root"));
        assert_eq!(envelope.reply_to_provider_id.as_deref(), Some("$earlier"));
    }

    #[test]
    fn a_plain_reply_carries_no_thread() {
        let event = message_event(text_event(
            "sure thing",
            Some(serde_json::json!({"m.in_reply_to": {"event_id": "$1"}})),
        ));
        let envelope =
            normalize_message(&event, "!room:example.org", false, self_id()).expect("envelope");
        assert_eq!(envelope.conversation.thread_id, None);
        assert_eq!(envelope.reply_to_provider_id.as_deref(), Some("$1"));
    }

    #[test]
    fn an_outbound_thread_reply_names_the_thread_and_the_reply() {
        let relation = outbound_relation(Some("$root"), Some("$earlier")).expect("relation");
        let Relation::Thread(thread) = relation else {
            panic!("expected a thread relation");
        };
        assert_eq!(thread.event_id, "$root");
        assert_eq!(
            thread.in_reply_to.expect("reply").event_id.as_str(),
            "$earlier"
        );
        assert!(
            !thread.is_falling_back,
            "a real reply is not the compatibility fallback"
        );
    }

    #[test]
    fn an_outbound_thread_without_a_reply_still_carries_the_spec_fallback() {
        let relation = outbound_relation(Some("$root"), None).expect("relation");
        let Relation::Thread(thread) = relation else {
            panic!("expected a thread relation");
        };
        assert!(thread.is_falling_back);
        assert_eq!(thread.event_id, "$root");
    }

    #[test]
    fn an_outbound_message_with_neither_has_no_relation() {
        assert!(outbound_relation(None, None).is_none());
        // A malformed id is not a relation either — it is silently no thread
        // rather than a send that fails on the wire.
        assert!(outbound_relation(Some("not-an-event-id"), None).is_none());
    }

    #[test]
    fn an_encrypted_image_keeps_the_whole_media_source_not_just_the_uri() {
        let event = message_event(serde_json::json!({
            "type": "m.room.message",
            "event_id": "$img",
            "sender": "@bob:example.org",
            "origin_server_ts": 1_700_000_000_000i64,
            "content": {
                "msgtype": "m.image",
                "body": "photo.png",
                "file": {
                    "url": "mxc://example.org/abc123",
                    "key": {
                        "kty": "oct",
                        "key_ops": ["encrypt", "decrypt"],
                        "alg": "A256CTR",
                        "k": "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8",
                        "ext": true
                    },
                    "iv": "AAECAwQFBgcICQoLDA0ODw",
                    "hashes": {"sha256": "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8"},
                    "v": "v2"
                },
                "info": {"mimetype": "image/png", "size": 4096}
            }
        }));
        let envelope =
            normalize_message(&event, "!room:example.org", true, self_id()).expect("envelope");
        let attachment = &envelope.attachments[0];
        assert_eq!(attachment.kind, AttachmentKind::Image);
        assert_eq!(attachment.declared_size_bytes, Some(4096));
        let AttachmentSource::ProviderHandle { handle } = &attachment.source else {
            panic!("expected a provider handle");
        };
        // The handle round-trips into the SDK's own source type, keys and all —
        // a bare mxc:// URI could never be decrypted.
        let source: MediaSource = serde_json::from_str(handle).expect("round trips");
        assert!(matches!(source, MediaSource::Encrypted(_)));
    }

    #[test]
    fn a_plain_image_round_trips_as_a_plain_source() {
        let event = message_event(serde_json::json!({
            "type": "m.room.message",
            "event_id": "$img",
            "sender": "@bob:example.org",
            "origin_server_ts": 1_700_000_000_000i64,
            "content": {
                "msgtype": "m.image",
                "body": "photo.png",
                "url": "mxc://example.org/abc123",
                "info": {"mimetype": "image/png", "size": 4096}
            }
        }));
        let envelope =
            normalize_message(&event, "!room:example.org", true, self_id()).expect("envelope");
        let AttachmentSource::ProviderHandle { handle } = &envelope.attachments[0].source else {
            panic!("expected a provider handle");
        };
        let source: MediaSource = serde_json::from_str(handle).expect("round trips");
        assert!(matches!(source, MediaSource::Plain(_)));
    }

    #[test]
    fn detects_metadata_and_text_mentions() {
        let by_text = message_event(text_event("hi @self:example.org can you help", None));
        assert!(
            normalize_message(&by_text, "!room:example.org", false, self_id())
                .expect("envelope")
                .mentions_self
        );
        let plain = message_event(text_event("hello there", None));
        assert!(
            !normalize_message(&plain, "!room:example.org", false, self_id())
                .expect("envelope")
                .mentions_self
        );
        let by_metadata = message_event(serde_json::json!({
            "type": "m.room.message",
            "event_id": "$m",
            "sender": "@bob:example.org",
            "origin_server_ts": 1_700_000_000_000i64,
            "content": {
                "msgtype": "m.text",
                "body": "nothing in the body",
                "m.mentions": {"user_ids": [SELF_ID]}
            }
        }));
        assert!(
            normalize_message(&by_metadata, "!room:example.org", false, self_id())
                .expect("envelope")
                .mentions_self
        );
    }

    #[test]
    fn our_own_message_is_flagged_as_ours() {
        let event = message_event(serde_json::json!({
            "type": "m.room.message",
            "event_id": "$mine",
            "sender": SELF_ID,
            "origin_server_ts": 1_700_000_000_000i64,
            "content": {"msgtype": "m.text", "body": "mine"}
        }));
        assert!(
            normalize_message(&event, "!room:example.org", true, self_id())
                .expect("envelope")
                .sender
                .is_self
        );
    }

    #[test]
    fn a_message_with_neither_text_nor_a_file_is_not_a_turn() {
        let event = message_event(text_event("", None));
        assert!(normalize_message(&event, "!room:example.org", true, self_id()).is_none());
    }

    #[test]
    fn a_retried_send_reuses_the_outboxs_own_transaction_id() {
        assert_eq!(transaction_id_for("idem-1").as_str(), "idem-1");
        assert_eq!(
            transaction_id_for("idem-1"),
            transaction_id_for("idem-1"),
            "the homeserver dedupes on this, so it must never be random"
        );
    }

    #[test]
    fn mention_matches_at_word_boundaries_only() {
        assert!(mentions_word_boundary(
            "hi @self:example.org there",
            "@self:example.org"
        ));
        assert!(!mentions_word_boundary(
            "notself:example.org",
            "self:example.org"
        ));
        assert!(mentions_word_boundary("SELF is here", "self"));
    }

    #[test]
    fn homeserver_url_requires_https_except_for_localhost() {
        assert!(validate_homeserver_url("https://matrix.example.org").is_ok());
        assert!(validate_homeserver_url("http://matrix.example.org").is_err());
        assert!(validate_homeserver_url("http://localhost:8008").is_ok());
        assert!(validate_homeserver_url("http://127.0.0.1:8008").is_ok());
    }

    #[test]
    fn homeserver_url_rejects_a_path_or_query() {
        assert!(validate_homeserver_url("https://matrix.example.org/_matrix").is_err());
        assert!(validate_homeserver_url("https://matrix.example.org?x=1").is_err());
    }

    fn test_account(
        non_secret_config: Value,
    ) -> super::super::super::channel_store::ChannelAccountRecord {
        super::super::super::channel_store::ChannelAccountRecord {
            account_id: "acct-1".to_string(),
            kind: ChannelKind::Matrix,
            label: "Matrix".to_string(),
            enabled: true,
            non_secret_config,
            credential_ref: Some("matrix/acct-1".to_string()),
            access_policy: Default::default(),
            health: ChannelHealth::error(0, "unused"),
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn rejects_a_missing_access_token() {
        let account = test_account(serde_json::json!({
            "homeserver_url": "https://matrix.example.org",
            "user_id": SELF_ID,
        }));
        assert!(MatrixAdapter::new(&AdapterConfig {
            account: &account,
            secret: String::new(),
        })
        .is_err());
    }

    #[test]
    fn rejects_a_missing_homeserver_url() {
        let account = test_account(serde_json::json!({ "user_id": SELF_ID }));
        assert!(MatrixAdapter::new(&AdapterConfig {
            account: &account,
            secret: "token".to_string(),
        })
        .is_err());
    }

    #[test]
    fn rejects_a_user_id_that_is_not_one() {
        let account = test_account(serde_json::json!({
            "homeserver_url": "https://matrix.example.org",
            "user_id": "not-a-matrix-id",
        }));
        assert!(MatrixAdapter::new(&AdapterConfig {
            account: &account,
            secret: "token".to_string(),
        })
        .is_err());
    }

    #[test]
    fn capabilities_report_threads_now_that_they_are_implemented() {
        let account = test_account(serde_json::json!({
            "homeserver_url": "https://matrix.example.org",
            "user_id": SELF_ID,
        }));
        let adapter = MatrixAdapter::new(&AdapterConfig {
            account: &account,
            secret: "token".to_string(),
        })
        .expect("adapter");
        let capabilities = adapter.capabilities();
        assert_eq!(capabilities.max_text_chars, MAX_TEXT_CHARS);
        assert_eq!(capabilities.kind, ChannelKind::Matrix);
        assert!(capabilities.supports_threads);
        assert!(capabilities.supports_attachments);
    }

    #[test]
    fn construction_alone_starts_no_client_and_no_sync() {
        let account = test_account(serde_json::json!({
            "homeserver_url": "https://matrix.example.org",
            "user_id": SELF_ID,
        }));
        let adapter = MatrixAdapter::new(&AdapterConfig {
            account: &account,
            secret: "token".to_string(),
        })
        .expect("adapter");
        assert!(adapter.client.get().is_none());
        assert!(adapter.started.get().is_none());
    }
}
