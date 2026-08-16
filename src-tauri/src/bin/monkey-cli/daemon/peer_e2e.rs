//! Two installations, in one process, talking to each other as peers.
//!
//! Every other peer test checks one side in isolation. This file is the one
//! that puts a *sender* and a *receiver* together and drives real signed
//! requests from one into the other's API, because the properties worth
//! proving are the ones that only exist between two nodes: that a message the
//! sender considers delivered is a turn the receiver actually queued, that a
//! result correlates back to the request that caused it, that content is
//! handed over before it is referenced, and that a loop between two installs
//! stops instead of circulating.
//!
//! # What is real and what is not
//!
//! Real: the pairing records, the HMAC signatures, the replay window, the
//! capability gates, both SQLite stores, the envelope rules and the artifact
//! content store. Not real: the TLS socket between them — requests are handed
//! to [`RemoteApi::handle`] directly — and the run itself, which is a fake
//! queue, because what a peer may *cause* is the subject here and what a run
//! then *does* is covered by the run tests.
//!
//! No second machine, and nothing here needs a network.

#![cfg(test)]

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use little_monkey_lib::peers::{PeerArtifactRef, PeerEnvelope, PeerMessageKind, DEFAULT_HOP_LIMIT};

use super::remote::api::{ApiRequest, ApiResponse, RemoteApi};
use super::remote::protocol::{
    sign_request, DeviceCapability, PeerArtifactStored, PeerArtifactUpload, PeerHelloRequest,
    PeerHelloResponse, RemoteHostConfig, RemoteScopes, SignedRequestHeaders,
    REMOTE_PROTOCOL_VERSION,
};
use super::remote::store::{RemoteSecretStore, RemoteStore};
use super::store::{DaemonConfig, DaemonPaths};

const NOW: u64 = 1_700_000_000_000;

#[derive(Default)]
struct MemorySecrets(Mutex<HashMap<String, Vec<u8>>>);

impl RemoteSecretStore for MemorySecrets {
    fn set(&self, slot: &str, secret: &[u8]) -> Result<(), String> {
        self.0
            .lock()
            .unwrap()
            .insert(slot.to_string(), secret.to_vec());
        Ok(())
    }

    fn get(&self, slot: &str) -> Result<Vec<u8>, String> {
        self.0
            .lock()
            .unwrap()
            .get(slot)
            .cloned()
            .ok_or_else(|| format!("no secret in slot {slot}"))
    }

    fn delete(&self, slot: &str) -> Result<(), String> {
        self.0.lock().unwrap().remove(slot);
        Ok(())
    }
}

/// The runs a receiving installation was asked to start.
#[derive(Default)]
struct FakeRuns {
    submitted: Mutex<Vec<little_monkey_lib::channels::ingress::ConversationIngress>>,
}

impl super::channel_worker::RunQueue for FakeRuns {
    fn freeze_execution(
        &self,
        ingress: &little_monkey_lib::channels::ingress::ConversationIngress,
    ) -> Result<little_monkey_lib::channels::ingress::FrozenExecutionContext, String> {
        Ok(super::channel_worker::test_frozen_execution(ingress))
    }

    fn submit(
        &self,
        ingress: &little_monkey_lib::channels::ingress::ConversationIngress,
        _params: Vec<String>,
    ) -> Result<String, String> {
        self.submitted.lock().unwrap().push(ingress.clone());
        Ok(ingress.deterministic_job_id())
    }
}

/// One installation, with its own data root, stores and identity.
struct Node {
    root: PathBuf,
    paths: DaemonPaths,
    instance_id: String,
    secrets: Arc<MemorySecrets>,
    api: RemoteApi,
    runs: Arc<FakeRuns>,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A credential one installation holds for calling another.
struct Pairing {
    device_id: String,
    secret: Vec<u8>,
    /// Bumped per call: the replay window refuses a repeated sequence, so a
    /// test that reused one would be testing the replay guard by accident.
    next_sequence: Mutex<u64>,
}

impl Pairing {
    fn sequence(&self) -> u64 {
        let mut next = self.next_sequence.lock().unwrap();
        let value = *next;
        *next += 1;
        value
    }
}

impl Node {
    fn start(instance_id: &str) -> Node {
        let root =
            std::env::temp_dir().join(format!("little-monkey-peer-e2e-{}", uuid::Uuid::new_v4()));
        let paths = DaemonPaths::under(&root);
        paths.ensure().expect("paths");
        DaemonConfig::default().save(&paths).expect("config");
        let host = RemoteHostConfig {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            runner_id: instance_id.to_string(),
            listen: "127.0.0.1:1".into(),
            advertise_url: format!("https://{instance_id}.invalid"),
            certificate_path: "/tmp/cert".into(),
            private_key_path: "/tmp/key".into(),
            certificate_sha256: "a".repeat(64),
            enabled: true,
        };
        let store = RemoteStore::open(&paths.root).expect("remote store");
        let secrets = Arc::new(MemorySecrets::default());
        let runs = Arc::new(FakeRuns::default());
        let api = RemoteApi::injected(paths.clone(), host, store, secrets.clone())
            .with_peer_runs(runs.clone());
        Node {
            root,
            paths,
            instance_id: instance_id.to_string(),
            secrets,
            api,
            runs,
        }
    }

    /// Reopen this installation's API against the same data root.
    ///
    /// What a restart is, for the purposes of these tests: every in-memory
    /// decision is gone and only what was written down survives.
    fn restart(&mut self) {
        let host = RemoteHostConfig {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            runner_id: self.instance_id.clone(),
            listen: "127.0.0.1:1".into(),
            advertise_url: format!("https://{}.invalid", self.instance_id),
            certificate_path: "/tmp/cert".into(),
            private_key_path: "/tmp/key".into(),
            certificate_sha256: "a".repeat(64),
            enabled: true,
        };
        let store = RemoteStore::open(&self.paths.root).expect("remote store");
        self.runs = Arc::new(FakeRuns::default());
        self.api = RemoteApi::injected(self.paths.clone(), host, store, self.secrets.clone())
            .with_peer_runs(self.runs.clone());
    }

    /// Invite another installation to be a peer here, and hand back the
    /// credential it will call with.
    ///
    /// The real invitation flow, not a shortcut: the scope is empty and the
    /// capabilities are exactly the peer grants asked for, which is what makes
    /// the resulting pairing unable to reach anything else on this node.
    fn admit_peer(&self, grants: BTreeSet<DeviceCapability>, guest: &str) -> Pairing {
        let mut store = RemoteStore::open(&self.paths.root).expect("remote store");
        let scopes = RemoteScopes {
            actions: BTreeSet::new(),
            run_ids: BTreeSet::new(),
            workspace_ids: BTreeSet::new(),
            max_artifact_bytes: 1_024,
        };
        let invitation = store
            .create_invitation_with_capabilities(&scopes, &grants, NOW, NOW + 600_000)
            .expect("invitation");
        let accepted = store
            .accept_invitation_with_capabilities(
                &invitation.pairing_id,
                &invitation.token,
                guest,
                &self.instance_id,
                None,
                NOW + 1,
                self.secrets.as_ref(),
            )
            .expect("accept");
        Pairing {
            device_id: accepted.device_id,
            secret: accepted.device_secret.as_bytes().to_vec(),
            next_sequence: Mutex::new(1),
        }
    }

    fn grants_for(&self, device_id: &str) -> BTreeSet<DeviceCapability> {
        RemoteStore::open(&self.paths.root)
            .expect("remote store")
            .device(device_id)
            .expect("device")
            .expect("paired")
            .capabilities
    }

    fn set_grants(&self, device_id: &str, grants: BTreeSet<DeviceCapability>) {
        RemoteStore::open(&self.paths.root)
            .expect("remote store")
            .set_peer_capabilities(device_id, &grants, NOW + 10)
            .expect("grant");
    }

    fn revoke(&self, device_id: &str) {
        RemoteStore::open(&self.paths.root)
            .expect("remote store")
            .revoke_device(
                device_id,
                "revoked in a test",
                NOW + 20,
                self.secrets.as_ref(),
                None,
            )
            .expect("revoke");
    }

    fn queued(&self) -> Vec<little_monkey_lib::channels::ingress::ConversationIngress> {
        self.runs.submitted.lock().unwrap().clone()
    }
}

/// One signed call from a paired installation into another's API.
fn call(
    host: &Node,
    pairing: &Pairing,
    method: &str,
    path: &str,
    body: &[u8],
    now_ms: u64,
) -> ApiResponse {
    let sequence = pairing.sequence();
    let mut auth = SignedRequestHeaders {
        device_id: pairing.device_id.clone(),
        secret_generation: 1,
        sequence,
        timestamp_ms: now_ms,
        nonce: format!("nonce-{sequence}-{}", uuid::Uuid::new_v4().simple()),
        command_id: format!("cmd-{sequence}-{}", uuid::Uuid::new_v4().simple()),
        signature: String::new(),
    };
    auth.signature = sign_request(&pairing.secret, &auth, method, path, body);
    host.api.handle(
        ApiRequest {
            method: method.into(),
            path_and_query: path.into(),
            body: body.to_vec(),
            auth: Some(auth),
        },
        now_ms,
    )
}

fn json(response: &ApiResponse) -> serde_json::Value {
    serde_json::from_slice(&response.body).unwrap_or(serde_json::Value::Null)
}

fn every_grant() -> BTreeSet<DeviceCapability> {
    BTreeSet::from([
        DeviceCapability::PeerMessage,
        DeviceCapability::PeerTaskRequest,
        DeviceCapability::PeerArtifact,
    ])
}

/// An envelope from `sender`, ready to travel.
fn envelope(sender: &Node, message_id: &str, kind: PeerMessageKind, body: &str) -> PeerEnvelope {
    let mut envelope = PeerEnvelope::new(
        message_id,
        "thread-1",
        kind,
        sender.instance_id.clone(),
        body,
        i64::try_from(NOW).unwrap(),
        600_000,
    );
    envelope.hop_limit = DEFAULT_HOP_LIMIT;
    envelope.correlation_id = Some("corr-1".into());
    envelope
}

fn send(receiver: &Node, pairing: &Pairing, envelope: &PeerEnvelope, now_ms: u64) -> ApiResponse {
    call(
        receiver,
        pairing,
        "POST",
        "/v1/remote/peer/messages",
        &serde_json::to_vec(envelope).unwrap(),
        now_ms,
    )
}

fn read_thread(receiver: &Node, pairing: &Pairing, thread_id: &str, now_ms: u64) -> ApiResponse {
    call(
        receiver,
        pairing,
        "GET",
        &format!("/v1/remote/peer/threads/{thread_id}"),
        b"",
        now_ms,
    )
}

/// Two installations, the second able to be reached by the first as a peer.
fn two_nodes(grants: BTreeSet<DeviceCapability>) -> (Node, Node, Pairing) {
    let alice = Node::start("instance-alice");
    let bob = Node::start("instance-bob");
    let alice_at_bob = bob.admit_peer(grants, "alice");
    (alice, bob, alice_at_bob)
}

#[test]
fn two_installations_pair_and_introduce_themselves_without_widening_anything() {
    let (alice, bob, pairing) = two_nodes(BTreeSet::from([DeviceCapability::PeerMessage]));

    let hello = PeerHelloRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        instance_id: alice.instance_id.clone(),
        advertised: every_grant(),
        // Asking for more than it holds is exactly the case worth proving:
        // an ask is data for the operator, never an entitlement.
        requested: every_grant(),
    };
    let response = call(
        &bob,
        &pairing,
        "POST",
        "/v1/remote/peer/hello",
        &serde_json::to_vec(&hello).unwrap(),
        NOW + 100,
    );
    assert_eq!(response.status, 200);
    let answered: PeerHelloResponse = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(answered.instance_id, bob.instance_id);
    // Bob reports what Alice actually has, not what she asked for.
    assert_eq!(
        answered.granted,
        BTreeSet::from([DeviceCapability::PeerMessage])
    );
    assert_eq!(
        bob.grants_for(&pairing.device_id),
        BTreeSet::from([DeviceCapability::PeerMessage]),
        "asking must not grant"
    );

    // The claim is recorded beside the pairing, for the operator to act on.
    let claim = RemoteStore::open(&bob.paths.root)
        .unwrap()
        .peer_advertisement(&pairing.device_id)
        .unwrap()
        .expect("advertisement");
    assert_eq!(claim.peer_instance_id, alice.instance_id);
    assert_eq!(claim.requested, every_grant());
}

#[test]
fn a_message_from_one_installation_becomes_a_turn_on_the_other() {
    let (alice, bob, pairing) = two_nodes(every_grant());

    let response = send(
        &bob,
        &pairing,
        &envelope(
            &alice,
            "msg-1",
            PeerMessageKind::Message,
            "the nightly build is red",
        ),
        NOW + 100,
    );
    assert_eq!(response.status, 202);
    assert_eq!(json(&response)["accepted"], true);

    let queued = bob.queued();
    assert_eq!(queued.len(), 1);
    assert_eq!(
        queued[0].source,
        little_monkey_lib::channels::ingress::ConversationSource::Peer
    );
    assert_eq!(
        queued[0].session_key,
        format!("peer:{}:thread-1", pairing.device_id)
    );
}

#[test]
fn a_task_request_runs_under_the_receivers_own_authority_and_its_result_correlates_back() {
    let (alice, bob, pairing) = two_nodes(every_grant());

    let asked = send(
        &bob,
        &pairing,
        &envelope(
            &alice,
            "msg-1",
            PeerMessageKind::TaskRequest,
            "find out why the nightly build is red",
        ),
        NOW + 100,
    );
    assert_eq!(asked.status, 202);
    assert_eq!(json(&asked)["correlation_id"], "corr-1");

    // Nothing the sender wrote reached the execution options: the receiver's
    // recipe decides, and the sender has no field that could say otherwise.
    let queued = bob.queued();
    let options = super::channel_ingress::queue_options_for(&queued[0], Vec::new());
    assert!(options.repository.is_none());
    assert!(!options.allow_commit);
    assert!(!options.allow_push);
    assert!(options.allowed_remotes.is_empty());
    assert!(options.snapshot_is_frozen);
    assert_eq!(
        queued[0].target.recipe,
        super::peer_ingress::PEER_TASK_RECIPE
    );

    // Reading the thread back carries the request and its correlation handle,
    // which is what a later result is matched against.
    let thread = read_thread(&bob, &pairing, "thread-1", NOW + 200);
    assert_eq!(thread.status, 200);
    let messages = json(&thread)["messages"].as_array().cloned().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["direction"], "inbound");
    assert_eq!(messages[0]["correlation_id"], "corr-1");
    assert_eq!(messages[0]["disposition"], "accepted");
}

#[test]
fn content_is_handed_over_before_it_is_referenced() {
    let (alice, bob, pairing) = two_nodes(every_grant());
    let bytes = b"nightly build log, line one".to_vec();
    let digest = super::remote::protocol::sha256_hex(&bytes);

    // Referencing first is refused: nothing on this node can read it.
    let mut premature = envelope(&alice, "msg-1", PeerMessageKind::Artifact, "the log");
    premature.artifacts.push(PeerArtifactRef {
        artifact_id: digest.clone(),
        sha256: digest.clone(),
        filename: Some("build.log".into()),
        media_type: Some("text/plain".into()),
        size_bytes: Some(bytes.len() as u64),
    });
    let refused = send(&bob, &pairing, &premature, NOW + 100);
    assert_eq!(refused.status, 400);
    assert!(bob.queued().is_empty());

    // Hand the bytes over, then reference them.
    use base64::Engine as _;
    let upload = PeerArtifactUpload {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        sha256: digest.clone(),
        content_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        filename: Some("build.log".into()),
        media_type: Some("text/plain".into()),
    };
    let stored = call(
        &bob,
        &pairing,
        "POST",
        "/v1/remote/peer/artifacts",
        &serde_json::to_vec(&upload).unwrap(),
        NOW + 110,
    );
    assert_eq!(stored.status, 201);
    let stored: PeerArtifactStored = serde_json::from_slice(&stored.body).unwrap();
    assert_eq!(stored.artifact_id, digest);
    assert_eq!(stored.size_bytes, bytes.len() as u64);

    let mut carried = premature.clone();
    carried.message_id = "msg-2".into();
    let accepted = send(&bob, &pairing, &carried, NOW + 120);
    assert_eq!(accepted.status, 202);

    let queued = bob.queued();
    let attachment = &queued[0].attachments[0];
    assert_eq!(
        attachment.stored_artifact_id.as_deref(),
        Some(digest.as_str())
    );
    assert_eq!(attachment.filename.as_deref(), Some("build.log"));
    // Content the receiver holds, under the digest of what it actually wrote.
    let store = super::peer_ingress::peer_content_store(&bob.paths).unwrap();
    assert_eq!(store.read(&digest).unwrap(), bytes);
}

#[test]
fn content_that_does_not_match_its_declared_digest_is_refused() {
    let (_alice, bob, pairing) = two_nodes(every_grant());
    use base64::Engine as _;
    let upload = PeerArtifactUpload {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        sha256: "b".repeat(64),
        content_base64: base64::engine::general_purpose::STANDARD.encode(b"not what was claimed"),
        filename: None,
        media_type: None,
    };
    let response = call(
        &bob,
        &pairing,
        "POST",
        "/v1/remote/peer/artifacts",
        &serde_json::to_vec(&upload).unwrap(),
        NOW + 100,
    );
    assert_eq!(response.status, 400);
}

#[test]
fn a_peer_without_the_grant_is_refused_and_nothing_runs() {
    let (alice, bob, pairing) = two_nodes(BTreeSet::from([DeviceCapability::PeerMessage]));

    let refused = send(
        &bob,
        &pairing,
        &envelope(&alice, "msg-1", PeerMessageKind::TaskRequest, "do my work"),
        NOW + 100,
    );
    assert_eq!(refused.status, 403);
    assert!(bob.queued().is_empty());

    // Granting it is the operator's act, and it takes effect immediately.
    bob.set_grants(&pairing.device_id, every_grant());
    let accepted = send(
        &bob,
        &pairing,
        &envelope(&alice, "msg-2", PeerMessageKind::TaskRequest, "do my work"),
        NOW + 200,
    );
    assert_eq!(accepted.status, 202);
    assert_eq!(bob.queued().len(), 1);
}

/// "Paired, and may not ask for anything" is a state an operator can actually
/// reach — the middle ground between a full grant and severing the pairing.
#[test]
fn a_peer_whose_grants_are_all_taken_away_stays_paired_and_powerless() {
    let (alice, bob, pairing) = two_nodes(every_grant());
    assert_eq!(
        send(
            &bob,
            &pairing,
            &envelope(&alice, "msg-1", PeerMessageKind::Message, "before"),
            NOW + 100,
        )
        .status,
        202
    );

    bob.set_grants(&pairing.device_id, BTreeSet::new());
    assert!(bob.grants_for(&pairing.device_id).is_empty());

    let refused = send(
        &bob,
        &pairing,
        &envelope(&alice, "msg-2", PeerMessageKind::Message, "after"),
        NOW + 200,
    );
    assert_eq!(refused.status, 403);
    assert_eq!(bob.queued().len(), 1);

    // Still paired, though: the credential is valid, it just reaches nothing.
    let device = RemoteStore::open(&bob.paths.root)
        .unwrap()
        .device(&pairing.device_id)
        .unwrap()
        .expect("still paired");
    assert!(device.active());
}

#[test]
fn an_unpaired_installation_reaches_nothing() {
    let (alice, bob, _pairing) = two_nodes(every_grant());
    let stranger = Pairing {
        device_id: "device-nobody".into(),
        secret: b"a secret nobody issued".to_vec(),
        next_sequence: Mutex::new(1),
    };

    let refused = send(
        &bob,
        &stranger,
        &envelope(&alice, "msg-1", PeerMessageKind::Message, "let me in"),
        NOW + 100,
    );
    assert!(
        refused.status == 401 || refused.status == 403,
        "an unknown device must not be admitted, got {}",
        refused.status
    );
    assert!(bob.queued().is_empty());
}

#[test]
fn revocation_takes_effect_on_the_very_next_message() {
    let (alice, bob, pairing) = two_nodes(every_grant());
    assert_eq!(
        send(
            &bob,
            &pairing,
            &envelope(&alice, "msg-1", PeerMessageKind::Message, "before"),
            NOW + 100,
        )
        .status,
        202
    );

    bob.revoke(&pairing.device_id);

    let after = send(
        &bob,
        &pairing,
        &envelope(&alice, "msg-2", PeerMessageKind::Message, "after"),
        NOW + 200,
    );
    assert!(
        after.status >= 400,
        "a revoked peer must be refused, got {}",
        after.status
    );
    assert_eq!(bob.queued().len(), 1, "only the pre-revocation message ran");
}

#[test]
fn the_same_message_id_twice_runs_once() {
    let (alice, bob, pairing) = two_nodes(every_grant());
    let sent = envelope(
        &alice,
        "msg-1",
        PeerMessageKind::TaskRequest,
        "look at the log",
    );

    let first = send(&bob, &pairing, &sent, NOW + 100);
    let second = send(&bob, &pairing, &sent, NOW + 200);

    assert_eq!(first.status, 202);
    assert_eq!(second.status, 200);
    assert_eq!(json(&second)["state"], "duplicate");
    assert_eq!(bob.queued().len(), 1);
}

#[test]
fn an_envelope_that_arrives_after_it_expired_is_not_run_late() {
    let (alice, bob, pairing) = two_nodes(every_grant());
    let stale = envelope(
        &alice,
        "msg-1",
        PeerMessageKind::TaskRequest,
        "still relevant?",
    );

    let refused = send(
        &bob,
        &pairing,
        &stale,
        u64::try_from(stale.expires_at_ms).unwrap() + 1,
    );
    assert_eq!(refused.status, 400);
    assert!(bob.queued().is_empty());
}

#[test]
fn a_message_that_has_already_been_here_is_dropped_rather_than_circulated() {
    let (alice, bob, pairing) = two_nodes(every_grant());

    // Bob's own instance id in the chain: the shape a three-node relay loop
    // takes by the time it comes back around.
    let mut looped = envelope(&alice, "msg-1", PeerMessageKind::Message, "round and round");
    looped.origin_chain.push(bob.instance_id.clone());
    assert_eq!(send(&bob, &pairing, &looped, NOW + 100).status, 400);

    // And an envelope with no hops left cannot travel at all.
    let mut spent = envelope(&alice, "msg-2", PeerMessageKind::Message, "one hop too far");
    spent.hop_limit = 0;
    assert_eq!(send(&bob, &pairing, &spent, NOW + 200).status, 400);

    assert!(bob.queued().is_empty());
    // Both refusals are visible to the operator without retaining what was said.
    let events = super::store::DaemonStore::open(&bob.paths)
        .unwrap()
        .peer_rejection_events(10)
        .unwrap();
    assert_eq!(events.len(), 2);
    assert!(events
        .iter()
        .all(|event| event.peer_device_id == pairing.device_id));
}

#[test]
fn a_peer_cannot_reach_anything_but_the_peer_plane() {
    let (_alice, bob, pairing) = two_nodes(every_grant());

    // A representative route from each other plane. None of them is reachable
    // with peer standing, which is the whole point of peer standing being its
    // own grant rather than a level of trust.
    for (method, path) in [
        ("GET", "/v1/remote/runs"),
        ("GET", "/v1/remote/node"),
        ("POST", "/v1/remote/node/runs"),
        ("GET", "/v1/remote/mobile/sessions"),
        ("POST", "/v1/remote/desktop/control/start"),
    ] {
        let response = call(&bob, &pairing, method, path, b"{}", NOW + 100);
        assert!(
            response.status == 403 || response.status == 404,
            "{method} {path} answered {} to a peer",
            response.status
        );
    }
    assert!(bob.queued().is_empty());
}

#[test]
fn a_restart_does_not_run_a_delivered_task_a_second_time() {
    let (alice, mut bob, pairing) = two_nodes(every_grant());
    let sent = envelope(
        &alice,
        "msg-1",
        PeerMessageKind::TaskRequest,
        "check the log",
    );

    assert_eq!(send(&bob, &pairing, &sent, NOW + 100).status, 202);
    assert_eq!(bob.queued().len(), 1);

    bob.restart();
    // The sender retries, as a sender with no answer would.
    let retried = send(&bob, &pairing, &sent, NOW + 200);
    assert_eq!(retried.status, 200);
    assert_eq!(json(&retried)["state"], "duplicate");
    assert!(
        bob.queued().is_empty(),
        "the durable dedupe row survived the restart, so nothing ran again"
    );
}

#[test]
fn one_peer_cannot_read_another_peers_thread_even_with_the_same_thread_id() {
    let alice = Node::start("instance-alice");
    let carol = Node::start("instance-carol");
    let bob = Node::start("instance-bob");
    let alice_at_bob = bob.admit_peer(every_grant(), "alice");
    let carol_at_bob = bob.admit_peer(every_grant(), "carol");

    assert_eq!(
        send(
            &bob,
            &alice_at_bob,
            &envelope(&alice, "msg-1", PeerMessageKind::Message, "alice's words"),
            NOW + 100,
        )
        .status,
        202
    );
    // Same thread id, different peer: its own conversation, not a way in.
    assert_eq!(
        send(
            &bob,
            &carol_at_bob,
            &envelope(&carol, "msg-1", PeerMessageKind::Message, "carol's words"),
            NOW + 110,
        )
        .status,
        202
    );

    let queued = bob.queued();
    assert_ne!(queued[0].session_key, queued[1].session_key);

    // Each reads exactly one message in "thread-1": its own.
    for pairing in [&alice_at_bob, &carol_at_bob] {
        let thread = read_thread(&bob, pairing, "thread-1", NOW + 200);
        assert_eq!(thread.status, 200);
        let messages = json(&thread)["messages"].as_array().cloned().unwrap();
        assert_eq!(messages.len(), 1);
    }
}
