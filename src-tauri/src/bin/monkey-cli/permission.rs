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
use std::sync::Arc;

use little_monkey_lib::permissions::path_risk_floor;
use little_monkey_lib::run_protocol::{PermissionDecision, RiskLevel, RunEvent};

use crate::durable_run::{
    operation_sha256, safe_protocol_id, sha256_hex, unix_time_ms, CliRunEventSink,
};

const DEFAULT_APPROVAL_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1_000;

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
    /// The flag spelling of this mode — what `--permission-mode` would take
    /// to reproduce the current session, so the REPL banner and the
    /// launcher's settings screen name it the same way.
    pub fn label(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::AcceptEdits => "acceptEdits",
            Self::Smart => "smart",
            Self::Plan => "plan",
            Self::Auto => "auto",
            Self::Bypass => "bypass",
        }
    }

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
fn mode_short_circuit(
    mode: PermissionMode,
    tool: &str,
    floored: bool,
) -> Option<Result<(), String>> {
    match mode {
        PermissionMode::Bypass => Some(Ok(())),
        PermissionMode::Plan => Some(Err(format!(
            "Blocked: monkey is in Plan Mode. Describe your plan instead of using {tool} — call the present_plan tool with your proposed plan, then approve it (\"y\" at its prompt) to switch to Act mode before making changes."
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
    event_sink: Option<Arc<dyn CliRunEventSink>>,
    current_tool: Option<ToolPermissionContext>,
    next_permission_nonce: u64,
    approval_timeout_ms: u64,
    quiet: bool,
    allow_network: bool,
    allow_external_mutations: bool,
    channel_send: Option<little_monkey_lib::run_protocol::ChannelSendPolicy>,
}

#[derive(Clone)]
struct ToolPermissionContext {
    tool_call_id: String,
    operation_sha256: String,
}

impl TerminalPermissions {
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            session_allow: HashSet::new(),
            event_sink: None,
            current_tool: None,
            next_permission_nonce: 1,
            approval_timeout_ms: DEFAULT_APPROVAL_TIMEOUT_MS,
            quiet: false,
            allow_network: true,
            allow_external_mutations: false,
            channel_send: None,
        }
    }

    /// Adds the durable observer used by `task run`. Ordinary interactive
    /// CLI sessions continue to use [`new`](Self::new), which has no ledger
    /// dependency and therefore preserves their existing behavior.
    pub fn with_event_sink(
        mode: PermissionMode,
        event_sink: Arc<dyn CliRunEventSink>,
        approval_timeout_ms: u64,
        quiet: bool,
    ) -> Self {
        Self {
            mode,
            session_allow: HashSet::new(),
            event_sink: Some(event_sink),
            current_tool: None,
            next_permission_nonce: 1,
            approval_timeout_ms: approval_timeout_ms.clamp(60_000, DEFAULT_APPROVAL_TIMEOUT_MS),
            quiet,
            allow_network: true,
            allow_external_mutations: false,
            channel_send: None,
        }
    }

    pub fn set_allow_network(&mut self, allow: bool) {
        self.allow_network = allow;
    }

    pub fn allow_network(&self) -> bool {
        self.allow_network
    }

    /// Whether this run may cause an effect outside the machine — sending a
    /// message, placing a call — as its immutable snapshot recorded. Default
    /// false: an interactive session that never set it has not been granted it,
    /// and the tools that read this refuse rather than prompt.
    pub fn set_allow_external_mutations(&mut self, allow: bool) {
        self.allow_external_mutations = allow;
    }

    pub fn allow_external_mutations(&self) -> bool {
        self.allow_external_mutations
    }

    /// The run's cross-conversation/cross-account messaging grant, as its
    /// immutable snapshot recorded. `None` — the default, and what every run
    /// without an explicit grant carries — means reply-only.
    pub fn set_channel_send(
        &mut self,
        policy: Option<little_monkey_lib::run_protocol::ChannelSendPolicy>,
    ) {
        self.channel_send = policy;
    }

    pub fn channel_send(&self) -> Option<&little_monkey_lib::run_protocol::ChannelSendPolicy> {
        self.channel_send.as_ref()
    }

    pub fn event_sink(&self) -> Option<Arc<dyn CliRunEventSink>> {
        self.event_sink.clone()
    }

    /// Bind any nested permission request to the exact model-proposed tool
    /// operation. The scope is the frozen canonical workspace root.
    pub fn begin_tool_call(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        raw_arguments: &str,
        scope: &str,
    ) {
        self.current_tool = Some(ToolPermissionContext {
            tool_call_id: safe_protocol_id("tool-call", tool_call_id),
            operation_sha256: operation_sha256(tool_name, raw_arguments, scope),
        });
    }

    pub fn finish_tool_call(&mut self) {
        self.current_tool = None;
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
    pub async fn request_with_path(
        &mut self,
        tool: &str,
        detail: &str,
        path: &Path,
        root: &Path,
    ) -> Result<(), String> {
        self.request_inner(tool, detail, Some((path, root))).await
    }

    async fn request_inner(
        &mut self,
        tool: &str,
        detail: &str,
        path: Option<(&Path, &Path)>,
    ) -> Result<(), String> {
        let floor_reason = path.and_then(|(candidate, root)| path_risk_floor(candidate, root));
        let floored = floor_reason.is_some();

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
            if daemon_approval_wait_enabled() {
                let request = self
                    .emit_permission_request(tool, detail, floor_reason)?
                    .ok_or_else(|| {
                        "Daemon approval wait requires a durable run event sink".to_string()
                    })?;
                let decision = self.wait_for_daemon_decision(&request).await?;
                return match decision {
                    PermissionDecision::AllowOnce => Ok(()),
                    PermissionDecision::AllowForRun => {
                        if tool != "run_shell" {
                            self.session_allow.insert(tool.to_string());
                        }
                        Ok(())
                    }
                    PermissionDecision::Deny => Err("Permission denied".to_string()),
                    PermissionDecision::Expired => {
                        Err("Permission denied: approval request expired".to_string())
                    }
                };
            }
            if let Some(request) = self.emit_permission_request(tool, detail, floor_reason)? {
                self.emit_permission_decision(&request, PermissionDecision::Deny)?;
            }
            return Err(non_interactive_denial(tool));
        }

        let observed_request = self.emit_permission_request(tool, detail, floor_reason)?;

        if self.quiet {
            eprintln!("\n--- Permission requested: {tool} ---\n{detail}\n");
        } else {
            println!("\n--- Permission requested: {tool} ---\n{detail}\n");
        }
        let remember_hint = if tool == "run_shell" {
            ""
        } else {
            " / [s]ession"
        };
        if self.quiet {
            eprint!("Allow? [y]es / [N]o{remember_hint}: ");
            std::io::stderr().flush().ok();
        } else {
            print!("Allow? [y]es / [N]o{remember_hint}: ");
            std::io::stdout().flush().ok();
        }

        let answer = read_line_blocking().await.trim().to_lowercase();

        let (result, decision) = match answer.as_str() {
            "y" | "yes" => (Ok(()), PermissionDecision::AllowOnce),
            "s" | "session" if tool != "run_shell" => {
                self.session_allow.insert(tool.to_string());
                (Ok(()), PermissionDecision::AllowForRun)
            }
            _ => (
                Err("Permission denied".to_string()),
                PermissionDecision::Deny,
            ),
        };

        if let Some(request) = observed_request {
            let decision = if unix_time_ms()? >= request.expires_at_ms {
                PermissionDecision::Expired
            } else {
                decision
            };
            self.emit_permission_decision(&request, decision.clone())?;
            if decision == PermissionDecision::Expired {
                return Err("Permission denied: approval request expired".to_string());
            }
        }
        result
    }

    fn emit_permission_request(
        &mut self,
        tool: &str,
        _detail: &str,
        floor_reason: Option<&str>,
    ) -> Result<Option<ObservedPermissionRequest>, String> {
        let (Some(sink), Some(context)) = (self.event_sink.as_ref(), self.current_tool.clone())
        else {
            return Ok(None);
        };
        let nonce = self.next_permission_nonce;
        self.next_permission_nonce = self.next_permission_nonce.saturating_add(1);
        let request_seed = format!(
            "{}:{}:{nonce}",
            context.tool_call_id, context.operation_sha256
        );
        let request_id = format!("permission-{}", &sha256_hex(request_seed.as_bytes())[..24]);
        let expires_at_ms = unix_time_ms()?
            .checked_add(self.approval_timeout_ms)
            .ok_or_else(|| "permission expiry timestamp overflow".to_string())?;
        let tool_name = safe_protocol_id("tool", tool);
        let audit_detail = format!(
            "Permission requested for {tool_name}; the exact operation is redacted and bound by operation_sha256."
        );
        sink.emit(RunEvent::PermissionRequested {
            request_id: request_id.clone(),
            tool_call_id: context.tool_call_id,
            tool_name,
            operation_sha256: context.operation_sha256.clone(),
            expires_at_ms,
            detail: audit_detail,
            risk_level: floor_reason.map(|_| RiskLevel::High),
            risk_reason: floor_reason.map(str::to_string),
        })?;
        sink.emit(RunEvent::AwaitingApproval {
            request_id: request_id.clone(),
            operation_sha256: context.operation_sha256.clone(),
            expires_at_ms,
            reason: Some(if std::io::stdin().is_terminal() {
                "Waiting for the terminal operator".to_string()
            } else {
                "No interactive approval channel is available".to_string()
            }),
        })?;
        Ok(Some(ObservedPermissionRequest {
            request_id,
            operation_sha256: context.operation_sha256,
            expires_at_ms,
        }))
    }

    fn emit_permission_decision(
        &self,
        request: &ObservedPermissionRequest,
        decision: PermissionDecision,
    ) -> Result<(), String> {
        let Some(sink) = self.event_sink.as_ref() else {
            return Ok(());
        };
        sink.emit(RunEvent::PermissionDecided {
            request_id: request.request_id.clone(),
            operation_sha256: request.operation_sha256.clone(),
            decision,
            decided_by: sink.client_identity(),
        })
    }

    async fn wait_for_daemon_decision(
        &self,
        request: &ObservedPermissionRequest,
    ) -> Result<PermissionDecision, String> {
        let sink = self
            .event_sink
            .as_ref()
            .ok_or_else(|| "Daemon approval wait has no durable sink".to_string())?;
        loop {
            let app_data = crate::app_data_dir()
                .ok_or_else(|| "Could not resolve the app data directory".to_string())?;
            let ledger =
                little_monkey_lib::run_ledger::RunLedger::open(app_data.join("profile-v1.sqlite3"))
                    .map_err(|error| error.to_string())?;
            let stored = ledger
                .load_approval(&sink.run_id(), &request.request_id)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| {
                    format!(
                        "Approval '{}' disappeared from the ledger",
                        request.request_id
                    )
                })?;
            if let Some(decision) = stored.decision {
                return Ok(decision);
            }
            if unix_time_ms()? >= request.expires_at_ms {
                match self.emit_permission_decision(request, PermissionDecision::Expired) {
                    Ok(()) => return Ok(PermissionDecision::Expired),
                    Err(error) if error.contains("already decided") => continue,
                    Err(error) => return Err(error),
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
}

fn daemon_approval_wait_enabled() -> bool {
    std::env::var_os("LITTLE_MONKEY_DAEMON_APPROVAL_WAIT").as_deref()
        == Some(std::ffi::OsStr::new("1"))
}

struct ObservedPermissionRequest {
    request_id: String,
    operation_sha256: String,
    expires_at_ms: u64,
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
    use std::sync::Mutex;

    use little_monkey_lib::run_protocol::{ClientIdentity, ClientKind, UsageSnapshot};

    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<RunEvent>>,
    }

    impl CliRunEventSink for RecordingSink {
        fn emit(&self, event: RunEvent) -> Result<(), String> {
            event.validate().map_err(|error| error.to_string())?;
            self.events.lock().unwrap().push(event);
            Ok(())
        }

        fn current_usage(&self) -> Result<UsageSnapshot, String> {
            Ok(crate::durable_run::zero_usage())
        }

        fn client_identity(&self) -> ClientIdentity {
            ClientIdentity {
                client_id: "monkey-cli-test".to_string(),
                instance_id: "permission-test".to_string(),
                kind: ClientKind::Test,
                version: "test".to_string(),
            }
        }

        fn run_id(&self) -> String {
            "permission-test-run".to_string()
        }
    }

    #[test]
    fn parse_accepts_every_valid_mode() {
        assert_eq!(PermissionMode::parse("manual"), Ok(PermissionMode::Manual));
        assert_eq!(
            PermissionMode::parse("acceptEdits"),
            Ok(PermissionMode::AcceptEdits)
        );
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
        assert_eq!(
            mode_short_circuit(PermissionMode::Bypass, "run_shell", true),
            Some(Ok(()))
        );
        assert_eq!(
            mode_short_circuit(PermissionMode::Bypass, "write_file", false),
            Some(Ok(()))
        );
    }

    #[test]
    fn plan_blocks_every_tool_regardless_of_which_one() {
        assert!(
            mode_short_circuit(PermissionMode::Plan, "write_file", false)
                .unwrap()
                .is_err()
        );
        assert!(mode_short_circuit(PermissionMode::Plan, "run_shell", false)
            .unwrap()
            .is_err());
        assert!(mode_short_circuit(PermissionMode::Plan, "web_fetch", false)
            .unwrap()
            .is_err());
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
        assert_eq!(
            mode_short_circuit(PermissionMode::Smart, "run_shell", false),
            None
        );
        assert_eq!(
            mode_short_circuit(PermissionMode::Smart, "run_shell", true),
            None
        );
    }

    #[test]
    fn smart_mode_auto_approves_write_and_edit_only_when_not_floored() {
        assert_eq!(
            mode_short_circuit(PermissionMode::Smart, "write_file", false),
            Some(Ok(()))
        );
        assert_eq!(
            mode_short_circuit(PermissionMode::Smart, "edit_file", false),
            Some(Ok(()))
        );
        assert_eq!(
            mode_short_circuit(PermissionMode::Smart, "write_file", true),
            None
        );
        assert_eq!(
            mode_short_circuit(PermissionMode::Smart, "edit_file", true),
            None
        );
    }

    #[test]
    fn smart_mode_never_short_circuits_remember() {
        // Not eligible in the GUI's own "smart" arm either — only
        // write_file/edit_file are.
        assert_eq!(
            mode_short_circuit(PermissionMode::Smart, "remember", false),
            None
        );
    }

    #[test]
    fn manual_never_short_circuits_anything() {
        assert_eq!(
            mode_short_circuit(PermissionMode::Manual, "write_file", false),
            None
        );
        assert_eq!(
            mode_short_circuit(PermissionMode::Manual, "run_shell", false),
            None
        );
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
            mode_short_circuit(
                PermissionMode::Smart,
                "write_file",
                path_risk_floor(env_path, root).is_some()
            ),
            None
        );
    }

    #[test]
    fn observed_prompt_is_digest_bound_and_records_a_terminal_decision() {
        let sink = Arc::new(RecordingSink::default());
        let event_sink: Arc<dyn CliRunEventSink> = sink.clone();
        let mut permissions =
            TerminalPermissions::with_event_sink(PermissionMode::Auto, event_sink, 60_000, false);
        permissions.begin_tool_call(
            "tool-1-1",
            "run_shell",
            r#"{"command":"cargo test"}"#,
            "/workspace",
        );
        let request = permissions
            .emit_permission_request("run_shell", "Run cargo test", None)
            .unwrap()
            .unwrap();
        permissions
            .emit_permission_decision(&request, PermissionDecision::Deny)
            .unwrap();

        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 3);
        let (requested_id, requested_digest, expires_at_ms) = match &events[0] {
            RunEvent::PermissionRequested {
                request_id,
                operation_sha256,
                expires_at_ms,
                ..
            } => (request_id, operation_sha256, expires_at_ms),
            other => panic!("unexpected first event: {other:?}"),
        };
        match &events[1] {
            RunEvent::AwaitingApproval {
                request_id,
                operation_sha256,
                expires_at_ms: awaiting_expiry,
                ..
            } => {
                assert_eq!(request_id, requested_id);
                assert_eq!(operation_sha256, requested_digest);
                assert_eq!(awaiting_expiry, expires_at_ms);
            }
            other => panic!("unexpected second event: {other:?}"),
        }
        match &events[2] {
            RunEvent::PermissionDecided {
                request_id,
                operation_sha256,
                decision,
                ..
            } => {
                assert_eq!(request_id, requested_id);
                assert_eq!(operation_sha256, requested_digest);
                assert_eq!(*decision, PermissionDecision::Deny);
            }
            other => panic!("unexpected third event: {other:?}"),
        }
    }
}
