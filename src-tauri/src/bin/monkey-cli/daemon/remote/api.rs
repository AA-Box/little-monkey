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
    canonical_request, effective_capabilities, legacy_capabilities, sha256_hex,
    ApprovalRequestBody, CancelRequestBody, DesktopControlActionRequest,
    DesktopControlStartRequest, DesktopControlStopRequest, DeviceCapability, DeviceCommand,
    DeviceCommandResult, DeviceCommandState, DeviceSurface, MigrationAcceptRequest,
    MigrationPreflightRequest, MigrationReceipt, PairAcceptRequest, RemoteAction, RemoteHostConfig,
    RemoteScopes, RunSummary, SignedRequestHeaders, DEVICE_LEASE_MS, MAX_REMOTE_BODY_BYTES,
    REMOTE_PROTOCOL_VERSION,
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
    /// Resolves the durable run id previously queued for `client_key`, if
    /// the job has one yet.
    fn chat_run_id(&self, client_key: &str) -> Result<Option<String>, String>;
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
    /// Where remote requests land in the unified subsystem event stream
    /// (roadmap K12).
    ///
    /// The remote node already keeps its own `remote_audit` table, but that
    /// table lives in its own database with no join to the run stream — which is
    /// the gap K12 names. This records the same requests where everything else
    /// can be read alongside them; it does not replace `remote_audit`, which
    /// holds the protocol-level denial detail this stream deliberately does not.
    audit: little_monkey_lib::subsystem_audit::SubsystemAudit,
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
            audit: self.audit.clone(),
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
            audit,
        })
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
            audit,
        }
    }

    /// Test builder: the injected API plus a fake chat queue, so the mobile
    /// chat contract is exercisable without a configured daemon.
    #[cfg(test)]
    pub fn with_mobile_chat(mut self, mobile_chat: Arc<dyn MobileChatQueue>) -> Self {
        self.mobile_chat = Some(mobile_chat);
        self
    }

    /// Test builder: the injected API plus a fake placement queue, so the K17
    /// placement contract is exercisable without a configured daemon.
    #[cfg(test)]
    pub fn with_placement(mut self, placement: Arc<dyn PlacementQueue>) -> Self {
        self.placement = Some(placement);
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
        let deadline_ms = match long_poll_wait_ms(&request) {
            Some(wait_ms) => wait_ms,
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
            if self.has_pending_device_command(&device_id, now_ms.saturating_add(elapsed)) {
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
            ("POST", ["v1", "remote", "device", "surface"]) => {
                self.device_surface_post(&request.body, device, now_ms)
            }
            ("GET", ["v1", "remote", "device", "state"]) => self.device_state(device),
            ("GET", ["v1", "remote", "device", "commands", "next"]) => {
                self.device_command_lease(device, now_ms)
            }
            ("POST", ["v1", "remote", "device", "commands", command_id, "start"]) => {
                self.device_command_start(device_id, command_id, now_ms)
            }
            ("POST", ["v1", "remote", "device", "commands", command_id, "result"]) => {
                self.device_command_result(&request.body, device, command_id, now_ms)
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
        // RunSpec contains only keychain references, never provider keys.
        Ok((
            200,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "run": summary,
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
        let value = runtime.start(
            device_id,
            &self.device_label(device_id),
            request.allowlist,
            request.batch_mode,
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
            let Some(run_id) = queue.chat_run_id(&message.message_id).map_err(internal)? else {
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
        let surface = store.device_surface(&device.device_id).map_err(internal)?;
        let effective = effective_capabilities(&device.capabilities, surface.as_ref());
        // Bounded: each iteration retires exactly one now-unauthorized command,
        // so this cannot spin.
        for _ in 0..64 {
            let Some(record) = store
                .lease_device_command(&device.device_id, DEVICE_LEASE_MS, now_ms)
                .map_err(internal)?
            else {
                return Ok((204, serde_json::json!({}), None));
            };
            if !effective.contains(&record.capability) {
                store
                    .complete_device_command(
                        &device.device_id,
                        &record.command_id,
                        DeviceCommandState::Failed,
                        None,
                        None,
                        Some(
                            "The capability this command needs is no longer granted, advertised \
                             or permitted by the device's operating system",
                        ),
                        now_ms,
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
    fn device_command_start(
        &self,
        device_id: &str,
        command_id: &str,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let started = self
            .locked_store()?
            .start_device_command(device_id, command_id, now_ms)
            .map_err(|error| (409, error))?;
        Ok((
            200,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "command_id": command_id,
                "started": started,
            }),
            Some(command_id.to_string()),
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
        let artifact = match (&result.artifact_base64, &result.artifact_media_type) {
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
                let directory = self.paths.root.join("device-artifacts");
                std::fs::create_dir_all(&directory).map_err(|error| {
                    internal(format!(
                        "Could not create device artifact directory: {error}"
                    ))
                })?;
                // The command id names the file, so a retried report overwrites
                // its own bytes and can never create a second artifact.
                std::fs::write(directory.join(command_id), &bytes).map_err(|error| {
                    internal(format!("Could not persist device artifact: {error}"))
                })?;
                Some(DeviceArtifact {
                    sha256: sha256_hex(&bytes),
                    bytes: bytes.len() as u64,
                    media_type: media_type.clone(),
                })
            }
            _ => None,
        };
        let record = self
            .locked_store()?
            .complete_device_command(
                &device.device_id,
                command_id,
                result.outcome,
                result.result.as_ref(),
                artifact.as_ref(),
                result.error.as_deref(),
                now_ms,
            )
            .map_err(|error| (409, error))?;
        Ok((
            200,
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "command_id": record.command_id,
                "state": record.state.as_str(),
            }),
            Some(record.command_id),
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

/// The `wait_ms` a lease request asked for, capped at the lease length, or
/// `None` when this request is not a lease at all.
fn long_poll_wait_ms(request: &ApiRequest) -> Option<u64> {
    if request.method != "GET" {
        return None;
    }
    let (path, query) = request
        .path_and_query
        .split_once('?')
        .map_or((request.path_and_query.as_str(), ""), |value| value);
    if path.trim_end_matches('/') != "/v1/remote/device/commands/next" {
        return None;
    }
    let wait_ms = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == "wait_ms")
        .and_then(|(_, value)| value.parse::<u64>().ok())
        .unwrap_or(0);
    (wait_ms > 0).then(|| wait_ms.min(DEVICE_LEASE_MS))
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
    serde_json::json!({
        "protocol_version": REMOTE_PROTOCOL_VERSION,
        "device_id": device.device_id,
        "device_name": device.device_name,
        "granted": granted,
        "advertised": surface.map(|surface| surface.capabilities.clone()),
        "os_permissions": surface.map(|surface| surface.permissions.clone()),
        "effective": effective_capabilities(&granted, surface),
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::path::PathBuf;

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
            let gate = arm
                .split_once("RemoteAction::")
                .map(|(_, tail)| format!("action:{}", variant(tail)))
                .or_else(|| {
                    arm.split_once("DeviceCapability::")
                        .map(|(_, tail)| format!("capability:{}", variant(tail)))
                })
                .unwrap_or_else(|| "self_service".to_string());
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
        let mut auth = SignedRequestHeaders {
            device_id: device_id.into(),
            secret_generation: 1,
            sequence,
            timestamp_ms: 2_000,
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

        fn chat_run_id(&self, client_key: &str) -> Result<Option<String>, String> {
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
            error: None,
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
        assert!(stored.error.unwrap().contains("no longer granted"));
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
