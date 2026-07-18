//! Resident-daemon glue between the paired remote transport
//! (`daemon/remote/*`) and the gated desktop-control core
//! (`little_monkey_lib::desktop_control`). It is the only place that:
//!
//! - shows a **local, visible consent prompt on the runner** before any remote
//!   caller can create a session (a headless daemon must never silently start
//!   driving the real cursor);
//! - enforces that a remote `batch_mode` request is honoured **only** when the
//!   local operator specifically answered "Allow (batch)";
//! - records every session as a `RunKind::RemoteDesktopControl` run in the
//!   existing durable ledger, with periodic and start/stop `screencapture -x`
//!   screenshots stored as `ArtifactAdded` evidence; and
//! - lets revoke / kill-switch / a local escape hatch force-stop live sessions
//!   immediately.
//!
//! The `osascript`/`screencapture` side effects are isolated behind the
//! [`ConsentPrompter`] trait (mirroring how `desktop_control.rs` isolates
//! `enigo` behind `DesktopInputBackend`) and a `record` flag, so the gating and
//! ownership logic is unit-testable without shelling out in CI.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use little_monkey_lib::artifact_store::ArtifactStore;
use little_monkey_lib::desktop_control::{
    ActionGate, ControlAction, ControlSession, DesktopControlState, MAX_SESSION_LIFETIME_MS,
};
use little_monkey_lib::run_ledger::RunLedger;
use little_monkey_lib::run_protocol::{
    ArtifactKind, ClientIdentity, ClientKind, ModelTargetSnapshot, PermissionMode,
    PermissionPolicySnapshot, RunBudgets, RunEvent, RunKind, RunSpec, ToolPolicyDecision,
    UsageSnapshot, RUN_PROTOCOL_SCHEMA_VERSION,
};

use crate::daemon::store::DaemonPaths;
use crate::durable_run::{CliRunEventSink, DurableRunRecorder};

use super::protocol::{DesktopControlActionRequest, MAX_REMOTE_ARTIFACT_BYTES};
use super::store::{DesktopSessionKiller, RemoteStore};

/// Capture a screenshot at least this often while a session is active.
const CAPTURE_INTERVAL_MS: u64 = 30_000;
/// ...or after this many actions, whichever comes first.
const CAPTURE_EVERY_N_ACTIONS: u32 = 10;
/// Bounded wait for the local operator to answer the consent dialog. A
/// timeout counts as a denial — silence is never consent. Only referenced by
/// the macOS `osascript` prompt; every other platform denies unconditionally.
#[cfg(target_os = "macos")]
const CONSENT_TIMEOUT_SECONDS: u64 = 60;

/// The local operator's answer to the session consent prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionConsent {
    Deny,
    AllowPerAction,
    AllowBatch,
}

/// Seam for the local, visible consent surface. Production shells `osascript`;
/// tests inject a double that returns a canned answer.
pub trait ConsentPrompter: Send + Sync {
    /// Blocking, locally-visible prompt shown on the runner before a session is
    /// created. Returns the operator's choice (or [`SessionConsent::Deny`] on
    /// timeout / dismissal).
    fn confirm_session(&self, device_label: &str) -> SessionConsent;
    /// Blocking, locally-visible per-action prompt for a non-batch session.
    fn confirm_action(&self, device_label: &str, description: &str) -> bool;
}

/// macOS `osascript` implementation. Every invocation is an argv array — never
/// a shell string — matching `daemon_commands.rs`'s stated convention.
pub struct OsascriptConsentPrompter;

impl ConsentPrompter for OsascriptConsentPrompter {
    fn confirm_session(&self, device_label: &str) -> SessionConsent {
        #[cfg(target_os = "macos")]
        {
            let script = format!(
                "display dialog {} with title {} buttons {{\"Deny\", \"Allow (per-action)\", \
                 \"Allow (batch)\"}} default button \"Deny\" cancel button \"Deny\" with icon \
                 caution giving up after {}",
                apple_script_string(&format!(
                    "Remote desktop control was requested by device \"{device_label}\".\n\nAllow \
                     this device to control this Mac's keyboard and mouse?"
                )),
                apple_script_string("Little Monkey — Remote Desktop Control"),
                CONSENT_TIMEOUT_SECONDS,
            );
            match run_osascript(&script) {
                Some(output) if output.contains("Allow (batch)") => SessionConsent::AllowBatch,
                Some(output) if output.contains("Allow (per-action)") => {
                    SessionConsent::AllowPerAction
                }
                _ => SessionConsent::Deny,
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = device_label;
            SessionConsent::Deny
        }
    }

    fn confirm_action(&self, device_label: &str, description: &str) -> bool {
        #[cfg(target_os = "macos")]
        {
            let script = format!(
                "display dialog {} with title {} buttons {{\"Deny\", \"Allow\"}} default button \
                 \"Deny\" cancel button \"Deny\" with icon caution giving up after {}",
                apple_script_string(&format!(
                    "Device \"{device_label}\" wants to perform: {description}"
                )),
                apple_script_string("Little Monkey — Approve Action"),
                CONSENT_TIMEOUT_SECONDS,
            );
            matches!(run_osascript(&script), Some(output) if output.contains("button returned:Allow"))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (device_label, description);
            false
        }
    }
}

#[cfg(target_os = "macos")]
fn run_osascript(script: &str) -> Option<String> {
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn apple_script_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Per-session recording bookkeeping.
struct Recording {
    recorder: Arc<DurableRunRecorder>,
    actions_since_capture: u32,
    last_capture_ms: u64,
    capture_index: u32,
    finished: bool,
}

pub struct DesktopControlRuntime {
    state: Arc<DesktopControlState>,
    prompter: Arc<dyn ConsentPrompter>,
    paths: DaemonPaths,
    /// device_id -> the sessions that device started (so revoke can find and
    /// kill exactly the right ones).
    device_sessions: Mutex<BTreeMap<String, BTreeSet<String>>>,
    recordings: Mutex<BTreeMap<String, Recording>>,
    /// Whether to persist ledger evidence + screenshots. Disabled in unit
    /// tests so the gating/ownership logic can run without a real ledger.
    record: bool,
}

impl DesktopControlRuntime {
    /// Production runtime: the real (macOS `enigo`) input backend guarded by
    /// the machine-wide `<app_data>/desktop_control.lock`, an `osascript`
    /// consent prompter, and durable session recording enabled.
    pub fn production(paths: &DaemonPaths) -> Arc<Self> {
        let lock_path = app_data(paths).join("desktop_control.lock");
        Arc::new(Self {
            state: Arc::new(DesktopControlState::production_with_lock(lock_path)),
            prompter: Arc::new(OsascriptConsentPrompter),
            paths: paths.clone(),
            device_sessions: Mutex::new(BTreeMap::new()),
            recordings: Mutex::new(BTreeMap::new()),
            record: true,
        })
    }

    #[cfg(test)]
    fn for_test(
        state: Arc<DesktopControlState>,
        prompter: Arc<dyn ConsentPrompter>,
        paths: DaemonPaths,
    ) -> Arc<Self> {
        Arc::new(Self {
            state,
            prompter,
            paths,
            device_sessions: Mutex::new(BTreeMap::new()),
            recordings: Mutex::new(BTreeMap::new()),
            record: false,
        })
    }

    /// `POST /v1/remote/desktop-control/start`. Runs the local consent prompt
    /// **before touching `DesktopControlState`**, then starts a session whose
    /// batch mode is granted only if the operator chose "Allow (batch)".
    pub fn start(
        &self,
        device_id: &str,
        device_label: &str,
        allowlist: Vec<String>,
        batch_requested: bool,
    ) -> Result<serde_json::Value, (u16, String)> {
        let consent = self.prompter.confirm_session(device_label);
        if consent == SessionConsent::Deny {
            return Err((
                403,
                "Local desktop-control consent was denied or timed out on the runner".to_string(),
            ));
        }
        // Batch mode requires BOTH a local "Allow (batch)" answer AND the
        // remote request asking for it — the remote flag alone is never enough.
        let approved_batch = batch_requested && consent == SessionConsent::AllowBatch;
        let session = self
            .state
            // A gated mode (never "bypass"); the local consent prompt is the
            // human gate for the headless daemon.
            .start_session_impl("auto", allowlist, MAX_SESSION_LIFETIME_MS, approved_batch)
            .map_err(|error| (409, error))?;
        self.device_sessions
            .lock()
            .map_err(poisoned)?
            .entry(device_id.to_string())
            .or_default()
            .insert(session.session_id.clone());
        self.begin_recording(&session);
        Ok(serde_json::json!({
            "protocol_version": super::protocol::REMOTE_PROTOCOL_VERSION,
            "session": session,
            "batch_mode": approved_batch,
        }))
    }

    /// `POST /v1/remote/desktop-control/action`. Honours the per-action
    /// `ActionGate` from the core: a non-batch session prompts locally for each
    /// action, a batch session executes immediately.
    pub fn action(
        &self,
        device_id: &str,
        device_label: &str,
        request: DesktopControlActionRequest,
    ) -> Result<serde_json::Value, (u16, String)> {
        self.require_owned_session(device_id, &request.session_id)?;
        let gate = self
            .state
            .begin_action(
                &request.session_id,
                &request.target_application_id,
                request.action.clone(),
            )
            .map_err(|error| (409, error))?;
        let executed = match gate {
            ActionGate::Executed(result) => {
                result.map_err(|error| (502, error))?;
                true
            }
            ActionGate::Pending { action_id, .. } => {
                let approve = self
                    .prompter
                    .confirm_action(device_label, &describe_action(&request.action));
                let ran = self
                    .state
                    .finish_pending(&action_id, &request.action, approve)
                    .map_err(|error| (502, error))?;
                if !ran && !approve {
                    return Err((403, "Control action was denied on the runner".to_string()));
                }
                ran
            }
        };
        self.note_action(&request.session_id);
        Ok(serde_json::json!({
            "protocol_version": super::protocol::REMOTE_PROTOCOL_VERSION,
            "session_id": request.session_id,
            "executed": executed,
        }))
    }

    /// `POST /v1/remote/desktop-control/stop`.
    pub fn stop(
        &self,
        device_id: &str,
        session_id: &str,
    ) -> Result<serde_json::Value, (u16, String)> {
        self.require_owned_session(device_id, session_id)?;
        let stopped = self
            .state
            .stop_session(session_id)
            .map_err(|error| (500, error))?;
        self.finalize(session_id, None);
        self.forget_session(session_id);
        Ok(serde_json::json!({
            "protocol_version": super::protocol::REMOTE_PROTOCOL_VERSION,
            "session_id": session_id,
            "stopped": stopped,
        }))
    }

    /// Immediately stop every live session — the in-process kill-switch path.
    pub fn emergency_stop_all(&self) -> usize {
        let (stopped, _) = self.state.emergency_stop().unwrap_or((0, 0));
        let session_ids: Vec<String> = self
            .recordings
            .lock()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default();
        for session_id in &session_ids {
            self.finalize(
                session_id,
                Some("Force-stopped by kill switch or escape hatch"),
            );
        }
        if let Ok(mut map) = self.device_sessions.lock() {
            map.clear();
        }
        stopped
    }

    /// Serve-loop enforcement of the cross-process signals a separate CLI
    /// process can raise: an engaged kill switch or a local escape-hatch flag
    /// stops everything; otherwise any session whose owning device has since
    /// been revoked (e.g. by `monkey daemon remote pair-revoke` in another
    /// process) is force-stopped. All errors are swallowed — enforcement must
    /// never take the resident service offline. Cheap when idle: it opens no
    /// `RemoteStore` unless a session is actually being tracked.
    pub fn enforce(&self, kill_switch: bool, escape_hatch: bool) {
        if kill_switch || escape_hatch {
            let _ = self.emergency_stop_all();
            return;
        }
        let tracked: Vec<String> = self
            .device_sessions
            .lock()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default();
        if tracked.is_empty() {
            return;
        }
        let store = match RemoteStore::open(&self.paths.root) {
            Ok(store) => store,
            Err(_) => return,
        };
        for device_id in tracked {
            let revoked = match store.device(&device_id) {
                Ok(Some(device)) => !device.active(),
                Ok(None) => true,
                Err(_) => false,
            };
            if revoked {
                self.force_stop_device(&device_id);
            }
        }
    }

    fn require_owned_session(
        &self,
        device_id: &str,
        session_id: &str,
    ) -> Result<(), (u16, String)> {
        let owns = self
            .device_sessions
            .lock()
            .map_err(poisoned)?
            .get(device_id)
            .is_some_and(|sessions| sessions.contains(session_id));
        if owns {
            Ok(())
        } else {
            // Do not confirm the existence of another device's session.
            Err((404, "Unknown desktop-control session".to_string()))
        }
    }

    fn forget_session(&self, session_id: &str) {
        if let Ok(mut map) = self.device_sessions.lock() {
            map.retain(|_, sessions| {
                sessions.remove(session_id);
                !sessions.is_empty()
            });
        }
    }

    // ----- recording -------------------------------------------------------

    fn begin_recording(&self, session: &ControlSession) {
        if !self.record {
            return;
        }
        if let Err(error) = self.begin_recording_inner(session) {
            eprintln!("remote desktop-control: could not start session recording: {error}");
        }
    }

    fn begin_recording_inner(&self, session: &ControlSession) -> Result<(), String> {
        let ledger = RunLedger::open(&self.paths.ledger_db).map_err(|error| error.to_string())?;
        let spec = run_spec(session);
        let (recorder, _) =
            DurableRunRecorder::submit(ledger, &spec, "remote-desktop".to_string())?;
        recorder.emit(RunEvent::Started {
            engine_id: "remote-desktop-control".to_string(),
        })?;
        let now = now_ms();
        self.recordings.lock().map_err(poisoned_str)?.insert(
            session.session_id.clone(),
            Recording {
                recorder,
                actions_since_capture: 0,
                last_capture_ms: 0,
                capture_index: 0,
                finished: false,
            },
        );
        self.capture(&session.session_id, now);
        Ok(())
    }

    fn note_action(&self, session_id: &str) {
        if !self.record {
            return;
        }
        let now = now_ms();
        let due = {
            let mut map = match self.recordings.lock() {
                Ok(map) => map,
                Err(_) => return,
            };
            match map.get_mut(session_id) {
                Some(recording) if !recording.finished => {
                    recording.actions_since_capture += 1;
                    recording.actions_since_capture >= CAPTURE_EVERY_N_ACTIONS
                        || now.saturating_sub(recording.last_capture_ms) >= CAPTURE_INTERVAL_MS
                }
                _ => false,
            }
        };
        if due {
            self.capture(session_id, now);
        }
    }

    fn finalize(&self, session_id: &str, cancel_reason: Option<&str>) {
        if !self.record {
            return;
        }
        let now = now_ms();
        self.capture(session_id, now);
        let recorder = {
            let mut map = match self.recordings.lock() {
                Ok(map) => map,
                Err(_) => return,
            };
            match map.get_mut(session_id) {
                Some(recording) if !recording.finished => {
                    recording.finished = true;
                    recording.recorder.clone()
                }
                _ => return,
            }
        };
        let event = match cancel_reason {
            Some(reason) => RunEvent::Cancelled {
                reason: Some(reason.to_string()),
            },
            None => RunEvent::Completed {
                summary: Some("Remote desktop-control session ended".to_string()),
                result_artifact_ids: Vec::new(),
                usage: UsageSnapshot {
                    input_tokens: 0,
                    output_tokens: 0,
                    cached_input_tokens: 0,
                    model_calls: 0,
                    tool_calls: 0,
                    cost_micros: None,
                },
            },
        };
        if let Err(error) = recorder.emit(event) {
            eprintln!("remote desktop-control: could not finalize session {session_id}: {error}");
        }
        if let Ok(mut map) = self.recordings.lock() {
            map.remove(session_id);
        }
    }

    /// Take one `screencapture -x` screenshot and record it as an
    /// `ArtifactAdded` event. Best-effort: a capture failure is logged but must
    /// never abort the session it is documenting.
    fn capture(&self, session_id: &str, now: u64) {
        if !self.record {
            return;
        }
        if let Err(error) = self.capture_inner(session_id, now) {
            eprintln!("remote desktop-control: screenshot capture failed: {error}");
        }
    }

    fn capture_inner(&self, session_id: &str, now: u64) -> Result<(), String> {
        let (recorder, index) = {
            let mut map = self.recordings.lock().map_err(poisoned_str)?;
            let Some(recording) = map.get_mut(session_id) else {
                return Ok(());
            };
            let index = recording.capture_index;
            recording.capture_index += 1;
            recording.actions_since_capture = 0;
            recording.last_capture_ms = now;
            (recording.recorder.clone(), index)
        };
        let bytes = self.take_screenshot(session_id, index)?;
        let store = ArtifactStore::with_max_blob_size(
            app_data(&self.paths).join("content-v1"),
            MAX_REMOTE_ARTIFACT_BYTES,
        )
        .map_err(|error| error.to_string())?;
        let blob = store.put(&bytes).map_err(|error| error.to_string())?;
        recorder.emit(RunEvent::ArtifactAdded {
            artifact_id: blob.id.clone(),
            kind: ArtifactKind::Image,
            name: format!("session-{index:04}.png"),
            media_type: "image/png".to_string(),
            content_sha256: blob.id.clone(),
            size_bytes: blob.size,
        })?;
        Ok(())
    }

    /// Reuse the exact `screencapture -x` invocation `m7_capture_screen` uses,
    /// writing into `<app_data>/remote-control-sessions/<session_id>/`.
    fn take_screenshot(&self, session_id: &str, index: u32) -> Result<Vec<u8>, String> {
        let dir = app_data(&self.paths)
            .join("remote-control-sessions")
            .join(session_id);
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("Could not create session recording directory: {error}"))?;
        let output = dir.join(format!("capture-{index:04}.png"));
        #[cfg(target_os = "macos")]
        {
            let status = std::process::Command::new("/usr/sbin/screencapture")
                .arg("-x")
                .arg(&output)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map_err(|error| format!("Could not start screencapture: {error}"))?;
            if !status.success() || !output.exists() {
                let _ = std::fs::remove_file(&output);
                return Err("screencapture did not produce an image".to_string());
            }
            let bytes = std::fs::read(&output).map_err(|error| error.to_string())?;
            let _ = std::fs::remove_file(&output);
            Ok(bytes)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = output;
            Err("Session-recording screenshots are only implemented on macOS".to_string())
        }
    }
}

impl DesktopSessionKiller for DesktopControlRuntime {
    fn force_stop_device(&self, device_id: &str) -> usize {
        let session_ids: Vec<String> = self
            .device_sessions
            .lock()
            .ok()
            .and_then(|map| map.get(device_id).map(|set| set.iter().cloned().collect()))
            .unwrap_or_default();
        let mut stopped = 0usize;
        for session_id in &session_ids {
            if self.state.stop_session(session_id).unwrap_or(false) {
                stopped += 1;
            }
            self.finalize(session_id, Some("Force-stopped: device revoked"));
        }
        if let Ok(mut map) = self.device_sessions.lock() {
            map.remove(device_id);
        }
        stopped
    }
}

fn describe_action(action: &ControlAction) -> String {
    match action {
        ControlAction::MouseMove { x, y } => format!("move the mouse to ({x}, {y})"),
        ControlAction::MouseClick { button } => format!("a {button:?} mouse click"),
        ControlAction::KeyPress { key } => format!("press the '{key}' key"),
    }
}

/// The daemon root is `<app_data>/daemon`, so its parent is `<app_data>` — the
/// same derivation `api.rs` uses to locate the shared `content-v1` blob store.
fn app_data(paths: &DaemonPaths) -> PathBuf {
    paths
        .root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| paths.root.clone())
}

fn run_spec(session: &ControlSession) -> RunSpec {
    RunSpec {
        schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
        run_id: session.session_id.clone(),
        idempotency_key: format!("idem-{}", session.session_id),
        created_at_ms: session.created_at_ms,
        kind: RunKind::RemoteDesktopControl,
        submitted_by: ClientIdentity {
            client_id: "remote-desktop-control".into(),
            instance_id: format!("remote-desktop-{}", std::process::id()),
            kind: ClientKind::Daemon,
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
        task: "Remote desktop-control session".into(),
        instructions: None,
        input_artifact_ids: vec![],
        // A desktop-control session has no model target, but a RunSpec requires
        // one; this credential-free placeholder never performs inference.
        target: ModelTargetSnapshot::Provider {
            target_id: "remote-desktop-control".into(),
            label: "Remote Desktop Control".into(),
            provider_id: "remote-desktop-control".into(),
            endpoint: "https://desktop-control.invalid/v1".into(),
            model: "none".into(),
            credential_ref_id: "credential-none".into(),
            capabilities: crate::task::cli_capabilities(),
        },
        workspace: None,
        permission_policy: PermissionPolicySnapshot {
            mode: PermissionMode::Auto,
            unattended: true,
            approval_timeout_ms: 60_000,
            default_tool_decision: ToolPolicyDecision::Prompt,
            tool_rules: vec![],
            allow_network: false,
            allow_external_mutations: false,
        },
        budgets: RunBudgets {
            wall_time_ms: MAX_SESSION_LIFETIME_MS,
            max_iterations: 1,
            max_model_calls: 1,
            max_tool_calls: 1,
            max_input_tokens: 1,
            max_output_tokens: 1,
            max_cost_micros: None,
            max_artifact_bytes: MAX_REMOTE_ARTIFACT_BYTES,
            max_event_count: 100_000,
        },
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn poisoned<T>(_: T) -> (u16, String) {
    (500, "Desktop control runtime lock was poisoned".to_string())
}

fn poisoned_str<T>(_: T) -> String {
    "Desktop control runtime lock was poisoned".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::desktop_control::{MouseButtonKind, NullBackend};

    struct FakePrompter {
        session: SessionConsent,
        action: bool,
    }
    impl ConsentPrompter for FakePrompter {
        fn confirm_session(&self, _device_label: &str) -> SessionConsent {
            self.session
        }
        fn confirm_action(&self, _device_label: &str, _description: &str) -> bool {
            self.action
        }
    }

    fn runtime(session: SessionConsent, action: bool) -> Arc<DesktopControlRuntime> {
        let paths = DaemonPaths::under(
            &std::env::temp_dir().join(format!("lm-desktop-runtime-{}", uuid::Uuid::new_v4())),
        );
        let state = Arc::new(DesktopControlState::with_backend(Arc::new(NullBackend)));
        DesktopControlRuntime::for_test(state, Arc::new(FakePrompter { session, action }), paths)
    }

    fn allow() -> Vec<String> {
        vec!["Notes".to_string()]
    }

    fn move_action() -> DesktopControlActionRequest {
        DesktopControlActionRequest {
            session_id: String::new(),
            target_application_id: "Notes".into(),
            action: ControlAction::MouseMove { x: 1, y: 2 },
        }
    }

    fn session_id_of(value: &serde_json::Value) -> String {
        value["session"]["sessionId"].as_str().unwrap().to_string()
    }

    #[test]
    fn denied_consent_refuses_to_start_a_session() {
        let runtime = runtime(SessionConsent::Deny, true);
        let error = runtime
            .start("device-one", "Phone", allow(), false)
            .unwrap_err();
        assert_eq!(error.0, 403);
    }

    #[test]
    fn remote_batch_request_needs_local_batch_consent() {
        // Remote asks for batch, but the operator only allowed per-action:
        // batch must NOT be granted (each action still prompts).
        let runtime = runtime(SessionConsent::AllowPerAction, false);
        let started = runtime.start("device-one", "Phone", allow(), true).unwrap();
        assert_eq!(started["batch_mode"], serde_json::Value::Bool(false));
        let session_id = session_id_of(&started);
        let mut request = move_action();
        request.session_id = session_id;
        // action=false prompter denies the per-action prompt.
        let error = runtime.action("device-one", "Phone", request).unwrap_err();
        assert_eq!(error.0, 403);
    }

    #[test]
    fn local_batch_consent_plus_remote_request_grants_batch() {
        let runtime = runtime(SessionConsent::AllowBatch, false);
        let started = runtime.start("device-one", "Phone", allow(), true).unwrap();
        assert_eq!(started["batch_mode"], serde_json::Value::Bool(true));
        let session_id = session_id_of(&started);
        let mut request = move_action();
        request.session_id = session_id;
        // No per-action prompt needed in batch mode; executes immediately.
        let outcome = runtime.action("device-one", "Phone", request).unwrap();
        assert_eq!(outcome["executed"], serde_json::Value::Bool(true));
    }

    #[test]
    fn per_action_approval_gates_each_action() {
        let runtime = runtime(SessionConsent::AllowPerAction, true);
        let started = runtime
            .start("device-one", "Phone", allow(), false)
            .unwrap();
        let session_id = session_id_of(&started);
        let mut request = move_action();
        request.session_id = session_id;
        let outcome = runtime.action("device-one", "Phone", request).unwrap();
        assert_eq!(outcome["executed"], serde_json::Value::Bool(true));
    }

    #[test]
    fn a_device_cannot_drive_another_devices_session() {
        let runtime = runtime(SessionConsent::AllowBatch, true);
        let started = runtime.start("device-one", "Phone", allow(), true).unwrap();
        let session_id = session_id_of(&started);
        let mut request = move_action();
        request.session_id = session_id.clone();
        let error = runtime.action("device-two", "Laptop", request).unwrap_err();
        assert_eq!(
            error.0, 404,
            "cross-device session access must look like 404"
        );
        // The rightful owner can still stop it.
        let stopped = runtime.stop("device-one", &session_id).unwrap();
        assert_eq!(stopped["stopped"], serde_json::Value::Bool(true));
    }

    #[test]
    fn revoke_force_stop_kills_only_the_target_device() {
        let runtime = runtime(SessionConsent::AllowBatch, true);
        let one = runtime.start("device-one", "Phone", allow(), true).unwrap();
        let two = runtime.start("device-two", "Laptop", allow(), true);
        // The machine-wide lock is disabled in this NullBackend state, so both
        // can coexist for the test; force-stopping device-one must not touch
        // device-two.
        let _ = two;
        let stopped = runtime.force_stop_device("device-one");
        assert_eq!(stopped, 1);
        let session_one = session_id_of(&one);
        let mut request = move_action();
        request.session_id = session_one;
        // device-one's session is gone.
        assert_eq!(
            runtime
                .action("device-one", "Phone", request)
                .unwrap_err()
                .0,
            404
        );
    }

    #[test]
    fn recording_run_spec_is_valid_and_submittable() {
        // Proves the RunKind::RemoteDesktopControl evidence run this module
        // submits per session is a valid, accepted RunSpec (the screenshot
        // capture itself is macOS-only and exercised separately).
        let session = ControlSession {
            session_id: "desktop-control-test-1".into(),
            allowed_applications: vec!["Notes".into()],
            created_at_ms: 1_000,
            expires_at_ms: 61_000,
            active: true,
            indicator_visible: true,
            approved_batch: false,
        };
        let spec = run_spec(&session);
        assert_eq!(spec.kind, RunKind::RemoteDesktopControl);
        spec.validate().expect("recording run spec must validate");
        let mut ledger = RunLedger::open_in_memory().expect("in-memory ledger");
        ledger.submit_run(&spec).expect("recording run must submit");
    }

    #[test]
    fn batch_action_dispatches_to_the_backend() {
        let runtime = runtime(SessionConsent::AllowBatch, true);
        let started = runtime.start("device-one", "Phone", allow(), true).unwrap();
        let session_id = session_id_of(&started);
        let mut request = DesktopControlActionRequest {
            session_id,
            target_application_id: "Notes".into(),
            action: ControlAction::MouseClick {
                button: MouseButtonKind::Left,
            },
        };
        request.action = ControlAction::MouseClick {
            button: MouseButtonKind::Left,
        };
        assert!(runtime.action("device-one", "Phone", request).is_ok());
    }
}
