//! File-based chat-session persistence.
//!
//! The frontend's `sessionStore` used to persist its whole
//! `{ sessions, activeSessionId, groups }` blob in `localStorage`, which has
//! a ~5MB quota that inline base64 image attachments blow through quickly —
//! and quota failures were silently swallowed. These commands store the same
//! JSON blob as a file in the app data directory instead: no quota, atomic
//! writes (temp file + rename), and errors that actually propagate back to
//! the frontend so it can surface them.
//!
//! The payload is treated as an opaque string on this side — the frontend
//! owns the schema (see `src/store/sessionStore.ts`), exactly as it did with
//! `localStorage`.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use tauri::{Emitter};
use crate::profiles::ProfileScopedPaths;

const SESSIONS_FILE: &str = "chat_sessions.json";

/// Emitted to every window after each successful [`sessions_save`], with the
/// saving window's label as payload. Secondary session windows (see
/// `system::open_session_window`) each run their own store instance writing
/// the whole blob last-writer-wins; this event lets the OTHER windows
/// rehydrate from the file (see `sessionStore.ts`) instead of clobbering each
/// other on their next save.
pub const SESSIONS_CHANGED_EVENT: &str = "sessions://changed";

pub(crate) fn sessions_file_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {}", e))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create app data dir: {}", e))?;
    Ok(dir.join(SESSIONS_FILE))
}

/// Core load logic, parameterized by path for testability.
fn load_from(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(Some(raw)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("Failed to read sessions file: {}", e)),
    }
}

/// Core save logic: write to a sibling temp file, then rename over the real
/// one, so a crash mid-write can never leave a truncated/corrupt sessions
/// file behind (rename within one directory is atomic on every supported
/// platform).
fn save_to(path: &Path, payload: &str) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, payload).map_err(|e| format!("Failed to write sessions file: {}", e))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("Failed to finalize sessions file: {}", e))?;
    Ok(())
}

/// The persisted sessions blob as a raw JSON string, or `None` if nothing
/// has been saved yet.
#[tauri::command]
pub fn sessions_load(app: tauri::AppHandle) -> Result<Option<String>, String> {
    load_from(&sessions_file_path(&app)?)
}

/// Persist the sessions blob (opaque JSON string owned by the frontend).
#[tauri::command]
pub fn sessions_save(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, crate::AppState>,
    payload: String,
) -> Result<(), String> {
    save_to(&sessions_file_path(&app)?, &payload)?;
    // Keep the validated transactional profile/index current on every
    // successful legacy snapshot. The JSON file remains a recovery copy.
    crate::profile_commands::sync_profile_payload(&app, state.inner(), &payload)?;
    // Best-effort fan-out to the other windows; the save itself succeeded.
    let _ = app.emit(SESSIONS_CHANGED_EVENT, window.label());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file() -> PathBuf {
        // Nanos alone can collide across parallel test threads — the atomic
        // counter guarantees uniqueness within the process.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "little_monkey_sessions_test_{}_{}_{}.json",
            std::process::id(),
            n,
            nanos
        ))
    }

    #[test]
    fn load_returns_none_when_file_missing() {
        let path = temp_file();
        assert_eq!(load_from(&path).unwrap(), None);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let path = temp_file();
        let payload = r#"{"sessions":[],"activeSessionId":"a","groups":[]}"#;
        save_to(&path, payload).unwrap();
        assert_eq!(load_from(&path).unwrap().as_deref(), Some(payload));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_overwrites_previous_content_atomically() {
        let path = temp_file();
        save_to(&path, "first").unwrap();
        save_to(&path, "second").unwrap();
        assert_eq!(load_from(&path).unwrap().as_deref(), Some("second"));
        // The temp file must not linger after a successful save.
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_file(&path);
    }
}
