//! User-configured lifecycle hooks (Claude-Code-style): shell commands the
//! USER wired to agent lifecycle events (PreToolUse / PostToolUse /
//! SessionStart / UserPromptSubmit). The frontend owns which hooks exist and
//! when they fire (`src/lib/userHooks.ts`); this module owns exactly two
//! things — the per-profile agent-home `hooks.json` config file, and one bounded
//! executor.
//!
//! Deliberately NOT `tools::tool_run_shell`: that command is the MODEL's
//! shell, so it permission-prompts, checkpoints, and workspace-sandboxes
//! every call. A hook is configuration the user typed into settings —
//! user-trusted by definition, so it runs without a prompt, but bounded
//! HARD (10s timeout, capped capture) because a hook that hangs would
//! otherwise stall every tool call of every turn that fires it.
//!
//! The capture mirrors a56d036's rule ("bound the shell capture at the
//! read, not at the return"): [`drain_capped`] keeps READING past the cap
//! and discards, so a chatty hook can never block on a full pipe — it just
//! loses output past the cap.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::app_paths;
use crate::AppState;

/// Hard wall-clock ceiling for one hook execution — a hook is glue, not a
/// build step; anything slower is treated as hung (`timed_out: true`) and
/// the frontend proceeds as if the hook had not answered.
const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-stream capture ceiling (bytes). A hook's stdout is a decision payload
/// or a short context block, never a log dump.
const HOOK_OUTPUT_CAP: usize = 64 * 1024;

/// What one hook execution produced. `exit_code: None` means the process
/// was killed (timeout) or died to a signal — the frontend treats anything
/// with `timed_out` as "hook did not answer" and proceeds.
#[derive(serde::Serialize)]
pub struct HookExecOutcome {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// The per-profile hooks config file. A home copy wins; an existing legacy
/// profile-data copy remains in use until the user creates the home copy.
fn hooks_file(roots: &app_paths::AgentConfigRoots) -> Result<PathBuf, String> {
    roots.effective_path("hooks.json")
}

/// Returns the raw `hooks.json` content, or an empty string when no hooks
/// were ever saved — the frontend treats both the same way.
#[tauri::command(rename_all = "snake_case")]
pub fn hooks_load() -> Result<String, String> {
    let roots = app_paths::agent_config_roots()?;
    let path = hooks_file(&roots)?;
    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(format!("Could not read {}: {}", path.display(), error)),
    }
}

/// Writes `hooks.json`. The content must at least parse as JSON — the
/// frontend owns the schema, but a corrupt write would silently disable
/// every hook on the next load, so malformed input is refused here.
#[tauri::command(rename_all = "snake_case")]
pub fn hooks_save(content: String) -> Result<(), String> {
    serde_json::from_str::<serde_json::Value>(&content)
        .map_err(|error| format!("Hooks config is not valid JSON: {}", error))?;
    let roots = app_paths::ensure_agent_config_roots()?;
    let path = hooks_file(&roots)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create {}: {}", parent.display(), error))?;
    }
    std::fs::write(&path, content)
        .map_err(|error| format!("Could not write {}: {}", path.display(), error))
}

/// Reads a pipe to EOF, KEEPING at most `cap` bytes — reading continues past
/// the cap (discarding) so the child never blocks on a full pipe, which is
/// the a56d036 bounded-capture rule this module's doc comment cites.
async fn drain_capped<R: AsyncRead + Unpin>(mut pipe: R, cap: usize) -> std::io::Result<String> {
    let mut chunk = vec![0u8; 8192];
    let mut kept: Vec<u8> = Vec::new();
    loop {
        let n = pipe.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        if kept.len() < cap {
            let take = (cap - kept.len()).min(n);
            kept.extend_from_slice(&chunk[..take]);
        }
    }
    Ok(String::from_utf8_lossy(&kept).into_owned())
}

/// Runs one user-configured hook command: spawns it through the platform
/// shell in the primary workspace root (falling back to the profile data
/// dir), writes `payload` to its stdin, and captures bounded stdout/stderr
/// under the 10-second ceiling. Never permission-prompted — see the module
/// doc comment for why that is correct for user-authored configuration and
/// would be wrong for anything model-supplied.
#[tauri::command(rename_all = "snake_case")]
pub async fn hook_exec(
    state: tauri::State<'_, AppState>,
    command: String,
    payload: String,
) -> Result<HookExecOutcome, String> {
    // Hooks usually inspect the project, so the workspace root is the natural
    // cwd — but hooks must still work in a workspace-less chat, so this falls
    // back rather than erroring like the model's own shell does.
    let cwd = crate::workspace::primary_root_canon(state.inner())
        .ok()
        .or_else(app_paths::data_dir)
        .unwrap_or_else(|| PathBuf::from("."));

    #[cfg(target_os = "windows")]
    let (shell, shell_flag) = ("cmd", "/C");
    #[cfg(not(target_os = "windows"))]
    let (shell, shell_flag) = ("sh", "-c");

    let mut builder = tokio::process::Command::new(shell);
    builder
        .arg(shell_flag)
        .arg(&command)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Its own process group, so the timeout kill below can end the hook's
    // whole tree rather than orphaning grandchildren — mirrors
    // `tools::tool_run_shell`'s spawn.
    #[cfg(unix)]
    builder.process_group(0);
    crate::os_limits::apply(crate::os_limits::ChildLimits::baseline(), &mut builder);

    let mut child = builder
        .spawn()
        .map_err(|error| format!("Failed to spawn hook: {}", error))?;
    let child_pgid = child.id();

    // Payload first, then the pipe is CLOSED (dropped) — a hook that reads
    // stdin to EOF must not hang waiting for more.
    if let Some(mut stdin) = child.stdin.take() {
        // A hook that never reads stdin and exits immediately closes the pipe;
        // the resulting write error is not a hook failure.
        let _ = stdin.write_all(payload.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }

    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| "Hook child had no stdout pipe".to_string())?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| "Hook child had no stderr pipe".to_string())?;

    let capture = async {
        let (status, stdout, stderr) = tokio::try_join!(
            child.wait(),
            drain_capped(stdout_pipe, HOOK_OUTPUT_CAP),
            drain_capped(stderr_pipe, HOOK_OUTPUT_CAP),
        )?;
        Ok::<_, std::io::Error>((status, stdout, stderr))
    };

    tokio::select! {
        result = capture => {
            let (status, stdout, stderr) = result.map_err(|error| format!("Failed to run hook: {}", error))?;
            Ok(HookExecOutcome { exit_code: status.code(), stdout, stderr, timed_out: false })
        }
        _ = tokio::time::sleep(HOOK_TIMEOUT) => {
            // End the whole tree, not just the shell (`kill_on_drop` reaps the
            // direct child as the backstop) — same lesson as tool_run_shell's
            // timeout path.
            if let Some(pgid) = child_pgid {
                if let Err(error) = crate::os_signal::terminate_process_group(pgid) {
                    eprintln!("hook_exec: could not terminate process group {pgid}: {error}");
                }
            }
            Ok(HookExecOutcome { exit_code: None, stdout: String::new(), stderr: String::new(), timed_out: true })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn hooks_use_authored_config_with_legacy_fallback() {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "little_monkey_hooks_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let roots = app_paths::AgentConfigRoots {
            profile_id: "work".to_string(),
            registry_active_id: "work".to_string(),
            agent_home: root.join("authored-home"),
            authored: root.join("authored"),
            legacy: root.join("legacy"),
        };
        std::fs::create_dir_all(&roots.legacy).unwrap();
        std::fs::write(roots.legacy.join("hooks.json"), "{}").unwrap();
        assert_eq!(hooks_file(&roots).unwrap(), roots.legacy.join("hooks.json"));

        std::fs::create_dir_all(&roots.authored).unwrap();
        std::fs::write(roots.authored.join("hooks.json"), "{}").unwrap();
        assert_eq!(
            hooks_file(&roots).unwrap(),
            roots.authored.join("hooks.json")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
