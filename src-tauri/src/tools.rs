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

use std::time::Duration;

use globset::GlobBuilder;
use regex::Regex;
use walkdir::WalkDir;

use crate::workspace::display_relative_path;
use crate::{
    artifact_commands, artifact_store::ArtifactStore, checkpoints, memory, native_skill_commands,
    permissions, workspace, AppState,
};

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
#[tauri::command(rename_all = "snake_case")]
pub async fn tool_read_file(
    state: tauri::State<'_, AppState>,
    path: String,
    workspace_root_override: Option<String>,
) -> Result<String, String> {
    let (resolved, _) = crate::agent_worktrees::resolve_with_override(state.inner(), &path, workspace_root_override.as_deref())?;

    if !resolved.is_file() {
        return Err(format!("'{}' is not a file", path));
    }

    std::fs::read_to_string(&resolved).map_err(|e| format!("Failed to read '{}': {}", path, e))
}

/// List the immediate contents of a directory in the workspace.
#[tauri::command(rename_all = "snake_case")]
pub async fn tool_list_dir(
    state: tauri::State<'_, AppState>,
    path: String,
    workspace_root_override: Option<String>,
) -> Result<Vec<serde_json::Value>, String> {
    let (resolved, _) = crate::agent_worktrees::resolve_with_override(state.inner(), &path, workspace_root_override.as_deref())?;

    if !resolved.is_dir() {
        return Err(format!("'{}' is not a directory", path));
    }

    let read_dir =
        std::fs::read_dir(&resolved).map_err(|e| format!("Failed to list '{}': {}", path, e))?;

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
#[tauri::command(rename_all = "snake_case")]
pub async fn tool_grep(
    state: tauri::State<'_, AppState>,
    pattern: String,
    path: Option<String>,
    workspace_root_override: Option<String>,
) -> Result<Vec<serde_json::Value>, String> {
    let regex = Regex::new(&pattern).map_err(|e| format!("Invalid regex '{}': {}", pattern, e))?;

    let (search_root, display_root) =
        crate::agent_worktrees::resolve_with_override(state.inner(), path.as_deref().unwrap_or("."), workspace_root_override.as_deref())?;
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
            display_relative_path(
                entry
                    .path()
                    .strip_prefix(&display_root)
                    .unwrap_or_else(|_| entry.path())
            )
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
#[tauri::command(rename_all = "snake_case")]
pub async fn tool_glob(
    state: tauri::State<'_, AppState>,
    pattern: String,
    path: Option<String>,
    workspace_root_override: Option<String>,
) -> Result<Vec<String>, String> {
    let (search_root, display_root) =
        crate::agent_worktrees::resolve_with_override(state.inner(), path.as_deref().unwrap_or("."), workspace_root_override.as_deref())?;
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

        let relative = entry
            .path()
            .strip_prefix(search_root)
            .unwrap_or_else(|_| entry.path());
        if !matcher.is_match(relative) {
            continue;
        }

        let display_path = format!(
            "{}{}",
            label_prefix,
            display_relative_path(
                entry
                    .path()
                    .strip_prefix(display_root)
                    .unwrap_or_else(|_| entry.path())
            )
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
    tool_call_id: Option<String>,
    risk_level: Option<String>,
    risk_reason: Option<String>,
    agent_label: Option<String>,
    workspace_root_override: Option<String>,
) -> Result<String, String> {
    // Resolved BEFORE the permission prompt (unlike this function's
    // pre-Phase-2 ordering) so `path_risk_floor` can be checked against the
    // actual sandboxed/canonicalized target — an invalid path now fails
    // before a prompt is even shown, which is also strictly safer. The
    // worktree override (validated against the managed registry inside
    // `resolve_with_override`) is applied at this same point for the same
    // reason: the floor and the prompt must describe the real target.
    let (resolved, root) =
        crate::agent_worktrees::resolve_with_override(state.inner(), &path, workspace_root_override.as_deref())?;
    let risk = permissions::compute_risk(Some((&resolved, &root)), risk_level, risk_reason);

    let detail = format!("Write {} bytes to {}", content.len(), path);
    permissions::request_permission(
        &app,
        state.inner(),
        "write_file",
        detail,
        turn_id.as_deref(),
        tool_call_id.as_deref(),
        risk,
        agent_label.as_deref(),
    )
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

    std::fs::write(&resolved, &content)
        .map_err(|e| format!("Failed to write '{}': {}", path, e))?;

    Ok(format!("Wrote {} bytes to {}", content.len(), path))
}

/// Extension → IANA media type for the image formats the chat's inline
/// preview can display. Case-insensitive; `None` for everything else.
fn image_mime_for_path(path: &std::path::Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "svg" => Some("image/svg+xml"),
        _ => None,
    }
}

/// Cap on the decoded size of a generated PNG ([`tool_generate_image`]) and
/// on a previewed image file's on-disk size ([`workspace_read_image`]). Both
/// travel through IPC as base64 (~4/3 inflation), so this also bounds the
/// transport payload.
const IMAGE_MAX_BYTES: u64 = 20 * 1024 * 1024;

/// Receipt persisted in the tool result. The transcript stores this compact
/// reference while the PNG bytes live in the app-owned durable artifact
/// store, so previews survive restarts without requiring an open workspace.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct GeneratedImageReceipt {
    artifact_id: String,
    media_type: &'static str,
    width: u32,
    height: u32,
    size: u64,
    suggested_name: String,
}

fn generated_image_filename(
    value: &str,
    timestamp: &str,
    uniqueness_suffix: &str,
) -> Result<String, String> {
    let trimmed = value.trim();
    let name = trimmed
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or_default();
    if name.is_empty() || !name.to_ascii_lowercase().ends_with(".png") {
        return Err(format!("'{value}' must be a PNG filename ending in .png"));
    }
    let raw_stem = &name[..name.len() - 4];
    let mut safe_stem = String::with_capacity(raw_stem.len());
    for character in raw_stem.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
            safe_stem.push(character);
        } else if !safe_stem.ends_with('-') {
            safe_stem.push('-');
        }
    }
    let safe_stem = safe_stem.trim_matches('-');
    let safe_stem = if safe_stem.is_empty() {
        "generated-image"
    } else {
        safe_stem
    };
    Ok(format!("{safe_stem}-{timestamp}-{uniqueness_suffix}.png"))
}

fn persist_generated_image(
    store: &ArtifactStore,
    filename: &str,
    content_base64: &str,
    width: u32,
    height: u32,
) -> Result<String, String> {
    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f").to_string();
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let suggested_name = generated_image_filename(filename, &timestamp, &unique[..8])?;
    let bytes = crate::artifact_commands::decode_bounded(content_base64, IMAGE_MAX_BYTES, "image")?;
    if !bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Err("Generated image content is not a PNG (bad magic number)".to_string());
    }
    let blob = store.put(&bytes).map_err(|error| error.to_string())?;
    serde_json::to_string(&GeneratedImageReceipt {
        artifact_id: blob.id,
        media_type: "image/png",
        width,
        height,
        size: blob.size,
        suggested_name,
    })
    .map_err(|error| format!("Failed to serialize generated image receipt: {error}"))
}

/// Persist a frontend-rasterized PNG in the app's private durable artifact
/// store — the Rust half of the `generate_image` model tool. The model only
/// supplies SVG markup and a suggested download filename; rasterization to
/// PNG happens in the webview (`imageGeneration.ts`, where a canvas exists).
///
/// This is deliberately not a workspace mutation: it resolves no workspace
/// path, requests no edit permission, and creates no checkpoint. The user
/// chooses a filesystem destination later via the card's Download action.
/// The PNG magic-number check is defense-in-depth against a spoofed IPC call.
#[tauri::command(rename_all = "snake_case")]
pub async fn tool_generate_image(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    filename: String,
    content_base64: String,
    width: u32,
    height: u32,
) -> Result<String, String> {
    let store = artifact_commands::store_for(&app, state.inner())?;
    persist_generated_image(&store, &filename, &content_base64, width, height)
}

/// A workspace image file's bytes and media type, for inline display in the
/// chat transcript. New generated images use the app-owned durable artifact
/// store above; this stays for ordinary workspace images and legacy turns.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceImage {
    pub mime: String,
    pub content_base64: String,
    pub size: u64,
}

/// Read an image file from the workspace as base64, for the chat's inline
/// image previews (`WorkspaceImagePreview.tsx`), legacy generated-image tool
/// rows, and workspace-relative `![...](x.png)` references in assistant
/// Markdown. Read-only and sandboxed through `resolve_path_and_root` like
/// [`tool_read_file`], so intentionally NOT permission-gated; NOT a model
/// tool (no `tool_` prefix — the model never calls this, only the UI does).
/// Refuses non-image extensions and files over [`IMAGE_MAX_BYTES`].
#[tauri::command(rename_all = "snake_case")]
pub async fn workspace_read_image(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<WorkspaceImage, String> {
    use base64::Engine;
    let (resolved, _root) = workspace::resolve_path_and_root(state.inner(), &path)?;
    let mime = image_mime_for_path(&resolved).ok_or_else(|| {
        format!("'{path}' is not a previewable image (png, jpg, gif, webp, bmp, svg)")
    })?;

    let metadata =
        std::fs::metadata(&resolved).map_err(|e| format!("Failed to stat '{}': {}", path, e))?;
    if !metadata.is_file() {
        return Err(format!("'{path}' is not a regular file"));
    }
    if metadata.len() > IMAGE_MAX_BYTES {
        return Err(format!(
            "'{path}' is {} bytes, exceeding the {IMAGE_MAX_BYTES}-byte preview limit",
            metadata.len()
        ));
    }

    let bytes =
        std::fs::read(&resolved).map_err(|e| format!("Failed to read '{}': {}", path, e))?;
    Ok(WorkspaceImage {
        mime: mime.to_string(),
        content_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
    })
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
        preview.push(format!(
            "  … ({} more removed lines)",
            old_lines.len() - MAX_PREVIEW_LINES
        ));
    }

    let new_lines: Vec<&str> = new_string.lines().collect();
    for line in new_lines.iter().take(MAX_PREVIEW_LINES) {
        preview.push(format!("+ {}", truncate(line)));
    }
    if new_lines.len() > MAX_PREVIEW_LINES {
        preview.push(format!(
            "  … ({} more added lines)",
            new_lines.len() - MAX_PREVIEW_LINES
        ));
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
    tool_call_id: Option<String>,
    risk_level: Option<String>,
    risk_reason: Option<String>,
    agent_label: Option<String>,
    workspace_root_override: Option<String>,
) -> Result<String, String> {
    if old_string.is_empty() {
        return Err("old_string must not be empty".to_string());
    }

    // Same before-the-prompt override point as `tool_write_file` — see its
    // comment.
    let (resolved, root) =
        crate::agent_worktrees::resolve_with_override(state.inner(), &path, workspace_root_override.as_deref())?;

    if !resolved.is_file() {
        return Err(format!("'{}' is not a file", path));
    }

    let current = std::fs::read_to_string(&resolved)
        .map_err(|e| format!("Failed to read '{}': {}", path, e))?;

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

    permissions::request_permission(
        &app,
        state.inner(),
        "edit_file",
        detail,
        turn_id.as_deref(),
        tool_call_id.as_deref(),
        risk,
        agent_label.as_deref(),
    )
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
    let fresh = std::fs::read_to_string(&resolved)
        .map_err(|e| format!("Failed to read '{}': {}", path, e))?;
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
    std::fs::write(&resolved, &updated)
        .map_err(|e| format!("Failed to write '{}': {}", path, e))?;

    Ok(format!("Edited {}", path))
}

/// The foreground shells one turn currently owns, plus whether that turn is
/// suspended — the value type of `AppState::shell_process_groups`.
///
/// A turn's cooperative pause only lands at its loop's next safe point, which
/// for a long `run_shell` can be many minutes away. Tracking the children's
/// process groups here lets `process_commands::process_signal` SIGSTOP them
/// the moment a suspend is signalled, so a paused turn actually stops
/// consuming the machine instead of merely promising to.
#[derive(Default)]
pub struct TurnShellGroups {
    /// Process-group ids of this turn's live foreground shells. On unix, a
    /// group id equals the pid of the child that leads it (`process_group(0)`
    /// at spawn); on other platforms the pid is signalled directly.
    pub groups: std::collections::HashSet<u32>,
    /// Whether the owning process record currently has `suspend_requested`
    /// latched. Kept here rather than re-read from the ledger so the timeout
    /// below never touches SQLite on its polling path, and so a resume can
    /// still find the entry after the last shell has been deregistered.
    pub suspended: bool,
}

/// Adds `pgid` to `turn_key`'s live set, returning whether the caller should
/// deregister it later. If the turn is *already* suspended when the child
/// registers, the child is stopped immediately — otherwise a command spawned
/// in the same tool round the pause landed in would keep running.
fn register_shell_process_group(state: &AppState, turn_key: &str, pgid: u32) -> bool {
    let Ok(mut guard) = state.shell_process_groups.lock() else {
        return false;
    };
    let entry = guard.entry(turn_key.to_string()).or_default();
    entry.groups.insert(pgid);
    if entry.suspended {
        let _ = crate::os_signal::suspend_process_group(pgid);
    }
    true
}

/// Removes `pgid` from `turn_key`'s live set, dropping the whole entry once
/// nothing is left to signal. The entry is kept while `suspended` is still
/// latched so a later resume has something to clear.
fn forget_shell_process_group(state: &AppState, turn_key: &str, pgid: u32) {
    let Ok(mut guard) = state.shell_process_groups.lock() else {
        return;
    };
    let Some(entry) = guard.get_mut(turn_key) else {
        return;
    };
    entry.groups.remove(&pgid);
    if entry.groups.is_empty() && !entry.suspended {
        guard.remove(turn_key);
    }
}

/// Resolves once `SHELL_TIMEOUT` of *unsuspended* wall time has passed for
/// `turn_key`.
///
/// A suspended command is SIGSTOPped: it cannot make progress, so counting
/// that time against its timeout would turn "pause this" into "kill this in
/// two minutes". Time only accrues while the turn is running, so a pause is
/// genuinely a pause. Polling (rather than waiting on a notify) is deliberate
/// — this future is only ever raced against the child's own exit inside a
/// `select!`, so a slice of granularity costs nothing and keeps the suspend
/// path lock-free of async machinery.
async fn shell_timeout_elapsed(state: &AppState, turn_key: &str) {
    const SLICE: Duration = Duration::from_millis(250);
    let mut remaining = SHELL_TIMEOUT;
    loop {
        tokio::time::sleep(SLICE).await;
        let suspended = state
            .shell_process_groups
            .lock()
            .map(|guard| guard.get(turn_key).is_some_and(|entry| entry.suspended))
            .unwrap_or(false);
        if suspended {
            continue;
        }
        remaining = remaining.saturating_sub(SLICE);
        if remaining.is_zero() {
            return;
        }
    }
}

/// Delivers a suspend/resume to every foreground shell `turn_key` owns and
/// records the new intent, so a command spawned moments later inherits it.
///
/// Returns the number of process groups actually signalled — zero is a normal
/// outcome (the turn may simply not be running a shell right now), not an
/// error, which is why the caller treats this as best-effort.
pub fn signal_turn_shells(state: &AppState, turn_key: &str, suspend: bool) -> usize {
    let Ok(mut guard) = state.shell_process_groups.lock() else {
        return 0;
    };
    if !suspend && !guard.contains_key(turn_key) {
        return 0;
    }
    let entry = guard.entry(turn_key.to_string()).or_default();
    entry.suspended = suspend;
    let mut delivered = 0;
    for pgid in &entry.groups {
        let result = if suspend {
            crate::os_signal::suspend_process_group(*pgid)
        } else {
            crate::os_signal::resume_process_group(*pgid)
        };
        if result.is_ok() {
            delivered += 1;
        }
    }
    // A resumed turn with nothing left running has no state worth keeping —
    // the child may have been reaped while the entry was pinned by `suspended`.
    if !suspend && entry.groups.is_empty() {
        guard.remove(turn_key);
    }
    delivered
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
    tool_call_id: Option<String>,
    risk_level: Option<String>,
    risk_reason: Option<String>,
    agent_label: Option<String>,
    full_output: Option<bool>,
    workspace_root_override: Option<String>,
) -> Result<serde_json::Value, String> {
    let risk = permissions::compute_risk(None, risk_level, risk_reason);
    permissions::request_permission(
        &app,
        state.inner(),
        "run_shell",
        command.clone(),
        turn_id.as_deref(),
        tool_call_id.as_deref(),
        risk,
        agent_label.as_deref(),
    )
    .await?;

    checkpoints::record_shell(state.inner(), checkpoint_id.as_deref())?;

    // Under a worktree override the DEFAULT cwd is the worktree itself, and
    // an explicit `cwd` is sandboxed inside it — a worktree-isolated agent's
    // commands must never silently run in the shared checkout.
    let (cwd_path, workspace_root) = match cwd {
        Some(ref c) => crate::agent_worktrees::resolve_with_override(
            state.inner(),
            c,
            workspace_root_override.as_deref(),
        )?,
        None => match workspace_root_override.as_deref() {
            Some(root) => {
                crate::agent_worktrees::resolve_with_override(state.inner(), ".", Some(root))?
            }
            None => {
                let root = workspace::primary_root_canon(state.inner())?;
                (root.clone(), root)
            }
        },
    };

    // Decided before the spawn, because it is now the *read* that is bounded
    // rather than the returned string: the drain below needs to know its ceiling
    // before the first byte arrives.
    let cap = stream_cap(full_output);

    let mut child = crate::workspace_shell::spawn_foreground(&workspace_root, &cwd_path, &command)
        .map_err(|e| format!("Failed to spawn shell: {}", e))?;
    // Captured before the capture future borrows the child. With
    // `process_group(0)` above, the child's own pid is also its group id.
    let child_pgid = child.id();
    // Taken out of the child *before* the wait, so the two pipes can be drained
    // concurrently with it. `Stdio::piped()` above guarantees both are `Some`;
    // the `ok_or` is a refusal rather than an `expect` because a panic inside a
    // Tauri command aborts the whole IPC task.
    let stdout_pipe = child
        .take_stdout()
        .ok_or_else(|| "Shell child had no stdout pipe".to_string())?;
    let stderr_pipe = child
        .take_stderr()
        .ok_or_else(|| "Shell child had no stderr pipe".to_string())?;

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

    // Registered for the whole life of the child so a `process_signal
    // <chat_turn> suspend` arriving mid-command can SIGSTOP it immediately,
    // rather than the pause only landing whenever this command happens to
    // finish. Deregistered right after the wait below, before the pid could
    // be recycled by the kernel.
    let registered_pgid = child_pgid.and_then(|pgid| {
        register_shell_process_group(state.inner(), &cancel_key, pgid).then_some(pgid)
    });

    let outcome = tokio::select! {
        result = capture_bounded(child.wait(), stdout_pipe, stderr_pipe, cap) => {
            result.map_err(|e| format!("Failed to run command: {}", e))
        }
        _ = cancel.notified() => {
            Err("Command cancelled by the user".to_string())
        }
        _ = shell_timeout_elapsed(state.inner(), &cancel_key) => {
            Err(format!(
                "Command timed out after {} seconds",
                SHELL_TIMEOUT.as_secs()
            ))
        }
    };

    // A timeout or a Stop must end the whole tree, not the shell we spawned.
    //
    // `kill_on_drop` (set above) SIGKILLs exactly one pid, so `sh -c "cargo
    // build"` reaped the shell and left the compiler running — consuming the
    // machine long after the tool reported "timed out after 120 seconds". The pgid
    // was already known here and used for suspend/resume; it simply was not used
    // for the kill. TERM first, so a build can flush and clean up its temp files.
    if outcome.is_err() {
        child.terminate_tree();
    }

    if let Some(pgid) = registered_pgid {
        forget_shell_process_group(state.inner(), &cancel_key, pgid);
    }

    // Drop this turn's channel once no other shell of the same turn still
    // holds it (strong count 2 = the map's Arc + our clone), so the map
    // doesn't accumulate one entry per turn forever. A racing new shell for
    // the same turn simply recreates the entry.
    {
        let mut guard = state
            .tool_cancel
            .lock()
            .map_err(|_| "Tool-cancel lock poisoned".to_string())?;
        if guard
            .get(&cancel_key)
            .is_some_and(|n| std::sync::Arc::strong_count(n) <= 2)
        {
            guard.remove(&cancel_key);
        }
    }

    let output = outcome?;
    // The command was watched to exit. A timeout or a Stop leaves only the
    // declaration above, which is the right way round: a killed `sh -c` had
    // already run for as long as it ran, and the preview says "may have" rather
    // than reporting nothing.
    checkpoints::commit_external_effect(
        state.inner(),
        checkpoint_id.as_deref(),
        checkpoints::ExternalEffectKind::Shell,
    )?;
    // Both streams were unbounded from the child's pipes all the way into the
    // model's context. Of the app's command-running paths this was the only
    // uncapped one, and the only one whose output a model reads directly:
    // `verify.rs` and `background_shell.rs` have always capped theirs.
    //
    // `full_output` exists for callers that parse the result as a *document*
    // rather than showing it to a model. `securityAutofix.ts` is the case that
    // forced it: it runs `pnpm audit --json` and `JSON.parse`s stdout, so a
    // truncated tail is not a shorter answer but an unparseable one — and its
    // failure mode is reporting zero vulnerabilities, a silent false negative in
    // a security tool. That output never reaches a model; only the parsed
    // findings do.
    //
    // Deliberately a flag and not a byte count: nothing needs a *different*
    // ceiling, and `Some(0)` meaning "unlimited" would be exactly the
    // zero-versus-absent overloading this codebase avoids elsewhere.
    //
    // The cap was applied *while reading* (see [`capture_bounded`]); by here both
    // strings are already at or under it, and this is only the decode.
    let (stdout, stdout_truncated) = output.stdout.into_string();
    let (stderr, stderr_truncated) = output.stderr.into_string();
    Ok(serde_json::json!({
        "stdout": stdout,
        "stderr": stderr,
        "code": output.status.code(),
        // Additive, so existing readers are unaffected. Reported separately from
        // the in-text marker because a command is free to print that text itself,
        // and a caller deciding whether it holds a whole document needs an answer
        // it does not have to pattern-match prose for.
        "stdoutTruncated": stdout_truncated,
        "stderrTruncated": stderr_truncated,
    }))
}

/// The ceiling one `run_shell` call's streams are held to.
///
/// Split out so the *default* is assertable: the whole point is that a caller who
/// says nothing gets the cap, since every model-initiated call says nothing. An
/// inverted default would be invisible in a passing shell command and would only
/// show up as a flooded context window.
fn stream_cap(full_output: Option<bool>) -> Option<usize> {
    if full_output.unwrap_or(false) {
        None
    } else {
        Some(crate::output_cap::MODEL_OUTPUT_CAP)
    }
}

/// One captured stream, already held to its ceiling.
///
/// Bytes rather than a `String` because the cap is enforced during the read, when
/// a chunk boundary can land inside a multi-byte character and there is no whole
/// string to decode yet. [`Self::into_string`] does the one decode, at the end,
/// over a buffer that is bounded by construction.
#[derive(Debug, Default)]
struct CappedStream {
    /// The kept tail. At most `cap` bytes once a cap is in force.
    bytes: Vec<u8>,
    /// Whether anything was dropped from the front to stay inside the cap.
    truncated: bool,
}

impl CappedStream {
    /// Appends `chunk`, dropping whole bytes off the *front* once `cap` is
    /// exceeded.
    ///
    /// Tail, not head, for the reason [`crate::output_cap::cap_tail`] gives: a
    /// failing command prints its diagnostic last. Front-dropping is what makes
    /// this bounded in the first place — a head-keeping cap could stop reading,
    /// but then the child blocks forever on a full pipe instead of running to
    /// completion, which turns a noisy command into a timeout.
    fn push(&mut self, chunk: &[u8], cap: Option<usize>) {
        self.bytes.extend_from_slice(chunk);
        let Some(cap) = cap else { return };
        if self.bytes.len() <= cap {
            return;
        }
        let overflow = self.bytes.len() - cap;
        self.bytes.drain(..overflow);
        self.truncated = true;
    }

    /// Decodes the kept tail, prefixing the shared truncation marker if the front
    /// was dropped.
    ///
    /// The leading continuation bytes are shed first, so a cut that landed inside
    /// a character widens the kept region forward to the next boundary instead of
    /// decoding to a replacement character. That is exactly what `cap_tail` did
    /// when it worked on a whole `String`, and keeping the behaviour identical is
    /// why it is done here rather than left to `from_utf8_lossy`.
    fn into_string(mut self) -> (String, bool) {
        if self.truncated {
            let keep = self
                .bytes
                .iter()
                .position(|byte| (byte & 0b1100_0000) != 0b1000_0000)
                .unwrap_or(self.bytes.len());
            self.bytes.drain(..keep);
        }
        let decoded = String::from_utf8_lossy(&self.bytes).to_string();
        if self.truncated {
            (
                format!("{}{decoded}", crate::output_cap::TRUNCATION_MARKER),
                true,
            )
        } else {
            (decoded, false)
        }
    }
}

/// What a finished `run_shell` child produced, with both streams already bounded.
struct ShellCapture {
    status: std::process::ExitStatus,
    stdout: CappedStream,
    stderr: CappedStream,
}

/// Reads one pipe to EOF, keeping at most `cap` bytes of its tail.
///
/// Reading to EOF rather than stopping at the cap is the whole point: an
/// early-returning reader leaves the child blocked on a full pipe buffer, so a
/// command that merely printed too much would report a timeout instead of its
/// exit code.
async fn drain_capped<R>(mut reader: R, cap: Option<usize>) -> std::io::Result<CappedStream>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut stream = CappedStream::default();
    // 8 KiB: larger than the 64 KiB pipe buffer would not help (the kernel hands
    // over what it has), and smaller would just mean more syscalls.
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok(stream);
        }
        stream.push(&chunk[..read], cap);
    }
}

/// Waits for `child` while draining both of its pipes, holding each to `cap`.
///
/// This is the fix for the last unbounded buffer on the shell path.
/// `wait_with_output` materialized *both* streams in full before any cap applied,
/// so `sh -c 'cat /dev/urandom | base64'` had the whole 120-second timeout to grow
/// the app's own heap — the returned string was capped, the heap behind it was
/// not.
///
/// The three futures are joined rather than sequenced, and that is the part with a
/// failure mode: reading stdout to EOF *before* touching stderr deadlocks a child
/// with a chatty stderr as soon as the 64 KiB stderr pipe buffer fills, and
/// waiting for exit before reading either deadlocks on the first full pipe.
/// `try_join!` polls all three, which is the service `wait_with_output` performed
/// and the reason it could not simply be dropped.
///
/// Cancellation safety is inherited rather than argued: the caller races this
/// whole future in a `tokio::select!`, and dropping it drops the two pipe readers
/// (closing the read ends) and stops the wait. The child itself is killed by the
/// caller's process-group terminate, with `kill_on_drop` as the backstop — the
/// same two mechanisms as before this change.
async fn capture_bounded<W, O, E>(
    wait: W,
    stdout_pipe: O,
    stderr_pipe: E,
    cap: Option<usize>,
) -> std::io::Result<ShellCapture>
where
    W: std::future::Future<Output = std::io::Result<std::process::ExitStatus>>,
    O: tokio::io::AsyncRead + Unpin,
    E: tokio::io::AsyncRead + Unpin,
{
    let (status, stdout, stderr) = tokio::try_join!(
        wait,
        drain_capped(stdout_pipe, cap),
        drain_capped(stderr_pipe, cap),
    )?;
    Ok(ShellCapture {
        status,
        stdout,
        stderr,
    })
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
/// `checkpoint_id` was deliberately NOT accepted here, on the grounds that a
/// remembered fact isn't a workspace file so there is nothing for a per-turn
/// checkpoint to snapshot or revert. The first half of that is still true and
/// the conclusion no longer follows: the checkpoint does not snapshot this, but
/// it does now *enumerate* it, because "nothing here can undo this" is the fact
/// a rollback needs and the reason `ExternalEffectKind` exists. So the id is
/// accepted and used for exactly one thing — recording that a memory write
/// happened — and never to snapshot anything. `turn_id` is injected by
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
    tool_call_id: Option<String>,
    checkpoint_id: Option<String>,
) -> Result<memory::Fact, String> {
    permissions::request_permission(
        &app,
        state.inner(),
        "remember",
        text.clone(),
        turn_id.as_deref(),
        tool_call_id.as_deref(),
        None,
        None,
    )
    .await?;

    // Recorded, never snapshotted — see this command's doc comment. A memory
    // write is outside the checkpointed workspace, so a revert cannot undo it
    // and the enumerated set is where that is said.
    checkpoints::record_external_effect(
        state.inner(),
        checkpoint_id.as_deref(),
        checkpoints::ExternalEffectKind::Memory,
    )?;

    let root = workspace::primary_root_canon(state.inner())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| memory::GLOBAL_SCOPE_KEY.to_string());
    let path = memory::memories_file_path(&app)?;

    // Serialized against concurrent split-pane `tool_remember` calls (and
    // against `memory_add`/`memory_delete`) via `AppState::memory_lock` — the
    // whole `memories.json` file is rewritten on every add, so two
    // unsynchronized concurrent writers could otherwise silently drop one
    // fact's write.
    let _lock = state
        .memory_lock
        .lock()
        .map_err(|_| "Memory lock poisoned".to_string())?;
    let fact = memory::add_fact_impl(&path, &root, &text, "agent", turn_id.as_deref())?;
    // Recorded *after* the add, with the id the store actually assigned — an id
    // guessed before the write could name a fact that was never created, and
    // `add_fact_impl` returns the pre-existing fact when the text is a duplicate.
    checkpoints::record_remembered_fact(
        state.inner(),
        checkpoint_id.as_deref(),
        checkpoints::RememberedFact {
            id: fact.id.clone(),
            text: fact.text.clone(),
        },
    )?;
    // The write returned, so the fact is in the store. This is the one kind
    // whose commit is also its compensator's precondition: `remembered_facts`
    // is what revert deletes, and both are filled by the same success path.
    checkpoints::commit_external_effect(
        state.inner(),
        checkpoint_id.as_deref(),
        checkpoints::ExternalEffectKind::Memory,
    )?;
    Ok(fact)
}

/// Read one bundled file from an installed native skill's folder — the
/// progressive-disclosure counterpart to a skill's `resource_files` listing
/// (see `native_skills.rs`'s `SkillDescriptor.resource_files` and
/// `NativeSkillManager::read_resource`): the model reads a specific bundled
/// reference/script only once it actually needs it, instead of the whole
/// bundle being loaded up front. Read-only, so no `permissions::request_permission`
/// gate — same posture as `tool_read_file`.
#[tauri::command]
pub async fn tool_read_skill_resource(
    app: tauri::State<'_, AppState>,
    native: tauri::State<'_, native_skill_commands::NativeSkillsCommandState>,
    command: String,
    path: String,
) -> Result<String, String> {
    let workspace = native_skill_commands::optional_primary_workspace(&app)?;
    let manager = native.manager.clone();
    native_skill_commands::run_blocking(move || {
        manager.read_resource(&command, &path, workspace.as_deref())
    })
    .await
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
pub fn list_workspace_paths(
    state: tauri::State<'_, AppState>,
) -> Result<WorkspacePathsResult, String> {
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

            let relative_str = display_relative_path(relative);

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
/// Pairing a turn's cooperative pause with a real SIGSTOP of the shell
/// children it owns — the half of the pause that is not cooperative and
/// therefore has to be proven against the actual OS, not a fake.
#[cfg(all(test, unix))]
mod shell_suspend_tests {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    fn process_state(pid: u32) -> String {
        let output = Command::new("ps")
            .args(["-o", "state=", "-p", &pid.to_string()])
            .output()
            .expect("ps runs");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Whether `ps` reports the process as stopped.
    ///
    /// Only the FIRST character is the state; BSD `ps` appends flag characters
    /// after it (`<` raised priority, `+` foreground group, `s` session leader),
    /// so a stopped process can read as `T<` rather than `T` depending on how
    /// the host schedules it. Comparing the whole field passes locally and fails
    /// on a CI runner, which is exactly what it did.
    fn is_stopped(pid: u32) -> bool {
        process_state(pid).starts_with('T')
    }

    /// A real child in its own process group, registered under `turn` exactly
    /// as `tool_run_shell` registers one.
    ///
    /// Two details here are load-bearing, and both were learned from a CI
    /// hang rather than guessed:
    ///
    /// 1. **Every stdio handle is `null`.** A child that inherits the test
    ///    harness's stderr keeps that pipe open for as long as it lives, and
    ///    `cargo test` does not finish until the pipe reaches EOF.
    /// 2. **Teardown runs on drop, not at the end of the test body.** A failed
    ///    assertion panics and unwinds straight past any explicit `kill`. Leak
    ///    a *running* child and the pipe closes when it exits 30s later; leak a
    ///    *stopped* one and it never exits, never closes the pipe, and the job
    ///    hangs until its timeout. That is precisely what happened: 48 minutes
    ///    on Linux for an assertion that failed in milliseconds on macOS.
    struct TestShell {
        child: std::process::Child,
        pgid: u32,
    }

    impl TestShell {
        fn spawn(state: &AppState, turn: &str) -> Self {
            use std::os::unix::process::CommandExt;
            let mut command = Command::new("sleep");
            command
                .arg("30")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            command.process_group(0);
            let child = command.spawn().expect("sleep spawns");
            let pgid = child.id();
            assert!(register_shell_process_group(state, turn, pgid));
            Self { child, pgid }
        }
    }

    impl Drop for TestShell {
        fn drop(&mut self) {
            // Resumed before killing. SIGKILL does terminate a stopped process,
            // but this is the one path where being wrong about that costs a
            // hung CI job rather than a failed assertion.
            let _ = crate::os_signal::resume_process_group(self.pgid);
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    fn suspending_a_turn_stops_its_foreground_shell_and_resuming_starts_it_again() {
        let state = AppState::default();
        let shell = TestShell::spawn(&state, "turn-1");
        let pgid = shell.pgid;
        std::thread::sleep(Duration::from_millis(100));
        assert!(!is_stopped(pgid), "child should start out running");

        assert_eq!(signal_turn_shells(&state, "turn-1", true), 1);
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            is_stopped(pgid),
            "a suspended turn's shell must actually be stopped, not just latched; ps said {}",
            process_state(pgid)
        );

        assert_eq!(signal_turn_shells(&state, "turn-1", false), 1);
        std::thread::sleep(Duration::from_millis(100));
        assert!(!is_stopped(pgid), "resume must restart the child");

        forget_shell_process_group(&state, "turn-1", pgid);
    }

    #[test]
    fn a_shell_spawned_into_an_already_suspended_turn_is_stopped_on_registration() {
        // The race the `suspended` flag exists for: the pause is signalled
        // while the loop is mid-tool-round, and the command starts after.
        let state = AppState::default();
        assert_eq!(signal_turn_shells(&state, "turn-2", true), 0);

        let shell = TestShell::spawn(&state, "turn-2");
        let pgid = shell.pgid;
        std::thread::sleep(Duration::from_millis(100));
        assert!(is_stopped(pgid), "ps said {}", process_state(pgid));

        signal_turn_shells(&state, "turn-2", false);
        std::thread::sleep(Duration::from_millis(100));
        assert!(!is_stopped(pgid));

        forget_shell_process_group(&state, "turn-2", pgid);
    }

    #[test]
    fn one_turns_pause_never_touches_another_turns_shell() {
        let state = AppState::default();
        let paused = TestShell::spawn(&state, "turn-a");
        let running = TestShell::spawn(&state, "turn-b");
        let (paused_pgid, running_pgid) = (paused.pgid, running.pgid);
        std::thread::sleep(Duration::from_millis(100));

        signal_turn_shells(&state, "turn-a", true);
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            is_stopped(paused_pgid),
            "ps said {}",
            process_state(paused_pgid)
        );
        assert!(
            !is_stopped(running_pgid),
            "the other pane's command must keep running"
        );

        signal_turn_shells(&state, "turn-a", false);
        forget_shell_process_group(&state, "turn-a", paused_pgid);
        forget_shell_process_group(&state, "turn-b", running_pgid);
    }

    #[test]
    fn deregistering_the_last_shell_of_a_running_turn_drops_the_entry() {
        // A pid is never signalled after its child is reaped — the kernel is
        // free to reuse it, and a stale entry would eventually SIGSTOP an
        // unrelated process.
        let state = AppState::default();
        let pgid = {
            let shell = TestShell::spawn(&state, "turn-3");
            shell.pgid
        };

        forget_shell_process_group(&state, "turn-3", pgid);
        assert!(!state
            .shell_process_groups
            .lock()
            .expect("lock")
            .contains_key("turn-3"));
        assert_eq!(signal_turn_shells(&state, "turn-3", false), 0);
    }

    #[test]
    fn a_suspended_turn_keeps_its_entry_until_resumed() {
        // The entry has to outlive its last shell while suspended, so a resume
        // that arrives after the child was reaped still clears the flag rather
        // than leaving the next command to be stopped on arrival.
        let state = AppState::default();
        let pgid = {
            let shell = TestShell::spawn(&state, "turn-4");
            signal_turn_shells(&state, "turn-4", true);
            shell.pgid
        };
        forget_shell_process_group(&state, "turn-4", pgid);

        assert!(state
            .shell_process_groups
            .lock()
            .expect("lock")
            .contains_key("turn-4"));
        signal_turn_shells(&state, "turn-4", false);
        assert!(!state
            .shell_process_groups
            .lock()
            .expect("lock")
            .contains_key("turn-4"));
    }
}

#[cfg(test)]
mod tests {
    /// The shell tool was the only command-running path in this app with no
    /// output bound, and the only one whose output a model reads directly.
    ///
    /// Asserted on `stream_cap` rather than on `tool_run_shell`, which needs a
    /// `tauri::State` and an `AppHandle` — the same reason `verify.rs` tests its
    /// `run_command_impl` core instead of its command wrapper.
    #[test]
    fn a_caller_that_says_nothing_gets_the_model_output_cap() {
        use crate::output_cap::MODEL_OUTPUT_CAP;

        // Every model-initiated call is this case: the tool schema shown to models
        // sets `additionalProperties: false` and does not list the flag, so a model
        // cannot ask for the uncapped form.
        assert_eq!(super::stream_cap(None), Some(MODEL_OUTPUT_CAP));
        assert_eq!(super::stream_cap(Some(false)), Some(MODEL_OUTPUT_CAP));

        // The opt-out exists for callers that parse the result as a document.
        // `securityAutofix.ts` runs `pnpm audit --json` and `JSON.parse`s stdout,
        // where a truncated tail is unparseable rather than merely shorter, and the
        // parse failure would read as zero vulnerabilities.
        assert_eq!(super::stream_cap(Some(true)), None);
    }

    /// Fed one byte at a time, which is the case the old whole-buffer cap never
    /// had to survive: the cap now has to hold across arbitrary chunk boundaries
    /// rather than being applied once to a finished string.
    #[test]
    fn a_capped_stream_keeps_its_tail_and_reports_that_it_was_cut() {
        let feed = |cap: Option<usize>| {
            let mut stream = super::CappedStream::default();
            for byte in b"0123456789" {
                stream.push(&[*byte], cap);
            }
            stream.into_string()
        };

        let (value, truncated) = feed(Some(4));
        assert!(truncated, "a cut stream must say so on the wire");
        assert!(
            value.ends_with("6789"),
            "the tail is what a failing command puts its diagnostic in, got {value:?}"
        );

        let (value, truncated) = feed(Some(64));
        assert_eq!(value, "0123456789");
        assert!(!truncated, "output inside the cap is not truncated");

        let (value, truncated) = feed(None);
        assert_eq!(value, "0123456789");
        assert!(
            !truncated,
            "an uncapped stream is whole, so nothing was dropped"
        );
    }

    /// The bound is on the *buffer*, not just on the returned string — that
    /// distinction is the whole reason this slice exists, so it is asserted
    /// directly rather than inferred from the output.
    #[test]
    fn the_kept_buffer_never_grows_past_the_cap_however_much_arrives() {
        const CAP: usize = 1_024;
        let mut stream = super::CappedStream::default();
        // 4 MiB through a 1 KiB ceiling, in chunks that do not divide it.
        for _ in 0..4_096 {
            stream.push(&[b'x'; 1_000], Some(CAP));
            assert!(
                stream.bytes.len() <= CAP,
                "the buffer grew to {} bytes past a {CAP}-byte cap",
                stream.bytes.len()
            );
        }
        let (value, truncated) = stream.into_string();
        assert!(truncated);
        assert!(value.len() <= CAP + super::super::output_cap::TRUNCATION_MARKER.len());
    }

    /// Command output is bytes, not text, and a stream cut mid-codepoint by the
    /// child itself must not lose the rest of the output.
    #[test]
    fn invalid_utf8_in_a_stream_is_replaced_rather_than_dropped() {
        let mut stream = super::CappedStream::default();
        stream.push(&[b'o', b'k', 0xff, b'!'], Some(64));
        let (value, truncated) = stream.into_string();
        assert!(!truncated);
        assert!(
            value.starts_with("ok") && value.ends_with('!'),
            "the surrounding bytes must survive a lossy decode, got {value:?}"
        );
    }

    /// A front-drop that lands inside a character widens forward to the next
    /// boundary, exactly as the whole-string `cap_tail` did. Getting this wrong
    /// shows up as a stray replacement character at the head of every truncated
    /// stream, which is small enough to ship unnoticed.
    #[test]
    fn a_front_drop_inside_a_character_widens_to_the_next_boundary() {
        // Four bytes each, so a cap of 5 cannot fall on a boundary.
        let mut stream = super::CappedStream::default();
        stream.push("🙈🙉🙊".as_bytes(), Some(5));
        let (value, truncated) = stream.into_string();
        assert!(truncated);
        assert_eq!(
            value,
            format!("{}🙊", super::super::output_cap::TRUNCATION_MARKER),
            "a split codepoint must be dropped whole, not decoded to U+FFFD"
        );
    }

    /// The deadlock this change had to avoid, run for real.
    ///
    /// A child that fills stderr while stdout is still open wedges any reader
    /// that drains one pipe to EOF before touching the other: the 64 KiB stderr
    /// pipe buffer fills, the child blocks writing to it, and stdout never sees
    /// EOF. Both streams here are far past that buffer, and both are past the
    /// cap, so this also proves the bounded read still lets the child *finish*
    /// rather than stopping early and hanging it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_child_that_floods_both_pipes_still_exits_and_is_held_to_the_cap() {
        use std::process::Stdio;

        const CAP: usize = 4_096;

        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            // ~200 KiB down each pipe, interleaved, so neither can be drained
            // to completion before the other is read.
            .arg(
                "i=0; while [ $i -lt 200 ]; do \
                   printf 'o%.0s' $(seq 1 1024); \
                   printf 'e%.0s' $(seq 1 1024) >&2; \
                   i=$((i+1)); \
                 done; exit 3",
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the flooding child");

        let stdout_pipe = child.stdout.take().expect("stdout pipe");
        let stderr_pipe = child.stderr.take().expect("stderr pipe");

        let captured = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            super::capture_bounded(child.wait(), stdout_pipe, stderr_pipe, Some(CAP)),
        )
        .await
        .expect("draining both pipes concurrently must not deadlock")
        .expect("the child ran to completion");

        assert_eq!(
            captured.status.code(),
            Some(3),
            "the child's own exit code must survive a truncated capture"
        );
        assert!(captured.stdout.bytes.len() <= CAP);
        assert!(captured.stderr.bytes.len() <= CAP);

        let (stdout, stdout_truncated) = captured.stdout.into_string();
        let (stderr, stderr_truncated) = captured.stderr.into_string();
        assert!(stdout_truncated && stderr_truncated);
        assert!(stdout.ends_with('o'), "the tail is what is kept");
        assert!(stderr.ends_with('e'), "the tail is what is kept");
    }

    /// The uncapped path still returns the whole document, because
    /// `securityAutofix.ts` `JSON.parse`s it and a truncated tail there reads as
    /// zero vulnerabilities.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_uncapped_path_still_returns_the_whole_stream() {
        use std::process::Stdio;

        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("printf 'x%.0s' $(seq 1 100000)")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        let stdout_pipe = child.stdout.take().expect("stdout pipe");
        let stderr_pipe = child.stderr.take().expect("stderr pipe");

        let captured = super::capture_bounded(child.wait(), stdout_pipe, stderr_pipe, None)
            .await
            .expect("the child ran");
        let (stdout, truncated) = captured.stdout.into_string();
        assert_eq!(stdout.len(), 100_000);
        assert!(!truncated);
    }

    use super::*;

    #[test]
    fn image_mime_covers_preview_formats_case_insensitively_and_rejects_the_rest() {
        use std::path::Path;
        assert_eq!(
            image_mime_for_path(Path::new("a/chart.png")),
            Some("image/png")
        );
        assert_eq!(
            image_mime_for_path(Path::new("a/PHOTO.JPG")),
            Some("image/jpeg")
        );
        assert_eq!(image_mime_for_path(Path::new("x.jpeg")), Some("image/jpeg"));
        assert_eq!(image_mime_for_path(Path::new("x.webp")), Some("image/webp"));
        assert_eq!(
            image_mime_for_path(Path::new("x.svg")),
            Some("image/svg+xml")
        );
        assert_eq!(image_mime_for_path(Path::new("x.ts")), None);
        assert_eq!(image_mime_for_path(Path::new("no-extension")), None);
    }

    #[test]
    fn image_base64_decode_roundtrips_and_enforces_both_bounds() {
        assert_eq!(
            crate::artifact_commands::decode_bounded("aGVsbG8=", 5, "image").unwrap(),
            b"hello"
        );
        // Decoded size over the cap.
        assert!(crate::artifact_commands::decode_bounded("aGVsbG8=", 4, "image").is_err());
        // Encoded length rejected before any decode allocation.
        let oversized = "A".repeat(64);
        assert!(
            crate::artifact_commands::decode_bounded(&oversized, 4, "image")
                .unwrap_err()
                .contains("Encoded image exceeds")
        );
        assert!(crate::artifact_commands::decode_bounded("not base64", 128, "image").is_err());
    }

    #[test]
    fn generated_image_filename_adds_a_timestamp_and_unique_suffix() {
        assert_eq!(
            generated_image_filename(
                "images/brand/Proxy Kit.PNG",
                "20260719-233420-123",
                "a1b2c3d4"
            )
            .unwrap(),
            "Proxy-Kit-20260719-233420-123-a1b2c3d4.png"
        );
        assert_eq!(
            generated_image_filename("images\\brand\\logo.png", "20260719-233420-124", "e5f6a7b8")
                .unwrap(),
            "logo-20260719-233420-124-e5f6a7b8.png"
        );
        assert!(generated_image_filename("logo.svg", "timestamp", "unique").is_err());
        assert!(generated_image_filename("  ", "timestamp", "unique").is_err());
    }

    #[test]
    fn generated_image_persists_without_a_workspace_root() {
        let tree = TempTree::new();
        let store = ArtifactStore::new(tree.path.join("generated-artifacts")).unwrap();
        let result = persist_generated_image(
            &store,
            "images/no-workspace-needed.png",
            "iVBORw0KGgo=",
            150,
            180,
        )
        .unwrap();
        let receipt: serde_json::Value = serde_json::from_str(&result).unwrap();
        let artifact_id = receipt["artifactId"].as_str().unwrap();
        let suggested_name = receipt["suggestedName"].as_str().unwrap();
        assert!(
            Regex::new(r"^no-workspace-needed-\d{8}-\d{6}-\d{3}-[0-9a-f]{8}\.png$")
                .unwrap()
                .is_match(suggested_name)
        );
        assert_ne!(suggested_name, "no-workspace-needed.png");
        assert_eq!(receipt["width"], 150);
        assert_eq!(receipt["height"], 180);
        assert_eq!(store.read(artifact_id).unwrap(), b"\x89PNG\r\n\x1a\n");
    }

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
        assert!(
            err.contains("Invalid glob pattern"),
            "unexpected error: {err}"
        );
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
    ///
    /// Built through `test_support::build` rather than `tauri::test`'s own
    /// context, so this app's app-data directory — and therefore the run
    /// ledger every auto-approved permission decision is written to — belongs
    /// to one test and no other.
    fn mock_app_with_workspace(root: &std::path::Path) -> tauri::App<tauri::test::MockRuntime> {
        let canonical = root.canonicalize().unwrap();
        let checkpoint_dir = canonical.join(".checkpoint");
        std::fs::create_dir_all(&checkpoint_dir).unwrap();

        let state = crate::AppState::default();
        *state.permissions.mode.lock().unwrap() = "acceptEdits".to_string();
        state
            .workspace_roots
            .lock()
            .unwrap()
            .push(workspace::WorkspaceRoot {
                id: canonical.to_string_lossy().to_string(),
                label: "test".to_string(),
                path: canonical,
            });
        state.checkpoints.lock().unwrap().insert(
            "test-checkpoint".to_string(),
            checkpoints::ActiveCheckpoint {
                remembered_facts: Vec::new(),
                staged_task_suggestions: Vec::new(),
                dir: checkpoint_dir,
                entries: Vec::new(),
                created_at_ms: 0,
                session_id: String::new(),
                anchor_index: 0,
                label: String::new(),
                shell_ran: false,
                external_effects: std::collections::BTreeSet::new(),
                committed_effects: std::collections::BTreeSet::new(),
                prev_id: None,
            },
        );

        crate::test_support::build(
            tauri::test::mock_builder()
                .invoke_handler(tauri::generate_handler![tool_edit_file])
                .manage(state),
        )
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

            let run = |handle: tauri::AppHandle<tauri::test::MockRuntime>,
                       new_value: &'static str| {
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
            assert_eq!(
                successes, 1,
                "expected exactly one edit to win, got: {result_a:?} / {result_b:?}"
            );

            let final_content = std::fs::read_to_string(tree.path.join("shared.txt")).unwrap();
            assert!(
                final_content == "hello FROM_A world" || final_content == "hello FROM_B world",
                "file content corrupted rather than a clean win by one editor: {final_content:?}"
            );
        }
    }
}
