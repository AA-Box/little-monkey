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

use little_monkey_lib::channels::ingress::{ConversationIngress, ConversationSource};
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
    // this table with junk that never became anything.
    if let Err(reason) = envelope.validate(context.local_instance_id, now_ms) {
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

    let ingress = ingress_for(envelope, context.device_id, &thread.session_key);
    let params = vec![format!(
        "{MESSAGE_PARAM}={}",
        channel_ingress::message_param(
            &ingress,
            &format!("a paired Little Monkey peer ({})", context.device_id),
        )
    )];

    match channel_ingress::submit_ingress(store, queue, &ingress, &params, now_ms)? {
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

fn ingress_for(envelope: &PeerEnvelope, device_id: &str, session_key: &str) -> ConversationIngress {
    let mut ingress = ConversationIngress::direct(
        ConversationSource::Peer,
        device_id,
        &envelope.message_id,
        session_key,
        &envelope.body,
        RouteTarget::new(PEER_TASK_RECIPE),
        envelope.created_at_ms,
    );
    ingress.attachments = envelope
        .artifacts
        .iter()
        .map(|artifact| ChannelAttachment {
            provider_id: Some(artifact.artifact_id.clone()),
            kind: AttachmentKind::Other,
            filename: artifact.filename.clone(),
            mime_type: artifact.media_type.clone(),
            declared_size_bytes: artifact.size_bytes,
            // A handle, not a URL: the bytes are fetched through the artifact
            // mechanisms this node already has, from the peer that offered
            // them. Nothing here can name a path on this machine.
            source: AttachmentSource::ProviderHandle {
                handle: format!("peer:{device_id}:{}", artifact.artifact_id),
            },
        })
        .collect();
    ingress
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

    fn context<'a>(granted: &'a BTreeSet<DeviceCapability>) -> PeerContext<'a> {
        PeerContext {
            device_id: "device-1",
            granted,
            revoked: false,
            local_instance_id: LOCAL,
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

        let accepted = accept_peer_envelope(
            &mut store,
            &queue,
            &envelope("msg-1", PeerMessageKind::TaskRequest),
            &context(&grants),
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
        accept_peer_envelope(
            &mut store,
            &queue,
            &envelope("msg-1", PeerMessageKind::TaskRequest),
            &context(&grants),
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

        let refused = accept_peer_envelope(
            &mut store,
            &queue,
            &envelope("msg-1", PeerMessageKind::TaskRequest),
            &context(&grants),
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

    #[test]
    fn attaching_an_artifact_needs_its_own_grant() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let queue = FakeQueue::default();
        let grants = BTreeSet::from([DeviceCapability::PeerMessage]);
        let mut with_file = envelope("msg-1", PeerMessageKind::Message);
        with_file.artifacts.push(PeerArtifactRef {
            artifact_id: "art-1".into(),
            sha256: "a".repeat(64),
            filename: Some("build.log".into()),
            media_type: Some("text/plain".into()),
            size_bytes: Some(2048),
        });

        let refused = accept_peer_envelope(&mut store, &queue, &with_file, &context(&grants), NOW)
            .expect("decide");
        assert!(matches!(
            refused,
            PeerAcceptance::Rejected {
                reason: PeerRejection::MissingCapability,
                ..
            }
        ));

        // With the grant, the reference travels as an artifact handle — never
        // as a path.
        let grants = all_grants();
        let mut allowed = with_file.clone();
        allowed.message_id = "msg-2".into();
        accept_peer_envelope(&mut store, &queue, &allowed, &context(&grants), NOW).expect("accept");
        let submitted = queue.submitted.lock().unwrap();
        let attachment = &submitted[0].0.attachments[0];
        assert_eq!(
            attachment.source,
            AttachmentSource::ProviderHandle {
                handle: "peer:device-1:art-1".into()
            }
        );
        assert_eq!(attachment.filename.as_deref(), Some("build.log"));
    }

    #[test]
    fn a_revoked_peer_is_refused_even_with_its_old_grants() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let queue = FakeQueue::default();
        let grants = all_grants();
        let mut context = context(&grants);
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
        let sent = envelope("msg-1", PeerMessageKind::TaskRequest);

        let first =
            accept_peer_envelope(&mut store, &queue, &sent, &context(&grants), NOW).expect("first");
        let second = accept_peer_envelope(&mut store, &queue, &sent, &context(&grants), NOW + 500)
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
        let mut looped = envelope("msg-1", PeerMessageKind::Message);
        looped.origin_chain.push(LOCAL.to_string());

        let refused = accept_peer_envelope(&mut store, &queue, &looped, &context(&grants), NOW)
            .expect("decide");

        assert_eq!(
            refused,
            PeerAcceptance::Rejected {
                thread_id: None,
                reason: PeerRejection::OriginLoop,
            }
        );
        assert!(store.peer_thread("thread-1").unwrap().is_none());
        assert!(queue.submitted.lock().unwrap().is_empty());
    }

    #[test]
    fn an_expired_request_is_not_run_late() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let queue = FakeQueue::default();
        let grants = all_grants();
        let stale = envelope("msg-1", PeerMessageKind::TaskRequest);

        let refused = accept_peer_envelope(
            &mut store,
            &queue,
            &stale,
            &context(&grants),
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

        accept_peer_envelope(
            &mut store,
            &queue,
            &envelope("msg-1", PeerMessageKind::Message),
            &context(&grants),
            NOW,
        )
        .expect("first peer");

        let mut other = context(&grants);
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
