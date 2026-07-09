//! Permission request/response system.
//!
//! Every mutating agent tool (write_file, edit_file, run_shell — see tools.rs) must
//! call [`request_permission`] before doing anything destructive. This emits a
//! `permission://request` event to the frontend, which renders a modal
//! (Allow Once / Allow for Session / Deny). The frontend responds via the
//! `permission_respond` command, which resolves the oneshot channel that
//! `request_permission` is awaiting on.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use tauri::Emitter;
use tokio::sync::oneshot;

use crate::AppState;

/// Payload sent to the frontend over the `permission://request` event.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PermissionRequestPayload {
    pub id: String,
    pub tool: String,
    pub detail: String,
}

/// Shared state tracking in-flight permission requests and tools that have
/// been granted "allow for session" status.
pub struct PermissionState {
    /// `id -> (tool the request was actually made for, response channel)`.
    /// The tool name is stored here (not just the sender) so
    /// [`permission_respond`] can use it as the *authoritative* source of
    /// truth for `session_allow` bookkeeping instead of trusting whatever
    /// tool name the IPC caller claims — see [`permission_respond`] for why
    /// that distinction matters.
    pub pending: Mutex<HashMap<String, (String, oneshot::Sender<bool>)>>,
    pub session_allow: Mutex<HashSet<String>>,
    /// Current permission mode — one of "manual"/"acceptEdits"/"plan"/"auto"/
    /// "bypass". See [`request_permission`] for what each mode does. Always
    /// boots at "manual" (see the `Default` impl below), regardless of
    /// whatever the frontend may have restored from its own storage — the
    /// frontend is responsible for pushing a restored non-"manual" mode back
    /// to [`set_permission_mode`] itself, once, at startup.
    pub mode: Mutex<String>,
}

impl Default for PermissionState {
    fn default() -> Self {
        PermissionState {
            pending: Mutex::new(HashMap::new()),
            session_allow: Mutex::new(HashSet::new()),
            mode: Mutex::new("manual".to_string()),
        }
    }
}

/// Tools for which an "allow for session" grant is never remembered, no
/// matter what the frontend requests: their blast radius (arbitrary shell
/// execution) is too large to silently pre-authorize for the rest of the
/// session off the back of a single approval. Approving one of these always
/// prompts again next time.
const NO_SESSION_REMEMBER: &[&str] = &["run_shell"];

/// Timeout for a permission prompt going unanswered — after this, the request
/// is treated as denied so the agent loop doesn't hang forever.
const PERMISSION_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Every valid permission mode identifier, shared verbatim with the frontend.
const VALID_MODES: &[&str] = &["manual", "acceptEdits", "plan", "auto", "bypass"];

/// Mode-based short-circuit decision for [`request_permission`]:
/// `Some(result)` means the mode decides on its own without prompting;
/// `None` means fall through to the normal prompting logic. Factored out of
/// [`request_permission`] so the decision table can be exercised directly in
/// tests without standing up a full Tauri app/window.
///
/// `run_shell` is deliberately NEVER auto-approved here outside of "bypass":
/// the agent reads untrusted workspace content, so any heuristic gate on
/// shell commands (a substring blacklist used to live here) is a prompt-
/// injection-shaped exfiltration path. Users who truly want promptless shell
/// have "bypass" mode.
fn mode_short_circuit(mode: &str, tool: &str) -> Option<Result<(), String>> {
    match mode {
        "bypass" => Some(Ok(())),
        "plan" => Some(Err(format!(
            "Blocked: Little Monkey is in Plan Mode. Describe your plan instead of using {tool} - ask the user to switch out of Plan Mode before making changes."
        ))),
        "acceptEdits" | "auto" => {
            if tool == "write_file" || tool == "edit_file" {
                Some(Ok(()))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Ask the user for permission to run `tool` (with human-readable `detail`
/// describing exactly what will happen). Resolves `Ok(())` if allowed, or
/// `Err` with a human-readable reason if not.
///
/// The current permission mode (see [`set_permission_mode`]) is consulted
/// first:
/// - `"bypass"`: always `Ok(())`, no prompt, no `session_allow` interaction.
/// - `"plan"`: always `Err(..)` — every caller of this function is already a
///   mutating tool (read-only tools never call it), so plan mode blocks all
///   of them unconditionally.
/// - `"acceptEdits"`: `write_file`/`edit_file` are auto-approved; anything
///   else (i.e. `run_shell`) falls through to the normal prompting logic.
/// - `"auto"`: same as `"acceptEdits"` — `write_file`/`edit_file` are
///   auto-approved, and `run_shell` ALWAYS falls through to the normal
///   prompting logic (see [`mode_short_circuit`] for why it is never
///   auto-approved).
/// - `"manual"`, or any unrecognized value (as a safe default): always falls
///   through to the normal prompting logic, unchanged.
///
/// The normal prompting logic: if `tool` has already been granted "allow for
/// session", resolves `Ok(())` immediately without prompting; otherwise emits
/// a `permission://request` event and awaits the user's decision (or the
/// timeout, which counts as a denial).
pub async fn request_permission(
    app: &tauri::AppHandle,
    state: &AppState,
    tool: &str,
    detail: String,
) -> Result<(), String> {
    let mode = state.permissions.mode.lock().unwrap().clone();

    if let Some(decision) = mode_short_circuit(&mode, tool) {
        return decision;
    }

    if state
        .permissions
        .session_allow
        .lock()
        .unwrap()
        .contains(tool)
    {
        return Ok(());
    }

    let id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel::<bool>();

    state
        .permissions
        .pending
        .lock()
        .unwrap()
        .insert(id.clone(), (tool.to_string(), tx));

    let payload = PermissionRequestPayload {
        id: id.clone(),
        tool: tool.to_string(),
        detail,
    };

    if app.emit("permission://request", payload).is_err() {
        // No windows to receive the event — nobody can grant permission.
        state.permissions.pending.lock().unwrap().remove(&id);
        return Err("Permission denied".to_string());
    }

    match tokio::time::timeout(PERMISSION_TIMEOUT, rx).await {
        Ok(Ok(true)) => Ok(()),
        Ok(Ok(false)) => Err("Permission denied".to_string()),
        // Timed out, or the sender was dropped without a response.
        Ok(Err(_)) | Err(_) => {
            state.permissions.pending.lock().unwrap().remove(&id);
            Err("Permission denied".to_string())
        }
    }
}

/// Set the active permission mode. If the new mode is `"manual"` or `"plan"`
/// (i.e. tightening restrictions), also clears every "allow for session"
/// grant — the same clearing behavior [`reset_for_new_workspace`] applies —
/// so a grant made under a looser mode doesn't silently keep applying once
/// the user has deliberately locked things back down.
#[tauri::command]
pub fn set_permission_mode(state: tauri::State<'_, AppState>, mode: String) -> Result<(), String> {
    set_permission_mode_impl(state.inner(), mode)
}

/// Core logic behind [`set_permission_mode`], factored out so it can be
/// exercised directly in tests without standing up a full Tauri app/window.
fn set_permission_mode_impl(state: &AppState, mode: String) -> Result<(), String> {
    if !VALID_MODES.contains(&mode.as_str()) {
        return Err("Unknown permission mode".to_string());
    }

    let tightening = mode == "manual" || mode == "plan";

    *state.permissions.mode.lock().unwrap() = mode;

    if tightening {
        state.permissions.session_allow.lock().unwrap().clear();
    }

    Ok(())
}

/// Return the currently-active permission mode.
#[tauri::command]
pub fn get_permission_mode(state: tauri::State<'_, AppState>) -> Result<String, String> {
    Ok(get_permission_mode_impl(state.inner()))
}

/// Core logic behind [`get_permission_mode`], factored out so it can be
/// exercised directly in tests without standing up a full Tauri app/window.
fn get_permission_mode_impl(state: &AppState) -> String {
    state.permissions.mode.lock().unwrap().clone()
}

/// Called by the frontend (PermissionModal) once the user makes a decision.
///
/// Deliberately does *not* take a `tool` parameter from the caller: the tool
/// name is looked up from the `pending` map by `id` instead. If it were
/// caller-supplied, anything able to invoke Tauri commands (a compromised
/// dependency, a stray devtools console, a future feature) could take the
/// `id` of a genuine, low-stakes pending prompt (e.g. `write_file`) and
/// answer it while claiming a different, more dangerous tool name (e.g.
/// `run_shell`), smuggling that tool into `session_allow` without the user
/// ever seeing or approving a prompt for it. Using the stored tool name
/// makes the persisted grant match exactly what was shown to the user.
#[tauri::command]
pub fn permission_respond(
    state: tauri::State<'_, AppState>,
    id: String,
    allow: bool,
    remember: bool,
) -> Result<(), String> {
    respond_impl(state.inner(), id, allow, remember)
}

/// Core logic behind [`permission_respond`], factored out so it can be
/// exercised directly in tests without standing up a full Tauri app/window.
fn respond_impl(state: &AppState, id: String, allow: bool, remember: bool) -> Result<(), String> {
    let entry = state.permissions.pending.lock().unwrap().remove(&id);

    let (tool, sender) = match entry {
        Some(entry) => entry,
        None => return Err(format!("No pending permission request with id {id}")),
    };

    if remember && allow && !NO_SESSION_REMEMBER.contains(&tool.as_str()) {
        state.permissions.session_allow.lock().unwrap().insert(tool);
    }

    // If the receiving end was already dropped (e.g. the request timed out
    // just before the user clicked), there's nothing left to notify.
    let _ = sender.send(allow);

    Ok(())
}

/// Denies every still-in-flight permission prompt WITHOUT touching
/// "allow for session" grants. Used by the Stop button's tool-cancellation
/// path (see `tools::tools_cancel_running`): a prompt belonging to a turn the
/// user just aborted must not sit on screen waiting for an answer, but
/// stopping one turn is not a reason to revoke grants the user made earlier.
pub fn deny_pending(state: &AppState) {
    let pending: Vec<oneshot::Sender<bool>> = state
        .permissions
        .pending
        .lock()
        .unwrap()
        .drain()
        .map(|(_, (_, sender))| sender)
        .collect();

    for sender in pending {
        let _ = sender.send(false);
    }
}

/// Clears every "allow for session" grant and denies any still-in-flight
/// permission prompts. Must be called whenever the workspace root changes:
/// a grant approved (or a prompt shown) in the context of one workspace must
/// never silently carry over and apply to a different one.
pub fn reset_for_new_workspace(state: &AppState) {
    state.permissions.session_allow.lock().unwrap().clear();
    deny_pending(state);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Directly inserts a pending request the way [`request_permission`]
    /// would, without needing a running app/window to emit an event through.
    fn insert_pending(state: &AppState, id: &str, tool: &str) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel::<bool>();
        state
            .permissions
            .pending
            .lock()
            .unwrap()
            .insert(id.to_string(), (tool.to_string(), tx));
        rx
    }

    #[test]
    fn respond_uses_the_stored_tool_not_a_caller_supplied_one() {
        // Regression test for the confused-deputy: permission_respond no
        // longer accepts a `tool` argument at all, so there is nothing for a
        // caller to spoof — the tool that ends up in `session_allow` must be
        // exactly the one `request_permission` was actually called with for
        // this pending id.
        let state = AppState::default();
        let _rx = insert_pending(&state, "req-1", "write_file");

        respond_impl(&state, "req-1".to_string(), true, true).unwrap();

        let allowed = state.permissions.session_allow.lock().unwrap();
        assert!(allowed.contains("write_file"));
        assert!(!allowed.contains("run_shell"));
    }

    #[test]
    fn run_shell_is_never_remembered_for_the_session() {
        let state = AppState::default();
        let _rx = insert_pending(&state, "req-1", "run_shell");

        respond_impl(&state, "req-1".to_string(), true, true).unwrap();

        assert!(!state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .contains("run_shell"));
    }

    #[test]
    fn respond_errors_for_unknown_id() {
        let state = AppState::default();
        let err = respond_impl(&state, "does-not-exist".to_string(), true, true).unwrap_err();
        assert!(err.contains("No pending permission request"));
    }

    #[test]
    fn respond_delivers_the_decision_to_the_waiting_receiver() {
        let state = AppState::default();
        let rx = insert_pending(&state, "req-1", "edit_file");

        respond_impl(&state, "req-1".to_string(), false, false).unwrap();

        assert_eq!(rx.blocking_recv(), Ok(false));
    }

    #[test]
    fn reset_for_new_workspace_clears_grants_and_denies_pending() {
        let state = AppState::default();
        state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .insert("write_file".to_string());
        let rx = insert_pending(&state, "req-1", "run_shell");

        reset_for_new_workspace(&state);

        assert!(state.permissions.session_allow.lock().unwrap().is_empty());
        assert!(state.permissions.pending.lock().unwrap().is_empty());
        assert_eq!(rx.blocking_recv(), Ok(false));
    }

    #[test]
    fn default_mode_is_manual() {
        let state = AppState::default();
        assert_eq!(*state.permissions.mode.lock().unwrap(), "manual");
    }

    #[test]
    fn set_permission_mode_rejects_unknown_values() {
        let state = AppState::default();
        let err = set_permission_mode_impl(&state, "yolo".to_string()).unwrap_err();
        assert_eq!(err, "Unknown permission mode");
        assert_eq!(*state.permissions.mode.lock().unwrap(), "manual");
    }

    #[test]
    fn set_permission_mode_accepts_every_valid_mode() {
        let state = AppState::default();
        for mode in VALID_MODES {
            set_permission_mode_impl(&state, mode.to_string()).unwrap();
            assert_eq!(*state.permissions.mode.lock().unwrap(), *mode);
        }
    }

    #[test]
    fn set_permission_mode_clears_session_allow_when_tightening_to_manual() {
        let state = AppState::default();
        state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .insert("write_file".to_string());

        set_permission_mode_impl(&state, "manual".to_string()).unwrap();

        assert!(state.permissions.session_allow.lock().unwrap().is_empty());
    }

    #[test]
    fn set_permission_mode_clears_session_allow_when_tightening_to_plan() {
        let state = AppState::default();
        state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .insert("write_file".to_string());

        set_permission_mode_impl(&state, "plan".to_string()).unwrap();

        assert!(state.permissions.session_allow.lock().unwrap().is_empty());
    }

    #[test]
    fn set_permission_mode_keeps_session_allow_when_loosening() {
        let state = AppState::default();
        state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .insert("write_file".to_string());

        set_permission_mode_impl(&state, "auto".to_string()).unwrap();

        assert!(state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .contains("write_file"));
    }

    #[test]
    fn get_permission_mode_returns_current_mode() {
        let state = AppState::default();
        *state.permissions.mode.lock().unwrap() = "auto".to_string();
        assert_eq!(get_permission_mode_impl(&state), "auto");
    }

    #[test]
    fn auto_mode_never_short_circuits_run_shell() {
        // Regression test: "auto" mode used to auto-approve run_shell behind
        // a substring blacklist. run_shell must always fall through to the
        // normal permission prompt (None), no matter how harmless the
        // command looks.
        assert!(mode_short_circuit("auto", "run_shell").is_none());
    }

    #[test]
    fn auto_and_accept_edits_short_circuit_only_file_edits() {
        for mode in ["auto", "acceptEdits"] {
            assert_eq!(mode_short_circuit(mode, "write_file"), Some(Ok(())));
            assert_eq!(mode_short_circuit(mode, "edit_file"), Some(Ok(())));
            assert!(mode_short_circuit(mode, "run_shell").is_none());
        }
    }

    #[test]
    fn bypass_mode_short_circuits_everything() {
        assert_eq!(mode_short_circuit("bypass", "run_shell"), Some(Ok(())));
        assert_eq!(mode_short_circuit("bypass", "write_file"), Some(Ok(())));
    }

    #[test]
    fn manual_and_unknown_modes_never_short_circuit() {
        for mode in ["manual", "yolo"] {
            assert!(mode_short_circuit(mode, "write_file").is_none());
            assert!(mode_short_circuit(mode, "run_shell").is_none());
        }
    }

    #[test]
    fn plan_mode_short_circuits_to_an_error() {
        let decision = mode_short_circuit("plan", "run_shell").unwrap();
        assert!(decision.unwrap_err().contains("Plan Mode"));
    }
}
