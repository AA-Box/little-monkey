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
/// timeout counts as a denial — silence is never consent. Referenced by the
/// macOS `osascript` and Linux `zenity` prompts; Windows' `MessageBoxW` has
/// no timeout parameter and blocks until answered.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const CONSENT_TIMEOUT_SECONDS: u64 = 60;

/// Consent-dialog titles and the two "allow" button labels, shared by every
/// platform's prompter so the wording is identical across macOS/Windows/Linux
/// (and, on Linux, so the label matched against zenity's stdout is guaranteed
/// to equal the label we actually passed to `--extra-button`).
const CONSENT_DIALOG_TITLE_SESSION: &str = "Little Monkey — Remote Desktop Control";
const CONSENT_DIALOG_TITLE_ACTION: &str = "Little Monkey — Approve Action";
const CONSENT_ALLOW_BATCH_LABEL: &str = "Allow (batch)";
const CONSENT_ALLOW_PER_ACTION_LABEL: &str = "Allow (per-action)";

/// The local operator's answer to the session consent prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionConsent {
    Deny,
    AllowPerAction,
    AllowBatch,
}

/// Seam for the local, visible consent surface. Production selects a native
/// prompter per OS (see [`production_prompter`]): macOS shells `osascript`,
/// Windows uses `MessageBoxW`, Linux uses `zenity` (falling back to `kdialog`);
/// tests inject a double that returns a canned answer.
pub trait ConsentPrompter: Send + Sync {
    /// Blocking, locally-visible prompt shown on the runner before a session is
    /// created. Returns the operator's choice (or [`SessionConsent::Deny`] on
    /// timeout / dismissal).
    fn confirm_session(&self, device_label: &str) -> SessionConsent;
    /// Blocking, locally-visible per-action prompt for a non-batch session.
    fn confirm_action(&self, device_label: &str, description: &str) -> bool;
}

/// Selects the native consent surface for the current OS — the prompter
/// counterpart to `desktop_control::production_backend`. macOS/Windows/Linux
/// each get a real, locally-visible dialog; every other platform gets a
/// default-deny [`DenyConsentPrompter`] (never a silent allow).
///
/// NOTE: only the arm matching this build's target_os is compiled. The Windows
/// and Linux prompters below were therefore NOT compiled or runtime-verified in
/// this macOS development environment; see each impl's own note.
fn production_prompter() -> Arc<dyn ConsentPrompter> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(OsascriptConsentPrompter)
    }
    #[cfg(target_os = "windows")]
    {
        Arc::new(MessageBoxConsentPrompter)
    }
    #[cfg(target_os = "linux")]
    {
        Arc::new(ZenityKdialogConsentPrompter)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Arc::new(DenyConsentPrompter)
    }
}

/// Maps a two-step yes/no consent flow to a [`SessionConsent`]. Both the
/// Windows `MessageBoxW` path and the Linux `kdialog` fallback need this: neither
/// can render three fully custom-labelled buttons in a single dialog, so they
/// ask "allow at all?" then "batch?" in sequence. Pure and host-testable:
/// - first "No"                  → Deny
/// - first "Yes" + second "Yes"  → AllowBatch
/// - first "Yes" + second "No"   → AllowPerAction
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
fn two_step_session_consent(allow: bool, batch: bool) -> SessionConsent {
    if !allow {
        SessionConsent::Deny
    } else if batch {
        SessionConsent::AllowBatch
    } else {
        SessionConsent::AllowPerAction
    }
}

/// macOS `osascript` implementation. Every invocation is an argv array — never
/// a shell string — matching `daemon_commands.rs`'s stated convention. This is
/// the one prompter compiled and runtime-verified in this dev environment.
#[cfg(target_os = "macos")]
pub struct OsascriptConsentPrompter;

#[cfg(target_os = "macos")]
impl ConsentPrompter for OsascriptConsentPrompter {
    fn confirm_session(&self, device_label: &str) -> SessionConsent {
        let script = format!(
            "display dialog {} with title {} buttons {{\"Deny\", \"{}\", \"{}\"}} default button \
             \"Deny\" cancel button \"Deny\" with icon caution giving up after {}",
            apple_script_string(&format!(
                "Remote desktop control was requested by device \"{device_label}\".\n\nAllow \
                 this device to control this Mac's keyboard and mouse?"
            )),
            apple_script_string(CONSENT_DIALOG_TITLE_SESSION),
            CONSENT_ALLOW_PER_ACTION_LABEL,
            CONSENT_ALLOW_BATCH_LABEL,
            CONSENT_TIMEOUT_SECONDS,
        );
        match run_osascript(&script) {
            Some(output) if output.contains(CONSENT_ALLOW_BATCH_LABEL) => {
                SessionConsent::AllowBatch
            }
            Some(output) if output.contains(CONSENT_ALLOW_PER_ACTION_LABEL) => {
                SessionConsent::AllowPerAction
            }
            _ => SessionConsent::Deny,
        }
    }

    fn confirm_action(&self, device_label: &str, description: &str) -> bool {
        let script = format!(
            "display dialog {} with title {} buttons {{\"Deny\", \"Allow\"}} default button \
             \"Deny\" cancel button \"Deny\" with icon caution giving up after {}",
            apple_script_string(&format!(
                "Device \"{device_label}\" wants to perform: {description}"
            )),
            apple_script_string(CONSENT_DIALOG_TITLE_ACTION),
            CONSENT_TIMEOUT_SECONDS,
        );
        matches!(run_osascript(&script), Some(output) if output.contains("button returned:Allow"))
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

// ===========================================================================
// Windows consent surface (`MessageBoxW`)
//
// COMPILED ONLY ON WINDOWS; NOT runtime-verified in this dev environment (no
// Windows machine available). All UTF-16 conversion — the only non-trivial
// logic — lives in the pure, host-tested `to_utf16_null_terminated`; the FFI
// wrapper below is a bare `MessageBoxW` call. It requires an interactive
// desktop session: a service running in Windows session 0 cannot show these
// dialogs to the logged-in user (a known limitation, out of scope here).
// ===========================================================================

/// Windows native consent surface. `MessageBoxW` cannot show three fully
/// custom-labelled buttons in one dialog, so `confirm_session` is a two-step
/// Yes/No flow (allow? then batch?) mapped via [`two_step_session_consent`].
#[cfg(target_os = "windows")]
pub struct MessageBoxConsentPrompter;

#[cfg(target_os = "windows")]
impl ConsentPrompter for MessageBoxConsentPrompter {
    fn confirm_session(&self, device_label: &str) -> SessionConsent {
        let allow = message_box_yes_no(
            CONSENT_DIALOG_TITLE_SESSION,
            &format!("Remote desktop control requested by device \"{device_label}\". Allow?"),
        );
        if !allow {
            return SessionConsent::Deny;
        }
        let batch = message_box_yes_no(
            CONSENT_DIALOG_TITLE_SESSION,
            "Allow this device to act without per-action approval (batch mode)? Choosing No \
             means every action still needs a separate approval.",
        );
        two_step_session_consent(allow, batch)
    }

    fn confirm_action(&self, device_label: &str, description: &str) -> bool {
        message_box_yes_no(
            CONSENT_DIALOG_TITLE_ACTION,
            &format!("Device \"{device_label}\" wants to perform: {description}. Allow?"),
        )
    }
}

/// Thin `MessageBoxW` wrapper: one modal Yes/No dialog, `true` iff Yes.
#[cfg(target_os = "windows")]
fn message_box_yes_no(title: &str, text: &str) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
    };
    let text_utf16 = to_utf16_null_terminated(text);
    let title_utf16 = to_utf16_null_terminated(title);
    // SAFETY: both buffers are valid, null-terminated UTF-16 and outlive the
    // call; a null HWND means the dialog has no owner window.
    let result = unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text_utf16.as_ptr(),
            title_utf16.as_ptr(),
            MB_YESNO | MB_ICONWARNING | MB_TOPMOST | MB_SETFOREGROUND,
        )
    };
    result == IDYES
}

/// Convert a `&str` into the null-terminated UTF-16 buffer the Win32 `*W` APIs
/// expect. Portable, allocation-only Rust — unit-tested on this macOS host even
/// though its only production caller is Windows-gated.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn to_utf16_null_terminated(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

// ===========================================================================
// Linux consent surface (`zenity`, falling back to `kdialog`)
//
// COMPILED ONLY ON LINUX; NOT runtime-verified in this dev environment (no
// Linux machine available). The one piece of real logic — turning zenity's
// exit-code/stdout contract into a decision — lives in the pure, host-tested
// `parse_zenity_session_consent` / `exit_code_is_yes`; the impl below just
// spawns the tools (argv arrays, never a shell) and captures their output.
// ===========================================================================

/// Linux native consent surface. Prefers `zenity` (one dialog with two custom
/// `--extra-button`s), falling back to `kdialog --yesno` in the same two-step
/// shape as the Windows path. If NEITHER tool is on `PATH` this fails closed to
/// [`SessionConsent::Deny`] / `false` and logs that the denial is a missing
/// local-consent dependency, not a normal operator refusal.
#[cfg(target_os = "linux")]
pub struct ZenityKdialogConsentPrompter;

#[cfg(target_os = "linux")]
impl ConsentPrompter for ZenityKdialogConsentPrompter {
    fn confirm_session(&self, device_label: &str) -> SessionConsent {
        let question =
            format!("Remote desktop control requested by device \"{device_label}\". Allow?");
        // zenity: both custom extra buttons in a single dialog. Its default OK
        // and Cancel buttons are treated as Deny — only the two extra-button
        // labels count as consent.
        if let Some(output) = run_consent_tool(
            "zenity",
            &[
                "--question".to_string(),
                format!("--title={CONSENT_DIALOG_TITLE_SESSION}"),
                format!("--text={question}"),
                format!("--timeout={CONSENT_TIMEOUT_SECONDS}"),
                format!("--extra-button={CONSENT_ALLOW_BATCH_LABEL}"),
                format!("--extra-button={CONSENT_ALLOW_PER_ACTION_LABEL}"),
            ],
        ) {
            let stdout = String::from_utf8_lossy(&output.stdout);
            return parse_zenity_session_consent(
                output.status.code(),
                &stdout,
                CONSENT_ALLOW_BATCH_LABEL,
                CONSENT_ALLOW_PER_ACTION_LABEL,
            );
        }
        // zenity unavailable → kdialog two-step (kdialog has no clean
        // three-custom-button primitive either).
        let Some(allow) = kdialog_yes_no(&question) else {
            no_local_consent_tool();
            return SessionConsent::Deny;
        };
        if !allow {
            return SessionConsent::Deny;
        }
        // A second-dialog spawn failure fails closed to the more restrictive
        // per-action mode rather than granting batch.
        let batch = kdialog_yes_no(
            "Allow this device to act without per-action approval (batch mode)? Choosing No \
             means every action still needs a separate approval.",
        )
        .unwrap_or(false);
        two_step_session_consent(allow, batch)
    }

    fn confirm_action(&self, device_label: &str, description: &str) -> bool {
        let question = format!("Device \"{device_label}\" wants to perform: {description}. Allow?");
        // A plain yes/no is enough here: zenity --question is OK/Cancel.
        if let Some(output) = run_consent_tool(
            "zenity",
            &[
                "--question".to_string(),
                format!("--title={CONSENT_DIALOG_TITLE_ACTION}"),
                format!("--text={question}"),
                format!("--timeout={CONSENT_TIMEOUT_SECONDS}"),
            ],
        ) {
            return exit_code_is_yes(output.status.code());
        }
        match kdialog_yes_no(&question) {
            Some(answer) => answer,
            None => {
                no_local_consent_tool();
                false
            }
        }
    }
}

/// Spawn a local consent GUI tool with an argv array (never a shell), capturing
/// its output. `None` means the tool could not be run at all (not installed /
/// not on `PATH`, or any other spawn error) — the caller then tries the next
/// tool or fails closed; `Some(output)` means it ran and its exit status /
/// stdout carry the operator's answer.
#[cfg(target_os = "linux")]
fn run_consent_tool(program: &str, args: &[String]) -> Option<std::process::Output> {
    std::process::Command::new(program).args(args).output().ok()
}

/// One `kdialog --yesno` dialog: `Some(true)` for Yes, `Some(false)` for No,
/// `None` if kdialog could not be run.
#[cfg(target_os = "linux")]
fn kdialog_yes_no(text: &str) -> Option<bool> {
    run_consent_tool("kdialog", &["--yesno".to_string(), text.to_string()])
        .map(|output| exit_code_is_yes(output.status.code()))
}

/// Logs that a denial was forced by the *absence* of any local consent tool,
/// so it is not mistaken for a normal operator denial.
#[cfg(target_os = "linux")]
fn no_local_consent_tool() {
    eprintln!(
        "remote desktop-control: no local consent tool is available (neither `zenity` nor \
         `kdialog` is on PATH) — denying. This is a missing-dependency default-deny, NOT a \
         normal operator denial; install zenity or kdialog to enable local consent prompts."
    );
}

/// Decide a session consent from zenity's raw exit code + stdout. Pure and
/// host-testable (it parses already-captured strings/codes, so it needs no
/// zenity installed).
///
/// zenity `--question` with two `--extra-button` values:
/// - OK pressed:        exit 0, empty stdout          → Deny
/// - Cancel pressed:    exit 1, empty stdout          → Deny
/// - `--extra-button`:  exit 1, stdout = button label → the matching consent
/// - `--timeout` fired: exit 5, empty stdout          → Deny
/// - killed (no status)                               → Deny
///
/// Only a stdout equal to one of the two extra-button labels we passed counts
/// as consent; the plain OK/Cancel buttons and every failure mode are a denial
/// ("silence is never consent").
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_zenity_session_consent(
    exit_code: Option<i32>,
    stdout: &str,
    batch_label: &str,
    per_action_label: &str,
) -> SessionConsent {
    // A dialog that never exited cleanly (killed / crashed) is never consent.
    if exit_code.is_none() {
        return SessionConsent::Deny;
    }
    match stdout.trim() {
        label if label == batch_label => SessionConsent::AllowBatch,
        label if label == per_action_label => SessionConsent::AllowPerAction,
        _ => SessionConsent::Deny,
    }
}

/// `true` iff the process exited successfully (code 0). Used for the plain
/// yes/no tools — zenity `--question` (OK = 0) and `kdialog --yesno` (Yes = 0);
/// every other code, and a killed process, is "no". Pure and host-testable.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn exit_code_is_yes(exit_code: Option<i32>) -> bool {
    exit_code == Some(0)
}

/// Fallback for platforms with no wired local consent surface (BSD, etc.):
/// always deny, since there is no way to show the operator a prompt — a
/// default-deny, never a silent allow.
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub struct DenyConsentPrompter;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
impl ConsentPrompter for DenyConsentPrompter {
    fn confirm_session(&self, device_label: &str) -> SessionConsent {
        let _ = device_label;
        eprintln!(
            "remote desktop-control: no local consent surface on this platform — denying \
             (default-deny, not an operator denial)"
        );
        SessionConsent::Deny
    }
    fn confirm_action(&self, device_label: &str, description: &str) -> bool {
        let _ = (device_label, description);
        false
    }
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
    /// Production runtime: the real `enigo` input backend (macOS/Windows/
    /// Linux-X11) guarded by the machine-wide `<app_data>/desktop_control.lock`,
    /// the OS-native consent prompter for this platform (see
    /// [`production_prompter`]), and durable session recording enabled.
    pub fn production(paths: &DaemonPaths) -> Arc<Self> {
        let lock_path = app_data(paths).join("desktop_control.lock");
        Arc::new(Self {
            state: Arc::new(DesktopControlState::production_with_lock(lock_path)),
            prompter: production_prompter(),
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

    // ----- pure consent-parsing helpers (host-testable) --------------------
    //
    // These cover the Windows and Linux prompters' only non-trivial logic even
    // though those prompters themselves are not compiled on this macOS host:
    // the functions are platform-agnostic pure Rust on purpose.

    #[test]
    fn two_step_flow_maps_to_the_right_consent() {
        assert_eq!(two_step_session_consent(false, false), SessionConsent::Deny);
        assert_eq!(two_step_session_consent(false, true), SessionConsent::Deny);
        assert_eq!(
            two_step_session_consent(true, true),
            SessionConsent::AllowBatch
        );
        assert_eq!(
            two_step_session_consent(true, false),
            SessionConsent::AllowPerAction
        );
    }

    #[test]
    fn utf16_conversion_is_null_terminated() {
        assert_eq!(to_utf16_null_terminated(""), vec![0]);
        assert_eq!(to_utf16_null_terminated("Hi"), vec![0x48, 0x69, 0]);
        // Non-ASCII is encoded as UTF-16 (em dash U+2014) and still terminated.
        let out = to_utf16_null_terminated("A—");
        assert_eq!(out.first(), Some(&0x41));
        assert_eq!(out.last(), Some(&0));
        assert!(out.contains(&0x2014));
    }

    #[test]
    fn zenity_extra_button_labels_map_to_consent() {
        let (batch, per) = (CONSENT_ALLOW_BATCH_LABEL, CONSENT_ALLOW_PER_ACTION_LABEL);
        // An extra button prints its label to stdout and exits 1.
        assert_eq!(
            parse_zenity_session_consent(Some(1), &format!("{batch}\n"), batch, per),
            SessionConsent::AllowBatch
        );
        assert_eq!(
            parse_zenity_session_consent(Some(1), &format!("{per}\n"), batch, per),
            SessionConsent::AllowPerAction
        );
    }

    #[test]
    fn zenity_ok_cancel_timeout_and_kill_are_all_denials() {
        let (batch, per) = (CONSENT_ALLOW_BATCH_LABEL, CONSENT_ALLOW_PER_ACTION_LABEL);
        // OK: exit 0, empty stdout.
        assert_eq!(
            parse_zenity_session_consent(Some(0), "", batch, per),
            SessionConsent::Deny
        );
        // Cancel: exit 1, empty stdout.
        assert_eq!(
            parse_zenity_session_consent(Some(1), "", batch, per),
            SessionConsent::Deny
        );
        // --timeout fired: exit 5, empty stdout.
        assert_eq!(
            parse_zenity_session_consent(Some(5), "", batch, per),
            SessionConsent::Deny
        );
        // Killed by a signal: no exit status at all.
        assert_eq!(
            parse_zenity_session_consent(None, "", batch, per),
            SessionConsent::Deny
        );
        // A stdout that is not one of our labels is never consent.
        assert_eq!(
            parse_zenity_session_consent(Some(1), "something else", batch, per),
            SessionConsent::Deny
        );
    }

    #[test]
    fn exit_code_yes_only_on_zero() {
        assert!(exit_code_is_yes(Some(0)));
        assert!(!exit_code_is_yes(Some(1)));
        assert!(!exit_code_is_yes(Some(5)));
        assert!(!exit_code_is_yes(None));
    }
}
