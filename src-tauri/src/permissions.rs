//! Permission request/response system.
//!
//! Every mutating agent tool (write_file, edit_file, run_shell, remember — see tools.rs) must
//! call [`request_permission`] before doing anything destructive. This emits a
//! `permission://request` event to the frontend, which renders a modal
//! (Allow Once / Allow for Session / Deny). The frontend responds via the
//! `permission_respond` command, which resolves the oneshot channel that
//! `request_permission` is awaiting on.

use std::collections::{HashMap, HashSet};
use std::path::Path;
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
    /// Advisory risk annotation (Phase 2 of the Plan/Act + risk-adaptive
    /// permissions design — docs/roadmap/p2-plan-act-safety.md). `None` when
    /// risk annotations are off, the tool isn't classified, or the judge
    /// produced nothing usable. Purely informative in every mode as of this
    /// phase: it changes what the modal *shows*, never what gets
    /// auto-approved (that's Phase 3's "smart" mode, and even then
    /// `run_shell` never short-circuits on it — see [`RiskAssessment`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk_reason: Option<String>,
    /// Whether `risk_level`/`risk_reason` came from the authoritative
    /// [`path_risk_floor`] rather than the LLM judge — lets the modal show a
    /// stronger "sensitive path" warning instead of an ordinary risk badge.
    /// Always `false` when `risk_level` is `None`.
    pub risk_floored: bool,
}

/// A risk annotation attached to a permission prompt — either computed
/// deterministically by [`path_risk_floor`] (`floored: true`, always wins) or
/// filled in by the frontend's LLM risk judge (`floored: false`). See
/// [`request_permission`]'s `risk` parameter.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RiskAssessment {
    pub level: String,
    pub reason: String,
    pub floored: bool,
}

/// Shell startup/rc files: sourced automatically by every new interactive
/// shell, so an edit here runs attacker-controlled code on the user's next
/// terminal session, not just inside this workspace.
const SHELL_RC_FILES: &[&str] = &[
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".profile",
    ".zshrc",
    ".zprofile",
    ".zlogin",
    ".zshenv",
    ".cshrc",
    ".kshrc",
    ".inputrc",
];

/// Package manifests/lockfiles whose declared scripts (npm `postinstall`,
/// Cargo `build.rs`, etc.) can execute arbitrary code the next time someone
/// installs/builds the project — editing them is a supply-chain-shaped
/// mutation, not an ordinary source-code change.
const SCRIPT_EXECUTING_MANIFESTS: &[&str] = &[
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Cargo.toml",
    "Cargo.lock",
    "Gemfile",
    "Gemfile.lock",
    "requirements.txt",
    "Pipfile",
    "Pipfile.lock",
    "pyproject.toml",
    "composer.json",
    "composer.lock",
];

/// Deterministic, pure-`std` floor over sensitive workspace paths — the
/// authoritative Layer 1 of the risk-annotation design (Layer 2 is the
/// frontend LLM judge in `src/lib/riskJudge.ts`). Returns `Some(reason)` for
/// dotfiles/dot-dirs that hold secrets or CI config (`.env*`, inside `.git/`,
/// inside `.github/workflows/`), script-executing package
/// manifests/lockfiles, and shell rc files — `None` otherwise.
///
/// This floor can NEVER be overridden or relaxed by the LLM judge: a floored
/// path always prompts in every mode below `"bypass"`, no matter what any
/// judge classification says (see [`RiskAssessment::floored`] and this
/// module's top doc comment on why `run_shell` — and, by the same reasoning,
/// any heuristic-driven relaxation of a floor — must never be gated on
/// judge-supplied text). `path` is expected already resolved/canonicalized
/// (as `workspace::resolve_path_and_root` returns), `root` is that same
/// call's canonical workspace root, so a path outside the workspace can never
/// reach here in the first place (the sandbox already rejects it upstream).
pub fn path_risk_floor(path: &Path, root: &Path) -> Option<&'static str> {
    let rel = path.strip_prefix(root).unwrap_or(path);

    let components: Vec<&std::ffi::OsStr> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(part) => Some(part),
            _ => None,
        })
        .collect();

    if components.iter().any(|part| *part == ".git") {
        return Some("inside .git/ — version-control metadata");
    }

    if components.windows(2).any(|w| w[0] == ".github" && w[1] == "workflows") {
        return Some("inside .github/workflows/ — CI pipeline definition, runs with repo permissions");
    }

    let file_name = path.file_name().and_then(|n| n.to_str())?;

    if file_name.starts_with(".env") {
        return Some("environment/secrets file (.env*)");
    }

    if SHELL_RC_FILES.contains(&file_name) {
        return Some("shell startup/rc file — runs on every new shell");
    }

    if SCRIPT_EXECUTING_MANIFESTS.contains(&file_name) {
        return Some("package manifest/lockfile that can execute scripts on install/build");
    }

    None
}

/// Combines the deterministic floor with the (optional, frontend-supplied)
/// LLM judge result into the single [`RiskAssessment`] a permission prompt
/// carries. `path` is `Some((resolved_path, canonical_root))` for
/// `write_file`/`edit_file` (which have a filesystem target to floor-check)
/// and `None` for `run_shell` (no path — judge-only, display purposes only,
/// see this module's top doc comment). The floor always wins when it fires;
/// `judge_level` is defensively re-validated against the three known levels
/// here too (never trusted blindly from the IPC boundary, even though
/// `riskJudge.ts` already only ever sends one of the three) — anything else
/// (including a model-supplied value that slipped past the frontend's own
/// scrub, belt-and-braces) is discarded, resulting in no risk annotation at
/// all rather than a fabricated one.
pub fn compute_risk(
    path: Option<(&Path, &Path)>,
    judge_level: Option<String>,
    judge_reason: Option<String>,
) -> Option<RiskAssessment> {
    if let Some((resolved, root)) = path {
        if let Some(reason) = path_risk_floor(resolved, root) {
            return Some(RiskAssessment {
                level: "high".to_string(),
                reason: reason.to_string(),
                floored: true,
            });
        }
    }

    let level = judge_level.filter(|l| l == "low" || l == "medium" || l == "high")?;
    Some(RiskAssessment {
        level,
        reason: judge_reason.unwrap_or_default(),
        floored: false,
    })
}

/// Shared state tracking in-flight permission requests and tools that have
/// been granted "allow for session" status.
pub struct PermissionState {
    /// `id -> (tool the request was actually made for, owning turn id,
    /// response channel)`. The tool name is stored here (not just the
    /// sender) so [`permission_respond`] can use it as the *authoritative*
    /// source of truth for `session_allow` bookkeeping instead of trusting
    /// whatever tool name the IPC caller claims — see [`permission_respond`]
    /// for why that distinction matters. The turn id lets Stop deny only the
    /// aborted turn's prompts — with the split pane, another turn's prompt
    /// may be pending concurrently.
    pub pending: Mutex<HashMap<String, (String, Option<String>, oneshot::Sender<bool>)>>,
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
            "Blocked: Little Monkey is in Plan Mode. Describe your plan instead of using {tool} - call the present_plan tool with your proposed plan, then ask the user to approve it and switch out of Plan Mode before making changes."
        ))),
        "acceptEdits" | "auto" => {
            if tool == "write_file" || tool == "edit_file" || tool == "remember" {
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
/// - `"acceptEdits"`: `write_file`/`edit_file`/`remember` are auto-approved;
///   anything else (i.e. `run_shell`) falls through to the normal prompting
///   logic.
/// - `"auto"`: same as `"acceptEdits"` — `write_file`/`edit_file`/`remember`
///   are auto-approved, and `run_shell` ALWAYS falls through to the normal
///   prompting logic (see [`mode_short_circuit`] for why it is never
///   auto-approved).
/// - `"manual"`, or any unrecognized value (as a safe default): always falls
///   through to the normal prompting logic, unchanged.
///
/// `risk` (see [`RiskAssessment`]/[`compute_risk`]) is purely advisory as of
/// this phase: it only ever changes what [`PermissionRequestPayload`] shows
/// the user, never anything above — the mode short-circuit table is decided
/// with no knowledge of it whatsoever, so a mis-classified "low risk" judge
/// result can never itself approve anything.
///
/// The normal prompting logic: if `tool` has already been granted "allow for
/// session", resolves `Ok(())` immediately without prompting; otherwise emits
/// a `permission://request` event and awaits the user's decision (or the
/// timeout, which counts as a denial).
pub async fn request_permission<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    tool: &str,
    detail: String,
    turn: Option<&str>,
    risk: Option<RiskAssessment>,
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
        .insert(id.clone(), (tool.to_string(), turn.map(str::to_string), tx));

    let payload = PermissionRequestPayload {
        id: id.clone(),
        tool: tool.to_string(),
        detail,
        risk_level: risk.as_ref().map(|r| r.level.clone()),
        risk_reason: risk.as_ref().map(|r| r.reason.clone()),
        risk_floored: risk.as_ref().map(|r| r.floored).unwrap_or(false),
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

    let (tool, _turn, sender) = match entry {
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

/// Denies still-in-flight permission prompts WITHOUT touching "allow for
/// session" grants. `turn` of `Some` denies only that turn's prompts — used
/// by the Stop button's tool-cancellation path (see
/// `tools::tools_cancel_running`): a prompt belonging to a turn the user
/// just aborted must not sit on screen waiting for an answer, but with the
/// split pane the *other* pane's turn may have its own prompt pending, and
/// stopping one turn must not answer the other's. `None` denies everything
/// (workspace switch, legacy Stop path).
pub fn deny_pending(state: &AppState, turn: Option<&str>) {
    let mut guard = state.permissions.pending.lock().unwrap();
    let matching: Vec<String> = guard
        .iter()
        .filter(|(_, (_, owner, _))| turn.is_none() || owner.as_deref() == turn)
        .map(|(id, _)| id.clone())
        .collect();
    let pending: Vec<oneshot::Sender<bool>> = matching
        .iter()
        .filter_map(|id| guard.remove(id))
        .map(|(_, _, sender)| sender)
        .collect();
    drop(guard);

    for sender in pending {
        let _ = sender.send(false);
    }
}

/// Clears every "allow for session" grant scoped to MCP server `server_id`
/// (i.e. every `mcp:<server_id>:<tool>` entry in `session_allow`), without
/// touching grants for any other tool/server or denying in-flight prompts.
///
/// MCP grants are keyed only by the mutable `server_id`/tool-name strings
/// (see `mcp.rs::mcp_call_tool`'s `mcp:<server_id>:<tool_name>` format), with
/// no binding to what that id's transport (command/args/env/url) actually
/// points at. So this must be called whenever `server_id`'s config could
/// have just changed out from under an existing grant — `mcp_update_server`
/// (the transport a grant was approved against may no longer be what this
/// id now does) and `mcp_remove_server`/`mcp_add_server` (the id may be
/// about to be, or was just, reused by a completely different server) — so a
/// grant approved for one server can never silently keep applying to
/// whatever now answers to the same id.
pub fn revoke_session_allow_for_mcp_server(state: &AppState, server_id: &str) {
    let prefix = format!("mcp:{}:", server_id);
    state
        .permissions
        .session_allow
        .lock()
        .unwrap()
        .retain(|tool| !tool.starts_with(&prefix));
}

/// Clears every "allow for session" grant and denies any still-in-flight
/// permission prompts. Must be called whenever the workspace root changes:
/// a grant approved (or a prompt shown) in the context of one workspace must
/// never silently carry over and apply to a different one.
pub fn reset_for_new_workspace(state: &AppState) {
    state.permissions.session_allow.lock().unwrap().clear();
    deny_pending(state, None);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Directly inserts a pending request the way [`request_permission`]
    /// would, without needing a running app/window to emit an event through.
    fn insert_pending(state: &AppState, id: &str, tool: &str) -> oneshot::Receiver<bool> {
        insert_pending_for_turn(state, id, tool, None)
    }

    fn insert_pending_for_turn(
        state: &AppState,
        id: &str,
        tool: &str,
        turn: Option<&str>,
    ) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel::<bool>();
        state
            .permissions
            .pending
            .lock()
            .unwrap()
            .insert(id.to_string(), (tool.to_string(), turn.map(str::to_string), tx));
        rx
    }

    #[test]
    fn deny_pending_scoped_to_a_turn_leaves_other_turns_prompts_alone() {
        let state = AppState::default();
        let mut rx_a = insert_pending_for_turn(&state, "req-a", "run_shell", Some("turn-a"));
        let mut rx_b = insert_pending_for_turn(&state, "req-b", "write_file", Some("turn-b"));

        deny_pending(&state, Some("turn-a"));

        // Turn A's prompt was denied…
        assert_eq!(rx_a.try_recv(), Ok(false));
        // …turn B's is still pending, unanswered.
        assert!(rx_b.try_recv().is_err());
        assert!(state.permissions.pending.lock().unwrap().contains_key("req-b"));
    }

    #[test]
    fn deny_pending_unscoped_denies_everything() {
        let state = AppState::default();
        let mut rx_a = insert_pending_for_turn(&state, "req-a", "run_shell", Some("turn-a"));
        let mut rx_b = insert_pending(&state, "req-b", "write_file");

        deny_pending(&state, None);

        assert_eq!(rx_a.try_recv(), Ok(false));
        assert_eq!(rx_b.try_recv(), Ok(false));
        assert!(state.permissions.pending.lock().unwrap().is_empty());
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
    fn revoke_session_allow_for_mcp_server_clears_only_that_servers_grants() {
        let state = AppState::default();
        {
            let mut allowed = state.permissions.session_allow.lock().unwrap();
            allowed.insert("mcp:docs:search".to_string());
            allowed.insert("mcp:docs:write".to_string());
            allowed.insert("mcp:other:search".to_string());
            allowed.insert("write_file".to_string());
        }

        revoke_session_allow_for_mcp_server(&state, "docs");

        let allowed = state.permissions.session_allow.lock().unwrap();
        assert!(!allowed.contains("mcp:docs:search"));
        assert!(!allowed.contains("mcp:docs:write"));
        assert!(allowed.contains("mcp:other:search"), "a different server's grant must survive");
        assert!(allowed.contains("write_file"), "a non-MCP tool's grant must survive");
    }

    #[test]
    fn revoke_session_allow_for_mcp_server_is_a_no_op_when_nothing_is_granted() {
        let state = AppState::default();
        revoke_session_allow_for_mcp_server(&state, "never-granted"); // must not panic
        assert!(state.permissions.session_allow.lock().unwrap().is_empty());
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
    fn auto_and_accept_edits_short_circuit_only_file_edits_and_remember() {
        for mode in ["auto", "acceptEdits"] {
            assert_eq!(mode_short_circuit(mode, "write_file"), Some(Ok(())));
            assert_eq!(mode_short_circuit(mode, "edit_file"), Some(Ok(())));
            assert_eq!(mode_short_circuit(mode, "remember"), Some(Ok(())));
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
            assert!(mode_short_circuit(mode, "remember").is_none());
        }
    }

    #[test]
    fn plan_mode_short_circuits_to_an_error() {
        let decision = mode_short_circuit("plan", "run_shell").unwrap();
        assert!(decision.unwrap_err().contains("Plan Mode"));
    }

    #[test]
    fn plan_mode_blocks_remember_too() {
        // Plan mode blocks every mutating tool unconditionally — remember
        // (writes to app-data, not the workspace) is no exception.
        let decision = mode_short_circuit("plan", "remember").unwrap();
        assert!(decision.unwrap_err().contains("Plan Mode"));
    }

    #[test]
    fn bypass_mode_short_circuits_remember() {
        assert_eq!(mode_short_circuit("bypass", "remember"), Some(Ok(())));
    }

    // --- path_risk_floor / compute_risk (Phase 2 risk annotations) ---

    #[test]
    fn floor_flags_env_files() {
        let root = Path::new("/ws");
        assert!(path_risk_floor(Path::new("/ws/.env"), root).is_some());
        assert!(path_risk_floor(Path::new("/ws/.env.local"), root).is_some());
        assert!(path_risk_floor(Path::new("/ws/.env.production"), root).is_some());
    }

    #[test]
    fn floor_flags_inside_git_dir() {
        let root = Path::new("/ws");
        assert!(path_risk_floor(Path::new("/ws/.git/config"), root).is_some());
        assert!(path_risk_floor(Path::new("/ws/.git/hooks/pre-commit"), root).is_some());
    }

    #[test]
    fn floor_flags_github_workflows() {
        let root = Path::new("/ws");
        assert!(path_risk_floor(Path::new("/ws/.github/workflows/ci.yml"), root).is_some());
        // A file directly under .github (not workflows/) is not flagged by
        // this rule — only the workflows subtree runs with repo permissions.
        assert!(path_risk_floor(Path::new("/ws/.github/ISSUE_TEMPLATE.md"), root).is_none());
    }

    #[test]
    fn floor_flags_shell_rc_files() {
        let root = Path::new("/ws");
        for name in [".bashrc", ".zshrc", ".profile", ".zshenv"] {
            assert!(
                path_risk_floor(&root.join(name), root).is_some(),
                "{name} should be floored"
            );
        }
    }

    #[test]
    fn floor_flags_script_executing_manifests() {
        let root = Path::new("/ws");
        for name in ["package.json", "package-lock.json", "Cargo.toml", "Cargo.lock", "Gemfile"] {
            assert!(
                path_risk_floor(&root.join(name), root).is_some(),
                "{name} should be floored"
            );
        }
    }

    #[test]
    fn floor_does_not_flag_ordinary_source_files() {
        let root = Path::new("/ws");
        assert!(path_risk_floor(Path::new("/ws/src/main.rs"), root).is_none());
        assert!(path_risk_floor(Path::new("/ws/README.md"), root).is_none());
        // An ordinary dotfile that isn't in any of the documented categories
        // (e.g. editor/lint config) must NOT be swept up by an overbroad
        // "any dotfile" rule — only the specific documented categories flag.
        assert!(path_risk_floor(Path::new("/ws/.eslintrc"), root).is_none());
        assert!(path_risk_floor(Path::new("/ws/.prettierrc"), root).is_none());
    }

    #[test]
    fn compute_risk_floor_always_overrides_a_judge_result_even_when_judge_says_low() {
        // The central invariant: the deterministic floor is authoritative and
        // can never be relaxed by the LLM judge, no matter how confidently
        // the judge (which only ever sees untrusted-content-derived text)
        // claims a floored path is actually low risk.
        let root = Path::new("/ws");
        let path = Path::new("/ws/.env");
        let assessment = compute_risk(
            Some((path, root)),
            Some("low".to_string()),
            Some("looks like a harmless template".to_string()),
        )
        .unwrap();
        assert_eq!(assessment.level, "high");
        assert!(assessment.floored);
        assert!(assessment.reason.contains("environment/secrets"));
    }

    #[test]
    fn compute_risk_falls_back_to_the_judge_when_the_path_is_not_floored() {
        let root = Path::new("/ws");
        let path = Path::new("/ws/src/main.rs");
        let assessment = compute_risk(Some((path, root)), Some("medium".to_string()), Some("touches parsing logic".to_string()))
            .unwrap();
        assert_eq!(assessment.level, "medium");
        assert!(!assessment.floored);
        assert_eq!(assessment.reason, "touches parsing logic");
    }

    #[test]
    fn compute_risk_is_none_when_unfloored_and_judge_gave_nothing_usable() {
        let root = Path::new("/ws");
        let path = Path::new("/ws/src/main.rs");
        assert!(compute_risk(Some((path, root)), None, None).is_none());
        // A malformed/out-of-enum level (should never happen once riskJudge.ts
        // has already validated it, but defensively re-checked here too) is
        // discarded rather than trusted — fails closed to "no annotation".
        assert!(compute_risk(Some((path, root)), Some("critical".to_string()), Some("x".to_string())).is_none());
    }

    #[test]
    fn compute_risk_with_no_path_is_judge_only_never_floored() {
        // run_shell has no path to floor-check — a judge result is used
        // as-is (still purely advisory, never floored).
        let assessment = compute_risk(None, Some("high".to_string()), Some("deletes files".to_string())).unwrap();
        assert_eq!(assessment.level, "high");
        assert!(!assessment.floored);
    }
}
