//! Per-turn file checkpoints: before the agent's first mutation of any file
//! in a turn, the file's original content is snapshotted so the whole turn's
//! file changes can be reverted with one click.
//!
//! Lifecycle (driven by the frontend agent loop in `src/lib/agentLoop.ts`):
//! 1. `checkpoint_begin` at turn start — opens a checkpoint and returns its
//!    id. Multiple checkpoints can be open at once (the split pane runs two
//!    turns concurrently), so all per-turn state is keyed by that id.
//! 2. While open, every `tool_write_file`/`tool_edit_file` calls
//!    [`record_original`] with the owning turn's checkpoint id *before*
//!    touching the file, which copies the pre-mutation content (or records
//!    "did not exist") into that checkpoint's directory. Only the first
//!    mutation of a given path per turn is recorded — later writes in the
//!    same turn would otherwise clobber the true original.
//! 3. `checkpoint_end` (with the id) at turn end — writes a manifest and
//!    reports which files were touched (empty = checkpoint discarded), so
//!    the frontend can show a "revert this turn" affordance only when
//!    something changed.
//! 4. `checkpoint_revert` (user-initiated UI action, like `git_commit` — not
//!    permission-gated) restores every recorded file: originals are copied
//!    back, files that didn't exist before the turn are deleted.
//!
//! Checkpoints live under `<app_data>/checkpoints/<uuid>/` and the newest
//! [`MAX_CHECKPOINTS`] are kept, so old turns remain revertable across app
//! restarts without growing unboundedly.

use std::path::{Path, PathBuf};

use tauri::Manager;

use crate::AppState;

/// How many finished checkpoints to keep on disk before pruning the oldest.
const MAX_CHECKPOINTS: usize = 20;

const MANIFEST_FILE: &str = "manifest.json";

/// One file recorded in a checkpoint.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct CheckpointEntry {
    /// Absolute path of the workspace file that was (about to be) mutated.
    pub path: String,
    /// Backup file name inside the checkpoint directory, when the file
    /// existed before the turn. `None` means the file was newly created by
    /// the turn, so reverting deletes it.
    pub backup: Option<String>,
}

/// An open checkpoint for one in-flight turn. Lives in `AppState::checkpoints`
/// keyed by its id until the turn's `checkpoint_end` removes it.
pub struct ActiveCheckpoint {
    pub dir: PathBuf,
    pub entries: Vec<CheckpointEntry>,
}

/// Summary returned to the frontend by `checkpoint_end`.
#[derive(serde::Serialize)]
pub struct CheckpointSummary {
    pub id: String,
    /// Absolute paths of every file the turn mutated. Empty means nothing
    /// was recorded and the checkpoint was discarded.
    pub files: Vec<String>,
}

fn checkpoints_base_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {}", e))?
        .join("checkpoints");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create checkpoints dir: {}", e))?;
    Ok(dir)
}

/// Reject anything that isn't a plain UUID-shaped id, so a crafted id can
/// never traverse outside the checkpoints directory.
fn validate_id(id: &str) -> Result<(), String> {
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        Ok(())
    } else {
        Err(format!("Invalid checkpoint id '{}'", id))
    }
}

/// Delete the oldest checkpoint directories beyond [`MAX_CHECKPOINTS`].
/// Best-effort: pruning failures never fail the turn that triggered them.
fn prune_old(base_dir: &Path) {
    let Ok(read_dir) = std::fs::read_dir(base_dir) else {
        return;
    };

    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = read_dir
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect();

    if dirs.len() <= MAX_CHECKPOINTS {
        return;
    }

    dirs.sort_by_key(|(modified, _)| *modified);
    let excess = dirs.len() - MAX_CHECKPOINTS;
    for (_, path) in dirs.into_iter().take(excess) {
        let _ = std::fs::remove_dir_all(path);
    }
}

/// Core begin logic, parameterized by base dir for testability.
pub fn begin_impl(state: &AppState, base_dir: &Path) -> Result<String, String> {
    prune_old(base_dir);

    let id = uuid::Uuid::new_v4().to_string();
    let dir = base_dir.join(&id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create checkpoint dir: {}", e))?;

    // A checkpoint whose turn crashed mid-flight (its `checkpoint_end` never
    // arrives) stays in the map until app restart — a few stray entries at
    // most, and its manifest-less directory on disk is inert and gets pruned
    // eventually.
    state
        .checkpoints
        .lock()
        .map_err(|_| "Checkpoint lock poisoned".to_string())?
        .insert(id.clone(), ActiveCheckpoint { dir, entries: Vec::new() });

    Ok(id)
}

/// Snapshot `resolved`'s current content into checkpoint `id` (no-op if `id`
/// is `None` — the turn runs without a checkpoint — or unknown, or this path
/// was already recorded this turn). Called by `tool_write_file`/
/// `tool_edit_file` BEFORE mutating the file. A backup failure aborts the
/// mutation — writing without a recoverable original would silently break
/// the revert guarantee.
pub fn record_original(state: &AppState, id: Option<&str>, resolved: &Path) -> Result<(), String> {
    let Some(id) = id else {
        return Ok(());
    };

    let mut guard = state
        .checkpoints
        .lock()
        .map_err(|_| "Checkpoint lock poisoned".to_string())?;
    let Some(active) = guard.get_mut(id) else {
        return Ok(());
    };

    let path_str = resolved.to_string_lossy().to_string();
    if active.entries.iter().any(|e| e.path == path_str) {
        return Ok(());
    }

    let backup = if resolved.is_file() {
        let backup_name = format!("{}.bak", active.entries.len());
        std::fs::copy(resolved, active.dir.join(&backup_name))
            .map_err(|e| format!("Failed to back up '{}' before modifying it: {}", path_str, e))?;
        Some(backup_name)
    } else {
        None
    };

    active.entries.push(CheckpointEntry { path: path_str, backup });
    Ok(())
}

/// Core end logic: close checkpoint `id`, persist its manifest (or discard
/// the empty directory), and report what was touched.
pub fn end_impl(state: &AppState, id: &str) -> Result<CheckpointSummary, String> {
    let taken = state
        .checkpoints
        .lock()
        .map_err(|_| "Checkpoint lock poisoned".to_string())?
        .remove(id);

    let Some(active) = taken else {
        return Ok(CheckpointSummary { id: String::new(), files: Vec::new() });
    };

    if active.entries.is_empty() {
        let _ = std::fs::remove_dir_all(&active.dir);
        return Ok(CheckpointSummary { id: id.to_string(), files: Vec::new() });
    }

    let manifest = serde_json::to_string_pretty(&active.entries)
        .map_err(|e| format!("Failed to serialize checkpoint manifest: {}", e))?;
    std::fs::write(active.dir.join(MANIFEST_FILE), manifest)
        .map_err(|e| format!("Failed to write checkpoint manifest: {}", e))?;

    Ok(CheckpointSummary {
        id: id.to_string(),
        files: active.entries.iter().map(|e| e.path.clone()).collect(),
    })
}

/// Core revert logic: restore every file recorded in checkpoint `id`.
/// Returns how many files were restored/removed.
pub fn revert_impl(base_dir: &Path, id: &str) -> Result<u32, String> {
    validate_id(id)?;

    let dir = base_dir.join(id);
    let manifest_raw = std::fs::read_to_string(dir.join(MANIFEST_FILE))
        .map_err(|e| format!("Checkpoint '{}' not found or unreadable: {}", id, e))?;
    let entries: Vec<CheckpointEntry> = serde_json::from_str(&manifest_raw)
        .map_err(|e| format!("Checkpoint '{}' manifest is corrupt: {}", id, e))?;

    let mut reverted = 0u32;
    let mut errors: Vec<String> = Vec::new();

    for entry in &entries {
        let target = Path::new(&entry.path);
        let result = match &entry.backup {
            Some(backup_name) => std::fs::copy(dir.join(backup_name), target).map(|_| ()),
            None => match std::fs::remove_file(target) {
                // Already gone — the desired end state, not an error.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            },
        };
        match result {
            Ok(()) => reverted += 1,
            Err(e) => errors.push(format!("{}: {}", entry.path, e)),
        }
    }

    if !errors.is_empty() {
        return Err(format!(
            "Reverted {} of {} files; failures: {}",
            reverted,
            entries.len(),
            errors.join("; ")
        ));
    }

    Ok(reverted)
}

/// Open a new per-turn checkpoint and return its id.
#[tauri::command]
pub fn checkpoint_begin(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<String, String> {
    begin_impl(state.inner(), &checkpoints_base_dir(&app)?)
}

/// Close checkpoint `id`; returns the touched files (empty = nothing was
/// mutated this turn and the checkpoint was discarded).
#[tauri::command]
pub fn checkpoint_end(state: tauri::State<'_, AppState>, id: String) -> Result<CheckpointSummary, String> {
    end_impl(state.inner(), &id)
}

/// Restore every file recorded in checkpoint `id` to its pre-turn state.
/// A direct, human-initiated UI action (the transcript's "Revert" button) —
/// like `git_commit`, intentionally NOT routed through the permission system.
#[tauri::command]
pub fn checkpoint_revert(app: tauri::AppHandle, id: String) -> Result<u32, String> {
    revert_impl(&checkpoints_base_dir(&app)?, &id)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            // Nanos alone can collide across parallel test threads — the
            // atomic counter guarantees uniqueness within the process.
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "little_monkey_checkpoint_test_{}_{}_{}_{}",
                tag,
                std::process::id(),
                n,
                nanos
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn record_is_a_noop_without_a_checkpoint_id() {
        let state = AppState::default();
        let ws = TempDir::new("ws");
        let file = ws.path.join("a.txt");
        std::fs::write(&file, "hello").unwrap();

        record_original(&state, None, &file).unwrap();
        record_original(&state, Some("0000-unknown-id"), &file).unwrap();

        assert!(state.checkpoints.lock().unwrap().is_empty());
    }

    #[test]
    fn end_discards_an_empty_checkpoint() {
        let state = AppState::default();
        let base = TempDir::new("base");

        let id = begin_impl(&state, &base.path).unwrap();
        let summary = end_impl(&state, &id).unwrap();

        assert_eq!(summary.id, id);
        assert!(summary.files.is_empty());
        assert!(!base.path.join(&id).exists(), "empty checkpoint dir must be removed");
    }

    #[test]
    fn revert_restores_modified_file_and_deletes_created_file() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let existing = ws.path.join("existing.txt");
        std::fs::write(&existing, "original").unwrap();
        let created = ws.path.join("created.txt");

        let id = begin_impl(&state, &base.path).unwrap();
        record_original(&state, Some(&id), &existing).unwrap();
        std::fs::write(&existing, "mutated").unwrap();
        record_original(&state, Some(&id), &created).unwrap();
        std::fs::write(&created, "brand new").unwrap();
        let summary = end_impl(&state, &id).unwrap();

        assert_eq!(summary.files.len(), 2);

        let reverted = revert_impl(&base.path, &id).unwrap();
        assert_eq!(reverted, 2);
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "original");
        assert!(!created.exists(), "file created during the turn must be deleted on revert");
    }

    #[test]
    fn concurrent_checkpoints_record_and_end_independently() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file_a = ws.path.join("a.txt");
        std::fs::write(&file_a, "a-original").unwrap();
        let file_b = ws.path.join("b.txt");
        std::fs::write(&file_b, "b-original").unwrap();

        // Two turns in flight at once (split pane): each records its own
        // file under its own checkpoint id, in interleaved order.
        let id_a = begin_impl(&state, &base.path).unwrap();
        let id_b = begin_impl(&state, &base.path).unwrap();
        record_original(&state, Some(&id_a), &file_a).unwrap();
        std::fs::write(&file_a, "a-mutated").unwrap();
        record_original(&state, Some(&id_b), &file_b).unwrap();
        std::fs::write(&file_b, "b-mutated").unwrap();

        // Ending turn A must not close or steal turn B's checkpoint.
        let summary_a = end_impl(&state, &id_a).unwrap();
        assert_eq!(summary_a.id, id_a);
        assert_eq!(summary_a.files, vec![file_a.to_string_lossy().to_string()]);

        let summary_b = end_impl(&state, &id_b).unwrap();
        assert_eq!(summary_b.id, id_b);
        assert_eq!(summary_b.files, vec![file_b.to_string_lossy().to_string()]);

        // Each revert restores only its own turn's file.
        revert_impl(&base.path, &id_a).unwrap();
        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "a-original");
        assert_eq!(std::fs::read_to_string(&file_b).unwrap(), "b-mutated");
        revert_impl(&base.path, &id_b).unwrap();
        assert_eq!(std::fs::read_to_string(&file_b).unwrap(), "b-original");
    }

    #[test]
    fn only_the_first_mutation_of_a_path_is_recorded() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file = ws.path.join("a.txt");
        std::fs::write(&file, "v1").unwrap();

        let id = begin_impl(&state, &base.path).unwrap();
        record_original(&state, Some(&id), &file).unwrap();
        std::fs::write(&file, "v2").unwrap();
        record_original(&state, Some(&id), &file).unwrap(); // second write same turn
        std::fs::write(&file, "v3").unwrap();
        end_impl(&state, &id).unwrap();

        revert_impl(&base.path, &id).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "v1",
            "revert must restore the true pre-turn original, not an intermediate version"
        );
    }

    #[test]
    fn revert_rejects_traversal_ids() {
        let base = TempDir::new("base");
        let err = revert_impl(&base.path, "../outside").unwrap_err();
        assert!(err.contains("Invalid checkpoint id"), "unexpected error: {err}");
    }

    #[test]
    fn revert_errors_for_unknown_id() {
        let base = TempDir::new("base");
        let err = revert_impl(&base.path, "0000-does-not-exist").unwrap_err();
        assert!(err.contains("not found"), "unexpected error: {err}");
    }
}
