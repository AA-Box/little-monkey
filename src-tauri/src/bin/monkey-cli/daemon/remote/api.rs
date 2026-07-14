use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use little_monkey_lib::artifact_store::ArtifactStore;
use little_monkey_lib::run_ledger::{RunLedger, StoredApproval, StoredRun};
use little_monkey_lib::run_protocol::{ModelTargetSnapshot, PermissionDecision, RunEvent};
use serde::Serialize;

use crate::daemon::ledger::SharedLedger;
use crate::daemon::store::{DaemonPaths, DaemonStore};
use crate::durable_run::{bounded_text, CliRunEventSink, DurableRunRecorder};

use super::protocol::{
    canonical_request, sha256_hex, ApprovalRequestBody, CancelRequestBody, PairAcceptRequest,
    RemoteAction, RemoteHostConfig, RemoteScopes, RunSummary, SignedRequestHeaders,
    MAX_REMOTE_BODY_BYTES, REMOTE_PROTOCOL_VERSION,
};
use super::store::{CommandReservation, KeyringRemoteSecrets, RemoteSecretStore, RemoteStore};

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
}

impl Clone for RemoteApi {
    fn clone(&self) -> Self {
        Self {
            paths: self.paths.clone(),
            host: self.host.clone(),
            store: Arc::clone(&self.store),
            secrets: Arc::clone(&self.secrets),
        }
    }
}

impl RemoteApi {
    pub fn production(paths: DaemonPaths, host: RemoteHostConfig) -> Result<Self, String> {
        let store = RemoteStore::open(&paths.root)?;
        Ok(Self {
            paths,
            host,
            store: Arc::new(Mutex::new(store)),
            secrets: Arc::new(KeyringRemoteSecrets),
        })
    }

    #[cfg(test)]
    pub fn injected(
        paths: DaemonPaths,
        host: RemoteHostConfig,
        store: RemoteStore,
        secrets: Arc<dyn RemoteSecretStore>,
    ) -> Self {
        Self {
            paths,
            host,
            store: Arc::new(Mutex::new(store)),
            secrets,
        }
    }

    pub fn handle(&self, request: ApiRequest, now_ms: u64) -> ApiResponse {
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
        let response = self.dispatch_authorized(
            &request,
            &device.scopes,
            &headers.device_id,
            &request_sha256,
            now_ms,
        );
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
        scopes: &RemoteScopes,
        device_id: &str,
        request_sha256: &str,
        now_ms: u64,
    ) -> ApiResponse {
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
            ("POST", ["v1", "remote", "kill"]) => require_action(scopes, RemoteAction::Kill)
                .and_then(|_| self.kill(device_id, now_ms)),
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

    fn kill(
        &self,
        device_id: &str,
        now_ms: u64,
    ) -> Result<(u16, serde_json::Value, Option<String>), (u16, String)> {
        let mut store = DaemonStore::open(&self.paths).map_err(internal)?;
        store.set_kill_switch(true).map_err(internal)?;
        let cancelled = store.request_cancel_all(now_ms).map_err(internal)?;
        Ok((
            202,
            serde_json::json!({
                "status":"kill_switch_engaged",
                "requested_by":device_id,
                "cancelled_runs":cancelled,
            }),
            None,
        ))
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

fn require_action(scopes: &RemoteScopes, action: RemoteAction) -> Result<(), (u16, String)> {
    if scopes.permits(action) {
        Ok(())
    } else {
        Err((403, format!("Remote action '{action:?}' is not paired")))
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
    use crate::daemon::remote::protocol::{sign_request, RemoteAction};
    use crate::daemon::remote::store::RemoteSecretStore;
    use crate::daemon::store::DaemonConfig;
    use crate::durable_run::DurableRunRecorder;

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

    fn fixture() -> (PathBuf, RemoteApi, Arc<FakeSecrets>, String, Vec<u8>) {
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
            actions: BTreeSet::from([
                RemoteAction::ViewRuns,
                RemoteAction::ViewEvents,
                RemoteAction::Approve,
                RemoteAction::Cancel,
            ]),
            run_ids: BTreeSet::from(["run-one".into()]),
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
        assert_eq!(first.status, 200);
        assert_eq!(first, replay);
        let approval = RunLedger::open(&DaemonPaths::under(&root).ledger_db)
            .unwrap()
            .load_approval("run-one", "approval-one")
            .unwrap()
            .unwrap();
        assert_eq!(approval.decision, Some(PermissionDecision::AllowOnce));
        let _ = std::fs::remove_dir_all(root);
    }
}
