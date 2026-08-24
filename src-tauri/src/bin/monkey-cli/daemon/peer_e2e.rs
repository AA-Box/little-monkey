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
//! capability gates, both SQLite stores, the envelope rules, the artifact
//! content store and its durable per-peer admissions, the durable job row, the
//! run ledger the result is read out of, and the result materialization the
//! thread poll performs. Not real: the TLS socket between them — requests are
//! handed to [`RemoteApi::handle`] directly — and the *execution* of the run,
//! which a controlled fake finishes, because what a peer may cause and what
//! comes back to it is the subject here and what a model does in between is
//! covered by the run tests.
//!
//! Nothing here inserts a row production would have written. A result exists in
//! these tests only because `materialize_peer_results` made one out of ledger
//! events, which is the only way one is ever made.
//!
//! No second machine, and nothing here needs a network.

#![cfg(test)]

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use little_monkey_lib::peers::{PeerArtifactRef, PeerEnvelope, PeerMessageKind, DEFAULT_HOP_LIMIT};
use little_monkey_lib::run_ledger::RunLedger;
use little_monkey_lib::run_protocol::{
    ClientIdentity, ClientKind, ModelTargetSnapshot, OutputChannel, PermissionMode,
    PermissionPolicySnapshot, RootAccess, RootGrant, RunBudgets, RunEvent, RunEventEnvelope,
    RunKind, RunSpec, ToolPolicyDecision, UsageSnapshot, WorkspaceContext,
    RUN_PROTOCOL_SCHEMA_VERSION,
};

use super::remote::api::{ApiRequest, ApiResponse, RemoteApi};
use super::remote::protocol::{
    sign_request, DeviceCapability, PeerArtifactStored, PeerArtifactUpload, PeerHelloRequest,
    PeerHelloResponse, RemoteHostConfig, RemoteScopes, SignedRequestHeaders,
    REMOTE_PROTOCOL_VERSION,
};
use super::remote::store::{RemoteSecretStore, RemoteStore};
use super::store::{DaemonConfig, DaemonPaths, DaemonStore, NewDaemonJob};

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
    /// Which key this is. A rotation mints generation *n+1*, and a request
    /// signed under the wrong one is refused — which is exactly what the
    /// rotation test needs to be able to say.
    generation: u64,
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
            generation: 1,
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
        secret_generation: pairing.generation,
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

/// Hand bytes to a peer the way the sending side does: upload first, then the
/// digest the receiver stored them under.
fn put_artifact(
    receiver: &Node,
    pairing: &Pairing,
    bytes: &[u8],
    filename: Option<&str>,
    media_type: Option<&str>,
    now_ms: u64,
) -> ApiResponse {
    use base64::Engine as _;
    let upload = PeerArtifactUpload {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        sha256: super::remote::protocol::sha256_hex(bytes),
        content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        filename: filename.map(str::to_string),
        media_type: media_type.map(str::to_string),
    };
    call(
        receiver,
        pairing,
        "POST",
        "/v1/remote/peer/artifacts",
        &serde_json::to_vec(&upload).unwrap(),
        now_ms,
    )
}

/// How a finished run ended, for the controlled executor below.
#[derive(Clone, Copy)]
enum Ending {
    Succeeded,
    Failed,
}

/// The job id the receiver's queue would have minted for the turn it took.
///
/// Derived from the ingress itself, exactly as the real queue derives it, so
/// the job these tests drive is the job the peer's message actually produced.
fn job_id_of(node: &Node, index: usize) -> String {
    node.queued()[index].deterministic_job_id()
}

/// Finish one peer-caused run, using every durable step the real worker uses.
///
/// The *execution* is what is fake here and nothing else: the job row is the
/// daemon store's own, the run id is associated through `mark_queued` the way
/// the queue associates it, and the terminal state is a real run-ledger event
/// chain. Nothing writes a peer result — that is left to the thread poll, which
/// is the code under test.
fn finish_run(node: &Node, job_id: &str, ending: Ending, text: &str, now_ms: u64) -> String {
    let run_id = format!("run-{}", &job_id[job_id.len().saturating_sub(20)..]);
    let mut store = DaemonStore::open(&node.paths).expect("daemon store");
    store
        .insert_preparing(
            &NewDaemonJob {
                job_id: job_id.to_string(),
                recipe_snapshot: PathBuf::from("/tmp/peer-task.json"),
                priority: 0,
                max_attempts: 1,
                created_at_ms: now_ms,
                max_runtime_ms: 60_000,
                max_memory_bytes: None,
                max_log_bytes: 1_024,
                repository_policy_json: None,
                worktree_json: None,
                parent_run_id: None,
            },
            64,
        )
        .expect("job row");
    store
        .mark_queued(job_id, &run_id, now_ms)
        .expect("associate the run");
    drop(store);

    let mut ledger = RunLedger::open(&node.paths.ledger_db).expect("ledger");
    ledger.submit_run(&run_spec(&run_id, now_ms)).expect("run");
    let emitter = ClientIdentity {
        client_id: "peer-e2e".into(),
        instance_id: node.instance_id.clone(),
        kind: ClientKind::Daemon,
        version: "1".into(),
    };
    let mut sequence = 0;
    let mut emit = |event: RunEvent, ledger: &mut RunLedger| {
        sequence += 1;
        ledger
            .append_event(&RunEventEnvelope {
                schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
                event_id: format!("evt-{run_id}-{sequence}"),
                run_id: run_id.clone(),
                sequence,
                occurred_at_ms: now_ms + sequence,
                actor_id: None,
                emitter: emitter.clone(),
                event,
            })
            .expect("event");
    };
    match ending {
        Ending::Succeeded => {
            emit(
                RunEvent::ModelDelta {
                    message_id: "assistant-1".into(),
                    channel: OutputChannel::Assistant,
                    text: text.to_string(),
                },
                &mut ledger,
            );
            emit(
                RunEvent::Completed {
                    summary: None,
                    result_artifact_ids: vec![],
                    usage: UsageSnapshot {
                        input_tokens: 0,
                        output_tokens: 0,
                        cached_input_tokens: 0,
                        model_calls: 1,
                        tool_calls: 0,
                        cost_micros: None,
                    },
                },
                &mut ledger,
            );
        }
        Ending::Failed => emit(
            RunEvent::Failed {
                code: "tool_failed".into(),
                message: text.to_string(),
                retryable: false,
            },
            &mut ledger,
        ),
    }
    run_id
}

/// A minimal valid run spec. Nothing here is peer-specific: what the result
/// path reads is the terminal event chain, not the spec.
fn run_spec(run_id: &str, now_ms: u64) -> RunSpec {
    RunSpec {
        schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
        run_id: run_id.to_string(),
        idempotency_key: format!("idem-{run_id}"),
        created_at_ms: now_ms,
        kind: RunKind::Workflow,
        submitted_by: ClientIdentity {
            client_id: "peer-e2e".into(),
            instance_id: "instance".into(),
            kind: ClientKind::Daemon,
            version: "1".into(),
        },
        task: "peer task".into(),
        instructions: None,
        input_artifact_ids: vec![],
        target: ModelTargetSnapshot::Provider {
            target_id: "target".into(),
            label: "test".into(),
            provider_id: "test".into(),
            endpoint: "http://127.0.0.1:1/v1".into(),
            model: "test".into(),
            credential_ref_id: "credential-none".into(),
            capabilities: crate::task::cli_capabilities(),
        },
        workspace: Some(WorkspaceContext {
            workspace_id: "workspace".into(),
            primary_root_id: "root".into(),
            roots: vec![RootGrant {
                root_id: "root".into(),
                canonical_path: "/tmp".into(),
                access: RootAccess::ReadWrite,
                allow_symlinks_within_root: false,
            }],
            repository_policy: None,
        }),
        permission_policy: PermissionPolicySnapshot {
            mode: PermissionMode::Auto,
            unattended: true,
            approval_timeout_ms: 60_000,
            default_tool_decision: ToolPolicyDecision::Prompt,
            tool_rules: vec![],
            allow_network: false,
            allow_external_mutations: false,
            egress_allowlist: None,
            channel_send: None,
        },
        budgets: RunBudgets {
            wall_time_ms: 60_000,
            max_iterations: 4,
            max_model_calls: 4,
            max_tool_calls: 4,
            max_input_tokens: 1_000,
            max_output_tokens: 1_000,
            max_cost_micros: None,
            max_artifact_bytes: 1_024,
            max_event_count: 100,
        },
        autonomous_task: None,
        execution_target: None,
        workspace_transfer: None,
    }
}

/// The result rows a thread poll returned, if any.
fn results(response: &ApiResponse) -> Vec<serde_json::Value> {
    json(response)["messages"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|message| message["kind"] == "result")
        .collect()
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
    let (alice, mut bob, pairing) = two_nodes(every_grant());

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

    // Nothing has finished, so there is no result yet — and saying there is
    // one before the run reached a terminal state would be the worst possible
    // lie this path could tell.
    let pending = read_thread(&bob, &pairing, "thread-1", NOW + 200);
    assert_eq!(pending.status, 200);
    let messages = json(&pending)["messages"].as_array().cloned().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["direction"], "inbound");
    assert_eq!(messages[0]["correlation_id"], "corr-1");
    assert_eq!(messages[0]["disposition"], "accepted");
    assert!(results(&pending).is_empty());

    // The run finishes on Bob, under Bob's own ledger.
    let job_id = job_id_of(&bob, 0);
    finish_run(
        &bob,
        &job_id,
        Ending::Succeeded,
        "the nightly build broke on the migration step",
        NOW + 300,
    );

    // Alice polls the thread she knows about, and the result is materialized
    // out of the ledger by the poll itself.
    let answered = read_thread(&bob, &pairing, "thread-1", NOW + 400);
    let rows = results(&answered);
    assert_eq!(rows.len(), 1);
    let result = &rows[0];
    assert_eq!(result["direction"], "outbound");
    assert_eq!(result["disposition"], "delivered");
    assert_eq!(result["correlation_id"], "corr-1");
    assert_eq!(result["payload"]["in_reply_to"], "msg-1");
    assert_eq!(result["payload"]["state"], "succeeded");
    assert_eq!(
        result["payload"]["text"],
        "the nightly build broke on the migration step"
    );

    // Polling is idempotent: a sender with a poor connection asks repeatedly,
    // and there is still exactly one result.
    for poll in 0..3 {
        let again = read_thread(&bob, &pairing, "thread-1", NOW + 500 + poll * 10);
        assert_eq!(results(&again).len(), 1);
    }

    // And so is a restart: the row is durable, and the same idempotent insert
    // decides again after everything in memory is gone.
    bob.restart();
    let after_restart = read_thread(&bob, &pairing, "thread-1", NOW + 600);
    let rows = results(&after_restart);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["payload"]["in_reply_to"], "msg-1");
}

/// The crash boundary that matters most: the work finished, and the answer had
/// nowhere to go yet.
#[test]
fn a_task_that_finished_while_the_receiver_was_down_still_answers_once() {
    let (alice, mut bob, pairing) = two_nodes(every_grant());
    assert_eq!(
        send(
            &bob,
            &pairing,
            &envelope(
                &alice,
                "msg-1",
                PeerMessageKind::TaskRequest,
                "check the log"
            ),
            NOW + 100,
        )
        .status,
        202
    );
    let job_id = job_id_of(&bob, 0);
    finish_run(
        &bob,
        &job_id,
        Ending::Succeeded,
        "log looks fine",
        NOW + 200,
    );

    // Nobody polled before this point, so nothing had materialized.
    bob.restart();

    let answered = read_thread(&bob, &pairing, "thread-1", NOW + 300);
    let rows = results(&answered);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["payload"]["state"], "succeeded");
    assert_eq!(rows[0]["payload"]["text"], "log looks fine");
    assert_eq!(rows[0]["correlation_id"], "corr-1");
}

#[test]
fn a_task_that_failed_comes_back_as_a_correlated_failure() {
    let (alice, bob, pairing) = two_nodes(every_grant());
    assert_eq!(
        send(
            &bob,
            &pairing,
            &envelope(
                &alice,
                "msg-1",
                PeerMessageKind::TaskRequest,
                "run the migration",
            ),
            NOW + 100,
        )
        .status,
        202
    );
    let job_id = job_id_of(&bob, 0);
    finish_run(
        &bob,
        &job_id,
        Ending::Failed,
        "the workspace root does not exist here",
        NOW + 200,
    );

    let answered = read_thread(&bob, &pairing, "thread-1", NOW + 300);
    let rows = results(&answered);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["payload"]["state"], "failed");
    assert_eq!(
        rows[0]["payload"]["text"],
        "the workspace root does not exist here"
    );
    assert_eq!(rows[0]["correlation_id"], "corr-1");
    assert_eq!(rows[0]["payload"]["in_reply_to"], "msg-1");
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

/// A refused upload leaves nothing behind — neither an admission nor bytes.
///
/// The digest is checked before the content store is touched, which is the
/// difference between refusing an upload and publishing it into a store shared
/// with runs, channels and the operator's own imports before noticing. A peer
/// that cannot be believed must not be able to fill that store one rejected
/// upload at a time.
#[test]
fn content_that_does_not_match_its_declared_digest_is_refused() {
    let (_alice, bob, pairing) = two_nodes(every_grant());
    use base64::Engine as _;
    let bytes = b"not what was claimed";
    let real_digest = super::remote::protocol::sha256_hex(bytes);
    let upload = PeerArtifactUpload {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        sha256: "b".repeat(64),
        content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
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

    let store = super::peer_ingress::peer_content_store(&bob.paths).unwrap();
    assert!(
        store.read(&real_digest).is_err(),
        "refused bytes must not be published under their true digest either"
    );
    assert!(
        super::store::DaemonStore::open(&bob.paths)
            .unwrap()
            .peer_artifact_receipts(None, 10)
            .unwrap()
            .is_empty(),
        "and must not be admitted"
    );
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
        generation: 1,
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

    // Every other plane, one route each, including the ones a peer might be
    // imagined to have a claim on: someone else's run artifacts, the kill
    // switch, placement, migration, the desktop and a paired phone's sessions.
    // None of them is reachable with peer standing, which is the whole point of
    // peer standing being its own grant rather than a level of trust.
    for (method, path) in [
        ("GET", "/v1/remote/runs"),
        ("GET", "/v1/remote/runs/run-1"),
        ("GET", "/v1/remote/runs/run-1/events"),
        ("GET", "/v1/remote/runs/run-1/approvals"),
        // Arbitrary artifact access: the one an artifact grant might be read as
        // implying, and does not.
        ("GET", "/v1/remote/runs/run-1/artifacts/deadbeef"),
        ("POST", "/v1/remote/runs/run-1/approve"),
        ("POST", "/v1/remote/runs/run-1/cancel"),
        ("POST", "/v1/remote/runs/run-1/pause"),
        ("POST", "/v1/remote/kill"),
        ("GET", "/v1/remote/node"),
        ("POST", "/v1/remote/node/runs"),
        ("POST", "/v1/remote/node/migration/preflight"),
        ("POST", "/v1/remote/node/migration/accept"),
        ("GET", "/v1/remote/mobile/sessions"),
        ("POST", "/v1/remote/mobile/captures"),
        ("GET", "/v1/remote/mobile/workflows"),
        ("POST", "/v1/remote/desktop-control/start"),
        ("POST", "/v1/remote/desktop-control/action"),
        ("POST", "/v1/remote/device/surface"),
        ("GET", "/v1/remote/device/commands/next"),
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

/// Content admission is per pairing, and the shared content store cannot be
/// used as the authorization database.
///
/// Alice hands bytes over. Carol, who is equally paired and equally granted,
/// knows the digest — digests travel in the open and are derivable from content
/// — and must still be refused. Before the durable receipt, "the digest resolves
/// in `content-v1`" was the whole check, and Carol would have been accepted.
#[test]
fn one_peers_uploaded_content_is_not_referenceable_by_another_peer() {
    let alice = Node::start("instance-alice");
    let carol = Node::start("instance-carol");
    let bob = Node::start("instance-bob");
    let alice_at_bob = bob.admit_peer(every_grant(), "alice");
    let carol_at_bob = bob.admit_peer(every_grant(), "carol");

    let bytes = b"nightly build log, line one".to_vec();
    let digest = super::remote::protocol::sha256_hex(&bytes);
    let stored = put_artifact(
        &bob,
        &alice_at_bob,
        &bytes,
        Some("build.log"),
        Some("text/plain"),
        NOW + 100,
    );
    assert_eq!(stored.status, 201);

    let reference = |sender: &Node, message_id: &str| {
        let mut envelope = envelope(sender, message_id, PeerMessageKind::Artifact, "the log");
        envelope.artifacts.push(PeerArtifactRef {
            artifact_id: digest.clone(),
            sha256: digest.clone(),
            filename: Some("build.log".into()),
            media_type: Some("text/plain".into()),
            size_bytes: Some(bytes.len() as u64),
        });
        envelope
    };

    // Carol's own thread id, so nothing here is about thread scoping.
    let mut carols = reference(&carol, "msg-1");
    carols.thread_id = "thread-carol".into();
    let refused = send(&bob, &carol_at_bob, &carols, NOW + 110);
    assert_eq!(refused.status, 400);
    assert!(bob.queued().is_empty(), "nothing ran for Carol");

    // Alice, who actually uploaded it, is accepted.
    assert_eq!(
        send(&bob, &alice_at_bob, &reference(&alice, "msg-2"), NOW + 120).status,
        202
    );
    assert_eq!(bob.queued().len(), 1);
    assert_eq!(
        bob.queued()[0].attachments[0].stored_artifact_id.as_deref(),
        Some(digest.as_str())
    );
}

/// An admission is durable, and it is still one peer's after a restart.
#[test]
fn an_admitted_artifact_survives_a_restart_and_still_belongs_to_one_peer() {
    let alice = Node::start("instance-alice");
    let carol = Node::start("instance-carol");
    let mut bob = Node::start("instance-bob");
    let alice_at_bob = bob.admit_peer(every_grant(), "alice");
    let carol_at_bob = bob.admit_peer(every_grant(), "carol");

    let bytes = b"a log worth keeping".to_vec();
    let digest = super::remote::protocol::sha256_hex(&bytes);
    assert_eq!(
        put_artifact(
            &bob,
            &alice_at_bob,
            &bytes,
            Some("keep.log"),
            None,
            NOW + 100
        )
        .status,
        201
    );

    bob.restart();

    let mut carols = envelope(&carol, "msg-1", PeerMessageKind::Artifact, "hand it over");
    carols.thread_id = "thread-carol".into();
    carols.artifacts.push(PeerArtifactRef {
        artifact_id: digest.clone(),
        sha256: digest.clone(),
        filename: None,
        media_type: None,
        size_bytes: Some(bytes.len() as u64),
    });
    assert_eq!(send(&bob, &carol_at_bob, &carols, NOW + 200).status, 400);

    let mut alices = carols.clone();
    alices.message_id = "msg-2".into();
    alices.thread_id = "thread-1".into();
    alices.sender_instance_id = alice.instance_id.clone();
    alices.origin_chain = vec![alice.instance_id.clone()];
    assert_eq!(send(&bob, &alice_at_bob, &alices, NOW + 210).status, 202);
    // The metadata the run sees is the one the *upload* declared, not the one
    // this envelope left out.
    assert_eq!(
        bob.queued()[0].attachments[0].filename.as_deref(),
        Some("keep.log")
    );
}

/// Clearing a peer withdraws what its uploads bought it, without touching the
/// bytes — they may equally belong to a run or to local content.
#[test]
fn clearing_a_peer_makes_it_upload_again_before_it_can_reference() {
    let (alice, bob, pairing) = two_nodes(every_grant());
    let bytes = b"transient log".to_vec();
    let digest = super::remote::protocol::sha256_hex(&bytes);
    assert_eq!(
        put_artifact(&bob, &pairing, &bytes, Some("t.log"), None, NOW + 100).status,
        201
    );

    let reference = |message_id: &str| {
        let mut envelope = envelope(&alice, message_id, PeerMessageKind::Artifact, "here");
        envelope.artifacts.push(PeerArtifactRef {
            artifact_id: digest.clone(),
            sha256: digest.clone(),
            filename: None,
            media_type: None,
            size_bytes: Some(bytes.len() as u64),
        });
        envelope
    };
    assert_eq!(
        send(&bob, &pairing, &reference("msg-1"), NOW + 110).status,
        202
    );

    // What `monkey peers clear` does, through the store it does it in.
    DaemonStore::open(&bob.paths)
        .unwrap()
        .delete_peer_traffic(&pairing.device_id)
        .expect("clear");

    assert_eq!(
        send(&bob, &pairing, &reference("msg-2"), NOW + 120).status,
        400
    );
    // The blob is untouched; only the standing to name it was withdrawn.
    let store = super::peer_ingress::peer_content_store(&bob.paths).unwrap();
    assert_eq!(store.read(&digest).unwrap(), bytes);

    // Uploading again restores it, which is the only thing that should.
    assert_eq!(
        put_artifact(&bob, &pairing, &bytes, Some("t.log"), None, NOW + 130).status,
        201
    );
    assert_eq!(
        send(&bob, &pairing, &reference("msg-3"), NOW + 140).status,
        202
    );
}

/// A paired peer that keeps sending correctly signed rubbish is the case the
/// refusal table has to survive, and it survives it on disk rather than only in
/// the query that reads it.
#[test]
fn a_flood_of_malformed_traffic_cannot_grow_the_refusal_table_without_bound() {
    let (alice, mut bob, pairing) = two_nodes(every_grant());
    let limit = super::peer_store::MAX_PEER_REJECTION_EVENTS_PER_PEER;

    let mut sent = 0u32;
    while sent < limit + 25 {
        let mut looped = envelope(
            &alice,
            &format!("msg-{sent}"),
            PeerMessageKind::Message,
            "round and round",
        );
        looped.origin_chain.push(bob.instance_id.clone());
        assert_eq!(
            send(&bob, &pairing, &looped, NOW + 100 + u64::from(sent)).status,
            400
        );
        sent += 1;
    }

    let stored = DaemonStore::open(&bob.paths).unwrap();
    assert_eq!(stored.peer_rejection_event_count(None).unwrap(), limit);
    // Security Doctor still has what it needs: the newest refusals, with a
    // reason and a pairing and no peer text at all.
    let events = stored.peer_rejection_events(500).unwrap();
    assert!(events.iter().all(|event| event.reason == "origin_loop"));
    assert!(events
        .iter()
        .all(|event| event.peer_device_id == pairing.device_id));
    drop(stored);

    // The bound is durable, and stays a bound afterwards.
    bob.restart();
    for extra in 0..10u32 {
        let mut spent = envelope(
            &alice,
            &format!("late-{extra}"),
            PeerMessageKind::Message,
            "one hop too far",
        );
        spent.hop_limit = 0;
        assert_eq!(
            send(&bob, &pairing, &spent, NOW + 10_000 + u64::from(extra)).status,
            400
        );
    }
    let stored = DaemonStore::open(&bob.paths).unwrap();
    assert_eq!(stored.peer_rejection_event_count(None).unwrap(), limit);
    assert!(bob.queued().is_empty());
}

/// The whole thing, once, in the order an operator would live it.
///
/// Each property here has its own test above; what this one proves is that they
/// hold *together* on one pair of installations, across a restart, with nothing
/// reset in between — which is the only way to catch a guarantee that quietly
/// depends on starting from an empty database.
#[test]
fn two_installations_run_the_whole_peer_lifecycle_between_them() {
    let alice = Node::start("instance-alice");
    let carol = Node::start("instance-carol");
    let mut bob = Node::start("instance-bob");
    let alice_at_bob = bob.admit_peer(every_grant(), "alice");
    let carol_at_bob = bob.admit_peer(every_grant(), "carol");

    // 1. Hello: advertised, requested and granted stay three different things.
    let hello = PeerHelloRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        instance_id: alice.instance_id.clone(),
        advertised: every_grant(),
        requested: every_grant(),
    };
    let answered = call(
        &bob,
        &alice_at_bob,
        "POST",
        "/v1/remote/peer/hello",
        &serde_json::to_vec(&hello).unwrap(),
        NOW + 100,
    );
    assert_eq!(answered.status, 200);
    let answered: PeerHelloResponse = serde_json::from_slice(&answered.body).unwrap();
    assert_eq!(answered.granted, every_grant());

    // 2. An ordinary message becomes exactly one durable turn.
    assert_eq!(
        send(
            &bob,
            &alice_at_bob,
            &envelope(&alice, "msg-hello", PeerMessageKind::Message, "morning"),
            NOW + 200,
        )
        .status,
        202
    );
    assert_eq!(bob.queued().len(), 1);

    // 3. A task request runs under Bob's own recipe, and the sender cannot
    //    reach Bob's execution authority through it.
    let mut task = envelope(
        &alice,
        "msg-task",
        PeerMessageKind::TaskRequest,
        "find out why the build is red",
    );
    task.correlation_id = Some("corr-123".into());
    assert_eq!(send(&bob, &alice_at_bob, &task, NOW + 300).status, 202);
    let queued = bob.queued();
    assert_eq!(queued.len(), 2);
    assert_eq!(
        queued[1].target.recipe,
        super::peer_ingress::PEER_TASK_RECIPE
    );
    let options = super::channel_ingress::queue_options_for(&queued[1], Vec::new());
    assert!(options.repository.is_none());
    assert!(!options.allow_commit);
    assert!(options.snapshot_is_frozen);

    // 4. Bob's run finishes, and Alice's poll of the thread she opened carries
    //    one correlated result.
    let job_id = job_id_of(&bob, 1);
    finish_run(
        &bob,
        &job_id,
        Ending::Succeeded,
        "a bad migration",
        NOW + 400,
    );
    let answer = read_thread(&bob, &alice_at_bob, "thread-1", NOW + 500);
    let rows = results(&answer);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["correlation_id"], "corr-123");
    assert_eq!(rows[0]["payload"]["state"], "succeeded");
    assert_eq!(rows[0]["payload"]["text"], "a bad migration");

    // 5. Content: Alice uploads, Alice may reference, Carol may not.
    let bytes = b"the migration log".to_vec();
    let digest = super::remote::protocol::sha256_hex(&bytes);
    assert_eq!(
        put_artifact(
            &bob,
            &alice_at_bob,
            &bytes,
            Some("migration.log"),
            Some("text/plain"),
            NOW + 600,
        )
        .status,
        201
    );
    let mut carried = envelope(&alice, "msg-file", PeerMessageKind::Artifact, "the log");
    carried.artifacts.push(PeerArtifactRef {
        artifact_id: digest.clone(),
        sha256: digest.clone(),
        filename: Some("migration.log".into()),
        media_type: Some("text/plain".into()),
        size_bytes: Some(bytes.len() as u64),
    });
    assert_eq!(send(&bob, &alice_at_bob, &carried, NOW + 610).status, 202);

    let mut carols = carried.clone();
    carols.thread_id = "thread-carol".into();
    carols.sender_instance_id = carol.instance_id.clone();
    carols.origin_chain = vec![carol.instance_id.clone()];
    assert_eq!(send(&bob, &carol_at_bob, &carols, NOW + 620).status, 400);

    // 6. Redelivery, loops and expiry: refused, and refused cheaply.
    assert_eq!(send(&bob, &alice_at_bob, &task, NOW + 700).status, 200);
    let mut looped = envelope(&alice, "msg-loop", PeerMessageKind::Message, "again");
    looped.origin_chain.push(bob.instance_id.clone());
    assert_eq!(send(&bob, &alice_at_bob, &looped, NOW + 710).status, 400);
    let stale = envelope(&alice, "msg-stale", PeerMessageKind::Message, "old news");
    assert_eq!(
        send(
            &bob,
            &alice_at_bob,
            &stale,
            u64::try_from(stale.expires_at_ms).unwrap() + 1,
        )
        .status,
        400
    );
    let telemetry = DaemonStore::open(&bob.paths).unwrap();
    assert!(
        telemetry.peer_rejection_event_count(None).unwrap()
            <= super::peer_store::MAX_PEER_REJECTION_EVENTS_PER_PEER * 2
    );
    drop(telemetry);
    let ran_before_restart = bob.queued().len();
    assert_eq!(
        ran_before_restart, 3,
        "message, task, artifact — and nothing else"
    );

    // 7. Restart: the same task does not run again, the result is still one
    //    result, and the admission still belongs to Alice alone.
    bob.restart();
    assert_eq!(send(&bob, &alice_at_bob, &task, NOW + 800).status, 200);
    assert!(bob.queued().is_empty(), "nothing re-ran after the restart");
    assert_eq!(
        results(&read_thread(&bob, &alice_at_bob, "thread-1", NOW + 810)).len(),
        1
    );
    let mut carols_again = carols.clone();
    carols_again.message_id = "msg-file-again".into();
    assert_eq!(
        send(&bob, &carol_at_bob, &carols_again, NOW + 820).status,
        400
    );

    // 8. Revocation is immediate, for everything.
    bob.revoke(&alice_at_bob.device_id);
    let after = send(
        &bob,
        &alice_at_bob,
        &envelope(
            &alice,
            "msg-after",
            PeerMessageKind::Message,
            "still there?",
        ),
        NOW + 900,
    );
    assert!(
        after.status >= 400,
        "a revoked peer is refused, got {}",
        after.status
    );
    assert!(bob.queued().is_empty());
}

/// Rotating a peer's key replaces the credential and nothing else.
///
/// Three properties, and the third is the one worth guarding: the old key stops
/// working the moment the bundle exists, the new key works as soon as the peer
/// takes it up, and the *authority* on both sides of the rotation is identical
/// — the grants do not widen, and the pairing is still the same pairing rather
/// than a new one that happens to be called the same thing.
#[test]
fn rotating_a_peers_key_replaces_the_credential_without_widening_anything() {
    let (alice, bob, pairing) = two_nodes(BTreeSet::from([DeviceCapability::PeerMessage]));
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

    let before = RemoteStore::open(&bob.paths.root)
        .unwrap()
        .device(&pairing.device_id)
        .unwrap()
        .expect("paired");
    let bundle = RemoteStore::open(&bob.paths.root)
        .unwrap()
        .rotate_device(
            &pairing.device_id,
            &bob.instance_id,
            "https://bob.invalid",
            "-----BEGIN CERTIFICATE-----\nnot a real one\n-----END CERTIFICATE-----",
            &"a".repeat(64),
            NOW + 200,
            bob.secrets.as_ref(),
        )
        .expect("rotate");

    // The old key is refused immediately, before the peer has been told
    // anything. That order is the point: a window in which both keys work is a
    // window in which a leaked key still works.
    let stale = send(
        &bob,
        &pairing,
        &envelope(
            &alice,
            "msg-2",
            PeerMessageKind::Message,
            "with the old key",
        ),
        NOW + 300,
    );
    assert!(
        stale.status == 401 || stale.status == 403,
        "the previous key must stop working, got {}",
        stale.status
    );

    // The peer takes up the bundle and is itself again.
    let rotated = Pairing {
        device_id: bundle.device_id.clone(),
        secret: bundle.device_secret.as_bytes().to_vec(),
        generation: bundle.secret_generation,
        // The replay window is the pairing's, not the key's: a rotation
        // replaces the secret and leaves the sequence a peer has already spent
        // where it was.
        next_sequence: Mutex::new(*pairing.next_sequence.lock().unwrap()),
    };
    assert_eq!(
        send(
            &bob,
            &rotated,
            &envelope(
                &alice,
                "msg-3",
                PeerMessageKind::Message,
                "with the new key"
            ),
            NOW + 400,
        )
        .status,
        202
    );

    // Same pairing, same standing, one generation on. A rotation that quietly
    // handed out a capability would be the worst kind of widening: the
    // operator's last decision about this peer was the grant list, not this.
    let after = RemoteStore::open(&bob.paths.root)
        .unwrap()
        .device(&pairing.device_id)
        .unwrap()
        .expect("still paired");
    assert_eq!(after.device_id, before.device_id);
    assert_eq!(after.capabilities, before.capabilities);
    assert_eq!(
        after.capabilities,
        BTreeSet::from([DeviceCapability::PeerMessage])
    );
    assert_eq!(bundle.capabilities, before.capabilities);
    assert_eq!(after.secret_generation, before.secret_generation + 1);
    // And the far side still has to ask for the task grant it never had.
    let refused = send(
        &bob,
        &rotated,
        &envelope(&alice, "msg-4", PeerMessageKind::TaskRequest, "do my work"),
        NOW + 500,
    );
    assert_eq!(refused.status, 403);
    assert_eq!(bob.queued().len(), 2, "only the two messages ran");
}

/// A revoked peer cannot be rotated back into existence.
#[test]
fn a_revoked_peer_cannot_be_rotated_back_into_service() {
    let (_alice, bob, pairing) = two_nodes(every_grant());
    bob.revoke(&pairing.device_id);

    let error = RemoteStore::open(&bob.paths.root)
        .unwrap()
        .rotate_device(
            &pairing.device_id,
            &bob.instance_id,
            "https://bob.invalid",
            "-----BEGIN CERTIFICATE-----\nnot a real one\n-----END CERTIFICATE-----",
            &"a".repeat(64),
            NOW + 300,
            bob.secrets.as_ref(),
        )
        .expect_err("a revoked pairing has no key to replace");
    assert!(error.contains("Revoked"), "{error}");
}

/// Every layer has to agree about how much a peer may hand over.
///
/// Five separate limits sit on this path — the HTTP body cap, the base64 length
/// check that runs before anything is decoded, the content store's own blob
/// ceiling, the size an envelope may declare, and the size recorded on the
/// receipt — and a disagreement between any two of them is a bug shaped like
/// "it worked in testing": either a peer is refused after 42 MiB has already
/// been buffered, or it is accepted at a size some later layer cannot hold.
///
/// Asserted arithmetically rather than by transferring 32 MiB, which would cost
/// every test run a great deal of memory to prove one inequality. The behaviour
/// at a real (small) size is proven below it.
#[test]
fn every_layer_agrees_on_what_a_peer_may_hand_over() {
    use super::remote::protocol::{MAX_REMOTE_ARTIFACT_BYTES, MAX_REMOTE_BODY_BYTES};
    let ceiling = little_monkey_lib::peers::MAX_PEER_ARTIFACT_BYTES;

    // The envelope's ceiling and the remote plane's are one number.
    assert_eq!(ceiling, MAX_REMOTE_ARTIFACT_BYTES);
    assert_eq!(ceiling, 32 * 1024 * 1024);
    // The body cap has to leave room for the base64 expansion plus metadata,
    // or the encoded-length refusal below could never be the one that fires.
    let encoded_ceiling = ceiling as usize * 4 / 3 + 4;
    assert!(
        MAX_REMOTE_BODY_BYTES > encoded_ceiling,
        "the body cap ({MAX_REMOTE_BODY_BYTES}) must exceed the encoded artifact ceiling ({encoded_ceiling})"
    );
    // An oversized upload is refused on the encoded length, before a byte is
    // decoded — the check that keeps a rejection cheap.
    let oversized = PeerArtifactUpload {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        sha256: "a".repeat(64),
        content_base64: "A".repeat(encoded_ceiling + 1),
        filename: None,
        media_type: None,
    };
    assert!(oversized.validate().is_err());
    // And the content store itself refuses past the same ceiling, so the limit
    // holds even for a caller that never went through `validate`.
    let (alice, bob, pairing) = two_nodes(every_grant());
    let store = super::peer_ingress::peer_content_store(&bob.paths).unwrap();
    assert_eq!(store.max_blob_size(), ceiling);

    // A real transfer at a real size, end to end, with the receipt agreeing.
    let bytes = vec![b'x'; 512 * 1024];
    let digest = super::remote::protocol::sha256_hex(&bytes);
    assert_eq!(
        put_artifact(&bob, &pairing, &bytes, Some("big.log"), None, NOW + 100).status,
        201
    );
    let mut carried = envelope(&alice, "msg-1", PeerMessageKind::Artifact, "a big one");
    carried.artifacts.push(PeerArtifactRef {
        artifact_id: digest.clone(),
        sha256: digest.clone(),
        filename: None,
        media_type: None,
        size_bytes: Some(bytes.len() as u64),
    });
    assert_eq!(send(&bob, &pairing, &carried, NOW + 110).status, 202);
    assert_eq!(
        bob.queued()[0].attachments[0].declared_size_bytes,
        Some(bytes.len() as u64)
    );

    // An envelope that claims more than a peer may ever offer never reaches the
    // receipt at all.
    let mut absurd = carried.clone();
    absurd.message_id = "msg-2".into();
    absurd.artifacts[0].size_bytes = Some(ceiling + 1);
    assert_eq!(send(&bob, &pairing, &absurd, NOW + 120).status, 400);
}
