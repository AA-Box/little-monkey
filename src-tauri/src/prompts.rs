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
//! (not private) and a `pub PromptEntry` struct is exposed too, so `monkey-cli`
//! can read personas straight out of this module without an `AppHandle` —
//! same seam as `checkpoints.rs`/`rules.rs` already provide for CLI reuse.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::profiles::ProfileScopedPaths;
use serde::{Deserialize, Serialize};
use tauri::Emitter;

use crate::config_revisions::{self, RecordRequest};

const PROMPTS_FILE: &str = "prompts.json";

/// Revision kind for the library as a whole — one head that every window's
/// save is checked against, which is what makes a concurrent edit detectable
/// (roadmap K24). Per-entry history lives under [`ENTRY_REVISION_KIND`].
pub const LIBRARY_REVISION_KIND: &str = "prompt-library";
/// The library is a single document, so it needs exactly one entity id.
pub const LIBRARY_REVISION_ENTITY: &str = "library";
/// Revision kind for one persona/snippet/skill, keyed by the entry's id. This
/// is what the Prompts tab's per-row History button reads.
pub const ENTRY_REVISION_KIND: &str = "prompt";

/// Serializes check-then-write across windows in this process: the
/// conflict check, the blob write, and the revision append must not
/// interleave with another save, or two windows could both see the same head
/// and both "win".
static SAVE_LOCK: Mutex<()> = Mutex::new(());

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

fn app_data_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {}", e))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create app data dir: {}", e))?;
    Ok(dir)
}

fn prompts_file_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    Ok(app_data_dir(app)?.join(PROMPTS_FILE))
}

/// The fields of an entry a revision remembers, as stable pretty JSON.
///
/// Deliberately omits `id`/`createdAt`/`updatedAt`: a timestamp that moves on
/// every unrelated library write would make every save look like a change,
/// filling the history with revisions whose diff is one epoch number. What is
/// left is exactly what a user edits — and exactly what "restore this
/// revision" should put back.
pub fn entry_snapshot(entry: &PromptEntry) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "kind": entry.kind,
        "name": entry.name,
        "command": entry.command,
        "description": entry.description,
        "content": entry.content,
    }))
    .unwrap_or_default()
}

/// The library payload's entries, or an empty list when the blob is not the
/// shape this module knows. The frontend owns that schema, so an unrecognized
/// payload costs the per-entry history for that save — never the save itself.
fn entries_of(payload: &str) -> Vec<PromptEntry> {
    #[derive(Deserialize)]
    struct Blob {
        #[serde(default)]
        entries: Vec<PromptEntry>,
    }
    serde_json::from_str::<Blob>(payload)
        .map(|blob| blob.entries)
        .unwrap_or_default()
}

/// Records a revision for the library and for every entry whose authored
/// fields changed. Unchanged entries dedupe inside
/// [`config_revisions::record`], so a save touching one persona appends one
/// entry revision, not one per entry.
fn record_revisions(
    root: &Path,
    payload: &str,
    base_revision_id: Option<String>,
) -> Result<String, String> {
    // One id for the whole save — the library revision and every changed
    // entry's revision are one change, and `config_revisions::changes` can only
    // say so if they were written saying so.
    let change_id = config_revisions::new_change_id();
    let library = config_revisions::record(
        root,
        LIBRARY_REVISION_KIND,
        LIBRARY_REVISION_ENTITY,
        RecordRequest {
            branch: None,
            base_revision_id,
            label: "Saved".to_string(),
            content: payload.to_string(),
            change_id: Some(change_id.clone()),
        },
    )
    .map_err(|e| e.to_string())?;

    for entry in entries_of(payload) {
        if entry.id.is_empty() {
            continue;
        }
        let label = if entry.name.trim().is_empty() {
            "Edited".to_string()
        } else {
            format!("Edited {}", entry.name.trim())
        };
        // Best-effort: an entry whose snapshot is somehow unstorable must not
        // fail the library save that already succeeded.
        let _ = config_revisions::record(
            root,
            ENTRY_REVISION_KIND,
            &entry.id,
            RecordRequest {
                branch: None,
                base_revision_id: None,
                label,
                content: entry_snapshot(&entry),
                change_id: Some(change_id.clone()),
            },
        );
    }
    Ok(library.revision_id)
}

/// Core load logic, parameterized by path so it needs no `AppHandle` —
/// directly unit-testable and reusable from `monkey-cli`.
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
/// frontend) and record a revision of it.
///
/// `base_revision_id` is the library revision the caller last saw. Supplying
/// it opts into the concurrent-edit check: if another window (or the CLI)
/// saved since, the write is REFUSED with a `conflict:`-prefixed error and the
/// blob on disk is left untouched, instead of the last writer silently winning
/// — roadmap K24 / ROADMAP #3. Passing `None` keeps the old unconditional
/// behavior, which is what a first save, an import, and a restore want.
///
/// Returns the new library revision id, which the caller holds as its base for
/// the next save.
#[tauri::command]
pub fn prompts_save(
    app: tauri::AppHandle,
    window: tauri::Window,
    payload: String,
    base_revision_id: Option<String>,
) -> Result<String, String> {
    let root = config_revisions::revision_root(&app_data_dir(&app)?);
    let path = prompts_file_path(&app)?;
    let revision_id = {
        let _guard = SAVE_LOCK
            .lock()
            .map_err(|_| "prompt save lock poisoned".to_string())?;
        // Revision first: it is the only step that can reject the write, and
        // rejecting after the blob is already overwritten would defeat the
        // point of detecting the conflict at all.
        let revision_id = record_revisions(&root, &payload, base_revision_id)?;
        save_impl(&path, &payload)?;
        revision_id
    };
    // Best-effort fan-out to the other windows; the save itself succeeded.
    let _ = app.emit(PROMPTS_CHANGED_EVENT, window.label());
    Ok(revision_id)
}

/// The current library revision id, so a window that just hydrated from disk
/// knows what base to save against. `None` when nothing has been recorded yet
/// (a library saved before this feature existed, or a fresh install).
#[tauri::command]
pub fn prompts_current_revision(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let root = config_revisions::revision_root(&app_data_dir(&app)?);
    config_revisions::head(&root, LIBRARY_REVISION_KIND, LIBRARY_REVISION_ENTITY, None)
        .map(|head| head.map(|revision| revision.revision_id))
        .map_err(|e| e.to_string())
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

    /// Canonical fixture also read (from disk, not re-typed) by
    /// `promptStore.test.ts` — a single shared file, not two independently
    /// hand-maintained literals, is what actually pins the TS<->Rust schema
    /// against drift (the `providers_cli.rs` `APP_IDENTIFIER` drift risk the
    /// design doc calls out): if either language's serialization of
    /// `PromptEntry` changes shape, this test and `promptStore.test.ts`'s
    /// both read the exact same bytes, so only a genuine cross-language
    /// disagreement about the shared shape can make one fail without the
    /// other. Matters because `monkey-cli` reads `PromptEntry` directly out of
    /// `prompts.json` without going through the frontend at all.
    const CANONICAL_ENTRY_JSON: &str = include_str!("../fixtures/prompt-entry.canonical.json");

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

    fn temp_dir() -> PathBuf {
        let path = temp_file().with_extension("d");
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn library_payload(entries: &str) -> String {
        format!(r#"{{"version":1,"entries":[{entries}],"defaultPersonaId":null}}"#)
    }

    const ONE_ENTRY: &str =
        r#"{"id":"e1","kind":"persona","name":"Reviewer","command":"rev","content":"first"}"#;

    #[test]
    fn a_save_records_a_library_revision_and_one_per_entry() {
        let root = temp_dir();
        let payload = library_payload(ONE_ENTRY);
        record_revisions(&root, &payload, None).unwrap();
        assert_eq!(
            crate::config_revisions::history(
                &root,
                LIBRARY_REVISION_KIND,
                LIBRARY_REVISION_ENTITY,
                None
            )
            .unwrap()
            .len(),
            1
        );
        let entry_history =
            crate::config_revisions::history(&root, ENTRY_REVISION_KIND, "e1", None).unwrap();
        assert_eq!(entry_history.len(), 1);
        assert_eq!(entry_history[0].label, "Edited Reviewer");
    }

    #[test]
    fn re_saving_an_unchanged_entry_adds_no_entry_revision() {
        let root = temp_dir();
        let first = library_payload(ONE_ENTRY);
        record_revisions(&root, &first, None).unwrap();
        // A second entry appears; the first entry's authored fields did not
        // change, so only the library and the NEW entry gain a revision.
        let second = library_payload(&format!(
            r#"{ONE_ENTRY},{{"id":"e2","kind":"snippet","name":"Snip","command":"snip","content":"x"}}"#
        ));
        record_revisions(&root, &second, None).unwrap();
        assert_eq!(
            crate::config_revisions::history(&root, ENTRY_REVISION_KIND, "e1", None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            crate::config_revisions::history(&root, ENTRY_REVISION_KIND, "e2", None)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_stale_base_revision_refuses_the_library_save() {
        let root = temp_dir();
        let first = record_revisions(&root, &library_payload(ONE_ENTRY), None).unwrap();
        // Another window saves in between.
        record_revisions(&root, &library_payload(""), None).unwrap();
        let error = record_revisions(&root, &library_payload(ONE_ENTRY), Some(first)).unwrap_err();
        assert!(error.starts_with("conflict:"), "unexpected error: {error}");
    }

    #[test]
    fn an_unparseable_payload_still_records_the_library_revision() {
        let root = temp_dir();
        // The blob is frontend-owned; a shape this module doesn't know must
        // cost the per-entry history, never the save.
        record_revisions(&root, "not json at all", None).unwrap();
        assert_eq!(
            crate::config_revisions::history(
                &root,
                LIBRARY_REVISION_KIND,
                LIBRARY_REVISION_ENTITY,
                None
            )
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn an_entry_snapshot_ignores_ids_and_timestamps() {
        let base = PromptEntry {
            id: "e1".to_string(),
            kind: "persona".to_string(),
            name: "Reviewer".to_string(),
            command: "rev".to_string(),
            content: "body".to_string(),
            description: None,
            created_at: 1,
            updated_at: 2,
        };
        let moved = PromptEntry {
            id: "different".to_string(),
            created_at: 999,
            updated_at: 1000,
            ..base.clone()
        };
        assert_eq!(entry_snapshot(&base), entry_snapshot(&moved));
        let edited = PromptEntry {
            content: "changed".to_string(),
            ..base.clone()
        };
        assert_ne!(entry_snapshot(&base), entry_snapshot(&edited));
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
