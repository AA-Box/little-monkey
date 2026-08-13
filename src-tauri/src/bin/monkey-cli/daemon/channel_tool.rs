//! The `send_message` agent tool.
//!
//! A run that arrived from a messaging channel can answer it. That is the
//! entire capability, and the shape of this file is what keeps it that small:
//!
//! - The destination is read from the durable event that produced the job, not
//!   from a tool argument. A model cannot be talked into replying somewhere
//!   else by the message it is reading, because there is no parameter for it.
//! - The reply is queued into the outbox rather than sent here. The tool
//!   returns as soon as the row is durable, so a crash between "the model said
//!   it" and "the provider has it" resolves the same way every other outbound
//!   message does.
//! - The idempotency key is derived from the job and the number of replies it
//!   has already queued, so a retried run cannot duplicate a reply.
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
const JOB_ID_ENV: &str = "LITTLE_MONKEY_DAEMON_JOB_ID";

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

/// Queue one reply to the conversation this run came from.
///
/// Returns the JSON the tool loop hands back to the model. Deliberately terse:
/// the model is told the reply is queued and nothing about the transport, the
/// account, or the recipient — none of which it needs, and all of which would
/// be new material for it to try to act on.
pub(crate) fn send_message(
    text: &str,
    attachment_paths: &[String],
) -> Result<serde_json::Value, String> {
    let text = text.trim();
    if text.is_empty() && attachment_paths.is_empty() {
        return Err("A reply must contain some text.".to_string());
    }
    if text.chars().count() > MAX_REPLY_CHARS {
        return Err(format!(
            "A reply must be at most {MAX_REPLY_CHARS} characters; this one is {}.",
            text.chars().count()
        ));
    }

    let Some((job_id, origin)) = current_channel_origin() else {
        return Err(
            "This run did not arrive from a messaging conversation, so there is nowhere to send a message."
                .to_string(),
        );
    };

    let paths = DaemonPaths::resolve()?;
    let mut store = DaemonStore::open(&paths)?;
    let account = store
        .channel_account(&origin.account_id)?
        .ok_or_else(|| "The account this conversation belongs to no longer exists.".to_string())?;
    if !account.enabled {
        return Err("The account this conversation belongs to is disabled.".to_string());
    }
    // Refused before the files are read, not after: an agent that cannot send
    // a file on this provider should not have caused it to be copied anywhere.
    if !attachment_paths.is_empty() && !super::adapters::sends_attachments(account.kind) {
        return Err(format!(
            "Little Monkey cannot send files on {} yet, so nothing was queued.",
            account.kind.label()
        ));
    }
    let attachments = import_attachments(&paths, attachment_paths)?;

    // The depth of the message being answered plus one, so an exchange between
    // two automated systems is bounded rather than perpetual.
    let reply_depth = inbound_reply_depth(&store, &job_id).saturating_add(1);
    let sequence = store.outbox_count_for_job(&job_id)?;
    let idempotency_key = format!("reply-{job_id}-{sequence}");

    let payload = OutboxPayload {
        message: OutboundMessage {
            account_id: origin.account_id.clone(),
            kind: account.kind,
            conversation_id: origin.conversation_id.clone(),
            thread_id: origin.thread_id.clone(),
            text: text.to_string(),
            attachments,
            reply_to_provider_id: Some(origin.provider_event_id.clone()),
            idempotency_key: idempotency_key.clone(),
        },
        reply_depth,
    };
    let payload_json = serde_json::to_string(&payload).map_err(|error| error.to_string())?;

    let queued = store.enqueue_channel_message(&NewOutboxMessage {
        account_id: origin.account_id,
        conversation_id: origin.conversation_id,
        thread_id: origin.thread_id,
        reply_to_provider_id: Some(origin.provider_event_id),
        payload_digest: sha256_hex(payload_json.as_bytes()),
        payload_json,
        idempotency_key,
        max_attempts: REPLY_MAX_ATTEMPTS,
        job_id: Some(job_id),
        created_at_ms: now_ms()?,
    })?;

    Ok(match queued {
        OutboxEnqueue::Queued { .. } => serde_json::json!({
            "status": "queued",
            "note": "The reply is queued for delivery to the originating conversation."
        }),
        OutboxEnqueue::AlreadyQueued { .. } => serde_json::json!({
            "status": "already_queued",
            "note": "An identical reply was already queued for this run; nothing was duplicated."
        }),
    })
}

/// How many files one reply may carry.
const MAX_ATTACHMENTS_PER_REPLY: usize = 4;

/// Copy the files an agent asked to attach into the content store.
///
/// Sending a file to an outside conversation is the one part of this tool that
/// could move data off the machine, so what it will accept is deliberately
/// narrow:
///
/// - Paths are resolved against the run's own working directory, and the
///   canonical result must still be inside it. `../../.ssh/id_rsa`, an absolute
///   path, and a symlink pointing out of the workspace all fail the same check,
///   because all three are compared after resolution rather than as text.
/// - Only regular files. A directory, a device node or a fifo is refused rather
///   than read.
/// - Each file is capped at [`MAX_ATTACHMENT_BYTES`], and a reply at
///   [`MAX_ATTACHMENTS_PER_REPLY`] files.
///
/// The bytes are copied now, not referenced: the outbox may retry this row
/// minutes later, and the file the agent meant is the one that existed when it
/// said so.
fn import_attachments(
    paths: &DaemonPaths,
    requested: &[String],
) -> Result<Vec<OutboundAttachment>, String> {
    if requested.is_empty() {
        return Ok(Vec::new());
    }
    let root = std::env::current_dir()
        .and_then(|directory| directory.canonicalize())
        .map_err(|_| "This run has no working directory to attach files from.".to_string())?;
    let app_data = paths
        .root
        .parent()
        .ok_or_else(|| "Daemon root has no app-data parent".to_string())?;
    let store = little_monkey_lib::artifact_store::ArtifactStore::with_max_blob_size(
        app_data.join("content-v1"),
        MAX_ATTACHMENT_BYTES,
    )
    .map_err(|error| format!("Failed to open the content store: {error}"))?;
    import_from(&root, &store, requested)
}

/// The confinement and size rules, separated from where the run's directory and
/// the content store come from so they can be exercised without a daemon.
fn import_from(
    root: &std::path::Path,
    store: &little_monkey_lib::artifact_store::ArtifactStore,
    requested: &[String],
) -> Result<Vec<OutboundAttachment>, String> {
    if requested.len() > MAX_ATTACHMENTS_PER_REPLY {
        return Err(format!(
            "A reply may carry at most {MAX_ATTACHMENTS_PER_REPLY} files; this one asked for {}.",
            requested.len()
        ));
    }
    let mut imported = Vec::with_capacity(requested.len());
    for path in requested {
        let resolved = root
            .join(path)
            .canonicalize()
            .map_err(|_| format!("There is no file at '{path}' in this run's directory."))?;
        if !resolved.starts_with(root) {
            return Err(format!(
                "'{path}' is outside this run's directory, so it cannot be attached."
            ));
        }
        let metadata = std::fs::symlink_metadata(&resolved)
            .map_err(|_| format!("'{path}' could not be read."))?;
        if !metadata.is_file() {
            return Err(format!("'{path}' is not a regular file."));
        }
        if metadata.len() > MAX_ATTACHMENT_BYTES {
            return Err(format!(
                "'{path}' is {} bytes; the limit for one attachment is {MAX_ATTACHMENT_BYTES}.",
                metadata.len()
            ));
        }
        let blob = store
            .import_file(&resolved)
            .map_err(|error| format!("Failed to store '{path}': {error}"))?;
        imported.push(OutboundAttachment {
            artifact_id: blob.id,
            filename: resolved
                .file_name()
                .map(|name| name.to_string_lossy().to_string()),
            mime_type: None,
        });
    }
    Ok(imported)
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

    #[test]
    fn an_empty_reply_is_refused_before_anything_is_opened() {
        assert!(send_message("   ", &[]).is_err());
    }

    #[test]
    fn an_oversized_reply_is_refused() {
        let huge = "x".repeat(MAX_REPLY_CHARS + 1);
        let error = send_message(&huge, &[]).expect_err("too long");
        assert!(error.contains("at most"));
    }

    /// A run directory with one file in it, plus a content store to import
    /// into. Both live in the same temp dir so nothing here touches the real
    /// daemon or the shared ledger.
    fn workspace() -> (std::path::PathBuf, std::path::PathBuf, ArtifactStore) {
        let base =
            std::env::temp_dir().join(format!("lm-attach-{}", uuid::Uuid::new_v4().simple()));
        let root = base.join("run");
        std::fs::create_dir_all(&root).expect("run dir");
        std::fs::write(root.join("report.txt"), b"the build passed").expect("file");
        let store =
            ArtifactStore::with_max_blob_size(base.join("content-v1"), MAX_ATTACHMENT_BYTES)
                .expect("store");
        let base = base.canonicalize().expect("canonical base");
        let root = root.canonicalize().expect("canonical run dir");
        (base, root, store)
    }

    #[test]
    fn a_file_in_the_run_directory_is_copied_into_the_content_store() {
        let (_base, root, store) = workspace();
        let imported = import_from(&root, &store, &["report.txt".to_string()]).expect("imported");
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].filename.as_deref(), Some("report.txt"));
        // Copied, not referenced: the bytes survive the original being replaced.
        std::fs::write(root.join("report.txt"), b"something else").expect("overwrite");
        assert_eq!(
            store.read(&imported[0].artifact_id).expect("read back"),
            b"the build passed"
        );
    }

    #[test]
    fn a_path_climbing_out_of_the_run_directory_is_refused() {
        let (base, root, store) = workspace();
        std::fs::write(base.join("secret"), b"private key").expect("outside file");
        let error = import_from(&root, &store, &["../secret".to_string()]).expect_err("refused");
        assert!(error.contains("outside this run's directory"), "{error}");
    }

    #[test]
    fn an_absolute_path_elsewhere_is_refused() {
        let (base, root, store) = workspace();
        let outside = base.join("secret");
        std::fs::write(&outside, b"private key").expect("outside file");
        let error =
            import_from(&root, &store, &[outside.display().to_string()]).expect_err("refused");
        assert!(error.contains("outside this run's directory"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_pointing_out_of_the_run_directory_is_refused() {
        // The name is inside the workspace; the file is not. Comparing the
        // resolved path rather than the requested one is what catches this.
        let (base, root, store) = workspace();
        let outside = base.join("secret");
        std::fs::write(&outside, b"private key").expect("outside file");
        std::os::unix::fs::symlink(&outside, root.join("innocent.txt")).expect("symlink");
        let error = import_from(&root, &store, &["innocent.txt".to_string()]).expect_err("refused");
        assert!(error.contains("outside this run's directory"), "{error}");
    }

    #[test]
    fn a_directory_is_not_a_file() {
        let (_base, root, store) = workspace();
        std::fs::create_dir(root.join("logs")).expect("dir");
        let error = import_from(&root, &store, &["logs".to_string()]).expect_err("refused");
        assert!(error.contains("not a regular file"), "{error}");
    }

    #[test]
    fn a_missing_file_is_named_rather_than_silently_dropped() {
        let (_base, root, store) = workspace();
        let error = import_from(&root, &store, &["nope.txt".to_string()]).expect_err("refused");
        assert!(error.contains("no file at 'nope.txt'"), "{error}");
    }

    #[test]
    fn more_files_than_the_cap_are_refused_before_any_are_copied() {
        let (_base, root, store) = workspace();
        let requested: Vec<String> = (0..MAX_ATTACHMENTS_PER_REPLY + 1)
            .map(|_| "report.txt".to_string())
            .collect();
        let error = import_from(&root, &store, &requested).expect_err("refused");
        assert!(error.contains("at most"), "{error}");
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
        let error = send_message("hello", &[]).expect_err("no origin");
        assert!(error.contains("did not arrive from a messaging conversation"));
    }
}
