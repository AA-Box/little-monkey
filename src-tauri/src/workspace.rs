//! Multi-root workspace management: which folders are attached (one
//! "primary" plus any number of "secondary" folders), sandboxed path
//! resolution across all of them, and a small on-disk "recently opened"
//! list backing the primary-folder picker's "Recent" dropdown.
//!
//! Every agent tool in `tools.rs` resolves paths through
//! [`resolve_path_and_root`] rather than assuming a single root: a plain
//! relative/absolute path resolves against the primary root exactly as it
//! always has (so nothing changes for the common single-folder case), while
//! a path prefixed with `"<label>/"` for an attached secondary folder
//! resolves against that root instead. This is a deliberate, minimal way to
//! let the agent address any attached folder without changing the shape of
//! every tool's `path` parameter.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::MutexGuard;

use tauri::Manager;

use crate::{permissions, AppState};

/// One attached folder. `id` is the canonicalized path string — stable and
/// unique for as long as the folder stays attached.
pub struct WorkspaceRoot {
    pub id: String,
    pub path: PathBuf,
    pub label: String,
}

/// Serialized shape handed to the frontend for a single attached folder.
#[derive(serde::Serialize)]
pub struct WorkspaceRootInfo {
    pub id: String,
    pub path: String,
    pub label: String,
    pub is_primary: bool,
}

impl WorkspaceRoot {
    fn to_info(&self, is_primary: bool) -> WorkspaceRootInfo {
        WorkspaceRootInfo {
            id: self.id.clone(),
            path: self.path.to_string_lossy().to_string(),
            label: self.label.clone(),
            is_primary,
        }
    }
}

/// One entry in the on-disk "recently opened primary workspace" list.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct RecentWorkspaceEntry {
    pub path: String,
    pub label: String,
    pub last_opened_at: u64,
}

const RECENT_WORKSPACES_FILE: &str = "recent_workspaces.json";
const MAX_RECENT_ENTRIES: usize = 12;

fn roots_lock(state: &AppState) -> Result<MutexGuard<'_, Vec<WorkspaceRoot>>, String> {
    state
        .workspace_roots
        .lock()
        .map_err(|_| "Workspace roots lock poisoned".to_string())
}

fn label_for(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// Compute a display label for `path` that doesn't collide with any label
/// already in `existing`, by prepending successive parent-directory
/// segments (e.g. `app` -> `workspace/app`), falling back to a numeric
/// suffix if parents run out and it still collides.
fn unique_label(path: &Path, existing: &[WorkspaceRoot]) -> String {
    let base = label_for(path);
    if !existing.iter().any(|r| r.label == base) {
        return base;
    }

    let segments: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().to_string()),
            _ => None,
        })
        .collect();

    for take in 2..=segments.len() {
        let candidate = segments[segments.len() - take..].join("/");
        if !existing.iter().any(|r| r.label == candidate) {
            return candidate;
        }
    }

    let mut n = 2;
    loop {
        let candidate = format!("{} ({})", base, n);
        if !existing.iter().any(|r| r.label == candidate) {
            return candidate;
        }
        n += 1;
    }
}

fn canonical_dir(path: &str) -> Result<PathBuf, String> {
    let canonical = PathBuf::from(path)
        .canonicalize()
        .map_err(|e| format!("Invalid workspace path '{}': {}", path, e))?;
    if !canonical.is_dir() {
        return Err(format!("'{}' is not a directory", path));
    }
    Ok(canonical)
}

/// The primary (index 0) root, re-canonicalized on every call — mirrors the
/// old single-root `workspace_root_canon`'s "re-verify it still exists"
/// behavior.
pub fn primary_root_canon(state: &AppState) -> Result<PathBuf, String> {
    let roots = roots_lock(state)?;
    let primary = roots
        .first()
        .ok_or_else(|| "No workspace folder is open. Open a folder first.".to_string())?;
    primary.path.canonicalize().map_err(|e| {
        format!(
            "Workspace root '{}' is no longer valid: {}",
            primary.path.display(),
            e
        )
    })
}

/// Every attached root as `(canonical_path, label, is_primary)`, primary
/// first. Used by `list_workspace_paths` to walk all of them.
pub fn all_roots(state: &AppState) -> Result<Vec<(PathBuf, String, bool)>, String> {
    let roots = roots_lock(state)?;
    if roots.is_empty() {
        return Err("No workspace folder is open. Open a folder first.".to_string());
    }
    roots
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let canon = r.path.canonicalize().map_err(|e| {
                format!(
                    "Workspace root '{}' is no longer valid: {}",
                    r.path.display(),
                    e
                )
            })?;
            Ok((canon, r.label.clone(), i == 0))
        })
        .collect()
}

/// If `root_canon` is a currently-attached *secondary* root, its label
/// (for prefixing display paths); `None` for the primary root or an
/// unrecognized path, so callers default to unprefixed display paths.
pub fn secondary_label_for(state: &AppState, root_canon: &Path) -> Result<Option<String>, String> {
    let roots = roots_lock(state)?;
    for (i, root) in roots.iter().enumerate() {
        if let Ok(canon) = root.path.canonicalize() {
            if canon == root_canon {
                return Ok(if i == 0 {
                    None
                } else {
                    Some(root.label.clone())
                });
            }
        }
    }
    Ok(None)
}

/// Resolve `raw` (relative or absolute) against `root_canon`, returning an
/// absolute, canonicalized path guaranteed to live inside it.
///
/// This is the original single-root sandboxing algorithm, parameterized by
/// which root to check against. It:
/// 1. Joins relative paths onto the (already-canonical) root.
/// 2. Lexically collapses `.`/`..` components *before* touching the
///    filesystem, so a crafted `..` sequence can't be used to walk out of
///    the sandbox even for paths that don't exist yet.
/// 3. Canonicalizes the longest existing ancestor of that path (resolving
///    any symlinks) and re-appends whatever trailing components don't exist
///    yet (e.g. a new file being created by `write_file`).
/// 4. Rejects the result unless it is the root itself or a descendant.
fn resolve_against_root(root_canon: &Path, raw: &str) -> Result<PathBuf, String> {
    let requested = Path::new(raw);
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root_canon.join(requested)
    };

    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }

    if normalized.as_os_str().is_empty() {
        return Err(format!("Invalid path: '{}'", raw));
    }

    let mut existing_ancestor = normalized.clone();
    let mut remainder: Vec<OsString> = Vec::new();
    while !existing_ancestor.exists() {
        match existing_ancestor.file_name() {
            Some(name) => {
                remainder.push(name.to_os_string());
                if !existing_ancestor.pop() {
                    break;
                }
            }
            None => break,
        }
    }

    let canon_ancestor = existing_ancestor
        .canonicalize()
        .map_err(|e| format!("Invalid path '{}': {}", raw, e))?;

    let mut resolved = canon_ancestor;
    for part in remainder.into_iter().rev() {
        resolved.push(part);
    }

    if !resolved.starts_with(root_canon) {
        return Err(format!(
            "Path '{}' escapes the workspace root and is not allowed",
            raw
        ));
    }

    Ok(resolved)
}

/// Resolve `path` (as given by the model) against the correct attached
/// root: a path prefixed with `"<label>/"` for a currently-attached
/// secondary folder resolves against that root; anything else (plain
/// relative, or absolute) resolves against the primary root exactly as
/// before, except that absolute paths falling inside a secondary root are
/// also honored. Returns the resolved path plus the canonical root it was
/// resolved against.
pub fn resolve_path_and_root(state: &AppState, path: &str) -> Result<(PathBuf, PathBuf), String> {
    let roots = roots_lock(state)?;
    if roots.is_empty() {
        return Err("No workspace folder is open. Open a folder first.".to_string());
    }

    if Path::new(path).is_absolute() {
        for root in roots.iter() {
            let root_canon = root.path.canonicalize().map_err(|e| {
                format!(
                    "Workspace root '{}' is no longer valid: {}",
                    root.path.display(),
                    e
                )
            })?;
            if let Ok(resolved) = resolve_against_root(&root_canon, path) {
                return Ok((resolved, root_canon));
            }
        }
        return Err(format!(
            "Path '{}' escapes the workspace root and is not allowed",
            path
        ));
    }

    for root in roots.iter().skip(1) {
        let prefix = format!("{}/", root.label);
        if let Some(rest) = path.strip_prefix(&prefix) {
            let root_canon = root.path.canonicalize().map_err(|e| {
                format!(
                    "Workspace root '{}' is no longer valid: {}",
                    root.path.display(),
                    e
                )
            })?;
            let resolved = resolve_against_root(&root_canon, rest)?;
            return Ok((resolved, root_canon));
        }
    }

    let primary = &roots[0];
    let root_canon = primary.path.canonicalize().map_err(|e| {
        format!(
            "Workspace root '{}' is no longer valid: {}",
            primary.path.display(),
            e
        )
    })?;
    let resolved = resolve_against_root(&root_canon, path)?;
    Ok((resolved, root_canon))
}

/// Core logic behind [`set_primary_workspace_root`], factored out so it's
/// directly testable without a `tauri::AppHandle`. Returns whether the
/// primary actually changed (callers use this to decide whether to reset
/// permission grants / record a recent-workspace entry) plus the resulting
/// info.
fn set_primary_workspace_root_impl(
    state: &AppState,
    path: String,
) -> Result<(bool, WorkspaceRootInfo), String> {
    let canonical = canonical_dir(&path)?;
    let label = label_for(&canonical);
    let id = canonical.to_string_lossy().to_string();

    let (changed, info) = {
        let mut roots = roots_lock(state)?;
        let changed = roots.first().map(|r| r.path != canonical).unwrap_or(true);
        let root = WorkspaceRoot {
            id,
            path: canonical,
            label,
        };
        let info = root.to_info(true);
        if changed {
            *roots = vec![root];
        }
        (changed, info)
    };

    if changed {
        permissions::reset_for_new_workspace(state);
    }

    Ok((changed, info))
}

/// Open a folder as the primary workspace root. If this actually changes
/// the primary (as opposed to re-opening the same one), every attached
/// secondary folder is dropped and every session-scoped permission grant
/// (and any still-pending prompt) is reset — a grant like "allow run_shell
/// for session" made while working in one workspace must not silently carry
/// over and apply, unattended, to a different one.
#[tauri::command]
pub fn set_primary_workspace_root(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<WorkspaceRootInfo, String> {
    let (changed, info) = set_primary_workspace_root_impl(state.inner(), path)?;
    if changed {
        // A PTY remains an unrestricted OS process after it starts. Kill it
        // before a workspace switch can leave a now-detached shell running
        // behind the new workspace's permission boundary.
        state.terminal.kill_all(Some(&app));
    }
    record_recent(&app, Path::new(&info.path), &info.label);
    Ok(info)
}

fn add_secondary_workspace_root_impl(
    state: &AppState,
    path: String,
) -> Result<WorkspaceRootInfo, String> {
    let canonical = canonical_dir(&path)?;
    let id = canonical.to_string_lossy().to_string();

    let mut roots = roots_lock(state)?;
    if roots.is_empty() {
        return Err("Open a primary workspace folder first.".to_string());
    }
    if let Some(idx) = roots.iter().position(|r| r.id == id) {
        return Ok(roots[idx].to_info(idx == 0));
    }

    let label = unique_label(&canonical, &roots);
    let root = WorkspaceRoot {
        id,
        path: canonical,
        label,
    };
    let info = root.to_info(false);
    roots.push(root);
    Ok(info)
}

/// Attach an additional folder the agent can read/write/list/grep/run shell
/// commands in, addressed by prefixing tool paths with its label (see
/// [`resolve_path_and_root`]). A no-op (returns the existing entry) if the
/// folder is already attached. Unlike swapping the primary, this never
/// touches permission grants — the sandbox boundary is still enforced per
/// path, and attaching a folder is itself an explicit, visible user action.
#[tauri::command]
pub fn add_secondary_workspace_root(
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<WorkspaceRootInfo, String> {
    add_secondary_workspace_root_impl(state.inner(), path)
}

fn remove_secondary_workspace_root_impl(state: &AppState, id: String) -> Result<(), String> {
    let mut roots = roots_lock(state)?;
    if roots.first().map(|r| r.id == id).unwrap_or(false) {
        return Err("Cannot remove the primary workspace".to_string());
    }
    let before = roots.len();
    roots.retain(|r| r.id != id);
    if roots.len() == before {
        return Err(format!("No attached folder with id '{}'", id));
    }
    Ok(())
}

/// Detach a secondary folder. Errors if `id` refers to the primary root —
/// use [`set_primary_workspace_root`] to change that instead.
#[tauri::command]
pub fn remove_secondary_workspace_root(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    remove_secondary_workspace_root_impl(state.inner(), id.clone())?;
    state.terminal.kill_workspace(&id, Some(&app));
    Ok(())
}

/// Every currently attached folder, primary first.
#[tauri::command]
pub fn get_workspace_roots(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<WorkspaceRootInfo>, String> {
    let roots = roots_lock(state.inner())?;
    Ok(roots
        .iter()
        .enumerate()
        .map(|(i, r)| r.to_info(i == 0))
        .collect())
}

fn recent_workspaces_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {}", e))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create app data dir: {}", e))?;
    Ok(dir.join(RECENT_WORKSPACES_FILE))
}

fn read_recent(app: &tauri::AppHandle) -> Vec<RecentWorkspaceEntry> {
    let Ok(file_path) = recent_workspaces_path(app) else {
        return Vec::new();
    };
    let Ok(raw) = std::fs::read_to_string(&file_path) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn write_recent(app: &tauri::AppHandle, entries: &[RecentWorkspaceEntry]) {
    let Ok(file_path) = recent_workspaces_path(app) else {
        return;
    };
    if let Ok(json) = serde_json::to_string_pretty(entries) {
        let _ = std::fs::write(file_path, json);
    }
}

/// Record `path` as the most-recently-opened primary workspace, moving it
/// to the front if already present and capping the list at
/// [`MAX_RECENT_ENTRIES`]. Best-effort: failures are swallowed since this is
/// convenience metadata, not something a folder-open should ever fail over.
fn record_recent(app: &tauri::AppHandle, path: &Path, label: &str) {
    let mut entries = read_recent(app);
    let path_str = path.to_string_lossy().to_string();
    entries.retain(|e| e.path != path_str);

    let last_opened_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    entries.insert(
        0,
        RecentWorkspaceEntry {
            path: path_str,
            label: label.to_string(),
            last_opened_at,
        },
    );
    entries.truncate(MAX_RECENT_ENTRIES);
    write_recent(app, &entries);
}

/// The persisted "recently opened primary workspace" list, most recent
/// first.
#[tauri::command]
pub fn get_recent_workspaces(app: tauri::AppHandle) -> Result<Vec<RecentWorkspaceEntry>, String> {
    Ok(read_recent(&app))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempWorkspace {
        path: PathBuf,
    }

    impl TempWorkspace {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "little_monkey_workspace_test_{}_{}_{}",
                std::process::id(),
                n,
                nanos
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempWorkspace { path }
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn state_with_primary(root: &Path) -> AppState {
        let state = AppState::default();
        let (_, _) =
            set_primary_workspace_root_impl(&state, root.to_string_lossy().to_string()).unwrap();
        state
    }

    #[test]
    fn allows_path_inside_workspace() {
        let ws = TempWorkspace::new();
        std::fs::write(ws.path.join("file.txt"), "hi").unwrap();
        let state = state_with_primary(&ws.path);

        let (resolved, _) = resolve_path_and_root(&state, "file.txt").unwrap();
        assert_eq!(resolved, ws.path.canonicalize().unwrap().join("file.txt"));
    }

    #[test]
    fn allows_nested_nonexistent_path_for_writes() {
        let ws = TempWorkspace::new();
        let state = state_with_primary(&ws.path);

        let (resolved, _) = resolve_path_and_root(&state, "new/deep/file.txt").unwrap();
        assert_eq!(
            resolved,
            ws.path
                .canonicalize()
                .unwrap()
                .join("new")
                .join("deep")
                .join("file.txt")
        );
    }

    #[test]
    fn rejects_parent_dir_traversal() {
        let ws = TempWorkspace::new();
        let state = state_with_primary(&ws.path);

        let err = resolve_path_and_root(&state, "../../../../etc/passwd").unwrap_err();
        assert!(err.contains("escapes"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_traversal_hidden_inside_a_deeper_relative_path() {
        let ws = TempWorkspace::new();
        std::fs::create_dir_all(ws.path.join("sub")).unwrap();
        let state = state_with_primary(&ws.path);

        let err = resolve_path_and_root(&state, "sub/../../../../etc/passwd").unwrap_err();
        assert!(err.contains("escapes"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_absolute_path_outside_workspace() {
        let ws = TempWorkspace::new();
        let state = state_with_primary(&ws.path);

        let err = resolve_path_and_root(&state, "/etc/passwd").unwrap_err();
        assert!(err.contains("escapes"), "unexpected error: {err}");
    }

    #[test]
    fn allows_absolute_path_inside_workspace() {
        let ws = TempWorkspace::new();
        std::fs::write(ws.path.join("file.txt"), "hi").unwrap();
        let state = state_with_primary(&ws.path);
        let canon_root = ws.path.canonicalize().unwrap();

        let abs = canon_root.join("file.txt");
        let (resolved, _) = resolve_path_and_root(&state, abs.to_str().unwrap()).unwrap();
        assert_eq!(resolved, abs);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escaping_workspace() {
        let ws = TempWorkspace::new();
        let outside = TempWorkspace::new();
        std::fs::write(outside.path.join("secret.txt"), "top secret").unwrap();
        std::os::unix::fs::symlink(&outside.path, ws.path.join("escape")).unwrap();

        let state = state_with_primary(&ws.path);

        let err = resolve_path_and_root(&state, "escape/secret.txt").unwrap_err();
        assert!(err.contains("escapes"), "unexpected error: {err}");
    }

    #[test]
    fn errors_when_no_workspace_open() {
        let state = AppState::default();
        let err = resolve_path_and_root(&state, "file.txt").unwrap_err();
        assert!(err.contains("No workspace"), "unexpected error: {err}");
    }

    #[test]
    fn set_primary_workspace_root_clears_session_grants_and_secondaries_when_it_actually_changes() {
        let ws_a = TempWorkspace::new();
        let ws_b = TempWorkspace::new();
        let state = state_with_primary(&ws_a.path);
        add_secondary_workspace_root_impl(&state, ws_b.path.to_string_lossy().to_string()).unwrap();
        state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .insert("write_file".to_string());

        let ws_c = TempWorkspace::new();
        set_primary_workspace_root_impl(&state, ws_c.path.to_string_lossy().to_string()).unwrap();

        assert!(
            state.permissions.session_allow.lock().unwrap().is_empty(),
            "switching the primary root must drop session-wide permission grants"
        );
        assert_eq!(
            roots_lock(&state).unwrap().len(),
            1,
            "switching the primary root must drop attached secondary folders"
        );
    }

    #[test]
    fn set_primary_workspace_root_keeps_session_grants_when_root_is_unchanged() {
        let ws = TempWorkspace::new();
        let state = state_with_primary(&ws.path);
        state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .insert("write_file".to_string());

        set_primary_workspace_root_impl(&state, ws.path.to_string_lossy().to_string()).unwrap();

        assert!(state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .contains("write_file"));
    }

    #[test]
    fn resolves_label_prefixed_path_into_secondary_root() {
        let primary = TempWorkspace::new();
        let secondary = TempWorkspace::new();
        std::fs::write(secondary.path.join("notes.txt"), "hi").unwrap();
        let state = state_with_primary(&primary.path);

        let info =
            add_secondary_workspace_root_impl(&state, secondary.path.to_string_lossy().to_string())
                .unwrap();
        assert!(!info.is_primary);

        let (resolved, root_canon) =
            resolve_path_and_root(&state, &format!("{}/notes.txt", info.label)).unwrap();
        assert_eq!(
            resolved,
            secondary.path.canonicalize().unwrap().join("notes.txt")
        );
        assert_eq!(root_canon, secondary.path.canonicalize().unwrap());
    }

    #[test]
    fn plain_relative_path_still_resolves_to_primary_when_secondary_attached() {
        let primary = TempWorkspace::new();
        let secondary = TempWorkspace::new();
        std::fs::write(primary.path.join("file.txt"), "hi").unwrap();
        let state = state_with_primary(&primary.path);
        add_secondary_workspace_root_impl(&state, secondary.path.to_string_lossy().to_string())
            .unwrap();

        let (resolved, root_canon) = resolve_path_and_root(&state, "file.txt").unwrap();
        assert_eq!(
            resolved,
            primary.path.canonicalize().unwrap().join("file.txt")
        );
        assert_eq!(root_canon, primary.path.canonicalize().unwrap());
    }

    #[test]
    fn add_secondary_workspace_root_dedupes_by_canonical_path() {
        let primary = TempWorkspace::new();
        let secondary = TempWorkspace::new();
        let state = state_with_primary(&primary.path);

        let first =
            add_secondary_workspace_root_impl(&state, secondary.path.to_string_lossy().to_string())
                .unwrap();
        let second =
            add_secondary_workspace_root_impl(&state, secondary.path.to_string_lossy().to_string())
                .unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(
            roots_lock(&state).unwrap().len(),
            2,
            "must not attach the same folder twice"
        );
    }

    #[test]
    fn add_secondary_workspace_root_disambiguates_colliding_labels() {
        let primary = TempWorkspace::new();
        let parent_a = TempWorkspace::new();
        let parent_b = TempWorkspace::new();
        let dup_a = parent_a.path.join("app");
        let dup_b = parent_b.path.join("app");
        std::fs::create_dir_all(&dup_a).unwrap();
        std::fs::create_dir_all(&dup_b).unwrap();
        let state = state_with_primary(&primary.path);

        let first =
            add_secondary_workspace_root_impl(&state, dup_a.to_string_lossy().to_string()).unwrap();
        let second =
            add_secondary_workspace_root_impl(&state, dup_b.to_string_lossy().to_string()).unwrap();

        assert_eq!(first.label, "app");
        assert_ne!(
            second.label, "app",
            "colliding basenames must be disambiguated"
        );
        assert!(second.label.ends_with("/app"));
    }

    #[test]
    fn remove_secondary_workspace_root_rejects_primary_id() {
        let primary = TempWorkspace::new();
        let state = state_with_primary(&primary.path);
        let primary_id = get_workspace_roots_for_test(&state)[0].id.clone();

        let err = remove_secondary_workspace_root_impl(&state, primary_id).unwrap_err();
        assert!(
            err.contains("Cannot remove the primary"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn remove_secondary_workspace_root_drops_only_that_entry() {
        let primary = TempWorkspace::new();
        let secondary = TempWorkspace::new();
        let state = state_with_primary(&primary.path);
        let info =
            add_secondary_workspace_root_impl(&state, secondary.path.to_string_lossy().to_string())
                .unwrap();

        remove_secondary_workspace_root_impl(&state, info.id).unwrap();

        assert_eq!(roots_lock(&state).unwrap().len(), 1);
    }

    #[test]
    fn adding_and_removing_secondary_roots_does_not_touch_permission_grants() {
        let primary = TempWorkspace::new();
        let secondary = TempWorkspace::new();
        let state = state_with_primary(&primary.path);
        state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .insert("write_file".to_string());

        let info =
            add_secondary_workspace_root_impl(&state, secondary.path.to_string_lossy().to_string())
                .unwrap();
        assert!(state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .contains("write_file"));

        remove_secondary_workspace_root_impl(&state, info.id).unwrap();
        assert!(state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .contains("write_file"));
    }

    fn get_workspace_roots_for_test(state: &AppState) -> Vec<WorkspaceRootInfo> {
        let roots = roots_lock(state).unwrap();
        roots
            .iter()
            .enumerate()
            .map(|(i, r)| r.to_info(i == 0))
            .collect()
    }
}
