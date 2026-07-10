//! Persistent per-project "remembered facts": structured app-data JSON at
//! `<app_data>/memories.json`, distinct from the plain-markdown `rules.rs`
//! module (see that module's docs for why the two stores are deliberately
//! separate homes).
//!
//! Schema: `{ "version": 1, "projects": { "<canonical primary root path>":
//! { "facts": [ { "id", "text", "source": "agent"|"user", "created_at" } ] }
//! } }`, keyed by the canonical primary-root path (no hashing needed — one
//! file, atomic temp+rename writes exactly like `sessions.rs`'s `save_to`).
//! Caps are enforced here, Rust-side, so both `tool_remember` and (in a later
//! slice) `lm-cli` share the same guarantees: [`MAX_FACTS_PER_PROJECT`] facts
//! per project, [`MAX_FACT_CHARS`] characters per fact, and an exact-duplicate
//! fact text is treated as an already-successful remember (silent success —
//! the existing fact is returned, not a second copy or an error).
//!
//! Follows the `checkpoints.rs`/`sessions.rs`/`rules.rs` AppHandle-free
//! `*_impl` split: [`load_impl`]/[`save_impl`]/[`add_fact_impl`]/
//! [`delete_fact_impl`] take plain paths so they're directly unit-testable
//! and reusable from `lm-cli` (slice 5), while `memory_list`/`memory_add`/
//! `memory_delete` are the thin `#[tauri::command]` wrappers.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use tauri::Manager;

use crate::{workspace, AppState};

const MEMORIES_FILE: &str = "memories.json";

/// Current (and, so far, only) on-disk schema version.
const SCHEMA_VERSION: u8 = 1;

/// Per-project fact cap enforced by [`add_fact_impl`] — see module docs.
const MAX_FACTS_PER_PROJECT: usize = 100;

/// Per-fact character cap enforced by [`add_fact_impl`] — see module docs.
const MAX_FACT_CHARS: usize = 500;

/// One durable fact remembered for a project.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Fact {
    pub id: String,
    pub text: String,
    /// `"agent"` (saved by the `remember` tool) or `"user"` (saved by hand
    /// via the Settings "Add fact" affordance, slice 4).
    pub source: String,
    pub created_at: String,
}

/// One project's remembered facts.
#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
pub struct ProjectMemory {
    #[serde(default)]
    pub facts: Vec<Fact>,
}

/// The whole on-disk `memories.json` document.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct MemoriesFile {
    pub version: u8,
    #[serde(default)]
    pub projects: HashMap<String, ProjectMemory>,
}

impl Default for MemoriesFile {
    fn default() -> Self {
        MemoriesFile { version: SCHEMA_VERSION, projects: HashMap::new() }
    }
}

pub(crate) fn memories_file_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {}", e))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create app data dir: {}", e))?;
    Ok(dir.join(MEMORIES_FILE))
}

/// Core load logic, parameterized by path for testability. A missing file
/// (nothing saved yet — the common case) is simply the empty default, never
/// an error.
pub fn load_impl(path: &Path) -> Result<MemoriesFile, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(|e| format!("Corrupt memories file: {}", e)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(MemoriesFile::default()),
        Err(e) => Err(format!("Failed to read memories file: {}", e)),
    }
}

/// Core save logic: atomic sibling temp file + rename, same idiom as
/// `sessions.rs`'s `save_to`, so a crash mid-write can never leave a
/// truncated/corrupt memories file behind.
pub fn save_impl(path: &Path, memories: &MemoriesFile) -> Result<(), String> {
    let payload = serde_json::to_string_pretty(memories)
        .map_err(|e| format!("Failed to serialize memories: {}", e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &payload).map_err(|e| format!("Failed to write memories file: {}", e))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("Failed to finalize memories file: {}", e))?;
    Ok(())
}

/// Formats the current time as an RFC 3339 UTC timestamp (e.g.
/// `"2026-07-10T12:34:56.789Z"`) without pulling in a date/time crate for
/// this one field — see [`civil_from_days`] for the day-count-to-calendar-date
/// conversion this relies on.
fn now_rfc3339() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let millis = dur.subsec_millis();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year,
        month,
        day,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        millis
    )
}

/// Converts a day count since the Unix epoch (1970-01-01 UTC) to a
/// `(year, month, day)` civil (Gregorian) date. Howard Hinnant's well-known
/// constant-time algorithm (public domain) — used instead of a date/time
/// crate dependency for this one timestamp field.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

/// Core add-fact logic behind `tool_remember`/`memory_add`, parameterized by
/// plain path + root string so it's directly unit-testable and reusable from
/// `lm-cli`. Validates the cap/length rules described in the module docs:
/// - An exact-duplicate `text` already recorded for `root` is a silent
///   success — the existing fact is returned rather than erroring or
///   inserting a second copy (weak models retrying "did that stick?" must
///   not spam duplicates).
/// - Over [`MAX_FACT_CHARS`] characters, or a project already at
///   [`MAX_FACTS_PER_PROJECT`] facts, is a real error the caller (and, for
///   `tool_remember`, the model) should see and react to.
pub fn add_fact_impl(path: &Path, root: &str, text: &str, source: &str) -> Result<Fact, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("Fact text must not be empty".to_string());
    }
    if trimmed.chars().count() > MAX_FACT_CHARS {
        return Err(format!(
            "Fact text is {} characters, over the {}-character limit — shorten it.",
            trimmed.chars().count(),
            MAX_FACT_CHARS
        ));
    }

    let mut memories = load_impl(path)?;
    let project = memories.projects.entry(root.to_string()).or_default();

    if let Some(existing) = project.facts.iter().find(|f| f.text == trimmed) {
        return Ok(existing.clone());
    }

    if project.facts.len() >= MAX_FACTS_PER_PROJECT {
        return Err(format!(
            "This project already has {} remembered facts (the limit) — forget one before adding another.",
            MAX_FACTS_PER_PROJECT
        ));
    }

    let fact = Fact {
        id: uuid::Uuid::new_v4().to_string(),
        text: trimmed.to_string(),
        source: source.to_string(),
        created_at: now_rfc3339(),
    };
    project.facts.push(fact.clone());
    save_impl(path, &memories)?;
    Ok(fact)
}

/// Core delete-fact logic behind `memory_delete`. Removing an id that isn't
/// present (already forgotten, or from a different project) is a no-op
/// success rather than an error — the caller's desired end state (the fact
/// is gone) already holds.
pub fn delete_fact_impl(path: &Path, root: &str, id: &str) -> Result<(), String> {
    let mut memories = load_impl(path)?;
    if let Some(project) = memories.projects.get_mut(root) {
        let before = project.facts.len();
        project.facts.retain(|f| f.id != id);
        if project.facts.len() != before {
            save_impl(path, &memories)?;
        }
    }
    Ok(())
}

/// Facts remembered for the current primary workspace root. Never fails just
/// because no workspace is open yet (mirrors `rules_read`'s "no workspace =
/// no project-scope entries" tolerance) — this is called once per turn via
/// `rulesStore.refresh()`, and a missing workspace must not block a turn.
#[tauri::command]
pub fn memory_list(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<Vec<Fact>, String> {
    let Some(root) = workspace::primary_root_canon(state.inner()).ok() else {
        return Ok(Vec::new());
    };
    let memories = load_impl(&memories_file_path(&app)?)?;
    Ok(memories
        .projects
        .get(&root.to_string_lossy().to_string())
        .map(|p| p.facts.clone())
        .unwrap_or_default())
}

/// Manually add a fact for the current primary root — the Settings "Add
/// fact" affordance (slice 4). Always recorded with `source: "user"`;
/// `tool_remember` (agent-initiated) calls [`add_fact_impl`] directly with
/// `source: "agent"` instead of going through this command. Serialized
/// against concurrent `tool_remember` calls via `AppState::memory_lock`.
#[tauri::command]
pub fn memory_add(app: tauri::AppHandle, state: tauri::State<'_, AppState>, text: String) -> Result<Fact, String> {
    let root = workspace::primary_root_canon(state.inner())?;
    let _lock = state.memory_lock.lock().map_err(|_| "Memory lock poisoned".to_string())?;
    add_fact_impl(&memories_file_path(&app)?, &root.to_string_lossy(), &text, "user")
}

/// Delete a fact by id for the current primary root — used by the
/// transcript's "Forget" button (see `MessageList.tsx`'s `MemoryRow`) and, in
/// slice 4, the Settings fact list. A direct, human-initiated UI action —
/// like `rules_write`/`checkpoint_revert`, intentionally NOT routed through
/// `permissions::request_permission`.
#[tauri::command]
pub fn memory_delete(app: tauri::AppHandle, state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    let root = workspace::primary_root_canon(state.inner())?;
    let _lock = state.memory_lock.lock().map_err(|_| "Memory lock poisoned".to_string())?;
    delete_fact_impl(&memories_file_path(&app)?, &root.to_string_lossy(), &id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_path() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "little_monkey_memory_test_{}_{}_{}.json",
            std::process::id(),
            n,
            nanos
        ))
    }

    #[test]
    fn load_returns_default_when_file_missing() {
        let path = temp_path();
        let memories = load_impl(&path).unwrap();
        assert_eq!(memories.version, SCHEMA_VERSION);
        assert!(memories.projects.is_empty());
    }

    #[test]
    fn add_then_load_roundtrips_and_persists_atomically() {
        let path = temp_path();
        let fact = add_fact_impl(&path, "/ws/project", "Uses pnpm, not npm.", "agent").unwrap();

        assert_eq!(fact.text, "Uses pnpm, not npm.");
        assert_eq!(fact.source, "agent");
        assert!(!fact.id.is_empty());
        assert!(!fact.created_at.is_empty());
        assert!(!path.with_extension("json.tmp").exists(), "temp file must not linger");

        let reloaded = load_impl(&path).unwrap();
        let facts = &reloaded.projects["/ws/project"].facts;
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].id, fact.id);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn exact_duplicate_text_is_a_silent_success_not_a_second_copy() {
        let path = temp_path();
        let first = add_fact_impl(&path, "/ws/project", "Build with `make build`.", "agent").unwrap();
        let second = add_fact_impl(&path, "/ws/project", "Build with `make build`.", "agent").unwrap();

        assert_eq!(first.id, second.id, "duplicate remember must return the existing fact, not a new one");
        assert_eq!(load_impl(&path).unwrap().projects["/ws/project"].facts.len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fact_over_the_char_cap_is_rejected() {
        let path = temp_path();
        let huge = "a".repeat(MAX_FACT_CHARS + 1);
        let err = add_fact_impl(&path, "/ws/project", &huge, "agent").unwrap_err();
        assert!(err.contains("character limit"), "unexpected error: {err}");
    }

    #[test]
    fn empty_fact_text_is_rejected() {
        let path = temp_path();
        let err = add_fact_impl(&path, "/ws/project", "   ", "agent").unwrap_err();
        assert!(err.contains("must not be empty"), "unexpected error: {err}");
    }

    #[test]
    fn project_at_the_fact_cap_rejects_a_new_distinct_fact() {
        let path = temp_path();
        for n in 0..MAX_FACTS_PER_PROJECT {
            add_fact_impl(&path, "/ws/project", &format!("fact number {n}"), "agent").unwrap();
        }

        let err = add_fact_impl(&path, "/ws/project", "one fact too many", "agent").unwrap_err();
        assert!(err.contains("100"), "unexpected error: {err}");
        assert_eq!(
            load_impl(&path).unwrap().projects["/ws/project"].facts.len(),
            MAX_FACTS_PER_PROJECT
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn facts_are_scoped_per_project_root() {
        let path = temp_path();
        add_fact_impl(&path, "/ws/a", "fact for a", "agent").unwrap();
        add_fact_impl(&path, "/ws/b", "fact for b", "agent").unwrap();

        let memories = load_impl(&path).unwrap();
        assert_eq!(memories.projects["/ws/a"].facts.len(), 1);
        assert_eq!(memories.projects["/ws/b"].facts.len(), 1);
        assert_eq!(memories.projects["/ws/a"].facts[0].text, "fact for a");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn delete_removes_only_the_matching_fact_in_its_own_project() {
        let path = temp_path();
        let a = add_fact_impl(&path, "/ws/project", "keep me", "agent").unwrap();
        let b = add_fact_impl(&path, "/ws/project", "forget me", "agent").unwrap();

        delete_fact_impl(&path, "/ws/project", &b.id).unwrap();

        let facts = load_impl(&path).unwrap().projects["/ws/project"].facts.clone();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].id, a.id);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn delete_of_unknown_id_is_a_no_op_success() {
        let path = temp_path();
        add_fact_impl(&path, "/ws/project", "stays put", "agent").unwrap();

        delete_fact_impl(&path, "/ws/project", "00000000-0000-0000-0000-000000000000").unwrap();

        assert_eq!(load_impl(&path).unwrap().projects["/ws/project"].facts.len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn timestamps_round_trip_a_few_known_epoch_days() {
        // 1970-01-01 (epoch day 0) and 2000-03-01 (a well-known leap-year
        // boundary case for the civil_from_days algorithm) both must convert
        // correctly, or every fact's created_at would be silently wrong.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
    }
}
