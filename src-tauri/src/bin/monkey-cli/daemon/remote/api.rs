use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use little_monkey_lib::artifact_store::ArtifactStore;
use little_monkey_lib::migration::{
    admit, MigrationVerdict, TargetNode, MAX_MIGRATION_PAYLOAD_BYTES,
};
use little_monkey_lib::run_ledger::{RunLedger, StoredApproval, StoredRun};
use little_monkey_lib::run_protocol::{
    ClientIdentity, ClientKind, ModelTargetSnapshot, PermissionDecision, RunEvent,
    RunEventEnvelope, RUN_PROTOCOL_SCHEMA_VERSION,
};
use serde::Serialize;

use crate::daemon::ledger::SharedLedger;
use crate::daemon::store::{DaemonPaths, DaemonStore};
use crate::durable_run::{bounded_text, CliRunEventSink, DurableRunRecorder};

use little_monkey_lib::run_protocol::OutputChannel;

use super::desktop::DesktopControlRuntime;
use super::migrate::land_migration;
use super::protocol::{
    all_peer_capabilities, canonical_request, capability_block, effective_capabilities,
    legacy_capabilities, peer_capabilities_of, sha256_hex, terminal_digest, ApprovalRequestBody,
    CancelRequestBody, DesktopControlActionRequest, DesktopControlStartRequest,
    DesktopControlStopRequest, DeviceCapability, DeviceCommand, DeviceCommandControl,
    DeviceCommandRecovery, DeviceCommandResult, DeviceCommandStartRequest, DeviceCommandState,
    DeviceSurface, MigrationAcceptRequest, MigrationPreflightRequest, MigrationReceipt,
    PairAcceptRequest, PeerArtifactStored, PeerArtifactUpload, PeerHelloRequest, PeerHelloResponse,
    RemoteAction, RemoteHostConfig, RemoteScopes, RunSummary, SignedRequestHeaders,
    TalkTicketRequest, TalkTicketResponse, VoiceChunkRequest, VoiceCloseRequest,
    DEFAULT_TALK_TICKET_TTL_MS, DEVICE_LEASE_MS, MAX_REMOTE_BODY_BYTES, MAX_VOICE_CHUNK_BYTES,
    PHYSICAL_DEVICE_CAPABILITIES, REMOTE_PROTOCOL_VERSION,
};
use super::store::{
    CommandReservation, DeviceArtifact, DeviceRecord, KeyringRemoteSecrets, MobileCaptureRecord,
    MobileMessageRecord, MobileWorkflowRunRecord, RemoteSecretStore, RemoteStore,
};

/// Seam through which the mobile chat route reaches the daemon's recipe
/// queue. Production (`daemon::DaemonMobileChatQueue`) queues the
/// operator-configured `mobile-chat` recipe; tests inject a fake so the API
/// contract is testable without a configured daemon.
pub trait MobileChatQueue: Send + Sync {
    /// Queues one chat turn. `client_key` is the mobile message id — the
    /// implementation derives a deterministic job id from it, so replaying
    /// the same signed request can never double-queue. `session_id` is the
    /// conversation the turn belongs to, which is what the durable ingress
    /// record keys its session on. Returns the durable run id.
    fn queue_chat(
        &self,
        session_id: &str,
        client_key: &str,
        prompt: &str,
    ) -> Result<String, String>;
    /// Resolves the durable run id previously queued for this turn, if the job
    /// has one yet. Both halves of the turn's identity are needed: the job id
    /// is a digest over them and cannot be inverted.
    fn chat_run_id(&self, session_id: &str, client_key: &str) -> Result<Option<String>, String>;
}

/// Seam through which the placement route reaches this node's own queue
/// (roadmap K17 S2).
///
/// The same shape as [`MobileChatQueue`] and for the same reason: the route's
/// contract — validate a foreign spec, refuse what this node cannot satisfy,
/// record the placement — is testable without a configured daemon, while
/// production (`daemon::DaemonPlacementQueue`) does the real enqueue.
pub trait PlacementQueue: Send + Sync {
    /// Accepts a frozen foreign spec and queues it here.
    ///
    /// The implementation owns the refusal for anything about *this* machine
    /// that the spec needs and this machine has not got — a workspace root that
    /// does not exist, a model target this node cannot execute — because those
    /// are exactly the facts the wire cannot carry.
    fn place(&self, spec: &little_monkey_lib::run_protocol::RunSpec) -> Result<PlacedJob, String>;
    /// Current state of a previously placed run, by the node-side job id.
    ///
    /// Keyed on the job rather than the run because the job row is what carries
    /// the *node's* verdict — its hold reason, its spawn failure, its budget
    /// cancellation — and that verdict is what a placer needs to read.
    fn placed_state(&self, job_id: &str) -> Result<Option<PlacedJobState>, String>;
}

/// What the node minted for one accepted placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedJob {
    pub node_run_id: String,
    pub job_id: String,
    pub state: String,
}

/// A placed run's current state as the node sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedJobState {
    pub state: String,
    pub terminal: bool,
    pub updated_at_ms: u64,
    pub last_error: Option<String>,
}

/// Every model resident on this node: the managed hub's inventory plus whatever
/// the local Ollama daemon has pulled (roadmap K17 S1).
///
/// # The synchronous-handler problem, and why it is not solved by giving up
///
/// Listing Ollama tags is one async loopback GET, and [`RemoteApi::handle`] is
/// synchronous — it is called from `handle_http`, which is not. The first cut of
/// this simply omitted Ollama, and the cost was real rather than cosmetic:
/// `select_node`'s strongest ranking key is "the model is already resident", and
/// a node's Ollama models are exactly the local models a placement would want to
/// avoid re-pulling. A whole class of placements silently ranked as if every
/// node were cold.
///
/// `block_in_place` moves this blocking section off the async worker so the
/// runtime can keep serving, which is precisely what it exists for. It is
/// **only** reached on a multi-threaded runtime — `block_in_place` panics on a
/// current-thread one, and unit tests call `handle` with no runtime at all — so
/// the flavour is checked first and the absence of a runtime degrades to "hub
/// models only" rather than to a panic in a route handler.
///
/// A daemon whose Ollama is not running is not an error either: an unreachable
/// Ollama contributes nothing and the node still describes itself.
fn resident_models(
    app_data: &std::path::Path,
) -> Vec<little_monkey_lib::node_placement::NodeModel> {
    let mut models = little_monkey_lib::m3_runtime_hub::installed_model_inventory(app_data);
    let known: std::collections::BTreeSet<String> =
        models.iter().map(|model| model.model_id.clone()).collect();
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return models;
    };
    if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
        return models;
    }
    let tags = tokio::task::block_in_place(|| {
        handle.block_on(async {
            let client = little_monkey_lib::egress::hardened()
                .build()
                .map_err(|error| error.to_string())?;
            little_monkey_lib::ollama::list_tag_names(&client).await
        })
    });
    let Ok(tags) = tags else {
        return models;
    };
    for tag in tags {
        if known.contains(&tag) {
            continue;
        }
        models.push(little_monkey_lib::node_placement::NodeModel {
            model_id: tag.clone(),
            display_name: tag,
            runtime: "ollama".to_string(),
            // Ollama's tag listing carries a size, but this route deliberately
            // does not ask for it: `/api/tags` reports the blob size on disk,
            // which is not the memory footprint the hub's numbers mean, and one
            // field holding two different measurements is worse than a zero that
            // is obviously not a measurement.
            weights_bytes: 0,
            estimated_ram_bytes: 0,
            estimated_vram_bytes: 0,
        });
    }
    models.sort_by(|left, right| left.model_id.cmp(&right.model_id));
    models
}

/// Meta keys holding the two operator statements a node makes about itself
/// (roadmap K17 S1). In the daemon's own meta table rather than in
/// `RemoteHostConfig` because they are facts about the *machine*, not about its
/// TLS listener, and an operator who has not configured a remote host can still
/// set them.
pub const NODE_RESIDENCY_META: &str = "node_residency";
pub const NODE_NAME_META: &str = "node_name";

#[derive(Debug, Clone)]
pub struct ApiRequest {
    pub method: String,
    pub path_and_query: String,
    pub body: Vec<u8>,
    pub auth: Option<SignedRequestHeaders>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

/// Unspent admissions held at once. A ticket lives thirty seconds and is spent
/// immediately, so this is a ceiling on a burst rather than on conversations.
const MAX_PENDING_TALK_TICKETS: usize = 64;

#[derive(Debug, Clone)]
struct PendingTalkTicket {
    device_id: String,
    secret_generation: u64,
    signed_request_sha256: String,
    session_id: String,
    session_generation: String,
    expires_at_ms: u64,
}

/// Identity frozen into a consumed Talk ticket. The ticket itself is removed
/// before the HTTP 101 is returned and is never retained in this value.
#[derive(Debug, Clone)]
pub(crate) struct TalkSocketAuthorization {
    pub device_id: String,
    /// Digest of the signed request that minted this admission. Every turn the
    /// socket submits is keyed on it, so a spoken turn's durable identity traces
    /// back to a request that carried a valid signature, sequence and nonce.
    pub signed_request_sha256: String,
    pub session_id: String,
    pub session_generation: String,
}

impl ApiResponse {
    fn json<T: Serialize>(status: u16, value: &T) -> Self {
        match serde_json::to_vec(value) {
            Ok(body) => Self {
                status,
                content_type: "application/json",
                body,
            },
            Err(error) => Self::error(500, &format!("Serialization failure: {error}")),
        }
    }

    pub(super) fn error(status: u16, message: &str) -> Self {
        Self::json(
            status,
            &serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "status": "error",
                "message": bounded_text(message, 4_096),
            }),
        )
    }
}

pub struct RemoteApi {
    paths: DaemonPaths,
    host: RemoteHostConfig,
    store: Arc<Mutex<RemoteStore>>,
    secrets: Arc<dyn RemoteSecretStore>,
    /// Shared with the resident serve loop so revoke / kill-switch / escape
    /// hatch can force-stop the same live sessions this API creates. `None`
    /// only in desktop-agnostic unit tests.
    desktop: Option<Arc<DesktopControlRuntime>>,
    /// Chat execution seam for `/v1/remote/mobile/sessions/*/messages`.
    /// `None` (bare unit tests) answers those routes with a clear 501-style
    /// error instead of pretending to queue anything.
    mobile_chat: Option<Arc<dyn MobileChatQueue>>,
    /// Placement execution seam for `/v1/remote/node/runs` (roadmap K17 S2).
    /// `None` (bare unit tests, and any build without a configured daemon)
    /// answers the placement route with an explicit refusal rather than
    /// accepting a spec it cannot run.
    placement: Option<Arc<dyn PlacementQueue>>,
    /// Run seam for `/v1/remote/peer/messages`. `None` (bare unit tests, and
    /// any build without a configured daemon) refuses peer traffic outright
    /// rather than recording envelopes it could never act on.
    peer_runs: Option<Arc<dyn crate::daemon::channel_worker::RunQueue>>,
    /// One lock per device command, held across its whole terminal commit.
    ///
    /// The commit is "decide whether this report is authoritative, publish its
    /// artifact bytes, then write the row that names them", and those three are
    /// one decision: a second report that lost the race must leave the winner's
    /// file *and* row exactly as they are. Checking the row, releasing, writing
    /// the file and taking the row again leaves a window where the loser's bytes
    /// replace the winner's under the winner's digest.
    ///
    /// Deliberately not the store lock: an artifact fsync is long, and every
    /// other request would queue behind it. Deliberately in memory: this API is
    /// one process, cloned per connection over shared `Arc`s, so every task that
    /// can commit a given command shares this map.
    terminal_commits: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// Where remote requests land in the unified subsystem event stream
    /// (roadmap K12).
    ///
    /// The remote node already keeps its own `remote_audit` table, but that
    /// table lives in its own database with no join to the run stream — which is
    /// the gap K12 names. This records the same requests where everything else
    /// can be read alongside them; it does not replace `remote_audit`, which
    /// holds the protocol-level denial detail this stream deliberately does not.
    audit: little_monkey_lib::subsystem_audit::SubsystemAudit,
    /// Short-lived, one-use WebSocket admissions keyed by a digest of the
    /// opaque ticket. Device secrets never enter this map or a URL.
    talk_tickets: Arc<Mutex<HashMap<String, PendingTalkTicket>>>,
    /// Speech backends for Talk sockets. `None` — always, in production — means
    /// the operator's own configured stack, resolved per session. A test
    /// substitutes the two things that are genuinely outside this process, a
    /// transcriber and a synthesizer, and nothing else.
    talk_speech: Option<Arc<dyn super::talk::TalkSpeech>>,
}

impl Clone for RemoteApi {
    fn clone(&self) -> Self {
        Self {
            paths: self.paths.clone(),
            host: self.host.clone(),
            store: Arc::clone(&self.store),
            secrets: Arc::clone(&self.secrets),
            desktop: self.desktop.clone(),
            mobile_chat: self.mobile_chat.clone(),
            placement: self.placement.clone(),
            peer_runs: self.peer_runs.clone(),
            // Shared, not copied: two clones that each had their own map would
            // be two locks over one command, which is no lock at all.
            terminal_commits: Arc::clone(&self.terminal_commits),
            audit: self.audit.clone(),
            talk_tickets: Arc::clone(&self.talk_tickets),
            talk_speech: self.talk_speech.clone(),
        }
    }
}

impl RemoteApi {
    pub fn production(
        paths: DaemonPaths,
        host: RemoteHostConfig,
        desktop: Arc<DesktopControlRuntime>,
        mobile_chat: Arc<dyn MobileChatQueue>,
        placement: Arc<dyn PlacementQueue>,
        peer_runs: Arc<dyn crate::daemon::channel_worker::RunQueue>,
    ) -> Result<Self, String> {
        let store = RemoteStore::open(&paths.root)?;
        let audit = audit_for(&paths);
        Ok(Self {
            paths,
            host,
            store: Arc::new(Mutex::new(store)),
            secrets: Arc::new(KeyringRemoteSecrets),
            desktop: Some(desktop),
            mobile_chat: Some(mobile_chat),
            placement: Some(placement),
            peer_runs: Some(peer_runs),
            terminal_commits: Arc::new(Mutex::new(HashMap::new())),
            audit,
            talk_tickets: Arc::new(Mutex::new(HashMap::new())),
            talk_speech: None,
        })
    }

    /// The store this API answers from, for tests that need to queue work or
    /// read the authoritative record beside the protocol.
    #[cfg(test)]
    pub fn store_for_tests(&self) -> Arc<Mutex<RemoteStore>> {
        Arc::clone(&self.store)
    }

    #[cfg(test)]
    pub fn injected(
        paths: DaemonPaths,
        host: RemoteHostConfig,
        store: RemoteStore,
        secrets: Arc<dyn RemoteSecretStore>,
    ) -> Self {
        let audit = audit_for(&paths);
        Self {
            paths,
            host,
            store: Arc::new(Mutex::new(store)),
            secrets,
            desktop: None,
            mobile_chat: None,
            placement: None,
            peer_runs: None,
            terminal_commits: Arc::new(Mutex::new(HashMap::new())),
            audit,
            talk_tickets: Arc::new(Mutex::new(HashMap::new())),
            talk_speech: None,
        }
    }

    /// Test builder: the injected API plus a fake chat queue, so the mobile
    /// chat contract is exercisable without a configured daemon.
    #[cfg(test)]
    pub fn with_mobile_chat(mut self, mobile_chat: Arc<dyn MobileChatQueue>) -> Self {
        self.mobile_chat = Some(mobile_chat);
        self
    }

    /// Test builder: the injected API plus a scripted transcriber and
    /// synthesizer, so a whole spoken conversation can be driven over a real
    /// socket without a whisper build or a system voice.
    #[cfg(test)]
    pub fn with_talk_speech(mut self, speech: Arc<dyn super::talk::TalkSpeech>) -> Self {
        self.talk_speech = Some(speech);
        self
    }

    pub(crate) fn talk_speech(&self) -> Option<Arc<dyn super::talk::TalkSpeech>> {
        self.talk_speech.clone()
    }

    /// Test builder: the injected API plus a fake placement queue, so the K17
    /// placement contract is exercisable without a configured daemon.
    #[cfg(test)]
    pub fn with_placement(mut self, placement: Arc<dyn PlacementQueue>) -> Self {
        self.placement = Some(placement);
        self
    }

    /// Test builder: the injected API plus a fake run queue, so the peer
    /// contract is exercisable without a configured daemon.
    #[cfg(test)]
    pub fn with_peer_runs(
        mut self,
        peer_runs: Arc<dyn crate::daemon::channel_worker::RunQueue>,
    ) -> Self {
        self.peer_runs = Some(peer_runs);
        self
    }

    /// Answer one remote request and record it.
    ///
    /// Every path through the API returns an `ApiResponse`, so this wrapper is a
    /// real choke point — unlike ACP's dispatch loop, no id-to-method
    /// bookkeeping is needed, because the request and its response are in scope
    /// together. `handle_request` below is the original body, unchanged.
    pub fn handle(&self, request: ApiRequest, now_ms: u64) -> ApiResponse {
        // Captured before the body is consumed. The query string is dropped: it
        // can carry run and session ids, and `detail_json` is covered by the
        // hash chain and therefore permanent.
        let action = format!(
            "{} {}",
            request.method,
            request
                .path_and_query
                .split('?')
                .next()
                .unwrap_or(&request.path_and_query)
        );
        let device_id = request
            .auth
            .as_ref()
            .map(|headers| headers.device_id.clone());
        let response = self.handle_request(request, now_ms);
        self.audit
            .record(little_monkey_lib::subsystem_audit::SubsystemAction {
                subsystem: little_monkey_lib::run_ledger::Subsystem::Remote,
                action,
                // A remote request is not a run — `run_scope::Unattributed`'s
                // `InboundRequest` is exactly this case — so the ambient scope is
                // the only honest source.
                turn_id: None,
                // Remote requests are signed, not permission-gated: authenticity
                // is proven by `verify_request`, not by a `request_permission`
                // decision, so there is none to point at.
                permission_request_id: None,
                outcome: little_monkey_lib::subsystem_audit::outcome_for_status(response.status),
                // The device is which paired client acted, which is the question
                // a reader of this row actually has. Never the body or the
                // signature.
                detail: device_id.map(|id| serde_json::json!({ "deviceId": id })),
            });
        response
    }

    /// [`Self::handle`], plus the one route that is allowed to wait.
    ///
    /// A phone that polled for work every few seconds would either burn its
    /// battery or answer commands late. Long-polling the lease route fixes both
    /// without a second general-purpose socket: the request is an ordinary
    /// signed one, it returns the moment a command exists, and it gives up at
    /// `wait_ms` (capped at the lease length) so no connection is held open
    /// indefinitely. Every other route is the unchanged synchronous path.
    pub async fn handle_waiting(&self, request: ApiRequest, now_ms: u64) -> ApiResponse {
        let (target, deadline_ms) = match long_poll_target(&request) {
            Some(value) => value,
            None => return self.handle(request, now_ms),
        };
        // The wait happens BEFORE dispatch, and the signed request is answered
        // exactly once at the end. Re-running it per tick would hit the replay
        // guard on the second pass and hand back the first tick's cached "no
        // work" answer for the rest of the wait.
        let Some(device_id) = request
            .auth
            .as_ref()
            .map(|headers| headers.device_id.clone())
        else {
            return self.handle(request, now_ms);
        };
        let started = std::time::Instant::now();
        let mut elapsed = 0u64;
        while elapsed < deadline_ms {
            // An unverified device id is enough to decide *whether to wait*: it
            // grants nothing, and the answer below still goes through the full
            // signature, revocation and replay checks.
            let ready = match &target {
                LongPollTarget::Lease => {
                    self.has_pending_device_command(&device_id, now_ms.saturating_add(elapsed))
                }
                LongPollTarget::Control(command_id) => {
                    self.command_control_changed(&device_id, command_id)
                }
            };
            if ready {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(
                LONG_POLL_TICK_MS.min(deadline_ms - elapsed),
            ))
            .await;
            elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        }
        self.handle(request, now_ms.saturating_add(elapsed))
    }

    fn has_pending_device_command(&self, device_id: &str, now_ms: u64) -> bool {
        self.store
            .lock()
            .ok()
            .and_then(|store| store.pending_device_command_count(device_id, now_ms).ok())
            .is_some_and(|count| count > 0)
    }

    /// Whether a control watcher has anything to hear yet: a cancellation
    /// asked for, or the command having left `running` under it.
    fn command_control_changed(&self, device_id: &str, command_id: &str) -> bool {
        self.store
            .lock()
            .ok()
            .and_then(|store| store.device_command(command_id).ok().flatten())
            .filter(|record| record.device_id == device_id)
            .is_none_or(|record| record.cancel_requested || record.state.terminal())
    }

    fn handle_request(&self, request: ApiRequest, now_ms: u64) -> ApiResponse {
        if request.body.len() > MAX_REMOTE_BODY_BYTES {
            return ApiResponse::error(413, "Remote request body exceeds 1 MiB");
        }
        if request.method == "POST" && request.path_and_query == "/v1/remote/pairings/accept" {
            return self.accept_pairing(&request.body, now_ms);
        }
        let Some(headers) = request.auth.as_ref() else {
            return ApiResponse::error(401, "Signed remote authentication is required");
        };
        if let Err(error) = headers.validate_shape(now_ms) {
            return ApiResponse::error(401, &error);
        }
        let device = match self
            .store
            .lock()
            .map_err(|_| "Remote state lock was poisoned".to_string())
            .and_then(|store| {
                store
                    .device(&headers.device_id)?
                    .ok_or_else(|| "Unknown remote device".to_string())
            }) {
            Ok(value) if value.active() => value,
            Ok(_) => return ApiResponse::error(401, "Remote device is revoked"),
            Err(error) => return ApiResponse::error(401, &error),
        };
        if device.secret_generation != headers.secret_generation {
            return ApiResponse::error(401, "Remote key generation is stale");
        }
        let secret = match self.secrets.get(&device.secret_slot()) {
            Ok(value) => value,
            Err(error) => return ApiResponse::error(503, &error),
        };
        if !super::protocol::verify_request(
            &secret,
            headers,
            &request.method,
            &request.path_and_query,
            &request.body,
        ) {
            self.audit_denied(
                now_ms,
                Some(&headers.device_id),
                "signature",
                None,
                "invalid_signature",
            );
            return ApiResponse::error(401, "Remote request signature is invalid");
        }
        let request_sha256 = sha256_hex(&canonical_request(
            headers,
            &request.method,
            &request.path_and_query,
            &request.body,
        ));
        let reservation = match self
            .store
            .lock()
            .map_err(|_| "Remote state lock was poisoned".to_string())
            .and_then(|mut store| {
                store.reserve_command(
                    &headers.device_id,
                    headers.secret_generation,
                    &headers.command_id,
                    &headers.nonce,
                    headers.sequence,
                    &request_sha256,
                    &request.method,
                    &request.path_and_query,
                    now_ms,
                )
            }) {
            Ok(value) => value,
            Err(error) => {
                self.audit_denied(
                    now_ms,
                    Some(&headers.device_id),
                    "replay_guard",
                    None,
                    &error,
                );
                return ApiResponse::error(409, &error);
            }
        };
        if let CommandReservation::Replay {
            status: Some(status),
            response_body: Some(body),
            processing: false,
        } = &reservation
        {
            return ApiResponse {
                status: *status,
                content_type: "application/json",
                body: body.clone(),
            };
        }
        let was_processing = matches!(
            reservation,
            CommandReservation::Replay {
                processing: true,
                ..
            }
        );
        let response = self.dispatch_authorized(&request, &device, &request_sha256, now_ms);
        if let Ok(mut store) = self.store.lock() {
            let _ = store.complete_command(
                &headers.device_id,
                &headers.command_id,
                response.status,
                &response.body,
                was_processing,
                now_ms,
            );
        }
        response
    }

    fn accept_pairing(&self, body: &[u8], now_ms: u64) -> ApiResponse {
        let request: PairAcceptRequest = match serde_json::from_slice(body) {
            Ok(value) => value,
            Err(error) => {
                return ApiResponse::error(400, &format!("Invalid pairing request: {error}"));
            }
        };
        if request.protocol_version != REMOTE_PROTOCOL_VERSION {
            return ApiResponse::error(400, "Unsupported remote protocol version");
        }
        let result = self
            .store
            .lock()
            .map_err(|_| "Remote state lock was poisoned".to_string())
            .and_then(|mut store| {
                let accepted = store.accept_invitation(
                    &request.pairing_id,
                    &request.pairing_token,
                    &request.device_name,
                    &self.host.runner_id,
                    now_ms,
                    self.secrets.as_ref(),
                )?;
                store.audit(
                    now_ms,
                    Some(&accepted.device_id),
                    "pair_accept",
                    Some(&request.pairing_id),
                    "allowed",
                    Some(&sha256_hex(body)),
                )?;
                Ok(accepted)
            });
        match result {
            Ok(value) => ApiResponse::json(201, &value),
            Err(error) => {
                self.audit_denied(now_ms, None, "pair_accept", None, &error);
                ApiResponse::error(403, &error)
            }
        }
    }

    fn dispatch_authorized(
        &self,
        request: &ApiRequest,
        device: &DeviceRecord,
        request_sha256: &str,
        now_ms: u64,
    ) -> ApiResponse {
        let scopes = &device.scopes;
        let device_id = device.device_id.as_str();
        let (path, query) = request
            .path_and_query
            .split_once('?')
            .map_or((request.path_and_query.as_str(), ""), |value| value);
        let segments = path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        let result = match (request.method.as_str(), segments.as_slice()) {
            ("GET", ["v1", "remote", "runs"]) => {
                require_action(scopes, RemoteAction::ViewRuns).and_then(|_| self.list_runs(scopes))
            }
            ("GET", ["v1", "remote", "runs", run_id]) => {
                require_action(scopes, RemoteAction::ViewRuns)
                    .and_then(|_| self.run_detail(scopes, run_id))
            }
            ("GET", ["v1", "remote", "runs", run_id, "events"]) => {
                require_action(scopes, RemoteAction::ViewEvents)
                    .and_then(|_| self.run_events(scopes, run_id, query))
            }
            ("GET", ["v1", "remote", "runs", run_id, "approvals"]) => {
                require_action(scopes, RemoteAction::ViewRuns)
                    .and_then(|_| self.run_approvals(scopes, run_id))
            }
            ("GET", ["v1", "remote", "runs", run_id, "artifacts", artifact_id]) => {
                require_action(scopes, RemoteAction::ReadArtifacts)
                    .and_then(|_| self.artifact(scopes, run_id, artifact_id))
            }
            ("POST", ["v1", "remote", "runs", run_id, "approve"]) => {
                require_action(scopes, RemoteAction::Approve)
                    .and_then(|_| self.approve(scopes, run_id, &request.body, device_id, now_ms))
            }
            ("POST", ["v1", "remote", "runs", run_id, "cancel"]) => {
                require_action(scopes, RemoteAction::Cancel)
                    .and_then(|_| self.cancel(scopes, run_id, &request.body, now_ms))
            }
            ("POST", ["v1", "remote", "runs", run_id, "pause"]) => {
                require_action(scopes, RemoteAction::Pause)
                    .and_then(|_| self.set_paused(scopes, run_id, true, now_ms))
            }
            ("POST", ["v1", "remote", "runs", run_id, "resume"]) => {
                require_action(scopes, RemoteAction::Pause)
                    .and_then(|_| self.set_paused(scopes, run_id, false, now_ms))
            }
            ("POST", ["v1", "remote", "kill"]) => require_action(scopes, RemoteAction::Kill)
                .and_then(|_| self.kill(device_id, now_ms)),
            ("POST", ["v1", "remote", "desktop-control", "start"]) => {
                require_action(scopes, RemoteAction::ControlDesktop)
                    .and_then(|_| self.desktop_control_start(&request.body, device_id))
            }
            ("POST", ["v1", "remote", "desktop-control", "action"]) => {
                require_action(scopes, RemoteAction::ControlDesktop)
                    .and_then(|_| self.desktop_control_action(&request.body, device_id))
            }
            ("POST", ["v1", "remote", "desktop-control", "stop"]) => {
                require_action(scopes, RemoteAction::ControlDesktop)
                    .and_then(|_| self.desktop_control_stop(&request.body, device_id))
            }
            // --- Versioned `/v1/remote/mobile/*` extension (first-party
            // mobile companion). Chat and workflow launch use the dedicated
            // capability grant; a legacy pairing without capabilities
            // resolves through `legacy_capabilities`, which never includes
            // the mobile-only grants — so an old runner pairing cannot be
            // escalated into a chat surface by a client-side update.
            ("GET", ["v1", "remote", "mobile", "sessions"]) => {
                require_capability(device, DeviceCapability::ViewSessions)
                    .and_then(|_| self.mobile_sessions())
            }
            ("GET", ["v1", "remote", "mobile", "sessions", session_id, "messages"]) => {
                require_capability(device, DeviceCapability::ViewSessions)
                    .and_then(|_| self.mobile_messages_get(session_id, now_ms))
            }
            ("POST", ["v1", "remote", "mobile", "sessions", session_id, "messages"]) => {
                require_capability(device, DeviceCapability::Chat).and_then(|_| {
                    self.mobile_message_post(
                        session_id,
                        &request.body,
                        device_id,
                        request_sha256,
                        now_ms,
                    )
                })
            }
            ("GET", ["v1", "remote", "mobile", "workflows"]) => {
                require_capability(device, DeviceCapability::ViewTasks)
                    .and_then(|_| self.mobile_workflows())
            }
            ("POST", ["v1", "remote", "mobile", "workflows", workflow_id, "runs"]) => {
                require_capability(device, DeviceCapability::RunWorkflows)
                    .and_then(|_| self.mobile_workflow_launch(workflow_id, device_id, now_ms))
            }
            ("POST", ["v1", "remote", "mobile", "captures"]) => {
                require_capability(device, DeviceCapability::Capture).and_then(|_| {
                    self.mobile_capture_post(&request.body, device, request_sha256, now_ms)
                })
            }
            // --- Versioned `/v1/remote/device/*` plane: the runner asking the
            // phone's own hardware for something.
            //
            // The first three routes are gated by device authentication alone
            // and by no capability. That is deliberate and is not a hole:
            // advertising a surface can only ever *narrow* what is effective
            // (see `protocol::effective_capabilities`), reading one's own grant
            // record discloses nothing the device was not already told at
            // pairing, and the queue hands out only commands already queued
            // *for this device* — each of which was capability-checked when it
            // was queued and is re-checked at lease time below. Requiring a
            // grant to advertise would instead deadlock the design: a device
            // granted only `camera_capture` could never advertise a camera, so
            // the camera would never become effective.
            //
            // What they are *not* gated by is peer standing, which is why each
            // one refuses a peer-only pairing outright below: a peer has no
            // hardware here, nothing is ever queued for it, and nothing about
            // the plane that serves a phone should answer it at all. Without
            // that, "a peer reaches only the peer plane" would be true of the
            // routes that happen to check a capability and false of the ones
            // that deliberately do not.
            ("POST", ["v1", "remote", "device", "surface"]) => refuse_peer_only(device)
                .and_then(|_| self.device_surface_post(&request.body, device, now_ms)),
            ("GET", ["v1", "remote", "device", "state"]) => {
                refuse_peer_only(device).and_then(|_| self.device_state(device))
            }
            ("GET", ["v1", "remote", "device", "commands", "next"]) => {
                refuse_peer_only(device).and_then(|_| self.device_command_lease(device, now_ms))
            }
            // Reconciliation, never a second lease: the commands this device
            // started and never finished, so a reconnect can deliver a staged
            // result or say honestly that the outcome is unknown.
            ("GET", ["v1", "remote", "device", "commands", "recover"]) => {
                refuse_peer_only(device).and_then(|_| self.device_commands_recover(device, now_ms))
            }
            ("GET", ["v1", "remote", "device", "commands", command_id, "control"]) => {
                self.device_command_control(device, command_id, now_ms)
            }
            ("POST", ["v1", "remote", "device", "commands", command_id, "start"]) => {
                self.device_command_start(&request.body, device, command_id, now_ms)
            }
            ("POST", ["v1", "remote", "device", "commands", command_id, "result"]) => {
                self.device_command_result(&request.body, device, command_id, now_ms)
            }
            // The audio of a live stream, while its control command is still
            // running. Gated on the grant — an operator who revokes
            // `voice_stream` mid-stream closes the microphone with the next
            // chunk — and on owning the session, which the device was told
            // about in the command it leased and never invents for itself.
            ("POST", ["v1", "remote", "device", "voice", session_id, "chunk"]) => {
                require_capability(device, DeviceCapability::VoiceStream)
                    .and_then(|_| self.voice_chunk(&request.body, device, session_id, now_ms))
            }
            ("POST", ["v1", "remote", "device", "voice", session_id, "close"]) => {
                require_capability(device, DeviceCapability::VoiceStream)
                    .and_then(|_| self.voice_close(&request.body, device, session_id, now_ms))
            }
            // A live conversation, not a recording. The ticket is the whole of
            // the authentication story for the socket that follows: a browser
            // cannot put signed headers on a WebSocket handshake, so the device
            // proves itself here — with the same signature, sequence, nonce and
            // key generation as any other route — and receives a one-use,
            // 30-second bearer it immediately spends. See `consume_talk_ticket`.
            ("POST", ["v1", "remote", "device", "talk", "ticket"]) => {
                require_capability(device, DeviceCapability::VoiceStream)
                    .and_then(|_| self.talk_ticket(&request.body, device, request_sha256, now_ms))
            }
            // The upgrade itself never reaches this match — `server.rs` answers
            // it before a body is collected. A *signed* GET that is not an
            // upgrade does reach here, and is told what it is missing rather
            // than 404ing on a route the contract publishes.
            ("GET", ["v1", "remote", "device", "talk", session_id, "stream"]) => {
                require_capability(device, DeviceCapability::VoiceStream)
                    .and_then(|_| self.talk_stream_needs_upgrade(session_id))
            }
            // Registering where to reach this device, and withdrawing it. Both
            // self-service for the same reason as the routes above: a push
            // address grants nothing — a woken device still has to make an
            // ordinary signed request — and a device must always be able to
            // stop being woken.
            ("GET", ["v1", "remote", "device", "push", "key"]) => {
                refuse_peer_only(device).and_then(|_| self.device_push_key())
            }
            ("POST", ["v1", "remote", "device", "push"]) => refuse_peer_only(device)
                .and_then(|_| self.device_push_register(&request.body, device_id, now_ms)),
            ("DELETE", ["v1", "remote", "device", "push"]) => {
                refuse_peer_only(device).and_then(|_| self.device_push_forget(device_id))
            }
            // --- Versioned `/v1/remote/node/*` placement plane (roadmap K17).
            // A second plane beside the control plane above, sharing only this
            // transport. The control-plane routes act on runs the node already
            // holds; these are the only ones through which a run authored
            // elsewhere can arrive.
            ("GET", ["v1", "remote", "node"]) => {
                require_capability(device, DeviceCapability::DescribeNode)
                    .and_then(|_| self.node_descriptor())
            }
            ("GET", ["v1", "remote", "node", "health"]) => {
                require_capability(device, DeviceCapability::DescribeNode)
                    .and_then(|_| self.node_health(now_ms))
            }
            ("POST", ["v1", "remote", "node", "runs"]) => {
                require_capability(device, DeviceCapability::PlaceRuns)
                    .and_then(|_| self.place_run(&request.body, device_id, request_sha256, now_ms))
            }
            ("GET", ["v1", "remote", "node", "runs", submitted_run_id]) => {
                require_capability(device, DeviceCapability::DescribeNode)
                    .and_then(|_| self.placed_run_status(device_id, submitted_run_id))
            }
            // Live migration (roadmap K18) sits on the placement plane rather
            // than beside it: a migration *is* a placement — a `RunSpec` this
            // node did not author — plus the frozen image that turns it into a
            // continuation. `GET /v1/remote/node` above is what an origin reads
            // to choose a target, so migration needs no describe route of its own.
            ("POST", ["v1", "remote", "node", "migration", "preflight"]) => {
                require_capability(device, DeviceCapability::Migrate)
                    .and_then(|_| self.migration_preflight(&request.body, now_ms))
            }
            ("POST", ["v1", "remote", "node", "migration", "accept"]) => {
                require_capability(device, DeviceCapability::Migrate)
                    .and_then(|_| self.migration_accept(&request.body, now_ms))
            }
            // --- Versioned `/v1/remote/peer/*` peer plane.
            // A third plane. The control plane acts on this node's runs, the
            // placement plane accepts a spec another owned node authored, and
            // this one accepts *words* from a peer that then run under this
            // node's own recipe. Peer standing implies nothing on the other
            // two: none of these arms consults `scopes`.
            ("POST", ["v1", "remote", "peer", "messages"]) => require_any_peer_capability(device)
                .and_then(|_| self.peer_message_post(device, &request.body, now_ms)),
            ("GET", ["v1", "remote", "peer", "threads", thread_id]) => {
                require_any_peer_capability(device)
                    .and_then(|_| self.peer_thread_get(device, thread_id, now_ms))
            }
            ("POST", ["v1", "remote", "peer", "hello"]) => require_any_peer_capability(device)
                .and_then(|_| self.peer_hello_post(device, &request.body, now_ms)),
            ("POST", ["v1", "remote", "peer", "artifacts"]) => {
                require_capability(device, DeviceCapability::PeerArtifact)
                    .and_then(|_| self.peer_artifact_post(device, &request.body, now_ms))
            }
            // Self-revocation needs no extra capability: a device may always
            // sever itself. The store path force-stops any live desktop
            // session the device owns, exactly like an operator revoke.
            ("DELETE", ["v1", "remote", "mobile", "devices", "self"]) => {
                self.mobile_revoke_self(device_id, now_ms)
            }
            _ => Err((404, "Unknown remote runner endpoint".to_string())),
        };
        let (response, outcome, target) = match result {
            Ok((status, value, target)) => (ApiResponse::json(status, &value), "allowed", target),
            Err((status, error)) => (
                ApiResponse::error(status, &error),
                if status == 403 {
                    "scope_denied"
                } else {
                    "rejected"
                },
                None,
            ),
        };
        if let Ok(mut store) = self.store.lock() {
            let _ = store.audit(
                now_ms,
                Some(device_id),
                &format!("{} {}", request.method, path),
                target.as_deref(),
                outcome,
                Some(request_sha256),
            );
        }
        response
    }

    fn list_runs(
        &self,
        scopes: &RemoteScopes,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let ledger = self.run_ledger()?;
        let runs = ledger.list_runs(1_000, false).map_err(internal)?;
        let shared = SharedLedger::open(&self.paths.ledger_db).map_err(internal)?;
        let summaries = runs
            .iter()
            .filter(|run| scopes.permits_run(run))
            .map(|run| summarize(run, &shared))
            .collect::<Result<Vec<_>, _>>()
            .map_err(internal)?;
        Ok((
            200,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "runs": summaries,
            }),
            None,
        ))
    }

    fn run_detail(
        &self,
        scopes: &RemoteScopes,
        run_id: &str,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let run = self.authorized_run(scopes, run_id)?;
        let shared = SharedLedger::open(&self.paths.ledger_db).map_err(internal)?;
        let summary = summarize(&run, &shared).map_err(internal)?;
        // Whether a pause is in effect, so a controller offers *resume* on a
        // paused run rather than pause again. It lives on the daemon's job
        // rather than in the run's status, and it is read here rather than in
        // `summarize` because the run list does not need it and would pay a
        // second database open per row for it. A machine whose daemon store
        // cannot be opened reports `false`: "not paused" is the state every
        // caller already handles, and refusing to describe a run because its
        // pause flag is unreadable would be a worse answer than a missing
        // button.
        let paused = DaemonStore::open(&self.paths)
            .ok()
            .and_then(|store| store.get_job(run_id).ok().flatten())
            .is_some_and(|job| job.pause_requested);
        // RunSpec contains only keychain references, never provider keys.
        Ok((
            200,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "run": summary,
                "paused": paused,
                "spec": run.spec,
            }),
            Some(run_id.to_string()),
        ))
    }

    fn run_events(
        &self,
        scopes: &RemoteScopes,
        run_id: &str,
        query: &str,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        self.authorized_run(scopes, run_id)?;
        let query = parse_query(query)?;
        let after = query
            .get("after")
            .map(|value| value.parse::<u64>())
            .transpose()
            .map_err(|_| (400, "Invalid event cursor".to_string()))?
            .unwrap_or(0);
        let limit = query
            .get("limit")
            .map(|value| value.parse::<usize>())
            .transpose()
            .map_err(|_| (400, "Invalid event limit".to_string()))?
            .unwrap_or(256)
            .min(1_000);
        let events = self
            .run_ledger()?
            .load_events(run_id, after, limit)
            .map_err(internal)?;
        let next_cursor = events.last().map(|event| event.sequence).unwrap_or(after);
        Ok((
            200,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "run_id": run_id,
                "after": after,
                "next_cursor": next_cursor,
                "events": events,
            }),
            Some(run_id.to_string()),
        ))
    }

    fn run_approvals(
        &self,
        scopes: &RemoteScopes,
        run_id: &str,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        self.authorized_run(scopes, run_id)?;
        let approvals = SharedLedger::open(&self.paths.ledger_db)
            .and_then(|shared| shared.pending_approvals(run_id))
            .map_err(internal)?;
        Ok((
            200,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "run_id": run_id,
                "approvals": approvals.iter().map(approval_json).collect::<Vec<_>>(),
            }),
            Some(run_id.to_string()),
        ))
    }

    fn artifact(
        &self,
        scopes: &RemoteScopes,
        run_id: &str,
        artifact_id: &str,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        self.authorized_run(scopes, run_id)?;
        super::protocol::validate_id(artifact_id).map_err(|error| (400, error))?;
        let connection = rusqlite::Connection::open(&self.paths.ledger_db).map_err(internal)?;
        let artifact = connection
            .query_row(
                "SELECT name,media_type,content_sha256,size_bytes FROM artifacts
                 WHERE artifact_id=?1 AND run_id=?2",
                rusqlite::params![artifact_id, run_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    (404, "Artifact is not linked to this run".to_string())
                }
                other => internal(other),
            })?;
        let size =
            u64::try_from(artifact.3).map_err(|_| internal("Stored artifact size is invalid"))?;
        if size > scopes.max_artifact_bytes {
            return Err((
                413,
                "Artifact exceeds this controller's paired byte budget".to_string(),
            ));
        }
        let app_data = self
            .paths
            .root
            .parent()
            .ok_or_else(|| internal("Daemon root has no app-data parent"))?;
        let store = ArtifactStore::with_max_blob_size(
            app_data.join("content-v1"),
            scopes.max_artifact_bytes,
        )
        .map_err(internal)?;
        let bytes = store.read(&artifact.2).map_err(internal)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != size {
            return Err(internal(
                "Artifact ledger size does not match verified blob",
            ));
        }
        Ok((
            200,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "artifact_id": artifact_id,
                "run_id": run_id,
                "name": artifact.0,
                "media_type": artifact.1,
                "content_sha256": artifact.2,
                "size_bytes": size,
                "content_base64": STANDARD.encode(bytes),
            }),
            Some(format!("{run_id}:{artifact_id}")),
        ))
    }

    fn approve(
        &self,
        scopes: &RemoteScopes,
        run_id: &str,
        body: &[u8],
        device_id: &str,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        self.authorized_run(scopes, run_id)?;
        let body: ApprovalRequestBody = serde_json::from_slice(body)
            .map_err(|error| (400, format!("Invalid approval request: {error}")))?;
        let decision = match body.decision.as_str() {
            "allow_once" => PermissionDecision::AllowOnce,
            "allow_for_run" => PermissionDecision::AllowForRun,
            "deny" => PermissionDecision::Deny,
            _ => return Err((400, "Unsupported approval decision".to_string())),
        };
        let ledger = self.run_ledger()?;
        let approval = ledger
            .load_approval(run_id, &body.request_id)
            .map_err(internal)?
            .ok_or_else(|| (404, "Unknown approval request".to_string()))?;
        if approval.operation_sha256 != body.operation_sha256 {
            return Err((403, "Approval operation digest does not match".to_string()));
        }
        if let Some(existing) = approval.decision {
            if existing == decision {
                return Ok((
                    200,
                    serde_json::json!({"status":"already_decided","decision":body.decision}),
                    Some(run_id.to_string()),
                ));
            }
            return Err((409, "Approval was already decided differently".to_string()));
        }
        if now_ms >= approval.expires_at_ms {
            return Err((409, "Approval request has expired".to_string()));
        }
        let shared = SharedLedger::open(&self.paths.ledger_db).map_err(internal)?;
        let recorder = control_recorder(&shared, run_id, device_id).map_err(internal)?;
        recorder
            .emit(RunEvent::PermissionDecided {
                request_id: body.request_id,
                operation_sha256: body.operation_sha256,
                decision,
                decided_by: recorder.client_identity(),
            })
            .map_err(internal)?;
        Ok((
            200,
            serde_json::json!({"status":"decided","decision":body.decision}),
            Some(run_id.to_string()),
        ))
    }

    fn cancel(
        &self,
        scopes: &RemoteScopes,
        run_id: &str,
        body: &[u8],
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let run = self.authorized_run(scopes, run_id)?;
        let body: CancelRequestBody = serde_json::from_slice(body)
            .map_err(|error| (400, format!("Invalid cancellation request: {error}")))?;
        if run.status.is_terminal() {
            return Ok((
                200,
                serde_json::json!({"status":"already_terminal"}),
                Some(run_id.to_string()),
            ));
        }
        let mut store = DaemonStore::open(&self.paths).map_err(internal)?;
        store.request_cancel(run_id, now_ms).map_err(internal)?;
        super::super::append_cancellation(
            &self.paths,
            run_id,
            body.reason
                .as_deref()
                .unwrap_or("Cancelled by paired controller"),
        )
        .map_err(internal)?;
        Ok((
            202,
            serde_json::json!({"status":"cancellation_requested"}),
            Some(run_id.to_string()),
        ))
    }

    /// Suspend or resume a run without ending it.
    ///
    /// The gap this closes: the daemon has supported pause locally since it had
    /// a `pause_requested` bit, but the remote protocol had no action for it, so
    /// a paired controller's only way to stop a run consuming the machine was to
    /// cancel it — destroying the work to stop it temporarily.
    ///
    /// Writes only the daemon's own bit, exactly as the local path does. Intent
    /// flows one way — latch to daemon bits, never the reverse — because
    /// `daemon_jobs` and `agent_processes` live in different databases with no
    /// transaction spanning them, so two writers would be a race with no
    /// arbitration primitive.
    fn set_paused(
        &self,
        scopes: &RemoteScopes,
        run_id: &str,
        paused: bool,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let run = self.authorized_run(scopes, run_id)?;
        // A terminal run is reported rather than errored, matching `cancel`: the
        // controller asked for a state the run is already past, which is not a
        // failure on its part.
        if run.status.is_terminal() {
            return Ok((
                200,
                serde_json::json!({"status":"already_terminal"}),
                Some(run_id.to_string()),
            ));
        }
        let mut store = DaemonStore::open(&self.paths).map_err(internal)?;
        // `request_pause` refuses a terminal job itself, so a race between the
        // check above and this call still cannot resurrect finished work.
        store
            .request_pause(run_id, paused, now_ms)
            .map_err(|error| (409, error))?;
        Ok((
            202,
            serde_json::json!({
                "status": if paused { "pause_requested" } else { "resume_requested" }
            }),
            Some(run_id.to_string()),
        ))
    }

    fn kill(
        &self,
        device_id: &str,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let mut store = DaemonStore::open(&self.paths).map_err(internal)?;
        store.set_kill_switch(true).map_err(internal)?;
        let cancelled = store.request_cancel_all(now_ms).map_err(internal)?;
        // Engaging the kill switch must also force-stop any live desktop
        // control session right away, in-process, rather than waiting for the
        // serve loop's next enforcement tick.
        let desktop_sessions_stopped = self
            .desktop
            .as_ref()
            .map(|runtime| runtime.emergency_stop_all())
            .unwrap_or(0);
        Ok((
            202,
            serde_json::json!({
                "status":"kill_switch_engaged",
                "requested_by":device_id,
                "cancelled_runs":cancelled,
                "desktop_sessions_stopped":desktop_sessions_stopped,
            }),
            None,
        ))
    }

    fn desktop_control_start(
        &self,
        body: &[u8],
        device_id: &str,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let runtime = self.require_desktop()?;
        let request: DesktopControlStartRequest =
            serde_json::from_slice(body).map_err(|error| {
                (
                    400,
                    format!("Invalid desktop-control start request: {error}"),
                )
            })?;
        // The consent prompt runs inside `runtime.start` before any session is
        // created — the human-visible gate on the runner itself.
        let value = runtime.start_with_options(
            device_id,
            &self.device_label(device_id),
            request.allowlist,
            request.batch_mode,
            request.allowed_windows,
            request
                .lifetime_ms
                .unwrap_or(little_monkey_lib::desktop_control::MAX_SESSION_LIFETIME_MS),
            request.allow_screenshots.unwrap_or(true),
            request.allow_keyboard_input.unwrap_or(true),
            request.allow_clipboard_read.unwrap_or(false),
            request.approval_policy,
        )?;
        Ok((201, value, Some(device_id.to_string())))
    }

    fn desktop_control_action(
        &self,
        body: &[u8],
        device_id: &str,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let runtime = self.require_desktop()?;
        let request: DesktopControlActionRequest =
            serde_json::from_slice(body).map_err(|error| {
                (
                    400,
                    format!("Invalid desktop-control action request: {error}"),
                )
            })?;
        let target = request.session_id.clone();
        let value = runtime.action(device_id, &self.device_label(device_id), request)?;
        Ok((200, value, Some(target)))
    }

    fn desktop_control_stop(
        &self,
        body: &[u8],
        device_id: &str,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let runtime = self.require_desktop()?;
        let request: DesktopControlStopRequest = serde_json::from_slice(body).map_err(|error| {
            (
                400,
                format!("Invalid desktop-control stop request: {error}"),
            )
        })?;
        let target = request.session_id.clone();
        let value = runtime.stop(device_id, &request.session_id)?;
        Ok((200, value, Some(target)))
    }

    fn require_desktop(&self) -> Result<&Arc<DesktopControlRuntime>, (u16, String)> {
        self.desktop.as_ref().ok_or_else(|| {
            (
                503,
                "Desktop control is not available on this runner".to_string(),
            )
        })
    }

    /// The paired device's human label, used in the local consent dialog.
    /// Falls back to the opaque id if the record is unreadable.
    fn device_label(&self, device_id: &str) -> String {
        self.store
            .lock()
            .ok()
            .and_then(|store| store.device(device_id).ok().flatten())
            .map(|device| device.device_name)
            .unwrap_or_else(|| device_id.to_string())
    }

    fn authorized_run(
        &self,
        scopes: &RemoteScopes,
        run_id: &str,
    ) -> Result<StoredRun, (u16, String)> {
        super::protocol::validate_id(run_id).map_err(|error| (400, error))?;
        let run = self
            .run_ledger()?
            .load_run(run_id)
            .map_err(internal)?
            .ok_or_else(|| (404, "Unknown durable run".to_string()))?;
        if !scopes.permits_run(&run) {
            // Do not reveal whether an out-of-scope run exists.
            return Err((404, "Unknown durable run".to_string()));
        }
        Ok(run)
    }

    fn run_ledger(&self) -> Result<RunLedger, (u16, String)> {
        RunLedger::open(&self.paths.ledger_db).map_err(internal)
    }

    // --- `/v1/remote/node` and `/v1/remote/migration/*` (roadmap K18) -------

    /// The app data directory this node's desktop half also uses.
    ///
    /// A migration writes into the *desktop's* checkpoint directory and session
    /// file on purpose: the thing that finally resumes a frozen turn is the
    /// desktop's own K13 re-entry, and it reads those two places. Landing the
    /// image anywhere else would make the daemon the only reader of a state
    /// whose whole point is being resumed.
    fn app_data_dir(&self) -> Result<&std::path::Path, (u16, String)> {
        self.paths.ledger_db.parent().ok_or_else(|| {
            (
                500,
                "This node's ledger path has no app-data parent".to_string(),
            )
        })
    }

    /// Collapses K17's node descriptor into what [`admit`] asks about.
    ///
    /// Built from `describe_node` rather than from a second probe, so a
    /// migration is admitted against exactly the facts an origin read from
    /// `GET /v1/remote/node` when it chose this target.
    ///
    /// **Installed rather than loaded, deliberately.** K13's `ModelNotResident`
    /// asks what the *next round trip would reach*, which on the machine running
    /// the turn is what is loaded. A target node is idle by definition — it has
    /// loaded nothing — so asking the residency question here would refuse every
    /// migration to every idle node. What a target can honestly promise is that
    /// the model is present and will load; what it still refuses is a model it
    /// does not have at all.
    fn migration_target(
        &self,
        descriptor: &little_monkey_lib::node_placement::NodeDescriptor,
        run_present: bool,
    ) -> (Vec<String>, Vec<String>, bool) {
        let mut models = descriptor
            .resident_models
            .iter()
            .map(|model| model.model_id.clone())
            .collect::<Vec<_>>();
        models.sort();
        models.dedup();
        let mut runtimes = descriptor
            .resident_models
            .iter()
            .map(|model| model.runtime.clone())
            .collect::<Vec<_>>();
        runtimes.sort();
        runtimes.dedup();
        (models, runtimes, run_present)
    }

    /// Answers "would you take this?" from metadata alone, before any bytes move.
    ///
    /// An optimisation and never the authority: `migration_accept` runs the very
    /// same `admit` against the very same header. A target that trusted a
    /// preflight would be trusting the *sender's* copy of facts about itself.
    fn migration_preflight(
        &self,
        body: &[u8],
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let request: MigrationPreflightRequest = serde_json::from_slice(body)
            .map_err(|error| (400, format!("Invalid migration preflight: {error}")))?;
        if request.protocol_version != REMOTE_PROTOCOL_VERSION {
            return Err((400, "Unsupported remote protocol version".to_string()));
        }
        let (verdict, descriptor) = self.admit_migration(&request.header, now_ms)?;
        Ok((
            200,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "node": descriptor,
                "verdict": verdict,
            }),
            Some(request.header.run_id),
        ))
    }

    /// The one admission decision, run identically by both migration routes.
    ///
    /// Returns the descriptor alongside the verdict because a refusal is only
    /// actionable next to the facts it was made against — "this node does not
    /// have that model" is answerable, "refused" is not.
    fn admit_migration(
        &self,
        header: &little_monkey_lib::migration::MigrationHeader,
        now_ms: u64,
    ) -> Result<
        (
            MigrationVerdict,
            little_monkey_lib::node_placement::NodeDescriptor,
        ),
        (u16, String),
    > {
        let descriptor = self.describe_node(now_ms)?;
        let run_present = self
            .run_ledger()?
            .load_run(&header.run_id)
            .map_err(internal)?
            .is_some();
        let (models, runtimes, run_present) = self.migration_target(&descriptor, run_present);
        let verdict = admit(
            header,
            &TargetNode {
                node_id: &descriptor.runner_id,
                resident_models: &models,
                runtime_ids: &runtimes,
                // No live approvals: this node has granted the incoming process
                // none, which is exactly why an image frozen with an outstanding
                // one is refused rather than resumed past a permission nobody
                // here gave.
                live_approvals: &[],
                // K17's rule, applied to a move: the *origin* states the
                // residency it required and this node checks it against its own
                // rather than trusting it — because a rule only the sender
                // enforces is not enforced, and an alias can start pointing at a
                // different host.
                residency: &descriptor.residency,
                max_payload_bytes: MAX_MIGRATION_PAYLOAD_BYTES,
                run_present,
            },
        );
        Ok((verdict, descriptor))
    }

    /// Takes the image, or refuses it — and on success leaves this node in the
    /// exact state its desktop half's K13 re-entry reads.
    fn migration_accept(
        &self,
        body: &[u8],
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let request: MigrationAcceptRequest = serde_json::from_slice(body)
            .map_err(|error| (400, format!("Invalid migration image: {error}")))?;
        if request.protocol_version != REMOTE_PROTOCOL_VERSION {
            return Err((400, "Unsupported remote protocol version".to_string()));
        }
        let image = request.image;
        // Structural first, capability second: a malformed image is a bad
        // request on any node and must not be reported as a refusal this node
        // could be reconfigured out of.
        image.validate().map_err(|error| (400, error))?;
        // The same gate K17's placement route applies, and for its reason: a run
        // must not arrive while the operator has stopped this machine.
        if DaemonStore::open(&self.paths)
            .map_err(internal)?
            .kill_switch()
            .map_err(internal)?
        {
            return Err((409, "Global kill switch is engaged".to_string()));
        }
        let (verdict, descriptor) = self.admit_migration(&image.header, now_ms)?;
        let MigrationVerdict::Acceptable { .. } = &verdict else {
            // 409, not 400: the image is well-formed and this node simply
            // cannot satisfy it. The blockers say what would have to change.
            return Ok((
                409,
                serde_json::json!({
                    "protocol_version": REMOTE_PROTOCOL_VERSION,
                    "node": descriptor,
                    "verdict": verdict,
                }),
                Some(image.header.run_id.clone()),
            ));
        };
        let mut ledger = self.run_ledger()?;

        // The run row comes from the *origin's* frozen spec, unmodified, and it
        // goes in *first*. That is what makes the policy travel: the allowlist
        // this node enforces and the budgets it charges are the ones the origin
        // declared, and `egress.rs` resolves them by run id against this node's
        // own ledger from here on. First rather than after the landing because
        // the process row the landing creates references it — a foreign key,
        // which is the schema saying the same thing.
        //
        // A landing that then fails leaves an event-less `queued` row, which is
        // recoverable: `submit_run` is keyed by the spec's idempotency key and
        // returns the existing run rather than erroring, so the same image can
        // be sent again.
        ledger.submit_run(&image.spec).map_err(internal)?;
        let app_data_dir = self.app_data_dir()?.to_path_buf();
        let landed = land_migration(&app_data_dir, &self.paths, &image, now_ms)
            .map_err(|error| (500, error))?;
        let arrival = RunEvent::MigrationArrived {
            origin_node_id: image.header.origin_node_id.clone(),
            origin_last_sequence: image.origin_last_sequence,
            origin_last_event_hash: image.origin_last_event_hash.clone(),
            payload_sha256: image.header.payload_sha256.clone(),
        };
        let envelope = RunEventEnvelope {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            event_id: format!("evt-migration-{}", &image.header.payload_sha256[..24]),
            run_id: image.header.run_id.clone(),
            sequence: 1,
            occurred_at_ms: now_ms,
            actor_id: None,
            emitter: ClientIdentity {
                client_id: descriptor.runner_id.clone(),
                instance_id: descriptor.runner_id.clone(),
                kind: ClientKind::RemoteRunner,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            event: arrival,
        };
        ledger.append_event(&envelope).map_err(internal)?;
        let arrival_event_hash = ledger
            .migration_arrival(&image.header.run_id)
            .map_err(internal)?
            .map(|arrival| arrival.event_hash)
            .ok_or_else(|| {
                (
                    500,
                    "The arrival event did not chain on this node".to_string(),
                )
            })?;

        Ok((
            201,
            serde_json::to_value(MigrationReceipt {
                protocol_version: REMOTE_PROTOCOL_VERSION,
                node_id: descriptor.runner_id,
                run_id: image.header.run_id.clone(),
                process_id: landed.process_id,
                workspace_root: landed.workspace_root.to_string_lossy().to_string(),
                arrival_event_hash,
                caveats: little_monkey_lib::migration::caveats(),
            })
            .map_err(internal)?,
            Some(image.header.run_id),
        ))
    }

    // --- `/v1/remote/mobile/*` handlers -----------------------------------

    fn locked_store(&self) -> Result<std::sync::MutexGuard<'_, RemoteStore>, (u16, String)> {
        self.store
            .lock()
            .map_err(|_| (500, "Remote state lock was poisoned".to_string()))
    }

    fn mobile_sessions(&self) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let sessions = self
            .locked_store()?
            .mobile_session_summaries()
            .map_err(internal)?;
        Ok((
            200,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "sessions": sessions
                    .iter()
                    .map(|session| serde_json::json!({
                        "id": session.session_id,
                        "title": bounded_text(&session.title, 120),
                        "model_label": "Node mobile-chat recipe",
                        "updated_at_ms": session.updated_at_ms,
                        "unread_count": 0,
                    }))
                    .collect::<Vec<_>>(),
            }),
            None,
        ))
    }

    fn mobile_messages_get(
        &self,
        session_id: &str,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        self.materialize_mobile_replies(session_id, now_ms)?;
        let messages = self
            .locked_store()?
            .mobile_messages(session_id, 2_000)
            .map_err(internal)?;
        Ok((
            200,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "messages": messages
                    .iter()
                    .map(|message| serde_json::json!({
                        "id": message.message_id,
                        "role": message.role,
                        "text": message.text,
                        "created_at_ms": message.created_at_ms,
                        "task_state": message.task_state,
                    }))
                    .collect::<Vec<_>>(),
            }),
            Some(session_id.to_string()),
        ))
    }

    /// Turns terminal chat runs into visible replies. Called lazily from the
    /// message GET (the client already polls), so no daemon-loop hook is
    /// needed: for every still-`queued` user message, resolve its durable
    /// run; once that run is terminal, append the assistant text (or a
    /// system-role failure notice) and settle the user row's `task_state`.
    /// Both inserts are idempotent (`ON CONFLICT DO NOTHING` + the state
    /// filter), so concurrent polls cannot double-append.
    fn materialize_mobile_replies(
        &self,
        session_id: &str,
        now_ms: u64,
    ) -> Result<(), (u16, String)> {
        let Some(queue) = self.mobile_chat.as_ref() else {
            return Ok(());
        };
        let pending: Vec<MobileMessageRecord> = {
            let store = self.locked_store()?;
            store
                .mobile_messages(session_id, 2_000)
                .map_err(internal)?
                .into_iter()
                .filter(|message| message.role == "user" && message.task_state == "queued")
                .collect()
        };
        if pending.is_empty() {
            return Ok(());
        }
        let ledger = self.run_ledger()?;
        for message in pending {
            let Some(run_id) = queue
                .chat_run_id(session_id, &message.message_id)
                .map_err(internal)?
            else {
                continue;
            };
            let events = match ledger.load_events(&run_id, 0, 1_000) {
                Ok(events) => events,
                Err(_) => continue, // Run not recorded yet — try again next poll.
            };
            let mut assistant_text = String::new();
            let mut completed_summary: Option<String> = None;
            let mut failed: Option<String> = None;
            let mut cancelled = false;
            for envelope in &events {
                match &envelope.event {
                    RunEvent::ModelDelta { channel, text, .. } => {
                        if matches!(channel, OutputChannel::Assistant) {
                            assistant_text.push_str(text);
                        }
                    }
                    RunEvent::Completed { summary, .. } => {
                        completed_summary = summary.clone();
                    }
                    RunEvent::Failed { message, .. } => failed = Some(message.clone()),
                    RunEvent::Cancelled { .. } => cancelled = true,
                    _ => {}
                }
            }
            let terminal = completed_summary.is_some()
                || failed.is_some()
                || cancelled
                || events
                    .iter()
                    .any(|envelope| matches!(envelope.event, RunEvent::Completed { .. }));
            if !terminal {
                continue;
            }
            let (role, text, final_state) = if let Some(reason) = failed {
                (
                    "system",
                    format!(
                        "The node could not answer this message: {}",
                        bounded_text(&reason, 2_048)
                    ),
                    "failed",
                )
            } else if cancelled && assistant_text.trim().is_empty() {
                (
                    "system",
                    "This message's run was cancelled on the node.".to_string(),
                    "failed",
                )
            } else {
                let text = if assistant_text.trim().is_empty() {
                    completed_summary
                        .unwrap_or_else(|| "(The run completed without any output.)".to_string())
                } else {
                    assistant_text
                };
                ("assistant", text, "accepted")
            };
            let mut store = self.locked_store()?;
            store
                .insert_mobile_message(&MobileMessageRecord {
                    message_id: format!("{}-reply", message.message_id),
                    session_id: message.session_id.clone(),
                    device_id: message.device_id.clone(),
                    role: role.to_string(),
                    text,
                    request_sha256: message.request_sha256.clone(),
                    task_state: final_state.to_string(),
                    created_at_ms: now_ms,
                })
                .map_err(internal)?;
            store
                .set_mobile_message_state(&message.message_id, final_state, now_ms)
                .map_err(internal)?;
        }
        Ok(())
    }

    fn mobile_message_post(
        &self,
        session_id: &str,
        body: &[u8],
        device_id: &str,
        request_sha256: &str,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let Some(queue) = self.mobile_chat.as_ref() else {
            return Err((
                501,
                "This node build does not expose mobile chat execution".to_string(),
            ));
        };
        let parsed: serde_json::Value = serde_json::from_slice(body)
            .map_err(|error| (400, format!("Invalid mobile message body: {error}")))?;
        let text = parsed
            .get("text")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or((400, "Mobile message requires non-empty 'text'".to_string()))?;
        if session_id.is_empty()
            || session_id.len() > 128
            || !session_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err((
                400,
                "Mobile session id must be 1-128 URL-safe characters".to_string(),
            ));
        }
        // The message id doubles as the idempotent queue key: derived from
        // the signed request digest, so an at-least-once retry of the SAME
        // signed request maps onto the same message and job.
        let message_id = format!("mm-{}", &request_sha256[..32]);
        {
            let mut store = self.locked_store()?;
            store
                .insert_mobile_message(&MobileMessageRecord {
                    message_id: message_id.clone(),
                    session_id: session_id.to_string(),
                    device_id: device_id.to_string(),
                    role: "user".to_string(),
                    text: text.to_string(),
                    request_sha256: request_sha256.to_string(),
                    task_state: "queued".to_string(),
                    created_at_ms: now_ms,
                })
                .map_err(internal)?;
        }
        match queue.queue_chat(session_id, &message_id, text) {
            Ok(_run_id) => {}
            Err(error) => {
                let mut store = self.locked_store()?;
                let _ = store.set_mobile_message_state(&message_id, "failed", now_ms);
                return Err((503, format!("Mobile chat could not be queued: {error}")));
            }
        }
        Ok((
            201,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "message": { "id": message_id, "created_at_ms": now_ms },
            }),
            Some(session_id.to_string()),
        ))
    }

    fn mobile_workflows(&self) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let app_data = self
            .paths
            .root
            .parent()
            .ok_or_else(|| internal("Daemon root has no app-data parent"))?
            .to_path_buf();
        let service = little_monkey_lib::m4_runtime::production_workflow_service(&app_data)
            .map_err(internal)?;
        let definitions = service.list().map_err(internal)?;
        let last_runs = self
            .locked_store()?
            .mobile_workflow_last_runs()
            .map_err(internal)?;
        Ok((
            200,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "workflows": definitions
                    .iter()
                    .map(|definition| {
                        let mut entry = serde_json::json!({
                            "id": definition.workflow_id,
                            "name": definition.name,
                            "summary": format!("v{} · {} nodes", definition.workflow_version, definition.nodes.len()),
                        });
                        if let Some(last) = last_runs.get(&definition.workflow_id) {
                            entry["last_run_at_ms"] = serde_json::json!(last);
                        }
                        entry
                    })
                    .collect::<Vec<_>>(),
            }),
            None,
        ))
    }

    fn mobile_workflow_launch(
        &self,
        workflow_id: &str,
        device_id: &str,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let store = DaemonStore::open(&self.paths).map_err(internal)?;
        if store.kill_switch().map_err(internal)? {
            return Err((409, "Global kill switch is engaged".to_string()));
        }
        drop(store);
        let app_data = self
            .paths
            .root
            .parent()
            .ok_or_else(|| internal("Daemon root has no app-data parent"))?
            .to_path_buf();
        let service = little_monkey_lib::m4_runtime::production_workflow_service(&app_data)
            .map_err(internal)?;
        let definition = service
            .load(workflow_id)
            .map_err(|error| (404, format!("Workflow is not available: {error}")))?;
        let ir = service
            .validate(&definition)
            .map_err(|error| (409, format!("Workflow no longer validates: {error}")))?;
        // A replay of the same SIGNED request never reaches this code — the
        // command reservation in `handle` returns the cached response — so
        // this id only needs to be unique per accepted launch.
        let run_id = format!(
            "m4-mobile-{}",
            &sha256_hex(format!("{device_id}:{workflow_id}:{now_ms}").as_bytes())[..32]
        );
        let history = little_monkey_lib::m4_runtime::run_daemon_workflow_delivery(
            &app_data,
            workflow_id,
            &ir.definition_sha256,
            &run_id,
            little_monkey_lib::workflow_core::WorkflowTrigger::Manual,
            serde_json::json!({}),
        )
        .map_err(|error| (409, format!("Workflow launch failed: {error}")))?;
        self.locked_store()?
            .insert_mobile_workflow_run(&MobileWorkflowRunRecord {
                run_id: history.run_id.clone(),
                workflow_id: workflow_id.to_string(),
                device_id: device_id.to_string(),
                created_at_ms: now_ms,
            })
            .map_err(internal)?;
        Ok((
            201,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "run": {
                    "run_id": history.run_id,
                    "status": format!("{:?}", history.status).to_ascii_lowercase(),
                    "kind": "workflow",
                    "created_at_ms": now_ms,
                    "updated_at_ms": now_ms,
                    "pending_approval_count": 0,
                },
            }),
            Some(workflow_id.to_string()),
        ))
    }

    fn mobile_capture_post(
        &self,
        body: &[u8],
        device: &DeviceRecord,
        request_sha256: &str,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let parsed: serde_json::Value = serde_json::from_slice(body)
            .map_err(|error| (400, format!("Invalid mobile capture body: {error}")))?;
        let field = |name: &str| parsed.get(name).and_then(|value| value.as_str());
        let capture_id = field("capture_id")
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= 128
                    && value
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            })
            .ok_or((400, "Capture requires a URL-safe 'capture_id'".to_string()))?;
        let kind = field("kind")
            .filter(|value| matches!(*value, "text" | "image" | "file" | "voice"))
            .ok_or((
                400,
                "Capture 'kind' must be text, image, file, or voice".to_string(),
            ))?;
        let title = field("title")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or((400, "Capture requires a non-empty 'title'".to_string()))?;
        let text = field("text").map(str::to_string);
        let declared_sha = field("content_sha256").map(str::to_string);
        let media_type = field("mime_type").map(str::to_string);
        let declared_size = parsed.get("size_bytes").and_then(|value| value.as_u64());

        let mut stored_size: Option<u64> = None;
        if let Some(content) = field("content_base64") {
            let bytes = STANDARD
                .decode(content)
                .map_err(|_| (400, "Capture content is not valid base64".to_string()))?;
            if bytes.len() as u64 > device.scopes.max_artifact_bytes {
                return Err((
                    413,
                    "Capture exceeds this device grant's artifact budget".to_string(),
                ));
            }
            let digest = sha256_hex(&bytes);
            match &declared_sha {
                Some(declared) if declared.eq_ignore_ascii_case(&digest) => {}
                _ => {
                    return Err((
                        400,
                        "Capture content_sha256 does not match the uploaded bytes".to_string(),
                    ))
                }
            }
            if let Some(size) = declared_size {
                if size != bytes.len() as u64 {
                    return Err((
                        400,
                        "Capture size_bytes does not match the uploaded bytes".to_string(),
                    ));
                }
            }
            let captures_dir = self.paths.root.join("mobile-captures");
            std::fs::create_dir_all(&captures_dir).map_err(|error| {
                internal(format!("Could not create capture directory: {error}"))
            })?;
            std::fs::write(captures_dir.join(capture_id), &bytes)
                .map_err(|error| internal(format!("Could not persist capture payload: {error}")))?;
            stored_size = Some(bytes.len() as u64);
        } else if text.is_none() {
            return Err((
                400,
                "Capture needs either 'text' or 'content_base64'".to_string(),
            ));
        }

        self.locked_store()?
            .insert_mobile_capture(&MobileCaptureRecord {
                capture_id: capture_id.to_string(),
                device_id: device.device_id.clone(),
                kind: kind.to_string(),
                title: title.to_string(),
                text,
                content_sha256: declared_sha,
                size_bytes: stored_size.or(declared_size),
                media_type,
                request_sha256: request_sha256.to_string(),
                created_at_ms: now_ms,
            })
            .map_err(internal)?;
        Ok((
            201,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "capture_id": capture_id,
            }),
            Some(capture_id.to_string()),
        ))
    }

    // --- `/v1/remote/device/*` handlers ------------------------------------

    /// The device reporting what it is and what its OS currently permits.
    fn device_surface_post(
        &self,
        body: &[u8],
        device: &DeviceRecord,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let mut surface: DeviceSurface = serde_json::from_slice(body)
            .map_err(|error| (400, format!("Invalid device surface: {error}")))?;
        // The runner timestamps the report itself. A device clock that is
        // wrong (or flattering) must not decide how fresh the operator's view
        // of it looks.
        surface.reported_at_ms = now_ms;
        surface.validate().map_err(|error| (400, error))?;
        self.locked_store()?
            .save_device_surface(&device.device_id, &surface, now_ms)
            .map_err(internal)?;
        Ok((
            200,
            device_state_json(device, Some(&surface)),
            Some(device.device_id.clone()),
        ))
    }

    // --- Realtime Talk -----------------------------------------------------

    /// Issues the one-use bearer that admits a Talk WebSocket.
    ///
    /// **Why a ticket exists at all.** Every other route on this plane is a
    /// signed request: HMAC over method, path, body, sequence, nonce and key
    /// generation. A browser cannot put any of that on a WebSocket handshake —
    /// the API takes no headers — so the choice is a socket authenticated by
    /// something weaker, or a signed request that *mints* the admission. This
    /// is the second: the ticket is issued only to a request that already
    /// passed the full signature, replay and revocation checks, it is random,
    /// it is single-use, it dies in thirty seconds, and it is spent
    /// immediately. The identity it carries is the identity of the signed
    /// request that made it, frozen — the socket cannot claim any other device.
    ///
    /// The ticket is never put in the response's `websocket_path`; the client
    /// appends it as a query parameter at the moment it opens the socket, so a
    /// path that ends up in a log or a history entry carries no bearer.
    fn talk_ticket(
        &self,
        body: &[u8],
        device: &DeviceRecord,
        request_sha256: &str,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let request: TalkTicketRequest = serde_json::from_slice(body)
            .map_err(|error| (400, format!("Invalid Talk ticket request: {error}")))?;
        request.validate().map_err(|error| (400, error))?;
        // The surface matters as much as the grant: a device whose OS refused
        // the microphone must not be handed a socket that can only fail.
        let surface = self
            .locked_store()?
            .device_surface(&device.device_id)
            .map_err(internal)?;
        if !effective_capabilities(&device.capabilities, surface.as_ref())
            .contains(&DeviceCapability::VoiceStream)
        {
            return Err((
                403,
                "This device's microphone is not effective: the grant, the device's own \
                 advertisement and its operating system permission must all allow it."
                    .to_string(),
            ));
        }
        let issued = TalkTicketResponse::issue(
            request.session_id.clone(),
            now_ms,
            DEFAULT_TALK_TICKET_TTL_MS,
        )
        .map_err(|error| (400, error))?;
        let mut tickets = self
            .talk_tickets
            .lock()
            .map_err(|_| (500, "Talk ticket state was poisoned".to_string()))?;
        // Expired admissions are swept on every issue rather than on a timer:
        // this is the only path that adds to the map, so it is the only place
        // it can grow.
        tickets.retain(|_, pending| pending.expires_at_ms > now_ms);
        if tickets.len() >= MAX_PENDING_TALK_TICKETS {
            return Err((
                429,
                "Too many Talk sockets are being opened at once.".to_string(),
            ));
        }
        tickets.insert(
            sha256_hex(issued.ticket.as_bytes()),
            PendingTalkTicket {
                device_id: device.device_id.clone(),
                secret_generation: device.secret_generation,
                signed_request_sha256: request_sha256.to_string(),
                session_id: issued.session_id.clone(),
                session_generation: issued.session_generation.clone(),
                expires_at_ms: issued.expires_at_ms,
            },
        );
        drop(tickets);
        Ok((
            201,
            serde_json::to_value(&issued).map_err(internal)?,
            Some(device.device_id.clone()),
        ))
    }

    /// Spends a ticket, returning the identity the socket then holds.
    ///
    /// `None` for anything at all wrong — unknown, expired, already spent,
    /// wrong session, a device revoked or re-keyed in the meantime — with no
    /// distinction between them, because a caller guessing tickets learns
    /// nothing from which of those it hit. Removal happens under the same lock
    /// as the lookup, which is what makes "one use" true against two sockets
    /// racing with the same ticket.
    pub(crate) fn consume_talk_ticket(
        &self,
        session_id: &str,
        ticket: &str,
        now_ms: u64,
    ) -> Option<TalkSocketAuthorization> {
        let pending = {
            let mut tickets = self.talk_tickets.lock().ok()?;
            let digest = sha256_hex(ticket.as_bytes());
            let pending = tickets.get(&digest)?.clone();
            if pending.expires_at_ms <= now_ms || pending.session_id != session_id {
                // Removed either way: an expired or misdirected ticket has no
                // second chance.
                tickets.remove(&digest);
                return None;
            }
            tickets.remove(&digest);
            pending
        };
        // Re-checked at the moment of admission, not only at issue: thirty
        // seconds is long enough for an operator to revoke a device, and the
        // socket that follows can stay open for an hour.
        let device = self
            .store
            .lock()
            .ok()?
            .device(&pending.device_id)
            .ok()
            .flatten()?;
        if !device.active() || device.secret_generation != pending.secret_generation {
            return None;
        }
        require_capability(&device, DeviceCapability::VoiceStream).ok()?;
        Some(TalkSocketAuthorization {
            device_id: pending.device_id,
            signed_request_sha256: pending.signed_request_sha256,
            session_id: pending.session_id,
            session_generation: pending.session_generation,
        })
    }

    /// Where the desktop half keeps its configuration, which is where the
    /// operator's own speech backends are read from.
    pub(crate) fn app_data_dir_for_talk(&self) -> std::path::PathBuf {
        self.paths
            .ledger_db
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| self.paths.root.clone())
    }

    /// What a finished Talk session leaves behind: bounded counters, on the
    /// same audit stream every other remote action is written to.
    ///
    /// Deliberately not the transcript, not the assistant's answer and not one
    /// byte of audio. A support bundle collects this stream, and a recording of
    /// somebody's room is not a thing to put in one.
    pub(crate) fn record_talk_session(
        &self,
        device_id: &str,
        report: &super::talk::TalkSessionReport,
    ) {
        self.audit
            .record(little_monkey_lib::subsystem_audit::SubsystemAction {
                subsystem: little_monkey_lib::run_ledger::Subsystem::Remote,
                action: "TALK /v1/remote/device/talk/stream".to_string(),
                turn_id: None,
                permission_request_id: None,
                outcome: if report.stream_dropped || report.grant_revoked {
                    little_monkey_lib::subsystem_audit::outcome_for_status(499)
                } else {
                    little_monkey_lib::subsystem_audit::outcome_for_status(200)
                },
                detail: Some(serde_json::json!({
                    "deviceId": device_id,
                    "utterances": report.utterances,
                    "turns": report.turns_submitted,
                    "interruptions": report.interruptions,
                    "spokenChunks": report.spoken_chunks,
                    "errors": report.errors,
                    "fallbacks": report.fallbacks,
                    "grantRevoked": report.grant_revoked,
                    // Durations, in the same seven spans the desktop records.
                    // Means and worst cases rather than samples, so a long
                    // conversation cannot grow this row.
                    "latencyMs": talk_latency_detail(&report.latency),
                })),
            });
    }

    /// Registers an open Talk socket as a live capture, and hands back the row
    /// to close when it ends. A failure to register is not a reason to refuse
    /// the conversation — but it is recorded, because an unobservable microphone
    /// is the thing this exists to prevent.
    pub(crate) fn open_talk_capture(
        &self,
        device_id: &str,
        session_id: &str,
        expires_at_ms: u64,
    ) -> Option<String> {
        let now_ms = super::now_ms_public().ok()?;
        let mut store = self.store.lock().ok()?;
        match store.open_talk_capture(device_id, session_id, expires_at_ms, now_ms) {
            Ok(record) => Some(record.command_id),
            Err(error) => {
                self.audit
                    .record(little_monkey_lib::subsystem_audit::SubsystemAction {
                        subsystem: little_monkey_lib::run_ledger::Subsystem::Remote,
                        action: "TALK /v1/remote/device/talk/stream".to_string(),
                        turn_id: None,
                        permission_request_id: None,
                        outcome: little_monkey_lib::subsystem_audit::outcome_for_status(500),
                        detail: Some(serde_json::json!({
                            "deviceId": device_id,
                            "captureRegistrationFailed": error,
                        })),
                    });
                None
            }
        }
    }

    pub(crate) fn close_talk_capture(
        &self,
        device_id: &str,
        command_id: &str,
        error: Option<&str>,
    ) {
        let Ok(now_ms) = super::now_ms_public() else {
            return;
        };
        if let Ok(mut store) = self.store.lock() {
            let _ = store.close_talk_capture(device_id, command_id, error, now_ms);
        }
    }

    /// Whether a device may still speak. Read between Talk turns, and on a timer
    /// while an answer streams, so a grant withdrawn mid-conversation closes the
    /// microphone.
    ///
    /// Deliberately the *same* test the ticket route admits on — grant ∩
    /// advertised surface ∩ OS permission — rather than the grant alone. A
    /// microphone permission withdrawn on the phone half way through a
    /// conversation is exactly the case where the weaker test would keep the
    /// session alive, and it is the case that matters most.
    pub(crate) fn talk_capability_live(&self, device_id: &str) -> bool {
        let Ok(store) = self.store.lock() else {
            return false;
        };
        let Some(device) = store.device(device_id).ok().flatten() else {
            return false;
        };
        if !device.active() {
            return false;
        }
        let surface = store.device_surface(device_id).ok().flatten();
        effective_capabilities(&device.capabilities, surface.as_ref())
            .contains(&DeviceCapability::VoiceStream)
    }

    fn talk_stream_needs_upgrade(
        &self,
        session_id: &str,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        Err((
            426,
            format!(
                "Talk session '{session_id}' is a WebSocket endpoint. Request a ticket at \
                 POST /v1/remote/device/talk/ticket and upgrade with it."
            ),
        ))
    }

    /// What this device may actually do, as the runner sees it — the same three
    /// sets the operator's device card shows, so the phone and the desktop can
    /// never disagree about why something is unavailable.
    fn device_state(
        &self,
        device: &DeviceRecord,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let surface = self
            .locked_store()?
            .device_surface(&device.device_id)
            .map_err(internal)?;
        Ok((
            200,
            device_state_json(device, surface.as_ref()),
            Some(device.device_id.clone()),
        ))
    }

    /// Leases the next command, re-checking authority at the moment of handing
    /// it over.
    ///
    /// A grant revoked, or an OS permission withdrawn, between queueing and
    /// leasing must stop the command — so the check is here and not only at
    /// enqueue. Such a command fails with an explicit reason rather than
    /// silently vanishing, because a run is waiting on an answer.
    fn device_command_lease(
        &self,
        device: &DeviceRecord,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let mut store = self.locked_store()?;
        // A stream whose deadline passed is closed here, on the same sweep that
        // expires stale commands: the device that abandoned it is by definition
        // not the one asking for work, so this is where someone else notices.
        super::voice::expire(&mut store, now_ms).map_err(internal)?;
        let surface = store.device_surface(&device.device_id).map_err(internal)?;
        let granted = granted_capabilities(device);
        // Bounded: each iteration retires exactly one now-unauthorized command,
        // so this cannot spin.
        for _ in 0..64 {
            let Some(record) = store
                .lease_device_command(&device.device_id, DEVICE_LEASE_MS, now_ms)
                .map_err(internal)?
            else {
                return Ok((204, serde_json::json!({}), None));
            };
            if let Some(block) = capability_block(&granted, surface.as_ref(), record.capability) {
                // Failed with the reason, not with a shrug: a run is waiting on
                // this answer and the operator needs to know which of the four
                // axes said no.
                store
                    .complete_device_command(
                        &device.device_id,
                        &record.command_id,
                        DeviceCommandState::Failed,
                        None,
                        None,
                        Some(&block.explain(record.capability)),
                        None,
                        now_ms,
                    )
                    .map_err(internal)?;
                store
                    .audit(
                        now_ms,
                        Some(&device.device_id),
                        "device_command_blocked",
                        Some(&record.command_id),
                        block.as_str(),
                        None,
                    )
                    .map_err(internal)?;
                continue;
            }
            let command = DeviceCommand {
                protocol_version: REMOTE_PROTOCOL_VERSION,
                command_id: record.command_id.clone(),
                capability: record.capability,
                arguments: record.arguments.clone(),
                arguments_sha256: record.arguments_sha256.clone(),
                expires_at_ms: record.expires_at_ms,
                lease_expires_at_ms: record.lease_expires_at_ms.unwrap_or(record.expires_at_ms),
                cancel_requested: record.cancel_requested,
            };
            let body = serde_json::to_value(&command)
                .map_err(|error| internal(format!("Could not encode device command: {error}")))?;
            return Ok((200, body, Some(record.command_id)));
        }
        Ok((204, serde_json::json!({}), None))
    }

    /// The device declaring it is about to touch hardware. `started: false`
    /// means this command was already running — the device must not repeat the
    /// action, and this is the reply a reconnect gets.
    ///
    /// Authority is re-checked here and not only at lease time. A lease and the
    /// moment hardware is touched are different moments, and a grant withdrawn
    /// or a permission revoked in between has to stop the action — the whole
    /// point of the split is that nothing physical has happened yet.
    ///
    /// That re-check belongs to the `leased` → `running` transition and to
    /// nothing else. The same route also answers a *recovery*: an execution that
    /// already holds this command and lost the reply. Re-running readiness there
    /// would fail a command whose effect may already have happened because the
    /// page went to the background afterwards — turning a momentary loss of
    /// readiness into a revocation of work already authorized. What ends a
    /// running command is cancellation or revocation, both on the control
    /// channel; never a readiness check at a boundary it already passed.
    fn device_command_start(
        &self,
        body: &[u8],
        device: &DeviceRecord,
        command_id: &str,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let request: DeviceCommandStartRequest = if body.is_empty() {
            DeviceCommandStartRequest::default()
        } else {
            serde_json::from_slice(body)
                .map_err(|error| (400, format!("Invalid device command start: {error}")))?
        };
        request.validate().map_err(|error| (400, error))?;
        let mut store = self.locked_store()?;
        let record = store
            .device_command(command_id)
            .map_err(internal)?
            .filter(|record| record.device_id == device.device_id)
            .ok_or((404, "Unknown device command".to_string()))?;
        // A command already past its own deadline never begins, however long
        // the device took to ask.
        if record.expires_at_ms <= now_ms && !record.state.terminal() {
            store.expire_device_commands(now_ms).map_err(internal)?;
            return Err((409, "This command expired before it started".to_string()));
        }
        // Only the one transition that authorizes a *new* physical effect. A
        // `running` command falls through to `start_device_command`, which
        // answers a matching execution with `started: false, recoverable: true`
        // and a different one with a refusal.
        if matches!(record.state, DeviceCommandState::Leased) {
            let surface = store.device_surface(&device.device_id).map_err(internal)?;
            let granted = granted_capabilities(device);
            if let Some(block) = capability_block(&granted, surface.as_ref(), record.capability) {
                store
                    .complete_device_command(
                        &device.device_id,
                        command_id,
                        DeviceCommandState::Failed,
                        None,
                        None,
                        Some(&block.explain(record.capability)),
                        request.execution_id.as_deref(),
                        now_ms,
                    )
                    .map_err(internal)?;
                store
                    .audit(
                        now_ms,
                        Some(&device.device_id),
                        "device_command_blocked",
                        Some(command_id),
                        block.as_str(),
                        None,
                    )
                    .map_err(internal)?;
                return Err((403, block.explain(record.capability)));
            }
        }
        let outcome = store
            .start_device_command(
                &device.device_id,
                command_id,
                request.execution_id.as_deref(),
                now_ms,
            )
            .map_err(|error| (409, error))?;
        Ok((
            200,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "command_id": command_id,
                "started": outcome.started,
                // True when this is the same execution reconnecting: it may
                // deliver a result it already staged, and must not re-execute.
                "recoverable": outcome.recoverable,
                "execution_id": outcome.execution_id,
            }),
            Some(command_id.to_string()),
        ))
    }

    /// Every nonterminal command the runner still believes this device owns.
    ///
    /// Deliberately not a lease: handing a `running` command back through the
    /// queue is precisely the second execution this design refuses. The device
    /// answers each of these from its own journal — deliver the staged result,
    /// or report the outcome unknown.
    fn device_commands_recover(
        &self,
        device: &DeviceRecord,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let mut store = self.locked_store()?;
        store.expire_device_commands(now_ms).map_err(internal)?;
        let commands = store
            .recoverable_device_commands(&device.device_id)
            .map_err(internal)?
            .into_iter()
            .map(|record| DeviceCommandRecovery {
                command_id: record.command_id,
                capability: record.capability,
                arguments_sha256: record.arguments_sha256,
                state: record.state,
                execution_id: record.execution_id,
                started_at_ms: record.started_at_ms,
                expires_at_ms: record.expires_at_ms,
                cancel_requested: record.cancel_requested,
            })
            .collect::<Vec<_>>();
        Ok((
            200,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "commands": commands,
            }),
            None,
        ))
    }

    /// A running command's control signals.
    ///
    /// One request the device makes while it is working, held open by the
    /// long-poll until something changes, rather than a poll it repeats. That is
    /// what lets a cancellation reach a recording already in progress without
    /// spending a signed request every second.
    fn device_command_control(
        &self,
        device: &DeviceRecord,
        command_id: &str,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let store = self.locked_store()?;
        let record = store
            .device_command(command_id)
            .map_err(internal)?
            .filter(|record| record.device_id == device.device_id)
            .ok_or((404, "Unknown device command".to_string()))?;
        // Authority, deliberately not readiness. A page that goes to the
        // background loses readiness for a moment; telling a recording already
        // in progress that it was revoked would cut it short over a glance at
        // another app. What ends a running command here is the operator taking
        // the grant away, or the pairing itself going.
        let revoked =
            !device.active() || !granted_capabilities(device).contains(&record.capability);
        let control = DeviceCommandControl {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            command_id: record.command_id.clone(),
            state: record.state,
            cancel_requested: record.cancel_requested,
            revoked,
            deadline_ms: record.expires_at_ms,
        };
        let _ = now_ms;
        Ok((
            200,
            serde_json::to_value(&control).map_err(internal)?,
            Some(record.command_id),
        ))
    }

    /// The device's terminal report, with any artifact it produced.
    fn device_command_result(
        &self,
        body: &[u8],
        device: &DeviceRecord,
        command_id: &str,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let result: DeviceCommandResult = serde_json::from_slice(body)
            .map_err(|error| (400, format!("Invalid device command result: {error}")))?;
        // The device's own declared bound never widens the operator's: the
        // artifact budget on the pairing is the ceiling either way.
        result
            .validate(device.scopes.max_artifact_bytes)
            .map_err(|error| (400, error))?;
        // Decoded and digested before anything is written, because the digest is
        // what decides whether this delivery is a retry of the stored result or
        // a contradiction of it — and a contradiction must not reach the
        // artifact file at all.
        let decoded = match (&result.artifact_base64, &result.artifact_media_type) {
            (Some(encoded), Some(media_type)) => {
                let bytes = STANDARD
                    .decode(encoded)
                    .map_err(|_| (400, "Device artifact is not valid base64".to_string()))?;
                if bytes.len() as u64 > device.scopes.max_artifact_bytes {
                    return Err((
                        413,
                        "Device artifact exceeds this pairing's artifact budget".to_string(),
                    ));
                }
                let sha256 = sha256_hex(&bytes);
                if let Some(declared) = &result.artifact_sha256 {
                    if declared != &sha256 {
                        return Err((
                            400,
                            "The artifact's bytes do not match the digest the device declared"
                                .to_string(),
                        ));
                    }
                }
                Some((
                    bytes,
                    DeviceArtifact {
                        sha256,
                        bytes: 0,
                        media_type: media_type.clone(),
                    },
                ))
            }
            _ => None,
        };
        let artifact = decoded.as_ref().map(|(bytes, artifact)| DeviceArtifact {
            bytes: bytes.len() as u64,
            ..artifact.clone()
        });
        let digest = terminal_digest(
            result.outcome,
            result.result.as_ref(),
            artifact.as_ref().map(|artifact| artifact.sha256.as_str()),
            result
                .error
                .as_deref()
                .map(|error| super::store::bounded(error, 4_096))
                .as_deref(),
        );
        // From here to the acknowledgement is one serialized commit per command.
        // Two conflicting reports racing each other must not be able to leave
        // the row naming one digest and the file holding the other's bytes.
        let commit = self.terminal_commit_lock(command_id);
        let _committing = commit
            .lock()
            .map_err(|_| internal("Device command commit lock was poisoned"))?;
        // Re-read *inside* the lock: whatever was true before it was taken is
        // exactly the state a racing commit may have changed.
        let already_terminal = {
            let store = self.locked_store()?;
            let existing = store
                .device_command(command_id)
                .map_err(internal)?
                .filter(|record| record.device_id == device.device_id)
                .ok_or((404, "Unknown device command".to_string()))?;
            if existing.state.terminal() {
                if let Some(stored) = &existing.terminal_sha256 {
                    if stored != &digest {
                        // The loser, and it changes nothing: not the file, not
                        // the row, not the digest. It is refused before a single
                        // byte of its artifact is written.
                        return Err((
                            409,
                            format!(
                                "This command already reported {} and that result is \
                                 authoritative; a different result cannot replace it",
                                existing.state.as_str()
                            ),
                        ));
                    }
                }
                true
            } else {
                // `/start` is the authorization boundary for a physical effect,
                // so a terminal report is only meaningful from the far side of
                // it. Accepting one for a `queued` or `leased` command would let
                // an authenticated device answer for an action the runner never
                // authorized — and skip the readiness, grant and cancellation
                // checks that boundary exists to make.
                if existing.state != DeviceCommandState::Running {
                    return Err((
                        409,
                        format!(
                            "This command is {} and has not been started; a result can only be \
                             reported for a running command",
                            existing.state.as_str()
                        ),
                    ));
                }
                // Ownership is settled before the artifact is published, not
                // after: an execution that does not hold this command must not
                // be able to write over the artifact path of the one that does.
                //
                // A missing identity is refused as firmly as a wrong one. The
                // pair-of-`Some`s test it replaces let an omitted `execution_id`
                // through — the one form a second execution can always produce.
                match (&existing.execution_id, result.execution_id.as_deref()) {
                    (Some(held), Some(offered)) if held == offered => {}
                    (Some(_), _) => {
                        return Err((
                            409,
                            "This result does not name the execution that holds the command"
                                .to_string(),
                        ));
                    }
                    // Started by a build that had no execution identity to give.
                    // Both ends must be silent about it: an id offered against a
                    // command that never recorded one proves nothing.
                    (None, None) => {}
                    (None, Some(_)) => {
                        return Err((
                            409,
                            "This command was started without an execution identity and cannot be \
                             completed under one"
                                .to_string(),
                        ));
                    }
                }
                false
            }
        };
        // A replay publishes nothing. The stored bytes are the authoritative
        // ones and they are already on disk under this command's name; rewriting
        // them would be a write with no answer it could change.
        if !already_terminal {
            if let Some((bytes, _)) = &decoded {
                self.persist_device_artifact(command_id, bytes)?;
            }
        }
        let record = self
            .locked_store()?
            .complete_device_command(
                &device.device_id,
                command_id,
                result.outcome,
                result.result.as_ref(),
                artifact.as_ref(),
                result.error.as_deref(),
                result.execution_id.as_deref(),
                now_ms,
            )
            .map_err(|error| (409, error))?;
        Ok((
            200,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "command_id": record.command_id,
                "state": record.state.as_str(),
                // The authoritative record, so a retrying device can see that
                // what the runner holds is what it delivered — and stop.
                "acknowledged": true,
                "artifact_sha256": record.artifact.as_ref().map(|artifact| artifact.sha256.clone()),
            }),
            Some(record.command_id),
        ))
    }

    /// The commit lock for one command, minted on first use.
    ///
    /// Swept while the map is held rather than on a timer: an entry nobody else
    /// holds is a command whose commit is over, and dropping it costs one
    /// comparison. The bound is what stops a long-lived runner accumulating one
    /// mutex per command it ever completed.
    fn terminal_commit_lock(&self, command_id: &str) -> Arc<Mutex<()>> {
        let mut locks = match self.terminal_commits.lock() {
            Ok(value) => value,
            // A poisoned map is not a reason to skip serialization: an
            // unshared lock still serializes nothing but is safe to return, and
            // the commit below re-reads authoritative state either way.
            Err(poisoned) => poisoned.into_inner(),
        };
        if locks.len() > 256 {
            locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        }
        Arc::clone(
            locks
                .entry(command_id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// Writes one artifact so a crash can never leave a stored record pointing
    /// at bytes that are not there.
    ///
    /// Staged under a deterministic temporary name, flushed, then renamed onto
    /// the final path — the rename is atomic, so the file at the destination is
    /// either the previous complete artifact or this complete one, never a
    /// half-written mixture. The DB row is written afterwards: an orphaned
    /// staging file or an artifact with no row is recoverable, a row naming
    /// bytes that do not exist is not.
    fn persist_device_artifact(&self, command_id: &str, bytes: &[u8]) -> Result<(), (u16, String)> {
        use std::io::Write;
        let directory = self.paths.root.join("device-artifacts");
        let staging = directory.join("staging");
        std::fs::create_dir_all(&staging).map_err(|error| {
            internal(format!(
                "Could not create device artifact directory: {error}"
            ))
        })?;
        Self::sweep_stale_staging(&staging);
        let temporary = staging.join(format!("{command_id}.part"));
        {
            let mut file = std::fs::File::create(&temporary)
                .map_err(|error| internal(format!("Could not stage device artifact: {error}")))?;
            file.write_all(bytes)
                .map_err(|error| internal(format!("Could not stage device artifact: {error}")))?;
            file.sync_all()
                .map_err(|error| internal(format!("Could not flush device artifact: {error}")))?;
        }
        // The command id names the final file, so a retried report replaces its
        // own bytes with identical ones and can never create a second artifact.
        std::fs::rename(&temporary, directory.join(command_id))
            .map_err(|error| internal(format!("Could not persist device artifact: {error}")))?;
        Ok(())
    }

    /// Removes staged files a crashed upload left behind. Best-effort and
    /// silent: an orphan costs disk, never correctness, and failing a live
    /// delivery because an old temporary file could not be removed would be the
    /// worse trade.
    fn sweep_stale_staging(staging: &std::path::Path) {
        const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(60 * 60);
        let Ok(entries) = std::fs::read_dir(staging) else {
            return;
        };
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .map(|modified| modified.elapsed().unwrap_or_default() > STALE_AFTER)
                .unwrap_or(false);
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// One chunk of a live microphone stream.
    ///
    /// The store lock is taken once and held across the disk write on purpose:
    /// that is what makes "check the sequence, append, move the counter" atomic,
    /// and therefore what stops two concurrent posts of the same chunk from
    /// writing the audio twice.
    fn voice_chunk(
        &self,
        body: &[u8],
        device: &DeviceRecord,
        session_id: &str,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let request: VoiceChunkRequest = serde_json::from_slice(body)
            .map_err(|error| (400, format!("Invalid voice chunk: {error}")))?;
        request.validate().map_err(|error| (400, error))?;
        let audio = STANDARD
            .decode(&request.audio_base64)
            .map_err(|_| (400, "Voice chunk audio is not valid base64".to_string()))?;
        if audio.len() > MAX_VOICE_CHUNK_BYTES {
            return Err((413, "Voice chunk exceeds the per-chunk ceiling".to_string()));
        }
        let mut store = self.locked_store()?;
        let outcome = super::voice::accept_chunk(
            &self.paths.root,
            &mut store,
            &device.device_id,
            session_id,
            &request,
            &audio,
            now_ms,
        )?;
        Ok((
            200,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "session_id": session_id,
                "accepted": outcome.accepted,
                "next_sequence": outcome.next_sequence,
                "bytes": outcome.bytes,
                // The device's stop signal, on the reply to a request it is
                // already making. No second poll exists for a cancellation to
                // be missed on.
                "stop": outcome.stop,
            }),
            Some(session_id.to_string()),
        ))
    }

    /// The device ending a stream, and with it the control command it rode on.
    fn voice_close(
        &self,
        body: &[u8],
        device: &DeviceRecord,
        session_id: &str,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let request: VoiceCloseRequest = if body.is_empty() {
            VoiceCloseRequest {
                protocol_version: REMOTE_PROTOCOL_VERSION,
                error: None,
            }
        } else {
            serde_json::from_slice(body)
                .map_err(|error| (400, format!("Invalid voice close: {error}")))?
        };
        request.validate().map_err(|error| (400, error))?;
        let mut store = self.locked_store()?;
        let record = super::voice::close(
            &mut store,
            &device.device_id,
            session_id,
            request.error.as_deref(),
            now_ms,
        )?;
        Ok((
            200,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "session_id": record.session_id,
                "state": record.state.as_str(),
                "chunks": record.next_sequence,
                "bytes": record.bytes,
            }),
            Some(record.session_id),
        ))
    }

    /// The `applicationServerKey` a browser needs before it can subscribe.
    ///
    /// Public by construction — it is the *public* half of this runner's VAPID
    /// identity, and a push service checks signatures against it. Answering 404
    /// when Web Push is not configured is what tells the client not to offer
    /// notifications at all.
    fn device_push_key(&self) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        match super::push::application_server_key(&self.paths, self.secrets.as_ref()) {
            Ok(Some(key)) => Ok((
                200,
                serde_json::json!({
                    "protocol_version": REMOTE_PROTOCOL_VERSION,
                    "backend": "web_push",
                    "application_server_key": key,
                }),
                None,
            )),
            Ok(None) => Err((404, "This runner does not send Web Push".to_string())),
            Err(error) => Err((503, error)),
        }
    }

    fn device_push_register(
        &self,
        body: &[u8],
        device_id: &str,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let parsed: serde_json::Value = serde_json::from_slice(body)
            .map_err(|error| (400, format!("Invalid push registration: {error}")))?;
        let backend = parsed
            .get("backend")
            .and_then(|value| value.as_str())
            .ok_or((400, "A push registration needs a 'backend'".to_string()))?;
        // A Web Push registration is the browser's whole subscription — the
        // endpoint plus the two keys it will be encrypted to. It is validated
        // here, before storage, so an unusable subscription is refused at the
        // moment the device can still be told about it.
        let token = if backend == "web_push" {
            let subscription: super::push::WebPushSubscription =
                serde_json::from_value(parsed.get("subscription").cloned().unwrap_or_default())
                    .map_err(|error| (400, format!("Invalid push subscription: {error}")))?;
            subscription.validate().map_err(|error| (400, error))?;
            serde_json::to_string(&subscription).map_err(internal)?
        } else {
            parsed
                .get("token")
                .and_then(|value| value.as_str())
                .ok_or((400, "A push registration needs a 'token'".to_string()))?
                .to_string()
        };
        self.locked_store()?
            .save_push_registration(device_id, backend, &token, now_ms)
            .map_err(|error| (400, error))?;
        Ok((
            200,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "registered": true,
            }),
            Some(device_id.to_string()),
        ))
    }

    fn device_push_forget(
        &self,
        device_id: &str,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        self.locked_store()?
            .delete_push_registration(device_id)
            .map_err(internal)?;
        Ok((
            200,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "registered": false,
            }),
            Some(device_id.to_string()),
        ))
    }

    fn mobile_revoke_self(
        &self,
        device_id: &str,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let killer = self
            .desktop
            .as_ref()
            .map(|desktop| Arc::clone(desktop) as Arc<dyn super::store::DesktopSessionKiller>);
        self.locked_store()?
            .revoke_device(
                device_id,
                "Self-revoked from the paired mobile device",
                now_ms,
                self.secrets.as_ref(),
                killer.as_deref(),
            )
            .map_err(internal)?;
        Ok((
            200,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "revoked": true,
            }),
            Some(device_id.to_string()),
        ))
    }

    // --- `/v1/remote/node/*` handlers (roadmap K17) ------------------------

    /// The app-data directory this node's hub and workflow service live under.
    /// The daemon root is a child of it, which is the same derivation the mobile
    /// workflow routes above already make.
    fn app_data(&self) -> Result<std::path::PathBuf, (u16, String)> {
        self.paths
            .root
            .parent()
            .map(std::path::Path::to_path_buf)
            .ok_or_else(|| internal("Daemon root has no app-data parent"))
    }

    /// The operator-set identity of this node.
    ///
    /// Both values are operator statements held in the daemon's own meta table,
    /// not inferred: nothing can derive which jurisdiction a machine's disks are
    /// in, and a guess there is worse than an explicit
    /// [`RESIDENCY_UNSPECIFIED`](little_monkey_lib::node_placement::RESIDENCY_UNSPECIFIED),
    /// which a residency rule naming a real zone never matches.
    fn node_identity(&self, store: &DaemonStore) -> (String, String) {
        let residency = store
            .get_meta(NODE_RESIDENCY_META)
            .ok()
            .flatten()
            .filter(|value| little_monkey_lib::node_placement::validate_residency(value).is_ok())
            .unwrap_or_else(|| {
                little_monkey_lib::node_placement::RESIDENCY_UNSPECIFIED.to_string()
            });
        let name = store
            .get_meta(NODE_NAME_META)
            .ok()
            .flatten()
            .unwrap_or_else(|| self.host.runner_id.clone());
        (name, residency)
    }

    fn describe_node(
        &self,
        now_ms: u64,
    ) -> Result<little_monkey_lib::node_placement::NodeDescriptor, (u16, String)> {
        use little_monkey_lib::m3_runtime_hub::M3HardwareProbe;
        let store = DaemonStore::open(&self.paths).map_err(internal)?;
        let config = crate::daemon::store::DaemonConfig::load(&self.paths).map_err(internal)?;
        let backpressure = crate::daemon::backpressure_for(&store, &config).map_err(internal)?;
        let (node_name, residency) = self.node_identity(&store);
        // The same probe the admission loop uses, so a placer reads the numbers
        // this node's own scheduler will judge the job against — not a second,
        // differently-collected view of the same machine.
        let hardware = little_monkey_lib::m3_production::SystemM3HardwareProbe
            .snapshot()
            .map_err(|error| {
                (
                    503,
                    format!("This node could not measure its own hardware: {error}"),
                )
            })?;
        Ok(little_monkey_lib::node_placement::NodeDescriptor {
            protocol_version: little_monkey_lib::node_placement::NODE_PROTOCOL_VERSION,
            runner_id: self.host.runner_id.clone(),
            node_name,
            residency,
            accelerators: little_monkey_lib::node_placement::describe_accelerators(&hardware),
            resident_models: resident_models(&self.app_data()?),
            hardware,
            accepting: backpressure.accepting,
            queue_depth: backpressure.queue_depth,
            queue_capacity: backpressure.queue_capacity,
            captured_at_ms: now_ms,
        })
    }

    fn node_descriptor(&self) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        // `captured_at_ms` comes from the hardware probe's own stamp rather than
        // from the request clock: the snapshot is the measurement, and stamping
        // it with "when you asked" would make a cached or slow probe look fresh.
        let descriptor = self.describe_node(0)?;
        let captured_at_ms = descriptor.hardware.captured_at_ms;
        let descriptor = little_monkey_lib::node_placement::NodeDescriptor {
            captured_at_ms,
            ..descriptor
        };
        Ok((
            200,
            serde_json::to_value(&descriptor).map_err(internal)?,
            None,
        ))
    }

    /// The cheap half of [`Self::node_descriptor`], for the heartbeat.
    ///
    /// Separate because the descriptor probes hardware — which forks
    /// `nvidia-smi` on CUDA hosts — and a placer polling every node every
    /// 30 seconds must not make each node pay that. This reads only the queue.
    fn node_health(
        &self,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let store = DaemonStore::open(&self.paths).map_err(internal)?;
        let config = crate::daemon::store::DaemonConfig::load(&self.paths).map_err(internal)?;
        let backpressure = crate::daemon::backpressure_for(&store, &config).map_err(internal)?;
        let placed_active = self.locked_store()?.placed_run_count().map_err(internal)?;
        let health = little_monkey_lib::node_placement::NodeHealth {
            protocol_version: little_monkey_lib::node_placement::NODE_PROTOCOL_VERSION,
            runner_id: self.host.runner_id.clone(),
            now_ms,
            accepting: backpressure.accepting,
            queue_depth: backpressure.queue_depth,
            queue_capacity: backpressure.queue_capacity,
            placed_active,
        };
        Ok((200, serde_json::to_value(&health).map_err(internal)?, None))
    }

    /// **Roadmap K17 S2: this node takes ownership of a foreign `RunSpec`.**
    ///
    /// The order of the checks is the contract. The spec is validated against
    /// the shared protocol first, then against *this node's* facts — its
    /// residency, its identity, its kill switch — and only then handed to the
    /// queue, which owns the last class of refusal (a workspace root that does
    /// not exist here, a target this node cannot execute). Nothing is recorded
    /// until the queue has accepted, so a refused placement leaves no row
    /// claiming the node took work it did not.
    // --- `/v1/remote/peer/*` -----------------------------------------------

    /// Take one envelope from a paired peer.
    ///
    /// Everything that decides *whether* it runs lives in the gate; this is the
    /// transport half — parse, refuse to act while the kill switch is on, and
    /// turn the gate's verdict into a status code the sender can act on.
    fn peer_message_post(
        &self,
        device: &DeviceRecord,
        body: &[u8],
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let Some(queue) = self.peer_runs.as_ref() else {
            return Err((
                501,
                "This node build does not accept peer messages".to_string(),
            ));
        };
        let envelope: little_monkey_lib::peers::PeerEnvelope = serde_json::from_slice(body)
            .map_err(|error| (400, format!("Invalid peer envelope: {error}")))?;
        let mut store = DaemonStore::open(&self.paths).map_err(internal)?;
        if store.kill_switch().map_err(internal)? {
            return Err((409, "Global kill switch is engaged".to_string()));
        }
        let granted = granted_capabilities(device);
        let artifacts = crate::daemon::peer_ingress::peer_content_store(&self.paths)
            .map_err(|error| (500, error))?;
        let context = crate::daemon::peer_ingress::PeerContext {
            device_id: &device.device_id,
            granted: &granted,
            revoked: !device.active(),
            local_instance_id: &self.host.runner_id,
            artifacts: &artifacts,
        };
        let accepted = crate::daemon::peer_ingress::accept_peer_envelope(
            &mut store,
            queue.as_ref(),
            &envelope,
            &context,
            i64::try_from(now_ms).unwrap_or(i64::MAX),
        )
        .map_err(internal)?;

        use crate::daemon::peer_ingress::PeerAcceptance;
        match accepted {
            PeerAcceptance::Accepted {
                thread_id, job_id, ..
            } => Ok((
                202,
                serde_json::json!({
                    "accepted": true,
                    "thread_id": thread_id,
                    "message_id": envelope.message_id,
                    // The peer's own handle for correlating a result later. The
                    // local job id is deliberately not returned: it is this
                    // node's business, and a peer that knew it would learn
                    // nothing it can use.
                    "correlation_id": envelope.correlation_id,
                    "state": "queued",
                    "queued": !job_id.is_empty(),
                }),
                Some(thread_id_target(&envelope.thread_id)),
            )),
            PeerAcceptance::AcceptedPending { thread_id, .. } => Ok((
                202,
                serde_json::json!({
                    "accepted": true,
                    "thread_id": thread_id,
                    "message_id": envelope.message_id,
                    "correlation_id": envelope.correlation_id,
                    // Durably taken, not yet queued. Saying "accepted" is the
                    // honest answer: a retry would be refused as a duplicate,
                    // and this node will finish the submission itself.
                    "state": "accepted",
                    "queued": false,
                }),
                Some(thread_id_target(&envelope.thread_id)),
            )),
            PeerAcceptance::Duplicate {
                thread_id,
                accepted,
                ..
            } => Ok((
                200,
                serde_json::json!({
                    "accepted": accepted,
                    "thread_id": thread_id,
                    "message_id": envelope.message_id,
                    "correlation_id": envelope.correlation_id,
                    "state": "duplicate",
                    "queued": false,
                }),
                Some(thread_id_target(&envelope.thread_id)),
            )),
            PeerAcceptance::Rejected { reason, .. } => {
                use little_monkey_lib::peers::PeerRejection;
                let status = match reason {
                    PeerRejection::MissingCapability | PeerRejection::PeerRevoked => 403,
                    PeerRejection::Duplicate => 409,
                    _ => 400,
                };
                Err((status, reason.message().to_string()))
            }
        }
    }

    /// One peer introducing itself, and learning what it may actually do here.
    ///
    /// The only route on this plane that changes nothing durable about
    /// authority: what the caller advertises and asks for is stored beside the
    /// pairing, never merged into it, so an operator sees the ask and decides.
    /// `granted` in the reply is computed here from the pairing record — the
    /// caller cannot influence it by anything it sent.
    fn peer_hello_post(
        &self,
        device: &DeviceRecord,
        body: &[u8],
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let hello: PeerHelloRequest = serde_json::from_slice(body)
            .map_err(|error| (400, format!("Invalid peer hello: {error}")))?;
        hello.validate().map_err(|error| (400, error))?;
        let granted = peer_capabilities_of(&granted_capabilities(device));
        if device.active() {
            RemoteStore::open(&self.paths.root)
                .map_err(internal)?
                .record_peer_advertisement(
                    &device.device_id,
                    &hello.instance_id,
                    &hello.advertised,
                    &hello.requested,
                    now_ms,
                )
                .map_err(internal)?;
        }
        let response = PeerHelloResponse {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            instance_id: self.host.runner_id.clone(),
            now_ms,
            advertised: all_peer_capabilities(),
            granted,
        };
        Ok((
            200,
            serde_json::to_value(&response).map_err(internal)?,
            Some(format!("peer:{}", device.device_id)),
        ))
    }

    /// Take the bytes behind an artifact a peer is about to reference.
    ///
    /// Push rather than pull. The digest the sender declared is a checksum, not
    /// an identifier this node trusts: the content store hashes what it
    /// actually wrote, and a mismatch is refused rather than stored under the
    /// name the sender chose.
    ///
    /// # This is where a peer earns the right to reference content
    ///
    /// The content store is shared with every other artifact on the machine, so
    /// a blob being *in* it says nothing about who put it there. The durable
    /// receipt written here does: it names the authenticated pairing this
    /// request resolved to, the id and digest of what verified, the size, and
    /// the metadata the receiver validated — and it is the only thing that lets
    /// a later envelope name these bytes.
    ///
    /// It is written last, after the content passed integrity validation, so a
    /// failed upload leaves no authorization behind.
    fn peer_artifact_post(
        &self,
        device: &DeviceRecord,
        body: &[u8],
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let upload: PeerArtifactUpload = serde_json::from_slice(body)
            .map_err(|error| (400, format!("Invalid peer artifact: {error}")))?;
        upload.validate().map_err(|error| (400, error))?;
        let bytes = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            &upload.content_base64,
        )
        .map_err(|_| (400, "Peer artifact content is not valid base64".to_string()))?;
        // Digest first, store second. The content store is content-addressed,
        // so writing and then comparing ids would detect the mismatch just as
        // well — but only after publishing the bytes into a store shared with
        // runs, channels and the operator's own imports. A peer whose upload is
        // refused must leave nothing behind, not an unreferenced blob it can
        // keep adding to.
        let digest = crate::durable_run::sha256_hex(&bytes);
        if digest != upload.sha256.to_ascii_lowercase() {
            return Err((
                400,
                "Peer artifact content does not match its declared digest".to_string(),
            ));
        }
        let store = crate::daemon::peer_ingress::peer_content_store(&self.paths)
            .map_err(|error| (500, error))?;
        let blob = store
            .put(&bytes)
            .map_err(|error| (400, format!("Could not store the peer artifact: {error}")))?;
        DaemonStore::open(&self.paths)
            .map_err(internal)?
            .record_peer_artifact_receipt(
                &device.device_id,
                &blob.id,
                &blob.id,
                blob.size,
                upload.filename.as_deref(),
                upload.media_type.as_deref(),
                i64::try_from(now_ms).unwrap_or(i64::MAX),
            )
            .map_err(internal)?;
        let stored = PeerArtifactStored {
            artifact_id: blob.id,
            sha256: upload.sha256.to_ascii_lowercase(),
            size_bytes: blob.size,
        };
        Ok((
            201,
            serde_json::to_value(&stored).map_err(internal)?,
            Some(format!("peer-artifact:{}", device.device_id)),
        ))
    }

    /// What a thread looks like now, including results for finished work.
    ///
    /// The peer polls this rather than being called back. That is not a
    /// shortcut: a callback would mean every receiving node holding an
    /// outbound pairing to every peer that ever wrote to it, and a peer that
    /// went away leaving retries behind. Polling keeps the trust one-way per
    /// direction, the same shape the mobile path already uses.
    fn peer_thread_get(
        &self,
        device: &DeviceRecord,
        thread_id: &str,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let mut store = DaemonStore::open(&self.paths).map_err(internal)?;
        // Scoped to the calling pairing in the query itself: a peer reads its
        // own threads and nobody else's, and a thread belonging to someone else
        // is indistinguishable from one that does not exist, so probing cannot
        // enumerate other peers.
        let Some(thread) = store
            .peer_thread(&device.device_id, thread_id)
            .map_err(internal)?
        else {
            return Err((404, "Unknown peer thread".to_string()));
        };
        self.materialize_peer_results(&mut store, &thread, now_ms)?;

        let messages = store
            .peer_messages(&device.device_id, thread_id, 200)
            .map_err(internal)?;
        let rows: Vec<serde_json::Value> = messages
            .iter()
            .map(|message| {
                serde_json::json!({
                    "message_id": message.message_id,
                    "direction": message.direction.as_str(),
                    "kind": message.kind,
                    "correlation_id": message.correlation_id,
                    "disposition": message.disposition.as_str(),
                    "rejection": message.rejection,
                    // A result row's payload is what this node produced and is
                    // meant to travel; an inbound row's is the peer's own
                    // envelope, echoed back unchanged.
                    "payload": serde_json::from_str::<serde_json::Value>(&message.envelope_json)
                        .unwrap_or(serde_json::Value::Null),
                    "created_at_ms": message.created_at_ms,
                })
            })
            .collect();
        Ok((
            200,
            serde_json::json!({
                "thread_id": thread.thread_id,
                "created_at_ms": thread.created_at_ms,
                "last_activity_at_ms": thread.last_activity_at_ms,
                "messages": rows,
            }),
            Some(thread_id_target(thread_id)),
        ))
    }

    /// Turn finished runs into result rows the peer can read.
    ///
    /// Runs when the peer polls rather than on a timer: nothing needs the
    /// answer until someone asks for it, and doing it here means a result is
    /// written exactly once, by the same idempotent insert, whether the peer
    /// polls once or fifty times.
    fn materialize_peer_results(
        &self,
        store: &mut DaemonStore,
        thread: &crate::daemon::peer_store::PeerThreadRecord,
        now_ms: u64,
    ) -> Result<(), (u16, String)> {
        let awaiting = store
            .peer_messages_awaiting_result(&thread.peer_device_id, &thread.thread_id)
            .map_err(internal)?;
        if awaiting.is_empty() {
            return Ok(());
        }
        let ledger = self.run_ledger()?;
        for message in awaiting {
            let Some(job_id) = message.job_id.as_deref() else {
                continue;
            };
            let Some(job) = store.get_job(job_id).map_err(internal)? else {
                continue;
            };
            let Some(run_id) = job.run_id.as_deref() else {
                continue; // Queued but not started yet.
            };
            let Ok(events) = ledger.load_events(run_id, 0, 1_000) else {
                continue; // Not recorded yet — the next poll tries again.
            };
            let Some(outcome) = terminal_outcome(&events) else {
                continue; // Still running.
            };
            store
                .record_peer_result(
                    &thread.thread_id,
                    &thread.peer_device_id,
                    &self.host.runner_id,
                    &format!("result-{}", message.message_id),
                    message.correlation_id.as_deref(),
                    job_id,
                    &serde_json::json!({
                        "in_reply_to": message.message_id,
                        "state": outcome.state,
                        "text": outcome.text,
                    })
                    .to_string(),
                    i64::try_from(now_ms).unwrap_or(i64::MAX),
                )
                .map_err(internal)?;
        }
        Ok(())
    }

    fn place_run(
        &self,
        body: &[u8],
        device_id: &str,
        request_sha256: &str,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let Some(queue) = self.placement.as_ref() else {
            return Err((
                501,
                "This node build does not accept placed runs".to_string(),
            ));
        };
        let request: little_monkey_lib::node_placement::PlaceRunRequest =
            serde_json::from_slice(body)
                .map_err(|error| (400, format!("Invalid placement request: {error}")))?;
        request
            .validate()
            .map_err(|error| (400, format!("Placement request is invalid: {error}")))?;

        let store = DaemonStore::open(&self.paths).map_err(internal)?;
        if store.kill_switch().map_err(internal)? {
            return Err((409, "Global kill switch is engaged".to_string()));
        }
        let (_, residency) = self.node_identity(&store);
        drop(store);

        // The placer states the rule it applied and this node checks it rather
        // than trusting it. Two owned machines is exactly the case where an
        // alias silently starts pointing somewhere else — a rotated bundle
        // restored onto a different host, a re-provisioned box reusing a name —
        // and a data-residency rule that only the *sender* enforces is not
        // enforced at all.
        if let Some(required) = &request.required_residency {
            if required != &residency {
                return Err((
                    409,
                    format!(
                        "This node's data residency is '{residency}', not the required '{required}'"
                    ),
                ));
            }
        }
        if let Some(expected) = &request.expected_runner_id {
            if expected != &self.host.runner_id {
                return Err((
                    409,
                    format!(
                        "This node is '{}', not the expected '{expected}'",
                        self.host.runner_id
                    ),
                ));
            }
        }

        let submitted_run_id = request.spec.run_id.clone();
        // A spec this node already owns is the same placement, not a second
        // one. The signed-request replay guard covers an identical *retried*
        // request; it cannot see a fresh request carrying a spec already placed.
        if let Some(existing) = self
            .locked_store()?
            .placed_run(&submitted_run_id)
            .map_err(internal)?
        {
            return Ok((
                200,
                serde_json::to_value(little_monkey_lib::node_placement::PlaceRunResponse {
                    protocol_version: little_monkey_lib::node_placement::NODE_PROTOCOL_VERSION,
                    submitted_run_id,
                    node_run_id: existing.node_run_id,
                    job_id: existing.job_id,
                    state: "queued".to_string(),
                    accepted_at_ms: existing.created_at_ms,
                    residency: existing.residency,
                })
                .map_err(internal)?,
                Some(existing.submitted_run_id),
            ));
        }

        let placed = queue
            .place(&request.spec)
            .map_err(|error| (409, format!("This node refused the placement: {error}")))?;
        self.locked_store()?
            .insert_placed_run(&super::store::PlacedRunRecord {
                submitted_run_id: submitted_run_id.clone(),
                device_id: device_id.to_string(),
                node_run_id: placed.node_run_id.clone(),
                job_id: placed.job_id.clone(),
                residency: residency.clone(),
                // The digest of the signed request, which covers the exact spec
                // bytes this node accepted. What was enforced here is auditable
                // against what the submitter says it sent.
                spec_sha256: request_sha256.to_string(),
                created_at_ms: now_ms,
            })
            .map_err(internal)?;
        Ok((
            201,
            serde_json::to_value(little_monkey_lib::node_placement::PlaceRunResponse {
                protocol_version: little_monkey_lib::node_placement::NODE_PROTOCOL_VERSION,
                submitted_run_id: submitted_run_id.clone(),
                node_run_id: placed.node_run_id,
                job_id: placed.job_id,
                state: placed.state,
                accepted_at_ms: now_ms,
                residency,
            })
            .map_err(internal)?,
            Some(submitted_run_id),
        ))
    }

    /// One placed run's current state, keyed by the *submitter's* run id.
    ///
    /// Scoped to the placing device: a device may read the placements it made
    /// and no others, which is the same rule `RemoteScopes::permits_run` applies
    /// to the control plane's run listing.
    fn placed_run_status(
        &self,
        device_id: &str,
        submitted_run_id: &str,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let Some(queue) = self.placement.as_ref() else {
            return Err((
                501,
                "This node build does not accept placed runs".to_string(),
            ));
        };
        let record = self
            .locked_store()?
            .placed_run(submitted_run_id)
            .map_err(internal)?
            .filter(|record| record.device_id == device_id)
            .ok_or((404, "No such placed run".to_string()))?;
        let state = queue
            .placed_state(&record.job_id)
            .map_err(internal)?
            .unwrap_or(PlacedJobState {
                // The placement row exists and the job row does not, which is
                // what job retention leaves behind. Reported as its own state
                // rather than as "failed": the node genuinely does not know how
                // it ended, and saying "failed" would be a claim.
                state: "unknown".to_string(),
                terminal: true,
                updated_at_ms: record.created_at_ms,
                last_error: Some(
                    "This node no longer retains the job row for this placement".to_string(),
                ),
            });
        Ok((
            200,
            serde_json::to_value(little_monkey_lib::node_placement::PlacedRunStatus {
                protocol_version: little_monkey_lib::node_placement::NODE_PROTOCOL_VERSION,
                submitted_run_id: record.submitted_run_id.clone(),
                node_run_id: record.node_run_id,
                job_id: record.job_id,
                state: state.state,
                terminal: state.terminal,
                updated_at_ms: state.updated_at_ms,
                last_error: state.last_error,
            })
            .map_err(internal)?,
            Some(record.submitted_run_id),
        ))
    }

    fn audit_denied(
        &self,
        now_ms: u64,
        device_id: Option<&str>,
        action: &str,
        target: Option<&str>,
        outcome: &str,
    ) {
        if let Ok(mut store) = self.store.lock() {
            let _ = store.audit(now_ms, device_id, action, target, outcome, None);
        }
    }
}

/// How often the long-poll checks for work. Short enough that a queued command
/// reaches a waiting phone promptly, long enough that a held connection costs
/// nothing measurable.
const LONG_POLL_TICK_MS: u64 = 500;

/// The two requests that may wait.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LongPollTarget {
    /// `GET /v1/remote/device/commands/next` — wait for work to exist.
    Lease,
    /// `GET /v1/remote/device/commands/{id}/control` — wait for a running
    /// command's control state to change. A watcher held open like this is why
    /// cancelling a recording does not need the device to poll every second.
    Control(String),
}

/// What a request wants to wait for and for how long, capped at the lease
/// length, or `None` when this request is not one that waits.
fn long_poll_target(request: &ApiRequest) -> Option<(LongPollTarget, u64)> {
    if request.method != "GET" {
        return None;
    }
    let (path, query) = request
        .path_and_query
        .split_once('?')
        .map_or((request.path_and_query.as_str(), ""), |value| value);
    let segments = path
        .trim_end_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let target = match segments.as_slice() {
        ["v1", "remote", "device", "commands", "next"] => LongPollTarget::Lease,
        ["v1", "remote", "device", "commands", command_id, "control"] => {
            LongPollTarget::Control((*command_id).to_string())
        }
        _ => return None,
    };
    let wait_ms = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == "wait_ms")
        .and_then(|(_, value)| value.parse::<u64>().ok())
        .unwrap_or(0);
    (wait_ms > 0).then(|| (target, wait_ms.min(DEVICE_LEASE_MS)))
}

/// The three sets an operator (and the phone) must be able to tell apart:
/// what Little Monkey granted, what the build supports, what the OS permits —
/// and the intersection that actually decides. Computed in one place so the
/// desktop card and the phone's own screen cannot drift.
fn device_state_json(device: &DeviceRecord, surface: Option<&DeviceSurface>) -> serde_json::Value {
    let granted = if device.capabilities.is_empty() {
        legacy_capabilities(&device.scopes)
    } else {
        device.capabilities.clone()
    };
    // One row per physical capability, with the four axes kept apart and the
    // single reason it is not effective named. A caller that only gets the
    // intersection cannot tell an operator what to do about it.
    let physical = PHYSICAL_DEVICE_CAPABILITIES
        .iter()
        .map(|capability| {
            let block = capability_block(&granted, surface, *capability);
            serde_json::json!({
                "capability": capability,
                "granted": granted.contains(capability),
                "supported": surface.is_some_and(|surface| surface.capabilities.contains(capability)),
                "permission": surface.map(|surface| surface.permission(*capability)),
                "readiness": surface.map(|surface| surface.readiness(*capability)),
                "effective": block.is_none(),
                "blocked_by": block.map(|block| block.as_str()),
                "reason": block.map(|block| block.explain(*capability)),
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "protocol_version": REMOTE_PROTOCOL_VERSION,
        "device_id": device.device_id,
        "device_name": device.device_name,
        "granted": granted,
        "advertised": surface.map(|surface| surface.capabilities.clone()),
        "os_permissions": surface.map(|surface| surface.permissions.clone()),
        "readiness": surface.map(|surface| surface.readiness.clone()),
        "effective": effective_capabilities(&granted, surface),
        "physical": physical,
        "surface": surface,
        "max_artifact_bytes": device.scopes.max_artifact_bytes,
    })
}

fn require_action(scopes: &RemoteScopes, action: RemoteAction) -> Result<(), (u16, String)> {
    if scopes.permits(action) {
        Ok(())
    } else {
        Err((403, format!("Remote action '{action:?}' is not paired")))
    }
}

/// Capability gate for the mobile extension. A device paired before
/// capabilities existed resolves through `legacy_capabilities`, which maps
/// only the legacy run-scope actions — so legacy pairings can never reach
/// chat, workflow launch, or capture without an explicit re-pair.
fn require_capability(
    device: &DeviceRecord,
    capability: DeviceCapability,
) -> Result<(), (u16, String)> {
    let effective = if device.capabilities.is_empty() {
        legacy_capabilities(&device.scopes)
    } else {
        device.capabilities.clone()
    };
    if effective.contains(&capability) {
        Ok(())
    } else {
        Err((
            403,
            format!("Device capability '{capability:?}' is not granted"),
        ))
    }
}

/// A peer needs at least one of the three peer grants to reach the peer plane
/// at all. Which one a given envelope needs is decided per envelope, inside the
/// gate, because that depends on what the envelope contains — but a pairing
/// with no peer standing whatsoever never gets that far, and never gets to
/// create a thread row.
/// The grants recorded for this device, resolving the legacy empty-set
/// convention the same way `require_capability` does.
///
/// Not [`protocol::effective_capabilities`], which additionally drops a
/// physical capability the device's own surface cannot serve: a peer grant is
/// never physical, and a peer has no surface to ask.
/// Refuse a pairing whose entire standing is peer standing.
///
/// For the device-plane routes that are gated by authentication alone. Those
/// routes are self-service for a paired phone — advertising a surface only ever
/// narrows what is effective, and the queue hands out only commands already
/// queued for that device — but a peer is not a phone: it has no hardware here,
/// nothing is ever queued for it, and there is no reading of "peer standing" in
/// which the plane that serves a phone is part of it.
fn refuse_peer_only(device: &DeviceRecord) -> Result<(), (u16, String)> {
    if crate::daemon::remote::protocol::is_peer_only(&granted_capabilities(device)) {
        return Err((403, "A peer cannot act as a device here".to_string()));
    }
    Ok(())
}

fn granted_capabilities(device: &DeviceRecord) -> std::collections::BTreeSet<DeviceCapability> {
    if device.capabilities.is_empty() {
        legacy_capabilities(&device.scopes)
    } else {
        device.capabilities.clone()
    }
}

/// What the audit trail records a peer request against. The thread, never the
/// message text.
fn thread_id_target(thread_id: &str) -> String {
    format!("peer-thread:{thread_id}")
}

/// One finished run, as a peer result.
struct PeerRunOutcome {
    state: &'static str,
    text: String,
}

/// Read a run's events and, if it ended, say how.
///
/// The assistant's own output is the answer when there is one; a summary is
/// the fallback, and a failure or a cancellation is reported as such rather
/// than as an empty success. A run still going produces `None`, which is how
/// the caller knows to leave the request waiting.
fn terminal_outcome(
    events: &[little_monkey_lib::run_protocol::RunEventEnvelope],
) -> Option<PeerRunOutcome> {
    let mut assistant_text = String::new();
    let mut summary: Option<String> = None;
    let mut failure: Option<String> = None;
    let mut cancelled = false;
    let mut completed = false;
    for envelope in events {
        match &envelope.event {
            RunEvent::ModelDelta { channel, text, .. } => {
                if matches!(channel, OutputChannel::Assistant) {
                    assistant_text.push_str(text);
                }
            }
            RunEvent::Completed { summary: value, .. } => {
                completed = true;
                summary = value.clone();
            }
            RunEvent::Failed { message, .. } => failure = Some(message.clone()),
            RunEvent::Cancelled { .. } => cancelled = true,
            _ => {}
        }
    }
    if let Some(reason) = failure {
        return Some(PeerRunOutcome {
            state: "failed",
            text: bounded_text(&reason, 2_048),
        });
    }
    if cancelled {
        return Some(PeerRunOutcome {
            state: "cancelled",
            text: "The run was cancelled on the receiving installation.".to_string(),
        });
    }
    if !completed {
        return None;
    }
    let text = if assistant_text.trim().is_empty() {
        summary.unwrap_or_else(|| "(The run completed without any output.)".to_string())
    } else {
        assistant_text
    };
    Some(PeerRunOutcome {
        state: "succeeded",
        text: bounded_text(&text, 16 * 1024),
    })
}

fn require_any_peer_capability(device: &DeviceRecord) -> Result<(), (u16, String)> {
    let effective = if device.capabilities.is_empty() {
        legacy_capabilities(&device.scopes)
    } else {
        device.capabilities.clone()
    };
    let has_peer_standing = [
        DeviceCapability::PeerMessage,
        DeviceCapability::PeerTaskRequest,
        DeviceCapability::PeerArtifact,
    ]
    .iter()
    .any(|capability| effective.contains(capability));
    if has_peer_standing {
        Ok(())
    } else {
        Err((403, "This pairing is not a peer".to_string()))
    }
}

fn summarize(run: &StoredRun, shared: &SharedLedger) -> Result<RunSummary, String> {
    let label = match &run.spec.target {
        ModelTargetSnapshot::ManagedLlama { label, .. }
        | ModelTargetSnapshot::Ollama { label, .. }
        | ModelTargetSnapshot::Provider { label, .. } => label.clone(),
    };
    Ok(RunSummary {
        run_id: run.spec.run_id.clone(),
        status: format!("{:?}", run.status).to_ascii_lowercase(),
        kind: format!("{:?}", run.spec.kind).to_ascii_lowercase(),
        created_at_ms: run.spec.created_at_ms,
        updated_at_ms: run.updated_at_ms,
        last_sequence: run.last_sequence,
        workspace_id: run
            .spec
            .workspace
            .as_ref()
            .map(|workspace| workspace.workspace_id.clone()),
        model_label: label,
        pending_approval_count: shared.pending_approvals(&run.spec.run_id)?.len(),
    })
}

fn approval_json(approval: &StoredApproval) -> serde_json::Value {
    serde_json::json!({
        "run_id": approval.run_id,
        "request_id": approval.request_id,
        "tool_call_id": approval.tool_call_id,
        "tool_name": approval.tool_name,
        "operation_sha256": approval.operation_sha256,
        "expires_at_ms": approval.expires_at_ms,
    })
}

fn control_recorder(
    shared: &SharedLedger,
    run_id: &str,
    device_id: &str,
) -> Result<Arc<DurableRunRecorder>, String> {
    DurableRunRecorder::attach(
        shared.run_ledger()?,
        run_id,
        "remote-controller".to_string(),
        little_monkey_lib::run_protocol::ClientIdentity {
            client_id: device_id.to_string(),
            instance_id: format!("remote-{device_id}"),
            kind: little_monkey_lib::run_protocol::ClientKind::RemoteRunner,
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    )
}

fn parse_query(query: &str) -> Result<std::collections::BTreeMap<String, String>, (u16, String)> {
    let mut output = std::collections::BTreeMap::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let key = key.into_owned();
        if output.insert(key.clone(), value.into_owned()).is_some() {
            return Err((400, format!("Duplicate query parameter '{key}'")));
        }
    }
    Ok(output)
}

/// The remote node's ledger sits beside its daemon database, under the same app
/// data directory `DaemonPaths` derives everything else from.
fn audit_for(paths: &DaemonPaths) -> little_monkey_lib::subsystem_audit::SubsystemAudit {
    match paths.ledger_db.parent() {
        Some(app_data_dir) => {
            little_monkey_lib::subsystem_audit::SubsystemAudit::in_data_dir(app_data_dir)
        }
        None => little_monkey_lib::subsystem_audit::SubsystemAudit::disabled(
            "the daemon ledger path has no app-data parent to record beside",
        ),
    }
}

fn internal(error: impl std::fmt::Display) -> (u16, String) {
    (500, error.to_string())
}

/// One session's latency, as the audit is allowed to see it: for each span, how
/// many samples, their mean and the worst one. Spans nobody measured are absent
/// rather than zero, because "no sample" and "instant" are different facts.
fn talk_latency_detail(latency: &super::talk::TalkSessionLatency) -> serde_json::Value {
    let span = |value: &super::talk::TalkLatencySpan| {
        value.mean_ms().map(|mean| {
            serde_json::json!({
                "samples": value.samples,
                "meanMs": mean,
                "worstMs": value.worst_ms,
            })
        })
    };
    let mut detail = serde_json::Map::new();
    for (name, value) in [
        ("speechDetection", &latency.speech_detection),
        ("capture", &latency.capture),
        ("upload", &latency.upload),
        ("transcription", &latency.transcription),
        ("modelFirstToken", &latency.model_first_token),
        ("ttsFirstAudio", &latency.tts_first_audio),
        ("endToEnd", &latency.end_to_end),
    ] {
        if let Some(entry) = span(value) {
            detail.insert(name.to_string(), entry);
        }
    }
    serde_json::Value::Object(detail)
}

/// A Talk session's turns, running through exactly the surface the typed mobile
/// chat uses.
///
/// **Why the mobile message rows are written here too.** A spoken turn and a
/// typed one land in the same session; if only the typed ones left a row, the
/// operator would open the chat after a conversation and find half of it
/// missing. The user row is written before the turn is queued and the assistant
/// row when it settles — the same two writes, in the same order, that
/// `mobile_message_post` and `materialize_mobile_replies` make, so the two
/// surfaces converge on one transcript instead of two.
pub(crate) struct TalkSessionTurns {
    api: RemoteApi,
    device_id: String,
    /// See [`TalkSocketAuthorization::signed_request_sha256`].
    admission_sha256: String,
}

impl TalkSessionTurns {
    pub(crate) fn new(api: RemoteApi, authorization: &TalkSocketAuthorization) -> Self {
        Self {
            api,
            device_id: authorization.device_id.clone(),
            admission_sha256: authorization.signed_request_sha256.clone(),
        }
    }
}

impl super::talk::TalkTurns for TalkSessionTurns {
    fn submit(&self, session_id: &str, client_key: &str, text: &str) -> Result<String, String> {
        let queue = self
            .api
            .mobile_chat
            .as_ref()
            .ok_or_else(|| "This node build does not execute conversation turns".to_string())?;
        let now_ms = super::now_ms_public()?;
        {
            let mut store = self
                .api
                .store
                .lock()
                .map_err(|_| "Remote state lock was poisoned".to_string())?;
            store.insert_mobile_message(&MobileMessageRecord {
                message_id: client_key.to_string(),
                session_id: session_id.to_string(),
                device_id: self.device_id.clone(),
                role: "user".to_string(),
                text: text.to_string(),
                // A Talk turn is admitted by the ticket the socket was opened
                // with, whose own signed request digest is this. Naming it keeps
                // the row auditable in the same way a typed one is.
                request_sha256: self.admission_sha256.clone(),
                task_state: "queued".to_string(),
                created_at_ms: now_ms,
            })?;
        }
        // A row that claims `queued` for a turn nothing ever queued is a lie the
        // operator has no way to detect: the reply materializer skips it forever
        // because its job never existed. Settle it here, exactly as
        // `mobile_message_post` does, rather than leaving it to a sweep.
        match queue.queue_chat(session_id, client_key, text) {
            Ok(run_id) => Ok(run_id),
            Err(error) => {
                if let Ok(mut store) = self.api.store.lock() {
                    let _ = store.set_mobile_message_state(client_key, "failed", now_ms);
                }
                Err(error)
            }
        }
    }

    fn progress(
        &self,
        run_id: &str,
        from_index: u64,
    ) -> Result<super::talk::TalkRunProgress, String> {
        let ledger =
            RunLedger::open(&self.api.paths.ledger_db).map_err(|error| error.to_string())?;
        let events = match ledger.load_events(run_id, from_index, 500) {
            Ok(events) => events,
            // Not an error: the run row is written by the worker, and a turn
            // queued microseconds ago may not have one yet.
            Err(_) => {
                return Ok(super::talk::TalkRunProgress {
                    next_index: from_index,
                    ..super::talk::TalkRunProgress::default()
                })
            }
        };
        let mut progress = super::talk::TalkRunProgress {
            next_index: from_index.saturating_add(events.len() as u64),
            ..super::talk::TalkRunProgress::default()
        };
        for envelope in &events {
            match &envelope.event {
                RunEvent::ModelDelta { channel, text, .. } => {
                    if matches!(channel, OutputChannel::Assistant) {
                        progress.delta.push_str(text);
                    }
                }
                RunEvent::Completed { .. } => progress.finished = true,
                RunEvent::Failed { message, .. } => {
                    progress.finished = true;
                    progress.error = Some(message.clone());
                }
                RunEvent::Cancelled { .. } => {
                    progress.finished = true;
                    if progress.delta.trim().is_empty() {
                        progress.error = Some("This turn was cancelled.".to_string());
                    }
                }
                _ => {}
            }
        }
        Ok(progress)
    }

    fn cancel(&self, run_id: &str) -> Result<(), String> {
        // The same two steps the run centre and the phone's cancel button take:
        // ask the store to stop the job, and append the durable cancellation
        // event. What a tool already did in the world is not undone by either,
        // and nothing in Talk claims otherwise.
        let now_ms = super::now_ms_public()?;
        DaemonStore::open(&self.api.paths)
            .and_then(|mut store| store.request_cancel(run_id, now_ms))
            .map_err(|error| error.to_string())?;
        super::super::append_cancellation(&self.api.paths, run_id, "Interrupted by speech")
            .map_err(|error| error.to_string())
    }

    fn still_granted(&self, device_id: &str) -> bool {
        self.api.talk_capability_live(device_id)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::path::PathBuf;

    use super::super::protocol::DeviceReadiness;

    use little_monkey_lib::run_ledger::RunLedger;
    use little_monkey_lib::run_protocol::{
        ClientIdentity, ClientKind, ModelTargetSnapshot, PermissionMode as RunPermissionMode,
        PermissionPolicySnapshot, RootAccess, RootGrant, RunBudgets, RunKind, RunSpec,
        ToolPolicyDecision, WorkspaceContext, RUN_PROTOCOL_SCHEMA_VERSION,
    };

    use super::*;
    use crate::daemon::remote::protocol::{
        sign_request, DeviceConstraints, OsPermission, RemoteAction,
    };
    use crate::daemon::remote::store::{DeviceCommandRequest, RemoteSecretStore};
    use crate::daemon::store::DaemonConfig;
    use crate::durable_run::DurableRunRecorder;
    use little_monkey_lib::contract;

    /// **The published contract's remote-plane table is checked against this
    /// file's own dispatch match, not against a memory of it.**
    ///
    /// `little_monkey_lib::contract::REMOTE_ROUTES` is what third parties read
    /// (K19). It cannot live beside the match — the contract is generated in
    /// the library and this is a binary crate — so the risk is the ordinary
    /// one for any second copy: a route added here, never published, and a
    /// package that gates on the contract version therefore gating on a lie.
    ///
    /// This scans the match arms themselves: method, path shape *and* the
    /// exact `RemoteAction`/`DeviceCapability` variant each arm requires. A
    /// new route, a moved segment or a re-graded gate fails here rather than
    /// in a reviewer's memory. The technique is `egress.rs`'s bare-client
    /// ratchet and `server.rs`'s admission scan, for the same reason: the
    /// defect class is "a call site that looks fine in isolation".
    #[test]
    fn every_dispatched_remote_route_is_in_the_published_contract() {
        const SOURCE: &str = include_str!("api.rs");
        let production = SOURCE
            .split_once("\n#[cfg(test)]")
            .map_or(SOURCE, |(before, _)| before);

        // The one route dispatched before the match, because it runs before a
        // device (and therefore a signature) exists.
        assert!(
            production.contains(r#"request.path_and_query == "/v1/remote/pairings/accept""#),
            "the unauthenticated pairing route moved; the contract still names it"
        );

        let lines: Vec<&str> = production.lines().collect();
        let mut dispatched: BTreeSet<(String, String, String)> = BTreeSet::new();
        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            let Some(rest) = trimmed.strip_prefix('(') else {
                continue;
            };
            let Some((method, rest)) = rest.split_once(", [") else {
                continue;
            };
            let Some(method) = method.strip_prefix('"').and_then(|m| m.strip_suffix('"')) else {
                continue;
            };
            let Some((segments, _)) = rest.split_once("]) =>") else {
                continue;
            };
            let path = segments
                .split(',')
                .map(str::trim)
                .filter(|segment| !segment.is_empty())
                .map(
                    |segment| match segment.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                        Some(literal) => format!("/{literal}"),
                        None => format!("/{{{segment}}}"),
                    },
                )
                .collect::<String>();
            // The gate is whatever the arm's first grant check names. Three
            // lines is enough for every arm rustfmt produces here; an arm that
            // grew past it would fail as `self_service` and be noticed.
            let arm = lines[index..(index + 3).min(lines.len())].join(" ");
            let gate = if arm.contains("require_any_peer_capability") {
                // The peer plane's gate is "any of the three peer grants",
                // which no single `DeviceCapability::` token can name.
                "peer_standing".to_string()
            } else {
                arm.split_once("RemoteAction::")
                    .map(|(_, tail)| format!("action:{}", variant(tail)))
                    .or_else(|| {
                        arm.split_once("DeviceCapability::")
                            .map(|(_, tail)| format!("capability:{}", variant(tail)))
                    })
                    .unwrap_or_else(|| "self_service".to_string())
            };
            dispatched.insert((method.to_string(), path, gate));
        }

        let published: BTreeSet<(String, String, String)> = contract::REMOTE_ROUTES
            .iter()
            .filter(|route| route.gate != contract::RemoteGate::Unauthenticated)
            .map(|route| {
                (
                    route.method.to_string(),
                    route.path.to_string(),
                    match route.gate {
                        contract::RemoteGate::Action(action) => format!("action:{action}"),
                        contract::RemoteGate::Capability(capability) => {
                            format!("capability:{capability}")
                        }
                        contract::RemoteGate::SelfService => "self_service".to_string(),
                        contract::RemoteGate::PeerStanding => "peer_standing".to_string(),
                        contract::RemoteGate::Unauthenticated => unreachable!("filtered above"),
                    },
                )
            })
            .collect();

        assert_eq!(
            dispatched, published,
            "the dispatch match and the published K19 contract disagree; \
             update contract::REMOTE_ROUTES and republish (docs/contract-abi.md)"
        );
    }

    /// The leading identifier of `Variant => ...`, `Variant)` or similar.
    fn variant(tail: &str) -> String {
        tail.chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect()
    }

    /// The wire version the contract publishes is the one this plane rejects
    /// mismatches against. Two constants, one fact.
    #[test]
    fn the_published_remote_protocol_version_is_the_one_enforced() {
        assert_eq!(contract::REMOTE_PROTOCOL_VERSION, REMOTE_PROTOCOL_VERSION);
    }

    #[derive(Default)]
    struct FakeSecrets(Mutex<HashMap<String, Vec<u8>>>);
    impl RemoteSecretStore for FakeSecrets {
        fn get(&self, slot: &str) -> Result<Vec<u8>, String> {
            self.0
                .lock()
                .unwrap()
                .get(slot)
                .cloned()
                .ok_or_else(|| "missing secret".to_string())
        }
        fn set(&self, slot: &str, secret: &[u8]) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .insert(slot.to_string(), secret.to_vec());
            Ok(())
        }
        fn delete(&self, slot: &str) -> Result<(), String> {
            self.0.lock().unwrap().remove(slot);
            Ok(())
        }
    }

    fn spec(run_id: &str, workspace: &str) -> RunSpec {
        RunSpec {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            run_id: run_id.to_string(),
            idempotency_key: format!("idem-{run_id}"),
            created_at_ms: 1_000,
            kind: RunKind::Background,
            submitted_by: ClientIdentity {
                client_id: "fixture".into(),
                instance_id: "fixture".into(),
                kind: ClientKind::Daemon,
                version: "1".into(),
            },
            task: "fixture".into(),
            instructions: None,
            input_artifact_ids: vec![],
            target: ModelTargetSnapshot::Provider {
                target_id: "fixture".into(),
                label: "fixture".into(),
                provider_id: "fixture".into(),
                endpoint: "https://example.invalid/v1".into(),
                model: "fixture".into(),
                credential_ref_id: "credential-none".into(),
                capabilities: crate::task::cli_capabilities(),
            },
            workspace: Some(WorkspaceContext {
                workspace_id: workspace.into(),
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
                mode: RunPermissionMode::Auto,
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
                max_iterations: 2,
                max_model_calls: 2,
                max_tool_calls: 2,
                max_input_tokens: 10_000,
                max_output_tokens: 10_000,
                max_cost_micros: None,
                max_artifact_bytes: 1_024,
                max_event_count: 1_000,
            },
        }
    }

    /// `handle` wraps every path, so an unauthenticated request — the one that
    /// never reaches a route at all — is still an event. That is the property
    /// the wrapper buys over instrumenting routes: there is no branch to miss.
    #[test]
    fn an_unauthenticated_remote_request_is_still_recorded_as_denied() {
        let (root, api, _secrets, _device, _key) = fixture();

        let response = api.handle(
            ApiRequest {
                method: "GET".to_string(),
                path_and_query: "/v1/remote/runs?limit=5".to_string(),
                body: Vec::new(),
                auth: None,
            },
            2_000,
        );
        assert_eq!(response.status, 401, "no signed auth means refused");

        let ledger =
            little_monkey_lib::run_ledger::RunLedger::open(DaemonPaths::under(&root).ledger_db)
                .unwrap();
        let events = ledger.recent_subsystem_events(None, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].subsystem,
            little_monkey_lib::run_ledger::Subsystem::Remote
        );
        assert_eq!(
            events[0].action, "GET /v1/remote/runs",
            "the query string is dropped: it carries ids and the row is permanent"
        );
        assert_eq!(
            events[0].outcome,
            little_monkey_lib::run_ledger::SubsystemOutcome::Denied,
            "a refusal is not a failure — a reader counting failures must not count it"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    fn fixture() -> (PathBuf, RemoteApi, Arc<FakeSecrets>, String, Vec<u8>) {
        fixture_with(BTreeSet::from([
            RemoteAction::ViewRuns,
            RemoteAction::ViewEvents,
            RemoteAction::Approve,
            RemoteAction::Cancel,
        ]))
    }

    /// The same fixture with an explicit grant, so a test can prove an action
    /// is refused without it as well as honoured with it.
    fn fixture_with(
        actions: BTreeSet<RemoteAction>,
    ) -> (PathBuf, RemoteApi, Arc<FakeSecrets>, String, Vec<u8>) {
        fixture_scoped(actions, BTreeSet::from(["run-one".to_string()]))
    }

    /// The same fixture with an explicit run scope, so a migration test can pair
    /// a device for a run this node does not have yet — which is the only shape
    /// a placement ever has.
    fn fixture_scoped(
        actions: BTreeSet<RemoteAction>,
        run_ids: BTreeSet<String>,
    ) -> (PathBuf, RemoteApi, Arc<FakeSecrets>, String, Vec<u8>) {
        let root =
            std::env::temp_dir().join(format!("little-monkey-remote-api-{}", uuid::Uuid::new_v4()));
        let paths = DaemonPaths::under(&root);
        paths.ensure().unwrap();
        DaemonConfig::default().save(&paths).unwrap();
        let ledger = RunLedger::open(&paths.ledger_db).unwrap();
        let (recorder, _) =
            DurableRunRecorder::submit(ledger, &spec("run-one", "workspace-one"), "fixture".into())
                .unwrap();
        recorder
            .emit(RunEvent::Queued {
                queue: Some("fixture".into()),
            })
            .unwrap();
        recorder
            .emit(RunEvent::Started {
                engine_id: "fixture".into(),
            })
            .unwrap();
        let fixture_expiry = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 60_000;
        recorder
            .emit(RunEvent::PermissionRequested {
                request_id: "approval-one".into(),
                tool_call_id: "tool-one".into(),
                tool_name: "write_file".into(),
                operation_sha256: "a".repeat(64),
                expires_at_ms: fixture_expiry,
                detail: "fixture approval".into(),
                risk_level: None,
                risk_reason: None,
            })
            .unwrap();
        recorder
            .emit(RunEvent::AwaitingApproval {
                request_id: "approval-one".into(),
                operation_sha256: "a".repeat(64),
                expires_at_ms: fixture_expiry,
                reason: Some("fixture".into()),
            })
            .unwrap();
        let mut daemon_store = DaemonStore::open(&paths).unwrap();
        let snapshot = paths.snapshots.join("job-run-one.json");
        std::fs::write(&snapshot, b"{}").unwrap();
        daemon_store
            .insert_preparing(
                &crate::daemon::store::NewDaemonJob {
                    job_id: "job-run-one".into(),
                    recipe_snapshot: snapshot,
                    priority: 0,
                    max_attempts: 1,
                    created_at_ms: 1_000,
                    max_runtime_ms: 60_000,
                    max_memory_bytes: None,
                    max_log_bytes: 1_024 * 1_024,
                    repository_policy_json: None,
                    worktree_json: None,
                    parent_run_id: None,
                },
                8,
            )
            .unwrap();
        daemon_store
            .mark_queued("job-run-one", "run-one", 1_000)
            .unwrap();
        let host = RemoteHostConfig {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            runner_id: "runner-one".into(),
            listen: "127.0.0.1:1".into(),
            advertise_url: "https://runner.invalid".into(),
            certificate_path: "/tmp/cert".into(),
            private_key_path: "/tmp/key".into(),
            certificate_sha256: "a".repeat(64),
            enabled: true,
        };
        let mut store = RemoteStore::open(&paths.root).unwrap();
        let scopes = RemoteScopes {
            actions,
            run_ids,
            workspace_ids: BTreeSet::new(),
            max_artifact_bytes: 1_024,
        };
        let secrets = Arc::new(FakeSecrets::default());
        let invite = store.create_invitation(&scopes, 1_000, 3_000).unwrap();
        let accepted = store
            .accept_invitation(
                &invite.pairing_id,
                &invite.token,
                "phone",
                "runner-one",
                1_100,
                secrets.as_ref(),
            )
            .unwrap();
        let secret = accepted.device_secret.as_bytes().to_vec();
        let api = RemoteApi::injected(paths, host, store, secrets.clone());
        (root, api, secrets, accepted.device_id, secret)
    }

    fn signed(
        device_id: &str,
        secret: &[u8],
        sequence: u64,
        command: &str,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> ApiRequest {
        signed_at(
            device_id, secret, sequence, command, method, path, body, 2_000,
        )
    }

    /// The same signed request against a caller-chosen clock.
    ///
    /// Every other test drives the API at a fixed `now_ms`, which is what makes
    /// them deterministic. A Talk ticket cannot: it is minted through the API
    /// and redeemed by the socket layer, which reads the real clock — so a
    /// ticket issued in 1970 is expired before the handshake starts.
    #[allow(clippy::too_many_arguments)]
    fn signed_at(
        device_id: &str,
        secret: &[u8],
        sequence: u64,
        command: &str,
        method: &str,
        path: &str,
        body: &[u8],
        timestamp_ms: u64,
    ) -> ApiRequest {
        let mut auth = SignedRequestHeaders {
            device_id: device_id.into(),
            secret_generation: 1,
            sequence,
            timestamp_ms,
            nonce: format!("nonce-{command}-0123456789"),
            command_id: command.into(),
            signature: String::new(),
        };
        auth.signature = sign_request(secret, &auth, method, path, body);
        ApiRequest {
            method: method.into(),
            path_and_query: path.into(),
            body: body.to_vec(),
            auth: Some(auth),
        }
    }

    #[test]
    fn out_of_scope_run_is_indistinguishable_from_missing() {
        let (root, api, _secrets, device, secret) = fixture();
        let response = api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-hidden",
                "GET",
                "/v1/remote/runs/run-hidden",
                b"",
            ),
            2_000,
        );
        assert_eq!(response.status, 404);
        assert!(!String::from_utf8_lossy(&response.body).contains("scope"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn pausing_requires_its_own_grant_and_is_not_implied_by_cancel() {
        // The weaker action is not free. A controller trusted to destroy a run
        // is a different decision from one trusted to suspend it, and neither
        // implies the other — otherwise adding this action would silently widen
        // every pairing that already had `cancel`.
        let (root, api, _secrets, device, secret) = fixture();
        let response = api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-pause-denied",
                "POST",
                "/v1/remote/runs/run-one/pause",
                b"{}",
            ),
            2_000,
        );
        assert_eq!(response.status, 403);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn pause_and_resume_move_the_daemons_own_bit() {
        // The gap this closes: the daemon has supported pause since it had a
        // `pause_requested` bit, but no remote action reached it, so a paired
        // controller could only stop a run by destroying it.
        let (root, api, _secrets, device, secret) = fixture_with(BTreeSet::from([
            RemoteAction::ViewRuns,
            RemoteAction::Pause,
        ]));
        let paths = DaemonPaths::under(&root);

        let paused = api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-pause",
                "POST",
                "/v1/remote/runs/run-one/pause",
                b"{}",
            ),
            2_000,
        );
        assert_eq!(paused.status, 202);
        assert!(
            DaemonStore::open(&paths)
                .unwrap()
                .get_job("run-one")
                .unwrap()
                .expect("job exists")
                .pause_requested,
            "a remote pause must reach the daemon's own bit, not just return 202"
        );

        let resumed = api.handle(
            signed(
                &device,
                &secret,
                2,
                "cmd-resume",
                "POST",
                "/v1/remote/runs/run-one/resume",
                b"{}",
            ),
            2_001,
        );
        assert_eq!(resumed.status, 202);
        assert!(
            !DaemonStore::open(&paths)
                .unwrap()
                .get_job("run-one")
                .unwrap()
                .expect("job exists")
                .pause_requested
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn lost_response_replay_returns_cached_result_without_second_cancel_event() {
        let (root, api, _secrets, device, secret) = fixture();
        let body = br#"{"reason":"phone stop"}"#;
        let request = signed(
            &device,
            &secret,
            1,
            "cmd-cancel",
            "POST",
            "/v1/remote/runs/run-one/cancel",
            body,
        );
        let first = api.handle(request.clone(), 2_000);
        let replay = api.handle(request, 2_001);
        assert_eq!(first.status, 202);
        assert_eq!(first, replay);
        let events = RunLedger::open(&DaemonPaths::under(&root).ledger_db)
            .unwrap()
            .load_events("run-one", 0, 100)
            .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, RunEvent::CancellationRequested { .. }))
                .count(),
            1
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reconnect_reconciles_a_reserved_command_once_after_server_crash() {
        let (root, api, _secrets, device, secret) = fixture();
        let body = br#"{"reason":"reconnect stop"}"#;
        let request = signed(
            &device,
            &secret,
            1,
            "cmd-crash",
            "POST",
            "/v1/remote/runs/run-one/cancel",
            body,
        );
        let auth = request.auth.as_ref().unwrap();
        let request_sha = sha256_hex(&canonical_request(
            auth,
            &request.method,
            &request.path_and_query,
            &request.body,
        ));
        // Simulate a runner crash immediately after reserving the monotonic
        // command but before dispatching the cancellation.
        assert_eq!(
            api.store
                .lock()
                .unwrap()
                .reserve_command(
                    &device,
                    1,
                    &auth.command_id,
                    &auth.nonce,
                    auth.sequence,
                    &request_sha,
                    &request.method,
                    &request.path_and_query,
                    2_000,
                )
                .unwrap(),
            CommandReservation::New
        );
        let recovered = api.handle(request.clone(), 2_001);
        let replay = api.handle(request, 2_002);
        assert_eq!(recovered.status, 202);
        assert_eq!(recovered, replay);
        let events = RunLedger::open(&DaemonPaths::under(&root).ledger_db)
            .unwrap()
            .load_events("run-one", 0, 100)
            .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, RunEvent::CancellationRequested { .. }))
                .count(),
            1
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_signature_cannot_consume_sequence_or_cancel() {
        let (root, api, _secrets, device, secret) = fixture();
        let mut request = signed(
            &device,
            &secret,
            1,
            "cmd-forged",
            "POST",
            "/v1/remote/runs/run-one/cancel",
            br#"{"reason":null}"#,
        );
        request.body = br#"{"reason":"tampered"}"#.to_vec();
        assert_eq!(api.handle(request, 2_000).status, 401);
        let valid = signed(
            &device,
            &secret,
            1,
            "cmd-valid",
            "GET",
            "/v1/remote/runs/run-one",
            b"",
        );
        assert_eq!(api.handle(valid, 2_001).status, 200);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn approval_requires_the_exact_pending_operation_digest_and_is_idempotent() {
        let (root, api, _secrets, device, secret) = fixture();
        let wrong = br#"{"request_id":"approval-one","operation_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","decision":"allow_once"}"#;
        let wrong_response = api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-wrong-digest",
                "POST",
                "/v1/remote/runs/run-one/approve",
                wrong,
            ),
            2_000,
        );
        assert_eq!(wrong_response.status, 403);
        let valid_body = br#"{"request_id":"approval-one","operation_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","decision":"allow_once"}"#;
        let valid = signed(
            &device,
            &secret,
            2,
            "cmd-approve",
            "POST",
            "/v1/remote/runs/run-one/approve",
            valid_body,
        );
        let first = api.handle(valid.clone(), 2_001);
        let replay = api.handle(valid, 2_002);
        assert_eq!(first.status, 200, "body: {:?}", first.body);
        assert_eq!(first, replay);
        let approval = RunLedger::open(&DaemonPaths::under(&root).ledger_db)
            .unwrap()
            .load_approval("run-one", "approval-one")
            .unwrap()
            .unwrap();
        assert_eq!(approval.decision, Some(PermissionDecision::AllowOnce));
        let _ = std::fs::remove_dir_all(root);
    }

    // --- `/v1/remote/mobile/*` extension ----------------------------------

    #[derive(Default)]
    struct FakeChatQueue {
        queued: Mutex<Vec<(String, String)>>,
    }

    impl MobileChatQueue for FakeChatQueue {
        fn queue_chat(
            &self,
            _session_id: &str,
            client_key: &str,
            prompt: &str,
        ) -> Result<String, String> {
            self.queued
                .lock()
                .unwrap()
                .push((client_key.to_string(), prompt.to_string()));
            Ok(format!("run-{client_key}"))
        }

        fn chat_run_id(
            &self,
            _session_id: &str,
            client_key: &str,
        ) -> Result<Option<String>, String> {
            Ok(self
                .queued
                .lock()
                .unwrap()
                .iter()
                .find(|(key, _)| key == client_key)
                .map(|(key, _)| format!("run-{key}")))
        }
    }

    /// The whole point of the separate capability grant: a device paired
    /// before mobile capabilities existed (or paired deliberately as a
    /// runner-only controller) resolves through `legacy_capabilities`, which
    /// never contains Chat — so a newer phone build cannot talk itself into
    /// a chat surface the operator never granted.
    #[test]
    fn legacy_pairing_cannot_reach_mobile_chat_or_workflow_launch() {
        let (root, api, _secrets, device, secret) = fixture();
        let api = api.with_mobile_chat(Arc::new(FakeChatQueue::default()));
        for (index, (method, path, body)) in [
            ("GET", "/v1/remote/mobile/sessions", &b""[..]),
            (
                "POST",
                "/v1/remote/mobile/sessions/s1/messages",
                br#"{"text":"hi"}"#,
            ),
            ("GET", "/v1/remote/mobile/workflows", b""),
            ("POST", "/v1/remote/mobile/workflows/wf/runs", b"{}"),
            (
                "POST",
                "/v1/remote/mobile/captures",
                br#"{"capture_id":"c1","kind":"text","title":"t","text":"x"}"#,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let response = api.handle(
                signed(
                    &device,
                    &secret,
                    index as u64 + 1,
                    &format!("cmd-legacy-{index}"),
                    method,
                    path,
                    body,
                ),
                2_000 + index as u64,
            );
            assert_eq!(
                response.status, 403,
                "{method} {path} should be capability-denied"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn granted_device_queues_one_chat_turn_and_reads_it_back() {
        let (root, api, secrets, _legacy_device, _legacy_secret) = fixture();
        let queue = Arc::new(FakeChatQueue::default());
        let api = api.with_mobile_chat(queue.clone());
        // Pair a second device that DOES carry the mobile grants.
        let (device, secret) = {
            let mut store = RemoteStore::open(&DaemonPaths::under(&root).root).unwrap();
            let scopes = RemoteScopes {
                actions: BTreeSet::from([RemoteAction::ViewRuns]),
                run_ids: BTreeSet::from(["run-one".into()]),
                workspace_ids: BTreeSet::new(),
                max_artifact_bytes: 1_024,
            };
            let capabilities = BTreeSet::from([
                DeviceCapability::ViewRuns,
                DeviceCapability::ViewSessions,
                DeviceCapability::Chat,
            ]);
            let invite = store
                .create_invitation_with_capabilities(&scopes, &capabilities, 1_000, 3_000)
                .unwrap();
            let accepted = store
                .accept_invitation(
                    &invite.pairing_id,
                    &invite.token,
                    "granted-phone",
                    "runner-one",
                    1_100,
                    secrets.as_ref(),
                )
                .unwrap();
            (
                accepted.device_id,
                accepted.device_secret.as_bytes().to_vec(),
            )
        };

        let post = api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-chat-post",
                "POST",
                "/v1/remote/mobile/sessions/s1/messages",
                br#"{"text":"what is queued?"}"#,
            ),
            2_000,
        );
        assert_eq!(
            post.status,
            201,
            "body: {:?}",
            String::from_utf8_lossy(&post.body)
        );
        assert_eq!(queue.queued.lock().unwrap().len(), 1);
        assert_eq!(queue.queued.lock().unwrap()[0].1, "what is queued?");

        let get = api.handle(
            signed(
                &device,
                &secret,
                2,
                "cmd-chat-get",
                "GET",
                "/v1/remote/mobile/sessions/s1/messages",
                b"",
            ),
            2_001,
        );
        assert_eq!(get.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&get.body).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(
            messages.len(),
            1,
            "only the user turn exists until the run is terminal"
        );
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["text"], "what is queued?");
        assert_eq!(messages[0]["task_state"], "queued");

        // The session list is derived from the same rows.
        let sessions = api.handle(
            signed(
                &device,
                &secret,
                3,
                "cmd-sessions",
                "GET",
                "/v1/remote/mobile/sessions",
                b"",
            ),
            2_002,
        );
        assert_eq!(sessions.status, 200);
        let listed: serde_json::Value = serde_json::from_slice(&sessions.body).unwrap();
        assert_eq!(listed["sessions"][0]["id"], "s1");
        assert_eq!(listed["sessions"][0]["title"], "what is queued?");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A capture whose declared digest does not match the uploaded bytes is
    /// rejected outright — the node never stores content it cannot vouch for.
    #[test]
    fn capture_rejects_a_digest_that_does_not_match_its_bytes() {
        let (root, api, secrets, _d, _s) = fixture();
        let (device, secret) = {
            let mut store = RemoteStore::open(&DaemonPaths::under(&root).root).unwrap();
            let scopes = RemoteScopes {
                actions: BTreeSet::from([RemoteAction::ViewRuns]),
                run_ids: BTreeSet::from(["run-one".into()]),
                workspace_ids: BTreeSet::new(),
                max_artifact_bytes: 1_024,
            };
            let capabilities =
                BTreeSet::from([DeviceCapability::ViewRuns, DeviceCapability::Capture]);
            let invite = store
                .create_invitation_with_capabilities(&scopes, &capabilities, 1_000, 3_000)
                .unwrap();
            let accepted = store
                .accept_invitation(
                    &invite.pairing_id,
                    &invite.token,
                    "capture-phone",
                    "runner-one",
                    1_100,
                    secrets.as_ref(),
                )
                .unwrap();
            (
                accepted.device_id,
                accepted.device_secret.as_bytes().to_vec(),
            )
        };
        let payload = STANDARD.encode(b"hello");
        let body = format!(
            r#"{{"capture_id":"c1","kind":"file","title":"note","content_base64":"{payload}","content_sha256":"{}"}}"#,
            "b".repeat(64)
        );
        let response = api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-capture-bad",
                "POST",
                "/v1/remote/mobile/captures",
                body.as_bytes(),
            ),
            2_000,
        );
        assert_eq!(response.status, 400);
        assert!(String::from_utf8_lossy(&response.body).contains("content_sha256"));
        let _ = std::fs::remove_dir_all(root);
    }
    // --- `/v1/remote/node/*` placement plane (roadmap K17) -----------------

    #[derive(Default)]
    struct FakePlacementQueue {
        placed: Mutex<Vec<String>>,
        refuse: Option<String>,
    }

    impl FakePlacementQueue {
        fn refusing(reason: &str) -> Self {
            Self {
                placed: Mutex::new(Vec::new()),
                refuse: Some(reason.to_string()),
            }
        }
    }

    impl PlacementQueue for FakePlacementQueue {
        fn place(
            &self,
            spec: &little_monkey_lib::run_protocol::RunSpec,
        ) -> Result<PlacedJob, String> {
            if let Some(reason) = &self.refuse {
                return Err(reason.clone());
            }
            self.placed.lock().unwrap().push(spec.run_id.clone());
            Ok(PlacedJob {
                // The node mints its own ids — deliberately different from the
                // submitter's, which is the property the response's two id
                // fields exist to keep visible.
                node_run_id: format!("node-{}", spec.run_id),
                job_id: format!("job-{}", spec.run_id),
                state: "queued".to_string(),
            })
        }

        fn placed_state(&self, job_id: &str) -> Result<Option<PlacedJobState>, String> {
            Ok(Some(PlacedJobState {
                state: "running".to_string(),
                terminal: false,
                updated_at_ms: 3_000,
                last_error: Some(format!("state of {job_id}")),
            }))
        }
    }

    /// A pairing that carries the two K17 grants, so the placement plane is
    /// reachable at all.
    fn placement_fixture() -> (PathBuf, RemoteApi, String, Vec<u8>) {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-remote-place-{}",
            uuid::Uuid::new_v4()
        ));
        let paths = DaemonPaths::under(&root);
        paths.ensure().unwrap();
        DaemonConfig::default().save(&paths).unwrap();
        let host = RemoteHostConfig {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            runner_id: "runner-one".into(),
            listen: "127.0.0.1:1".into(),
            advertise_url: "https://runner.invalid".into(),
            certificate_path: "/tmp/cert".into(),
            private_key_path: "/tmp/key".into(),
            certificate_sha256: "a".repeat(64),
            enabled: true,
        };
        let mut store = RemoteStore::open(&paths.root).unwrap();
        let scopes = RemoteScopes {
            actions: BTreeSet::from([RemoteAction::ViewRuns]),
            run_ids: BTreeSet::from(["run-one".into()]),
            workspace_ids: BTreeSet::new(),
            max_artifact_bytes: 1_024,
        };
        let capabilities = BTreeSet::from([
            DeviceCapability::ViewRuns,
            DeviceCapability::DescribeNode,
            DeviceCapability::PlaceRuns,
        ]);
        let secrets = Arc::new(FakeSecrets::default());
        let invite = store
            .create_invitation_with_capabilities(&scopes, &capabilities, 1_000, 3_000)
            .unwrap();
        let accepted = store
            .accept_invitation_with_capabilities(
                &invite.pairing_id,
                &invite.token,
                "scheduler",
                "runner-one",
                None,
                1_100,
                secrets.as_ref(),
            )
            .unwrap();
        let secret = accepted.device_secret.as_bytes().to_vec();
        let api = RemoteApi::injected(paths, host, store, secrets);
        (root, api, accepted.device_id, secret)
    }

    /// Two installations, in one process: this node, and a peer paired into it
    /// with the grants named. No second machine, no network — the signature
    /// path and the gate are the same ones a real pairing uses.
    fn peer_fixture(
        capabilities: BTreeSet<DeviceCapability>,
    ) -> (PathBuf, RemoteApi, String, Vec<u8>) {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-remote-peer-{}",
            uuid::Uuid::new_v4()
        ));
        let paths = DaemonPaths::under(&root);
        paths.ensure().unwrap();
        DaemonConfig::default().save(&paths).unwrap();
        let host = RemoteHostConfig {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            runner_id: "runner-local".into(),
            listen: "127.0.0.1:1".into(),
            advertise_url: "https://runner.invalid".into(),
            certificate_path: "/tmp/cert".into(),
            private_key_path: "/tmp/key".into(),
            certificate_sha256: "a".repeat(64),
            enabled: true,
        };
        let mut store = RemoteStore::open(&paths.root).unwrap();
        let scopes = RemoteScopes {
            actions: BTreeSet::new(),
            run_ids: BTreeSet::new(),
            workspace_ids: BTreeSet::new(),
            max_artifact_bytes: 1_024,
        };
        let secrets = Arc::new(FakeSecrets::default());
        let invite = store
            .create_invitation_with_capabilities(&scopes, &capabilities, 1_000, 3_000)
            .unwrap();
        let accepted = store
            .accept_invitation_with_capabilities(
                &invite.pairing_id,
                &invite.token,
                "peer-two",
                "runner-local",
                None,
                1_100,
                secrets.as_ref(),
            )
            .unwrap();
        let secret = accepted.device_secret.as_bytes().to_vec();
        let api = RemoteApi::injected(paths, host, store, secrets);
        (root, api, accepted.device_id, secret)
    }

    fn every_peer_grant() -> BTreeSet<DeviceCapability> {
        BTreeSet::from([
            DeviceCapability::PeerMessage,
            DeviceCapability::PeerTaskRequest,
            DeviceCapability::PeerArtifact,
        ])
    }

    fn peer_body(message_id: &str, kind: little_monkey_lib::peers::PeerMessageKind) -> Vec<u8> {
        let mut envelope = little_monkey_lib::peers::PeerEnvelope::new(
            message_id,
            "thread-1",
            kind,
            "instance-peer-two",
            "summarize the failing nightly build",
            2_000,
            600_000,
        );
        envelope.correlation_id = Some("corr-1".into());
        serde_json::to_vec(&envelope).unwrap()
    }

    #[derive(Default)]
    struct FakePeerRuns {
        submitted: std::sync::Mutex<Vec<little_monkey_lib::channels::ingress::ConversationIngress>>,
    }

    impl crate::daemon::channel_worker::RunQueue for FakePeerRuns {
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
            ingress: &little_monkey_lib::channels::ingress::ConversationIngress,
            _params: Vec<String>,
        ) -> Result<String, String> {
            self.submitted.lock().unwrap().push(ingress.clone());
            Ok(ingress.deterministic_job_id())
        }
    }

    /// Peer standing is its own thing. A pairing without any of the three peer
    /// grants cannot reach the peer plane at all — which is what keeps every
    /// pairing that existed before this build from becoming a peer.
    #[test]
    fn a_pairing_without_peer_grants_cannot_reach_the_peer_plane() {
        // An ordinary controller pairing — the shape every pairing that
        // existed before peers shipped still has.
        let (root, api, _secrets, device, secret) = fixture();
        let api = api.with_peer_runs(Arc::new(FakePeerRuns::default()));
        for (index, (method, path, body)) in [
            (
                "POST",
                "/v1/remote/peer/messages",
                peer_body("msg-1", little_monkey_lib::peers::PeerMessageKind::Message),
            ),
            ("GET", "/v1/remote/peer/threads/thread-1", Vec::new()),
        ]
        .into_iter()
        .enumerate()
        {
            let response = api.handle(
                signed(
                    &device,
                    &secret,
                    index as u64 + 1,
                    &format!("cmd-peer-{index}"),
                    method,
                    path,
                    &body,
                ),
                2_000,
            );
            assert_eq!(
                response.status, 403,
                "{method} {path} must need a peer grant"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_peer_task_request_becomes_a_turn_and_the_thread_shows_it_still_running() {
        let (root, api, device, secret) = peer_fixture(every_peer_grant());
        let runs = Arc::new(FakePeerRuns::default());
        let api = api.with_peer_runs(runs.clone());

        let accepted = api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-peer-send",
                "POST",
                "/v1/remote/peer/messages",
                &peer_body(
                    "msg-1",
                    little_monkey_lib::peers::PeerMessageKind::TaskRequest,
                ),
            ),
            2_000,
        );
        assert_eq!(accepted.status, 202);
        let body: serde_json::Value = serde_json::from_slice(&accepted.body).unwrap();
        assert_eq!(body["accepted"], true);
        assert_eq!(body["thread_id"], "thread-1");
        assert_eq!(body["correlation_id"], "corr-1");

        // It reached the ordinary durable path, as a peer turn.
        let submitted = runs.submitted.lock().unwrap().clone();
        assert_eq!(submitted.len(), 1);
        assert_eq!(
            submitted[0].source,
            little_monkey_lib::channels::ingress::ConversationSource::Peer
        );

        // Nothing has finished, so the thread carries the request and no result.
        let thread = api.handle(
            signed(
                &device,
                &secret,
                2,
                "cmd-peer-poll",
                "GET",
                "/v1/remote/peer/threads/thread-1",
                b"",
            ),
            2_100,
        );
        assert_eq!(thread.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&thread.body).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["direction"], "inbound");
        assert_eq!(messages[0]["disposition"], "accepted");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_retried_delivery_is_answered_with_the_first_decision_and_runs_once() {
        let (root, api, device, secret) = peer_fixture(every_peer_grant());
        let runs = Arc::new(FakePeerRuns::default());
        let api = api.with_peer_runs(runs.clone());
        let body = peer_body(
            "msg-1",
            little_monkey_lib::peers::PeerMessageKind::TaskRequest,
        );

        let first = api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-peer-a",
                "POST",
                "/v1/remote/peer/messages",
                &body,
            ),
            2_000,
        );
        let second = api.handle(
            signed(
                &device,
                &secret,
                2,
                "cmd-peer-b",
                "POST",
                "/v1/remote/peer/messages",
                &body,
            ),
            2_050,
        );

        assert_eq!(first.status, 202);
        assert_eq!(second.status, 200);
        let repeated: serde_json::Value = serde_json::from_slice(&second.body).unwrap();
        assert_eq!(repeated["state"], "duplicate");
        assert_eq!(repeated["accepted"], true);
        assert_eq!(runs.submitted.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_peer_granted_only_conversation_cannot_ask_for_work() {
        let (root, api, device, secret) =
            peer_fixture(BTreeSet::from([DeviceCapability::PeerMessage]));
        let runs = Arc::new(FakePeerRuns::default());
        let api = api.with_peer_runs(runs.clone());

        let refused = api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-peer-task",
                "POST",
                "/v1/remote/peer/messages",
                &peer_body(
                    "msg-1",
                    little_monkey_lib::peers::PeerMessageKind::TaskRequest,
                ),
            ),
            2_000,
        );
        assert_eq!(refused.status, 403);
        assert!(runs.submitted.lock().unwrap().is_empty());

        // The same peer may still talk.
        let allowed = api.handle(
            signed(
                &device,
                &secret,
                2,
                "cmd-peer-msg",
                "POST",
                "/v1/remote/peer/messages",
                &peer_body("msg-2", little_monkey_lib::peers::PeerMessageKind::Message),
            ),
            2_010,
        );
        assert_eq!(allowed.status, 202);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_peer_cannot_read_another_peers_thread() {
        let (root, api, device, secret) = peer_fixture(every_peer_grant());
        let api = api.with_peer_runs(Arc::new(FakePeerRuns::default()));
        api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-peer-seed",
                "POST",
                "/v1/remote/peer/messages",
                &peer_body("msg-1", little_monkey_lib::peers::PeerMessageKind::Message),
            ),
            2_000,
        );

        // A second peer, paired into the same node, asks for the first one's
        // thread by name. It gets the same answer a thread that does not exist
        // gets, so probing cannot enumerate anyone.
        let mut store = RemoteStore::open(&api.paths.root).unwrap();
        let scopes = RemoteScopes {
            actions: BTreeSet::new(),
            run_ids: BTreeSet::new(),
            workspace_ids: BTreeSet::new(),
            max_artifact_bytes: 1_024,
        };
        let secrets = Arc::new(FakeSecrets::default());
        let invite = store
            .create_invitation_with_capabilities(&scopes, &every_peer_grant(), 1_000, 3_000)
            .unwrap();
        let intruder = store
            .accept_invitation_with_capabilities(
                &invite.pairing_id,
                &invite.token,
                "peer-three",
                "runner-local",
                None,
                1_100,
                secrets.as_ref(),
            )
            .unwrap();
        drop(store);
        let api = RemoteApi::injected(
            api.paths.clone(),
            api.host.clone(),
            RemoteStore::open(&api.paths.root).unwrap(),
            secrets,
        )
        .with_peer_runs(Arc::new(FakePeerRuns::default()));

        let response = api.handle(
            signed(
                &intruder.device_id,
                intruder.device_secret.as_bytes(),
                1,
                "cmd-peer-peek",
                "GET",
                "/v1/remote/peer/threads/thread-1",
                b"",
            ),
            2_100,
        );
        assert_eq!(response.status, 404);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_envelope_that_loops_back_here_is_refused_before_it_is_stored() {
        let (root, api, device, secret) = peer_fixture(every_peer_grant());
        let runs = Arc::new(FakePeerRuns::default());
        let api = api.with_peer_runs(runs.clone());
        let mut looped = little_monkey_lib::peers::PeerEnvelope::new(
            "msg-1",
            "thread-1",
            little_monkey_lib::peers::PeerMessageKind::Message,
            "instance-peer-two",
            "round and round",
            2_000,
            600_000,
        );
        // This node is already in the chain: the message has been here before.
        looped.origin_chain.push("runner-local".into());

        let response = api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-peer-loop",
                "POST",
                "/v1/remote/peer/messages",
                &serde_json::to_vec(&looped).unwrap(),
            ),
            2_000,
        );
        assert_eq!(response.status, 400);
        assert!(runs.submitted.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_expired_request_is_refused_rather_than_run_late() {
        let (root, api, device, secret) = peer_fixture(every_peer_grant());
        let runs = Arc::new(FakePeerRuns::default());
        let api = api.with_peer_runs(runs.clone());

        // A one-second life, so the request can arrive after it expired and
        // still be well inside the signature's own skew window: this has to
        // fail as expired, not as unauthorized.
        let stale = little_monkey_lib::peers::PeerEnvelope::new(
            "msg-1",
            "thread-1",
            little_monkey_lib::peers::PeerMessageKind::TaskRequest,
            "instance-peer-two",
            "summarize the failing nightly build",
            2_000,
            1_000,
        );
        let response = api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-peer-stale",
                "POST",
                "/v1/remote/peer/messages",
                &serde_json::to_vec(&stale).unwrap(),
            ),
            120_000,
        );
        assert_eq!(response.status, 400);
        assert!(runs.submitted.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    fn placement_body(
        run_id: &str,
        required_residency: Option<&str>,
        expected_runner_id: Option<&str>,
    ) -> Vec<u8> {
        serde_json::to_vec(&little_monkey_lib::node_placement::PlaceRunRequest {
            protocol_version: little_monkey_lib::node_placement::NODE_PROTOCOL_VERSION,
            spec: spec(run_id, "workspace-one"),
            required_residency: required_residency.map(str::to_string),
            expected_runner_id: expected_runner_id.map(str::to_string),
        })
        .unwrap()
    }

    /// **The grant that gates the only route through which a run this machine
    /// did not author can start here.** Every existing pairing — and any new
    /// one that was not explicitly given the K17 grants — is refused, which is
    /// why `PlaceRuns` is its own capability rather than an implication of
    /// `RunWorkflows` or of any run scope.
    #[test]
    fn a_pairing_without_the_placement_grants_cannot_describe_or_place() {
        let (root, api, _secrets, device, secret) = fixture();
        let api = api.with_placement(Arc::new(FakePlacementQueue::default()));
        for (index, (method, path, body)) in [
            ("GET", "/v1/remote/node", &b""[..]),
            ("GET", "/v1/remote/node/health", b""),
            ("POST", "/v1/remote/node/runs", b"{}"),
            ("GET", "/v1/remote/node/runs/run-one", b""),
        ]
        .into_iter()
        .enumerate()
        {
            let response = api.handle(
                signed(
                    &device,
                    &secret,
                    index as u64 + 1,
                    &format!("cmd-node-{index}"),
                    method,
                    path,
                    body,
                ),
                2_000,
            );
            assert_eq!(
                response.status, 403,
                "{method} {path} must need an explicit K17 grant"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    /// The node checks the residency claim rather than trusting the placer's
    /// word for it. A rule enforced only by the sender is not enforced.
    #[test]
    fn the_node_refuses_a_placement_whose_residency_rule_it_does_not_satisfy() {
        let (root, api, device, secret) = placement_fixture();
        let api = api.with_placement(Arc::new(FakePlacementQueue::default()));
        let body = placement_body("run-placed", Some("eu-west"), None);
        let response = api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-residency",
                "POST",
                "/v1/remote/node/runs",
                &body,
            ),
            2_000,
        );
        assert_eq!(response.status, 409);
        let message = String::from_utf8_lossy(&response.body).to_string();
        assert!(
            message.contains("unspecified") && message.contains("eu-west"),
            "the refusal must name both labels: {message}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// Same shape for identity: an alias that has started pointing at a
    /// different machine is a refusal, not a silent re-target.
    #[test]
    fn the_node_refuses_a_placement_addressed_to_a_different_runner() {
        let (root, api, device, secret) = placement_fixture();
        let api = api.with_placement(Arc::new(FakePlacementQueue::default()));
        let body = placement_body("run-placed", None, Some("runner-two"));
        let response = api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-runner",
                "POST",
                "/v1/remote/node/runs",
                &body,
            ),
            2_000,
        );
        assert_eq!(response.status, 409);
        assert!(String::from_utf8_lossy(&response.body).contains("runner-two"));
        let _ = std::fs::remove_dir_all(root);
    }

    /// The node owns what it accepts: it mints its own run id, records the
    /// placement against the placing device, and a second *different* signed
    /// request carrying the same spec resolves to the same placement instead of
    /// starting a second run. (The replay guard covers an identical retry; this
    /// covers the case it cannot see.)
    #[test]
    fn an_accepted_placement_is_owned_recorded_and_idempotent_per_spec() {
        let (root, api, device, secret) = placement_fixture();
        let queue = Arc::new(FakePlacementQueue::default());
        let api = api.with_placement(queue.clone());
        let body = placement_body("run-placed", None, Some("runner-one"));

        let first = api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-place-a",
                "POST",
                "/v1/remote/node/runs",
                &body,
            ),
            2_000,
        );
        assert_eq!(
            first.status,
            201,
            "{}",
            String::from_utf8_lossy(&first.body)
        );
        let accepted: little_monkey_lib::node_placement::PlaceRunResponse =
            serde_json::from_slice(&first.body).unwrap();
        assert_eq!(accepted.submitted_run_id, "run-placed");
        assert_eq!(accepted.node_run_id, "node-run-placed");
        assert_ne!(
            accepted.node_run_id, accepted.submitted_run_id,
            "the node must not adopt a foreign run id as its own"
        );

        let second = api.handle(
            signed(
                &device,
                &secret,
                2,
                "cmd-place-b",
                "POST",
                "/v1/remote/node/runs",
                &body,
            ),
            2_500,
        );
        assert_eq!(second.status, 200, "a re-placed spec is the same placement");
        let replayed: little_monkey_lib::node_placement::PlaceRunResponse =
            serde_json::from_slice(&second.body).unwrap();
        assert_eq!(replayed.node_run_id, accepted.node_run_id);
        assert_eq!(
            queue.placed.lock().unwrap().len(),
            1,
            "the node queued the spec exactly once"
        );

        // And the placement reads back, keyed by the SUBMITTER's id.
        let status = api.handle(
            signed(
                &device,
                &secret,
                3,
                "cmd-status",
                "GET",
                "/v1/remote/node/runs/run-placed",
                b"",
            ),
            2_600,
        );
        assert_eq!(status.status, 200);
        let status: little_monkey_lib::node_placement::PlacedRunStatus =
            serde_json::from_slice(&status.body).unwrap();
        assert_eq!(status.node_run_id, "node-run-placed");
        assert_eq!(status.state, "running");
        let _ = std::fs::remove_dir_all(root);
    }

    /// The node's own refusal reaches the placer as a refusal, and nothing is
    /// recorded: a placement row claiming this node took work it never queued
    /// would be worse than the failed request.
    #[test]
    fn a_queue_refusal_is_reported_and_leaves_no_placement_record() {
        let (root, api, device, secret) = placement_fixture();
        let api = api.with_placement(Arc::new(FakePlacementQueue::refusing(
            "the placed workspace root '/nowhere' does not exist on this node",
        )));
        let body = placement_body("run-placed", None, None);
        let response = api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-refuse",
                "POST",
                "/v1/remote/node/runs",
                &body,
            ),
            2_000,
        );
        assert_eq!(response.status, 409);
        assert!(String::from_utf8_lossy(&response.body).contains("/nowhere"));

        let follow_up = api.handle(
            signed(
                &device,
                &secret,
                2,
                "cmd-refuse-status",
                "GET",
                "/v1/remote/node/runs/run-placed",
                b"",
            ),
            2_100,
        );
        assert_eq!(
            follow_up.status, 404,
            "a refused placement must leave no record behind"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// A build with no placement queue answers the route explicitly rather than
    /// accepting a spec it has no way to run.
    #[test]
    fn a_node_without_a_placement_queue_refuses_rather_than_accepting() {
        let (root, api, device, secret) = placement_fixture();
        let response = api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-no-queue",
                "POST",
                "/v1/remote/node/runs",
                &placement_body("run-placed", None, None),
            ),
            2_000,
        );
        assert_eq!(response.status, 501);
        let _ = std::fs::remove_dir_all(root);
    }

    // --- Live migration (roadmap K18) -------------------------------------
    //
    // Two machines is the honest bar for this feature and this repository's CI
    // has one. What these exercise is the *wire path* against a loopback node:
    // the real routes, the real signed transport, the real ledger, the real
    // files on disk. They are not a substitute for two hosts — nothing here
    // proves a network, a clock skew between machines, or a partial transfer.

    /// Writes a frozen checkpoint and its workspace on a pretend origin node,
    /// and returns that node's app-data root plus the checkpoint id.
    fn frozen_origin(model: Option<&str>) -> (PathBuf, String) {
        use little_monkey_lib::checkpoints::{CheckpointEntry, CheckpointManifest, ResumeState};

        let origin = std::env::temp_dir().join(format!(
            "little-monkey-migration-origin-{}",
            uuid::Uuid::new_v4()
        ));
        let workspace = origin.join("work");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src").join("main.rs"), b"fn main() {}").unwrap();

        let checkpoint_id = "cp-migrate-01".to_string();
        let checkpoint_dir = origin.join("checkpoints").join(&checkpoint_id);
        std::fs::create_dir_all(&checkpoint_dir).unwrap();
        std::fs::write(checkpoint_dir.join("0.bak"), b"fn main() {} // before").unwrap();
        let manifest = CheckpointManifest {
            version: 3,
            created_at_ms: 1_000,
            session_id: "session-migrated".to_string(),
            anchor_index: 0,
            label: "the frozen turn".to_string(),
            shell_ran: false,
            external_effects: vec![],
            committed_effects: None,
            reverted: false,
            prev_id: None,
            entries: vec![CheckpointEntry {
                path: workspace
                    .join("src")
                    .join("main.rs")
                    .to_string_lossy()
                    .to_string(),
                backup: Some("0.bak".to_string()),
                redo: None,
                after: None,
            }],
            remembered_facts: vec![],
            staged_task_suggestions: vec![],
            resume: Some(ResumeState {
                process_id: "turn-origin-01".to_string(),
                frozen_at_ms: 1_500,
                model: model.map(str::to_string),
                runtime_id: None,
                workspace: Some(workspace.to_string_lossy().to_string()),
                pending_approvals: vec![],
            }),
        };
        std::fs::write(
            checkpoint_dir.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(
            origin.join("chat_sessions.json"),
            serde_json::json!({
                "sessions": [{ "id": "session-migrated", "messages": ["the frozen conversation"] }],
                "activeSessionId": "session-migrated",
            })
            .to_string(),
        )
        .unwrap();
        (origin, checkpoint_id)
    }

    /// K17's placement pairing plus the K18 grant, which is exactly the shape
    /// the capability rule requires: `migrate` implies `place_runs`.
    fn migration_fixture() -> (PathBuf, RemoteApi, String, Vec<u8>) {
        migration_pairing(BTreeSet::from([
            DeviceCapability::ViewRuns,
            DeviceCapability::DescribeNode,
            DeviceCapability::PlaceRuns,
            DeviceCapability::Migrate,
        ]))
    }

    fn migration_pairing(
        capabilities: BTreeSet<DeviceCapability>,
    ) -> (PathBuf, RemoteApi, String, Vec<u8>) {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-remote-migrate-{}",
            uuid::Uuid::new_v4()
        ));
        let paths = DaemonPaths::under(&root);
        paths.ensure().unwrap();
        DaemonConfig::default().save(&paths).unwrap();
        let host = RemoteHostConfig {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            runner_id: "runner-one".into(),
            listen: "127.0.0.1:1".into(),
            advertise_url: "https://runner.invalid".into(),
            certificate_path: "/tmp/cert".into(),
            private_key_path: "/tmp/key".into(),
            certificate_sha256: "a".repeat(64),
            enabled: true,
        };
        let mut store = RemoteStore::open(&paths.root).unwrap();
        let scopes = RemoteScopes {
            actions: BTreeSet::from([RemoteAction::ViewRuns]),
            run_ids: BTreeSet::from(["run-one".into()]),
            workspace_ids: BTreeSet::new(),
            max_artifact_bytes: 1_024,
        };
        let secrets = Arc::new(FakeSecrets::default());
        let invite = store
            .create_invitation_with_capabilities(&scopes, &capabilities, 1_000, 3_000)
            .unwrap();
        let accepted = store
            .accept_invitation_with_capabilities(
                &invite.pairing_id,
                &invite.token,
                "origin",
                "runner-one",
                None,
                1_100,
                secrets.as_ref(),
            )
            .unwrap();
        let secret = accepted.device_secret.as_bytes().to_vec();
        let api = RemoteApi::injected(paths, host, store, secrets);
        (root, api, accepted.device_id, secret)
    }

    #[test]
    fn a_frozen_image_moves_to_the_node_and_lands_as_a_resumable_turn() {
        let (root, api, device, secret) = migration_fixture();
        // No model recorded, so the target's "is it here" check has nothing to
        // refuse. The model refusal has its own test below.
        let (origin, checkpoint_id) = frozen_origin(None);
        let spec = spec("run-migrated", "workspace-one");
        let image = super::super::migrate::build_image(
            &origin,
            "runner-origin",
            &checkpoint_id,
            &spec,
            7,
            &"c".repeat(64),
            None,
        )
        .expect("the origin can read its own frozen image");

        let preflight = serde_json::to_vec(&MigrationPreflightRequest {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            header: image.header.clone(),
        })
        .unwrap();
        let response = api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-preflight",
                "POST",
                "/v1/remote/node/migration/preflight",
                &preflight,
            ),
            2_000,
        );
        assert_eq!(
            response.status,
            200,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(body["verdict"]["state"], "acceptable");
        // The determinism statement travels with the verdict, so whoever presses
        // Migrate reads it rather than a doc.
        assert!(!body["verdict"]["caveats"].as_array().unwrap().is_empty());
        // Nothing has moved yet: a preflight that landed anything would make the
        // refusal path a write.
        assert!(!root.join("checkpoints").join(&checkpoint_id).exists());

        let accept = serde_json::to_vec(&MigrationAcceptRequest {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            image: image.clone(),
        })
        .unwrap();
        let response = api.handle(
            signed(
                &device,
                &secret,
                2,
                "cmd-accept",
                "POST",
                "/v1/remote/node/migration/accept",
                &accept,
            ),
            2_100,
        );
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let receipt: MigrationReceipt = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(receipt.run_id, "run-migrated");

        // The workspace really crossed.
        let landed_file = PathBuf::from(&receipt.workspace_root)
            .join("src")
            .join("main.rs");
        assert_eq!(std::fs::read(&landed_file).unwrap(), b"fn main() {}");

        // The conversation crossed too — without it a resume would continue a
        // turn with no history.
        let sessions: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("chat_sessions.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(sessions["sessions"][0]["id"], "session-migrated");

        // And the checkpoint the desktop's K13 re-entry reads is on disk, with
        // its paths re-rooted here and its resume naming the *local* row.
        let manifest: little_monkey_lib::checkpoints::CheckpointManifest = serde_json::from_str(
            &std::fs::read_to_string(
                root.join("checkpoints")
                    .join(&checkpoint_id)
                    .join("manifest.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let resume = manifest
            .resume
            .expect("the landed checkpoint is still a freeze");
        assert_eq!(resume.process_id, receipt.process_id);
        assert_eq!(
            resume.workspace.as_deref(),
            Some(receipt.workspace_root.as_str())
        );
        assert!(manifest.entries[0]
            .path
            .starts_with(&receipt.workspace_root));
        assert!(root
            .join("checkpoints")
            .join(&checkpoint_id)
            .join("0.bak")
            .exists());

        // The process row is suspended, which is exactly the state the desktop's
        // Resume path looks for.
        let ledger = RunLedger::open(&DaemonPaths::under(&root).ledger_db).unwrap();
        let record = ledger
            .process_table()
            .get(&receipt.process_id)
            .unwrap()
            .expect("the landed process exists");
        assert_eq!(
            record.state,
            little_monkey_lib::process_table::ProcessState::Suspended
        );
        assert_eq!(record.run_id.as_deref(), Some("run-migrated"));

        // One chain across both nodes: the target's first event names the
        // origin's tip, and the join is what an auditor holding both halves runs.
        let arrival = ledger
            .migration_arrival("run-migrated")
            .unwrap()
            .expect("the target's half starts with an arrival");
        assert_eq!(arrival.event_hash, receipt.arrival_event_hash);
        let departure = little_monkey_lib::run_ledger::MigrationDeparture {
            run_id: "run-migrated".to_string(),
            sequence: 7,
            event_hash: "c".repeat(64),
            target_node_id: "runner-one".to_string(),
            payload_sha256: image.header.payload_sha256.clone(),
            checkpoint_id: checkpoint_id.clone(),
        };
        assert!(matches!(
            little_monkey_lib::run_ledger::join_migration_chain(&departure, &arrival),
            little_monkey_lib::run_ledger::MigrationChainJoin::Joined { .. }
        ));
        // And an origin claiming a different tip does not join, which is the
        // whole point of hashing the link rather than trusting the field.
        let mut forged = departure;
        forged.event_hash = "d".repeat(64);
        assert!(matches!(
            little_monkey_lib::run_ledger::join_migration_chain(&forged, &arrival),
            little_monkey_lib::run_ledger::MigrationChainJoin::Broken { .. }
        ));

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(origin);
    }

    #[test]
    fn a_node_without_the_model_refuses_and_writes_nothing() {
        let (root, api, device, secret) = migration_fixture();
        let (origin, checkpoint_id) = frozen_origin(Some("a-model-this-node-never-installed"));
        let spec = spec("run-migrated", "workspace-one");
        let image = super::super::migrate::build_image(
            &origin,
            "runner-origin",
            &checkpoint_id,
            &spec,
            7,
            &"c".repeat(64),
            None,
        )
        .unwrap();
        let accept = serde_json::to_vec(&MigrationAcceptRequest {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            image,
        })
        .unwrap();
        let response = api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-accept-refused",
                "POST",
                "/v1/remote/node/migration/accept",
                &accept,
            ),
            2_000,
        );
        assert_eq!(response.status, 409);
        let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(body["verdict"]["state"], "refused");
        assert!(body["verdict"]["blockers"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("model-not-resident")));
        // A refusal is not a partial landing.
        assert!(!root.join("checkpoints").join(&checkpoint_id).exists());
        let ledger = RunLedger::open(&DaemonPaths::under(&root).ledger_db).unwrap();
        assert!(ledger.load_run("run-migrated").unwrap().is_none());
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(origin);
    }

    /// A scheduler paired to *place* runs must not thereby be able to write a
    /// workspace and a conversation onto this machine.
    #[test]
    fn a_pairing_that_may_place_runs_still_cannot_migrate_one_here() {
        let (root, api, device, secret) = migration_pairing(BTreeSet::from([
            DeviceCapability::ViewRuns,
            DeviceCapability::DescribeNode,
            DeviceCapability::PlaceRuns,
        ]));
        let response = api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-migrate-denied",
                "POST",
                "/v1/remote/node/migration/accept",
                b"{}",
            ),
            2_000,
        );
        assert_eq!(response.status, 403);
        assert!(String::from_utf8_lossy(&response.body).contains("Migrate"));
        let _ = std::fs::remove_dir_all(root);
    }

    /// Tampering with a transferred file breaks the payload digest, and the
    /// image is refused as malformed rather than admitted and then landed.
    #[test]
    fn a_tampered_payload_is_refused_before_any_capability_question() {
        let (root, api, device, secret) = migration_fixture();
        let (origin, checkpoint_id) = frozen_origin(None);
        let spec = spec("run-migrated", "workspace-one");
        let mut image = super::super::migrate::build_image(
            &origin,
            "runner-origin",
            &checkpoint_id,
            &spec,
            7,
            &"c".repeat(64),
            None,
        )
        .unwrap();
        image.payload.workspace_files[0].contents_base64 = STANDARD.encode(b"fn main() { evil() }");
        let accept = serde_json::to_vec(&MigrationAcceptRequest {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            image,
        })
        .unwrap();
        let response = api.handle(
            signed(
                &device,
                &secret,
                1,
                "cmd-accept-tampered",
                "POST",
                "/v1/remote/node/migration/accept",
                &accept,
            ),
            2_000,
        );
        assert_eq!(response.status, 400);
        assert!(String::from_utf8_lossy(&response.body).contains("digest"));
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(origin);
    }

    // --- `/v1/remote/device/*` -------------------------------------------

    fn grant(api: &RemoteApi, device_id: &str, extra: &[DeviceCapability]) {
        let mut store = api.store.lock().unwrap();
        let mut capabilities = store.device(device_id).unwrap().unwrap().capabilities;
        capabilities.extend(extra.iter().copied());
        store
            .set_device_capabilities(device_id, &capabilities, 2_000)
            .unwrap();
    }

    fn advertise(
        api: &RemoteApi,
        device_id: &str,
        secret: &[u8],
        sequence: u64,
        capabilities: &[DeviceCapability],
        permissions: &[(DeviceCapability, OsPermission)],
    ) -> ApiResponse {
        let surface = DeviceSurface {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            platform: "android".into(),
            platform_version: "15".into(),
            app_version: "1.3.0".into(),
            device_model: "Pixel 9".into(),
            capabilities: capabilities.iter().copied().collect(),
            permissions: permissions.iter().copied().collect(),
            // Everything this helper advertises is ready; a test that needs an
            // unready capability states its permission instead, which is the
            // axis those tests are about.
            readiness: capabilities
                .iter()
                .map(|capability| (*capability, DeviceReadiness::Ready))
                .collect(),
            constraints: DeviceConstraints::default(),
            reported_at_ms: 0,
        };
        let body = serde_json::to_vec(&surface).unwrap();
        api.handle(
            signed(
                device_id,
                secret,
                sequence,
                &format!("cmd-surface-{sequence}"),
                "POST",
                "/v1/remote/device/surface",
                &body,
            ),
            2_000,
        )
    }

    /// Everything a Talk ticket has to be, in one pass.
    ///
    /// A ticket is the only credential a WebSocket handshake can carry, so the
    /// properties below are the whole of that surface's security and each one
    /// fails loudly here rather than in a reviewer's memory:
    ///
    /// - it is issued **only** to a request that already passed the signature,
    ///   sequence, nonce and revocation checks, and **only** with the grant;
    /// - it admits **once** — a second socket with the same ticket is refused;
    /// - it is bound to **its own session**, so a ticket for one conversation
    ///   cannot open another;
    /// - it **expires**, in seconds rather than for the life of the socket;
    /// - the bearer never appears in the path that a log or a history entry
    ///   would keep.
    #[test]
    fn a_talk_ticket_admits_one_socket_once_and_only_with_the_grant() {
        let (root, api, _secrets, device_id, secret) = fixture();
        let body = serde_json::to_vec(&serde_json::json!({
            "protocol_version": super::super::protocol::TALK_PROTOCOL_VERSION,
            "session_id": "talk-session-one",
        }))
        .unwrap();
        let ask = |sequence: u64| {
            signed(
                &device_id,
                &secret,
                sequence,
                &format!("cmd-talk-{sequence}"),
                "POST",
                "/v1/remote/device/talk/ticket",
                &body,
            )
        };

        // No grant, no ticket — before anything about sockets is considered.
        assert_eq!(api.handle(ask(1), 2_000).status, 403);

        // `voice_stream` is not grantable on its own — a stream is a
        // microphone — so the pair is what an operator actually grants.
        grant(
            &api,
            &device_id,
            &[
                DeviceCapability::MicrophoneCapture,
                DeviceCapability::VoiceStream,
            ],
        );
        assert_eq!(
            advertise(
                &api,
                &device_id,
                &secret,
                2,
                &[
                    DeviceCapability::MicrophoneCapture,
                    DeviceCapability::VoiceStream
                ],
                &[
                    (DeviceCapability::MicrophoneCapture, OsPermission::Granted),
                    (DeviceCapability::VoiceStream, OsPermission::Granted),
                ],
            )
            .status,
            200
        );
        let response = api.handle(ask(3), 2_000);
        assert_eq!(response.status, 201);
        let issued: TalkTicketResponse = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(
            issued.websocket_path,
            "/v1/remote/device/talk/talk-session-one/stream"
        );
        assert!(
            !issued.websocket_path.contains(&issued.ticket),
            "the bearer must not be part of the path"
        );

        // A ticket for this session opens no other one.
        assert!(api
            .consume_talk_ticket("talk-session-two", &issued.ticket, 2_100)
            .is_none());
        // …and that misdirected attempt burned it, so the right session cannot
        // use it afterwards either.
        assert!(api
            .consume_talk_ticket("talk-session-one", &issued.ticket, 2_100)
            .is_none());

        let second = api.handle(ask(4), 2_000);
        let issued: TalkTicketResponse = serde_json::from_slice(&second.body).unwrap();
        let admitted = api
            .consume_talk_ticket("talk-session-one", &issued.ticket, 2_100)
            .expect("a fresh ticket admits its own session");
        assert_eq!(admitted.device_id, device_id);
        assert_eq!(admitted.session_generation, issued.session_generation);
        assert!(
            api.consume_talk_ticket("talk-session-one", &issued.ticket, 2_100)
                .is_none(),
            "one use only: a captured ticket cannot open a second socket"
        );

        // Expiry is real, and short.
        let third = api.handle(ask(5), 2_000);
        let issued: TalkTicketResponse = serde_json::from_slice(&third.body).unwrap();
        assert!(api
            .consume_talk_ticket("talk-session-one", &issued.ticket, issued.expires_at_ms)
            .is_none());

        // A grant withdrawn between issue and handshake closes the door, which
        // is why the check is repeated at admission rather than trusted from
        // issue time.
        let fourth = api.handle(ask(6), 2_000);
        let issued: TalkTicketResponse = serde_json::from_slice(&fourth.body).unwrap();
        {
            let mut store = api.store.lock().unwrap();
            // Exactly what an operator withdrawing one capability does: the
            // rest of the grant is untouched.
            let mut kept = store.device(&device_id).unwrap().unwrap().capabilities;
            kept.remove(&DeviceCapability::VoiceStream);
            store
                .set_device_capabilities(&device_id, &kept, 2_000)
                .unwrap();
        }
        assert!(
            api.consume_talk_ticket("talk-session-one", &issued.ticket, 2_100)
                .is_none(),
            "a revoked grant must not be admitted by a ticket minted before it"
        );
        assert!(!api.talk_capability_live(&device_id));

        let _ = std::fs::remove_dir_all(root);
    }

    /// A signed GET on the stream route is not the way in, and says so rather
    /// than 404ing on a route the published contract names.
    #[test]
    fn a_plain_get_on_the_talk_stream_route_asks_for_an_upgrade() {
        let (root, api, _secrets, device_id, secret) = fixture();
        grant(
            &api,
            &device_id,
            &[
                DeviceCapability::MicrophoneCapture,
                DeviceCapability::VoiceStream,
            ],
        );
        let response = api.handle(
            signed(
                &device_id,
                &secret,
                1,
                "cmd-talk-get",
                "GET",
                "/v1/remote/device/talk/talk-session-one/stream",
                b"",
            ),
            2_000,
        );
        assert_eq!(response.status, 426);
        assert!(String::from_utf8_lossy(&response.body).contains("talk/ticket"));
        let _ = std::fs::remove_dir_all(root);
    }

    // --- Talk, on the wire -------------------------------------------------

    /// A conversation queue that goes through the **real** durable ingress.
    ///
    /// The only thing standing in for production here is the run *executor*:
    /// `submit_conversation_turn` writes a real `ingress_turns` row under a real
    /// dedupe key, and the answer is a real run in a real ledger. That is the
    /// boundary a test is allowed to draw — a fake ingress would make the whole
    /// exercise meaningless, since "does a spoken turn become an ordinary
    /// durable turn" is the question.
    struct IngressTalkQueue {
        paths: DaemonPaths,
        runs: Mutex<HashMap<String, String>>,
        /// The recorders the test plays the model's part through.
        recorders: Mutex<HashMap<String, Arc<DurableRunRecorder>>>,
        /// The turns that reached ingress, with the outcome each one got.
        accepted: Mutex<Vec<(String, String)>>,
    }

    struct RecordingRunQueue;

    impl crate::daemon::channel_worker::RunQueue for RecordingRunQueue {
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
            ingress: &little_monkey_lib::channels::ingress::ConversationIngress,
            _params: Vec<String>,
        ) -> Result<String, String> {
            Ok(ingress.deterministic_job_id())
        }
    }

    impl IngressTalkQueue {
        fn new(paths: DaemonPaths) -> Self {
            Self {
                paths,
                runs: Mutex::new(HashMap::new()),
                recorders: Mutex::new(HashMap::new()),
                accepted: Mutex::new(Vec::new()),
            }
        }

        /// The run a spoken turn produced, so the test can play the model's part
        /// by appending real events to it.
        fn recorder(&self, client_key: &str) -> Option<Arc<DurableRunRecorder>> {
            self.recorders.lock().unwrap().get(client_key).cloned()
        }
    }

    impl MobileChatQueue for IngressTalkQueue {
        fn queue_chat(
            &self,
            session_id: &str,
            client_key: &str,
            prompt: &str,
        ) -> Result<String, String> {
            use little_monkey_lib::channels::ingress::{ConversationIngress, ConversationSource};
            use little_monkey_lib::channels::routing::RouteTarget;

            let now_ms = crate::daemon::remote::now_ms_public()? as i64;
            // Exactly the shape `queue_mobile_chat_recipe` builds, because a
            // spoken turn is a mobile chat turn: same source, same session key,
            // same recipe.
            let ingress = ConversationIngress::direct(
                ConversationSource::Mobile,
                session_id,
                client_key,
                format!("mobile:{session_id}"),
                prompt,
                RouteTarget::new("mobile-chat"),
                now_ms,
            );
            let mut store = crate::daemon::store::DaemonStore::open(&self.paths)
                .map_err(|error| error.to_string())?;
            let outcome = crate::daemon::channel_ingress::submit_conversation_turn(
                &mut store,
                &RecordingRunQueue,
                &ingress,
                &[format!("prompt={prompt}")],
                now_ms,
            )?;
            self.accepted
                .lock()
                .unwrap()
                .push((client_key.to_string(), format!("{outcome:?}")));

            // One real run per turn, so the session reads its answer back out of
            // the ledger the way it does in production.
            let run_id = format!("run-{client_key}");
            let ledger = RunLedger::open(&self.paths.ledger_db).map_err(|e| e.to_string())?;
            let (recorder, _) = DurableRunRecorder::submit(
                ledger,
                &spec(&run_id, "workspace-talk"),
                "talk-fixture".into(),
            )
            .map_err(|error| error.to_string())?;
            recorder
                .emit(RunEvent::Started {
                    engine_id: "talk-fixture".into(),
                })
                .map_err(|error| error.to_string())?;
            self.runs
                .lock()
                .unwrap()
                .insert(client_key.to_string(), run_id.clone());
            self.recorders
                .lock()
                .unwrap()
                .insert(client_key.to_string(), recorder);
            Ok(run_id)
        }

        fn chat_run_id(
            &self,
            _session_id: &str,
            client_key: &str,
        ) -> Result<Option<String>, String> {
            Ok(self.runs.lock().unwrap().get(client_key).cloned())
        }
    }

    /// A scripted transcriber and synthesizer — the two things genuinely outside
    /// this process.
    struct ScriptedSpeech {
        transcripts: Mutex<std::collections::VecDeque<String>>,
        heard_bytes: Mutex<Vec<usize>>,
        spoken: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl super::super::talk::TalkSpeech for ScriptedSpeech {
        async fn transcribe(&self, audio: Vec<u8>, _media_type: &str) -> Result<String, String> {
            self.heard_bytes.lock().unwrap().push(audio.len());
            Ok(self
                .transcripts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default())
        }

        async fn synthesize(&self, text: &str) -> Result<(Vec<u8>, String), String> {
            self.spoken.lock().unwrap().push(text.to_string());
            Ok((b"RIFFfake".to_vec(), "audio/wav".to_string()))
        }
    }

    fn talk_frame(
        session_id: &str,
        generation: &str,
        sequence: u64,
        kind: serde_json::Value,
    ) -> tokio_tungstenite::tungstenite::Message {
        let mut frame = serde_json::json!({
            "protocol_version": super::super::protocol::TALK_PROTOCOL_VERSION,
            "session_id": session_id,
            "session_generation": generation,
            "frame_sequence": sequence,
        });
        let object = frame.as_object_mut().unwrap();
        for (key, value) in kind.as_object().unwrap() {
            object.insert(key.clone(), value.clone());
        }
        tokio_tungstenite::tungstenite::Message::Text(frame.to_string().into())
    }

    /// **The whole spoken path, over a real socket, with nothing internal
    /// faked.**
    ///
    /// Every unit test in this repository passed while mobile Talk could not
    /// complete a single utterance, because two defects lived in the seams no
    /// unit test crosses: the shipped client never sent the `hello` the runner
    /// demands, and the connection was served without upgrades so the socket
    /// after the `101` never arrived. Both are invisible to a scripted socket
    /// and to a source-string scan. So this drives the real thing:
    ///
    /// signed ticket → real HTTP upgrade → real `tokio-tungstenite` client →
    /// hello → audio → transcription → **real durable ingress** → a real run in
    /// a real ledger → assistant deltas → speech before the run finishes →
    /// barge-in that cancels and becomes the next turn → revocation that closes
    /// the socket.
    #[tokio::test]
    async fn a_paired_phone_holds_a_spoken_conversation_over_a_real_talk_socket() {
        use futures_util::{SinkExt, StreamExt};

        let (root, api, _secrets, device_id, secret) = fixture();
        grant(
            &api,
            &device_id,
            &[
                DeviceCapability::VoiceStream,
                DeviceCapability::MicrophoneCapture,
            ],
        );
        advertise(
            &api,
            &device_id,
            &secret,
            1,
            &[
                DeviceCapability::VoiceStream,
                DeviceCapability::MicrophoneCapture,
            ],
            &[
                (DeviceCapability::VoiceStream, OsPermission::Granted),
                (DeviceCapability::MicrophoneCapture, OsPermission::Granted),
            ],
        );

        let queue = Arc::new(IngressTalkQueue::new(DaemonPaths::under(&root)));
        let speech = Arc::new(ScriptedSpeech {
            transcripts: Mutex::new(
                [
                    "what is the deploy status",
                    "stop and tell me about staging",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
            ),
            heard_bytes: Mutex::new(Vec::new()),
            spoken: Mutex::new(Vec::new()),
        });
        let api = api
            .with_mobile_chat(queue.clone())
            .with_talk_speech(speech.clone());

        // The real server, minus only TLS — which is the same listener every
        // other route on this plane shares and is not what is under test.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let served = api.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let served = served.clone();
                tokio::spawn(async move {
                    // Production's own connection path, so a regression there —
                    // dropping `with_upgrades`, say — fails this test rather
                    // than only the phone.
                    let _ = super::super::server::serve_upgradable(
                        hyper_util::rt::TokioIo::new(stream),
                        served,
                    )
                    .await;
                });
            }
        });

        // One ordinary signed request mints the ticket.
        let session_id = format!("mobile-{device_id}");
        let ticket_body = serde_json::to_vec(&serde_json::json!({
            "protocol_version": super::super::protocol::TALK_PROTOCOL_VERSION,
            "session_id": session_id,
        }))
        .unwrap();
        let now_ms = crate::daemon::remote::now_ms_public().unwrap();
        let issued = api.handle(
            signed_at(
                &device_id,
                &secret,
                2,
                "cmd-talk-ticket",
                "POST",
                "/v1/remote/device/talk/ticket",
                &ticket_body,
                now_ms,
            ),
            now_ms,
        );
        assert_eq!(issued.status, 201, "the grant admits a ticket");
        let ticket: serde_json::Value = serde_json::from_slice(&issued.body).unwrap();
        let bearer = ticket["ticket"].as_str().unwrap().to_string();
        let generation = ticket["session_generation"].as_str().unwrap().to_string();
        let path = ticket["websocket_path"].as_str().unwrap().to_string();

        let url = format!("ws://{address}{path}?ticket={bearer}");
        let (mut socket, response) = tokio_tungstenite::connect_async(&url).await.expect(
            "the ticket admits a real WebSocket — a 101 whose upgrade never \
             resolves fails exactly here",
        );
        assert_eq!(response.status().as_u16(), 101);

        let mut sequence = 0u64;
        let mut next = |kind: serde_json::Value| {
            sequence += 1;
            talk_frame(&session_id, &generation, sequence, kind)
        };

        // Frame 1 is the hello, and its media type is the one the audio frames
        // will actually carry.
        socket
            .send(next(serde_json::json!({
                "type": "hello",
                "media_type": "audio/webm;codecs=opus",
                "sample_rate_hz": 48_000,
                "channels": 1,
            })))
            .await
            .unwrap();
        // The client's own order: telemetry naming the utterance, then the
        // utterance. The runner answers the instant an utterance closes, so
        // metrics sent after it would be too late to belong to it.
        socket
            .send(next(serde_json::json!({
                "type": "metrics",
                "audio_sequence": 1,
                "speech_detection_ms": 180,
                "capture_ms": 1_200,
                "upload_ms": 40,
            })))
            .await
            .unwrap();
        socket
            .send(next(serde_json::json!({
                "type": "audio",
                "audio_sequence": 1,
                "media_type": "audio/webm;codecs=opus",
                "audio_base64": STANDARD.encode(b"first utterance bytes"),
                "last": true,
                // The device's own name for this utterance, which a closing
                // frame must carry: it is the key the turn is queued under, and
                // the only identity that survives a restart of this runner.
                "utterance_id": "utt-first",
            })))
            .await
            .unwrap();

        // The runner reaches transcription, which means the hello was accepted
        // and the audio was not refused.
        let transcript = read_until(&mut socket, "transcript").await;
        assert_eq!(transcript["text"], "what is the deploy status");
        assert!(!speech.heard_bytes.lock().unwrap().is_empty());

        // The turn is a real durable one, under the utterance's own identity.
        let first_key = queue.accepted.lock().unwrap()[0].0.clone();
        assert!(first_key.starts_with("talk-"));
        {
            let store = crate::daemon::store::DaemonStore::open(&DaemonPaths::under(&root))
                .expect("daemon store");
            let dedupe = little_monkey_lib::channels::ingress::dedupe_key_for(
                little_monkey_lib::channels::ingress::ConversationSource::Mobile,
                &session_id,
                &first_key,
            );
            let row = store
                .ingress_turn_by_dedupe_key(&dedupe)
                .expect("ingress lookup")
                .expect("a spoken turn is an ordinary durable turn");
            assert_eq!(row.source_account_id, session_id);
        }

        // The model answers, and the first sentence is spoken before the run
        // completes — incremental synthesis, not a wait for the whole answer.
        let recorder = queue.recorder(&first_key).expect("a run for the turn");
        emit_delta(&recorder, "The deploy finished. ");
        let delta = read_until(&mut socket, "assistant_delta").await;
        assert_eq!(delta["text"], "The deploy finished. ");
        let audio = read_until(&mut socket, "output_audio").await;
        assert_eq!(audio["media_type"], "audio/wav");
        assert!(
            !speech.spoken.lock().unwrap().is_empty(),
            "a sentence is synthesized while the run is still going"
        );

        // Talking over it: the audio that interrupts is the next utterance.
        sequence += 1;
        socket
            .send(talk_frame(
                &session_id,
                &generation,
                sequence,
                serde_json::json!({
                    "type": "audio",
                    "audio_sequence": 2,
                    "media_type": "audio/webm;codecs=opus",
                    "audio_base64": STANDARD.encode(b"second utterance bytes"),
                    "last": true,
                    "utterance_id": "utt-second",
                }),
            ))
            .await
            .unwrap();

        let second = read_until(&mut socket, "transcript").await;
        assert_eq!(
            second["text"], "stop and tell me about staging",
            "the interrupting words become the next turn instead of being thrown away"
        );
        assert_eq!(
            queue.accepted.lock().unwrap().len(),
            2,
            "two spoken turns, two durable turns"
        );

        // Withdrawing the grant closes the conversation rather than waiting for
        // the device to say something.
        revoke(&api, &device_id, DeviceCapability::VoiceStream);
        let closed = read_until_closed(&mut socket).await;
        assert!(
            closed,
            "a revoked voice_stream ends the socket without another frame from the device"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A spoken turn that the queue refuses does not leave a row claiming to
    /// be queued.**
    ///
    /// The transcript row and the durable turn live in two different SQLite
    /// files, so they cannot be one transaction: the row is written first and
    /// the turn is queued after. When that second step fails, the row is the
    /// only thing left, and a row stuck at `queued` is worse than a lost one —
    /// the reply materializer skips it forever (its job never existed), so the
    /// operator sees a question of theirs waiting for an answer that no part of
    /// the system is going to produce.
    #[test]
    fn a_spoken_turn_the_queue_refuses_settles_its_row_instead_of_stranding_it() {
        use super::super::talk::TalkTurns;

        struct RefusingQueue;
        impl MobileChatQueue for RefusingQueue {
            fn queue_chat(&self, _: &str, _: &str, _: &str) -> Result<String, String> {
                Err("the daemon queue is not accepting work".to_string())
            }
            fn chat_run_id(&self, _: &str, _: &str) -> Result<Option<String>, String> {
                Ok(None)
            }
        }

        let (root, api, _secrets, device_id, _secret) = fixture();
        let api = api.with_mobile_chat(Arc::new(RefusingQueue));
        let turns = TalkSessionTurns::new(
            api.clone(),
            &TalkSocketAuthorization {
                device_id: device_id.clone(),
                signed_request_sha256: "a".repeat(64),
                session_id: "mobile-session".to_string(),
                session_generation: "generation-one".to_string(),
            },
        );

        let refused = turns.submit("mobile-session", "talk-generation-1", "what is the status");
        assert!(
            refused.is_err(),
            "the caller is told the turn did not queue"
        );

        let store = api.store.lock().unwrap();
        let messages = store.mobile_messages("mobile-session", 10).unwrap();
        assert_eq!(messages.len(), 1, "the transcript keeps what was said");
        assert_eq!(
            messages[0].task_state, "failed",
            "a turn nothing queued must not sit at 'queued' forever"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **The security audit sees a real Talk socket, not a fabricated row.**
    ///
    /// `docs/paired-devices.md` promises that an open Talk socket "shows up
    /// there as a running `voice_stream` command, like any other capture in
    /// flight". Until this test that promise was checked by handing the audit a
    /// hand-built `DeviceCommandSnapshot` — which would have passed just as
    /// happily with the entire Talk path deleted, and did pass while a live
    /// socket wrote nothing anywhere.
    ///
    /// So: open a real admitted socket, then run the *production* device-state
    /// reader against the same store and ask the real audit what it sees.
    #[tokio::test]
    async fn an_open_talk_socket_is_a_capture_the_security_audit_can_see() {
        use futures_util::SinkExt;

        let (root, api, _secrets, device_id, secret) = fixture();
        grant(
            &api,
            &device_id,
            &[
                DeviceCapability::VoiceStream,
                DeviceCapability::MicrophoneCapture,
            ],
        );
        advertise(
            &api,
            &device_id,
            &secret,
            1,
            &[
                DeviceCapability::VoiceStream,
                DeviceCapability::MicrophoneCapture,
            ],
            &[
                (DeviceCapability::VoiceStream, OsPermission::Granted),
                (DeviceCapability::MicrophoneCapture, OsPermission::Granted),
            ],
        );
        let paths = DaemonPaths::under(&root);
        let speech = Arc::new(ScriptedSpeech {
            transcripts: Mutex::new(Default::default()),
            heard_bytes: Mutex::new(Vec::new()),
            spoken: Mutex::new(Vec::new()),
        });
        let api = api
            .with_mobile_chat(Arc::new(IngressTalkQueue::new(paths.clone())))
            .with_talk_speech(speech);

        // Nothing is listening yet.
        assert!(
            !capture_in_flight(&paths),
            "an idle runner reports no capture"
        );

        let address = spawn_talk_server(api.clone()).await;
        let session_id = format!("mobile-{device_id}");
        let (mut socket, generation) =
            open_talk_socket(&api, &device_id, &secret, 2, &session_id, address).await;
        socket
            .send(talk_frame(
                &session_id,
                &generation,
                1,
                serde_json::json!({
                    "type": "hello",
                    "media_type": "audio/webm;codecs=opus",
                    "sample_rate_hz": 48_000,
                    "channels": 1,
                }),
            ))
            .await
            .unwrap();
        // The `ready` frame proves the session is running, so the registration
        // that happens before it has already landed.
        let _ = read_until(&mut socket, "ready").await;

        assert!(
            capture_in_flight(&paths),
            "an open Talk socket is a voice_stream capture in flight"
        );

        // Withdrawing the grant closes the socket, and the capture clears with
        // it rather than outliving the authority that allowed it.
        //
        // Nothing else is sent. A device that is listening has no reason to
        // send anything, and a session that only noticed the withdrawal when
        // the next frame arrived would hold this microphone open until the idle
        // deadline — fifteen minutes of capture on a grant that is gone. So the
        // close below is on the runner's own clock, or it does not happen.
        revoke(&api, &device_id, DeviceCapability::VoiceStream);
        assert!(read_until_closed(&mut socket).await);
        for _ in 0..100 {
            if !capture_in_flight(&paths) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            !capture_in_flight(&paths),
            "a closed Talk socket leaves no capture claiming to be open"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// What the real audit says about this daemon root, right now.
    fn capture_in_flight(paths: &DaemonPaths) -> bool {
        let mut runtime = little_monkey_lib::security_doctor::SecurityRuntimeSnapshot::default();
        crate::security_cli::collect_device_state_at(&mut runtime, paths);
        let report = little_monkey_lib::security_doctor::run_security_audit(
            &little_monkey_lib::security_doctor::SecurityAuditRequest {
                app_data_dir: paths.root.clone(),
                workspace: None,
                deep: false,
                fix: false,
                runtime,
            },
        )
        .expect("the audit runs");
        report
            .findings
            .iter()
            .any(|finding| finding.id == "devices.capture_in_flight")
    }

    async fn spawn_talk_server(api: RemoteApi) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let api = api.clone();
                tokio::spawn(async move {
                    // Production's own connection path, so a regression there —
                    // dropping `with_upgrades`, say — fails these tests rather
                    // than only the phone.
                    let _ = super::super::server::serve_upgradable(
                        hyper_util::rt::TokioIo::new(stream),
                        api,
                    )
                    .await;
                });
            }
        });
        address
    }

    /// Mints a ticket the ordinary signed way and spends it on a real socket.
    async fn open_talk_socket(
        api: &RemoteApi,
        device_id: &str,
        secret: &[u8],
        sequence: u64,
        session_id: &str,
        address: std::net::SocketAddr,
    ) -> (
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        String,
    ) {
        let body = serde_json::to_vec(&serde_json::json!({
            "protocol_version": super::super::protocol::TALK_PROTOCOL_VERSION,
            "session_id": session_id,
        }))
        .unwrap();
        let now_ms = crate::daemon::remote::now_ms_public().unwrap();
        let issued = api.handle(
            signed_at(
                device_id,
                secret,
                sequence,
                &format!("cmd-talk-ticket-{sequence}"),
                "POST",
                "/v1/remote/device/talk/ticket",
                &body,
                now_ms,
            ),
            now_ms,
        );
        assert_eq!(issued.status, 201, "the grant admits a ticket");
        let ticket: serde_json::Value = serde_json::from_slice(&issued.body).unwrap();
        let url = format!(
            "ws://{address}{}?ticket={}",
            ticket["websocket_path"].as_str().unwrap(),
            ticket["ticket"].as_str().unwrap()
        );
        let (socket, response) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("the ticket admits a real WebSocket");
        assert_eq!(response.status().as_u16(), 101);
        (
            socket,
            ticket["session_generation"].as_str().unwrap().to_string(),
        )
    }

    /// Reads server frames until one of `kind` arrives, or the socket ends.
    async fn read_until(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        kind: &str,
    ) -> serde_json::Value {
        use futures_util::StreamExt;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let message = tokio::time::timeout_at(deadline, socket.next())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for a '{kind}' frame"));
            let Some(Ok(message)) = message else {
                panic!("the socket closed while waiting for a '{kind}' frame");
            };
            let Ok(text) = message.into_text() else {
                continue;
            };
            let Ok(frame) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            if frame["type"] == kind {
                return frame;
            }
        }
    }

    async fn read_until_closed(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> bool {
        use futures_util::StreamExt;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut saw_revocation = false;
        loop {
            let message = match tokio::time::timeout_at(deadline, socket.next()).await {
                Ok(message) => message,
                Err(_) => return false,
            };
            match message {
                None => return saw_revocation,
                Some(Ok(message)) => {
                    if let Ok(text) = message.into_text() {
                        if text.contains("capability_revoked") {
                            saw_revocation = true;
                        }
                    }
                }
                Some(Err(_)) => return saw_revocation,
            }
        }
    }

    fn emit_delta(recorder: &DurableRunRecorder, text: &str) {
        use crate::durable_run::CliRunEventSink;
        recorder
            .emit(RunEvent::ModelDelta {
                message_id: "talk-answer".to_string(),
                channel: OutputChannel::Assistant,
                text: text.to_string(),
            })
            .expect("append an assistant delta");
    }

    /// Withdraws one capability and leaves the rest of the pairing intact —
    /// which is what an operator revoking "may hear the room" actually does.
    fn revoke(api: &RemoteApi, device_id: &str, capability: DeviceCapability) {
        let mut store = api.store.lock().unwrap();
        let mut capabilities = store.device(device_id).unwrap().unwrap().capabilities;
        capabilities.remove(&capability);
        store
            .set_device_capabilities(device_id, &capabilities, 2_000)
            .expect("revoke");
    }

    /// A whole voice stream over the signed plane: leased, started, audio
    /// posted in order, stopped by an operator, closed by the device.
    ///
    /// The two properties worth having a test for are both about what happens
    /// when the link is unreliable. A chunk delivered twice must be stored once
    /// — otherwise a phone on a bad connection produces stuttering audio that
    /// nothing downstream can detect. And an operator's stop must reach the
    /// microphone, which it does on the answer to a chunk the device is already
    /// posting rather than on a poll it might not make.
    #[test]
    fn a_voice_stream_survives_a_retry_and_stops_when_an_operator_says_so() {
        let (root, api, _secrets, device_id, secret) = fixture();
        grant(
            &api,
            &device_id,
            &[
                DeviceCapability::MicrophoneCapture,
                DeviceCapability::VoiceStream,
            ],
        );
        assert_eq!(
            advertise(
                &api,
                &device_id,
                &secret,
                1,
                &[
                    DeviceCapability::MicrophoneCapture,
                    DeviceCapability::VoiceStream
                ],
                &[
                    (DeviceCapability::MicrophoneCapture, OsPermission::Granted),
                    (DeviceCapability::VoiceStream, OsPermission::Granted),
                ],
            )
            .status,
            200
        );

        let session_id = "vs-testsessionidentifier".to_string();
        let command_id = {
            let mut store = api.store.lock().unwrap();
            let command = store
                .enqueue_device_command(
                    &DeviceCommandRequest {
                        device_id: device_id.clone(),
                        capability: DeviceCapability::VoiceStream,
                        arguments: serde_json::json!({
                            "session_id": session_id,
                            "duration_ms": 60_000,
                            "chunk_ms": 1_000,
                        }),
                        source_run_id: None,
                        source_session_id: None,
                        source_tool_call_id: None,
                        invocation_id: None,
                        expires_at_ms: 300_000,
                    },
                    2_000,
                )
                .unwrap();
            store
                .open_voice_session(
                    &session_id,
                    &device_id,
                    &command.command_id,
                    None,
                    None,
                    200_000,
                    2_000,
                )
                .unwrap();
            command.command_id
        };

        // Lease and start, exactly as a photograph would.
        let leased = api.handle(
            signed(
                &device_id,
                &secret,
                2,
                "cmd-vlease",
                "GET",
                "/v1/remote/device/commands/next",
                b"",
            ),
            2_000,
        );
        assert_eq!(leased.status, 200);
        let start_path = format!("/v1/remote/device/commands/{command_id}/start");
        assert_eq!(
            api.handle(
                signed(
                    &device_id,
                    &secret,
                    3,
                    "cmd-vstart",
                    "POST",
                    &start_path,
                    b"{}"
                ),
                2_000,
            )
            .status,
            200
        );

        let chunk_path = format!("/v1/remote/device/voice/{session_id}/chunk");
        let chunk = |sequence: u64, audio: &[u8], first: bool| {
            serde_json::to_vec(&VoiceChunkRequest {
                protocol_version: REMOTE_PROTOCOL_VERSION,
                sequence,
                audio_base64: STANDARD.encode(audio),
                media_type: first.then(|| "audio/webm".to_string()),
                last: false,
            })
            .unwrap()
        };
        let first = api.handle(
            signed(
                &device_id,
                &secret,
                4,
                "cmd-vchunk-0",
                "POST",
                &chunk_path,
                &chunk(0, b"opus-one", true),
            ),
            2_100,
        );
        assert_eq!(first.status, 200);
        let first: serde_json::Value = serde_json::from_slice(&first.body).unwrap();
        assert_eq!(first["accepted"], serde_json::json!(true));
        assert_eq!(first["next_sequence"], serde_json::json!(1));
        assert_eq!(first["stop"], serde_json::json!(false));

        // The device's reply was lost, so it sends chunk 0 again. The runner
        // already holds it: the answer says so, and nothing is appended.
        let retry = api.handle(
            signed(
                &device_id,
                &secret,
                5,
                "cmd-vchunk-0b",
                "POST",
                &chunk_path,
                &chunk(0, b"opus-one", false),
            ),
            2_200,
        );
        let retry: serde_json::Value = serde_json::from_slice(&retry.body).unwrap();
        assert_eq!(retry["accepted"], serde_json::json!(false));
        assert_eq!(retry["bytes"], serde_json::json!(8));

        // An operator stops the stream. The device has not asked for anything
        // since, so this is the moment nothing has told it yet.
        api.store
            .lock()
            .unwrap()
            .request_device_cancel(&command_id, 2_300)
            .unwrap();

        let second = api.handle(
            signed(
                &device_id,
                &secret,
                6,
                "cmd-vchunk-1",
                "POST",
                &chunk_path,
                &chunk(1, b"opus-two", false),
            ),
            2_400,
        );
        let second: serde_json::Value = serde_json::from_slice(&second.body).unwrap();
        assert_eq!(
            second["stop"],
            serde_json::json!(true),
            "an operator's stop must reach the microphone on the next chunk"
        );
        // Still accepted: the audio already recorded is not thrown away just
        // because someone asked for the stream to end.
        assert_eq!(second["accepted"], serde_json::json!(true));

        let close_path = format!("/v1/remote/device/voice/{session_id}/close");
        let closed = api.handle(
            signed(
                &device_id,
                &secret,
                7,
                "cmd-vclose",
                "POST",
                &close_path,
                br#"{"protocol_version":1}"#,
            ),
            2_500,
        );
        assert_eq!(closed.status, 200);
        let closed: serde_json::Value = serde_json::from_slice(&closed.body).unwrap();
        assert_eq!(closed["state"], serde_json::json!("closed"));
        assert_eq!(closed["chunks"], serde_json::json!(2));

        // Exactly the bytes that were recorded, once each, in order.
        assert_eq!(
            std::fs::read(super::super::voice::audio_path(
                &api.paths.root,
                &session_id
            ))
            .unwrap(),
            b"opus-oneopus-two"
        );

        // A closed session takes nothing more.
        let late = api.handle(
            signed(
                &device_id,
                &secret,
                8,
                "cmd-vchunk-late",
                "POST",
                &chunk_path,
                &chunk(2, b"opus-three", false),
            ),
            2_600,
        );
        assert_eq!(late.status, 409);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The end-to-end shape task 06 is judged on: a device advertises, a
    /// command is queued, it is leased exactly once, started once, and its
    /// result comes back with an artifact.
    #[test]
    fn a_device_receives_a_queued_command_exactly_once_and_returns_its_artifact() {
        let (root, api, _secrets, device_id, secret) = fixture();
        grant(&api, &device_id, &[DeviceCapability::CameraCapture]);
        assert_eq!(
            advertise(
                &api,
                &device_id,
                &secret,
                1,
                &[DeviceCapability::CameraCapture],
                &[(DeviceCapability::CameraCapture, OsPermission::Granted)],
            )
            .status,
            200
        );

        let queued = api
            .store
            .lock()
            .unwrap()
            .enqueue_device_command(
                &DeviceCommandRequest {
                    device_id: device_id.clone(),
                    capability: DeviceCapability::CameraCapture,
                    arguments: serde_json::json!({ "position": "back" }),
                    source_run_id: Some("run-one".into()),
                    source_session_id: None,
                    source_tool_call_id: Some("call-1".into()),
                    invocation_id: None,
                    expires_at_ms: 300_000,
                },
                2_000,
            )
            .unwrap();

        let leased = api.handle(
            signed(
                &device_id,
                &secret,
                2,
                "cmd-lease-1",
                "GET",
                "/v1/remote/device/commands/next",
                b"",
            ),
            2_000,
        );
        assert_eq!(leased.status, 200);
        let command: DeviceCommand = serde_json::from_slice(&leased.body).unwrap();
        assert_eq!(command.command_id, queued.command_id);
        assert_eq!(command.capability, DeviceCapability::CameraCapture);

        // A second connection finds nothing: the command is leased, not shared.
        let empty = api.handle(
            signed(
                &device_id,
                &secret,
                3,
                "cmd-lease-2",
                "GET",
                "/v1/remote/device/commands/next",
                b"",
            ),
            2_000,
        );
        assert_eq!(empty.status, 204);

        let start_path = format!("/v1/remote/device/commands/{}/start", command.command_id);
        let started = api.handle(
            signed(
                &device_id,
                &secret,
                4,
                "cmd-start-1",
                "POST",
                &start_path,
                b"{}",
            ),
            2_000,
        );
        assert_eq!(started.status, 200);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&started.body).unwrap()["started"],
            serde_json::json!(true)
        );
        // The reconnect case: the device retries `start` because it lost the
        // reply. It must be told the action already began, not allowed to
        // repeat it.
        let again = api.handle(
            signed(
                &device_id,
                &secret,
                5,
                "cmd-start-2",
                "POST",
                &start_path,
                b"{}",
            ),
            2_000,
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&again.body).unwrap()["started"],
            serde_json::json!(false),
            "a reconnecting device must not perform the physical action twice"
        );

        let result = DeviceCommandResult {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            outcome: DeviceCommandState::Succeeded,
            result: Some(serde_json::json!({ "width": 4, "height": 3 })),
            artifact_base64: Some(STANDARD.encode(b"jpeg-bytes")),
            artifact_media_type: Some("image/jpeg".into()),
            artifact_sha256: None,
            error: None,
            execution_id: None,
        };
        let result_path = format!("/v1/remote/device/commands/{}/result", command.command_id);
        let reported = api.handle(
            signed(
                &device_id,
                &secret,
                6,
                "cmd-result-1",
                "POST",
                &result_path,
                &serde_json::to_vec(&result).unwrap(),
            ),
            2_000,
        );
        assert_eq!(reported.status, 200);
        let stored = api
            .store
            .lock()
            .unwrap()
            .device_command(&command.command_id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, DeviceCommandState::Succeeded);
        let artifact = stored.artifact.unwrap();
        assert_eq!(artifact.bytes, 10);
        assert_eq!(artifact.media_type, "image/jpeg");
        assert!(root
            .join("daemon/device-artifacts")
            .join(&command.command_id)
            .exists());
        let _ = std::fs::remove_dir_all(root);
    }

    /// One running command, and a terminal report to race against itself.
    ///
    /// Returns the fixture, the device record the commit path needs, and the
    /// command id — leased, started, and owned by `exec-race-0001`.
    fn running_command_fixture() -> (
        PathBuf,
        RemoteApi,
        crate::daemon::remote::store::DeviceRecord,
        String,
    ) {
        command_fixture(Stage::Running(Some("exec-race-0001")))
    }

    /// How far along its lifecycle a fixture's command has travelled.
    #[derive(Clone, Copy)]
    enum Stage {
        Queued,
        Leased,
        /// Started, naming an execution — or, with `None`, started by a build
        /// that had no execution identity to give.
        Running(Option<&'static str>),
    }

    fn command_fixture(
        stage: Stage,
    ) -> (
        PathBuf,
        RemoteApi,
        crate::daemon::remote::store::DeviceRecord,
        String,
    ) {
        let (root, api, _secrets, device_id, secret) = fixture();
        grant(&api, &device_id, &[DeviceCapability::CameraCapture]);
        advertise(
            &api,
            &device_id,
            &secret,
            1,
            &[DeviceCapability::CameraCapture],
            &[(DeviceCapability::CameraCapture, OsPermission::Granted)],
        );
        let queued = api
            .store
            .lock()
            .unwrap()
            .enqueue_device_command(
                &DeviceCommandRequest {
                    device_id: device_id.clone(),
                    capability: DeviceCapability::CameraCapture,
                    arguments: serde_json::json!({ "position": "back" }),
                    source_run_id: Some("run-one".into()),
                    source_session_id: None,
                    source_tool_call_id: Some("call-race".into()),
                    invocation_id: None,
                    expires_at_ms: 300_000,
                },
                2_000,
            )
            .unwrap();
        if !matches!(stage, Stage::Queued) {
            let leased = api.handle(
                signed(
                    &device_id,
                    &secret,
                    2,
                    "cmd-race-lease",
                    "GET",
                    "/v1/remote/device/commands/next",
                    b"",
                ),
                2_000,
            );
            assert_eq!(leased.status, 200);
        }
        if let Stage::Running(execution_id) = stage {
            let body = match execution_id {
                Some(value) => format!(r#"{{"execution_id":"{value}"}}"#).into_bytes(),
                None => b"{}".to_vec(),
            };
            let start_path = format!("/v1/remote/device/commands/{}/start", queued.command_id);
            let started = api.handle(
                signed(
                    &device_id,
                    &secret,
                    3,
                    "cmd-race-start",
                    "POST",
                    &start_path,
                    &body,
                ),
                2_000,
            );
            assert_eq!(started.status, 200);
        }
        let device = api
            .store
            .lock()
            .unwrap()
            .device(&device_id)
            .unwrap()
            .unwrap();
        (root, api, device, queued.command_id)
    }

    fn camera_report(bytes: &[u8], execution_id: &str) -> Vec<u8> {
        report_body(bytes, Some(execution_id))
    }

    fn report_body(bytes: &[u8], execution_id: Option<&str>) -> Vec<u8> {
        serde_json::to_vec(&DeviceCommandResult {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            outcome: DeviceCommandState::Succeeded,
            result: Some(serde_json::json!({ "bytes": bytes.len() })),
            artifact_base64: Some(STANDARD.encode(bytes)),
            artifact_media_type: Some("image/jpeg".into()),
            artifact_sha256: Some(sha256_hex(bytes)),
            error: None,
            execution_id: execution_id.map(str::to_string),
        })
        .unwrap()
    }

    fn artifact_path(root: &std::path::Path, command_id: &str) -> PathBuf {
        root.join("daemon/device-artifacts").join(command_id)
    }

    /// `/start` is the authorization boundary, so a terminal report is only
    /// meaningful from the far side of it.
    ///
    /// Before this, an authenticated device could take a command straight from
    /// `queued` — or from `leased`, without ever asking — to `succeeded`, which
    /// skips every check that boundary exists to make: the grant, the readiness,
    /// the cancellation, and the record of *which* execution is answering.
    #[test]
    fn a_result_for_a_command_that_was_never_started_is_refused() {
        for (stage, expected) in [
            (Stage::Queued, DeviceCommandState::Queued),
            (Stage::Leased, DeviceCommandState::Leased),
        ] {
            let (root, api, device, command_id) = command_fixture(stage);
            let body = camera_report(b"never-authorized", "exec-invented-1");
            let (status, message) = api
                .device_command_result(&body, &device, &command_id, 2_100)
                .expect_err("a command that was never started has no result to report");
            assert_eq!(status, 409, "{message}");
            assert_eq!(
                api.store
                    .lock()
                    .unwrap()
                    .device_command(&command_id)
                    .unwrap()
                    .unwrap()
                    .state,
                expected,
                "a refused report must not move the command"
            );
            assert!(
                !artifact_path(&root, &command_id).exists(),
                "a refused report must not publish an artifact"
            );
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// A running command answers only to the execution that holds it.
    ///
    /// Including the silent case: an omitted `execution_id` is the one form any
    /// second execution can always produce, so it is refused exactly as firmly
    /// as a wrong one.
    #[test]
    fn a_running_command_accepts_only_its_own_executions_result() {
        for (offered, accepted) in [
            (None, false),
            (Some("exec-somebody-el"), false),
            (Some("exec-race-0001"), true),
        ] {
            let (root, api, device, command_id) = running_command_fixture();
            let body = report_body(b"jpeg-bytes", offered);
            let answer = api.device_command_result(&body, &device, &command_id, 2_100);
            let state = api
                .store
                .lock()
                .unwrap()
                .device_command(&command_id)
                .unwrap()
                .unwrap()
                .state;
            if accepted {
                assert_eq!(answer.expect("the holder's result is accepted").0, 200);
                assert_eq!(state, DeviceCommandState::Succeeded);
                assert!(artifact_path(&root, &command_id).exists());
            } else {
                let (status, message) = answer.expect_err("only the holder may report");
                assert_eq!(status, 409, "{message}");
                assert_eq!(
                    state,
                    DeviceCommandState::Running,
                    "a refused report must leave the command running"
                );
                assert!(
                    !artifact_path(&root, &command_id).exists(),
                    "a refused report must not publish an artifact"
                );
            }
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// A command started before execution identities existed stays completable.
    ///
    /// Both ends have to be silent about it. An id offered against a command
    /// that never recorded one proves nothing, and accepting it would let a
    /// second execution answer for the first.
    #[test]
    fn a_command_started_without_an_execution_identity_completes_without_one() {
        let (root, api, device, command_id) = command_fixture(Stage::Running(None));
        let (status, message) = api
            .device_command_result(
                &report_body(b"jpeg-bytes", Some("exec-invented-1")),
                &device,
                &command_id,
                2_100,
            )
            .expect_err("an invented identity proves nothing about a command that recorded none");
        assert_eq!(status, 409, "{message}");
        assert!(!artifact_path(&root, &command_id).exists());

        let (status, _, _) = api
            .device_command_result(
                &report_body(b"jpeg-bytes", None),
                &device,
                &command_id,
                2_100,
            )
            .expect("a legacy start must stay completable");
        assert_eq!(status, 200);
        assert_eq!(
            api.store
                .lock()
                .unwrap()
                .device_command(&command_id)
                .unwrap()
                .unwrap()
                .state,
            DeviceCommandState::Succeeded
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// What the runner holds, as two facts that must agree: the digest in the
    /// row, and the bytes on disk.
    fn stored_artifact(
        root: &std::path::Path,
        api: &RemoteApi,
        command_id: &str,
    ) -> (String, String) {
        let stored = api
            .store
            .lock()
            .unwrap()
            .device_command(command_id)
            .unwrap()
            .unwrap();
        let bytes = std::fs::read(root.join("daemon/device-artifacts").join(command_id))
            .expect("a terminal record must never name bytes that are not there");
        (stored.artifact.unwrap().sha256, sha256_hex(&bytes))
    }

    /// The same result delivered twice at the same moment.
    ///
    /// The device cannot tell a lost response from a lost request, so it
    /// retries — and nothing stops the retry overlapping the original. Both
    /// deliveries have to be acknowledged, and between them they may leave only
    /// one artifact.
    #[test]
    fn two_identical_terminal_reports_racing_each_other_commit_once() {
        for round in 0..8 {
            let (root, api, device, command_id) = running_command_fixture();
            let body = camera_report(b"jpeg-bytes-identical", "exec-race-0001");
            let gate = std::sync::Barrier::new(2);
            let (first, second) = std::thread::scope(|scope| {
                let one = scope.spawn(|| {
                    gate.wait();
                    api.device_command_result(&body, &device, &command_id, 2_100)
                });
                let two = scope.spawn(|| {
                    gate.wait();
                    api.device_command_result(&body, &device, &command_id, 2_100)
                });
                (one.join().unwrap(), two.join().unwrap())
            });
            for answer in [&first, &second] {
                let (status, _, _) = answer.as_ref().unwrap_or_else(|error| {
                    panic!("round {round}: an identical retry was refused: {error:?}")
                });
                assert_eq!(*status, 200);
            }
            let (row, file) = stored_artifact(&root, &api, &command_id);
            assert_eq!(row, sha256_hex(b"jpeg-bytes-identical"));
            assert_eq!(row, file, "round {round}: the row and the bytes disagree");
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// Two *different* results for one physical action, delivered at the same
    /// moment.
    ///
    /// Exactly one may win, and the loser must change nothing — not the row,
    /// not the digest, and above all not the bytes. Publishing the artifact
    /// outside the commit is what used to make "the row says A, the file holds
    /// B" reachable.
    #[test]
    fn a_conflicting_terminal_report_racing_the_winner_changes_nothing() {
        for round in 0..8 {
            let (root, api, device, command_id) = running_command_fixture();
            let first_body = camera_report(b"jpeg-bytes-first", "exec-race-0001");
            let second_body = camera_report(b"jpeg-bytes-second", "exec-race-0001");
            let gate = std::sync::Barrier::new(2);
            let (first, second) = std::thread::scope(|scope| {
                let one = scope.spawn(|| {
                    gate.wait();
                    api.device_command_result(&first_body, &device, &command_id, 2_100)
                });
                let two = scope.spawn(|| {
                    gate.wait();
                    api.device_command_result(&second_body, &device, &command_id, 2_100)
                });
                (one.join().unwrap(), two.join().unwrap())
            });
            let accepted = [&first, &second]
                .iter()
                .filter(|answer| answer.is_ok())
                .count();
            assert_eq!(
                accepted, 1,
                "round {round}: exactly one report is authoritative"
            );
            for answer in [&first, &second] {
                if let Err((status, _)) = answer {
                    assert_eq!(
                        *status, 409,
                        "round {round}: the loser is refused, not failed"
                    );
                }
            }
            let (row, file) = stored_artifact(&root, &api, &command_id);
            let winner = if first.is_ok() {
                b"jpeg-bytes-first".as_slice()
            } else {
                b"jpeg-bytes-second".as_slice()
            };
            assert_eq!(row, sha256_hex(winner));
            assert_eq!(
                row, file,
                "round {round}: the loser's bytes replaced the winner's under the winner's digest"
            );
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// Authority is re-checked at the moment the command is handed over, not
    /// only when it was queued — an OS permission the user switched off in
    /// between must stop it, with a reason the waiting run can read.
    #[test]
    fn a_permission_withdrawn_after_queueing_fails_the_command_at_lease_time() {
        let (root, api, _secrets, device_id, secret) = fixture();
        grant(&api, &device_id, &[DeviceCapability::LocationRead]);
        advertise(
            &api,
            &device_id,
            &secret,
            1,
            &[DeviceCapability::LocationRead],
            &[(DeviceCapability::LocationRead, OsPermission::Granted)],
        );
        let queued = api
            .store
            .lock()
            .unwrap()
            .enqueue_device_command(
                &DeviceCommandRequest {
                    device_id: device_id.clone(),
                    capability: DeviceCapability::LocationRead,
                    arguments: serde_json::json!({}),
                    source_run_id: None,
                    source_session_id: None,
                    source_tool_call_id: None,
                    invocation_id: None,
                    expires_at_ms: 300_000,
                },
                2_000,
            )
            .unwrap();
        // The user revokes location in the OS and the app re-advertises.
        advertise(
            &api,
            &device_id,
            &secret,
            2,
            &[DeviceCapability::LocationRead],
            &[(DeviceCapability::LocationRead, OsPermission::Denied)],
        );
        let leased = api.handle(
            signed(
                &device_id,
                &secret,
                3,
                "cmd-lease-1",
                "GET",
                "/v1/remote/device/commands/next",
                b"",
            ),
            2_000,
        );
        assert_eq!(leased.status, 204, "a denied capability yields no command");
        let stored = api
            .store
            .lock()
            .unwrap()
            .device_command(&queued.command_id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, DeviceCommandState::Failed);
        // Not a shrug: the reason names the axis that refused and what the
        // operator would do about it.
        let reason = stored.error.unwrap();
        assert!(
            reason.contains("denies") && reason.contains("system settings"),
            "the failure must say the OS denied it and where to fix it: {reason}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// Advertising is not authority: a device that claims every capability
    /// gains none it was not granted.
    #[test]
    fn advertising_a_capability_never_grants_it() {
        let (root, api, _secrets, device_id, secret) = fixture();
        let response = advertise(
            &api,
            &device_id,
            &secret,
            1,
            &[
                DeviceCapability::CameraCapture,
                DeviceCapability::ScreenCapture,
                DeviceCapability::MicrophoneCapture,
            ],
            &[
                (DeviceCapability::CameraCapture, OsPermission::Granted),
                (DeviceCapability::ScreenCapture, OsPermission::Granted),
                (DeviceCapability::MicrophoneCapture, OsPermission::Granted),
            ],
        );
        assert_eq!(response.status, 200);
        let state: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        let effective = state["effective"].as_array().unwrap();
        assert!(
            !effective
                .iter()
                .any(|value| value == "camera_capture" || value == "screen_capture"),
            "a self-declared capability must not become effective: {effective:?}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// The long poll waits for work and returns as soon as it exists, and it
    /// never holds a connection longer than the lease.
    #[tokio::test]
    async fn the_lease_long_poll_waits_for_work_and_gives_up_on_time() {
        let (root, api, _secrets, device_id, secret) = fixture();
        grant(&api, &device_id, &[DeviceCapability::NotificationPost]);
        advertise(
            &api,
            &device_id,
            &secret,
            1,
            &[DeviceCapability::NotificationPost],
            &[(DeviceCapability::NotificationPost, OsPermission::Granted)],
        );

        let started = std::time::Instant::now();
        let empty = api
            .handle_waiting(
                signed(
                    &device_id,
                    &secret,
                    2,
                    "cmd-poll-1",
                    "GET",
                    "/v1/remote/device/commands/next?wait_ms=1200",
                    b"",
                ),
                2_000,
            )
            .await;
        assert_eq!(empty.status, 204);
        let waited = started.elapsed();
        assert!(
            waited >= std::time::Duration::from_millis(1_000),
            "an empty poll must actually wait, not spin: {waited:?}"
        );
        assert!(
            waited < std::time::Duration::from_millis(10_000),
            "the poll must give up well inside the lease: {waited:?}"
        );

        // With work already queued, the same request returns immediately.
        api.store
            .lock()
            .unwrap()
            .enqueue_device_command(
                &DeviceCommandRequest {
                    device_id: device_id.clone(),
                    capability: DeviceCapability::NotificationPost,
                    arguments: serde_json::json!({ "title": "hi", "body": "there" }),
                    source_run_id: None,
                    source_session_id: None,
                    source_tool_call_id: None,
                    invocation_id: None,
                    expires_at_ms: 300_000,
                },
                2_000,
            )
            .unwrap();
        let started = std::time::Instant::now();
        let leased = api
            .handle_waiting(
                signed(
                    &device_id,
                    &secret,
                    3,
                    "cmd-poll-2",
                    "GET",
                    "/v1/remote/device/commands/next?wait_ms=20000",
                    b"",
                ),
                2_000,
            )
            .await;
        assert_eq!(leased.status, 200);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(2_000),
            "a queued command must not wait for the poll deadline"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// A revoked device gets nothing, including on the device plane.
    #[test]
    fn a_revoked_device_cannot_lease_or_report() {
        let (root, api, secrets, device_id, secret) = fixture();
        grant(&api, &device_id, &[DeviceCapability::DeviceInfo]);
        api.store
            .lock()
            .unwrap()
            .revoke_device(&device_id, "lost", 2_000, secrets.as_ref(), None)
            .unwrap();
        for (sequence, command, method, path) in [
            (1u64, "cmd-a", "GET", "/v1/remote/device/state"),
            (2, "cmd-b", "GET", "/v1/remote/device/commands/next"),
            (3, "cmd-c", "POST", "/v1/remote/device/surface"),
        ] {
            let response = api.handle(
                signed(&device_id, &secret, sequence, command, method, path, b"{}"),
                2_000,
            );
            assert_eq!(response.status, 401, "{path} answered a revoked device");
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
