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

use sha2::{Digest, Sha256};
use tauri::Emitter;
use tokio::sync::oneshot;

use crate::run_protocol::{PermissionDecision, RiskLevel, RunEvent};
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
    /// The description of the `code`-profile subagent (p3) this call
    /// originated from, if any — a dedicated, separately-serialized field
    /// (NOT folded into `detail` as a parsed-out-by-regex prefix, the
    /// pre-fix design) so the frontend never has to reparse free-text the
    /// model itself ultimately controls (the subagent's `description` comes
    /// straight from the model's own `task` tool-call arguments). Whatever
    /// characters (quotes, newlines, a fake "Subagent '...':`-looking
    /// string) the description contains, `PermissionModal.tsx` renders it
    /// verbatim in its own attribution line and `detail` is never touched —
    /// there is no delimiter for a crafted description to escape or forge a
    /// decoy line ahead of. `None` for every parent-turn call and any
    /// `explore`-profile subagent (mirrors `with_agent_label`'s old `None`
    /// case). Purely cosmetic/informational, same as before: this field has
    /// no path into [`compute_risk`]/`mode_short_circuit`/any auto-approval
    /// decision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_label: Option<String>,
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
///
/// Kept all-lowercase deliberately: [`path_risk_floor`] lowercases the
/// candidate file name before comparing against this list (case-insensitive
/// match — see that function's doc comment for why), so these literals must
/// already be lowercase or the comparison would never match, even though
/// some of these files' canonical on-disk spelling is mixed-case (e.g.
/// `Cargo.toml`, `Gemfile`).
const SCRIPT_EXECUTING_MANIFESTS: &[&str] = &[
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "cargo.toml",
    "cargo.lock",
    "gemfile",
    "gemfile.lock",
    "requirements.txt",
    "pipfile",
    "pipfile.lock",
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
///
/// All comparisons here are case-insensitive (via `.to_ascii_lowercase()` on
/// each path component/file name before matching against the — already
/// lowercase — literals above). macOS's default APFS and Windows' default
/// NTFS are both case-insensitive-but-case-preserving: a model-supplied path
/// like `.ZSHRC` or `PACKAGE.JSON` for a not-yet-existing file resolves
/// (`workspace::resolve_against_root`) with that exact casing preserved on
/// disk, yet is the *same file* `.zshrc`/`package.json` would be to the
/// filesystem, the shell, and every other tool. A case-sensitive comparison
/// here would let a case-variant filename sail past the floor entirely on
/// exactly the platforms this app ships on — folding case before comparing
/// keeps the floor authoritative regardless of the casing the model chose.
pub fn path_risk_floor(path: &Path, root: &Path) -> Option<&'static str> {
    let rel = path.strip_prefix(root).unwrap_or(path);

    let components: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(part) => part.to_str().map(|s| s.to_ascii_lowercase()),
            _ => None,
        })
        .collect();

    if components.iter().any(|part| part == ".git") {
        return Some("inside .git/ — version-control metadata");
    }

    if components
        .windows(2)
        .any(|w| w[0] == ".github" && w[1] == "workflows")
    {
        return Some(
            "inside .github/workflows/ — CI pipeline definition, runs with repo permissions",
        );
    }

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())?
        .to_ascii_lowercase();

    if file_name.starts_with(".env") {
        return Some("environment/secrets file (.env*)");
    }

    if SHELL_RC_FILES.contains(&file_name.as_str()) {
        return Some("shell startup/rc file — runs on every new shell");
    }

    if SCRIPT_EXECUTING_MANIFESTS.contains(&file_name.as_str()) {
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
pub struct PendingPermission {
    tool: String,
    turn: Option<String>,
    tool_call_id: String,
    operation_sha256: String,
    expires_at_ms: u64,
    sender: oneshot::Sender<bool>,
}

#[derive(Clone)]
struct PendingPermissionSnapshot {
    tool: String,
    turn: Option<String>,
    tool_call_id: String,
    operation_sha256: String,
    expires_at_ms: u64,
}

pub struct PermissionState {
    /// `id -> (tool the request was actually made for, owning turn id,
    /// response channel)`. The tool name is stored here (not just the
    /// sender) so [`permission_respond`] can use it as the *authoritative*
    /// source of truth for `session_allow` bookkeeping instead of trusting
    /// whatever tool name the IPC caller claims — see [`permission_respond`]
    /// for why that distinction matters. The turn id lets Stop deny only the
    /// aborted turn's prompts — with the split pane, another turn's prompt
    /// may be pending concurrently.
    pub pending: Mutex<HashMap<String, PendingPermission>>,
    pub session_allow: Mutex<HashSet<String>>,
    /// Remembered grants for a specific immutable run only. Durable turns
    /// never consult the legacy workspace-session grant set above.
    pub run_allow: Mutex<HashSet<(String, String)>>,
    /// Current permission mode — one of "manual"/"acceptEdits"/"plan"/"auto"/
    /// "bypass". See [`request_permission`] for what each mode does. Always
    /// boots at "manual" (see the `Default` impl below), regardless of
    /// whatever the frontend may have restored from its own storage — the
    /// frontend is responsible for pushing a restored non-"manual" mode back
    /// to [`set_permission_mode`] itself, once, at startup.
    pub mode: Mutex<String>,
    /// `turn_id -> mode`, consulted by [`request_permission`] (via
    /// [`effective_mode`]) *before* falling back to the global `mode` above.
    /// This is the turn-scoped counterpart to `pending`'s existing turn-id
    /// keying (see that field's doc comment) — a scheduled automation run
    /// (or any other single turn that needs its own mode) can set an
    /// override for just its own turn id via [`set_permission_mode_for_turn`]
    /// and clear it via [`clear_permission_mode_for_turn`] when done, without
    /// racing a concurrent split-pane turn's global mode. Purely additive:
    /// nothing that doesn't set an override is affected, so every existing
    /// Plan/Act/smart-mode call site keeps using the global `mode` exactly
    /// as before.
    pub turn_mode_overrides: Mutex<HashMap<String, String>>,
}

impl Default for PermissionState {
    fn default() -> Self {
        PermissionState {
            pending: Mutex::new(HashMap::new()),
            session_allow: Mutex::new(HashSet::new()),
            run_allow: Mutex::new(HashSet::new()),
            mode: Mutex::new("manual".to_string()),
            turn_mode_overrides: Mutex::new(HashMap::new()),
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
/// `pub(crate)` so `recipes.rs`'s `validate_recipe` can check a recipe's
/// `permission_mode` field against exactly this list — one source of truth
/// instead of a second hand-copied list that could drift.
pub(crate) const VALID_MODES: &[&str] =
    &["manual", "acceptEdits", "smart", "plan", "auto", "bypass"];

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
/// have "bypass" mode. This is the same invariant "smart" mode must uphold —
/// see the `"smart"` arm below, and the `smart_mode_never_short_circuits_run_shell`
/// regression test.
///
/// `risk` is the same [`RiskAssessment`] [`request_permission`] will go on to
/// show in the prompt payload if this falls through — passed in here (rather
/// than computed inside this function) so this stays a pure decision table
/// over already-known inputs, exercisable in tests without needing a
/// filesystem or a judge call. Only `"smart"` ever looks at it; every other
/// mode's decision is unchanged by whatever `risk` says (Phase 2's invariant
/// that risk annotations are purely advisory outside "smart" mode).
fn mode_short_circuit(
    mode: &str,
    tool: &str,
    risk: Option<&RiskAssessment>,
) -> Option<Result<(), String>> {
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
        "smart" => {
            // Only write_file/edit_file are ever eligible — run_shell (and
            // anything else) always falls through to `None` here, no matter
            // what `risk` claims, exactly like "auto"/"acceptEdits" above.
            if (tool == "write_file" || tool == "edit_file")
                && matches!(risk, Some(r) if r.level == "low" && !r.floored)
            {
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
/// - `"smart"` (Phase 3): `write_file`/`edit_file` are auto-approved ONLY
///   when `risk` is `Some` with `level == "low"` and `floored == false`;
///   every other case for those two tools, `remember`, and — critically —
///   `run_shell` in every case, falls through to the normal prompting logic.
///   `run_shell` NEVER short-circuits in `"smart"`, identical to `"auto"`/
///   `"acceptEdits"` above and for the same reason (see [`mode_short_circuit`]'s
///   doc comment).
/// - `"manual"`, or any unrecognized value (as a safe default): always falls
///   through to the normal prompting logic, unchanged.
///
/// `risk` (see [`RiskAssessment`]/[`compute_risk`]) is purely advisory in
/// every mode except `"smart"`: outside `"smart"` it only ever changes what
/// [`PermissionRequestPayload`] shows the user, never anything above — those
/// modes' short-circuit decisions are made with no knowledge of it
/// whatsoever, so a mis-classified "low risk" judge result can never itself
/// approve anything under them. `"smart"` is the sole, narrow exception, and
/// even there it can only ever affect `write_file`/`edit_file` — never
/// `run_shell` (see [`mode_short_circuit`]).
///
/// The normal prompting logic: if `tool` has already been granted "allow for
/// session", resolves `Ok(())` immediately without prompting; otherwise emits
/// a `permission://request` event and awaits the user's decision (or the
/// timeout, which counts as a denial).
/// Resolves the mode that should govern a given permission request: a
/// turn-scoped override (see [`PermissionState::turn_mode_overrides`]) wins
/// when `turn` is `Some` and one was set for that exact turn id, otherwise
/// falls back to the global `mode`. Factored out (like [`mode_short_circuit`])
/// so it's directly testable without a Tauri `AppHandle`.
fn effective_mode(state: &AppState, turn: Option<&str>) -> String {
    if let Some(turn_id) = turn {
        if let Some(overridden) = state
            .permissions
            .turn_mode_overrides
            .lock()
            .unwrap()
            .get(turn_id)
        {
            return overridden.clone();
        }
    }
    state.permissions.mode.lock().unwrap().clone()
}

fn operation_digest(run_id: &str, tool_call_id: &str, tool: &str, detail: &str) -> String {
    let mut hasher = Sha256::new();
    for value in [run_id, tool_call_id, tool, detail, "run"] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn protocol_risk(risk: Option<&RiskAssessment>) -> Option<RiskLevel> {
    match risk.map(|assessment| assessment.level.as_str()) {
        Some("low") => Some(RiskLevel::Low),
        Some("medium") => Some(RiskLevel::Medium),
        Some("high") => Some(RiskLevel::High),
        _ => None,
    }
}

fn durable_run_exists<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    turn: Option<&str>,
) -> Result<bool, String> {
    let Some(run_id) = turn else { return Ok(false) };
    crate::run_commands::with_ledger(app, state, |ledger| Ok(ledger.load_run(run_id)?.is_some()))
}

struct PermissionAudit<'a> {
    run_id: &'a str,
    request_id: &'a str,
    tool_call_id: &'a str,
    tool: &'a str,
    operation_sha256: &'a str,
    expires_at_ms: u64,
    risk: Option<&'a RiskAssessment>,
}

fn append_permission_requested<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    audit: &PermissionAudit<'_>,
    awaiting_human: bool,
) -> Result<(), String> {
    let identity = crate::run_commands::engine_identity(app, "permission-engine");
    crate::run_commands::append_event_as(
        app,
        state,
        audit.run_id.to_string(),
        None,
        RunEvent::PermissionRequested {
            request_id: audit.request_id.to_string(),
            tool_call_id: audit.tool_call_id.to_string(),
            tool_name: audit.tool.to_string(),
            operation_sha256: audit.operation_sha256.to_string(),
            expires_at_ms: audit.expires_at_ms,
            detail: format!("Approval required for {}", audit.tool),
            risk_level: protocol_risk(audit.risk),
            risk_reason: audit.risk.map(|assessment| {
                if assessment.floored {
                    assessment.reason.clone()
                } else {
                    "Advisory risk classification recorded; free-form classifier text was redacted"
                        .to_string()
                }
            }),
        },
        identity.clone(),
    )?;
    if awaiting_human {
        crate::run_commands::append_event_as(
            app,
            state,
            audit.run_id.to_string(),
            None,
            RunEvent::AwaitingApproval {
                request_id: audit.request_id.to_string(),
                operation_sha256: audit.operation_sha256.to_string(),
                expires_at_ms: audit.expires_at_ms,
                reason: Some("Waiting for a local user decision".to_string()),
            },
            identity,
        )?;
    }
    Ok(())
}

fn append_automatic_decision<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    audit: &PermissionAudit<'_>,
    decision: PermissionDecision,
) -> Result<(), String> {
    let identity = crate::run_commands::engine_identity(app, "permission-policy");
    crate::run_commands::append_event_as(
        app,
        state,
        audit.run_id.to_string(),
        None,
        RunEvent::PermissionDecided {
            request_id: audit.request_id.to_string(),
            operation_sha256: audit.operation_sha256.to_string(),
            decision,
            decided_by: identity.clone(),
        },
        identity,
    )?;
    Ok(())
}

pub async fn request_permission<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    tool: &str,
    detail: String,
    turn: Option<&str>,
    tool_call_id: Option<&str>,
    risk: Option<RiskAssessment>,
    agent_label: Option<&str>,
) -> Result<(), String> {
    let mode = effective_mode(state, turn);
    let request_id = uuid::Uuid::new_v4().to_string();
    let durable = durable_run_exists(app, state, turn)?;
    let run_id = turn.unwrap_or_default();
    let normalized_tool_call_id = tool_call_id
        .map(str::to_string)
        .unwrap_or_else(|| format!("tool-{}", uuid::Uuid::new_v4().simple()));
    let operation_sha256 = operation_digest(run_id, &normalized_tool_call_id, tool, &detail);
    let now = crate::run_commands::unix_time_ms()?;
    let expires_at_ms = now
        .checked_add(
            u64::try_from(PERMISSION_TIMEOUT.as_millis())
                .map_err(|_| "Permission timeout exceeds protocol bounds")?,
        )
        .ok_or_else(|| "Permission expiry exceeds protocol bounds".to_string())?;
    let audit = PermissionAudit {
        run_id,
        request_id: &request_id,
        tool_call_id: &normalized_tool_call_id,
        tool,
        operation_sha256: &operation_sha256,
        expires_at_ms,
        risk: risk.as_ref(),
    };

    if let Some(decision) = mode_short_circuit(&mode, tool, risk.as_ref()) {
        if durable {
            append_permission_requested(app, state, &audit, false)?;
            append_automatic_decision(
                app,
                state,
                &audit,
                if decision.is_ok() {
                    PermissionDecision::AllowOnce
                } else {
                    PermissionDecision::Deny
                },
            )?;
        }
        return decision;
    }

    let remembered = if let Some(run_id) = turn {
        state
            .permissions
            .run_allow
            .lock()
            .unwrap()
            .contains(&(run_id.to_string(), tool.to_string()))
    } else {
        state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .contains(tool)
    };
    if remembered {
        if durable {
            append_permission_requested(app, state, &audit, false)?;
            append_automatic_decision(app, state, &audit, PermissionDecision::AllowForRun)?;
        }
        return Ok(());
    }

    let (tx, rx) = oneshot::channel::<bool>();

    state.permissions.pending.lock().unwrap().insert(
        request_id.clone(),
        PendingPermission {
            tool: tool.to_string(),
            turn: turn.map(str::to_string),
            tool_call_id: normalized_tool_call_id.clone(),
            operation_sha256: operation_sha256.clone(),
            expires_at_ms,
            sender: tx,
        },
    );

    if durable {
        if let Err(error) = append_permission_requested(app, state, &audit, true) {
            state
                .permissions
                .pending
                .lock()
                .unwrap()
                .remove(&request_id);
            return Err(error);
        }
    }

    let payload = PermissionRequestPayload {
        id: request_id.clone(),
        tool: tool.to_string(),
        detail,
        risk_level: risk.as_ref().map(|r| r.level.clone()),
        risk_reason: risk.as_ref().map(|r| r.reason.clone()),
        risk_floored: risk.as_ref().map(|r| r.floored).unwrap_or(false),
        agent_label: agent_label.map(str::to_string),
    };

    if app.emit("permission://request", payload).is_err() {
        // No windows to receive the event — nobody can grant permission.
        state
            .permissions
            .pending
            .lock()
            .unwrap()
            .remove(&request_id);
        if durable {
            append_automatic_decision(app, state, &audit, PermissionDecision::Deny)?;
        }
        return Err("Permission denied".to_string());
    }

    match tokio::time::timeout(PERMISSION_TIMEOUT, rx).await {
        Ok(Ok(true)) => Ok(()),
        Ok(Ok(false)) => Err("Permission denied".to_string()),
        // Timed out, or the sender was dropped without a response.
        Ok(Err(_)) | Err(_) => {
            state
                .permissions
                .pending
                .lock()
                .unwrap()
                .remove(&request_id);
            if durable {
                append_automatic_decision(app, state, &audit, PermissionDecision::Expired)?;
            }
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
        state.permissions.run_allow.lock().unwrap().clear();
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

/// Sets a turn-scoped mode override, consulted by [`effective_mode`] before
/// the global mode for any [`request_permission`] call carrying this exact
/// `turn_id`. First real consumer: a scheduled automation run applying its
/// recipe's `permission_mode` to just its own turn, without touching (or
/// racing) whatever mode a concurrent split-pane turn is using.
#[tauri::command]
pub fn set_permission_mode_for_turn(
    state: tauri::State<'_, AppState>,
    turn_id: String,
    mode: String,
) -> Result<(), String> {
    set_permission_mode_for_turn_impl(state.inner(), turn_id, mode)
}

fn set_permission_mode_for_turn_impl(
    state: &AppState,
    turn_id: String,
    mode: String,
) -> Result<(), String> {
    if !VALID_MODES.contains(&mode.as_str()) {
        return Err("Unknown permission mode".to_string());
    }
    state
        .permissions
        .turn_mode_overrides
        .lock()
        .unwrap()
        .insert(turn_id, mode);
    Ok(())
}

/// Removes a turn-scoped mode override, if any — [`effective_mode`] falls
/// back to the global mode for that turn id again immediately. Callers
/// should always clear their override when their turn ends (success,
/// failure, or cancellation) so the map doesn't accumulate stale entries.
#[tauri::command]
pub fn clear_permission_mode_for_turn(
    state: tauri::State<'_, AppState>,
    turn_id: String,
) -> Result<(), String> {
    state
        .permissions
        .turn_mode_overrides
        .lock()
        .unwrap()
        .remove(&turn_id);
    state
        .permissions
        .run_allow
        .lock()
        .unwrap()
        .retain(|(run_id, _)| run_id != &turn_id);
    Ok(())
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
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
    id: String,
    allow: bool,
    remember: bool,
) -> Result<(), String> {
    // Team Mode (ROADMAP.md Phase 6) gate: responding to a pending
    // permission request (allow or deny) requires the active team member to
    // have Approver or Owner role. A complete no-op — see
    // `team_mode::require_approver`'s doc comment — when no team members
    // have ever been configured, so solo users see no behavior change.
    crate::team_mode::require_approver(&app, state.inner())?;

    let pending = {
        let guard = state.permissions.pending.lock().unwrap();
        let pending = guard
            .get(&id)
            .ok_or_else(|| format!("No pending permission request with id {id}"))?;
        PendingPermissionSnapshot {
            tool: pending.tool.clone(),
            turn: pending.turn.clone(),
            tool_call_id: pending.tool_call_id.clone(),
            operation_sha256: pending.operation_sha256.clone(),
            expires_at_ms: pending.expires_at_ms,
        }
    };
    if durable_run_exists(&app, state.inner(), pending.turn.as_deref())? {
        let run_id = pending
            .turn
            .clone()
            .expect("durable permission has a run id");
        let approval = crate::run_commands::with_ledger(&app, state.inner(), |ledger| {
            ledger.load_approval(&run_id, &id)?.ok_or_else(|| {
                crate::run_ledger::LedgerError::NotFound {
                    entity: "approval",
                    id: id.clone(),
                }
            })
        })?;
        if approval.tool_call_id != pending.tool_call_id
            || approval.tool_name != pending.tool
            || approval.operation_sha256 != pending.operation_sha256
            || approval.expires_at_ms != pending.expires_at_ms
        {
            return Err(
                "Pending permission does not match its immutable ledger approval".to_string(),
            );
        }
        let expired = crate::run_commands::unix_time_ms()? >= pending.expires_at_ms;
        crate::run_commands::append_host_event(
            &app,
            &window,
            state.inner(),
            run_id,
            None,
            RunEvent::PermissionDecided {
                request_id: id.clone(),
                operation_sha256: pending.operation_sha256.clone(),
                decision: if expired {
                    PermissionDecision::Expired
                } else if allow {
                    if remember {
                        PermissionDecision::AllowForRun
                    } else {
                        PermissionDecision::AllowOnce
                    }
                } else {
                    PermissionDecision::Deny
                },
                decided_by: crate::run_commands::desktop_identity(&app, &window),
            },
        )?;
        if expired {
            return respond_impl(state.inner(), id, false, false);
        }
    }
    respond_impl(state.inner(), id, allow, remember)
}

/// Core logic behind [`permission_respond`], factored out so it can be
/// exercised directly in tests without standing up a full Tauri app/window.
fn respond_impl(state: &AppState, id: String, allow: bool, remember: bool) -> Result<(), String> {
    if respond_if_pending(state, &id, allow, remember)? {
        Ok(())
    } else {
        Err(format!("No pending permission request with id {id}"))
    }
}

pub(crate) fn respond_if_pending(
    state: &AppState,
    id: &str,
    allow: bool,
    remember: bool,
) -> Result<bool, String> {
    let Some(pending) = state.permissions.pending.lock().unwrap().remove(id) else {
        return Ok(false);
    };

    if remember && allow && !NO_SESSION_REMEMBER.contains(&pending.tool.as_str()) {
        if let Some(turn) = &pending.turn {
            state
                .permissions
                .run_allow
                .lock()
                .unwrap()
                .insert((turn.clone(), pending.tool.clone()));
        } else {
            state
                .permissions
                .session_allow
                .lock()
                .unwrap()
                .insert(pending.tool.clone());
        }
    }

    // If the receiving end was already dropped (e.g. the request timed out
    // just before the user clicked), there's nothing left to notify.
    let _ = pending.sender.send(allow);

    Ok(true)
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
        .filter(|(_, pending)| turn.is_none() || pending.turn.as_deref() == turn)
        .map(|(id, _)| id.clone())
        .collect();
    let pending: Vec<oneshot::Sender<bool>> = matching
        .iter()
        .filter_map(|id| guard.remove(id))
        .map(|pending| pending.sender)
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
    state
        .permissions
        .run_allow
        .lock()
        .unwrap()
        .retain(|(_, tool)| !tool.starts_with(&prefix));
}

/// Clears every "allow for session" grant and denies any still-in-flight
/// permission prompts. Must be called whenever the workspace root changes:
/// a grant approved (or a prompt shown) in the context of one workspace must
/// never silently carry over and apply to a different one.
pub fn reset_for_new_workspace(state: &AppState) {
    state.permissions.session_allow.lock().unwrap().clear();
    state.permissions.run_allow.lock().unwrap().clear();
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
        state.permissions.pending.lock().unwrap().insert(
            id.to_string(),
            PendingPermission {
                tool: tool.to_string(),
                turn: turn.map(str::to_string),
                tool_call_id: "tool-test".to_string(),
                operation_sha256: "0".repeat(64),
                expires_at_ms: u64::MAX,
                sender: tx,
            },
        );
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
        assert!(state
            .permissions
            .pending
            .lock()
            .unwrap()
            .contains_key("req-b"));
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
    fn remembered_durable_permission_is_scoped_to_its_exact_run() {
        let state = AppState::default();
        let _rx = insert_pending_for_turn(&state, "req-run", "write_file", Some("run-a"));

        respond_impl(&state, "req-run".to_string(), true, true).unwrap();

        let grants = state.permissions.run_allow.lock().unwrap();
        assert!(grants.contains(&("run-a".to_string(), "write_file".to_string())));
        assert!(!grants.contains(&("run-b".to_string(), "write_file".to_string())));
        assert!(state.permissions.session_allow.lock().unwrap().is_empty());
    }

    #[test]
    fn operation_digest_binds_run_tool_call_tool_and_exact_detail() {
        let baseline = operation_digest("run-a", "tool-a", "run_shell", "echo safe");
        assert_eq!(baseline.len(), 64);
        assert_ne!(
            baseline,
            operation_digest("run-b", "tool-a", "run_shell", "echo safe")
        );
        assert_ne!(
            baseline,
            operation_digest("run-a", "tool-b", "run_shell", "echo safe")
        );
        assert_ne!(
            baseline,
            operation_digest("run-a", "tool-a", "write_file", "echo safe")
        );
        assert_ne!(
            baseline,
            operation_digest("run-a", "tool-a", "run_shell", "echo secret")
        );
        assert!(!baseline.contains("echo"));
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
        assert!(
            allowed.contains("mcp:other:search"),
            "a different server's grant must survive"
        );
        assert!(
            allowed.contains("write_file"),
            "a non-MCP tool's grant must survive"
        );
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
    fn set_permission_mode_keeps_session_allow_when_switching_to_smart() {
        // "smart" counts as neither tightening nor loosening — same
        // treatment as "acceptEdits"/"auto": switching into it must not wipe
        // out grants a stricter earlier mode never had reason to clear.
        let state = AppState::default();
        state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .insert("write_file".to_string());

        set_permission_mode_impl(&state, "smart".to_string()).unwrap();

        assert!(state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .contains("write_file"));
    }

    #[test]
    fn set_permission_mode_does_not_treat_smart_as_tightening() {
        // Mirrors `set_permission_mode_clears_session_allow_when_tightening_to_*`
        // but asserts the opposite for "smart" — it must NOT be in the
        // tightening set alongside "manual"/"plan".
        let state = AppState::default();
        state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .insert("edit_file".to_string());

        set_permission_mode_impl(&state, "smart".to_string()).unwrap();

        assert!(!state.permissions.session_allow.lock().unwrap().is_empty());
    }

    #[test]
    fn valid_modes_includes_smart() {
        assert!(VALID_MODES.contains(&"smart"));
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
        assert!(mode_short_circuit("auto", "run_shell", None).is_none());
    }

    #[test]
    fn smart_mode_never_short_circuits_run_shell() {
        // Twin of `auto_mode_never_short_circuits_run_shell` for Phase 3's
        // "smart" mode — the load-bearing invariant restated in the design
        // doc: the LLM risk judge must NEVER be allowed to influence
        // run_shell, in any mode, no matter how confidently it (or a
        // fabricated assessment) claims "low" risk.
        assert!(mode_short_circuit("smart", "run_shell", None).is_none());
        let low = RiskAssessment {
            level: "low".to_string(),
            reason: "looks harmless".to_string(),
            floored: false,
        };
        assert!(mode_short_circuit("smart", "run_shell", Some(&low)).is_none());
    }

    #[test]
    fn auto_and_accept_edits_short_circuit_only_file_edits_and_remember() {
        for mode in ["auto", "acceptEdits"] {
            assert_eq!(mode_short_circuit(mode, "write_file", None), Some(Ok(())));
            assert_eq!(mode_short_circuit(mode, "edit_file", None), Some(Ok(())));
            assert_eq!(mode_short_circuit(mode, "remember", None), Some(Ok(())));
            assert!(mode_short_circuit(mode, "run_shell", None).is_none());
        }
    }

    #[test]
    fn smart_mode_auto_approves_write_and_edit_only_when_risk_is_low_and_unfloored() {
        let low = RiskAssessment {
            level: "low".to_string(),
            reason: "trivial rename".to_string(),
            floored: false,
        };
        assert_eq!(
            mode_short_circuit("smart", "write_file", Some(&low)),
            Some(Ok(()))
        );
        assert_eq!(
            mode_short_circuit("smart", "edit_file", Some(&low)),
            Some(Ok(()))
        );
    }

    #[test]
    fn smart_mode_falls_through_for_write_and_edit_when_risk_is_medium_or_high() {
        for level in ["medium", "high"] {
            let risk = RiskAssessment {
                level: level.to_string(),
                reason: "reason".to_string(),
                floored: false,
            };
            assert!(mode_short_circuit("smart", "write_file", Some(&risk)).is_none());
            assert!(mode_short_circuit("smart", "edit_file", Some(&risk)).is_none());
        }
    }

    #[test]
    fn smart_mode_falls_through_for_write_and_edit_when_risk_is_none() {
        // No classification available (judge disabled/timed out/unparseable)
        // — fails closed to a normal prompt, exactly like every other
        // "unknown" case in this design.
        assert!(mode_short_circuit("smart", "write_file", None).is_none());
        assert!(mode_short_circuit("smart", "edit_file", None).is_none());
    }

    #[test]
    fn smart_mode_never_auto_approves_a_floored_low_risk_path() {
        // The deterministic path floor is authoritative and can never be
        // relaxed by the judge — a floored path stays "low" only because
        // `compute_risk` never actually emits `floored: true` with a level
        // other than "high" in practice, but this pins the short-circuit
        // table's own defense-in-depth: `floored: true` always falls through
        // no matter what `level` says.
        let floored_low = RiskAssessment {
            level: "low".to_string(),
            reason: "floored anyway".to_string(),
            floored: true,
        };
        assert!(mode_short_circuit("smart", "write_file", Some(&floored_low)).is_none());
        assert!(mode_short_circuit("smart", "edit_file", Some(&floored_low)).is_none());
    }

    #[test]
    fn smart_mode_never_short_circuits_remember() {
        // `remember` is not write_file/edit_file — "smart" only ever
        // auto-approves those two tools, unlike "auto"/"acceptEdits" which
        // also cover `remember`.
        let low = RiskAssessment {
            level: "low".to_string(),
            reason: "reason".to_string(),
            floored: false,
        };
        assert!(mode_short_circuit("smart", "remember", Some(&low)).is_none());
        assert!(mode_short_circuit("smart", "remember", None).is_none());
    }

    #[test]
    fn bypass_mode_short_circuits_everything() {
        assert_eq!(
            mode_short_circuit("bypass", "run_shell", None),
            Some(Ok(()))
        );
        assert_eq!(
            mode_short_circuit("bypass", "write_file", None),
            Some(Ok(()))
        );
    }

    #[test]
    fn manual_and_unknown_modes_never_short_circuit() {
        for mode in ["manual", "yolo"] {
            assert!(mode_short_circuit(mode, "write_file", None).is_none());
            assert!(mode_short_circuit(mode, "run_shell", None).is_none());
            assert!(mode_short_circuit(mode, "remember", None).is_none());
        }
    }

    #[test]
    fn plan_mode_short_circuits_to_an_error() {
        let decision = mode_short_circuit("plan", "run_shell", None).unwrap();
        assert!(decision.unwrap_err().contains("Plan Mode"));
    }

    #[test]
    fn plan_mode_blocks_remember_too() {
        // Plan mode blocks every mutating tool unconditionally — remember
        // (writes to app-data, not the workspace) is no exception.
        let decision = mode_short_circuit("plan", "remember", None).unwrap();
        assert!(decision.unwrap_err().contains("Plan Mode"));
    }

    #[test]
    fn bypass_mode_short_circuits_remember() {
        assert_eq!(mode_short_circuit("bypass", "remember", None), Some(Ok(())));
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
        for name in [
            "package.json",
            "package-lock.json",
            "Cargo.toml",
            "Cargo.lock",
            "Gemfile",
        ] {
            assert!(
                path_risk_floor(&root.join(name), root).is_some(),
                "{name} should be floored"
            );
        }
    }

    #[test]
    fn floor_flags_case_variant_filenames() {
        // Case-insensitive-but-case-preserving filesystems (default on both
        // macOS/APFS and Windows/NTFS) treat ".ZSHRC" and ".zshrc" as the
        // same on-disk file — the floor must fire on the case variant too,
        // or "smart" mode could silently auto-approve writing what is
        // effectively a shell rc file / .env / script-executing manifest.
        let root = Path::new("/ws");
        assert!(path_risk_floor(&root.join(".ZSHRC"), root).is_some());
        assert!(path_risk_floor(&root.join(".Env"), root).is_some());
        assert!(path_risk_floor(&root.join(".ENV.LOCAL"), root).is_some());
        assert!(path_risk_floor(&root.join("PACKAGE.JSON"), root).is_some());
        assert!(path_risk_floor(&root.join("Cargo.TOML"), root).is_some());
        assert!(path_risk_floor(&root.join(".Git").join("config"), root).is_some());
        assert!(
            path_risk_floor(&root.join(".GitHub").join("Workflows").join("ci.yml"), root).is_some()
        );
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
        let assessment = compute_risk(
            Some((path, root)),
            Some("medium".to_string()),
            Some("touches parsing logic".to_string()),
        )
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
        assert!(compute_risk(
            Some((path, root)),
            Some("critical".to_string()),
            Some("x".to_string())
        )
        .is_none());
    }

    #[test]
    fn compute_risk_with_no_path_is_judge_only_never_floored() {
        // run_shell has no path to floor-check — a judge result is used
        // as-is (still purely advisory, never floored).
        let assessment = compute_risk(
            None,
            Some("high".to_string()),
            Some("deletes files".to_string()),
        )
        .unwrap();
        assert_eq!(assessment.level, "high");
        assert!(!assessment.floored);
    }

    // `PermissionRequestPayload.agent_label` (the fix for the review finding
    // that flagged `tools.rs`'s old `with_agent_label` detail-prefixing as
    // spoofable/corruptible by a crafted subagent description or command
    // string): the subagent attribution is now carried as its OWN
    // serialized field, entirely independent of `detail`, so there is no
    // string for a model-supplied description/command to forge a prefix
    // into. Pinned here as a plain serde round-trip on the payload shape
    // itself — `request_permission`'s actual emit path is covered
    // end-to-end via `tools.rs`'s IPC-level tests instead, since exercising
    // the emitted event itself needs a real window/listener this module's
    // other tests deliberately avoid setting up.
    #[test]
    fn permission_request_payload_serializes_agent_label_as_its_own_field_when_present() {
        let payload = PermissionRequestPayload {
            id: "req-1".to_string(),
            tool: "write_file".to_string(),
            detail: "Write 12 bytes to a.txt".to_string(),
            risk_level: None,
            risk_reason: None,
            risk_floored: false,
            agent_label: Some("fix user's login bug".to_string()),
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["agent_label"], "fix user's login bug");
        // The detail string is untouched by the label — no "Subagent '...'"
        // prefix baked into it, unlike the pre-fix `with_agent_label` design.
        assert_eq!(json["detail"], "Write 12 bytes to a.txt");
    }

    #[test]
    fn permission_request_payload_omits_agent_label_when_absent() {
        let payload = PermissionRequestPayload {
            id: "req-2".to_string(),
            tool: "write_file".to_string(),
            detail: "Write 3 bytes to b.txt".to_string(),
            risk_level: None,
            risk_reason: None,
            risk_floored: false,
            agent_label: None,
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert!(
            json.get("agent_label").is_none(),
            "agent_label should be omitted, not null, when absent"
        );
    }

    #[test]
    fn effective_mode_falls_back_to_the_global_mode_when_no_turn_is_given() {
        let state = AppState::default();
        set_permission_mode_impl(&state, "acceptEdits".to_string()).unwrap();
        assert_eq!(effective_mode(&state, None), "acceptEdits");
    }

    #[test]
    fn effective_mode_falls_back_to_the_global_mode_when_the_turn_has_no_override() {
        let state = AppState::default();
        set_permission_mode_impl(&state, "acceptEdits".to_string()).unwrap();
        assert_eq!(effective_mode(&state, Some("turn-a")), "acceptEdits");
    }

    #[test]
    fn effective_mode_prefers_a_turn_scoped_override_over_the_global_mode() {
        let state = AppState::default();
        set_permission_mode_impl(&state, "manual".to_string()).unwrap();
        set_permission_mode_for_turn_impl(&state, "turn-a".to_string(), "bypass".to_string())
            .unwrap();

        assert_eq!(effective_mode(&state, Some("turn-a")), "bypass");
        // A concurrent turn with no override of its own is unaffected.
        assert_eq!(effective_mode(&state, Some("turn-b")), "manual");
        assert_eq!(effective_mode(&state, None), "manual");
    }

    #[test]
    fn set_permission_mode_for_turn_impl_rejects_an_unknown_mode() {
        let state = AppState::default();
        let err =
            set_permission_mode_for_turn_impl(&state, "turn-a".to_string(), "yolo".to_string())
                .unwrap_err();
        assert_eq!(err, "Unknown permission mode");
        assert!(state
            .permissions
            .turn_mode_overrides
            .lock()
            .unwrap()
            .get("turn-a")
            .is_none());
    }

    #[test]
    fn clearing_a_turns_override_falls_back_to_the_global_mode_again() {
        let state = AppState::default();
        set_permission_mode_impl(&state, "manual".to_string()).unwrap();
        set_permission_mode_for_turn_impl(&state, "turn-a".to_string(), "bypass".to_string())
            .unwrap();
        assert_eq!(effective_mode(&state, Some("turn-a")), "bypass");

        // `clear_permission_mode_for_turn`'s `#[tauri::command]` wrapper is a
        // one-line passthrough to this same map removal — exercised directly
        // here rather than through a mocked `tauri::State`, matching every
        // other command in this module's test style.
        state
            .permissions
            .turn_mode_overrides
            .lock()
            .unwrap()
            .remove("turn-a");

        assert_eq!(effective_mode(&state, Some("turn-a")), "manual");
    }

    #[test]
    fn clear_permission_mode_for_turn_impl_removes_only_the_named_turns_override() {
        let state = AppState::default();
        state
            .permissions
            .turn_mode_overrides
            .lock()
            .unwrap()
            .insert("turn-a".to_string(), "bypass".to_string());
        state
            .permissions
            .turn_mode_overrides
            .lock()
            .unwrap()
            .insert("turn-b".to_string(), "auto".to_string());

        state
            .permissions
            .turn_mode_overrides
            .lock()
            .unwrap()
            .remove("turn-a");

        assert_eq!(effective_mode(&state, Some("turn-a")), "manual");
        assert_eq!(effective_mode(&state, Some("turn-b")), "auto");
    }
}
