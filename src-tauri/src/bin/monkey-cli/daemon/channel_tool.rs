//! The `send_message` agent tool: the one provider-independent way a run
//! sends a message.
//!
//! By default a run answers the conversation it came from — the destination is
//! read from the durable event that produced the job, not from a tool
//! argument, so the message being read cannot redirect the reply. A run may
//! name a different conversation or account only when its immutable permission
//! snapshot granted that destination ([`SendAuthority`]); the grant is decided
//! by the operator's route and recipe, never by anything in the run.
//!
//! Whatever the destination:
//!
//! - The message is queued into the durable outbox rather than sent here. The
//!   tool returns as soon as the row is durable, so a crash between "the model
//!   said it" and "the provider has it" resolves the same way every other
//!   outbound message does. This file never calls a provider adapter.
//! - The idempotency key is derived from the job and the number of messages it
//!   has already queued, so a retried run cannot duplicate a send.
//! - Reply depth is carried forward, which is what lets the inbound gate stop
//!   two agents from talking to each other forever.

use little_monkey_lib::channels::types::{ChannelEnvelope, OutboundAttachment, OutboundMessage};

use super::channel_adapter::MAX_ATTACHMENT_BYTES;
use super::channel_ingress::OutboxPayload;
use super::channel_store::{ChannelOrigin, NewOutboxMessage, OutboxEnqueue};
use super::store::{DaemonPaths, DaemonStore};
use super::trigger::sha256_hex;

/// Retry budget for an agent's reply. Matches the pairing challenge: a reply
/// that will not go out in a few attempts needs an operator, not a longer tail.
const REPLY_MAX_ATTEMPTS: u32 = 3;

/// Longest reply this tool will queue. Providers impose their own, much smaller
/// limits and adapters split accordingly; this is only the outer bound that
/// keeps a runaway model from writing a megabyte into the daemon database.
const MAX_REPLY_CHARS: usize = 16_000;

/// Environment variable the daemon sets on a task child so it knows which job
/// it is. Absent for every other kind of run, which is exactly how this tool
/// knows it has nothing to reply to.
pub(crate) const JOB_ID_ENV: &str = "LITTLE_MONKEY_DAEMON_JOB_ID";

/// The origin of the current process's run, if it has one.
pub(crate) fn current_channel_origin() -> Option<(String, ChannelOrigin)> {
    let job_id = std::env::var(JOB_ID_ENV).ok().filter(|id| !id.is_empty())?;
    let paths = DaemonPaths::resolve().ok()?;
    let store = DaemonStore::open(&paths).ok()?;
    let origin = store.channel_origin_for_job(&job_id).ok().flatten()?;
    Some((job_id, origin))
}

/// Image files this run's own inbound message carried.
///
/// The list comes from the durable event that produced this job, never from
/// the prompt: the message text is written by a stranger, and scanning it for
/// paths would let that stranger name any image on this machine and have the
/// model describe it back to them. Only what an adapter downloaded for this
/// turn is offered.
///
/// Empty for every run that did not arrive from a conversation, which is what
/// keeps this invisible to ordinary CLI use.
pub(crate) fn current_turn_images() -> Vec<std::path::PathBuf> {
    let Ok(job_id) = std::env::var(JOB_ID_ENV) else {
        return Vec::new();
    };
    if job_id.is_empty() {
        return Vec::new();
    }
    let Ok(paths) = DaemonPaths::resolve() else {
        return Vec::new();
    };
    let Ok(store) = DaemonStore::open(&paths) else {
        return Vec::new();
    };
    let Ok(Some(envelope_json)) = store.inbound_envelope_for_job(&job_id) else {
        return Vec::new();
    };
    let Ok(envelope) = serde_json::from_str::<ChannelEnvelope>(&envelope_json) else {
        return Vec::new();
    };
    envelope
        .attachments
        .iter()
        .filter_map(|attachment| {
            let artifact_id = attachment.stored_artifact_id.as_deref()?;
            let extension =
                super::channel_adapter::vision_extension(attachment.mime_type.as_deref())?;
            super::channel_adapter::image_path_in(&paths, artifact_id, extension)
        })
        .collect()
}

/// One provider-independent send, as the tool loop hands it over. Everything
/// the model may say about a message is here; everything it may not — which
/// account exists, what the run is allowed to reach — is resolved against the
/// store and [`SendAuthority`], never against these fields alone.
#[derive(Debug, Clone, Default)]
pub(crate) struct ChannelSendRequest {
    /// Configured account to send through. Absent means the account this
    /// run's conversation arrived on.
    pub account_id: Option<String>,
    /// Destination conversation. Absent means the conversation this run came
    /// from — and is an error when `account_id` names a different account,
    /// because there is no origin conversation on that account to default to.
    pub conversation_id: Option<String>,
    /// Provider thread inside the conversation.
    pub thread_id: Option<String>,
    /// Provider message id to reply to. Absent on an origin reply means the
    /// message that produced this run.
    pub reply_to_provider_id: Option<String>,
    pub text: String,
    /// Durable artifact ids already in the content store. The only way a file
    /// travels: the tool takes no filesystem path, so nothing the model says
    /// can name a file on this machine that was not already made durable.
    pub artifact_ids: Vec<String>,
}

/// What this run's immutable snapshot lets it reach. Derived once per call in
/// [`send_authority`] from the permission snapshot plus the frozen route that
/// produced the job — both decided before the run started, neither writable
/// from inside it.
#[derive(Debug, Clone, Default)]
pub(crate) struct SendAuthority {
    /// May answer the conversation this run came from.
    pub reply: bool,
    /// May target other conversations on the origin account.
    pub cross_conversation: bool,
    /// Other account ids this run may send through.
    pub accounts: Vec<String>,
}

impl SendAuthority {
    pub(crate) fn allows_anything(&self) -> bool {
        self.reply || self.cross_conversation || !self.accounts.is_empty()
    }
}

/// The run's send authority: the legacy external-mutations grant or the
/// frozen route's reply flag covers answering the origin conversation, and
/// the snapshot's explicit [`ChannelSendPolicy`] covers everything wider.
///
/// Thin wrapper: it only resolves the current process's job and store, then
/// derives through [`send_authority_for_job`], so a test exercising the
/// derivation against its own store crosses the same logic.
pub(crate) fn send_authority(
    allow_external_mutations: bool,
    policy: Option<&little_monkey_lib::run_protocol::ChannelSendPolicy>,
) -> SendAuthority {
    let route_reply = std::env::var(JOB_ID_ENV)
        .ok()
        .filter(|id| !id.is_empty())
        .and_then(|job_id| {
            let paths = DaemonPaths::resolve().ok()?;
            let store = DaemonStore::open(&paths).ok()?;
            store.ingress_reply_grant_for_job(&job_id).ok().flatten()
        })
        .unwrap_or(false);
    authority_from_grants(route_reply, allow_external_mutations, policy)
}

/// [`send_authority`] with the store and job injected: the reply grant comes
/// from the frozen route recorded on the durable turn, never from anything
/// the run says about itself.
pub(crate) fn send_authority_for_job(
    store: &DaemonStore,
    job_id: &str,
    allow_external_mutations: bool,
    policy: Option<&little_monkey_lib::run_protocol::ChannelSendPolicy>,
) -> SendAuthority {
    let route_reply = store
        .ingress_reply_grant_for_job(job_id)
        .ok()
        .flatten()
        .unwrap_or(false);
    authority_from_grants(route_reply, allow_external_mutations, policy)
}

fn authority_from_grants(
    route_reply: bool,
    allow_external_mutations: bool,
    policy: Option<&little_monkey_lib::run_protocol::ChannelSendPolicy>,
) -> SendAuthority {
    SendAuthority {
        reply: allow_external_mutations || route_reply,
        cross_conversation: policy.map(|p| p.cross_conversation).unwrap_or(false),
        accounts: policy.map(|p| p.accounts.clone()).unwrap_or_default(),
    }
}

/// Which authority rule let a send through. Logged with the decision, so an
/// operator reading the daemon log can see not just that a message left but
/// what allowed it to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendRule {
    OriginReply,
    CrossConversation,
    CrossAccount,
}

impl SendRule {
    fn as_str(self) -> &'static str {
        match self {
            SendRule::OriginReply => "origin_reply",
            SendRule::CrossConversation => "cross_conversation",
            SendRule::CrossAccount => "cross_account",
        }
    }
}

/// Longest destination/thread/reply id this tool will accept. Provider ids
/// are far shorter; the cap only keeps a runaway model from writing junk into
/// the outbox columns.
const MAX_DESTINATION_CHARS: usize = 512;

/// Queue one message through the durable outbox.
///
/// Returns the JSON the tool loop hands back to the model: the queue status
/// and the durable outbox id, and nothing about the transport or credentials.
///
/// Split in two on purpose. Everything that decides *whether* this send is
/// allowed — [`plan_send`] — runs before any store is opened, so a refusal
/// never touches the daemon's database; [`queue_send`] then writes the row.
/// The seam is also what lets the whole path be exercised against a store a
/// test owns rather than the operator's real one.
pub(crate) fn send_message(
    request: &ChannelSendRequest,
    authority: &SendAuthority,
) -> Result<serde_json::Value, String> {
    let job_id = std::env::var(JOB_ID_ENV).ok().filter(|id| !id.is_empty());
    let origin = current_channel_origin().map(|(_, origin)| origin);
    let plan = plan_send(request, authority, origin.as_ref())?;

    let paths = DaemonPaths::resolve()?;
    let mut store = DaemonStore::open(&paths)?;
    queue_send(
        &mut store,
        &paths,
        request,
        &plan,
        origin.as_ref(),
        job_id.as_deref(),
        now_ms()?,
    )
}

/// A send that has passed every check that does not need the store: what it
/// is asking for, and which grant allowed it.
#[derive(Debug, Clone)]
pub(crate) struct SendPlan {
    destination: Destination,
    rule: SendRule,
}

/// Validate the request, resolve the destination against the run's origin, and
/// check the authority. No I/O: what is being asked for has to be concrete
/// before "may this run do that" means anything.
pub(crate) fn plan_send(
    request: &ChannelSendRequest,
    authority: &SendAuthority,
    origin: Option<&ChannelOrigin>,
) -> Result<SendPlan, String> {
    let text = request.text.trim();
    if text.is_empty() && request.artifact_ids.is_empty() {
        return Err("A message must contain some text.".to_string());
    }
    if text.chars().count() > MAX_REPLY_CHARS {
        return Err(format!(
            "A message must be at most {MAX_REPLY_CHARS} characters; this one is {}.",
            text.chars().count()
        ));
    }
    for (field, value) in [
        ("account", request.account_id.as_deref()),
        ("to", request.conversation_id.as_deref()),
        ("thread", request.thread_id.as_deref()),
        ("reply_to", request.reply_to_provider_id.as_deref()),
    ] {
        if let Some(value) = value {
            if value.trim().is_empty() || value.chars().count() > MAX_DESTINATION_CHARS {
                return Err(format!(
                    "'{field}' must be a non-empty id of at most {MAX_DESTINATION_CHARS} characters."
                ));
            }
        }
    }

    let destination = resolve_destination(request, origin)?;
    let origin_on_this_account =
        origin.filter(|origin| origin.account_id == destination.account_id);
    let rule = authorize(
        &destination.account_id,
        destination.is_origin_reply,
        origin_on_this_account.is_some(),
        authority,
    )
    .map_err(|reason| {
        refuse(
            &destination.account_id,
            &destination.conversation_id,
            reason,
        )
    })?;
    Ok(SendPlan { destination, rule })
}

/// Write the planned send into the durable outbox.
///
/// Everything a provider will need is decided here and nowhere later: the
/// files are copied, the idempotency key is derived from the job, and the row
/// is the only record of the send. This function never calls an adapter —
/// delivery is the outbox worker's job, so a crash between "the model said it"
/// and "the provider has it" resolves the same way every other outbound
/// message does.
pub(crate) fn queue_send(
    store: &mut DaemonStore,
    paths: &DaemonPaths,
    request: &ChannelSendRequest,
    plan: &SendPlan,
    origin: Option<&ChannelOrigin>,
    job_id: Option<&str>,
    now_ms: i64,
) -> Result<serde_json::Value, String> {
    let text = request.text.trim();
    let has_files = !request.artifact_ids.is_empty();
    let Destination {
        account_id,
        conversation_id,
        is_origin_reply,
    } = plan.destination.clone();
    let origin_on_this_account = origin.filter(|origin| origin.account_id == account_id);
    let account = store
        .channel_account(&account_id)?
        .ok_or_else(|| "No configured account has that id.".to_string())?;
    if !account.enabled {
        return Err("That account is disabled, so nothing was queued.".to_string());
    }
    if has_files && !super::adapters::sends_attachments(account.kind) {
        return Err(format!(
            "Little Monkey cannot send files on {} yet, so nothing was queued.",
            account.kind.label()
        ));
    }
    // The account's own configured limits, bounded by the ceilings nothing may
    // raise — the reply-wide file cap and the artifact store's blob cap. All
    // checked before the row is written, so a request over a deterministic
    // limit never becomes durable.
    let limits = super::channel_adapter::AttachmentLimits::for_account(&account.non_secret_config);
    let max_files = limits.max_listed.min(MAX_ATTACHMENTS_PER_REPLY);
    if request.artifact_ids.len() > max_files {
        return Err(format!(
            "A message on this account may carry at most {max_files} files; this one asked for {}.",
            request.artifact_ids.len()
        ));
    }
    let max_bytes = limits.max_bytes.min(MAX_ATTACHMENT_BYTES);
    let mut attachments = resolve_artifacts(paths, &request.artifact_ids, max_bytes)?;
    // An artifact id names bytes and nothing else, so a file forwarded by id
    // would otherwise leave as "attachment" with no type. What this run was
    // *sent* is the one place a real name and type for those bytes exists.
    if let Some(job_id) = job_id {
        name_known_artifacts(store, job_id, &mut attachments);
    }

    // Thread and reply target default from the origin only on an origin
    // reply; an explicit destination gets exactly what was asked, because
    // origin ids are meaningless in another conversation.
    let thread_id = request.thread_id.clone().or_else(|| {
        is_origin_reply
            .then(|| origin_on_this_account.and_then(|origin| origin.thread_id.clone()))
            .flatten()
    });
    let reply_to_provider_id = request.reply_to_provider_id.clone().or_else(|| {
        is_origin_reply
            .then(|| origin_on_this_account.map(|origin| origin.provider_event_id.clone()))
            .flatten()
    });

    // The depth of the message being answered plus one, so an exchange between
    // two automated systems is bounded rather than perpetual. A run with no
    // conversation origin starts a chain rather than continuing one.
    let reply_depth = match job_id {
        Some(job_id) if origin.is_some() => inbound_reply_depth(store, job_id).saturating_add(1),
        _ => 0,
    };
    let idempotency_key = match job_id {
        Some(job_id) => {
            let sequence = store.outbox_count_for_job(job_id)?;
            let prefix = if is_origin_reply { "reply" } else { "send" };
            format!("{prefix}-{job_id}-{sequence}")
        }
        // No durable job to be retried under: a fresh key per call is the
        // honest statement that nothing will ever legitimately resubmit it.
        None => format!("send-adhoc-{}", uuid::Uuid::new_v4().simple()),
    };

    let payload = OutboxPayload {
        message: OutboundMessage {
            account_id: account_id.clone(),
            kind: account.kind,
            conversation_id: conversation_id.clone(),
            thread_id: thread_id.clone(),
            text: text.to_string(),
            attachments,
            reply_to_provider_id: reply_to_provider_id.clone(),
            idempotency_key: idempotency_key.clone(),
        },
        reply_depth,
    };
    let payload_json = serde_json::to_string(&payload).map_err(|error| error.to_string())?;

    let queued = store.enqueue_channel_message(&NewOutboxMessage {
        account_id: account_id.clone(),
        conversation_id: conversation_id.clone(),
        thread_id,
        reply_to_provider_id,
        payload_digest: sha256_hex(payload_json.as_bytes()),
        payload_json,
        idempotency_key,
        max_attempts: REPLY_MAX_ATTEMPTS,
        job_id: job_id.map(str::to_string),
        created_at_ms: now_ms,
    })?;

    let outbox_id = match &queued {
        OutboxEnqueue::Queued { outbox_id } | OutboxEnqueue::AlreadyQueued { outbox_id } => {
            outbox_id.clone()
        }
    };
    // The authorization decision, durably in the daemon log: which rule
    // allowed it and where it went. Ids only — never the text.
    eprintln!(
        "channel-send: queued outbox_id={outbox_id} rule={} account={account_id} job={}",
        plan.rule.as_str(),
        job_id.unwrap_or("-"),
    );

    Ok(match queued {
        OutboxEnqueue::Queued { outbox_id } => serde_json::json!({
            "status": "queued",
            "outbox_id": outbox_id,
            "note": "The message is queued for durable delivery."
        }),
        OutboxEnqueue::AlreadyQueued { outbox_id } => serde_json::json!({
            "status": "already_queued",
            "outbox_id": outbox_id,
            "note": "An identical message was already queued for this run; nothing was duplicated."
        }),
    })
}

/// A resolved destination: concrete account and conversation, and whether the
/// pair is exactly where this run came from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Destination {
    account_id: String,
    conversation_id: String,
    is_origin_reply: bool,
}

/// Fill the request's omitted destination fields from the run's origin.
///
/// Pure: what is being asked for has to be concrete before "may this run do
/// that" means anything, and keeping this free of the store is what lets the
/// rules be exercised without a daemon.
fn resolve_destination(
    request: &ChannelSendRequest,
    origin: Option<&ChannelOrigin>,
) -> Result<Destination, String> {
    let account_id = match (&request.account_id, origin) {
        (Some(explicit), _) => explicit.clone(),
        (None, Some(origin)) => origin.account_id.clone(),
        (None, None) => {
            return Err(
                "This run did not arrive from a messaging conversation, so 'account' and 'to' \
                 must name the destination explicitly."
                    .to_string(),
            )
        }
    };
    let origin_on_this_account = origin.filter(|origin| origin.account_id == account_id);
    let conversation_id = match (&request.conversation_id, origin_on_this_account) {
        (Some(explicit), _) => explicit.clone(),
        (None, Some(origin)) => origin.conversation_id.clone(),
        (None, None) => {
            return Err(
                "'to' must name the destination conversation when sending through an account \
                 this run did not arrive on."
                    .to_string(),
            )
        }
    };
    let is_origin_reply =
        origin_on_this_account.is_some_and(|origin| origin.conversation_id == conversation_id);
    Ok(Destination {
        account_id,
        conversation_id,
        is_origin_reply,
    })
}

/// The authorization ladder. Each refusal names the missing grant rather than
/// a generic "denied", because the operator reading it in the transcript is
/// the one who could add the grant.
fn authorize(
    account_id: &str,
    is_origin_reply: bool,
    origin_on_this_account: bool,
    authority: &SendAuthority,
) -> Result<SendRule, &'static str> {
    let account_granted = authority.accounts.iter().any(|id| id == account_id);
    if is_origin_reply {
        if authority.reply {
            return Ok(SendRule::OriginReply);
        }
        return Err("This run was not granted the authority to answer its conversation.");
    }
    if origin_on_this_account && (authority.cross_conversation || account_granted) {
        return Ok(SendRule::CrossConversation);
    }
    if !origin_on_this_account && account_granted {
        return Ok(SendRule::CrossAccount);
    }
    Err("This run's permission snapshot does not grant that destination.")
}

/// One refusal, logged and returned. The log line carries the ids so the
/// operator can see what was attempted; the returned text tells the model why
/// without teaching it anything new about the configuration.
fn refuse(account_id: &str, conversation_id: &str, reason: &str) -> String {
    eprintln!(
        "channel-send: refused account={account_id} conversation={conversation_id} reason={reason}"
    );
    reason.to_string()
}

/// Reference already-durable artifacts by id.
///
/// Nothing is copied and nothing is read into memory — the id names bytes the
/// content store already holds, and the size check reads the blob's metadata.
/// What is checked: the id is a well-formed digest, the blob exists, and it is
/// inside `max_bytes` — the account's configured limit already bounded by the
/// application ceiling.
fn resolve_artifacts(
    paths: &DaemonPaths,
    artifact_ids: &[String],
    max_bytes: u64,
) -> Result<Vec<OutboundAttachment>, String> {
    if artifact_ids.is_empty() {
        return Ok(Vec::new());
    }
    let app_data = paths
        .root
        .parent()
        .ok_or_else(|| "Daemon root has no app-data parent".to_string())?;
    let store = little_monkey_lib::artifact_store::ArtifactStore::with_max_blob_size(
        app_data.join("content-v1"),
        MAX_ATTACHMENT_BYTES,
    )
    .map_err(|error| format!("Failed to open the content store: {error}"))?;
    let mut resolved = Vec::with_capacity(artifact_ids.len());
    for id in artifact_ids {
        if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("'{id}' is not an artifact id."));
        }
        let path = store
            .blob_path(id)
            .map_err(|error| format!("'{id}': {error}"))?;
        let metadata = std::fs::metadata(&path)
            .map_err(|_| format!("There is no stored artifact with id '{id}'."))?;
        if metadata.len() > max_bytes {
            return Err(format!(
                "Artifact '{id}' is {} bytes; the limit for one attachment on this account is {max_bytes}.",
                metadata.len()
            ));
        }
        resolved.push(OutboundAttachment {
            artifact_id: id.clone(),
            filename: None,
            mime_type: None,
        });
    }
    Ok(resolved)
}

/// How many files one reply may carry, no matter what the account configures.
const MAX_ATTACHMENTS_PER_REPLY: usize = 4;

/// Give a forwarded artifact back the filename and type it arrived with.
///
/// Only for attachments that have no name yet. The lookup is the run's own
/// inbound envelope, so an id this conversation never received simply stays
/// unnamed rather than borrowing another message's metadata.
fn name_known_artifacts(store: &DaemonStore, job_id: &str, attachments: &mut [OutboundAttachment]) {
    if attachments
        .iter()
        .all(|attachment| attachment.filename.is_some() && attachment.mime_type.is_some())
    {
        return;
    }
    let Ok(Some(envelope_json)) = store.inbound_envelope_for_job(job_id) else {
        return;
    };
    let Ok(envelope) = serde_json::from_str::<ChannelEnvelope>(&envelope_json) else {
        return;
    };
    for attachment in attachments.iter_mut() {
        let Some(known) = envelope.attachments.iter().find(|inbound| {
            inbound.stored_artifact_id.as_deref() == Some(attachment.artifact_id.as_str())
        }) else {
            continue;
        };
        if attachment.filename.is_none() {
            attachment.filename = known.filename.clone();
        }
        if attachment.mime_type.is_none() {
            attachment.mime_type = known.mime_type.clone();
        }
    }
}

/// Depth of the message being answered.
///
/// Recomputed from the stored inbound envelope rather than carried in the
/// environment, for the same reason the destination is: the model's process
/// must not be able to influence the number that bounds an automated chain.
fn inbound_reply_depth(store: &DaemonStore, job_id: &str) -> u32 {
    let Ok(Some(envelope_json)) = store.inbound_envelope_for_job(job_id) else {
        return 0;
    };
    let Ok(envelope) = serde_json::from_str::<ChannelEnvelope>(&envelope_json) else {
        return 0;
    };
    super::channel_ingress::inherited_reply_depth(store, &envelope).unwrap_or(0)
}

fn now_ms() -> Result<i64, String> {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "System clock is before the Unix epoch".to_string())?
            .as_millis(),
    )
    .map_err(|_| "System clock is beyond the supported range".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::artifact_store::ArtifactStore;

    fn request(text: &str) -> ChannelSendRequest {
        ChannelSendRequest {
            text: text.to_string(),
            ..ChannelSendRequest::default()
        }
    }

    fn reply_authority() -> SendAuthority {
        SendAuthority {
            reply: true,
            ..SendAuthority::default()
        }
    }

    #[test]
    fn an_empty_reply_is_refused_before_anything_is_opened() {
        assert!(send_message(&request("   "), &reply_authority()).is_err());
    }

    #[test]
    fn an_oversized_reply_is_refused() {
        let huge = "x".repeat(MAX_REPLY_CHARS + 1);
        let error = send_message(&request(&huge), &reply_authority()).expect_err("too long");
        assert!(error.contains("at most"));
    }

    #[test]
    fn a_blank_destination_id_is_refused() {
        let mut asked = request("hello");
        asked.conversation_id = Some("   ".to_string());
        let error = send_message(&asked, &reply_authority()).expect_err("blank id");
        assert!(error.contains("'to'"), "{error}");
    }

    #[test]
    fn an_oversized_destination_id_is_refused() {
        let mut asked = request("hello");
        asked.account_id = Some("x".repeat(MAX_DESTINATION_CHARS + 1));
        let error = send_message(&asked, &reply_authority()).expect_err("oversized id");
        assert!(error.contains("'account'"), "{error}");
    }

    #[test]
    fn only_the_providers_with_a_real_upload_accept_files() {
        use little_monkey_lib::channels::types::ChannelKind;
        assert!(super::super::adapters::sends_attachments(
            ChannelKind::Telegram
        ));
        assert!(super::super::adapters::sends_attachments(
            ChannelKind::WhatsApp
        ));
        for kind in [
            ChannelKind::Matrix,
            ChannelKind::Signal,
            ChannelKind::Slack,
            ChannelKind::Discord,
            ChannelKind::Mattermost,
        ] {
            assert!(super::super::adapters::sends_attachments(kind), "{kind:?}");
        }
        // Inbound attachments are normalized for these, but nothing uploads
        // one, and the tool refuses rather than queueing a reply that would
        // arrive with the file missing.
        for kind in [
            ChannelKind::IMessage,
            ChannelKind::Teams,
            ChannelKind::Line,
            ChannelKind::GoogleChat,
            ChannelKind::Irc,
        ] {
            assert!(!super::super::adapters::sends_attachments(kind), "{kind:?}");
        }
    }

    #[test]
    fn a_run_with_no_channel_origin_has_nowhere_to_send() {
        // No job id in the environment: every non-channel run looks like this.
        std::env::remove_var(JOB_ID_ENV);
        let error = send_message(&request("hello"), &reply_authority()).expect_err("no origin");
        assert!(error.contains("did not arrive from a messaging conversation"));
    }

    #[test]
    fn an_explicit_account_without_the_grant_is_refused() {
        std::env::remove_var(JOB_ID_ENV);
        let mut asked = request("hello");
        asked.account_id = Some("chan-someone-elses".to_string());
        asked.conversation_id = Some("conv-1".to_string());
        let error = send_message(&asked, &reply_authority()).expect_err("no grant");
        assert!(error.contains("does not grant"), "{error}");
    }

    fn origin() -> ChannelOrigin {
        ChannelOrigin {
            account_id: "chan-origin".to_string(),
            conversation_id: "conv-origin".to_string(),
            thread_id: None,
            provider_event_id: "msg-1".to_string(),
        }
    }

    #[test]
    fn omitted_fields_resolve_to_the_origin() {
        let resolved = resolve_destination(&request("hi"), Some(&origin())).expect("resolved");
        assert_eq!(resolved.account_id, "chan-origin");
        assert_eq!(resolved.conversation_id, "conv-origin");
        assert!(resolved.is_origin_reply);
    }

    #[test]
    fn an_explicit_conversation_on_the_origin_account_is_not_a_reply() {
        let mut asked = request("hi");
        asked.conversation_id = Some("conv-other".to_string());
        let resolved = resolve_destination(&asked, Some(&origin())).expect("resolved");
        assert!(!resolved.is_origin_reply);
    }

    #[test]
    fn another_account_requires_an_explicit_conversation() {
        let mut asked = request("hi");
        asked.account_id = Some("chan-second".to_string());
        let error = resolve_destination(&asked, Some(&origin())).expect_err("no conversation");
        assert!(error.contains("'to'"), "{error}");
    }

    #[test]
    fn the_reply_grant_covers_only_the_origin_conversation() {
        let authority = reply_authority();
        assert_eq!(
            authorize("chan-origin", true, true, &authority),
            Ok(SendRule::OriginReply)
        );
        assert!(authorize("chan-origin", false, true, &authority).is_err());
        assert!(authorize("chan-second", false, false, &authority).is_err());
    }

    #[test]
    fn cross_conversation_needs_its_own_grant() {
        let authority = SendAuthority {
            reply: true,
            cross_conversation: true,
            accounts: Vec::new(),
        };
        assert_eq!(
            authorize("chan-origin", false, true, &authority),
            Ok(SendRule::CrossConversation)
        );
        // The grant is scoped to the origin account: another account still
        // needs its own entry.
        assert!(authorize("chan-second", false, false, &authority).is_err());
    }

    #[test]
    fn a_named_account_grant_reaches_exactly_that_account() {
        let authority = SendAuthority {
            reply: false,
            cross_conversation: false,
            accounts: vec!["chan-second".to_string()],
        };
        assert_eq!(
            authorize("chan-second", false, false, &authority),
            Ok(SendRule::CrossAccount)
        );
        assert!(authorize("chan-third", false, false, &authority).is_err());
        // Knowing an account id is not authority to use it, and the reply
        // grant does not leak into other accounts.
        assert!(authorize("chan-origin", true, true, &authority).is_err());
    }

    #[test]
    fn a_malformed_artifact_id_is_refused() {
        let base =
            std::env::temp_dir().join(format!("lm-artifact-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&base).expect("app data root");
        // resolve_artifacts only needs `root` to find app-data's content store.
        let paths = DaemonPaths::under(&base);
        let error = resolve_artifacts(&paths, &["not-a-digest".to_string()], MAX_ATTACHMENT_BYTES)
            .expect_err("malformed artifact id");
        assert!(error.contains("not an artifact id"), "{error}");
    }

    #[test]
    fn a_missing_artifact_is_named() {
        let base =
            std::env::temp_dir().join(format!("lm-artifact-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&base).expect("app data root");
        let paths = DaemonPaths::under(&base);
        let id = "a".repeat(64);
        let error =
            resolve_artifacts(&paths, &[id], MAX_ATTACHMENT_BYTES).expect_err("missing artifact");
        assert!(error.contains("no stored artifact"), "{error}");
    }

    #[test]
    fn an_existing_artifact_resolves_by_reference() {
        let base =
            std::env::temp_dir().join(format!("lm-artifact-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&base).expect("app data root");
        let paths = DaemonPaths::under(&base);
        let store =
            ArtifactStore::with_max_blob_size(base.join("content-v1"), MAX_ATTACHMENT_BYTES)
                .expect("store");
        let blob = store.put(b"the chart").expect("blob");
        let resolved =
            resolve_artifacts(&paths, &[blob.id.clone()], MAX_ATTACHMENT_BYTES).expect("resolved");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].artifact_id, blob.id);
    }

    #[test]
    fn an_artifact_over_the_accounts_own_limit_is_refused() {
        // The stored blob is fine by the application ceiling; it is the
        // account's configured limit — already folded into `max_bytes` by the
        // caller — that refuses it. Checked from metadata: the bytes are
        // never read.
        let base =
            std::env::temp_dir().join(format!("lm-artifact-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&base).expect("app data root");
        let paths = DaemonPaths::under(&base);
        let store =
            ArtifactStore::with_max_blob_size(base.join("content-v1"), MAX_ATTACHMENT_BYTES)
                .expect("store");
        let blob = store.put(b"nine bytes").expect("blob");
        let error =
            resolve_artifacts(&paths, &[blob.id.clone()], 4).expect_err("over the account limit");
        assert!(error.contains("on this account"), "{error}");
        resolve_artifacts(&paths, &[blob.id], MAX_ATTACHMENT_BYTES)
            .expect("the same blob is fine under the default limit");
    }

    #[test]
    fn the_tool_takes_no_filesystem_path() {
        // The universal contract is artifact-only: the model-visible schema
        // must offer no way to name a file on this machine.
        let schema = little_monkey_lib::agent_tools::send_message_tool_def();
        let properties = &schema["function"]["parameters"]["properties"];
        assert!(properties.get("artifacts").is_some());
        assert!(properties.get("attachments").is_none());
        assert!(properties.get("paths").is_none());
        assert_eq!(
            schema["function"]["parameters"]["additionalProperties"],
            false
        );
    }
}
