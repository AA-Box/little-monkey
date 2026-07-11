//! File-based prompt-library persistence: saved personas (system-prompt
//! extensions) and reusable snippets, invoked from the chat input via a
//! "/"-command popup (see `src/store/promptStore.ts`).
//!
//! Mirrors `sessions.rs` exactly: the payload is an opaque JSON blob the
//! frontend owns the schema for, written atomically (temp file + rename) to
//! `<app_data>/prompts.json`, with a cross-window "changed" event so a
//! second session window rehydrates instead of last-writer-clobbering.
//!
//! Unlike `sessions.rs`, the core `load_impl`/`save_impl` pair here is `pub`
//! (not private) and a `pub PromptEntry` struct is exposed too, so `lm-cli`
//! can read personas straight out of this module without an `AppHandle` —
//! same seam as `checkpoints.rs`/`rules.rs` already provide for CLI reuse.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};

const PROMPTS_FILE: &str = "prompts.json";

/// Emitted to every window after each successful [`prompts_save`], with the
/// saving window's label as payload — same cross-window rehydrate mechanism
/// as `sessions::SESSIONS_CHANGED_EVENT`.
pub const PROMPTS_CHANGED_EVENT: &str = "prompts://changed";

/// One saved prompt-library entry: a persona (system-prompt extension) or a
/// reusable snippet. Mirrors the frontend's `PromptEntry` (see
/// `src/store/promptStore.ts`) field-for-field — `camelCase` on the wire
/// since `prompts.json` is a frontend-owned opaque blob, not meant to be
/// hand-edited (same convention as `McpServerInfo`).
///
/// Every field is `#[serde(default)]`-lenient: a hand-edited or partially
/// malformed entry deserializes into empty/absent defaults instead of
/// failing outright, mirroring `promptStore.ts`'s `normalizeEntry` on the
/// frontend side and `CustomProviderEntry`'s general leniency stance.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PromptEntry {
    #[serde(default)]
    pub id: String,
    /// `"persona"` or `"snippet"` — kept as a plain `String` (not an enum)
    /// so an unrecognized future variant round-trips instead of failing to
    /// parse; the frontend is the schema owner.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
}

fn prompts_file_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {}", e))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create app data dir: {}", e))?;
    Ok(dir.join(PROMPTS_FILE))
}

/// Core load logic, parameterized by path so it needs no `AppHandle` —
/// directly unit-testable and reusable from `lm-cli`.
pub fn load_impl(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(Some(raw)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("Failed to read prompts file: {}", e)),
    }
}

/// Core save logic: write to a sibling temp file, then rename over the real
/// one, so a crash mid-write can never leave a truncated/corrupt prompts
/// file behind (rename within one directory is atomic on every supported
/// platform) — identical mechanics to `sessions::save_to`.
pub fn save_impl(path: &Path, payload: &str) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, payload).map_err(|e| format!("Failed to write prompts file: {}", e))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("Failed to finalize prompts file: {}", e))?;
    Ok(())
}

/// The persisted prompt-library blob as a raw JSON string, or `None` if
/// nothing has been saved yet.
#[tauri::command]
pub fn prompts_load(app: tauri::AppHandle) -> Result<Option<String>, String> {
    load_impl(&prompts_file_path(&app)?)
}

/// Persist the prompt-library blob (opaque JSON string owned by the
/// frontend).
#[tauri::command]
pub fn prompts_save(
    app: tauri::AppHandle,
    window: tauri::Window,
    payload: String,
) -> Result<(), String> {
    save_impl(&prompts_file_path(&app)?, &payload)?;
    // Best-effort fan-out to the other windows; the save itself succeeded.
    let _ = app.emit(PROMPTS_CHANGED_EVENT, window.label());
    Ok(())
}

/// Read an arbitrary file's contents as UTF-8 text, for the Settings
/// "Prompts" tab's Import button. `path` comes from a user-picked OS file
/// dialog (`@tauri-apps/plugin-dialog`'s `open()`), not a workspace-relative
/// path — unlike every `tool_*` command this deliberately does NOT go
/// through `workspace::resolve_path_and_root` and is NOT routed through
/// `permissions::request_permission`. Same precedent as `git_commit` /
/// `checkpoint_revert` / `rules_write`: the human already picked exactly
/// this file via a native dialog, so there is nothing to gate. It also isn't
/// model-callable regardless, since it's a plain command name, not a
/// `tool_*` one the agent loop dispatches.
#[tauri::command]
pub fn prompts_read_external(path: String) -> Result<String, String> {
    std::fs::read_to_string(&path).map_err(|e| format!("Failed to read '{}': {}", path, e))
}

/// Write `payload` verbatim to an arbitrary file, for the Settings "Prompts"
/// tab's Export button. Same rationale as [`prompts_read_external`] for why
/// this skips both the workspace sandbox and the permission system.
#[tauri::command]
pub fn prompts_write_external(path: String, payload: String) -> Result<(), String> {
    std::fs::write(&path, payload).map_err(|e| format!("Failed to write '{}': {}", path, e))
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
            "little_monkey_prompts_test_{}_{}_{}.json",
            std::process::id(),
            n,
            nanos
        ))
    }

    #[test]
    fn load_returns_none_when_file_missing() {
        let path = temp_file();
        assert_eq!(load_impl(&path).unwrap(), None);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let path = temp_file();
        let payload = r#"{"version":1,"entries":[],"defaultPersonaId":null}"#;
        save_impl(&path, payload).unwrap();
        assert_eq!(load_impl(&path).unwrap().as_deref(), Some(payload));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_overwrites_previous_content_atomically() {
        let path = temp_file();
        save_impl(&path, "first").unwrap();
        save_impl(&path, "second").unwrap();
        assert_eq!(load_impl(&path).unwrap().as_deref(), Some("second"));
        // The temp file must not linger after a successful save.
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_file(&path);
    }

    /// Canonical fixture also parsed by `promptStore.test.ts` — pins the
    /// TS<->Rust schema against drift (the `providers_cli.rs`
    /// `APP_IDENTIFIER` drift risk the design doc calls out), since
    /// `PromptEntry` is read directly by `lm-cli` without going through the
    /// frontend at all.
    const CANONICAL_ENTRY_JSON: &str = r#"{
        "id": "11111111-1111-4111-8111-111111111111",
        "kind": "persona",
        "name": "Code Reviewer",
        "command": "code-reviewer",
        "content": "You are a meticulous code reviewer.",
        "description": "Reviews diffs for bugs",
        "createdAt": 1700000000000,
        "updatedAt": 1700000000000
    }"#;

    #[test]
    fn prompt_entry_deserializes_canonical_fixture() {
        let entry: PromptEntry = serde_json::from_str(CANONICAL_ENTRY_JSON).unwrap();
        assert_eq!(entry.id, "11111111-1111-4111-8111-111111111111");
        assert_eq!(entry.kind, "persona");
        assert_eq!(entry.name, "Code Reviewer");
        assert_eq!(entry.command, "code-reviewer");
        assert_eq!(entry.content, "You are a meticulous code reviewer.");
        assert_eq!(entry.description.as_deref(), Some("Reviews diffs for bugs"));
        assert_eq!(entry.created_at, 1700000000000);
        assert_eq!(entry.updated_at, 1700000000000);
    }

    #[test]
    fn read_external_returns_err_for_missing_file() {
        let path = temp_file();
        assert!(prompts_read_external(path.to_string_lossy().into_owned()).is_err());
    }

    #[test]
    fn write_then_read_external_roundtrips_arbitrary_path() {
        let path = temp_file();
        let payload = r#"{"version":1,"entries":[]}"#;
        prompts_write_external(path.to_string_lossy().into_owned(), payload.to_string()).unwrap();
        let read = prompts_read_external(path.to_string_lossy().into_owned()).unwrap();
        assert_eq!(read, payload);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_external_overwrites_existing_file() {
        let path = temp_file();
        prompts_write_external(path.to_string_lossy().into_owned(), "first".to_string()).unwrap();
        prompts_write_external(path.to_string_lossy().into_owned(), "second".to_string()).unwrap();
        assert_eq!(
            prompts_read_external(path.to_string_lossy().into_owned()).unwrap(),
            "second"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn prompt_entry_tolerates_missing_optional_fields() {
        // A hand-edited or partially-imported entry missing everything but
        // the required shape must still parse, not error out — same
        // leniency stance as `normalizeEntry` on the frontend.
        let entry: PromptEntry = serde_json::from_str(r#"{"id":"x","kind":"snippet"}"#).unwrap();
        assert_eq!(entry.id, "x");
        assert_eq!(entry.kind, "snippet");
        assert_eq!(entry.name, "");
        assert_eq!(entry.command, "");
        assert_eq!(entry.content, "");
        assert_eq!(entry.description, None);
        assert_eq!(entry.created_at, 0);
        assert_eq!(entry.updated_at, 0);
    }
}
