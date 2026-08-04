//! Post-edit verification: user-configured lint/test/build commands the
//! agent loop runs automatically after a turn that wrote files (see
//! `agentLoop.ts`'s `mutatedFiles` tracking), so the user gets an immediate
//! "did my edit break the build?" signal without asking for it every time.
//!
//! Slice 1 (this file) is report-only: `verify_run` executes one stored
//! command and returns its result; nothing is fed back into the model yet
//! (that's `verifyMaxRounds` — slice 2).
//!
//! DATA MODEL / STORAGE. Per-workspace config lives in
//! `<app_data>/verify_configs.json`, a `HashMap<String, VerifyConfig>` keyed
//! by the *canonicalized primary-root path* — the same app-data-file pattern
//! as `recent_workspaces.json` (see `workspace.rs`), with atomic temp+rename
//! writes (see `sessions.rs::save_to`). Deliberately NOT a file inside the
//! workspace: verify commands run with no per-run permission prompt, so the
//! command strings must never be writable by the model or by a cloned repo.
//! Storing them app-side, editable only through the Settings UI, keeps the
//! trust boundary identical to `git_commit`/`checkpoint_revert` — see the
//! design doc's "Risks" section for why a future "read config from the
//! workspace" feature would need to reopen this as an explicit-confirmation
//! flow.
//!
//! SECURITY INVARIANT: `verify_run` looks up the command to execute BY ID
//! ONLY — the frontend never passes a raw command string over IPC, mirroring
//! `permissions::permission_respond`'s anti-confused-deputy shape (respond to
//! a request by id, never by re-submitting the thing being approved). This
//! command is also NOT named with a `tool_` prefix, so `agentLoop.ts`'s
//! `executeToolCall` (which dispatches a model tool call by invoking
//! `` `tool_${name}` ``) can never reach it from a model-requested tool call
//! — by construction, not by a runtime check. A model can only ever trigger
//! verification indirectly, by mutating files, never by naming this command.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::Manager;
use tokio::sync::Notify;

use crate::{workspace, AppState};

const VERIFY_CONFIGS_FILE: &str = "verify_configs.json";

/// Default per-command timeout when `VerifyCommand::timeout_secs` is unset —
/// generous relative to `tools.rs`'s 120s `SHELL_TIMEOUT`, since test suites
/// routinely outlive that.
const DEFAULT_VERIFY_TIMEOUT_SECS: u64 = 300;

/// Each of stdout/stderr is tail-capped before ever leaving this module — a
/// runaway test suite's output must not flood the model's context window.
///
/// The number, the truncation direction and the marker now live in
/// [`crate::output_cap`], shared with `tools.rs`'s shell tool, which had no cap at
/// all. Previously documented as bounding "chars" while measuring `s.len()`, which
/// is bytes.
const VERIFY_OUTPUT_CAP: usize = crate::output_cap::MODEL_OUTPUT_CAP;

/// One user-configured verification command.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VerifyCommand {
    pub id: String,
    pub label: String,
    pub command: String,
    /// One of "lint" | "test" | "build" | "custom" — free-form on this side
    /// (never matched against by Rust logic), just carried through for the
    /// frontend's kind-select and MessageList's icon.
    pub kind: String,
    pub enabled: bool,
    /// Renamed on the wire — `verifyStore.ts`'s `VerifyCommand.timeoutSecs`
    /// (like `VerifyResult`'s `commandId`/`durationMs`/`timedOut` below)
    /// expects camelCase; without this rename Serde would silently drop an
    /// incoming `timeoutSecs` field (missing `Option` fields default to
    /// `None` rather than erroring), so the per-command timeout the editor
    /// sets would never actually persist.
    #[serde(rename = "timeoutSecs", skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// A workspace's full verification config — currently just its command list,
/// kept as a struct (rather than a bare `Vec`) so a future slice can add
/// workspace-level fields (e.g. a per-workspace rounds override) without a
/// storage migration.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct VerifyConfig {
    pub commands: Vec<VerifyCommand>,
}

type VerifyConfigMap = HashMap<String, VerifyConfig>;

/// Result of running one `VerifyCommand`, returned by `verify_run` and
/// rendered as a `[Verify]` notice by `agentLoop.ts`/`MessageList.tsx`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VerifyResult {
    #[serde(rename = "commandId")]
    pub command_id: String,
    pub label: String,
    pub kind: String,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    #[serde(rename = "durationMs")]
    pub duration_ms: u64,
    #[serde(rename = "timedOut")]
    pub timed_out: bool,
}

fn verify_configs_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {}", e))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create app data dir: {}", e))?;
    Ok(dir.join(VERIFY_CONFIGS_FILE))
}

/// Core load logic, parameterized by path for testability. Anything missing
/// or unparsable degrades to "no commands configured for any workspace"
/// rather than an error — a corrupt/hand-edited file must not block turns.
fn load_configs_from(path: &Path) -> VerifyConfigMap {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return VerifyConfigMap::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Core save logic: write to a sibling temp file, then rename over the real
/// one — same atomic-write pattern as `sessions.rs::save_to`.
fn save_configs_to(path: &Path, configs: &VerifyConfigMap) -> Result<(), String> {
    let json = serde_json::to_string_pretty(configs)
        .map_err(|e| format!("Failed to serialize verify configs: {}", e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("Failed to write verify configs: {}", e))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("Failed to finalize verify configs: {}", e))?;
    Ok(())
}

/// Resolves the config-map key for `workspace_path` if given, otherwise the
/// current primary workspace root — both canonicalized so the same folder
/// always maps to the same key regardless of how it was opened (symlink,
/// trailing slash, relative path at the call site, etc.).
fn resolve_root_key(state: &AppState, workspace_path: Option<&str>) -> Result<String, String> {
    let canon = match workspace_path {
        Some(p) => PathBuf::from(p)
            .canonicalize()
            .map_err(|e| format!("Invalid workspace path '{}': {}", p, e))?,
        None => workspace::primary_root_canon(state)?,
    };
    Ok(canon.to_string_lossy().to_string())
}

/// AppHandle-free lookup of a single workspace's verify config directly from
/// disk, given an already-resolved `configs_path` and the workspace's
/// canonicalized `root` — the same key shape `resolve_root_key` produces
/// (`root.to_string_lossy()`), just without needing an `AppState`/`AppHandle`
/// to derive either from. `monkey-cli` (`verify_cli.rs`) computes `configs_path`
/// via the same hardcoded-identifier app-data convention `providers_cli.rs`
/// uses for `providers.json`, so both binaries read the exact same
/// `verify_configs.json` the desktop app's Settings > Verification tab
/// writes. Degrades to an empty `VerifyConfig` (no commands) for a
/// missing/corrupt file or an unconfigured root — never an error, matching
/// `verify_get_config`'s own tolerance.
pub fn load_config_for_workspace(configs_path: &Path, root: &Path) -> VerifyConfig {
    let key = root.to_string_lossy().to_string();
    load_configs_from(configs_path)
        .get(&key)
        .cloned()
        .unwrap_or_default()
}

/// Finds `command_id` within `config.commands` — the ONLY way `verify_run`
/// is allowed to turn an id into an actual command to execute. Never takes
/// (or matches against) a raw command string, which is the whole point: a
/// frontend/model that only ever has ids to work with can't smuggle
/// arbitrary shell through this path.
fn find_command<'a>(config: &'a VerifyConfig, command_id: &str) -> Option<&'a VerifyCommand> {
    config.commands.iter().find(|c| c.id == command_id)
}

/// Tail-caps `s` at [`VERIFY_OUTPUT_CAP`] bytes.
///
/// A thin wrapper over [`crate::output_cap::cap_tail`], kept so this module's two
/// call sites stay readable. `VerifyResult` has no truncation flag on the wire, so
/// the marker in the text is the only signal here — unlike the shell tool, whose
/// callers may need to parse the output as a whole document.
fn cap_output(s: String) -> String {
    crate::output_cap::cap_tail(s, VERIFY_OUTPUT_CAP).0
}

/// Core, `AppHandle`-free execution logic — a near-copy of
/// `tools.rs::tool_run_shell`'s plumbing, minus the permission prompt (see
/// this module's doc comment for why verify commands are deliberately never
/// permission-gated) and minus checkpoint recording (verify commands aren't
/// model tool calls, so there's no `checkpoint_id` to thread through).
/// `AppHandle`-free so `monkey-cli` (a later slice) can call this directly, the
/// same `begin_impl`/`end_impl` split `checkpoints.rs` uses.
///
/// Mirrors `tool_run_shell`'s `tokio::select!` over: the command completing,
/// the turn's `tool_cancel` `Notify` firing (the existing Stop button kills a
/// running verify command with zero new wiring), and a per-command timeout
/// (default [`DEFAULT_VERIFY_TIMEOUT_SECS`]).
pub async fn run_command_impl(
    state: &AppState,
    root: &Path,
    cmd: &VerifyCommand,
    turn_id: Option<&str>,
) -> VerifyResult {
    let started = Instant::now();

    #[cfg(target_os = "windows")]
    let (shell, shell_flag) = ("cmd", "/C");
    #[cfg(not(target_os = "windows"))]
    let (shell, shell_flag) = ("sh", "-c");

    let mut command_builder = tokio::process::Command::new(shell);
    command_builder
        .arg(shell_flag)
        .arg(&cmd.command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The timeout and cancellation branches below both work by DROPPING
        // the in-flight `wait_with_output` future (and the child with it) —
        // without this, the spawned process would keep running orphaned.
        .kill_on_drop(true);
    // Its own process group, so a timeout can end the whole tree rather than the
    // shell alone — `kill_on_drop` reaps one pid, which for `sh -c "npm test"`
    // leaves the test runner alive. Mirrors `tools.rs`'s own spawn.
    #[cfg(unix)]
    command_builder.process_group(0);
    // Kernel-held bounds that outlive this app's supervision — see `os_limits`
    // for why only core dumps are refused and not CPU time or memory. A verify
    // command that segfaults mid-build should not leave gigabytes of core in the
    // repository it was checking.
    crate::os_limits::apply(
        crate::os_limits::ChildLimits::baseline(),
        &mut command_builder,
    );

    let timeout = Duration::from_secs(
        cmd.timeout_secs
            .unwrap_or(DEFAULT_VERIFY_TIMEOUT_SECS)
            .max(1),
    );

    let child = match command_builder.spawn() {
        Ok(child) => child,
        Err(e) => {
            return VerifyResult {
                command_id: cmd.id.clone(),
                label: cmd.label.clone(),
                kind: cmd.kind.clone(),
                code: None,
                stdout: String::new(),
                stderr: format!("Failed to spawn command: {}", e),
                duration_ms: started.elapsed().as_millis() as u64,
                timed_out: false,
            };
        }
    };

    // Each turn gets its own cancellation channel so Stop in one pane never
    // kills a command the other pane's turn is still running — exact
    // `tool_run_shell` pattern (see `AppState::tool_cancel`'s doc comment).
    let cancel_key = turn_id.unwrap_or_default().to_string();
    let cancel: Option<Arc<Notify>> = state.tool_cancel.lock().ok().map(|mut guard| {
        guard
            .entry(cancel_key.clone())
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone()
    });

    // Captured before `wait_with_output` consumes the child; with
    // `process_group(0)` above, the child's own pid is also its group id.
    let child_pgid = child.id();

    let (outcome, timed_out): (Result<std::process::Output, String>, bool) = match &cancel {
        Some(cancel) => tokio::select! {
            result = child.wait_with_output() => (result.map_err(|e| format!("Failed to run command: {}", e)), false),
            _ = cancel.notified() => (Err("Command cancelled by the user".to_string()), false),
            _ = tokio::time::sleep(timeout) => (Err(format!("Command timed out after {} seconds", timeout.as_secs())), true),
        },
        // Lock poisoned — extremely unlikely, and cancellation simply isn't
        // available for this run; the timeout still applies.
        None => tokio::select! {
            result = child.wait_with_output() => (result.map_err(|e| format!("Failed to run command: {}", e)), false),
            _ = tokio::time::sleep(timeout) => (Err(format!("Command timed out after {} seconds", timeout.as_secs())), true),
        },
    };

    // End the tree on a timeout or a cancel. A verify command is typically a
    // build or a test runner, so the process that matters is almost always a
    // grandchild of the `sh -c` this spawned — exactly the process `kill_on_drop`
    // does not touch.
    if outcome.is_err() {
        if let Some(pgid) = child_pgid {
            if let Err(error) = crate::os_signal::terminate_process_group(pgid) {
                eprintln!("verify: could not terminate process group {pgid}: {error}");
            }
        }
    }

    // Drop this turn's channel once no other verify/shell command of the
    // same turn still holds it — same strong-count cleanup as
    // `tool_run_shell`, so the map doesn't accumulate one entry per turn
    // forever.
    if let Ok(mut guard) = state.tool_cancel.lock() {
        if guard
            .get(&cancel_key)
            .is_some_and(|n| Arc::strong_count(n) <= 2)
        {
            guard.remove(&cancel_key);
        }
    }

    let duration_ms = started.elapsed().as_millis() as u64;

    match outcome {
        Ok(output) => VerifyResult {
            command_id: cmd.id.clone(),
            label: cmd.label.clone(),
            kind: cmd.kind.clone(),
            code: output.status.code(),
            stdout: cap_output(String::from_utf8_lossy(&output.stdout).to_string()),
            stderr: cap_output(String::from_utf8_lossy(&output.stderr).to_string()),
            duration_ms,
            timed_out: false,
        },
        Err(message) => VerifyResult {
            command_id: cmd.id.clone(),
            label: cmd.label.clone(),
            kind: cmd.kind.clone(),
            code: None,
            stdout: String::new(),
            stderr: message,
            duration_ms,
            timed_out,
        },
    }
}

/// The current workspace's verification config (empty `commands` if none
/// configured yet, or if no workspace is open). `workspace_path` lets a
/// caller ask about a specific folder rather than "whatever's primary right
/// now" — unused by the frontend today, but keeps this symmetric with
/// `verify_set_config`'s implicit "current primary root" target and testable
/// independent of `AppState`'s live workspace.
#[tauri::command]
pub fn verify_get_config(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    workspace_path: Option<String>,
) -> Result<VerifyConfig, String> {
    let key = resolve_root_key(state.inner(), workspace_path.as_deref())?;
    let configs = load_configs_from(&verify_configs_path(&app)?);
    Ok(configs.get(&key).cloned().unwrap_or_default())
}

/// Replaces the current primary workspace's verification config wholesale —
/// human-initiated only (the Settings UI's command editor), never called
/// with model-supplied content. `verify_set_config` (unlike `verify_get_config`)
/// intentionally has no `workspace_path` param: it always targets whatever
/// workspace is primary right now, so there's no way to write a config for a
/// folder that isn't the one currently open in this window.
#[tauri::command]
pub fn verify_set_config(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    config: VerifyConfig,
) -> Result<(), String> {
    let key = resolve_root_key(state.inner(), None)?;
    let path = verify_configs_path(&app)?;
    let mut configs = load_configs_from(&path);
    configs.insert(key, config);
    save_configs_to(&path, &configs)
}

/// Executes ONE stored verification command, looked up by `command_id`
/// against the current primary workspace's config — see this module's doc
/// comment for the id-only-lookup / no-`tool_`-prefix security invariant.
/// `turn_id` (optional — a manual "run now" affordance in a later slice would
/// omit it) scopes Stop-button cancellation to the calling turn, exactly like
/// `tool_run_shell`'s.
#[tauri::command]
pub async fn verify_run(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    command_id: String,
    turn_id: Option<String>,
) -> Result<VerifyResult, String> {
    let root = workspace::primary_root_canon(state.inner())?;
    let key = root.to_string_lossy().to_string();
    let configs = load_configs_from(&verify_configs_path(&app)?);
    let config = configs.get(&key).cloned().unwrap_or_default();
    let cmd = find_command(&config, &command_id)
        .cloned()
        .ok_or_else(|| format!("Unknown verify command id '{}'", command_id))?;

    Ok(run_command_impl(state.inner(), &root, &cmd, turn_id.as_deref()).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_path(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "little_monkey_verify_test_{}_{}_{}_{}",
            std::process::id(),
            n,
            nanos,
            name
        ))
    }

    fn command(id: &str, cmd: &str) -> VerifyCommand {
        VerifyCommand {
            id: id.to_string(),
            label: id.to_string(),
            command: cmd.to_string(),
            kind: "custom".to_string(),
            enabled: true,
            timeout_secs: None,
        }
    }

    #[test]
    fn config_save_then_load_roundtrips() {
        let path = temp_path("config.json");
        let mut configs = VerifyConfigMap::new();
        configs.insert(
            "/some/root".to_string(),
            VerifyConfig {
                commands: vec![command("a", "echo hi")],
            },
        );
        save_configs_to(&path, &configs).unwrap();

        let loaded = load_configs_from(&path);
        assert_eq!(loaded.get("/some/root").unwrap().commands.len(), 1);
        assert_eq!(loaded.get("/some/root").unwrap().commands[0].id, "a");

        let _ = std::fs::remove_file(&path);
    }

    /// The frontend (`verifyStore.ts`) sends/reads `timeoutSecs` (camelCase)
    /// over IPC — without the `#[serde(rename = "timeoutSecs")]` on
    /// `VerifyCommand::timeout_secs`, this field would silently deserialize
    /// to `None` instead of erroring (missing `Option` fields default rather
    /// than fail), so the command editor's timeout input would never
    /// actually take effect. Guards the wire format directly rather than
    /// relying on the frontend types alone.
    #[test]
    fn verify_command_timeout_secs_round_trips_as_camel_case_on_the_wire() {
        let mut cmd = command("a", "echo hi");
        cmd.timeout_secs = Some(45);

        let json = serde_json::to_value(&cmd).unwrap();
        assert_eq!(json.get("timeoutSecs").and_then(|v| v.as_u64()), Some(45));
        assert!(json.get("timeout_secs").is_none());

        let round_tripped: VerifyCommand = serde_json::from_value(json).unwrap();
        assert_eq!(round_tripped.timeout_secs, Some(45));
    }

    /// `monkey-cli` (`verify_cli.rs`) is the only consumer of this — it never
    /// has an `AppState`/`AppHandle` to derive `resolve_root_key` through,
    /// only a plain `configs_path` and its own canonicalized workspace root.
    #[test]
    fn load_config_for_workspace_finds_the_matching_root_and_defaults_for_others() {
        let path = temp_path("cli_config.json");
        let mut configs = VerifyConfigMap::new();
        configs.insert(
            "/some/root".to_string(),
            VerifyConfig {
                commands: vec![command("a", "echo hi")],
            },
        );
        save_configs_to(&path, &configs).unwrap();

        let found = load_config_for_workspace(&path, Path::new("/some/root"));
        assert_eq!(found.commands.len(), 1);
        assert_eq!(found.commands[0].id, "a");

        let missing = load_config_for_workspace(&path, Path::new("/no/such/root"));
        assert!(missing.commands.is_empty());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_missing_file_returns_empty_map() {
        let path = temp_path("missing.json");
        assert!(load_configs_from(&path).is_empty());
    }

    #[test]
    fn load_corrupt_file_degrades_to_empty_map_instead_of_erroring() {
        let path = temp_path("corrupt.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(load_configs_from(&path).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    /// `find_command` — the only way an id becomes a runnable command —
    /// rejects an id that isn't configured.
    #[test]
    fn find_command_rejects_unknown_id() {
        let config = VerifyConfig {
            commands: vec![command("real-id", "echo hi")],
        };
        assert!(find_command(&config, "not-a-real-id").is_none());
    }

    /// The anti-confused-deputy invariant: passing a raw COMMAND STRING where
    /// an id is expected must never resolve to anything, even if that string
    /// happens to match a configured command's `command` field exactly. Only
    /// the `id` field is ever matched against.
    #[test]
    fn find_command_never_matches_by_command_string() {
        let config = VerifyConfig {
            commands: vec![command("real-id", "echo hi")],
        };
        assert!(find_command(&config, "echo hi").is_none());
        assert!(find_command(&config, "real-id").is_some());
    }

    #[tokio::test]
    async fn run_command_impl_reports_exit_code_and_output() {
        let state = AppState::default();
        let cwd = temp_path("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let cmd = command("c1", "echo hello");

        let result = run_command_impl(&state, &cwd, &cmd, None).await;

        assert_eq!(result.command_id, "c1");
        assert_eq!(result.code, Some(0));
        assert!(result.stdout.contains("hello"));
        assert!(!result.timed_out);

        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn run_command_impl_reports_nonzero_exit_code() {
        let state = AppState::default();
        let cwd = temp_path("cwd_fail");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut cmd = command("c1", "exit 3");
        cmd.kind = "test".to_string();

        let result = run_command_impl(&state, &cwd, &cmd, None).await;

        assert_eq!(result.code, Some(3));
        assert!(!result.timed_out);

        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn run_command_impl_times_out_a_long_running_command() {
        let state = AppState::default();
        let cwd = temp_path("cwd_timeout");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut cmd = command("c1", "sleep 30");
        cmd.timeout_secs = Some(1);

        let started = Instant::now();
        let result = run_command_impl(&state, &cwd, &cmd, None).await;

        assert!(result.timed_out);
        assert!(result.code.is_none());
        // The whole call returned near the 1s timeout, not the 30s sleep —
        // proof the child was actually killed rather than awaited out.
        assert!(started.elapsed() < Duration::from_secs(10));

        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn run_command_impl_caps_output_length() {
        let state = AppState::default();
        let cwd = temp_path("cwd_cap");
        std::fs::create_dir_all(&cwd).unwrap();
        // Print well over VERIFY_OUTPUT_CAP characters of 'x'.
        let cmd = command("c1", "yes x | head -c 50000");

        let result = run_command_impl(&state, &cwd, &cmd, None).await;

        assert!(
            result.stdout.len() <= VERIFY_OUTPUT_CAP + 64,
            "stdout not capped: {} chars",
            result.stdout.len()
        );
        assert!(result.stdout.starts_with("… (truncated)"));

        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn run_command_impl_is_killed_by_tool_cancel_notify() {
        let state = AppState::default();
        let cwd = temp_path("cwd_cancel");
        std::fs::create_dir_all(&cwd).unwrap();
        let cmd = command("c1", "sleep 30");
        let turn_id = "turn-1".to_string();

        // Pre-seed the cancel channel for this turn (mirrors what
        // `tools_cancel_running` finds when it looks the turn up) and fire it
        // shortly after the command starts, from a separate task — simulating
        // the user hitting Stop mid-run.
        let notify = {
            let mut guard = state.tool_cancel.lock().unwrap();
            guard
                .entry(turn_id.clone())
                .or_insert_with(|| Arc::new(Notify::new()))
                .clone()
        };
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            notify.notify_waiters();
        });

        let started = Instant::now();
        let result = run_command_impl(&state, &cwd, &cmd, Some(&turn_id)).await;

        assert!(!result.timed_out);
        assert!(result.code.is_none());
        assert!(result.stderr.contains("cancelled"));
        // Returned promptly after the ~100ms cancel fired, not the 30s sleep.
        assert!(started.elapsed() < Duration::from_secs(10));

        let _ = std::fs::remove_dir_all(&cwd);
    }
}
