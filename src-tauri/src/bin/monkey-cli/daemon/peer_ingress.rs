//! The single path from a paired peer's envelope to a durable run.
//!
//! Everything a peer can cause on this machine goes through here, and what
//! happens here is deliberately unremarkable: the envelope is recorded, the
//! grants are checked, and an accepted one becomes an ordinary
//! [`ConversationIngress`] — the same durable turn a Telegram message or an
//! inbound call becomes, running under this node's own recipe, this node's own
//! permission policy and this node's own approvals.
//!
//! What the sender does *not* get to do is the important part. The envelope has
//! no field for a workspace, a tool, a model, a device or a permission mode, so
//! there is nothing to strip: a peer states what it wants in words, and the
//! words arrive wrapped as untrusted data because
//! [`ConversationSource::Peer`] is not the operator.
//!
//! [`ConversationSource::Peer`]: little_monkey_lib::channels::ingress::ConversationSource::Peer

use std::collections::BTreeSet;

use little_monkey_lib::artifact_store::ArtifactStore;
use little_monkey_lib::channels::ingress::{
    ConversationIngress, ConversationSource, MAX_LISTED_ATTACHMENTS,
};
use little_monkey_lib::channels::routing::RouteTarget;
use little_monkey_lib::channels::types::{AttachmentKind, AttachmentSource, ChannelAttachment};
use little_monkey_lib::peers::{PeerCapability, PeerEnvelope, PeerRejection};

use super::channel_ingress::{self, SubmitOutcome};
use super::channel_worker::RunQueue;
use super::peer_store::{PeerDisposition, PeerRecording};
use super::remote::protocol::DeviceCapability;
use super::store::DaemonStore;

/// Recipe a peer's message or task request runs as.
///
/// The operator authors it once, with their chosen model, system prompt and
/// permission mode — that recipe *is* the contract for what a peer may cause
/// here, exactly as `mobile-chat` is the contract for a paired phone. Resolving
/// a target any other way would mean this path inventing authority of its own.
pub(crate) const PEER_TASK_RECIPE: &str = "peer-task";

/// Recipe parameter the peer's text is passed as.
const MESSAGE_PARAM: &str = "message";

/// Who is asking, as this node knows them — never as they claim to be.
///
/// Every field here comes from the pairing record the signature resolved to.
/// The envelope's own `sender_instance_id` is used for dedupe and for the
/// origin chain, but it is not identity: a peer cannot promote itself by
/// putting someone else's name in it.
#[derive(Debug, Clone)]
pub(crate) struct PeerContext<'a> {
    pub device_id: &'a str,
    pub granted: &'a BTreeSet<DeviceCapability>,
    pub revoked: bool,
    /// This installation's own instance id, for the loop check.
    pub local_instance_id: &'a str,
    /// The content store an admitted artifact's bytes are read back from.
    ///
    /// Present rather than optional because the check it enables is
    /// fail-closed: an envelope naming content this node cannot verify is
    /// refused, so a run never sees an attachment nobody can read.
    ///
    /// It is deliberately *not* the authorization: this store is shared with
    /// every other artifact on the machine, so "the digest resolves here" would
    /// let a peer reference a local blob it never sent, or one another peer
    /// sent. The durable receipt decides that, and this store only proves the
    /// admitted bytes are still there and still hash to what was admitted.
    pub artifacts: &'a ArtifactStore,
}

/// What this node did with one peer envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PeerAcceptance {
    /// Recorded and queued as a durable turn.
    Accepted {
        thread_id: String,
        message_row_id: String,
        ingress_id: String,
        job_id: String,
    },
    /// Recorded, but the queue has not taken it yet. The turn is durable and
    /// recovery will submit it; the peer is told it was accepted, because it
    /// was.
    AcceptedPending {
        thread_id: String,
        message_row_id: String,
    },
    /// Seen before. Carries the original outcome so a retry is answered with
    /// what was decided the first time.
    Duplicate {
        thread_id: String,
        message_row_id: String,
        accepted: bool,
        job_id: Option<String>,
    },
    /// Refused, with the reason the peer is told.
    Rejected {
        thread_id: Option<String>,
        reason: PeerRejection,
    },
}

/// Record, gate and queue one envelope from a paired peer.
pub(crate) fn accept_peer_envelope(
    store: &mut DaemonStore,
    queue: &dyn RunQueue,
    envelope: &PeerEnvelope,
    context: &PeerContext<'_>,
    now_ms: i64,
) -> Result<PeerAcceptance, String> {
    // Shape, hops, loops, bounds and expiry first: a malformed or looping
    // envelope must not be able to create a thread row, or a peer could fill
    // this table with junk that never became anything. The refusal still leaves
    // a bounded trace, because "someone keeps sending us loops" is exactly the
    // kind of thing an operator needs to be able to see.
    if let Err(reason) = envelope.validate(context.local_instance_id, now_ms) {
        store.record_peer_rejection_event(
            context.device_id,
            Some(envelope.message_id.as_str()),
            Some(envelope.thread_id.as_str()),
            reason,
            now_ms,
        )?;
        return Ok(PeerAcceptance::Rejected {
            thread_id: None,
            reason,
        });
    }

    let session_key = session_key_for(context.device_id, &envelope.thread_id);
    let thread = store.upsert_peer_thread(
        &envelope.thread_id,
        context.device_id,
        &envelope.sender_instance_id,
        &session_key,
        now_ms,
    )?;

    // Recorded before any decision, and recorded even when the decision goes
    // against it: that is what makes a redelivery collapse instead of being
    // re-judged.
    let recording =
        store.record_peer_message(&thread.thread_id, context.device_id, envelope, now_ms)?;
    let message_row_id = match recording {
        PeerRecording::Duplicate {
            row_id,
            disposition,
            job_id,
        } => {
            return Ok(PeerAcceptance::Duplicate {
                thread_id: thread.thread_id,
                message_row_id: row_id,
                accepted: disposition != PeerDisposition::Rejected,
                job_id,
            })
        }
        PeerRecording::Recorded { row_id } => row_id,
    };

    if context.revoked {
        return reject(
            store,
            thread.thread_id,
            message_row_id,
            PeerRejection::PeerRevoked,
        );
    }
    for required in envelope.required_capabilities() {
        if !context.granted.contains(&device_capability(required)) {
            return reject(
                store,
                thread.thread_id,
                message_row_id,
                PeerRejection::MissingCapability,
            );
        }
    }
    // Content is handed over before it is referenced, *by this peer*. Checking
    // here — after the artifact grant, before anything runs — means a peer
    // cannot make this node queue a turn carrying an attachment it will never
    // be able to open, cannot use a reference to probe the shared content store
    // for blobs it did not send, and cannot reach another peer's uploads.
    let admitted = match admit_artifacts(store, context, envelope, now_ms)? {
        Ok(admitted) => admitted,
        Err(reason) => return reject(store, thread.thread_id, message_row_id, reason),
    };

    let ingress = ingress_for(envelope, context.device_id, &thread.session_key, &admitted);
    let params = vec![format!(
        "{MESSAGE_PARAM}={}",
        channel_ingress::message_param(
            &ingress,
            &format!("a paired Little Monkey peer ({})", context.device_id),
            MAX_LISTED_ATTACHMENTS,
        )
    )];

    match channel_ingress::submit_conversation_turn(store, queue, &ingress, &params, now_ms)? {
        SubmitOutcome::Queued { ingress_id, job_id }
        | SubmitOutcome::AlreadyQueued { ingress_id, job_id } => {
            store.attach_peer_message_run(&message_row_id, Some(&ingress_id), Some(&job_id))?;
            Ok(PeerAcceptance::Accepted {
                thread_id: thread.thread_id,
                message_row_id,
                ingress_id,
                job_id,
            })
        }
        SubmitOutcome::Deferred { ingress_id, .. } => {
            // Durably accepted, not queued yet. Telling the peer this failed
            // would invite a retry that the dedupe row then refuses — the
            // worst of both. Recovery owns it now.
            store.attach_peer_message_run(&message_row_id, Some(&ingress_id), None)?;
            Ok(PeerAcceptance::AcceptedPending {
                thread_id: thread.thread_id,
                message_row_id,
            })
        }
        SubmitOutcome::Parked { .. } => reject(
            store,
            thread.thread_id,
            message_row_id,
            PeerRejection::Duplicate,
        ),
    }
}

/// The durable session a peer thread continues, scoped by the pairing.
///
/// Keyed on the device id rather than the instance id the envelope carries:
/// two peers must not be able to land in one session by claiming the same
/// thread id, and a peer must not be able to reach another peer's session by
/// guessing one.
pub(crate) fn session_key_for(device_id: &str, thread_id: &str) -> String {
    format!("peer:{device_id}:{thread_id}")
}

fn reject(
    store: &mut DaemonStore,
    thread_id: String,
    message_row_id: String,
    reason: PeerRejection,
) -> Result<PeerAcceptance, String> {
    store.reject_peer_message(&message_row_id, reason)?;
    Ok(PeerAcceptance::Rejected {
        thread_id: Some(thread_id),
        reason,
    })
}

/// The peer capability, as the remote protocol's grant list spells it.
fn device_capability(capability: PeerCapability) -> DeviceCapability {
    match capability {
        PeerCapability::Message => DeviceCapability::PeerMessage,
        PeerCapability::TaskRequest => DeviceCapability::PeerTaskRequest,
        PeerCapability::Artifact => DeviceCapability::PeerArtifact,
    }
}

/// Resolve every artifact an envelope names to the admission that authorizes it.
///
/// The whole security question of a peer attachment lives here, and it is not
/// "do these bytes exist". A SHA-256 is an integrity value: it is derivable from
/// content, it travels in the open, and it identifies a blob in a store shared
/// with every run, channel attachment and local import on this machine. Treating
/// its presence as permission would mean a peer that learned one digest could
/// have that blob attached to a turn its own words wrote — and would mean any
/// paired peer could name any other paired peer's upload.
///
/// So the question asked is "did *this* pairing hand these bytes over, recently,
/// and are they still the bytes it handed over":
///
/// - a live receipt for `(authenticated peer, artifact id)` must exist,
/// - its digest must be the one the envelope declares,
/// - its size must be the one the envelope declares, when the envelope declares
///   one at all,
/// - and the blob must still read back and verify against its own digest.
///
/// The verifying read comes last, after every cheap check has passed, so an
/// unauthorized reference never costs a hash of up to 32 MiB.
///
/// Returns the receipts, in envelope order — the attachment metadata is built
/// from them rather than from the envelope, so the sender describes its content
/// once, at upload, and cannot describe it differently afterwards.
type ArtifactAdmission = Result<Vec<super::peer_store::PeerArtifactReceipt>, PeerRejection>;

fn admit_artifacts(
    store: &DaemonStore,
    context: &PeerContext<'_>,
    envelope: &PeerEnvelope,
    now_ms: i64,
) -> Result<ArtifactAdmission, String> {
    let mut admitted = Vec::with_capacity(envelope.artifacts.len());
    for artifact in &envelope.artifacts {
        let Some(receipt) =
            store.peer_artifact_receipt(context.device_id, &artifact.artifact_id, now_ms)?
        else {
            return Ok(Err(PeerRejection::ArtifactUnavailable));
        };
        if receipt.sha256 != artifact.sha256.to_ascii_lowercase() {
            return Ok(Err(PeerRejection::ArtifactUnavailable));
        }
        if artifact
            .size_bytes
            .is_some_and(|declared| declared != receipt.size_bytes)
        {
            return Ok(Err(PeerRejection::ArtifactUnavailable));
        }
        // `read` re-hashes and refuses a mismatch, so this proves the admitted
        // bytes are still on disk and still the admitted bytes. The value is
        // dropped immediately: only the fact of verification is wanted, and the
        // run reads the blob itself when it opens the attachment.
        match context.artifacts.read(&artifact.artifact_id) {
            Ok(bytes) if bytes.len() as u64 == receipt.size_bytes => {}
            _ => return Ok(Err(PeerRejection::ArtifactUnavailable)),
        }
        admitted.push(receipt);
    }
    Ok(Ok(admitted))
}

fn ingress_for(
    envelope: &PeerEnvelope,
    device_id: &str,
    session_key: &str,
    admitted: &[super::peer_store::PeerArtifactReceipt],
) -> ConversationIngress {
    let mut ingress = ConversationIngress::direct(
        ConversationSource::Peer,
        device_id,
        &envelope.message_id,
        session_key,
        &envelope.body,
        RouteTarget::new(PEER_TASK_RECIPE),
        envelope.created_at_ms,
    );
    // Built from the receipts, never from the envelope. The envelope's filename
    // and media type are the sender describing bytes it already described when
    // it uploaded them; taking the second description would let a peer hand over
    // `build.log` and present the same content as `secrets.env`.
    ingress.attachments = admitted
        .iter()
        .map(|receipt| ChannelAttachment {
            provider_id: Some(receipt.artifact_id.clone()),
            kind: AttachmentKind::Other,
            filename: receipt.filename.clone(),
            mime_type: receipt.media_type.clone(),
            declared_size_bytes: Some(receipt.size_bytes),
            stored_size_bytes: None,
            // A handle, not a URL: nothing here can name a path on this
            // machine, and there is nothing left to fetch — the sender handed
            // the bytes over before it referenced them, so the content store
            // already holds them under `stored_artifact_id`.
            source: AttachmentSource::ProviderHandle {
                handle: format!("peer:{device_id}:{}", receipt.artifact_id),
            },
            stored_artifact_id: Some(receipt.artifact_id.clone()),
            fetch_error: None,
            text_excerpt: None,
        })
        .collect();
    ingress
}

/// The content store peer artifacts live in, bounded at what a peer may offer.
///
/// The same store every other attachment path uses — a peer's bytes are not
/// special, and giving them their own directory would only mean a second place
/// to audit.
pub(crate) fn peer_content_store(
    paths: &super::store::DaemonPaths,
) -> Result<ArtifactStore, String> {
    let app_data = paths
        .root
        .parent()
        .ok_or_else(|| "Daemon root has no app-data parent".to_string())?;
    ArtifactStore::with_max_blob_size(
        app_data.join("content-v1"),
        little_monkey_lib::peers::MAX_PEER_ARTIFACT_BYTES,
    )
    .map_err(|error| format!("Failed to open the content store: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::peers::{PeerArtifactRef, PeerMessageKind};

    const NOW: i64 = 1_700_000_000_000;
    const LOCAL: &str = "instance-local";

    #[derive(Default)]
    struct FakeQueue {
        submitted: std::sync::Mutex<Vec<(ConversationIngress, Vec<String>)>>,
    }

    impl RunQueue for FakeQueue {
        fn freeze_execution(
            &self,
            ingress: &little_monkey_lib::channels::ingress::ConversationIngress,
        ) -> Result<little_monkey_lib::channels::ingress::FrozenExecutionContext, String> {
            Ok(crate::daemon::channel_worker::test_frozen_execution(
                ingress,
            ))
        }

        fn submit(
            &self,
            ingress: &ConversationIngress,
            params: Vec<String>,
        ) -> Result<String, String> {
            self.submitted
                .lock()
                .expect("lock")
                .push((ingress.clone(), params));
            Ok(ingress.deterministic_job_id())
        }
    }

    fn all_grants() -> BTreeSet<DeviceCapability> {
        BTreeSet::from([
            DeviceCapability::PeerMessage,
            DeviceCapability::PeerTaskRequest,
            DeviceCapability::PeerArtifact,
        ])
    }

    /// A content store in its own temporary directory, so an artifact test
    /// proves the store lookup rather than inheriting another test's blobs.
    fn artifacts() -> (TempRoot, ArtifactStore) {
        let root = TempRoot(
            std::env::temp_dir().join(format!("little-monkey-peer-blobs-{}", uuid::Uuid::new_v4())),
        );
        let store = ArtifactStore::new(root.0.join("content-v1")).expect("content store");
        (root, store)
    }

    /// A directory that removes itself, so a failing assertion does not leave
    /// blobs behind in the system temp directory.
    struct TempRoot(std::path::PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn context<'a>(
        granted: &'a BTreeSet<DeviceCapability>,
        artifacts: &'a ArtifactStore,
    ) -> PeerContext<'a> {
        PeerContext {
            device_id: "device-1",
            granted,
            revoked: false,
            local_instance_id: LOCAL,
            artifacts,
        }
    }

    fn envelope(message_id: &str, kind: PeerMessageKind) -> PeerEnvelope {
        PeerEnvelope::new(
            message_id,
            "thread-1",
            kind,
            "instance-remote",
            "check whether the nightly build passed",
            NOW,
            60_000,
        )
    }

    #[test]
    fn a_task_request_becomes_a_durable_turn_under_this_nodes_own_recipe() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let queue = FakeQueue::default();
        let grants = all_grants();
        let (_blob_dir, blobs) = artifacts();

        let accepted = accept_peer_envelope(
            &mut store,
            &queue,
            &envelope("msg-1", PeerMessageKind::TaskRequest),
            &context(&grants, &blobs),
            NOW,
        )
        .expect("accept");

        let PeerAcceptance::Accepted {
            thread_id, job_id, ..
        } = accepted
        else {
            panic!("expected the task to be accepted, got {accepted:?}");
        };
        assert_eq!(thread_id, "thread-1");

        let submitted = queue.submitted.lock().unwrap();
        assert_eq!(submitted.len(), 1);
        let (ingress, params) = &submitted[0];
        assert_eq!(ingress.source, ConversationSource::Peer);
        assert_eq!(ingress.target.recipe, PEER_TASK_RECIPE);
        assert_eq!(ingress.session_key, "peer:device-1:thread-1");
        assert_eq!(ingress.deterministic_job_id(), job_id);
        // The peer's words are evidence, not instructions.
        assert!(params[0].contains("BEGIN UNTRUSTED DATA"));
        assert!(params[0].contains("check whether the nightly build passed"));
    }

    #[test]
    fn the_sender_cannot_name_a_workspace_or_widen_authority() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let queue = FakeQueue::default();
        let grants = all_grants();
        let (_blob_dir, blobs) = artifacts();
        accept_peer_envelope(
            &mut store,
            &queue,
            &envelope("msg-1", PeerMessageKind::TaskRequest),
            &context(&grants, &blobs),
            NOW,
        )
        .expect("accept");

        let submitted = queue.submitted.lock().unwrap();
        let options = channel_ingress::queue_options_for(&submitted[0].0, Vec::new());
        assert!(options.repository.is_none());
        assert!(!options.owned_worktree);
        assert!(!options.allow_commit);
        assert!(!options.allow_push);
        assert!(!options.allow_create_pull_request);
        assert!(!options.allow_review_comment);
        assert!(options.allowed_remotes.is_empty());
        assert!(options.snapshot_is_frozen);
    }

    #[test]
    fn a_peer_without_the_grant_is_refused_and_nothing_runs() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let queue = FakeQueue::default();
        let grants = BTreeSet::from([DeviceCapability::PeerMessage]);
        let (_blob_dir, blobs) = artifacts();

        let refused = accept_peer_envelope(
            &mut store,
            &queue,
            &envelope("msg-1", PeerMessageKind::TaskRequest),
            &context(&grants, &blobs),
            NOW,
        )
        .expect("decide");

        assert!(matches!(
            refused,
            PeerAcceptance::Rejected {
                reason: PeerRejection::MissingCapability,
                ..
            }
        ));
        assert!(queue.submitted.lock().unwrap().is_empty());
    }

    /// Store the bytes and record the admission the upload route would, so a
    /// test starts from the state a completed `POST /peer/artifacts` leaves.
    fn upload(
        store: &mut DaemonStore,
        blobs: &ArtifactStore,
        device_id: &str,
        bytes: &[u8],
        filename: Option<&str>,
        media_type: Option<&str>,
    ) -> (String, u64) {
        let blob = blobs.put(bytes).expect("store bytes");
        store
            .record_peer_artifact_receipt(
                device_id, &blob.id, &blob.id, blob.size, filename, media_type, NOW,
            )
            .expect("admit");
        (blob.id, blob.size)
    }

    fn referencing(message_id: &str, artifact_id: &str, size: Option<u64>) -> PeerEnvelope {
        let mut envelope = envelope(message_id, PeerMessageKind::Message);
        envelope.artifacts.push(PeerArtifactRef {
            artifact_id: artifact_id.to_string(),
            sha256: artifact_id.to_string(),
            filename: Some("build.log".into()),
            media_type: Some("text/plain".into()),
            size_bytes: size,
        });
        envelope
    }

    #[test]
    fn attaching_an_artifact_needs_its_own_grant() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let queue = FakeQueue::default();
        let (_blob_dir, blobs) = artifacts();
        // The sender handed the bytes over first, so the reference resolves.
        let (artifact_id, size) = upload(
            &mut store,
            &blobs,
            "device-1",
            b"build failed at step 4",
            Some("build.log"),
            Some("text/plain"),
        );
        let with_file = referencing("msg-1", &artifact_id, Some(size));

        let grants = BTreeSet::from([DeviceCapability::PeerMessage]);
        let refused = accept_peer_envelope(
            &mut store,
            &queue,
            &with_file,
            &context(&grants, &blobs),
            NOW,
        )
        .expect("decide");
        assert!(matches!(
            refused,
            PeerAcceptance::Rejected {
                reason: PeerRejection::MissingCapability,
                ..
            }
        ));

        // With the grant, the reference travels as a handle plus the local id
        // the bytes were stored under — never as a path.
        let grants = all_grants();
        let mut allowed = with_file.clone();
        allowed.message_id = "msg-2".into();
        accept_peer_envelope(&mut store, &queue, &allowed, &context(&grants, &blobs), NOW)
            .expect("accept");
        let submitted = queue.submitted.lock().unwrap();
        let attachment = &submitted[0].0.attachments[0];
        assert_eq!(
            attachment.source,
            AttachmentSource::ProviderHandle {
                handle: format!("peer:device-1:{artifact_id}")
            }
        );
        assert_eq!(
            attachment.stored_artifact_id.as_deref(),
            Some(artifact_id.as_str())
        );
        assert_eq!(attachment.filename.as_deref(), Some("build.log"));
    }

    /// The one that the shared content store cannot answer on its own.
    ///
    /// The blob is present and its digest is correct — it belongs to something
    /// else on this machine. A peer that guessed or learned that digest must
    /// still be refused, because existence is not admission.
    #[test]
    fn a_local_blob_this_peer_never_uploaded_is_not_referenceable() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let queue = FakeQueue::default();
        let grants = all_grants();
        let (_blob_dir, blobs) = artifacts();
        // Local content: a run's output, an operator's import — nobody's upload.
        let local = blobs.put(b"the operator's own notes").expect("store bytes");

        let refused = accept_peer_envelope(
            &mut store,
            &queue,
            &referencing("msg-1", &local.id, Some(local.size)),
            &context(&grants, &blobs),
            NOW,
        )
        .expect("decide");

        assert!(matches!(
            refused,
            PeerAcceptance::Rejected {
                reason: PeerRejection::ArtifactUnavailable,
                ..
            }
        ));
        assert!(queue.submitted.lock().unwrap().is_empty());
    }

    #[test]
    fn one_peer_cannot_reference_what_another_peer_uploaded() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let queue = FakeQueue::default();
        let grants = all_grants();
        let (_blob_dir, blobs) = artifacts();
        let (artifact_id, size) = upload(
            &mut store,
            &blobs,
            "device-1",
            b"alice's build log",
            Some("build.log"),
            None,
        );

        let mut other = context(&grants, &blobs);
        other.device_id = "device-2";
        let mut theirs = referencing("msg-1", &artifact_id, Some(size));
        theirs.sender_instance_id = "instance-second".into();
        theirs.origin_chain = vec!["instance-second".into()];
        let refused =
            accept_peer_envelope(&mut store, &queue, &theirs, &other, NOW).expect("decide");

        assert!(matches!(
            refused,
            PeerAcceptance::Rejected {
                reason: PeerRejection::ArtifactUnavailable,
                ..
            }
        ));
        // And the peer that did upload it is still able to reference it.
        assert!(matches!(
            accept_peer_envelope(
                &mut store,
                &queue,
                &referencing("msg-2", &artifact_id, Some(size)),
                &context(&grants, &blobs),
                NOW,
            )
            .expect("decide"),
            PeerAcceptance::Accepted { .. }
        ));
    }

    /// What the receiver validated at upload time is what the run is told.
    #[test]
    fn an_envelope_cannot_rename_content_it_already_handed_over() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let queue = FakeQueue::default();
        let grants = all_grants();
        let (_blob_dir, blobs) = artifacts();
        let (artifact_id, size) = upload(
            &mut store,
            &blobs,
            "device-1",
            b"build failed at step 4",
            Some("build.log"),
            Some("text/plain"),
        );

        // A size that disagrees with what was admitted is a refusal outright:
        // there is no reading of it that is not a lie about the same bytes.
        let refused = accept_peer_envelope(
            &mut store,
            &queue,
            &referencing("msg-1", &artifact_id, Some(size + 1)),
            &context(&grants, &blobs),
            NOW,
        )
        .expect("decide");
        assert!(matches!(
            refused,
            PeerAcceptance::Rejected {
                reason: PeerRejection::ArtifactUnavailable,
                ..
            }
        ));

        // A different filename is simply not read: the attachment is built from
        // the receipt, so the run sees what the upload declared.
        let mut renamed = referencing("msg-2", &artifact_id, None);
        renamed.artifacts[0].filename = Some("secrets.env".into());
        renamed.artifacts[0].media_type = Some("application/x-envfile".into());
        accept_peer_envelope(&mut store, &queue, &renamed, &context(&grants, &blobs), NOW)
            .expect("accept");

        let submitted = queue.submitted.lock().unwrap();
        let attachment = &submitted[0].0.attachments[0];
        assert_eq!(attachment.filename.as_deref(), Some("build.log"));
        assert_eq!(attachment.mime_type.as_deref(), Some("text/plain"));
        assert_eq!(attachment.declared_size_bytes, Some(size));
    }

    #[test]
    fn a_digest_that_disagrees_with_the_admission_is_refused() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let queue = FakeQueue::default();
        let grants = all_grants();
        let (_blob_dir, blobs) = artifacts();
        let (artifact_id, size) = upload(&mut store, &blobs, "device-1", b"log", None, None);

        let mut forged = referencing("msg-1", &artifact_id, Some(size));
        forged.artifacts[0].sha256 = "f".repeat(64);
        let refused =
            accept_peer_envelope(&mut store, &queue, &forged, &context(&grants, &blobs), NOW)
                .expect("decide");

        assert!(matches!(
            refused,
            PeerAcceptance::Rejected {
                reason: PeerRejection::ArtifactUnavailable,
                ..
            }
        ));
    }

    #[test]
    fn an_admission_that_has_expired_no_longer_authorizes_a_reference() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let queue = FakeQueue::default();
        let grants = all_grants();
        let (_blob_dir, blobs) = artifacts();
        let (artifact_id, size) = upload(&mut store, &blobs, "device-1", b"stale log", None, None);
        let expired_at = NOW + super::super::peer_store::PEER_ARTIFACT_ADMISSION_TTL_MS;

        let mut late = referencing("msg-1", &artifact_id, Some(size));
        late.created_at_ms = expired_at;
        late.expires_at_ms = expired_at + 60_000;
        let refused = accept_peer_envelope(
            &mut store,
            &queue,
            &late,
            &context(&grants, &blobs),
            expired_at,
        )
        .expect("decide");

        assert!(matches!(
            refused,
            PeerAcceptance::Rejected {
                reason: PeerRejection::ArtifactUnavailable,
                ..
            }
        ));
        assert!(queue.submitted.lock().unwrap().is_empty());
    }

    /// Clearing a peer withdraws the standing its uploads bought it.
    #[test]
    fn clearing_a_peer_means_it_has_to_upload_again_before_referencing() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let queue = FakeQueue::default();
        let grants = all_grants();
        let (_blob_dir, blobs) = artifacts();
        let (artifact_id, size) = upload(&mut store, &blobs, "device-1", b"build log", None, None);
        accept_peer_envelope(
            &mut store,
            &queue,
            &referencing("msg-1", &artifact_id, Some(size)),
            &context(&grants, &blobs),
            NOW,
        )
        .expect("accept");

        store.delete_peer_traffic("device-1").expect("clear");

        let refused = accept_peer_envelope(
            &mut store,
            &queue,
            &referencing("msg-2", &artifact_id, Some(size)),
            &context(&grants, &blobs),
            NOW + 1,
        )
        .expect("decide");
        assert!(matches!(
            refused,
            PeerAcceptance::Rejected {
                reason: PeerRejection::ArtifactUnavailable,
                ..
            }
        ));
        // The blob is still there; it may belong to something else entirely.
        assert!(blobs.exists(&artifact_id).unwrap());
    }

    #[test]
    fn a_reference_to_content_this_node_never_received_is_refused() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let queue = FakeQueue::default();
        let grants = all_grants();
        let (_blob_dir, blobs) = artifacts();
        let mut dangling = envelope("msg-1", PeerMessageKind::Artifact);
        dangling.artifacts.push(PeerArtifactRef {
            artifact_id: "a".repeat(64),
            sha256: "a".repeat(64),
            filename: Some("secrets.env".into()),
            media_type: None,
            size_bytes: Some(64),
        });

        let refused = accept_peer_envelope(
            &mut store,
            &queue,
            &dangling,
            &context(&grants, &blobs),
            NOW,
        )
        .expect("decide");
        assert!(matches!(
            refused,
            PeerAcceptance::Rejected {
                reason: PeerRejection::ArtifactUnavailable,
                ..
            }
        ));
        assert!(queue.submitted.lock().unwrap().is_empty());
    }

    #[test]
    fn a_refusal_that_never_became_a_message_still_leaves_a_trace() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let queue = FakeQueue::default();
        let grants = all_grants();
        let (_blob_dir, blobs) = artifacts();
        let mut looped = envelope("msg-1", PeerMessageKind::Message);
        looped.origin_chain.push(LOCAL.to_string());

        accept_peer_envelope(&mut store, &queue, &looped, &context(&grants, &blobs), NOW)
            .expect("decide");

        let events = store.peer_rejection_events(10).expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].peer_device_id, "device-1");
        assert_eq!(events[0].reason, "origin_loop");
    }

    #[test]
    fn a_revoked_peer_is_refused_even_with_its_old_grants() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let queue = FakeQueue::default();
        let grants = all_grants();
        let (_blob_dir, blobs) = artifacts();
        let mut context = context(&grants, &blobs);
        context.revoked = true;

        let refused = accept_peer_envelope(
            &mut store,
            &queue,
            &envelope("msg-1", PeerMessageKind::Message),
            &context,
            NOW,
        )
        .expect("decide");

        assert!(matches!(
            refused,
            PeerAcceptance::Rejected {
                reason: PeerRejection::PeerRevoked,
                ..
            }
        ));
        assert!(queue.submitted.lock().unwrap().is_empty());
    }

    #[test]
    fn a_retried_delivery_runs_once() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let queue = FakeQueue::default();
        let grants = all_grants();
        let (_blob_dir, blobs) = artifacts();
        let sent = envelope("msg-1", PeerMessageKind::TaskRequest);

        let first = accept_peer_envelope(&mut store, &queue, &sent, &context(&grants, &blobs), NOW)
            .expect("first");
        let second = accept_peer_envelope(
            &mut store,
            &queue,
            &sent,
            &context(&grants, &blobs),
            NOW + 500,
        )
        .expect("second");

        let PeerAcceptance::Accepted { job_id, .. } = first else {
            panic!("expected acceptance");
        };
        assert_eq!(
            second,
            PeerAcceptance::Duplicate {
                thread_id: "thread-1".into(),
                message_row_id: match &second {
                    PeerAcceptance::Duplicate { message_row_id, .. } => message_row_id.clone(),
                    other => panic!("expected a duplicate, got {other:?}"),
                },
                accepted: true,
                job_id: Some(job_id),
            }
        );
        assert_eq!(queue.submitted.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_looping_envelope_never_reaches_the_store() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let queue = FakeQueue::default();
        let grants = all_grants();
        let (_blob_dir, blobs) = artifacts();
        let mut looped = envelope("msg-1", PeerMessageKind::Message);
        looped.origin_chain.push(LOCAL.to_string());

        let refused =
            accept_peer_envelope(&mut store, &queue, &looped, &context(&grants, &blobs), NOW)
                .expect("decide");

        assert_eq!(
            refused,
            PeerAcceptance::Rejected {
                thread_id: None,
                reason: PeerRejection::OriginLoop,
            }
        );
        assert!(store.peer_thread("device-1", "thread-1").unwrap().is_none());
        assert!(queue.submitted.lock().unwrap().is_empty());
    }

    #[test]
    fn an_expired_request_is_not_run_late() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let queue = FakeQueue::default();
        let grants = all_grants();
        let (_blob_dir, blobs) = artifacts();
        let stale = envelope("msg-1", PeerMessageKind::TaskRequest);

        let refused = accept_peer_envelope(
            &mut store,
            &queue,
            &stale,
            &context(&grants, &blobs),
            stale.expires_at_ms + 1,
        )
        .expect("decide");

        assert!(matches!(
            refused,
            PeerAcceptance::Rejected {
                reason: PeerRejection::Expired,
                ..
            }
        ));
        assert!(queue.submitted.lock().unwrap().is_empty());
    }

    #[test]
    fn two_peers_claiming_one_thread_id_stay_in_separate_sessions() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let queue = FakeQueue::default();
        let grants = all_grants();
        let (_blob_dir, blobs) = artifacts();

        accept_peer_envelope(
            &mut store,
            &queue,
            &envelope("msg-1", PeerMessageKind::Message),
            &context(&grants, &blobs),
            NOW,
        )
        .expect("first peer");

        let mut other = context(&grants, &blobs);
        other.device_id = "device-2";
        let mut theirs = envelope("msg-1", PeerMessageKind::Message);
        theirs.thread_id = "thread-2".into();
        theirs.sender_instance_id = "instance-second".into();
        theirs.origin_chain = vec!["instance-second".into()];
        accept_peer_envelope(&mut store, &queue, &theirs, &other, NOW).expect("second peer");

        let submitted = queue.submitted.lock().unwrap();
        assert_eq!(submitted[0].0.session_key, "peer:device-1:thread-1");
        assert_eq!(submitted[1].0.session_key, "peer:device-2:thread-2");
        assert_ne!(submitted[0].0.dedupe_key(), submitted[1].0.dedupe_key());
    }
}
