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

use tokio::sync::Notify;

use crate::process_table::{ProcessKind, ProcessLimitKind, ProcessLimits};
use crate::profiles::ProfileScopedPaths;
use crate::resource_control::{EffectiveLimits, LimitLayer, LimitSource, ResourceController};
use crate::{workspace, AppState};

const VERIFY_CONFIGS_FILE: &str = "verify_configs.json";

/// Default per-command timeout when `VerifyCommand::timeout_secs` is unset —
/// generous relative to `tools.rs`'s 120s `SHELL_TIMEOUT`, since test suites
/// routinely outlive that.
pub(crate) const DEFAULT_VERIFY_TIMEOUT_SECS: u64 = 300;

/// Each of stdout/stderr is tail-capped before ever leaving this module — a
/// runaway test suite's output must not flood the model's context window.
///
/// The number, the truncation direction and the marker now live in
/// [`crate::output_cap`], shared with `tools.rs`'s shell tool, which had no cap at
/// all. Previously documented as bounding "chars" while measuring `s.len()`, which
/// is bytes.
pub(crate) const VERIFY_OUTPUT_CAP: usize = crate::output_cap::MODEL_OUTPUT_CAP;

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
        .profile_data_dir()
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

/// A finished verify command, with both streams already held to
/// [`VERIFY_OUTPUT_CAP`] as they arrived.
///
/// Replaces `std::process::Output` here so the type itself carries the bound:
/// `Output` holds two unbounded `Vec<u8>`s, which is what
/// `wait_with_output` filled before anything trimmed them. `VerifyResult` has no
/// truncation flag on the wire, so the marker in the text is the only signal —
/// unlike the shell tool, whose callers may need to parse the output as a whole
/// document.
struct CapturedVerifyOutput {
    code: Option<i32>,
    stdout: String,
    stderr: String,
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
    projector: Option<std::sync::Arc<dyn crate::process_table::ProcessProjector>>,
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
    // leaves the test runner alive. Set here as well as by the controller's
    // supervised backend, exactly as `workspace_shell` does: a cgroup scope does
    // not install one, and the group is what a later session's startup reclaim
    // can still reach.
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

    // The same resource controller every agent shell runs under, for the same
    // reason: a verify command is a build or a test runner, so the process that
    // holds the machine is a *grandchild* of this `sh -c` and the deadline above
    // is the only bound this path used to have. The class defaults supply the
    // tree's memory and process ceilings; the command's own timeout is the wall
    // bound, stated as a limit rather than as a `sleep` racing the wait, so a
    // verify command that runs out of time is reclaimed by the same code that
    // reclaims one that runs out of memory.
    let mut controller = ResourceController::new(EffectiveLimits::resolve(&[
        LimitLayer::new(
            LimitSource::ClassDefault,
            ProcessKind::VerifyCommand.default_limits(),
        ),
        LimitLayer::new(
            LimitSource::UserOverride,
            ProcessLimits {
                max_wall_ms: Some(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)),
                max_output_bytes: Some(u64::try_from(VERIFY_OUTPUT_CAP).unwrap_or(u64::MAX)),
                ..ProcessLimits::default()
            },
        ),
    ]));
    // The row, before the spawn rather than after the wait. A verify command
    // that fails to `exec` is exactly the case a reader wants a record of, and
    // one written afterwards would miss every command that never started.
    //
    // `None` only where there is nothing to project onto — `monkey-cli` and this
    // module's own tests, which have no ledger. A missing projector is a missing
    // row, never a refused command.
    let mut execution = projector.map(|projector| {
        crate::bounded_execution::BoundedExecution::admit(
            projector,
            ProcessKind::VerifyCommand,
            Some(root.to_string_lossy().into_owned()),
            controller.limits().to_process_limits(),
        )
    });
    if let (Some(execution), Some(turn)) = (execution.as_mut(), turn_id) {
        // The turn is what makes `monkey processes list --parent` answer "what
        // did this turn verify".
        execution.set_parent(ProcessKind::ChatTurn, turn);
    }
    let failed_to_start = |message: String| VerifyResult {
        command_id: cmd.id.clone(),
        label: cmd.label.clone(),
        kind: cmd.kind.clone(),
        code: None,
        stdout: String::new(),
        stderr: message,
        duration_ms: started.elapsed().as_millis() as u64,
        timed_out: false,
    };
    // Before the spawn, so the containment exists before the command's first
    // instruction rather than being applied to a process already running.
    if let Err(error) = controller.prepare_tokio(&mut command_builder) {
        return failed_to_start(format!("Failed to bound command: {error}"));
    }

    // Through the controller, so on Windows the job holds this command before its
    // first instruction rather than microseconds after it. On Unix this is the
    // ordinary spawn: `prepare_tokio` above already installed the containment the
    // child joins between `fork` and `exec`.
    let mut child = match controller.spawn_contained_tokio(&mut command_builder) {
        Ok(child) => child,
        Err(e) => {
            let message = format!("Failed to spawn command: {}", e);
            if let Some(execution) = execution.take() {
                execution.exited(
                    crate::process_table::ProcessExit::failed(message.clone()),
                    None,
                );
            }
            return failed_to_start(message);
        }
    };

    // Fail closed: a command that is *running* and cannot be shown to be inside
    // its containment is reclaimed rather than reported as bounded. A command
    // that simply finished first is not a containment failure.
    if let Some(pid) = child.id() {
        match controller.attach(pid) {
            Ok(()) | Err(crate::resource_control::AttachFailure::AlreadyExited) => {}
            Err(crate::resource_control::AttachFailure::Containment(error)) => {
                let _ = controller.terminate_tree();
                let message = format!("Failed to bound command: {error}");
                if let Some(execution) = execution.take() {
                    execution.exited(
                        crate::process_table::ProcessExit::failed(message.clone()),
                        None,
                    );
                }
                return failed_to_start(message);
            }
        }
    }
    // After the attach, not after the spawn: the row records the identity and the
    // containment the attach has just *verified*, rather than one it assumed.
    if let Some(execution) = execution.as_mut() {
        execution.running(&controller);
    }

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

    // Bounded as the bytes arrive rather than trimmed after the child exits.
    // `wait_with_output` collected both pipes whole, so a verify command that
    // printed a gigabyte — a `-v` build, a test runner in debug mode — took a
    // gigabyte of this app's heap before `cap_output` looked at any of it. The
    // two drains run concurrently with the wait for the older reason: a child
    // that fills one pipe while nothing reads it blocks forever.
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let collect = async {
        let (status, stdout, stderr) = tokio::try_join!(
            child.wait(),
            crate::output_cap::drain_capped(
                stdout_pipe.expect("stdout was piped at spawn"),
                Some(VERIFY_OUTPUT_CAP)
            ),
            crate::output_cap::drain_capped(
                stderr_pipe.expect("stderr was piped at spawn"),
                Some(VERIFY_OUTPUT_CAP)
            ),
        )?;
        Ok::<_, std::io::Error>(CapturedVerifyOutput {
            code: status.code(),
            stdout: stdout.into_string().0,
            stderr: stderr.into_string().0,
        })
    };

    // The wall bound, the memory bound and the process-count bound are all held
    // by the controller now, and a breach of any of them has already reclaimed
    // the whole tree by the time this returns. What is left for the `select` is
    // the user's Stop, which is a different fact from a limit and stays one.
    //
    // The breach and the last sample are kept beside the outcome rather than
    // folded into the message: a limit kill has to reach the row as typed fields
    // — which limit, configured, observed, backend, level — and a UI cannot parse
    // that back out of an English sentence.
    let limit_breach: Option<crate::resource_control::LimitBreach>;
    let last_sample: Option<crate::resource_control::ResourceSample>;
    let observer = execution.as_ref();
    let supervised = async {
        let sampled = crate::resource_control::run_under_observed(
            &mut controller,
            collect,
            // Every tick, so the panel can show what a running build is holding
            // instead of only what it held once it was over.
            |sample| {
                if let Some(execution) = observer {
                    execution.sampled(sample);
                }
            },
        )
        .await;
        match sampled {
            Ok(crate::resource_control::Supervised::Completed(result, sample)) => (
                result.map_err(|e| format!("Failed to run command: {}", e)),
                false,
                None,
                Some(sample),
            ),
            Ok(crate::resource_control::Supervised::Breached(breach, sample)) => {
                // A wall breach *is* the timeout this command declared, so it
                // keeps reporting as one. Any other limit is reported with both
                // numbers and the mechanism that held it, because "the build
                // failed" and "the build asked for 9 GiB" are different things
                // for a reader to do something about.
                let timed_out = breach.limit == ProcessLimitKind::Wall.as_str();
                let message = if timed_out {
                    format!("Command timed out after {} seconds", timeout.as_secs())
                } else {
                    breach.describe()
                };
                (Err(message), timed_out, Some(breach), Some(sample))
            }
            Err(error) => (
                Err(format!("Failed to run command: {}", error)),
                false,
                None,
                None,
            ),
        }
    };

    let (outcome, timed_out): (Result<CapturedVerifyOutput, String>, bool) = {
        let (outcome, timed_out, breach, sample) = match &cancel {
            Some(cancel) => tokio::select! {
                result = supervised => result,
                _ = cancel.notified() => (
                    Err("Command cancelled by the user".to_string()), false, None, None
                ),
            },
            // Lock poisoned — extremely unlikely, and cancellation simply isn't
            // available for this run; every limit still applies.
            None => supervised.await,
        };
        limit_breach = breach;
        last_sample = sample;
        (outcome, timed_out)
    };

    // A cancel is the one exit that leaves the tree standing: the limits all
    // reclaim it themselves. A verify command is typically a build or a test
    // runner, so the process that matters is almost always a grandchild of the
    // `sh -c` this spawned — exactly the process `kill_on_drop` does not touch.
    if outcome.is_err() {
        if let Err(error) = controller.terminate_tree() {
            eprintln!("verify: could not terminate the command's process tree: {error}");
        }
    }

    // The row's terminal state, with the cause typed rather than described. Four
    // outcomes and four different facts: a resource kill is not a failure, a
    // user's Stop is not a resource kill, and a command that exited non-zero on
    // its own is neither.
    if let Some(execution) = execution.take() {
        let exit = match (&outcome, limit_breach) {
            (_, Some(breach)) => crate::process_table::ProcessExit::limit_exceeded(breach),
            (Ok(captured), None) => match captured.code {
                Some(0) => crate::process_table::ProcessExit::succeeded(),
                code => crate::process_table::ProcessExit {
                    status: crate::process_table::ExitStatus::Failed,
                    code,
                    signal: None,
                    reason: None,
                    breach: None,
                },
            },
            (Err(reason), None) => crate::process_table::ProcessExit::cancelled(reason.clone()),
        };
        execution.exited(exit, last_sample);
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
            code: output.code,
            stdout: output.stdout,
            stderr: output.stderr,
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

    Ok(run_command_impl(
        state.inner(),
        &root,
        &cmd,
        turn_id.as_deref(),
        Some(crate::bounded_execution::AppProcessProjector::shared(app)),
    )
    .await)
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

        let result = run_command_impl(&state, &cwd, &cmd, None, None).await;

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

        let result = run_command_impl(&state, &cwd, &cmd, None, None).await;

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
        let result = run_command_impl(&state, &cwd, &cmd, None, None).await;

        assert!(result.timed_out);
        assert!(result.code.is_none());
        // The whole call returned near the 1s timeout, not the 30s sleep —
        // proof the child was actually killed rather than awaited out.
        assert!(started.elapsed() < Duration::from_secs(10));

        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// A verify command's whole process-table lifecycle, against a real child.
    ///
    /// The gap this closes: this path installed a real bound, measured a real
    /// tree and reclaimed it on a breach, and none of it was visible — the row
    /// did not exist. So the assertions are about what a reader can *find*
    /// afterwards: the pid that ran, the identity that names it, the limits that
    /// were installed, and the mechanism that held them.
    #[tokio::test]
    async fn a_verify_command_records_its_identity_limits_and_containment() {
        let state = AppState::default();
        let cwd = temp_path("cwd_row");
        std::fs::create_dir_all(&cwd).unwrap();
        let projector = crate::test_support::RecordingProjector::shared();
        let cmd = command("c1", "echo hi");

        let result = run_command_impl(&state, &cwd, &cmd, None, Some(projector.clone())).await;
        assert_eq!(result.code, Some(0));

        let row = projector.only(ProcessKind::VerifyCommand);
        assert_eq!(row.state, crate::process_table::ProcessState::Exited);
        assert_eq!(
            row.exit.expect("an exited row carries its exit").status,
            crate::process_table::ExitStatus::Succeeded
        );
        assert_eq!(
            row.workspace.as_deref(),
            Some(cwd.to_string_lossy().as_ref())
        );
        // The native identity, not a bare pid: a row that names only a pid cannot
        // be reconciled after a restart without risking an unrelated process.
        assert!(row.native_pid.is_some(), "the row records the pid that ran");
        assert!(
            row.native_start_time.is_some(),
            "the row records the start time that makes the pid an identity"
        );
        // The *effective* limits, which for this command are the class defaults
        // intersected with the runner's own deadline and output cap.
        assert_eq!(
            row.limits.max_memory_bytes,
            ProcessKind::VerifyCommand.default_limits().max_memory_bytes
        );
        assert_eq!(
            row.limits.max_output_bytes,
            Some(VERIFY_OUTPUT_CAP as u64),
            "the runner's own output cap is the tighter one and must be what is recorded"
        );
        // What actually held it, recorded rather than recomputed by whoever reads
        // the row later.
        let containment = row.containment.expect("the row states what held it");
        assert!(!containment.backend.is_empty());
        assert!(!containment.tree_primitive.is_empty());
        assert!(
            containment
                .for_limit(ProcessLimitKind::Memory)
                .is_some_and(|capability| capability.is_enforced()),
            "a verify command's memory bound is held by something, and the row must name it"
        );

        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// A limit kill has to reach the row as typed fields, not as prose.
    ///
    /// Before the row existed, a verify command reclaimed for exceeding its
    /// deadline reported "Command timed out after 1 seconds" into its own result
    /// and vanished. Nothing downstream — the panel, `monkey processes show`, a
    /// query — could tell that from a command that failed on its own.
    #[tokio::test]
    async fn a_verify_command_stopped_by_a_limit_persists_the_typed_breach() {
        let state = AppState::default();
        let cwd = temp_path("cwd_row_breach");
        std::fs::create_dir_all(&cwd).unwrap();
        let projector = crate::test_support::RecordingProjector::shared();
        let mut cmd = command("c1", "sleep 30");
        cmd.timeout_secs = Some(1);

        let result = run_command_impl(&state, &cwd, &cmd, None, Some(projector.clone())).await;
        assert!(result.timed_out);

        let row = projector.only(ProcessKind::VerifyCommand);
        let exit = row.exit.expect("an exited row carries its exit");
        assert_eq!(exit.status, crate::process_table::ExitStatus::LimitExceeded);
        let breach = exit.breach.expect("a limit kill carries its typed breach");
        assert_eq!(breach.limit, ProcessLimitKind::Wall.as_str());
        assert_eq!(breach.configured, 1_000);
        assert!(
            breach.observed >= 1_000,
            "the observed value is a measurement, not the configured number echoed back"
        );
        assert!(!breach.backend.is_empty());
        assert!(
            matches!(breach.level.as_str(), "kernel" | "supervised"),
            "a breach names the level that held it, got {}",
            breach.level
        );

        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// A user's Stop and a limit kill are different facts and stay different.
    #[tokio::test]
    async fn a_cancelled_verify_command_closes_as_cancelled_and_not_as_a_limit() {
        let state = AppState::default();
        let cwd = temp_path("cwd_row_cancel");
        std::fs::create_dir_all(&cwd).unwrap();
        let projector = crate::test_support::RecordingProjector::shared();
        let turn_id = "turn-cancel-row".to_string();
        let cmd = command("c1", "sleep 30");

        let cancel = state
            .tool_cancel
            .lock()
            .map(|mut guard| {
                guard
                    .entry(turn_id.clone())
                    .or_insert_with(|| Arc::new(Notify::new()))
                    .clone()
            })
            .expect("the lock is not poisoned");
        let notifier = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            notifier.notify_waiters();
        });

        let result =
            run_command_impl(&state, &cwd, &cmd, Some(&turn_id), Some(projector.clone())).await;
        assert!(!result.timed_out);

        let row = projector.only(ProcessKind::VerifyCommand);
        let exit = row.exit.expect("an exited row carries its exit");
        assert_eq!(exit.status, crate::process_table::ExitStatus::Cancelled);
        assert!(exit.breach.is_none(), "a Stop is not a resource kill");

        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// A running process's row shows what it is holding, not a blank until it
    /// ends.
    ///
    /// The gap: the sampling loop's readings never left it — only the final
    /// sample reached a caller — so a build sitting at gigabytes for ten minutes
    /// displayed nothing at all. The child here holds a real process tree for
    /// longer than one sample interval, and the assertions are that the *live*
    /// row carries a current measurement and a peak at least as large.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_live_command_reports_what_its_tree_is_holding_now() {
        let state = AppState::default();
        let cwd = temp_path("cwd_live_usage");
        std::fs::create_dir_all(&cwd).unwrap();
        let projector = crate::test_support::RecordingProjector::shared();
        // Three live processes for two seconds: comfortably more than one
        // 500 ms sampling interval, and a tree rather than a single process, so
        // the process count is a number a single `sleep` could not produce.
        let mut cmd = command("c1", "sleep 2 & sleep 2 & sleep 2");
        cmd.timeout_secs = Some(30);

        let watcher = {
            let projector = projector.clone();
            tokio::spawn(async move {
                // Sampled from outside the runner, while it is still running:
                // reading after it returns would prove only that a final sample
                // was written, which is the thing that already worked.
                for _ in 0..60 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let rows = projector.rows(ProcessKind::VerifyCommand);
                    let Some(row) = rows.into_iter().next() else {
                        continue;
                    };
                    if row.state != crate::process_table::ProcessState::Running {
                        continue;
                    }
                    if let Some(usage) = row.usage {
                        if usage.rss_bytes.is_some() || usage.process_count.is_some() {
                            return Some((usage, row.usage_sampled_at_ms));
                        }
                    }
                }
                None
            })
        };

        let result = run_command_impl(&state, &cwd, &cmd, None, Some(projector.clone())).await;
        assert_eq!(result.code, Some(0), "{result:?}");

        let (usage, sampled_at) = watcher
            .await
            .expect("the watcher finishes")
            .expect("a running command's row must carry a measurement while it is running");
        assert!(
            sampled_at.is_some(),
            "a measurement is stamped with when it was taken"
        );
        if let Some(rss) = usage.rss_bytes {
            assert!(
                rss > 0,
                "a live tree holding zero bytes is not a measurement"
            );
            assert!(
                usage.peak_rss_bytes.unwrap_or(0) >= rss,
                "a peak below the current reading is not a peak: {usage:?}"
            );
        }
        if let Some(count) = usage.process_count {
            assert!(count >= 1, "{usage:?}");
            assert!(usage.peak_process_count.unwrap_or(0) >= count, "{usage:?}");
        }

        // And the wall clock is answerable for a *live* process, which is the
        // other half of the same defect.
        let row = projector.only(ProcessKind::VerifyCommand);
        let report = crate::process_commands::build_resource_report(&row, None, Some(i64::MAX / 2));
        let wall = report
            .limits
            .iter()
            .find(|limit| limit.limit == ProcessLimitKind::Wall.as_str())
            .expect("wall is reported");
        assert!(wall.observed.is_some(), "{wall:?}");

        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// A verify command's deadline ends the *tree*, not the shell it named.
    ///
    /// The property the resource controller brought to this path. A verify
    /// command is a build or a test runner, so the process holding the machine is
    /// a grandchild of the `sh -c` this spawns — and `kill_on_drop` reaps exactly
    /// one pid. The backgrounded `sleep` is that grandchild: before the deadline
    /// was a wall limit, it survived its own run by twenty-nine seconds.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_verify_deadline_ends_the_grandchild_and_not_only_the_shell() {
        let state = AppState::default();
        let cwd = temp_path("cwd_tree_timeout");
        std::fs::create_dir_all(&cwd).unwrap();
        // The grandchild reports its own pid, because by the time the assertion
        // runs the shell is gone and there is no tree left to walk down from.
        let pid_file = cwd.join("grandchild.pid");
        let mut cmd = command(
            "c1",
            &format!(
                "sleep 30 & echo $! > {}; sleep 30",
                pid_file.to_string_lossy()
            ),
        );
        cmd.timeout_secs = Some(1);

        let result = run_command_impl(&state, &cwd, &cmd, None, None).await;
        assert!(result.timed_out);

        let grandchild: u32 = std::fs::read_to_string(&pid_file)
            .expect("the command wrote its background pid")
            .trim()
            .parse()
            .expect("a pid");
        // Settle rather than assert on the kill's timing: a wall breach reclaims
        // the tree before returning, and a loaded host still takes a moment.
        for _ in 0..50 {
            if !crate::os_signal::process_is_alive(grandchild) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            !crate::os_signal::process_is_alive(grandchild),
            "the backgrounded grandchild outlived the deadline that was supposed to end it"
        );

        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// The capture is front-truncated at the cap while the child is running.
    ///
    /// The workload is one program reading one file, rather than the `yes | head`
    /// pipeline this used to run. That pipeline is three processes under `cmd`,
    /// and it failed here about one Windows run in four with an empty capture —
    /// on a host where nothing bounded a verify command at all until this branch,
    /// so it had never run under the job before. A flaky test in a required gate
    /// is worse than no test: it teaches everyone to re-run. The property being
    /// checked is the cap, and the cap does not care how many processes produced
    /// the bytes.
    #[tokio::test]
    async fn run_command_impl_caps_output_length() {
        let state = AppState::default();
        let cwd = temp_path("cwd_cap");
        std::fs::create_dir_all(&cwd).unwrap();
        // Well over VERIFY_OUTPUT_CAP bytes, written here so the command is a
        // single read on every host.
        std::fs::write(cwd.join("flood.txt"), "x".repeat(50_000)).unwrap();
        #[cfg(target_os = "windows")]
        let cmd = command("c1", "type flood.txt");
        #[cfg(not(target_os = "windows"))]
        let cmd = command("c1", "cat flood.txt");

        let result = run_command_impl(&state, &cwd, &cmd, None, None).await;

        assert!(
            result.stdout.len() <= VERIFY_OUTPUT_CAP + 64,
            "stdout not capped: {} chars",
            result.stdout.len()
        );
        // The whole result on failure, not just the predicate. A short stdout has
        // several causes that look identical from a bare `starts_with` — the
        // command never ran, it was reclaimed for a limit, the shell could not
        // find the program — and the first Windows failure of this assertion cost
        // a CI round to tell them apart.
        assert!(
            result.stdout.starts_with("… (truncated)"),
            "expected a front-truncated capture, got {} bytes; code={:?} timed_out={} stderr={:?}",
            result.stdout.len(),
            result.code,
            result.timed_out,
            result.stderr
        );

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
        let result = run_command_impl(&state, &cwd, &cmd, Some(&turn_id), None).await;

        assert!(!result.timed_out);
        assert!(result.code.is_none());
        assert!(result.stderr.contains("cancelled"));
        // Returned promptly after the ~100ms cancel fired, not the 30s sleep.
        assert!(started.elapsed() < Duration::from_secs(10));

        let _ = std::fs::remove_dir_all(&cwd);
    }
}
