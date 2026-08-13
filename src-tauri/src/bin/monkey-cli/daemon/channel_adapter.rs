//! The seam every messaging provider plugs into.
//!
//! An adapter does exactly two translations — provider wire format into
//! [`ChannelEnvelope`] on the way in, [`OutboundMessage`] into provider wire
//! format on the way out — plus a health probe. It never executes an agent,
//! never resolves a route, never decides who may talk, and never touches the
//! run ledger. Everything downstream of `poll` goes through
//! `channel_ingress::plan_channel_ingress`, which is the one gate.
//!
//! Two shapes of provider exist and both live behind this trait:
//!
//! - **Polling / socket** providers own their own inbound loop and hand batches
//!   to [`ChannelAdapter::poll`]. Their resume state is a bounded cursor stored
//!   in `channel_cursors` — never a credential.
//! - **Webhook** providers cannot be polled. They implement
//!   [`WebhookChannelAdapter`] instead, verify their own signature over the raw
//!   body, and are driven by the daemon's existing webhook listener.
//!
//! # Credentials
//!
//! An adapter is constructed with the secret already resolved by
//! [`ChannelSecrets`], so no adapter reads the keychain itself and no adapter
//! can be built with a secret it was not handed. `ChannelAccountRecord` carries
//! only a `credential_ref` — the keychain account name — which is what keeps a
//! copied database useless.

use super::channel_store::ChannelAccountRecord;
use async_trait::async_trait;
use little_monkey_lib::channels::types::{
    AttachmentSource, ChannelAttachment, ChannelEnvelope, ChannelHealth, ChannelKind,
    DeliveryReceipt, OutboundMessage, ProviderCapabilities, SendOutcome,
};

/// One batch of inbound events plus the cursor to resume from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InboundBatch {
    pub envelopes: Vec<ChannelEnvelope>,
    /// Bounded resume token to persist. `None` leaves the stored cursor alone,
    /// which is what a provider with no resume concept wants.
    pub cursor: Option<String>,
}

/// What an adapter needs to exist: the account row plus its resolved secret.
pub struct AdapterConfig<'a> {
    pub account: &'a ChannelAccountRecord,
    /// The credential, already read from the keychain. Empty when the account
    /// has no `credential_ref`, which every adapter must reject in `probe`
    /// rather than by panicking.
    pub secret: String,
}

#[async_trait]
pub trait ChannelAdapter: Send + Sync {
    fn kind(&self) -> ChannelKind;

    fn capabilities(&self) -> ProviderCapabilities;

    /// Ask the provider who we are. This — and only this — is what may write
    /// `HealthState::Connected`: saved configuration is not a connection.
    async fn probe(&self) -> ChannelHealth;

    /// Fetch the next batch of inbound events, resuming from `cursor`.
    ///
    /// Long-polling adapters block here for their own bounded interval. A
    /// webhook-driven adapter returns an empty batch forever and is not
    /// scheduled by the poll loop.
    async fn poll(&self, cursor: Option<&str>) -> Result<InboundBatch, String>;

    /// Send one message. The returned [`SendOutcome`] is what decides whether
    /// the outbox retries, gives up, or parks the row for reconciliation —
    /// an adapter that cannot prove a request never left the machine must say
    /// `NeedsReconciliation` rather than `RetryableFailure`.
    async fn send(&self, message: &OutboundMessage) -> SendOutcome;

    /// Called once the envelopes of a batch are durably recorded (and the
    /// cursor persisted), so a transport with its own delivery handshake can
    /// acknowledge them to the provider. Slack's Socket Mode is the customer:
    /// an envelope acknowledged before this point can be lost in a crash,
    /// one acknowledged here at worst gets redelivered and deduplicated.
    ///
    /// The default does nothing, which is correct for every transport whose
    /// resume token is the cursor itself.
    async fn commit_batch(&self, _envelopes: &[ChannelEnvelope]) {}

    /// Download one inbound attachment.
    ///
    /// The default handles [`AttachmentSource::Url`], which is what most
    /// providers hand out, through the hardened client every other request
    /// uses. A provider that instead gives an opaque handle has to resolve it
    /// itself — that is a second authenticated call only the adapter knows how
    /// to make — and says so rather than pretending it cannot be done.
    async fn fetch_attachment(
        &self,
        attachment: &ChannelAttachment,
        limits: AttachmentLimits,
    ) -> Result<Vec<u8>, String> {
        match &attachment.source {
            AttachmentSource::Url { url } => fetch_url(url, None, limits.max_bytes).await,
            AttachmentSource::ProviderHandle { .. } => Err(format!(
                "Little Monkey cannot download {} attachments yet",
                self.kind().label()
            )),
        }
    }
}

/// GET one attachment URL under the size cap, optionally with a bearer token.
///
/// Shared by the default implementation and by the adapters that first resolve
/// a handle into a URL. The body is read in chunks and abandoned the moment it
/// crosses the cap, so an oversized file costs the cap and not its own size —
/// a `Content-Length` cannot be trusted to be the truth about what follows.
pub async fn fetch_url(url: &str, bearer: Option<&str>, max_bytes: u64) -> Result<Vec<u8>, String> {
    let client = little_monkey_lib::egress::hardened()
        .build()
        .map_err(|error| format!("Failed to build the download client: {error}"))?;
    let mut request = client.get(url);
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    let response = little_monkey_lib::egress::send(request)
        .await
        .map_err(|_| "The attachment could not be downloaded".to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "The provider returned {} for the attachment",
            response.status()
        ));
    }
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "The attachment download was interrupted".to_string())?
    {
        if bytes.len() as u64 + chunk.len() as u64 > max_bytes {
            return Err(format!(
                "The attachment is larger than the {max_bytes}-byte limit"
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// How much of a text file is carried into the turn.
///
/// Enough to answer a question about a log or a short CSV, small enough that
/// one file cannot crowd out the conversation it arrived in.
const MAX_TEXT_EXCERPT_CHARS: usize = 4_000;

/// How many attachments are downloaded at once.
///
/// Bounded rather than unbounded: a message with ten files should not open ten
/// sockets to a provider that is already rate-limiting this account, and an
/// unbounded fan-out is how a poll loop turns one chatty conversation into a
/// burst the provider answers with 429s.
const CONCURRENT_DOWNLOADS: usize = 4;

/// What one account allows an inbound attachment to cost.
///
/// Per account rather than global: a Telegram bot on a home connection and a
/// WhatsApp number on a server have no reason to share a limit, and an operator
/// who wants a 64 MB cap on one of them should not have to raise it everywhere.
/// Read from the account's own non-secret config, which is why these are plain
/// numbers rather than a type the UI has to learn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachmentLimits {
    pub max_bytes: u64,
    pub max_excerpt_chars: usize,
    pub max_listed: usize,
}

impl Default for AttachmentLimits {
    fn default() -> Self {
        Self {
            max_bytes: MAX_ATTACHMENT_BYTES,
            max_excerpt_chars: MAX_TEXT_EXCERPT_CHARS,
            max_listed: little_monkey_lib::channels::ingress::MAX_LISTED_ATTACHMENTS,
        }
    }
}

impl AttachmentLimits {
    /// The limits an account configured, falling back to the defaults for
    /// anything it did not set.
    ///
    /// Each value is clamped to a ceiling no account may raise: these bound
    /// what a stranger's message can make this machine spend, so an operator
    /// can lower them freely and can only raise them so far.
    pub fn for_account(config: &serde_json::Value) -> Self {
        const CEILING_BYTES: u64 = 64 * 1024 * 1024;
        const CEILING_EXCERPT: usize = 32_000;
        const CEILING_LISTED: usize = 50;
        let default = Self::default();
        let number = |key: &str| config.get(key).and_then(serde_json::Value::as_u64);
        Self {
            max_bytes: number("max_attachment_bytes")
                .unwrap_or(default.max_bytes)
                .clamp(1, CEILING_BYTES),
            max_excerpt_chars: number("max_attachment_excerpt_chars")
                .map(|value| usize::try_from(value).unwrap_or(usize::MAX))
                .unwrap_or(default.max_excerpt_chars)
                .clamp(0, CEILING_EXCERPT),
            max_listed: number("max_listed_attachments")
                .map(|value| usize::try_from(value).unwrap_or(usize::MAX))
                .unwrap_or(default.max_listed)
                .clamp(1, CEILING_LISTED),
        }
    }
}

/// The beginning of a file's text, when the bytes are text at all.
///
/// A file that is not valid UTF-8 has no excerpt rather than a mangled one —
/// lossy decoding of a JPEG produces thousands of replacement characters and
/// answers no question anybody asked.
fn text_excerpt(bytes: &[u8], max_chars: usize) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    if text.trim().is_empty() || max_chars == 0 {
        return None;
    }
    let mut excerpt: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        excerpt.push('…');
    }
    Some(excerpt)
}

/// Download every attachment on a batch of envelopes and store the bytes.
///
/// Runs before ingest, so what becomes durable is the turn as the agent will
/// see it. A failure is recorded on the attachment rather than dropped: an
/// attachment that was too large or that the provider refused is something the
/// sender should be told about, and silence would make it look as though
/// nothing was sent at all.
pub async fn hydrate_attachments(
    adapter: &dyn ChannelAdapter,
    blobs: &dyn BlobSource,
    limits: AttachmentLimits,
    envelopes: &mut [ChannelEnvelope],
) {
    // Downloads run a few at a time across the whole batch. Serially, one slow
    // 16 MB file holds up every other message in the same poll, including the
    // ones carrying no files at all — ingest waits for the batch, not for one
    // envelope.
    let pending: Vec<(usize, usize)> = envelopes
        .iter()
        .enumerate()
        .flat_map(|(envelope_index, envelope)| {
            envelope
                .attachments
                .iter()
                .enumerate()
                .filter(|(_, attachment)| attachment.stored_artifact_id.is_none())
                .map(move |(attachment_index, _)| (envelope_index, attachment_index))
        })
        .collect();

    for window in pending.chunks(CONCURRENT_DOWNLOADS) {
        let fetches = window.iter().map(|(envelope_index, attachment_index)| {
            let attachment = &envelopes[*envelope_index].attachments[*attachment_index];
            async move { adapter.fetch_attachment(attachment, limits).await }
        });
        let results = futures_util::future::join_all(fetches).await;
        for ((envelope_index, attachment_index), result) in window.iter().zip(results) {
            let attachment = &mut envelopes[*envelope_index].attachments[*attachment_index];
            match result {
                Ok(bytes) => match blobs.write(&bytes) {
                    Ok(artifact_id) => {
                        attachment.declared_size_bytes = Some(bytes.len() as u64);
                        attachment.text_excerpt = text_excerpt(&bytes, limits.max_excerpt_chars);
                        // Given a name a vision model can be pointed at, while
                        // the bytes are known to be on disk — a failure here is
                        // visible now rather than silent at prompt time.
                        if let Some(extension) = vision_extension(attachment.mime_type.as_deref()) {
                            blobs.image_path(&artifact_id, extension);
                        }
                        attachment.stored_artifact_id = Some(artifact_id);
                    }
                    Err(error) => attachment.fetch_error = Some(error),
                },
                Err(error) => attachment.fetch_error = Some(error),
            }
        }
    }
}

/// The image types this tree's encoders can name a MIME type for.
///
/// A file whose type is not one of these is stored but never offered to a
/// vision model, because the encoder would have to guess — and it guesses
/// `image/jpeg`, which is wrong for everything else.
pub fn vision_extension(mime: Option<&str>) -> Option<&'static str> {
    match mime? {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

/// Providers that are delivered to rather than polled.
///
/// Signature verification happens here, over the exact bytes received, because
/// only the adapter knows the provider's canonicalization. The daemon's
/// listener never reconstructs a URL from `Host` or `X-Forwarded-*` headers for
/// this purpose — those are attacker-controlled — and passes the configured
/// public base URL instead when a provider's signature covers it.
pub trait WebhookChannelAdapter: Send + Sync {
    fn kind(&self) -> ChannelKind;

    /// Verify and normalize one delivery. `headers` are lowercase-keyed.
    ///
    /// Returning `Err` rejects the delivery without recording anything, which
    /// is the correct answer for a bad signature: an unverified body has not
    /// earned a row in the durable event log.
    fn verify_and_normalize(
        &self,
        headers: &[(String, String)],
        body: &[u8],
        public_base_url: Option<&str>,
        now_ms: i64,
    ) -> Result<Vec<ChannelEnvelope>, String>;

    /// Delivery progress this same body reports for messages already sent.
    ///
    /// Separate from the envelopes because a receipt is not a turn: nobody is
    /// speaking, so nothing runs. It is only ever called on a body whose
    /// signature already verified, and the default is empty for providers that
    /// report nothing.
    fn delivery_receipts(&self, _body: &[u8], _now_ms: i64) -> Vec<DeliveryReceipt> {
        Vec::new()
    }

    /// Answer this provider's webhook-registration handshake, if it has one.
    ///
    /// Meta will not save a callback URL until the endpoint echoes the
    /// `hub.challenge` it sends, so without an answer here an operator cannot
    /// finish WhatsApp setup at all. `query` is the raw query string of a GET
    /// to the account's callback path.
    ///
    /// `None` refuses, which is both the default and the right answer for
    /// every provider that has no such handshake — the route turns it into a
    /// flat 403 rather than a hint about what would have worked.
    fn verification_challenge(&self, _query: &str) -> Option<String> {
        None
    }
}

/// Largest single file an agent may attach to a reply.
///
/// Well under every provider's own limit, and chosen so a runaway model cannot
/// push a disk image through the outbox one reply at a time. The cap is applied
/// twice — when the file is imported, and again when the bytes are read back —
/// so a blob that grew between the two cannot slip past.
pub const MAX_ATTACHMENT_BYTES: u64 = 16 * 1024 * 1024;

/// Where an adapter gets an attachment's bytes.
///
/// A trait for the same reason [`ChannelSecrets`] is one: the real
/// implementation resolves the daemon's own paths, which a test has no business
/// creating. Injecting it is what lets the multipart upload itself be exercised
/// against a loopback server rather than only the branch that picks the method.
pub trait BlobSource: Send + Sync {
    fn read(&self, artifact_id: &str) -> Result<Vec<u8>, String>;

    /// Store bytes and return the id they can be read back by. Used by
    /// inbound hydration, which has bytes and needs somewhere durable to put
    /// them before the turn is queued.
    fn write(&self, bytes: &[u8]) -> Result<String, String>;

    /// Give a stored image a name a vision model can be pointed at.
    ///
    /// The content store names blobs by digest and gives them no extension,
    /// and this tree's image encoders read the extension to decide the MIME
    /// type. Rather than keep a second copy, the blob is hard-linked under a
    /// name that carries one, falling back to a copy where links are refused.
    fn image_path(&self, _artifact_id: &str, _extension: &str) -> Option<std::path::PathBuf> {
        None
    }
}

/// The production source: the daemon's own content store.
pub struct DaemonBlobs;

impl BlobSource for DaemonBlobs {
    fn read(&self, artifact_id: &str) -> Result<Vec<u8>, String> {
        let paths = super::store::DaemonPaths::resolve()?;
        read_blob(&paths, artifact_id)
    }

    fn image_path(&self, artifact_id: &str, extension: &str) -> Option<std::path::PathBuf> {
        let paths = super::store::DaemonPaths::resolve().ok()?;
        image_path_in(&paths, artifact_id, extension)
    }

    fn write(&self, bytes: &[u8]) -> Result<String, String> {
        let paths = super::store::DaemonPaths::resolve()?;
        content_store(&paths)?
            .put(bytes)
            .map(|blob| blob.id)
            .map_err(|error| format!("Failed to store the attachment: {error}"))
    }
}

/// Where a stored image is given a name a vision model can be pointed at.
///
/// Deterministic, so the same blob resolves to the same path on every call and
/// a second turn about the same photo does not copy it again. Public because
/// the agent process resolves the same path when it builds a prompt, without
/// having to be told it.
pub fn image_path_in(
    paths: &super::store::DaemonPaths,
    artifact_id: &str,
    extension: &str,
) -> Option<std::path::PathBuf> {
    let directory = paths.root.join("attachments");
    let linked = directory.join(format!("{artifact_id}.{extension}"));
    if linked.is_file() {
        return Some(linked);
    }
    let blob = content_store(paths).ok()?.blob_path(artifact_id).ok()?;
    if !blob.is_file() {
        return None;
    }
    std::fs::create_dir_all(&directory).ok()?;
    match std::fs::hard_link(&blob, &linked) {
        Ok(()) => Some(linked),
        // Links are refused across volumes, and by some filesystems entirely.
        // A copy costs the bytes twice but is never wrong.
        Err(_) => std::fs::copy(&blob, &linked).ok().map(|_| linked),
    }
}

/// The daemon's content store, sized to the attachment cap.
fn content_store(
    paths: &super::store::DaemonPaths,
) -> Result<little_monkey_lib::artifact_store::ArtifactStore, String> {
    let app_data = paths
        .root
        .parent()
        .ok_or_else(|| "Daemon root has no app-data parent".to_string())?;
    little_monkey_lib::artifact_store::ArtifactStore::with_max_blob_size(
        app_data.join("content-v1"),
        MAX_ATTACHMENT_BYTES,
    )
    .map_err(|error| format!("Failed to open the content store: {error}"))
}

/// One outbound attachment with its bytes already read, which is the shape
/// every provider's upload needs: a filename, a type to declare, and the file.
pub struct LoadedAttachment {
    pub filename: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

/// Read every attachment on a message, or the outcome the outbox should record.
///
/// A blob that cannot be read is a permanent failure rather than a retry: the
/// bytes were copied into the content store when the reply was queued, so a
/// read failing now fails the same way every time after.
pub fn load_attachments(
    blobs: &dyn BlobSource,
    message: &OutboundMessage,
) -> Result<Vec<LoadedAttachment>, SendOutcome> {
    message
        .attachments
        .iter()
        .map(|attachment| {
            let bytes = blobs
                .read(&attachment.artifact_id)
                .map_err(|error| SendOutcome::PermanentFailure { error })?;
            Ok(LoadedAttachment {
                filename: attachment
                    .filename
                    .clone()
                    .unwrap_or_else(|| "attachment".to_string()),
                mime_type: attachment_mime(attachment).to_string(),
                bytes,
            })
        })
        .collect()
}

/// Read one outbound attachment's bytes out of the content store.
///
/// Attachments are copied into the store when the reply is queued, not read
/// from disk at send time: an outbox row can be retried minutes later, and the
/// file the agent meant is the one that existed when it said so, not whatever
/// occupies that path now.
pub fn read_blob(paths: &super::store::DaemonPaths, artifact_id: &str) -> Result<Vec<u8>, String> {
    content_store(paths)?
        .read(artifact_id)
        .map_err(|error| format!("Failed to read the attachment: {error}"))
}

/// The MIME type to send a file as.
///
/// The attachment's own type when the tool recorded one, else guessed from the
/// extension, else the generic byte stream — never a guess at the *contents*,
/// which is the provider's business and not something to assert wrongly.
pub fn attachment_mime(
    attachment: &little_monkey_lib::channels::types::OutboundAttachment,
) -> &str {
    if let Some(mime) = attachment.mime_type.as_deref() {
        return mime;
    }
    let extension = attachment
        .filename
        .as_deref()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "json" => "application/json",
        "csv" => "text/csv",
        "zip" => "application/zip",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    }
}

/// Whether this account cannot work until a credential is stored.
///
/// True for every provider Little Monkey authenticates to itself. It is false
/// for the two helper providers — Signal and iMessage authenticate through a
/// helper the operator installs and registers, which owns the account's own
/// keys — and for IRC unless SASL is turned on. Demanding a credential from
/// those would make them impossible to enable, so `channels enable` asks this
/// rather than assuming, and the answer is reported to the UI so the panel
/// stops showing a credential box nobody can fill.
pub fn credential_required(account: &ChannelAccountRecord) -> bool {
    match account.kind {
        ChannelKind::Signal | ChannelKind::IMessage => false,
        ChannelKind::Irc => account
            .non_secret_config
            .get("use_sasl")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        _ => true,
    }
}

/// Keychain-backed credential storage for messaging accounts.
///
/// A trait so tests never touch the real keychain — the CI machines have none —
/// and so a distributor can substitute a different store without every adapter
/// learning about it.
pub trait ChannelSecrets: Send + Sync {
    fn put(&self, credential_ref: &str, secret: &str) -> Result<(), String>;
    fn get(&self, credential_ref: &str) -> Result<String, String>;
    fn delete(&self, credential_ref: &str) -> Result<(), String>;
}

pub struct KeyringChannelSecrets;

impl KeyringChannelSecrets {
    fn entry(credential_ref: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(
            &little_monkey_lib::channels::KEYCHAIN_SERVICE,
            credential_ref,
        )
        .map_err(|error| format!("Failed to open the messaging keychain entry: {error}"))
    }
}

impl ChannelSecrets for KeyringChannelSecrets {
    fn put(&self, credential_ref: &str, secret: &str) -> Result<(), String> {
        if secret.is_empty() || secret.len() > 8192 {
            return Err("A messaging credential must contain 1-8192 bytes".to_string());
        }
        Self::entry(credential_ref)?
            .set_password(secret)
            .map_err(|error| format!("Failed to save the messaging credential: {error}"))
    }

    fn get(&self, credential_ref: &str) -> Result<String, String> {
        Self::entry(credential_ref)?
            .get_password()
            .map_err(|error| format!("Failed to read the messaging credential: {error}"))
    }

    fn delete(&self, credential_ref: &str) -> Result<(), String> {
        match Self::entry(credential_ref)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(format!(
                "Failed to delete the messaging credential: {error}"
            )),
        }
    }
}

/// An in-memory secret store for tests and for a build with no keychain.
#[derive(Default)]
pub struct MemoryChannelSecrets {
    entries: std::sync::Mutex<std::collections::BTreeMap<String, String>>,
}

impl ChannelSecrets for MemoryChannelSecrets {
    fn put(&self, credential_ref: &str, secret: &str) -> Result<(), String> {
        self.entries
            .lock()
            .map_err(|_| "channel secret store poisoned".to_string())?
            .insert(credential_ref.to_string(), secret.to_string());
        Ok(())
    }

    fn get(&self, credential_ref: &str) -> Result<String, String> {
        self.entries
            .lock()
            .map_err(|_| "channel secret store poisoned".to_string())?
            .get(credential_ref)
            .cloned()
            .ok_or_else(|| format!("No stored credential for '{credential_ref}'"))
    }

    fn delete(&self, credential_ref: &str) -> Result<(), String> {
        self.entries
            .lock()
            .map_err(|_| "channel secret store poisoned".to_string())?
            .remove(credential_ref);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use little_monkey_lib::channels::types::{AttachmentKind, AttachmentSource};

    #[test]
    fn an_account_that_configures_nothing_gets_the_defaults() {
        let limits = AttachmentLimits::for_account(&serde_json::json!({}));
        assert_eq!(limits, AttachmentLimits::default());
    }

    #[test]
    fn an_account_can_tune_all_three_limits() {
        let limits = AttachmentLimits::for_account(&serde_json::json!({
            "max_attachment_bytes": 1024,
            "max_attachment_excerpt_chars": 50,
            "max_listed_attachments": 2
        }));
        assert_eq!(limits.max_bytes, 1024);
        assert_eq!(limits.max_excerpt_chars, 50);
        assert_eq!(limits.max_listed, 2);
    }

    #[test]
    fn an_account_cannot_raise_a_limit_past_its_ceiling() {
        // These bound what a stranger's message can make this machine spend,
        // so lowering is free and raising stops somewhere.
        let limits = AttachmentLimits::for_account(&serde_json::json!({
            "max_attachment_bytes": 999_999_999_999u64,
            "max_attachment_excerpt_chars": 10_000_000,
            "max_listed_attachments": 10_000
        }));
        assert_eq!(limits.max_bytes, 64 * 1024 * 1024);
        assert_eq!(limits.max_excerpt_chars, 32_000);
        assert_eq!(limits.max_listed, 50);
    }

    #[test]
    fn a_nonsense_value_falls_back_rather_than_disabling_the_limit() {
        let limits = AttachmentLimits::for_account(&serde_json::json!({
            "max_attachment_bytes": "lots",
            "max_listed_attachments": 0
        }));
        assert_eq!(limits.max_bytes, AttachmentLimits::default().max_bytes);
        // Zero would list nothing at all; one is the smallest honest answer.
        assert_eq!(limits.max_listed, 1);
    }

    #[test]
    fn an_excerpt_stops_at_the_configured_length() {
        let long = "x".repeat(100);
        let excerpt = text_excerpt(long.as_bytes(), 10).expect("text");
        assert_eq!(
            excerpt.chars().count(),
            11,
            "ten characters plus the ellipsis"
        );
        assert!(excerpt.ends_with('…'));
        assert!(
            text_excerpt(&[0xff, 0xfe], 10).is_none(),
            "not UTF-8, no excerpt"
        );
        assert!(
            text_excerpt(b"hello", 0).is_none(),
            "an account may turn excerpts off"
        );
    }

    #[test]
    fn only_the_image_types_the_encoder_can_name_are_offered_to_a_vision_model() {
        assert_eq!(vision_extension(Some("image/png")), Some("png"));
        assert_eq!(vision_extension(Some("image/jpeg")), Some("jpg"));
        assert_eq!(vision_extension(Some("image/webp")), Some("webp"));
        // The encoder would guess image/jpeg for these, which is wrong.
        assert_eq!(vision_extension(Some("image/gif")), None);
        assert_eq!(vision_extension(Some("image/svg+xml")), None);
        assert_eq!(vision_extension(Some("application/pdf")), None);
        assert_eq!(vision_extension(None), None);
    }

    /// A daemon layout under a fresh temp dir, so the content store and the
    /// attachments directory are this test's own.
    fn temp_paths() -> super::super::store::DaemonPaths {
        let root =
            std::env::temp_dir().join(format!("lm-vision-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(root.join("state")).expect("root");
        super::super::store::DaemonPaths {
            config: root.join("config.json"),
            state_db: root.join("state.db"),
            ledger_db: root.join("ledger.db"),
            snapshots: root.join("snapshots"),
            logs: root.join("logs"),
            worktrees: root.join("worktrees"),
            lock: root.join("lock"),
            root,
        }
    }

    #[test]
    fn a_stored_image_gets_a_name_that_carries_its_type() {
        let paths = temp_paths();
        let store = content_store(&paths).expect("store");
        let blob = store.put(b"\x89PNG pretend").expect("stored");

        let first = image_path_in(&paths, &blob.id, "png").expect("linked");
        assert_eq!(first.extension().and_then(|e| e.to_str()), Some("png"));
        assert_eq!(std::fs::read(&first).expect("read"), b"\x89PNG pretend");

        // Deterministic: a second turn about the same photo resolves to the
        // same path rather than making another copy.
        let second = image_path_in(&paths, &blob.id, "png").expect("resolved again");
        assert_eq!(first, second);
    }

    #[test]
    fn an_image_that_was_never_stored_has_no_path() {
        let paths = temp_paths();
        assert!(image_path_in(&paths, "sha256-nothing", "png").is_none());
    }

    /// An adapter that answers every fetch with the bytes it was built with,
    /// counting the calls so a test can prove they overlapped.
    struct CountingAdapter {
        bytes: Vec<u8>,
        in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        peak: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl ChannelAdapter for CountingAdapter {
        fn kind(&self) -> ChannelKind {
            ChannelKind::Telegram
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::minimal(
                ChannelKind::Telegram,
                little_monkey_lib::channels::types::InboundTransport::LongPoll,
            )
        }
        async fn probe(&self) -> ChannelHealth {
            ChannelHealth::error(1, "unused".to_string())
        }
        async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
            Ok(InboundBatch::default())
        }
        async fn send(&self, _message: &OutboundMessage) -> SendOutcome {
            SendOutcome::PermanentFailure {
                error: "unused".to_string(),
            }
        }
        async fn fetch_attachment(
            &self,
            _attachment: &ChannelAttachment,
            _limits: AttachmentLimits,
        ) -> Result<Vec<u8>, String> {
            use std::sync::atomic::Ordering;
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(self.bytes.clone())
        }
    }

    fn envelope_with(count: usize) -> ChannelEnvelope {
        use little_monkey_lib::channels::types::{ChannelConversation, ChannelSender};
        ChannelEnvelope {
            account_id: "acct-1".into(),
            kind: ChannelKind::Telegram,
            provider_event_id: "1".into(),
            conversation: ChannelConversation::direct("chat-1"),
            sender: ChannelSender::new("user-1"),
            text: String::new(),
            attachments: (0..count)
                .map(|index| ChannelAttachment {
                    provider_id: Some(format!("f{index}")),
                    kind: AttachmentKind::Document,
                    filename: Some(format!("file-{index}.txt")),
                    mime_type: Some("text/plain".into()),
                    declared_size_bytes: None,
                    source: AttachmentSource::ProviderHandle {
                        handle: format!("f{index}"),
                    },
                    stored_artifact_id: None,
                    text_excerpt: None,
                    fetch_error: None,
                })
                .collect(),
            reply_to_provider_id: None,
            mentions_self: false,
            received_at_ms: 1,
            metadata: Default::default(),
        }
    }

    #[tokio::test]
    async fn a_batch_downloads_several_files_at_once_and_never_more_than_the_cap() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let in_flight = std::sync::Arc::new(AtomicUsize::new(0));
        let peak = std::sync::Arc::new(AtomicUsize::new(0));
        let adapter = CountingAdapter {
            bytes: b"hello".to_vec(),
            in_flight: in_flight.clone(),
            peak: peak.clone(),
        };
        let mut envelopes = vec![envelope_with(5), envelope_with(4)];

        hydrate_attachments(
            &adapter,
            &test_http::FixtureBlobs(Vec::new()),
            AttachmentLimits::default(),
            &mut envelopes,
        )
        .await;

        assert!(
            peak.load(Ordering::SeqCst) > 1,
            "downloads ran one at a time"
        );
        assert!(
            peak.load(Ordering::SeqCst) <= CONCURRENT_DOWNLOADS,
            "more sockets than the cap allows: {}",
            peak.load(Ordering::SeqCst)
        );
        // Every attachment on both envelopes, and each one's own result.
        for envelope in &envelopes {
            for attachment in &envelope.attachments {
                assert_eq!(
                    attachment.stored_artifact_id.as_deref(),
                    Some("fixture-blob")
                );
                assert_eq!(attachment.text_excerpt.as_deref(), Some("hello"));
                assert!(attachment.fetch_error.is_none());
            }
        }
    }

    use super::*;
    use little_monkey_lib::channels::types::{ChannelHealth, HealthState};

    fn account(kind: ChannelKind, config: serde_json::Value) -> ChannelAccountRecord {
        ChannelAccountRecord {
            account_id: "acct-1".into(),
            kind,
            label: "Test".into(),
            enabled: false,
            non_secret_config: config,
            credential_ref: None,
            access_policy: Default::default(),
            health: ChannelHealth {
                state: HealthState::Unconfigured,
                detail: None,
                last_error: None,
                probed_at_ms: 0,
            },
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn the_helper_providers_need_no_credential_of_our_own() {
        // The helper holds the account; there is no token to demand, and
        // demanding one would make these two impossible to enable at all.
        for kind in [ChannelKind::Signal, ChannelKind::IMessage] {
            assert!(!credential_required(&account(kind, serde_json::json!({}))));
        }
    }

    #[test]
    fn irc_needs_a_password_only_when_sasl_is_on() {
        assert!(!credential_required(&account(
            ChannelKind::Irc,
            serde_json::json!({"server": "irc.example.org", "nick": "monkey"})
        )));
        assert!(credential_required(&account(
            ChannelKind::Irc,
            serde_json::json!({"server": "irc.example.org", "nick": "monkey", "use_sasl": true})
        )));
    }

    #[test]
    fn every_other_provider_still_needs_one() {
        for kind in [
            ChannelKind::Telegram,
            ChannelKind::Slack,
            ChannelKind::Matrix,
            ChannelKind::Mattermost,
            ChannelKind::WhatsApp,
        ] {
            assert!(credential_required(&account(kind, serde_json::json!({}))));
        }
    }
}

/// Loopback HTTP fixtures for adapter tests.
///
/// Kept here rather than duplicated per adapter because the upload paths all
/// need the same two things: the exact bytes a provider received, and more than
/// one canned response in order. Every server accepts a fixed number of
/// connections and then returns, so a test can never leave a listener behind.
#[cfg(test)]
pub(crate) mod test_http {
    use std::io::{Read, Write};
    use std::sync::mpsc::{channel, Receiver};

    /// Serve `responses` in order, one per connection, and hand back every
    /// request received in full.
    ///
    /// The body is read by `Content-Length` rather than to EOF, because a
    /// multipart upload keeps the connection open until it has been answered.
    pub(crate) fn serve(responses: Vec<(u16, String)>) -> (String, Receiver<Vec<u8>>) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (sender, receiver) = channel();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let mut received = Vec::new();
                let mut scratch = [0u8; 4096];
                let mut header_end = None;
                let mut content_length = 0usize;
                let mut chunked = false;
                loop {
                    match stream.read(&mut scratch) {
                        Ok(0) => break,
                        Ok(count) => {
                            received.extend_from_slice(&scratch[..count]);
                            if header_end.is_none() {
                                if let Some(index) = find(&received, b"\r\n\r\n") {
                                    header_end = Some(index + 4);
                                    content_length = content_length_of(&received[..index]);
                                    chunked = String::from_utf8_lossy(&received[..index])
                                        .to_ascii_lowercase()
                                        .contains("transfer-encoding: chunked");
                                }
                            }
                            if let Some(start) = header_end {
                                // A multipart upload has no Content-Length —
                                // reqwest streams it chunked — so the end of
                                // the body is the terminating zero-length
                                // chunk. Answering before it arrives closes the
                                // connection under a request still being
                                // written, which the client reports as a
                                // failure to send at all.
                                let complete = if content_length > 0 {
                                    received.len() >= start + content_length
                                } else if chunked {
                                    find(&received[start..], b"0\r\n\r\n").is_some()
                                } else {
                                    // No length and no chunking means no body —
                                    // a GET. Waiting for one costs the read
                                    // timeout and proves nothing.
                                    true
                                };
                                if complete {
                                    break;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                let _ = sender.send(received);
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        (format!("http://127.0.0.1:{port}"), receiver)
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn content_length_of(headers: &[u8]) -> usize {
        String::from_utf8_lossy(headers)
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0)
    }

    /// A blob source holding one file, so an upload test needs no daemon.
    pub(crate) struct FixtureBlobs(pub Vec<u8>);

    impl super::BlobSource for FixtureBlobs {
        fn read(&self, _artifact_id: &str) -> Result<Vec<u8>, String> {
            Ok(self.0.clone())
        }

        fn write(&self, _bytes: &[u8]) -> Result<String, String> {
            Ok("fixture-blob".to_string())
        }
    }
}
