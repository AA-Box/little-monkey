//! Agent file/shell tools, exposed as Tauri commands the local model can call
//! (via OpenAI-style `tool_calls`) once the frontend's agent loop dispatches
//! them with `invoke('tool_<name>', args)`.
//!
//! Every path argument is sandboxed through [`workspace::resolve_path_and_root`],
//! which canonicalizes the requested path and rejects anything that resolves
//! outside the target workspace root — including via `..` traversal or a
//! symlink that points outside the sandbox. A path may target any attached
//! folder (see `workspace.rs`), not just the primary one. Every *mutating*
//! tool (`write_file`, `edit_file`, `run_shell`, `remember`) calls
//! [`permissions::request_permission`] and refuses to run if the user (or an
//! existing "allow for session" grant) doesn't approve it.

use std::process::Stdio;
use std::time::Duration;

use globset::GlobBuilder;
use regex::Regex;
use walkdir::WalkDir;

use crate::{checkpoints, memory, permissions, workspace, AppState};

/// Directory names that are never descended into by [`tool_grep`] — build
/// output, VCS metadata, and dependency trees are noisy, huge, and almost
/// never what the agent is looking for.
const GREP_SKIP_DIRS: [&str; 4] = [".git", "node_modules", "target", "dist"];

/// Directory names that are never descended into by [`list_workspace_paths`]
/// — VCS metadata, build output, and dependency/cache trees that would
/// otherwise flood the "@"-mention autocomplete list with noise.
///
/// `pub(crate)` (unlike `GREP_SKIP_DIRS` above) so `stacks.rs`'s source
/// folder walker can reuse the exact same skip-dir philosophy instead of
/// duplicating the list — see that module's `collect_source_files`.
pub(crate) const MENTION_SKIP_DIRS: [&str; 10] = [
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    "__pycache__",
    ".venv",
    "venv",
    ".cache",
];

/// Maximum number of entries (files + directories combined) that
/// [`list_workspace_paths`] will return before stopping early and reporting
/// `truncated: true`.
const MENTION_MAX_ENTRIES: usize = 5000;

/// Maximum number of matches [`tool_grep`] will return, regardless of how
/// many the pattern actually matches, so a broad pattern can't flood the
/// model's context window.
const GREP_MAX_MATCHES: usize = 200;

/// Maximum number of paths [`tool_glob`] will return.
const GLOB_MAX_MATCHES: usize = 300;

/// How long [`tool_run_shell`] lets a command run before it is killed and an
/// error is returned.
const SHELL_TIMEOUT: Duration = Duration::from_secs(120);

/// Read a UTF-8 text file from the workspace.
#[tauri::command]
pub async fn tool_read_file(state: tauri::State<'_, AppState>, path: String) -> Result<String, String> {
    let (resolved, _) = workspace::resolve_path_and_root(state.inner(), &path)?;

    if !resolved.is_file() {
        return Err(format!("'{}' is not a file", path));
    }

    std::fs::read_to_string(&resolved).map_err(|e| format!("Failed to read '{}': {}", path, e))
}

/// List the immediate contents of a directory in the workspace.
#[tauri::command]
pub async fn tool_list_dir(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<Vec<serde_json::Value>, String> {
    let (resolved, _) = workspace::resolve_path_and_root(state.inner(), &path)?;

    if !resolved.is_dir() {
        return Err(format!("'{}' is not a directory", path));
    }

    let read_dir = std::fs::read_dir(&resolved).map_err(|e| format!("Failed to list '{}': {}", path, e))?;

    let mut entries = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|e| format!("Failed to read entry in '{}': {}", path, e))?;
        let metadata = entry
            .metadata()
            .map_err(|e| format!("Failed to stat entry in '{}': {}", path, e))?;

        entries.push(serde_json::json!({
            "name": entry.file_name().to_string_lossy().to_string(),
            "is_dir": metadata.is_dir(),
            "size": metadata.len(),
        }));
    }

    entries.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or_default()
            .cmp(b["name"].as_str().unwrap_or_default())
    });

    Ok(entries)
}

/// Regex-search text files under `path` (defaults to the workspace root),
/// skipping VCS/build/dependency directories, capped at
/// [`GREP_MAX_MATCHES`] results.
#[tauri::command]
pub async fn tool_grep(
    state: tauri::State<'_, AppState>,
    pattern: String,
    path: Option<String>,
) -> Result<Vec<serde_json::Value>, String> {
    let regex = Regex::new(&pattern).map_err(|e| format!("Invalid regex '{}': {}", pattern, e))?;

    let (search_root, display_root) =
        workspace::resolve_path_and_root(state.inner(), path.as_deref().unwrap_or("."))?;
    let label_prefix = workspace::secondary_label_for(state.inner(), &display_root)?
        .map(|label| format!("{}/", label))
        .unwrap_or_default();

    let mut matches = Vec::new();

    let walker = WalkDir::new(&search_root)
        .into_iter()
        .filter_entry(|entry| {
            if entry.file_type().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    return !GREP_SKIP_DIRS.contains(&name);
                }
            }
            true
        });

    'outer: for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };

        if !entry.file_type().is_file() {
            continue;
        }

        let content = match std::fs::read_to_string(entry.path()) {
            Ok(content) => content,
            Err(_) => continue, // binary or unreadable file — skip silently
        };

        let display_path = format!(
            "{}{}",
            label_prefix,
            entry
                .path()
                .strip_prefix(&display_root)
                .unwrap_or_else(|_| entry.path())
                .to_string_lossy()
        );

        for (idx, line) in content.lines().enumerate() {
            if regex.is_match(line) {
                matches.push(serde_json::json!({
                    "file": display_path,
                    "line": idx + 1,
                    "text": line,
                }));

                if matches.len() >= GREP_MAX_MATCHES {
                    break 'outer;
                }
            }
        }
    }

    Ok(matches)
}

/// Find files by glob pattern (e.g. `**/*.ts`, `src/**/test_*.py`) under
/// `path` (defaults to the workspace root), skipping VCS/build/dependency
/// directories, capped at [`GLOB_MAX_MATCHES`] results sorted by most
/// recently modified first.
#[tauri::command]
pub async fn tool_glob(
    state: tauri::State<'_, AppState>,
    pattern: String,
    path: Option<String>,
) -> Result<Vec<String>, String> {
    let (search_root, display_root) =
        workspace::resolve_path_and_root(state.inner(), path.as_deref().unwrap_or("."))?;
    let label_prefix = workspace::secondary_label_for(state.inner(), &display_root)?
        .map(|label| format!("{}/", label))
        .unwrap_or_default();

    glob_impl(&pattern, &search_root, &display_root, &label_prefix)
}

/// Core glob logic, separated from workspace-root plumbing for testability.
fn glob_impl(
    pattern: &str,
    search_root: &std::path::Path,
    display_root: &std::path::Path,
    label_prefix: &str,
) -> Result<Vec<String>, String> {
    let matcher = GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .map_err(|e| format!("Invalid glob pattern '{}': {}", pattern, e))?
        .compile_matcher();

    let mut matches: Vec<(std::time::SystemTime, String)> = Vec::new();

    let walker = WalkDir::new(search_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() > 0 && entry.file_type().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    return !MENTION_SKIP_DIRS.contains(&name);
                }
            }
            true
        });

    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }

        let relative = entry.path().strip_prefix(search_root).unwrap_or_else(|_| entry.path());
        if !matcher.is_match(relative) {
            continue;
        }

        let display_path = format!(
            "{}{}",
            label_prefix,
            entry
                .path()
                .strip_prefix(display_root)
                .unwrap_or_else(|_| entry.path())
                .to_string_lossy()
        );
        let modified = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        matches.push((modified, display_path));
    }

    // Most recently modified first — the file the agent wants is usually the
    // one being worked on.
    matches.sort_by(|a, b| b.0.cmp(&a.0));
    matches.truncate(GLOB_MAX_MATCHES);

    Ok(matches.into_iter().map(|(_, path)| path).collect())
}

/// Write (overwrite/create) a text file in the workspace. Permission-gated.
/// `checkpoint_id` is injected by the frontend agent loop (not the model) so
/// the pre-mutation backup lands in the calling turn's own checkpoint.
/// `risk_level`/`risk_reason` are likewise frontend-injected (never
/// model-suppliable — see `turnEngine.ts`'s `executeToolCall`, which
/// unconditionally scrubs any risk keys the model's own arguments JSON might
/// contain before ever setting these): the optional LLM risk-judge
/// classification for this call, combined here with the authoritative
/// `permissions::path_risk_floor` (which always wins) into the
/// `RiskAssessment` shown on the permission prompt. `agent_label` is the same
/// story — frontend-injected only — but is passed straight through to
/// [`permissions::request_permission`] as its own field rather than folded
/// into `detail`: see that field's doc comment on
/// `PermissionRequestPayload` for why detail-prefixing was the bug (a
/// `code`-profile subagent's `description` is itself model-supplied text,
/// and folding it into a string the frontend later re-parses by regex let a
/// crafted description forge/corrupt the shown detail).
///
/// `file_write_lock` (see `AppState`'s doc comment on that field) is
/// acquired AFTER permission is granted, held across the checkpoint backup
/// and the write itself, and released before returning — the whole point is
/// to serialize the backup+write pair for a given path against another
/// concurrent `write_file`/`edit_file` call (most plausibly two `code`-
/// profile subagents in the same round, see
/// `agentLoop.ts::runToolCallsForRound`) that resolves to the SAME path,
/// which could otherwise race past `record_original`'s dedup and interleave
/// with this call's own `std::fs::write`, silently discarding one write with
/// no error. Never held across an `.await` (permission is requested BEFORE
/// acquiring it), so a plain `std::sync::Mutex` guard is safe to hold here.
///
/// `rename_all = "snake_case"`: the model's tool-call arguments arrive with
/// snake_case keys (as declared in the frontend tool schema) and are passed
/// through verbatim, so the invoke payload must be matched by snake_case
/// names rather than the macro's camelCase default.
#[tauri::command(rename_all = "snake_case")]
pub async fn tool_write_file(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
    content: String,
    checkpoint_id: Option<String>,
    turn_id: Option<String>,
    risk_level: Option<String>,
    risk_reason: Option<String>,
    agent_label: Option<String>,
) -> Result<String, String> {
    // Resolved BEFORE the permission prompt (unlike this function's
    // pre-Phase-2 ordering) so `path_risk_floor` can be checked against the
    // actual sandboxed/canonicalized target — an invalid path now fails
    // before a prompt is even shown, which is also strictly safer.
    let (resolved, root) = workspace::resolve_path_and_root(state.inner(), &path)?;
    let risk = permissions::compute_risk(Some((&resolved, &root)), risk_level, risk_reason);

    let detail = format!("Write {} bytes to {}", content.len(), path);
    permissions::request_permission(&app, state.inner(), "write_file", detail, turn_id.as_deref(), risk, agent_label.as_deref())
        .await?;

    // Serializes the backup+write critical section against any other
    // concurrent write_file/edit_file targeting the same path — see this
    // function's own doc comment above for the race this closes. Dropped
    // automatically at the end of this synchronous block (no `.await` while
    // held).
    let _write_guard = state
        .file_write_lock
        .lock()
        .map_err(|_| "File-write lock poisoned".to_string())?;

    checkpoints::record_original(state.inner(), checkpoint_id.as_deref(), &resolved)?;

    if let Some(parent) = resolved.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create parent directories for '{}': {}", path, e))?;
    }

    std::fs::write(&resolved, &content).map_err(|e| format!("Failed to write '{}': {}", path, e))?;

    Ok(format!("Wrote {} bytes to {}", content.len(), path))
}

/// Build a short, human-readable diff-style preview (no external diff crate)
/// for the permission prompt shown before an edit is applied.
fn build_diff_preview(old_string: &str, new_string: &str) -> String {
    const MAX_PREVIEW_LINES: usize = 6;
    const MAX_LINE_CHARS: usize = 120;

    fn truncate(line: &str) -> String {
        if line.chars().count() > MAX_LINE_CHARS {
            let mut truncated: String = line.chars().take(MAX_LINE_CHARS).collect();
            truncated.push('…');
            truncated
        } else {
            line.to_string()
        }
    }

    let mut preview = Vec::new();

    let old_lines: Vec<&str> = old_string.lines().collect();
    for line in old_lines.iter().take(MAX_PREVIEW_LINES) {
        preview.push(format!("- {}", truncate(line)));
    }
    if old_lines.len() > MAX_PREVIEW_LINES {
        preview.push(format!("  … ({} more removed lines)", old_lines.len() - MAX_PREVIEW_LINES));
    }

    let new_lines: Vec<&str> = new_string.lines().collect();
    for line in new_lines.iter().take(MAX_PREVIEW_LINES) {
        preview.push(format!("+ {}", truncate(line)));
    }
    if new_lines.len() > MAX_PREVIEW_LINES {
        preview.push(format!("  … ({} more added lines)", new_lines.len() - MAX_PREVIEW_LINES));
    }

    preview.join("\n")
}

/// Replace a single, unique occurrence of `old_string` with `new_string` in a
/// workspace file. Permission-gated; errors if `old_string` isn't found, or
/// is found more than once (to avoid ambiguous edits). `checkpoint_id` is
/// injected by the frontend agent loop (not the model) so the pre-mutation
/// backup lands in the calling turn's own checkpoint. `risk_level`/
/// `risk_reason`/`agent_label` are likewise frontend-injected — see
/// `tool_write_file`'s doc comment, identical treatment here.
///
/// The initial `current`/`occurrences` check below (before the permission
/// prompt) is a best-effort pre-check only, purely to build the diff preview
/// and reject an obviously-bad call before ever prompting. The content it
/// actually mutates is RE-READ fresh from disk after `file_write_lock` is
/// acquired (see `tool_write_file`'s doc comment on that field/lock) and the
/// occurrence check is redone against that fresh read — so if another
/// concurrent `write_file`/`edit_file` call for the SAME path completed in
/// between (most plausibly two `code`-profile subagents in the same round —
/// see `agentLoop.ts::runToolCallsForRound`), this call correctly errors
/// (`old_string` no longer found/unique) instead of silently clobbering that
/// other call's write with a `replacen` computed against stale content.
///
/// `rename_all = "snake_case"`: the model's tool-call arguments arrive with
/// snake_case keys (as declared in the frontend tool schema) and are passed
/// through verbatim, so the invoke payload must be matched by snake_case
/// names rather than the macro's camelCase default.
#[tauri::command(rename_all = "snake_case")]
pub async fn tool_edit_file<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    path: String,
    old_string: String,
    new_string: String,
    checkpoint_id: Option<String>,
    turn_id: Option<String>,
    risk_level: Option<String>,
    risk_reason: Option<String>,
    agent_label: Option<String>,
) -> Result<String, String> {
    if old_string.is_empty() {
        return Err("old_string must not be empty".to_string());
    }

    let (resolved, root) = workspace::resolve_path_and_root(state.inner(), &path)?;

    if !resolved.is_file() {
        return Err(format!("'{}' is not a file", path));
    }

    let current =
        std::fs::read_to_string(&resolved).map_err(|e| format!("Failed to read '{}': {}", path, e))?;

    let occurrences = current.matches(old_string.as_str()).count();
    if occurrences == 0 {
        return Err(format!("old_string not found in '{}'", path));
    }
    if occurrences > 1 {
        return Err(format!(
            "old_string appears {} times in '{}'; it must be unique. Include more surrounding context.",
            occurrences, path
        ));
    }

    let risk = permissions::compute_risk(Some((&resolved, &root)), risk_level, risk_reason);
    let preview = build_diff_preview(&old_string, &new_string);
    let detail = format!("Edit {}\n{}", path, preview);

    permissions::request_permission(&app, state.inner(), "edit_file", detail, turn_id.as_deref(), risk, agent_label.as_deref())
        .await?;

    // Serializes the re-read+backup+write critical section against any
    // other concurrent write_file/edit_file targeting the same path — see
    // this function's own doc comment above and `tool_write_file`'s for the
    // race this closes.
    let _write_guard = state
        .file_write_lock
        .lock()
        .map_err(|_| "File-write lock poisoned".to_string())?;

    // Re-read fresh, now that we hold the lock: `current` above may already
    // be stale if another call mutated this same path while this call's own
    // permission prompt was pending.
    let fresh = std::fs::read_to_string(&resolved).map_err(|e| format!("Failed to read '{}': {}", path, e))?;
    let fresh_occurrences = fresh.matches(old_string.as_str()).count();
    if fresh_occurrences == 0 {
        return Err(format!(
            "old_string not found in '{}' — the file changed since this edit was prepared (likely a concurrent edit).",
            path
        ));
    }
    if fresh_occurrences > 1 {
        return Err(format!(
            "old_string appears {} times in '{}'; it must be unique. Include more surrounding context.",
            fresh_occurrences, path
        ));
    }

    checkpoints::record_original(state.inner(), checkpoint_id.as_deref(), &resolved)?;

    let updated = fresh.replacen(old_string.as_str(), new_string.as_str(), 1);
    std::fs::write(&resolved, &updated).map_err(|e| format!("Failed to write '{}': {}", path, e))?;

    Ok(format!("Edited {}", path))
}

/// Run a shell command (via `sh -c`, or `cmd /C` on Windows) rooted at `cwd`
/// (defaults to the workspace root), with a hard timeout. Permission-gated.
/// `checkpoint_id` is injected by the frontend agent loop (not the model), the
/// same as `tool_write_file`/`tool_edit_file` — but here it isn't used to
/// snapshot anything (shell side effects aren't captured); it only flags the
/// owning turn's checkpoint as `shell_ran` so the UI can show a revert-coverage
/// caveat. `risk_level`/`risk_reason` are likewise frontend-injected, DISPLAY
/// PURPOSES ONLY — there is no path here for `permissions::path_risk_floor`
/// (a shell command has no single filesystem target to floor-check), so the
/// risk shown is judge-only, and — this is the load-bearing invariant, see
/// `permissions.rs`'s module doc comment and `mode_short_circuit` — it can
/// NEVER be threaded into anything that decides whether this call is
/// auto-approved. `run_shell` always falls through to a real prompt in every
/// mode below `"bypass"`, full stop. `agent_label` is passed straight through
/// to `request_permission` as its own field (see that field's doc comment on
/// `PermissionRequestPayload`) — same cosmetic-only treatment, and the same
/// "never affects auto-approval" guarantee applies to it too. Deliberately
/// NOT folded into `detail`: `command` here is the raw, fully model-supplied
/// shell command text, and a detail-string prefix a model could itself
/// mimic (e.g. a command literally containing `"Subagent 'x': ..."`) would
/// let a crafted command spoof/misattribute an ordinary parent-turn command
/// as a vetted subagent's — passing `agent_label` as its own field instead
/// of text `detail` shares means there is nothing for `command` to forge.
#[tauri::command(rename_all = "snake_case")]
pub async fn tool_run_shell(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    command: String,
    cwd: Option<String>,
    checkpoint_id: Option<String>,
    turn_id: Option<String>,
    risk_level: Option<String>,
    risk_reason: Option<String>,
    agent_label: Option<String>,
) -> Result<serde_json::Value, String> {
    let risk = permissions::compute_risk(None, risk_level, risk_reason);
    permissions::request_permission(&app, state.inner(), "run_shell", command.clone(), turn_id.as_deref(), risk, agent_label.as_deref())
        .await?;

    checkpoints::record_shell(state.inner(), checkpoint_id.as_deref())?;

    let cwd_path = match cwd {
        Some(ref c) => workspace::resolve_path_and_root(state.inner(), c)?.0,
        None => workspace::primary_root_canon(state.inner())?,
    };

    // `sh` does not exist on Windows (and the app bundles for all targets) —
    // use the platform's own command interpreter there.
    #[cfg(target_os = "windows")]
    let (shell, shell_flag) = ("cmd", "/C");
    #[cfg(not(target_os = "windows"))]
    let (shell, shell_flag) = ("sh", "-c");

    let mut command_builder = tokio::process::Command::new(shell);
    command_builder
        .arg(shell_flag)
        .arg(&command)
        .current_dir(&cwd_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Both the timeout and cancellation branches below work by DROPPING
        // the in-flight `wait_with_output` future (and the child with it) —
        // without this, the spawned process would keep running orphaned
        // after a timeout or a Stop-button cancellation.
        .kill_on_drop(true);

    let child = command_builder
        .spawn()
        .map_err(|e| format!("Failed to spawn shell: {}", e))?;

    // Each turn gets its own cancellation channel so Stop in one pane never
    // kills a command the other pane's turn is still running. Callers that
    // don't thread a turn id share the "" channel.
    let cancel_key = turn_id.unwrap_or_default();
    let cancel = state
        .tool_cancel
        .lock()
        .map_err(|_| "Tool-cancel lock poisoned".to_string())?
        .entry(cancel_key.clone())
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Notify::new()))
        .clone();

    let outcome = tokio::select! {
        result = child.wait_with_output() => {
            result.map_err(|e| format!("Failed to run command: {}", e))
        }
        _ = cancel.notified() => {
            Err("Command cancelled by the user".to_string())
        }
        _ = tokio::time::sleep(SHELL_TIMEOUT) => {
            Err(format!(
                "Command timed out after {} seconds",
                SHELL_TIMEOUT.as_secs()
            ))
        }
    };

    // Drop this turn's channel once no other shell of the same turn still
    // holds it (strong count 2 = the map's Arc + our clone), so the map
    // doesn't accumulate one entry per turn forever. A racing new shell for
    // the same turn simply recreates the entry.
    {
        let mut guard = state
            .tool_cancel
            .lock()
            .map_err(|_| "Tool-cancel lock poisoned".to_string())?;
        if guard.get(&cancel_key).is_some_and(|n| std::sync::Arc::strong_count(n) <= 2) {
            guard.remove(&cancel_key);
        }
    }

    let output = outcome?;
    Ok(serde_json::json!({
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
        "code": output.status.code(),
    }))
}

/// Save a short durable fact about the current project/user preferences to
/// `<app_data>/memories.json` (see `memory.rs`), so it's injected into every
/// future turn's system prompt. Permission-gated (auto-allowed in
/// acceptEdits/auto, blocked in plan mode — see `permissions::mode_short_circuit`).
/// Takes no path — it only ever writes app-data, never a workspace file — so
/// unlike the other mutating tools it skips `workspace::resolve_path_and_root`
/// sandboxing entirely. When no workspace is open, the fact is keyed under
/// `memory::GLOBAL_SCOPE_KEY` instead of a project root — otherwise a plain
/// chat with no folder open (e.g. "remember my name") silently had nowhere
/// to save to and the tool call failed outright, even though the model had
/// already told the user it remembered.
///
/// `checkpoint_id` is deliberately NOT accepted here (unlike write/edit/
/// run_shell): a remembered fact isn't a workspace file, so there is nothing
/// for a per-turn checkpoint to snapshot or revert. `turn_id` is injected by
/// the frontend agent loop (never model-supplied) purely to scope the
/// permission prompt to the calling turn, exactly as it does for the other
/// mutating tools.
///
/// `rename_all = "snake_case"`: matches every other tool command, so the
/// model's snake_case tool-call arguments (and the agent loop's injected
/// `turn_id`) are accepted without translation.
#[tauri::command(rename_all = "snake_case")]
pub async fn tool_remember(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    text: String,
    turn_id: Option<String>,
) -> Result<memory::Fact, String> {
    permissions::request_permission(&app, state.inner(), "remember", text.clone(), turn_id.as_deref(), None, None)
        .await?;

    let root = workspace::primary_root_canon(state.inner())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| memory::GLOBAL_SCOPE_KEY.to_string());
    let path = memory::memories_file_path(&app)?;

    // Serialized against concurrent split-pane `tool_remember` calls (and
    // against `memory_add`/`memory_delete`) via `AppState::memory_lock` — the
    // whole `memories.json` file is rewritten on every add, so two
    // unsynchronized concurrent writers could otherwise silently drop one
    // fact's write.
    let _lock = state.memory_lock.lock().map_err(|_| "Memory lock poisoned".to_string())?;
    memory::add_fact_impl(&path, &root, &text, "agent")
}

/// Cancel in-flight tool invocations: kills running `tool_run_shell` child
/// processes (via the per-turn cancel notification each one selects on) and
/// denies permission prompts still awaiting an answer. Invoked by the
/// frontend when the user hits Stop while a tool call is executing.
/// `turn_id` of `Some` scopes the cancellation to that turn — with the split
/// pane, the other pane's turn may have its own shell command or prompt in
/// flight that this Stop must not touch. `None` cancels everything.
#[tauri::command]
pub fn tools_cancel_running(
    state: tauri::State<'_, AppState>,
    turn_id: Option<String>,
) -> Result<(), String> {
    let notifies: Vec<std::sync::Arc<tokio::sync::Notify>> = {
        let guard = state
            .tool_cancel
            .lock()
            .map_err(|_| "Tool-cancel lock poisoned".to_string())?;
        match turn_id.as_deref() {
            Some(turn) => guard.get(turn).cloned().into_iter().collect(),
            None => guard.values().cloned().collect(),
        }
    };
    for notify in notifies {
        notify.notify_waiters();
    }
    permissions::deny_pending(state.inner(), turn_id.as_deref());
    Ok(())
}

/// A single workspace-relative path, for the "@"-mention autocomplete list
/// in the chat input.
#[derive(serde::Serialize)]
pub struct WorkspacePathEntry {
    pub path: String,
    pub is_dir: bool,
}

/// Result of [`list_workspace_paths`]: every workspace-relative path found,
/// capped at [`MENTION_MAX_ENTRIES`].
#[derive(serde::Serialize)]
pub struct WorkspacePathsResult {
    pub entries: Vec<WorkspacePathEntry>,
    pub truncated: bool,
}

/// Recursively list every file and directory path in the open workspace, for
/// the chat input's "@"-mention autocomplete. This is read-only,
/// non-sensitive metadata (paths only, no file contents) — like
/// [`tool_list_dir`] and [`tool_grep`], it is intentionally NOT
/// permission-gated.
#[tauri::command]
pub fn list_workspace_paths(state: tauri::State<'_, AppState>) -> Result<WorkspacePathsResult, String> {
    let roots = workspace::all_roots(state.inner())?;

    let mut entries = Vec::new();
    let mut truncated = false;

    'roots: for (root, label, is_primary) in roots {
        let walker = WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                if entry.depth() == 0 {
                    return true;
                }
                if entry.file_type().is_dir() {
                    if let Some(name) = entry.file_name().to_str() {
                        return !MENTION_SKIP_DIRS.contains(&name);
                    }
                }
                true
            });

        for entry in walker {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };

            if entry.depth() == 0 {
                continue; // a root itself is not an entry
            }

            if entries.len() >= MENTION_MAX_ENTRIES {
                truncated = true;
                break 'roots;
            }

            let relative = match entry.path().strip_prefix(&root) {
                Ok(relative) => relative,
                Err(_) => continue,
            };

            let relative_str = relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");

            // Primary-root entries stay unprefixed (no behavior change for
            // the common single-folder case); secondary-root entries are
            // prefixed with their label so the model can address them via
            // `workspace::resolve_path_and_root`.
            let path = if is_primary {
                relative_str
            } else {
                format!("{}/{}", label, relative_str)
            };

            entries.push(WorkspacePathEntry {
                path,
                is_dir: entry.file_type().is_dir(),
            });
        }
    }

    Ok(WorkspacePathsResult { entries, truncated })
}

// Sandbox/multi-root resolution tests live in workspace.rs now, alongside
// resolve_path_and_root itself.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_preview_contains_removed_and_added_markers() {
        let preview = build_diff_preview("old line", "new line");
        assert!(preview.contains("- old line"));
        assert!(preview.contains("+ new line"));
    }

    struct TempTree {
        path: std::path::PathBuf,
    }

    impl TempTree {
        fn new() -> Self {
            // Nanos alone can collide across parallel test threads — the
            // atomic counter guarantees uniqueness within the process.
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "little_monkey_glob_test_{}_{}_{}",
                std::process::id(),
                n,
                nanos
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempTree { path }
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn glob_matches_by_extension_recursively() {
        let tree = TempTree::new();
        std::fs::create_dir_all(tree.path.join("src/deep")).unwrap();
        std::fs::write(tree.path.join("src/a.ts"), "").unwrap();
        std::fs::write(tree.path.join("src/deep/b.ts"), "").unwrap();
        std::fs::write(tree.path.join("src/c.rs"), "").unwrap();

        let results = glob_impl("**/*.ts", &tree.path, &tree.path, "").unwrap();

        assert_eq!(results.len(), 2, "unexpected results: {results:?}");
        assert!(results.iter().any(|p| p == "src/a.ts"));
        assert!(results.iter().any(|p| p == "src/deep/b.ts"));
    }

    #[test]
    fn glob_skips_dependency_directories() {
        let tree = TempTree::new();
        std::fs::create_dir_all(tree.path.join("node_modules/pkg")).unwrap();
        std::fs::write(tree.path.join("node_modules/pkg/index.ts"), "").unwrap();
        std::fs::write(tree.path.join("main.ts"), "").unwrap();

        let results = glob_impl("**/*.ts", &tree.path, &tree.path, "").unwrap();

        assert_eq!(results, vec!["main.ts".to_string()]);
    }

    #[test]
    fn glob_rejects_invalid_pattern() {
        let tree = TempTree::new();
        let err = glob_impl("a{b", &tree.path, &tree.path, "").unwrap_err();
        assert!(err.contains("Invalid glob pattern"), "unexpected error: {err}");
    }

    #[test]
    fn glob_prefixes_secondary_root_label() {
        let tree = TempTree::new();
        std::fs::write(tree.path.join("notes.md"), "").unwrap();

        let results = glob_impl("*.md", &tree.path, &tree.path, "other/").unwrap();

        assert_eq!(results, vec!["other/notes.md".to_string()]);
    }

    /// Builds a mock Tauri app whose workspace root is `root`, with the
    /// permission mode preset so `edit_file`/`write_file` auto-approve
    /// instead of hanging on a prompt no one can answer in a test.
    fn mock_app_with_workspace(root: &std::path::Path) -> tauri::App<tauri::test::MockRuntime> {
        let canonical = root.canonicalize().unwrap();
        let checkpoint_dir = canonical.join(".checkpoint");
        std::fs::create_dir_all(&checkpoint_dir).unwrap();

        let state = crate::AppState::default();
        *state.permissions.mode.lock().unwrap() = "acceptEdits".to_string();
        state.workspace_roots.lock().unwrap().push(workspace::WorkspaceRoot {
            id: canonical.to_string_lossy().to_string(),
            label: "test".to_string(),
            path: canonical,
        });
        state.checkpoints.lock().unwrap().insert(
            "test-checkpoint".to_string(),
            checkpoints::ActiveCheckpoint {
                dir: checkpoint_dir,
                entries: Vec::new(),
                created_at_ms: 0,
                session_id: String::new(),
                anchor_index: 0,
                label: String::new(),
                shell_ran: false,
                prev_id: None,
            },
        );

        tauri::test::mock_builder()
            .invoke_handler(tauri::generate_handler![tool_edit_file])
            .manage(state)
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap()
    }

    fn edit_file_invoke_request(args: serde_json::Value) -> tauri::webview::InvokeRequest {
        tauri::webview::InvokeRequest {
            cmd: "tool_edit_file".to_string(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: if cfg!(any(windows, target_os = "android")) {
                "http://tauri.localhost"
            } else {
                "tauri://localhost"
            }
            .parse()
            .unwrap(),
            body: tauri::ipc::InvokeBody::Json(args),
            headers: Default::default(),
            invoke_key: tauri::test::INVOKE_KEY.to_string(),
        }
    }

    /// The model emits snake_case argument keys (as declared in the frontend
    /// tool schema) and the agent loop forwards them verbatim, so the IPC
    /// layer must accept them — this pins `rename_all = "snake_case"` on the
    /// command, without which the macro only matches camelCase keys and every
    /// edit_file call fails with "missing required key oldString".
    #[test]
    fn edit_file_ipc_accepts_snake_case_argument_keys() {
        let tree = TempTree::new();
        std::fs::write(tree.path.join("hello.txt"), "hello old world").unwrap();

        let app = mock_app_with_workspace(&tree.path);
        let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();

        let response = tauri::test::get_ipc_response(
            &webview,
            edit_file_invoke_request(serde_json::json!({
                "path": "hello.txt",
                "old_string": "old",
                "new_string": "new",
                "checkpoint_id": "test-checkpoint",
            })),
        );

        assert!(response.is_ok(), "snake_case invoke failed: {response:?}");
        assert_eq!(
            std::fs::read_to_string(tree.path.join("hello.txt")).unwrap(),
            "hello new world"
        );

        // The snake_case `checkpoint_id` key must reach the command too (the
        // agent loop injects it in that form) — proven by the pre-edit backup
        // recorded in the matching active checkpoint.
        use tauri::Manager;
        let state = app.state::<crate::AppState>();
        let checkpoints = state.checkpoints.lock().unwrap();
        let entries = &checkpoints["test-checkpoint"].entries;
        assert_eq!(entries.len(), 1, "expected one checkpoint entry");
        assert!(entries[0].path.ends_with("hello.txt"));
        assert!(entries[0].backup.is_some(), "pre-edit backup missing");
    }

    /// Companion to the test above: camelCase keys must NOT match, proving
    /// the rename is actually in effect (nothing in the app sends camelCase
    /// to this command anymore — the agent loop's checkpoint_id injection is
    /// snake_case too).
    #[test]
    fn edit_file_ipc_rejects_camel_case_argument_keys() {
        let tree = TempTree::new();
        std::fs::write(tree.path.join("hello.txt"), "hello old world").unwrap();

        let app = mock_app_with_workspace(&tree.path);
        let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();

        let response = tauri::test::get_ipc_response(
            &webview,
            edit_file_invoke_request(serde_json::json!({
                "path": "hello.txt",
                "oldString": "old",
                "newString": "new",
            })),
        );

        assert!(response.is_err(), "camelCase keys unexpectedly accepted");
        assert_eq!(
            std::fs::read_to_string(tree.path.join("hello.txt")).unwrap(),
            "hello old world"
        );
    }

    /// Reproduces (and pins the fix for) the `tool_edit_file` half of the
    /// review-flagged concurrent-write race: two concurrent edits targeting
    /// the SAME path, both prepared against the same pre-existing
    /// `old_string`, driven through the real command function (not just the
    /// underlying primitives — see `checkpoints.rs`'s own concurrency test
    /// for that side) via genuine tokio multi-thread parallelism. Without
    /// `file_write_lock` and the fresh re-read/re-check performed under it
    /// (see `tool_edit_file`'s doc comment), both calls could see
    /// `old_string` present in their own pre-permission read and both
    /// blindly `replacen` + write, silently discarding one edit with no
    /// error. With the fix, exactly one call wins and the other correctly
    /// errors (`old_string` no longer present) instead of corrupting the
    /// file or losing a write silently.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_edit_file_calls_to_the_same_path_never_silently_lose_a_write() {
        use tauri::Manager;

        for _ in 0..20 {
            let tree = TempTree::new();
            std::fs::write(tree.path.join("shared.txt"), "hello OLD world").unwrap();

            let app = mock_app_with_workspace(&tree.path);
            let handle = app.handle().clone();

            let run = |handle: tauri::AppHandle<tauri::test::MockRuntime>, new_value: &'static str| {
                tokio::spawn(async move {
                    // Widen the window for the two calls to genuinely
                    // overlap before either takes the file-write lock.
                    tokio::task::yield_now().await;
                    let state = handle.state::<crate::AppState>();
                    tool_edit_file(
                        handle.clone(),
                        state,
                        "shared.txt".to_string(),
                        "OLD".to_string(),
                        new_value.to_string(),
                        Some("test-checkpoint".to_string()),
                        None,
                        None,
                        None,
                        None,
                    )
                    .await
                })
            };

            let a = run(handle.clone(), "FROM_A");
            let b = run(handle.clone(), "FROM_B");
            let (result_a, result_b) = tokio::join!(a, b);
            let result_a = result_a.unwrap();
            let result_b = result_b.unwrap();

            let successes = [&result_a, &result_b].iter().filter(|r| r.is_ok()).count();
            assert_eq!(successes, 1, "expected exactly one edit to win, got: {result_a:?} / {result_b:?}");

            let final_content = std::fs::read_to_string(tree.path.join("shared.txt")).unwrap();
            assert!(
                final_content == "hello FROM_A world" || final_content == "hello FROM_B world",
                "file content corrupted rather than a clean win by one editor: {final_content:?}"
            );
        }
    }
}
