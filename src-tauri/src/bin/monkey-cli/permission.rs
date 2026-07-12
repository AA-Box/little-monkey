//! Terminal-driven permission gate. Mirrors `src-tauri/src/permissions.rs`'s
//! mode semantics — manual/acceptEdits/smart/plan/auto/bypass, all six —
//! prompting on stdin/stdout instead of emitting a `permission://request`
//! event for a modal to answer, since there is no window here to emit to.
//! "plan" mode's chat-surface-shaped affordance (the desktop app's
//! `present_plan` tool + `PlanCard` approve button) is played out via
//! `agent.rs`'s `present_plan` dispatch arm prompting on stdin instead — see
//! that module for the terminal-side approve flow.

use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::path::Path;

use little_monkey_lib::permissions::path_risk_floor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    Manual,
    AcceptEdits,
    Smart,
    Plan,
    Auto,
    Bypass,
}

impl PermissionMode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "manual" => Ok(Self::Manual),
            "acceptEdits" => Ok(Self::AcceptEdits),
            "smart" => Ok(Self::Smart),
            "plan" => Ok(Self::Plan),
            "auto" => Ok(Self::Auto),
            "bypass" => Ok(Self::Bypass),
            other => Err(format!(
                "Unknown permission mode '{other}' (expected manual, acceptEdits, smart, plan, auto, or bypass)"
            )),
        }
    }
}

/// Mode-based short-circuit decision, mirroring the GUI's
/// `permissions::mode_short_circuit` exactly — including `run_shell`'s
/// never-short-circuits-outside-bypass invariant — but over a `PermissionMode`
/// enum instead of a raw mode string, and over a plain `floored: bool` rather
/// than a full `RiskAssessment`: the CLI has no LLM risk judge (Phase 4 of
/// docs/roadmap/p2-plan-act-safety.md explicitly skips it for the terminal),
/// so the deterministic [`path_risk_floor`] is the *only* risk signal
/// available here, not merely a veto over an otherwise-judge-supplied "low"
/// rating like the GUI's `"smart"` arm. `Some(result)` means the mode decides
/// on its own without prompting; `None` means fall through to the normal
/// prompting logic below. Factored out (like the GUI's version) so the table
/// is exercisable in tests without stdin, a filesystem, or an async runtime.
fn mode_short_circuit(mode: PermissionMode, tool: &str, floored: bool) -> Option<Result<(), String>> {
    match mode {
        PermissionMode::Bypass => Some(Ok(())),
        PermissionMode::Plan => Some(Err(format!(
            "Blocked: monkey-cli is in Plan Mode. Describe your plan instead of using {tool} — call the present_plan tool with your proposed plan, then approve it (\"y\" at its prompt) to switch to Act mode before making changes."
        ))),
        // `run_shell` is never auto-approved outside of bypass — same rule as
        // the GUI's `permissions.rs::mode_short_circuit` — so both
        // edit-approving modes behave identically here.
        PermissionMode::AcceptEdits | PermissionMode::Auto => {
            if tool == "write_file" || tool == "edit_file" || tool == "remember" {
                Some(Ok(()))
            } else {
                None
            }
        }
        // Only write_file/edit_file are ever eligible — run_shell (and
        // anything else) always falls through to `None` here, no matter what
        // `floored` says, exactly like `"acceptEdits"`/`"auto"` above. Unlike
        // the GUI's `"smart"` (which requires an LLM judge to have rated the
        // call "low" AND `!floored`), the CLI has no judge to consult at all,
        // so `!floored` alone is the auto-approve threshold — the floor
        // doubles as smart mode's sole signal here instead of just vetoing a
        // judge rating (see docs/roadmap/p2-plan-act-safety.md's "floor-only
        // smart mode" phrasing for this phase's accepted initial scope).
        PermissionMode::Smart => {
            if (tool == "write_file" || tool == "edit_file") && !floored {
                Some(Ok(()))
            } else {
                None
            }
        }
        PermissionMode::Manual => None,
    }
}

pub struct TerminalPermissions {
    mode: PermissionMode,
    session_allow: HashSet<String>,
}

impl TerminalPermissions {
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            session_allow: HashSet::new(),
        }
    }

    /// Current permission mode — read by `agent.rs` to decide whether
    /// `present_plan` should be offered to the model this turn (and to guard
    /// its dispatch arm against being invoked outside Plan Mode).
    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    /// Flips the in-process mode — the terminal counterpart of the desktop
    /// app's `permissionStore.setMode`. Currently only called by `agent.rs`'s
    /// `present_plan` dispatch arm on an approved plan (Plan → AcceptEdits);
    /// see that function's doc comment for why AcceptEdits specifically.
    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }

    /// Ask for permission to run `tool` (human-readable `detail` describing
    /// exactly what will happen). Blocks on stdin — fine for a CLI, unlike
    /// the GUI's async oneshot-channel wait for a modal click. No filesystem
    /// path to floor-check for `"smart"` mode — see [`request_with_path`]
    /// for `write_file`/`edit_file`, the only two tools that have one.
    ///
    /// [`request_with_path`]: TerminalPermissions::request_with_path
    pub async fn request(&mut self, tool: &str, detail: &str) -> Result<(), String> {
        self.request_inner(tool, detail, None).await
    }

    /// Same as [`request`](TerminalPermissions::request), but additionally
    /// supplies the resolved filesystem path (and canonical workspace root)
    /// so `"smart"` mode can consult [`path_risk_floor`] before deciding
    /// whether to auto-approve. Only `write_file`/`edit_file` have a concrete
    /// path to check — every other permission-gated tool goes through the
    /// path-less [`request`](TerminalPermissions::request) above instead.
    pub async fn request_with_path(&mut self, tool: &str, detail: &str, path: &Path, root: &Path) -> Result<(), String> {
        self.request_inner(tool, detail, Some((path, root))).await
    }

    async fn request_inner(&mut self, tool: &str, detail: &str, path: Option<(&Path, &Path)>) -> Result<(), String> {
        let floored = path.map(|(p, root)| path_risk_floor(p, root).is_some()).unwrap_or(false);

        if let Some(decision) = mode_short_circuit(self.mode, tool, floored) {
            return decision;
        }

        // `run_shell` is never remembered for the session — its blast
        // radius (arbitrary shell execution) is too large to silently
        // pre-authorize off the back of a single approval. Same rule as
        // the GUI's `NO_SESSION_REMEMBER`.
        if tool != "run_shell" && self.session_allow.contains(tool) {
            return Ok(());
        }

        // Fail closed instead of blocking forever when nothing can answer
        // this prompt: a piped/non-interactive stdin (CI, `task run`, a
        // recipe invoked from a script) would otherwise hang on
        // `read_line_blocking` indefinitely, or silently consume stray piped
        // bytes as if they were a "y". Checked here (after every mode that
        // can decide without prompting has already had its chance above) so
        // `bypass`/`acceptEdits`/`auto`/`smart`'s auto-approved calls and
        // `plan`'s block are completely unaffected by this guard.
        if !std::io::stdin().is_terminal() {
            return Err(non_interactive_denial(tool));
        }

        println!("\n--- Permission requested: {tool} ---\n{detail}\n");
        let remember_hint = if tool == "run_shell" { "" } else { " / [s]ession" };
        print!("Allow? [y]es / [N]o{remember_hint}: ");
        std::io::stdout().flush().ok();

        let answer = read_line_blocking().await.trim().to_lowercase();

        match answer.as_str() {
            "y" | "yes" => Ok(()),
            "s" | "session" if tool != "run_shell" => {
                self.session_allow.insert(tool.to_string());
                Ok(())
            }
            _ => Err("Permission denied".to_string()),
        }
    }
}

/// Message for the non-interactive-stdin fail-closed guard in
/// `request_inner`. Factored out (like `mode_short_circuit`) so its content
/// is directly testable without needing to fake `IsTerminal` in a unit test.
fn non_interactive_denial(tool: &str) -> String {
    format!(
        "Permission denied: {tool} requires an interactive terminal to approve, but stdin is not a TTY (non-interactive or piped input). Re-run in an interactive shell, or choose a permission mode that never prompts for this tool (bypass, or acceptEdits/auto for write_file/edit_file/remember)."
    )
}

/// Blocks on a line of stdin off the async executor's blocking pool. `pub(crate)`
/// so `agent.rs`'s `present_plan` dispatch arm can reuse the exact same
/// stdin-read primitive for its own "Approve plan and switch to act mode?"
/// prompt, rather than duplicating it.
pub(crate) async fn read_line_blocking() -> String {
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
        line
    })
    .await
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_every_valid_mode() {
        assert_eq!(PermissionMode::parse("manual"), Ok(PermissionMode::Manual));
        assert_eq!(PermissionMode::parse("acceptEdits"), Ok(PermissionMode::AcceptEdits));
        assert_eq!(PermissionMode::parse("smart"), Ok(PermissionMode::Smart));
        assert_eq!(PermissionMode::parse("plan"), Ok(PermissionMode::Plan));
        assert_eq!(PermissionMode::parse("auto"), Ok(PermissionMode::Auto));
        assert_eq!(PermissionMode::parse("bypass"), Ok(PermissionMode::Bypass));
    }

    #[test]
    fn parse_rejects_unknown_modes_and_lists_all_six_in_the_error() {
        let err = PermissionMode::parse("yolo").unwrap_err();
        assert!(err.contains("manual"));
        assert!(err.contains("acceptEdits"));
        assert!(err.contains("smart"));
        assert!(err.contains("plan"));
        assert!(err.contains("auto"));
        assert!(err.contains("bypass"));
    }

    #[test]
    fn bypass_short_circuits_every_tool() {
        assert_eq!(mode_short_circuit(PermissionMode::Bypass, "run_shell", true), Some(Ok(())));
        assert_eq!(mode_short_circuit(PermissionMode::Bypass, "write_file", false), Some(Ok(())));
    }

    #[test]
    fn plan_blocks_every_tool_regardless_of_which_one() {
        assert!(mode_short_circuit(PermissionMode::Plan, "write_file", false).unwrap().is_err());
        assert!(mode_short_circuit(PermissionMode::Plan, "run_shell", false).unwrap().is_err());
        assert!(mode_short_circuit(PermissionMode::Plan, "web_fetch", false).unwrap().is_err());
    }

    #[test]
    fn accept_edits_and_auto_approve_write_edit_remember_only() {
        for mode in [PermissionMode::AcceptEdits, PermissionMode::Auto] {
            assert_eq!(mode_short_circuit(mode, "write_file", false), Some(Ok(())));
            assert_eq!(mode_short_circuit(mode, "edit_file", false), Some(Ok(())));
            assert_eq!(mode_short_circuit(mode, "remember", false), Some(Ok(())));
            assert_eq!(mode_short_circuit(mode, "run_shell", false), None);
        }
    }

    /// The load-bearing regression test for this whole module: `run_shell`
    /// must NEVER be short-circuited by `"smart"` mode, no matter what
    /// `floored` says — mirrors the GUI's
    /// `smart_mode_never_short_circuits_run_shell` test guarding the same
    /// invariant in `permissions.rs`.
    #[test]
    fn smart_mode_never_short_circuits_run_shell() {
        assert_eq!(mode_short_circuit(PermissionMode::Smart, "run_shell", false), None);
        assert_eq!(mode_short_circuit(PermissionMode::Smart, "run_shell", true), None);
    }

    #[test]
    fn smart_mode_auto_approves_write_and_edit_only_when_not_floored() {
        assert_eq!(mode_short_circuit(PermissionMode::Smart, "write_file", false), Some(Ok(())));
        assert_eq!(mode_short_circuit(PermissionMode::Smart, "edit_file", false), Some(Ok(())));
        assert_eq!(mode_short_circuit(PermissionMode::Smart, "write_file", true), None);
        assert_eq!(mode_short_circuit(PermissionMode::Smart, "edit_file", true), None);
    }

    #[test]
    fn smart_mode_never_short_circuits_remember() {
        // Not eligible in the GUI's own "smart" arm either — only
        // write_file/edit_file are.
        assert_eq!(mode_short_circuit(PermissionMode::Smart, "remember", false), None);
    }

    #[test]
    fn manual_never_short_circuits_anything() {
        assert_eq!(mode_short_circuit(PermissionMode::Manual, "write_file", false), None);
        assert_eq!(mode_short_circuit(PermissionMode::Manual, "run_shell", false), None);
    }

    #[test]
    fn non_interactive_denial_names_the_tool_and_explains_why() {
        let msg = non_interactive_denial("write_file");
        assert!(msg.contains("write_file"));
        assert!(msg.contains("interactive terminal"));
        assert!(msg.contains("not a TTY"));
    }

    #[test]
    fn non_interactive_denial_suggests_a_mode_that_never_prompts() {
        let msg = non_interactive_denial("run_shell");
        assert!(msg.contains("bypass"));
    }

    #[test]
    fn path_risk_floor_wins_over_smart_mode_for_a_sensitive_path() {
        // End-to-end through the real `path_risk_floor`, not just the
        // `floored: bool` table above — pins that `request_with_path`'s
        // floor computation actually reaches `mode_short_circuit`.
        let root = Path::new("/ws");
        let env_path = Path::new("/ws/.env");
        assert!(path_risk_floor(env_path, root).is_some());
        assert_eq!(
            mode_short_circuit(PermissionMode::Smart, "write_file", path_risk_floor(env_path, root).is_some()),
            None
        );
    }
}
