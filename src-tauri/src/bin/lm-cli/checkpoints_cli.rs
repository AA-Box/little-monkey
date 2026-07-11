//! Per-turn file checkpoints for the CLI, reusing `little_monkey_lib::checkpoints`'s
//! `AppHandle`-free impls directly. Resolves the same on-disk directory the
//! GUI's `checkpoints.rs` uses — no `tauri::AppHandle` to go through, so the
//! base dir is computed the same way `providers_cli.rs` reads `providers.json`:
//! the OS data-dir convention (what Tauri v2's `app_data_dir()` also uses)
//! joined with the app's identifier.
//!
//! Every CLI-originated checkpoint is tagged with the fixed `"cli"` session
//! id (there is no real per-window session here — CLI history is in-memory
//! only), which also lets `/revert` and `lm revert` find "the most recent CLI
//! checkpoint" without mixing in the desktop app's own per-window checkpoints
//! that happen to live in the same directory.
//!
//! No timeline UI or conversation rewind here — out of scope for the CLI per
//! the design doc (its history is in-memory and lost on exit anyway).

use std::path::PathBuf;

use little_monkey_lib::checkpoints;

/// Must match `identifier` in `src-tauri/tauri.conf.json`.
const APP_IDENTIFIER: &str = "com.littlemonkey.app";

/// Session id every CLI turn's checkpoint is stamped with.
pub const CLI_SESSION_ID: &str = "cli";

/// Resolves (creating if necessary) `<app-data>/checkpoints`. `None` only
/// when the OS's data dir itself can't be resolved, or it can't be created
/// (e.g. a read-only home) — callers degrade to "no checkpoint this turn"
/// rather than failing the turn.
pub fn base_dir() -> Option<PathBuf> {
    let dir = dirs::data_dir()?.join(APP_IDENTIFIER).join("checkpoints");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Reverts checkpoint `id`, or — when `None` — the most recent `"cli"`-tagged
/// checkpoint. Returns the restored-file count, same as `checkpoint_revert`.
pub fn revert(id: Option<&str>) -> Result<u32, String> {
    let base = base_dir().ok_or("Could not resolve the app data directory")?;
    let target_id = match id {
        Some(id) => id.to_string(),
        None => most_recent(&base)?.ok_or("No checkpoints found")?.id,
    };
    checkpoints::revert_impl(&base, &target_id)
}

/// The newest `"cli"`-tagged checkpoint on disk, if any.
fn most_recent(base: &std::path::Path) -> Result<Option<checkpoints::CheckpointInfo>, String> {
    // `list_impl` already sorts newest-first.
    Ok(checkpoints::list_impl(base, Some(CLI_SESSION_ID))?.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revert_with_no_checkpoints_on_disk_errors_clearly() {
        // An empty temp dir stands in for a fresh app-data/checkpoints — no
        // OS dependency, since `base_dir()` itself isn't exercised here.
        let base = std::env::temp_dir().join(format!(
            "little_monkey_checkpoints_cli_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();

        let result = most_recent(&base).unwrap();
        assert!(result.is_none(), "empty checkpoints dir must have no most-recent checkpoint");

        let _ = std::fs::remove_dir_all(&base);
    }
}
