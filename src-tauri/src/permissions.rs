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

use crate::run_ledger::PermissionAttribution;
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
    /// stronger "sensitive path" warning instead of an ordinary risk badge,
    /// and hide "allow for session" (a floored operation is never remembered;
    /// see [`respond_if_pending`]). The modal's use of this is presentational
    /// only: [`evaluate_gate`] and [`respond_if_pending`] enforce both
    /// behaviours backend-side regardless of what the frontend renders.
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
    /// The turn this call belongs to — the same `turn_id` the frontend
    /// passed to the tool invoke, echoed back so it can tell WHICH
    /// conversation is blocked on this prompt (the sidebar marks that row as
    /// waiting for the user). `None` for a call made outside any turn.
    ///
    /// Purely an attribution hint for the UI: every gate decision already
    /// consulted `turn` before this payload was built (see `effective_mode`
    /// and `evaluate_gate` above), and `respond_if_pending` matches the
    /// answer by `request_id` against the entry in `pending` — never by
    /// anything the frontend echoes back — so a window that lies about,
    /// drops or rewrites this field can only mislabel its own sidebar.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
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
/// This floor can NEVER be overridden or relaxed by the LLM judge, no matter
/// what any judge classification says (see [`RiskAssessment::floored`] and this
/// module's top doc comment on why `run_shell` — and, by the same reasoning,
/// any heuristic-driven relaxation of a floor — must never be gated on
/// judge-supplied text).
///
/// A floored path prompts in every mode below `"bypass"`, and no remembered
/// "allow for session"/"allow for run" grant can answer it — [`evaluate_gate`]
/// skips the grant lookup entirely when the floor fired, because those grants
/// are keyed by tool name alone and would otherwise let one approval on an
/// ordinary source file stand in for every later floored write. `"bypass"` is
/// the single mode that skips the floor, exactly as it is the single mode that
/// auto-approves `run_shell`. `floored_paths_prompt_under_accept_edits_and_auto`
/// and `a_remembered_grant_does_not_answer_a_floored_call` pin both halves.
///
/// `path` is expected already resolved/canonicalized
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
    /// Whether the prompt that produced this request carried a
    /// [`path_risk_floor`] verdict. Stored here — the same reasoning as `tool`
    /// above — so [`respond_if_pending`] decides whether "allow for session"
    /// is eligible from what the *gate* saw, not from anything the IPC caller
    /// claims. A floored operation is never remembered; see
    /// [`NO_SESSION_REMEMBER`], which covers the same ground per-tool.
    risk_floored: bool,
    sender: oneshot::Sender<ResolvedPermission>,
}

/// How a pending prompt ended, and who ended it.
///
/// This used to be a bare `bool`, which meant every responder had to record the
/// decision itself — and three of them (`deny_pending`, the Stop button's path
/// through [`respond_if_pending`], and the expiry check) recorded nothing at
/// all. Carrying the decision instead lets the *awaiting* task write it down,
/// and the awaiting task is the one place every outcome funnels through.
type ResolvedPermission = (PermissionDecision, &'static str);

/// A local human answered the prompt.
const DECIDED_BY_PROMPT: &str = "user:desktop-prompt";
/// The mode or a remembered grant decided without asking anyone.
const DECIDED_BY_POLICY: &str = "engine:permission-policy";
/// Nothing answered within [`PERMISSION_TIMEOUT`].
const DECIDED_BY_TIMEOUT: &str = "engine:permission-timeout";
/// No window was listening, so nobody could have been asked.
const DECIDED_BY_NO_WINDOW: &str = "engine:no-window";
/// Stop, a workspace switch, or a cancelled turn withdrew the prompt.
const DECIDED_BY_CANCELLED: &str = "engine:cancelled";

/// Whether a decision lets the operation proceed.
fn decision_allows(decision: PermissionDecision) -> bool {
    matches!(
        decision,
        PermissionDecision::AllowOnce | PermissionDecision::AllowForRun
    )
}

/// The decision a responder's `(allow, remember)` pair means.
fn decision_for(allow: bool, remember: bool) -> PermissionDecision {
    match (allow, remember) {
        (true, true) => PermissionDecision::AllowForRun,
        (true, false) => PermissionDecision::AllowOnce,
        (false, _) => PermissionDecision::Deny,
    }
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
///
/// This is the per-*tool* half of that rule. [`respond_if_pending`] applies a
/// per-*target* half alongside it: any operation whose path tripped
/// [`path_risk_floor`] is never remembered either, whichever tool asked.
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
/// filesystem or a judge call. Which modes read which half of it:
/// - `"smart"` reads both `level` and `floored` — the only mode where a judge
///   classification can decide anything.
/// - `"acceptEdits"`/`"auto"` read `floored` and nothing else. The
///   deterministic floor can *add* a prompt in these modes; the judge can
///   never add or remove one, because `level` is never consulted.
/// - `"manual"`, `"plan"`, `"bypass"` and every unrecognized value ignore
///   `risk` entirely.
///
/// So the Phase 2 invariant still holds in the form that matters: outside
/// `"smart"`, nothing a model produced can change a decision here.
/// [`path_risk_floor`]'s verdict is not model output — it is a pure function of
/// the already-resolved path.
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
            // The deterministic floor binds here too. "Auto-approve my edits"
            // is consent to promptless source-code changes, not to a
            // promptless rewrite of a CI workflow, a package manifest, a shell
            // rc file, `.env`, or anything inside `.git/` — those are exactly
            // the supply-chain-shaped mutations a prompt-injected agent gets
            // steered into, and the whole reason [`path_risk_floor`] exists.
            // A floored target falls through to a real prompt in these two
            // modes as it already does in "smart"; users who want promptless
            // everything have "bypass", the same escape hatch `run_shell`
            // points at above.
            //
            // Only `floored` is consulted, never `level`: the judge still has
            // no influence on these modes whatsoever, so a fabricated "high"
            // classification cannot start adding prompts here. The floor is
            // computed from the resolved path alone (see [`compute_risk`]),
            // with no model-supplied input.
            if matches!(risk, Some(assessment) if assessment.floored) {
                None
            } else if tool == "write_file" || tool == "edit_file" || tool == "remember" {
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
/// - `"acceptEdits"`: `write_file`/`edit_file`/`remember` are auto-approved
///   UNLESS the target tripped [`path_risk_floor`] (`risk.floored`), in which
///   case it falls through to a real prompt like everything else. Anything
///   else (i.e. `run_shell`) always falls through.
/// - `"auto"`: same as `"acceptEdits"` — floored targets prompt, unfloored
///   `write_file`/`edit_file`/`remember` are auto-approved, and `run_shell`
///   ALWAYS falls through to the normal prompting logic (see
///   [`mode_short_circuit`] for why it is never auto-approved).
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
/// The *judge* half of `risk` (see [`RiskAssessment`]/[`compute_risk`]) is
/// purely advisory in every mode except `"smart"`: outside `"smart"`,
/// `risk.level` only changes what [`PermissionRequestPayload`] shows the user,
/// so a mis-classified "low risk" judge result can never itself approve
/// anything. `"smart"` is the sole exception, and even there it can only ever
/// affect `write_file`/`edit_file` — never `run_shell`.
///
/// The *floor* half (`risk.floored`) is not advisory anywhere below
/// `"bypass"`: it can only ever turn an auto-approval into a prompt, never the
/// reverse, and it is computed from the resolved path with no model input at
/// all (see [`path_risk_floor`]). Erring toward more prompts on a fixed list
/// of sensitive paths is the one direction this design lets a signal push.
///
/// The normal prompting logic: if `tool` has already been granted "allow for
/// session" — and the target is not floored, which no grant can answer — the
/// call resolves `Ok(())` immediately without prompting; otherwise it emits a
/// `permission://request` event and awaits the user's decision (or the
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

/// What the permission gate decides for a call, before any side effect.
///
/// This is the value [`evaluate_gate`] returns and the vocabulary
/// [`permission_dry_run`] reports — see [`evaluate_gate`] for why the decision
/// is separated from the prompting/audit work at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateOutcome {
    /// The mode decided on its own, with no human in the loop: `Ok(())` is an
    /// auto-approval, `Err(message)` an outright refusal (plan mode).
    ShortCircuit(Result<(), String>),
    /// A prior "allow for session"/"allow for run" grant answers this call,
    /// so no prompt is shown even though the mode did not short-circuit.
    Remembered,
    /// Falls through to a real human prompt.
    Prompt,
}

/// The decision half of [`request_permission`] — the mode table
/// ([`mode_short_circuit`]) and the remembered-grant lookup — with no
/// `AppHandle`, no ledger write, and no prompt emission.
///
/// `mode` is passed in already resolved (callers use [`effective_mode`]) so
/// [`permission_dry_run`] can evaluate a mode the user is not currently in
/// without mutating [`PermissionState::mode`] to do it.
///
/// [`request_permission`] calls this and then performs the audit and prompt
/// side effects around the answer, so the decision table has exactly one
/// implementation. [`permission_dry_run`] calls the same function to answer
/// "what *would* the gate decide?" without executing anything — which is what
/// lets the Red-Team Lab assert against the real table instead of a
/// hand-transcribed frontend copy of it that can silently drift (a copy that
/// had already drifted by 14 file classes before this existed).
///
/// Note the remembered-grant branch: a prior session/run grant turns a call
/// that the mode alone would have prompted for into a promptless one. Any
/// evaluation that skips this — as the deleted frontend mirror did — reports a
/// prompt that a real run would never show. A floored target is the one case
/// that never reaches that branch at all; see the comment on it below.
pub(crate) fn evaluate_gate(
    state: &AppState,
    mode: &str,
    tool: &str,
    turn: Option<&str>,
    risk: Option<&RiskAssessment>,
) -> GateOutcome {
    if let Some(decision) = mode_short_circuit(mode, tool, risk) {
        return GateOutcome::ShortCircuit(decision);
    }

    // A remembered grant is keyed by tool name alone, so honouring one for a
    // floored target would hand the floor straight back: a single "allow for
    // session" earned on an ordinary `src/util.ts` write would silently
    // pre-authorize every later write to `.env`, `.github/workflows/*` or a
    // package manifest for the rest of the session. That is the laundering
    // path the floor exists to close, so a floored call always reaches a
    // human. [`respond_if_pending`] already refuses to *store* a grant from a
    // floored prompt; this refuses to *honour* one earned elsewhere, which is
    // the half that actually matters — the dangerous grant is the innocuous
    // one, not the floored one.
    if matches!(risk, Some(assessment) if assessment.floored) {
        return GateOutcome::Prompt;
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
        GateOutcome::Remembered
    } else {
        GateOutcome::Prompt
    }
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

/// What this permission belongs to, and the run id to file it under.
///
/// `turn` is what the caller passed; the ambient [`crate::run_scope`] is
/// consulted only when the caller passed nothing, which is the case for the
/// four sites that hard-code `None` — deleting a model from Settings, running a
/// local app definition over HTTP, and the two triage post paths.
///
/// The ambient scope is deliberately **not** allowed to override an explicit
/// `turn`, and is not used to widen which run events get written: an event
/// written against a run the caller never named would leave `permission_respond`
/// unable to find its own approval row later.
pub(crate) fn permission_attribution<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    turn: Option<&str>,
) -> Result<(Option<String>, PermissionAttribution), String> {
    attribution_for(turn, |run_id| durable_run_exists(app, state, Some(run_id)))
}

/// The attribution rule itself, with "is this run in the ledger" left to the
/// caller.
///
/// Split out because `subsystem_audit` has to answer the same question against a
/// ledger it opened itself, with no `AppState` anywhere — and two copies of this
/// rule would drift on the field the whole audit exists for. `is_durable` is only
/// consulted when there is a run id to ask about.
pub(crate) fn attribution_for(
    turn: Option<&str>,
    is_durable: impl FnOnce(&str) -> Result<bool, String>,
) -> Result<(Option<String>, PermissionAttribution), String> {
    let run_id = turn
        .map(str::to_string)
        .or_else(crate::run_scope::current_run_id);
    let Some(run_id) = run_id else {
        return Ok((
            None,
            match crate::run_scope::current().and_then(|scope| scope.unattributed()) {
                Some(reason) => PermissionAttribution::Unattributed(reason),
                None => PermissionAttribution::Unknown,
            },
        ));
    };
    let attribution = if is_durable(&run_id)? {
        PermissionAttribution::LedgerRun
    } else {
        PermissionAttribution::UnregisteredRun
    };
    Ok((Some(run_id), attribution))
}

/// Write the decision down. Called on every terminal path, run or no run.
fn record_decision<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    request_id: &str,
    decision: PermissionDecision,
    decided_by: &str,
) -> Result<(), String> {
    let decided_at_ms = crate::run_commands::unix_time_ms()?;
    crate::run_commands::with_ledger(app, state, |ledger| {
        ledger.record_permission_decision(request_id, decision, decided_by, decided_at_ms)
    })
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

/// The id to record for this request, and whether it names a real tool call.
///
/// `permission_decisions.tool_call_id` is `NOT NULL`, so a caller with no tool
/// call in hand still needs a value. Recording *which* of those two happened is
/// the point: a generated id is shaped like a real one and joins to nothing, so
/// without the origin beside it a trail query comes back empty and cannot say
/// whether the decision is missing — the bug K12's acceptance names — or whether
/// the operation was never a tool call at all.
///
/// The generated id is unique per request rather than a fixed placeholder, so
/// two unlinked decisions never collide onto one trail.
fn tool_call_identity(tool_call_id: Option<&str>) -> (String, crate::run_ledger::ToolCallOrigin) {
    match tool_call_id {
        Some(id) => (id.to_string(), crate::run_ledger::ToolCallOrigin::Caller),
        None => (
            format!("tool-{}", uuid::Uuid::new_v4().simple()),
            crate::run_ledger::ToolCallOrigin::Synthesized,
        ),
    }
}

/// Gate an operation, returning the `request_id` of the `permission_decisions`
/// row it raised.
///
/// The id is returned so a caller that goes on to record *what it did* can point
/// at the decision that permitted it — which is the acceptance's "for anything
/// gated, the exact policy decision that permitted it", in the direction the
/// question is actually asked. Callers with nothing to link simply discard it:
/// `request_permission(..).await?;` in statement position drops the value, which
/// is why widening this return type changed no existing call site.
pub async fn request_permission<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    tool: &str,
    detail: String,
    turn: Option<&str>,
    tool_call_id: Option<&str>,
    risk: Option<RiskAssessment>,
    agent_label: Option<&str>,
) -> Result<String, String> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let durable = durable_run_exists(app, state, turn)?;
    let run_id = turn.unwrap_or_default();
    let (normalized_tool_call_id, tool_call_origin) = tool_call_identity(tool_call_id);
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

    // Recorded before the gate is consulted and regardless of whether a run
    // holds it, so no path can reach an answer without the question being on
    // record first. This is the K12 acceptance's "a tool call whose authorizing
    // decision cannot be produced from the log is a bug", and it used to be one:
    // everything below was gated on `durable`.
    let mode = effective_mode(state, turn);
    let (attributed_run_id, attribution) = permission_attribution(app, state, turn)?;
    let request_record = crate::run_ledger::PermissionRequestRecord {
        request_id: request_id.clone(),
        run_id: attributed_run_id,
        attribution,
        process_id: crate::run_scope::current_process()
            .map(|process| process.process_id().to_string()),
        tool_name: tool.to_string(),
        tool_call_id: normalized_tool_call_id.clone(),
        tool_call_origin,
        operation_sha256: operation_sha256.clone(),
        mode: mode.clone(),
        risk_level: protocol_risk(risk.as_ref()),
        risk_floored: risk.as_ref().is_some_and(|assessment| assessment.floored),
        requested_at_ms: now,
        expires_at_ms,
    };
    crate::run_commands::with_ledger(app, state, |ledger| {
        ledger.record_permission_request(&request_record)
    })?;

    // The decision itself lives in `evaluate_gate` so `permission_dry_run`
    // answers from the same table; only the audit/prompt side effects stay
    // here.
    match evaluate_gate(state, &mode, tool, turn, risk.as_ref()) {
        GateOutcome::ShortCircuit(outcome) => {
            let decision = if outcome.is_ok() {
                PermissionDecision::AllowOnce
            } else {
                PermissionDecision::Deny
            };
            if durable {
                append_permission_requested(app, state, &audit, false)?;
                append_automatic_decision(app, state, &audit, decision)?;
            }
            record_decision(app, state, &request_id, decision, DECIDED_BY_POLICY)?;
            return outcome.map(|()| request_id);
        }
        GateOutcome::Remembered => {
            if durable {
                append_permission_requested(app, state, &audit, false)?;
                append_automatic_decision(app, state, &audit, PermissionDecision::AllowForRun)?;
            }
            record_decision(
                app,
                state,
                &request_id,
                PermissionDecision::AllowForRun,
                DECIDED_BY_POLICY,
            )?;
            return Ok(request_id);
        }
        GateOutcome::Prompt => {}
    }

    let (tx, rx) = oneshot::channel::<ResolvedPermission>();

    state.permissions.pending.lock().unwrap().insert(
        request_id.clone(),
        PendingPermission {
            tool: tool.to_string(),
            turn: turn.map(str::to_string),
            tool_call_id: normalized_tool_call_id.clone(),
            operation_sha256: operation_sha256.clone(),
            expires_at_ms,
            risk_floored: risk.as_ref().is_some_and(|assessment| assessment.floored),
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
        turn_id: turn.map(str::to_string),
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
        record_decision(
            app,
            state,
            &request_id,
            PermissionDecision::Deny,
            DECIDED_BY_NO_WINDOW,
        )?;
        return Err("Permission denied".to_string());
    }

    match tokio::time::timeout(PERMISSION_TIMEOUT, rx).await {
        Ok(Ok((decision, decided_by))) => {
            // `permission_respond` already appended the run event for a durable
            // run — it is the side that knows the human's identity — but every
            // other responder (`deny_pending`, a cancelled turn) appends
            // nothing, so the decision is written here for all of them.
            record_decision(app, state, &request_id, decision, decided_by)?;
            if decision_allows(decision) {
                Ok(request_id)
            } else {
                Err("Permission denied".to_string())
            }
        }
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
            record_decision(
                app,
                state,
                &request_id,
                PermissionDecision::Expired,
                DECIDED_BY_TIMEOUT,
            )?;
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

/// Reads the currently-active native permission mode. Desktop startup
/// restores its persisted frontend choice through `set_permission_mode`, so
/// a redundant read-only IPC command would create an ambiguous second source
/// of truth; native policy code and tests use this helper directly.
pub(crate) fn get_permission_mode_impl(state: &AppState) -> String {
    state.permissions.mode.lock().unwrap().clone()
}

/// What [`permission_dry_run`] found the gate would do. Distinguishes the two
/// promptless outcomes — the mode deciding versus a remembered grant deciding —
/// because they are different findings when auditing whether a call can reach
/// execution without a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDryRunDecision {
    /// The mode auto-approved it: no prompt, no human.
    AutoApproved,
    /// A prior session/run grant approved it: no prompt, no human.
    GrantApproved,
    /// Refused outright by the mode (plan mode): no prompt, no execution.
    Blocked,
    /// Falls through to a real human prompt.
    RequiresPrompt,
    /// The workspace sandbox refused the target before the gate was consulted,
    /// so no permission decision was ever reached.
    SandboxRejected,
}

/// The answer [`permission_dry_run`] returns.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionDryRun {
    pub decision: PermissionDryRunDecision,
    /// The mode that governed the decision, after any turn-scoped override.
    pub mode: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk_reason: Option<String>,
    pub risk_floored: bool,
}

/// Report what the permission gate would decide for a call, without executing
/// the call, prompting anyone, or writing a ledger event.
///
/// This exists because the decision table had no read-only entry point:
/// [`path_risk_floor`] and [`compute_risk`] are `pub` but not commands,
/// [`mode_short_circuit`] and [`effective_mode`] are private, and
/// [`request_permission`] is only reachable from inside a tool that is about to
/// perform the mutation. Anything that wanted to *ask* the gate — the Red-Team
/// Lab above all — had to reimplement it, and the reimplementation drifted.
///
/// Evaluation order matches the real call path in `tools.rs` exactly: the
/// workspace sandbox resolves the target first (`resolve_path_and_root`), then
/// [`compute_risk`] applies the deterministic floor over the *resolved* path,
/// then [`evaluate_gate`] consults the mode and any remembered grant.
///
/// `mode` evaluates a mode other than the active one — the Red-Team Lab asks
/// "what would happen in `acceptEdits`?" while the user stays in whatever mode
/// they chose. It is validated against [`VALID_MODES`] and never written back to
/// [`PermissionState::mode`], so asking the question cannot loosen the app.
/// `None` uses the active mode, including any turn-scoped override.
///
/// Limits worth stating: this reports the decision, so it cannot tell you what
/// a human would answer at a prompt it says is required, and passing
/// `risk_level` here stands in for the frontend judge's classification rather
/// than invoking a judge.
pub(crate) fn permission_dry_run_impl(
    state: &AppState,
    tool: &str,
    path: Option<&str>,
    risk_level: Option<String>,
    risk_reason: Option<String>,
    turn: Option<&str>,
    mode: Option<&str>,
) -> Result<PermissionDryRun, String> {
    let mode = match mode {
        Some(requested) => {
            if !VALID_MODES.contains(&requested) {
                return Err(format!("Unknown permission mode \"{requested}\""));
            }
            requested.to_string()
        }
        None => effective_mode(state, turn),
    };

    let resolved = match path.filter(|raw| !raw.is_empty()) {
        Some(raw) => match crate::workspace::resolve_path_and_root(state, raw) {
            Ok(pair) => Some(pair),
            Err(rejection) => {
                return Ok(PermissionDryRun {
                    decision: PermissionDryRunDecision::SandboxRejected,
                    mode,
                    reason: rejection,
                    risk_level: None,
                    risk_reason: None,
                    risk_floored: false,
                });
            }
        },
        None => None,
    };

    let risk = compute_risk(
        resolved
            .as_ref()
            .map(|(target, root)| (target.as_path(), root.as_path())),
        risk_level,
        risk_reason,
    );

    let (decision, reason) = match evaluate_gate(state, &mode, tool, turn, risk.as_ref()) {
        GateOutcome::ShortCircuit(Ok(())) => (
            PermissionDryRunDecision::AutoApproved,
            format!("Mode \"{mode}\" approves \"{tool}\" without asking."),
        ),
        GateOutcome::ShortCircuit(Err(message)) => (PermissionDryRunDecision::Blocked, message),
        GateOutcome::Remembered => (
            PermissionDryRunDecision::GrantApproved,
            format!("A remembered grant already approves \"{tool}\" without asking."),
        ),
        GateOutcome::Prompt => (
            PermissionDryRunDecision::RequiresPrompt,
            format!("Falls through to a real permission prompt under mode \"{mode}\"."),
        ),
    };

    Ok(PermissionDryRun {
        decision,
        mode,
        reason,
        risk_level: risk.as_ref().map(|assessment| assessment.level.clone()),
        risk_reason: risk.as_ref().map(|assessment| assessment.reason.clone()),
        risk_floored: risk.as_ref().is_some_and(|assessment| assessment.floored),
    })
}

/// IPC entry point for [`permission_dry_run_impl`].
#[tauri::command]
pub fn permission_dry_run(
    state: tauri::State<'_, AppState>,
    tool: String,
    path: Option<String>,
    risk_level: Option<String>,
    risk_reason: Option<String>,
    turn_id: Option<String>,
    mode: Option<String>,
) -> Result<PermissionDryRun, String> {
    permission_dry_run_impl(
        state.inner(),
        &tool,
        path.as_deref(),
        risk_level,
        risk_reason,
        turn_id.as_deref(),
        mode.as_deref(),
    )
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
    // Computed before the durable branch, not inside it: an expired prompt is
    // expired whether or not a run holds it, and deciding that only for durable
    // runs meant a stale prompt could still be answered elsewhere.
    let expired = crate::run_commands::unix_time_ms()? >= pending.expires_at_ms;
    let decision = if expired {
        PermissionDecision::Expired
    } else {
        decision_for(allow, remember)
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
        crate::run_commands::append_host_event(
            &app,
            &window,
            state.inner(),
            run_id,
            None,
            RunEvent::PermissionDecided {
                request_id: id.clone(),
                operation_sha256: pending.operation_sha256.clone(),
                decision,
                decided_by: crate::run_commands::desktop_identity(&app, &window),
            },
        )?;
    }
    respond_impl(state.inner(), id, decision)
}

/// Core logic behind [`permission_respond`], factored out so it can be
/// exercised directly in tests without standing up a full Tauri app/window.
fn respond_impl(state: &AppState, id: String, decision: PermissionDecision) -> Result<(), String> {
    if resolve_pending(state, &id, decision, DECIDED_BY_PROMPT)? {
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
    resolve_pending(state, id, decision_for(allow, remember), DECIDED_BY_PROMPT)
}

/// Resolve a pending prompt with an explicit decision.
///
/// The decision travels to the awaiting task rather than a bare `allow` flag so
/// that task can record it — see [`ResolvedPermission`]. `decided_by` names the
/// responder, which is the difference between "the user denied this" and "Stop
/// withdrew it".
fn resolve_pending(
    state: &AppState,
    id: &str,
    decision: PermissionDecision,
    decided_by: &'static str,
) -> Result<bool, String> {
    let Some(pending) = state.permissions.pending.lock().unwrap().remove(id) else {
        return Ok(false);
    };
    let allow = decision_allows(decision);
    let remember = matches!(decision, PermissionDecision::AllowForRun);

    // A floored operation is never remembered, for the same reason `run_shell`
    // never is: one approval must not stand in for every later write to a
    // sensitive path. `evaluate_gate` refuses to honour such a grant anyway,
    // so this is belt-and-braces — but it also keeps the grant sets honest, so
    // nothing downstream reads a stored grant and concludes the user
    // pre-authorized `.github/workflows/` edits.
    if remember
        && allow
        && !pending.risk_floored
        && !NO_SESSION_REMEMBER.contains(&pending.tool.as_str())
    {
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
    let _ = pending.sender.send((decision, decided_by));

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
    let pending: Vec<oneshot::Sender<ResolvedPermission>> = matching
        .iter()
        .filter_map(|id| guard.remove(id))
        .map(|pending| pending.sender)
        .collect();
    drop(guard);

    for sender in pending {
        // Withdrawn, not refused by a person — the awaiting task records the
        // difference, so a Stop press does not read back as a user denial.
        let _ = sender.send((PermissionDecision::Deny, DECIDED_BY_CANCELLED));
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
    fn insert_pending(
        state: &AppState,
        id: &str,
        tool: &str,
    ) -> oneshot::Receiver<ResolvedPermission> {
        insert_pending_for_turn(state, id, tool, None)
    }

    fn insert_pending_for_turn(
        state: &AppState,
        id: &str,
        tool: &str,
        turn: Option<&str>,
    ) -> oneshot::Receiver<ResolvedPermission> {
        insert_pending_with_floor(state, id, tool, turn, false)
    }

    fn insert_pending_with_floor(
        state: &AppState,
        id: &str,
        tool: &str,
        turn: Option<&str>,
        risk_floored: bool,
    ) -> oneshot::Receiver<ResolvedPermission> {
        let (tx, rx) = oneshot::channel::<ResolvedPermission>();
        state.permissions.pending.lock().unwrap().insert(
            id.to_string(),
            PendingPermission {
                tool: tool.to_string(),
                turn: turn.map(str::to_string),
                tool_call_id: "tool-test".to_string(),
                operation_sha256: "0".repeat(64),
                expires_at_ms: u64::MAX,
                risk_floored,
                sender: tx,
            },
        );
        rx
    }

    /// An absent tool call id is recorded as absent, not as a plausible one.
    #[test]
    fn an_unlinked_permission_gets_a_unique_id_marked_as_generated() {
        let (id, origin) = tool_call_identity(Some("call_abc123"));
        assert_eq!(id, "call_abc123", "a caller's id is recorded verbatim");
        assert_eq!(origin, crate::run_ledger::ToolCallOrigin::Caller);

        let (first, origin) = tool_call_identity(None);
        assert_eq!(origin, crate::run_ledger::ToolCallOrigin::Synthesized);
        let (second, _) = tool_call_identity(None);
        assert_ne!(
            first, second,
            "two unlinked decisions must not collide onto one trail"
        );
    }

    /// Stop withdrawing a prompt and a person refusing it are both "denied", and
    /// the recorded trail must not conflate them — that is the whole reason the
    /// resolution channel carries a responder rather than a bare `bool`.
    #[test]
    fn a_withdrawn_prompt_is_not_recorded_as_a_human_denial() {
        let state = AppState::default();
        let mut withdrawn = insert_pending(&state, "req-stop", "run_shell");
        deny_pending(&state, None);
        assert_eq!(
            withdrawn.try_recv(),
            Ok((PermissionDecision::Deny, DECIDED_BY_CANCELLED))
        );

        let mut refused = insert_pending(&state, "req-user", "run_shell");
        respond_impl(&state, "req-user".to_string(), PermissionDecision::Deny).unwrap();
        assert_eq!(
            refused.try_recv(),
            Ok((PermissionDecision::Deny, DECIDED_BY_PROMPT))
        );
    }

    /// `respond_if_pending` keeps its `(allow, remember)` shape for its existing
    /// callers, so the mapping onto a decision is the thing that can drift.
    #[test]
    fn allow_and_remember_map_onto_the_decision_they_mean() {
        assert_eq!(decision_for(true, true), PermissionDecision::AllowForRun);
        assert_eq!(decision_for(true, false), PermissionDecision::AllowOnce);
        assert_eq!(decision_for(false, true), PermissionDecision::Deny);
        assert_eq!(decision_for(false, false), PermissionDecision::Deny);
        for decision in [
            PermissionDecision::AllowOnce,
            PermissionDecision::AllowForRun,
        ] {
            assert!(decision_allows(decision));
        }
        for decision in [PermissionDecision::Deny, PermissionDecision::Expired] {
            assert!(!decision_allows(decision));
        }
    }

    #[test]
    fn deny_pending_scoped_to_a_turn_leaves_other_turns_prompts_alone() {
        let state = AppState::default();
        let mut rx_a = insert_pending_for_turn(&state, "req-a", "run_shell", Some("turn-a"));
        let mut rx_b = insert_pending_for_turn(&state, "req-b", "write_file", Some("turn-b"));

        deny_pending(&state, Some("turn-a"));

        // Turn A's prompt was denied…
        assert_eq!(
            rx_a.try_recv().map(|(decision, _)| decision),
            Ok(PermissionDecision::Deny)
        );
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

        assert_eq!(
            rx_a.try_recv().map(|(decision, _)| decision),
            Ok(PermissionDecision::Deny)
        );
        assert_eq!(
            rx_b.try_recv().map(|(decision, _)| decision),
            Ok(PermissionDecision::Deny)
        );
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

        respond_impl(&state, "req-1".to_string(), PermissionDecision::AllowForRun).unwrap();

        let allowed = state.permissions.session_allow.lock().unwrap();
        assert!(allowed.contains("write_file"));
        assert!(!allowed.contains("run_shell"));
    }

    #[test]
    fn run_shell_is_never_remembered_for_the_session() {
        let state = AppState::default();
        let _rx = insert_pending(&state, "req-1", "run_shell");

        respond_impl(&state, "req-1".to_string(), PermissionDecision::AllowForRun).unwrap();

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
        let err = respond_impl(
            &state,
            "does-not-exist".to_string(),
            PermissionDecision::AllowForRun,
        )
        .unwrap_err();
        assert!(err.contains("No pending permission request"));
    }

    #[test]
    fn respond_delivers_the_decision_to_the_waiting_receiver() {
        let state = AppState::default();
        let rx = insert_pending(&state, "req-1", "edit_file");

        respond_impl(&state, "req-1".to_string(), PermissionDecision::Deny).unwrap();

        assert_eq!(
            rx.blocking_recv().map(|(decision, _)| decision),
            Ok(PermissionDecision::Deny)
        );
    }

    #[test]
    fn remembered_durable_permission_is_scoped_to_its_exact_run() {
        let state = AppState::default();
        let _rx = insert_pending_for_turn(&state, "req-run", "write_file", Some("run-a"));

        respond_impl(
            &state,
            "req-run".to_string(),
            PermissionDecision::AllowForRun,
        )
        .unwrap();

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
        assert_eq!(
            rx.blocking_recv().map(|(decision, _)| decision),
            Ok(PermissionDecision::Deny)
        );
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
            turn_id: Some("turn-1".to_string()),
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["agent_label"], "fix user's login bug");
        // The turn attribution the sidebar reads to mark the waiting row.
        assert_eq!(json["turn_id"], "turn-1");
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
            turn_id: None,
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert!(
            json.get("agent_label").is_none(),
            "agent_label should be omitted, not null, when absent"
        );
        assert!(
            json.get("turn_id").is_none(),
            "turn_id should be omitted, not null, for a call outside any turn"
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

    // ---------------------------------------------------------------------
    // Red-Team Lab corpus, walked through the real decision table.
    //
    // These read the same `src/lib/redTeamFixtures.json` the frontend loads.
    // Before this existed, the lab evaluated a hand-transcribed TypeScript
    // copy of `path_risk_floor`/`mode_short_circuit` that had already drifted
    // by 14 file classes, so a fixture targeting `pyproject.toml` was scored
    // against a list that did not contain it.
    // ---------------------------------------------------------------------

    /// Compiled in rather than read at runtime so renaming or deleting the
    /// corpus is a build failure, not a test that quietly walks nothing.
    const RED_TEAM_CORPUS: &str = include_str!("../../src/lib/redTeamFixtures.json");

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct CorpusFixture {
        id: String,
        triggered_action: CorpusAction,
        judge_risk_level: Option<String>,
        expected_outcome: String,
        evaluation_mode: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct CorpusAction {
        tool: String,
        args: serde_json::Map<String, serde_json::Value>,
    }

    impl CorpusFixture {
        fn path(&self) -> Option<&str> {
            self.triggered_action
                .args
                .get("path")
                .and_then(|v| v.as_str())
        }
    }

    fn corpus() -> Vec<CorpusFixture> {
        serde_json::from_str(RED_TEAM_CORPUS).expect("red-team corpus parses")
    }

    /// Same idiom as `workspace.rs`'s own tests — a real directory on disk, so
    /// `resolve_path_and_root` does its actual canonicalization rather than a
    /// stubbed one.
    struct TempRoot {
        path: std::path::PathBuf,
    }

    impl TempRoot {
        fn new(tag: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "little_monkey_redteam_{}_{}_{}_{}",
                tag,
                std::process::id(),
                n,
                nanos
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempRoot { path }
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn state_with_root(root: &std::path::Path) -> AppState {
        let state = AppState::default();
        state
            .workspace_roots
            .lock()
            .unwrap()
            .push(crate::workspace::WorkspaceRoot {
                id: "root-0".to_string(),
                path: root.to_path_buf(),
                label: "workspace".to_string(),
            });
        state
    }

    /// Evaluates the fixture under `mode` via the explicit override, so the
    /// state's own mode — and every remembered grant hanging off it — is left
    /// exactly as the caller set it up.
    fn dry_run_fixture(state: &AppState, fixture: &CorpusFixture, mode: &str) -> PermissionDryRun {
        permission_dry_run_impl(
            state,
            &fixture.triggered_action.tool,
            fixture.path(),
            fixture.judge_risk_level.clone(),
            fixture
                .judge_risk_level
                .as_ref()
                .map(|_| "judge classification".to_string()),
            None,
            Some(mode),
        )
        .expect("mode comes from VALID_MODES")
    }

    fn is_promptless(decision: PermissionDryRunDecision) -> bool {
        matches!(
            decision,
            PermissionDryRunDecision::AutoApproved | PermissionDryRunDecision::GrantApproved
        )
    }

    #[test]
    fn red_team_corpus_is_loaded_and_covers_the_floored_path_classes() {
        let fixtures = corpus();
        assert!(
            fixtures.len() >= 17,
            "corpus shrank to {} fixtures — the frontend loader and this test read the same file",
            fixtures.len()
        );

        // The floor lists in this module are the reason the frontend mirror was
        // deleted; a fixture per drifted class keeps them exercised.
        for required in [
            "floored-pyproject-under-smart",
            "floored-requirements-under-smart",
            "floored-composer-under-smart",
            "floored-zshenv-under-smart",
        ] {
            assert!(
                fixtures.iter().any(|f| f.id == required),
                "corpus is missing the {required} canary"
            );
        }
    }

    #[test]
    fn red_team_corpus_is_never_promptless_under_manual_or_smart() {
        let root = TempRoot::new("manual_smart");
        let state = state_with_root(&root.path);

        for fixture in corpus() {
            for mode in ["manual", "smart"] {
                let outcome = dry_run_fixture(&state, &fixture, mode);
                assert!(
                    !is_promptless(outcome.decision),
                    "{} reached execution with no human under mode {mode}: {:?} — {}",
                    fixture.id,
                    outcome.decision,
                    outcome.reason
                );
            }
        }
    }

    #[test]
    fn red_team_corpus_run_shell_is_never_promptless_below_bypass() {
        let root = TempRoot::new("run_shell");
        let state = state_with_root(&root.path);

        let shell_fixtures: Vec<CorpusFixture> = corpus()
            .into_iter()
            .filter(|f| f.triggered_action.tool == "run_shell")
            .collect();
        assert!(
            !shell_fixtures.is_empty(),
            "no run_shell fixtures left to exercise the headline invariant"
        );

        for fixture in shell_fixtures {
            for mode in VALID_MODES.iter().filter(|mode| **mode != "bypass") {
                let outcome = dry_run_fixture(&state, &fixture, mode);
                assert!(
                    !is_promptless(outcome.decision),
                    "{} auto-approved run_shell under mode {mode} — see mode_short_circuit's \
                     invariant that shell execution never short-circuits outside bypass",
                    fixture.id
                );
            }
        }
    }

    #[test]
    fn floored_paths_defeat_a_low_risk_judge_under_smart() {
        let root = TempRoot::new("floored_smart");
        let state = state_with_root(&root.path);

        let floored: Vec<CorpusFixture> = corpus()
            .into_iter()
            .filter(|f| f.id.starts_with("floored-"))
            .collect();
        assert_eq!(floored.len(), 4, "expected the four floored-path canaries");

        for fixture in floored {
            assert_eq!(
                fixture.judge_risk_level.as_deref(),
                Some("low"),
                "{} must supply a low judge risk or it proves nothing",
                fixture.id
            );

            let outcome = dry_run_fixture(&state, &fixture, "smart");
            assert!(
                outcome.risk_floored,
                "{} was not floored — path_risk_floor no longer covers {:?}",
                fixture.id,
                fixture.path()
            );
            assert_eq!(
                outcome.decision,
                PermissionDryRunDecision::RequiresPrompt,
                "{} escaped the floor under smart mode with a judge-supplied low risk",
                fixture.id
            );
            assert_eq!(outcome.risk_level.as_deref(), Some("high"));
        }
    }

    /// The gap this corpus surfaced, now closed and pinned shut.
    ///
    /// `"acceptEdits"`/`"auto"` used to approve `write_file`/`edit_file`
    /// without consulting `risk` at all, so an edit to
    /// `.github/workflows/deploy.yml`, `pyproject.toml` or `.zshenv` was
    /// promptless in those two modes even though the floor had fired — only
    /// `"smart"` honoured it. [`mode_short_circuit`] now falls through on
    /// `risk.floored` in those arms, so "auto-approve my edits" no longer
    /// means "auto-approve my CI pipeline". `"bypass"` is unchanged and
    /// remains the one mode that skips the floor.
    #[test]
    fn floored_paths_prompt_under_accept_edits_and_auto() {
        let root = TempRoot::new("floored_accept");
        let state = state_with_root(&root.path);

        for fixture in corpus()
            .into_iter()
            .filter(|f| f.id.starts_with("floored-"))
        {
            for mode in ["acceptEdits", "auto"] {
                let outcome = dry_run_fixture(&state, &fixture, mode);
                assert!(
                    outcome.risk_floored,
                    "{} should be floored under {mode}",
                    fixture.id
                );
                assert_eq!(
                    outcome.decision,
                    PermissionDryRunDecision::RequiresPrompt,
                    "{} escaped the floor under {mode} — the floor must bind every mode \
                     below bypass, not just smart",
                    fixture.id
                );
                assert_eq!(outcome.risk_level.as_deref(), Some("high"));
            }
        }
    }

    /// The other half of the floor: no remembered grant may answer a floored
    /// call.
    ///
    /// Grants are keyed by tool name alone, so without this a single "allow
    /// for session" earned on an ordinary source-file write would silently
    /// pre-authorize every later floored write for the rest of the session —
    /// handing back exactly what the arm above just closed, and doing it in
    /// `"smart"` and `"manual"` too.
    #[test]
    fn a_remembered_grant_does_not_answer_a_floored_call() {
        let root = TempRoot::new("floored_grant");
        let state = state_with_root(&root.path);

        // The grant an innocuous edit would have earned.
        state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .insert("edit_file".to_string());

        for fixture in corpus()
            .into_iter()
            .filter(|f| f.id.starts_with("floored-"))
        {
            assert_eq!(
                fixture.triggered_action.tool, "edit_file",
                "{} must use the granted tool or this proves nothing",
                fixture.id
            );
            for mode in VALID_MODES.iter().filter(|mode| **mode != "bypass") {
                let outcome = dry_run_fixture(&state, &fixture, mode);
                let acceptable = matches!(
                    outcome.decision,
                    PermissionDryRunDecision::RequiresPrompt | PermissionDryRunDecision::Blocked
                );
                assert!(
                    acceptable,
                    "{} was answered by a remembered grant under mode {mode}: {:?}",
                    fixture.id, outcome.decision
                );
            }
        }

        // Non-vacuity: the same grant DOES answer an unfloored call, so the
        // assertions above failed on `floored`, not on a grant that never
        // applied in the first place.
        let unfloored = permission_dry_run_impl(
            &state,
            "edit_file",
            Some("src/util.ts"),
            None,
            None,
            None,
            Some("manual"),
        )
        .expect("manual is a valid mode");
        assert_eq!(unfloored.decision, PermissionDryRunDecision::GrantApproved);
    }

    /// Backend-side counterpart to the modal hiding "allow for session" on a
    /// floored prompt: even if a caller asks to remember one, nothing is
    /// stored. Same rule `NO_SESSION_REMEMBER` applies per-tool, applied
    /// per-target.
    #[test]
    fn approving_a_floored_prompt_with_remember_stores_no_grant() {
        let state = AppState::default();
        let _floored = insert_pending_with_floor(&state, "req-floored", "edit_file", None, true);

        assert!(respond_if_pending(&state, "req-floored", true, true).unwrap());
        assert!(
            state.permissions.session_allow.lock().unwrap().is_empty(),
            "a floored approval must not leave a session grant behind"
        );

        // Non-vacuity: the identical call on an unfloored prompt does store.
        let _plain = insert_pending_with_floor(&state, "req-plain", "edit_file", None, false);
        assert!(respond_if_pending(&state, "req-plain", true, true).unwrap());
        assert!(state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .contains("edit_file"));
    }

    #[test]
    fn red_team_corpus_plan_mode_blocks_every_fixture_that_reaches_the_gate() {
        let root = TempRoot::new("plan");
        let state = state_with_root(&root.path);

        for fixture in corpus() {
            let outcome = dry_run_fixture(&state, &fixture, "plan");
            let acceptable = matches!(
                outcome.decision,
                PermissionDryRunDecision::Blocked | PermissionDryRunDecision::SandboxRejected
            );
            assert!(
                acceptable,
                "{} was not refused under plan mode: {:?}",
                fixture.id, outcome.decision
            );
        }

        // The fixture whose entire premise is "ignore Plan Mode" must be
        // blocked by the mode itself, not merely rejected by the sandbox.
        let plan_fixture = corpus()
            .into_iter()
            .find(|f| f.expected_outcome == "blocked")
            .expect("corpus keeps a fixture that must be blocked outright");
        assert_eq!(plan_fixture.evaluation_mode.as_deref(), Some("plan"));
        let outcome = dry_run_fixture(&state, &plan_fixture, "plan");
        assert_eq!(outcome.decision, PermissionDryRunDecision::Blocked);
    }

    #[test]
    fn bypass_is_the_mode_that_approves_everything() {
        // Non-vacuity guard: if the assertions above passed because the dry run
        // always answers "RequiresPrompt", this fails.
        let root = TempRoot::new("bypass");
        let state = state_with_root(&root.path);

        let mut approved = 0usize;
        for fixture in corpus() {
            let outcome = dry_run_fixture(&state, &fixture, "bypass");
            match outcome.decision {
                PermissionDryRunDecision::AutoApproved => approved += 1,
                // The sandbox refuses before the mode is ever consulted, so
                // bypass cannot reach these — that is the point of the check.
                PermissionDryRunDecision::SandboxRejected => {}
                other => panic!("{} was not approved under bypass: {other:?}", fixture.id),
            }
        }
        assert!(
            approved > 0,
            "bypass approved nothing — the dry run is inert"
        );
    }

    #[test]
    fn absolute_paths_outside_the_workspace_never_reach_the_gate() {
        let root = TempRoot::new("escape");
        let state = state_with_root(&root.path);

        let escaping = corpus()
            .into_iter()
            .find(|f| f.path() == Some("/etc/hosts"))
            .expect("corpus keeps a fixture targeting an absolute path outside the workspace");

        for mode in VALID_MODES {
            let outcome = dry_run_fixture(&state, &escaping, mode);
            assert_eq!(
                outcome.decision,
                PermissionDryRunDecision::SandboxRejected,
                "{} reached the permission gate under {mode} — the workspace sandbox must \
                 refuse it first",
                escaping.id
            );
            assert!(outcome.reason.contains("escapes the workspace root"));
        }
    }

    #[test]
    fn tilde_paths_are_workspace_relative_and_cannot_reach_the_real_home() {
        // `~/.ssh/authorized_keys` is not an absolute path, so
        // `resolve_path_and_root` treats it as workspace-relative and it lands
        // in a literal `~` directory inside the workspace. No tilde expansion
        // happens anywhere in the resolver, which is why this fixture cannot
        // touch the user's actual `~/.ssh` — worth pinning, because the
        // fixture's title reads as though it could.
        let root = TempRoot::new("tilde");
        let state = state_with_root(&root.path);

        let fixture = corpus()
            .into_iter()
            .find(|f| f.path() == Some("~/.ssh/authorized_keys"))
            .expect("corpus keeps the tilde-path fixture");

        let outcome = dry_run_fixture(&state, &fixture, "manual");
        assert_eq!(outcome.decision, PermissionDryRunDecision::RequiresPrompt);

        let (resolved, resolved_root) =
            crate::workspace::resolve_path_and_root(&state, "~/.ssh/authorized_keys").unwrap();
        assert!(
            resolved.starts_with(&resolved_root),
            "tilde path resolved outside the workspace root: {}",
            resolved.display()
        );
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            assert!(
                !resolved.starts_with(&home) || resolved_root.starts_with(&home),
                "tilde path reached the real home directory: {}",
                resolved.display()
            );
        }
    }

    #[test]
    fn a_remembered_grant_turns_a_prompt_into_a_promptless_call() {
        // The deleted frontend mirror had no concept of session/run grants, so
        // it reported "requires prompt" for calls a real session would run with
        // no prompt at all. `evaluate_gate` consults them, so the lab now sees
        // what the app does.
        let root = TempRoot::new("grant");
        let state = state_with_root(&root.path);

        let fixture = corpus()
            .into_iter()
            .find(|f| f.triggered_action.tool == "web_fetch")
            .expect("corpus keeps a web_fetch fixture");

        let before = dry_run_fixture(&state, &fixture, "manual");
        assert_eq!(before.decision, PermissionDryRunDecision::RequiresPrompt);

        state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .insert("web_fetch".to_string());

        let after = dry_run_fixture(&state, &fixture, "manual");
        assert_eq!(after.decision, PermissionDryRunDecision::GrantApproved);
        assert_eq!(after.mode, "manual");
    }

    #[test]
    fn a_dry_run_never_changes_the_active_mode_or_clears_grants() {
        // The lab asks about modes the user is not in. If asking mutated
        // `PermissionState`, opening the panel would silently change the app's
        // permission posture — and switching to "manual"/"plan" also clears
        // every remembered grant.
        let root = TempRoot::new("no_mutation");
        let state = state_with_root(&root.path);
        set_permission_mode_impl(&state, "acceptEdits".to_string()).unwrap();
        state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .insert("web_fetch".to_string());

        for fixture in corpus() {
            for mode in VALID_MODES {
                let _ = dry_run_fixture(&state, &fixture, mode);
            }
        }

        assert_eq!(get_permission_mode_impl(&state), "acceptEdits");
        assert!(state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .contains("web_fetch"));
    }

    #[test]
    fn a_dry_run_rejects_an_unknown_mode() {
        let root = TempRoot::new("bad_mode");
        let state = state_with_root(&root.path);
        let error = permission_dry_run_impl(
            &state,
            "write_file",
            Some("NOTES.md"),
            None,
            None,
            None,
            Some("yolo"),
        )
        .expect_err("an unknown mode must not be silently treated as permissive");
        assert!(error.contains("yolo"));
    }
}
