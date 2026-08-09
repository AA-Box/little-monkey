//! Persistent per-project "remembered facts": structured app-data JSON at
//! `<app_data>/memories.json`, distinct from the plain-markdown `rules.rs`
//! module (see that module's docs for why the two stores are deliberately
//! separate homes).
//!
//! Schema: `{ "version": 1, "projects": { "<canonical primary root path>":
//! { "facts": [ { "id", "text", "source": "agent"|"user", "created_at",
//! "enabled", "source_turn_id" } ] } } }`, keyed by the canonical
//! primary-root path (no hashing needed — one file, atomic temp+rename
//! writes exactly like `sessions.rs`'s `save_to`). Caps are enforced here,
//! Rust-side, so both `tool_remember` and `monkey-cli` share the same
//! guarantees: [`MAX_FACTS_PER_PROJECT`] facts per project, [`MAX_FACT_CHARS`]
//! characters per fact, and an exact-duplicate fact text is treated as an
//! already-successful remember (silent success — the existing fact is
//! returned, not a second copy or an error).
//!
//! Follows the `checkpoints.rs`/`sessions.rs`/`rules.rs` AppHandle-free
//! `*_impl` split: [`load_impl`]/[`save_impl`]/[`add_fact_impl`]/
//! [`delete_fact_impl`]/[`list_impl`]/[`list_all_impl`]/[`import_impl`] take
//! plain paths so they're directly unit-testable and reusable from
//! `monkey-cli`, while `memory_list`/`memory_add`/`memory_delete`/etc. are
//! the thin `#[tauri::command]` wrappers.
//!
//! ## Memory Studio (ROADMAP.md "Memory Studio")
//!
//! This app's only durable memory store is the one described above — two
//! real scopes, `"global"` (applies to every project, keyed under
//! [`GLOBAL_SCOPE_KEY`]) and `"project"` (keyed by canonical workspace
//! root). ROADMAP.md's Memory Studio spec also names `"workspace"`,
//! `"user"`, `"device"`, and connector-derived memories, plus per-memory
//! confidence and a "last-used" timestamp — none of those exist in this
//! codebase: there is exactly one local app-data store per install (so
//! "user"/"device" collapse into the existing `"global"` scope), nothing
//! ever writes a memory tagged as coming from a specific workspace distinct
//! from a project root, no connector/tool attaches a confidence score to a
//! remembered fact, and no code path records when a fact was last actually
//! folded into a model prompt (see `systemPrompt.ts`). Memory Studio
//! (`memory_list_all` below, and the frontend panel that calls it) only
//! surfaces the two scopes and the fields that are real: `source`
//! (`"agent"` vs `"user"`), `source_turn_id` (set when `tool_remember` ran
//! inside a specific chat turn — `None` for Settings-entered facts), and
//! `created_at`. `enabled` (this slice's new soft-disable flag) is the one
//! genuinely new lifecycle field, and it is wired all the way through
//! [`list_impl`] — the exact function `memory_list` (and therefore
//! `rulesStore.facts` and `systemPrompt.ts`'s `factsLines`) calls — so a
//! disabled or deleted fact is provably excluded from every subsequent
//! prompt (see this module's tests).
//!
//! Export intentionally has no Rust-side command: `src/lib/memoryStudio.ts`'s
//! `exportMemories` fetches the full listing via `memory_list_all`, redacts
//! secret-shaped text with `redactSensitiveText` (`src/lib/durableRun.ts` —
//! the same function the run-capsule export and checkpoint evidence already
//! use), and writes the file itself via `@tauri-apps/plugin-fs`, mirroring
//! `RunCapsulePanel.tsx`'s `serializeRedactedRunCapsule` +
//! `writeTextFile` pattern instead of introducing a second, Rust-side
//! secret-scanner. [`import_impl`]/`memory_import` stay Rust-side because
//! restoring scope/dedup/caps genuinely needs [`add_fact_impl`]'s existing
//! validation — there is no frontend equivalent for "add a fact to an
//! arbitrary project scope" to build that from.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};


use crate::{workspace, AppState};
use crate::profiles::ProfileScopedPaths;

const MEMORIES_FILE: &str = "memories.json";

/// Reserved `projects` key for facts remembered while no workspace folder is
/// open (`tool_remember` used to hard-fail in that case — see
/// ROADMAP/session notes on the "remember with no workspace" bug). Never
/// collides with a real project root: canonicalized workspace roots are
/// always absolute filesystem paths, which this string is not. Global facts
/// are included in every `memory_list` call regardless of which (if any)
/// project is open, in addition to that project's own facts.
pub(crate) const GLOBAL_SCOPE_KEY: &str = "__global__";

/// Current (and, so far, only) on-disk schema version.
const SCHEMA_VERSION: u8 = 1;

/// Per-project fact cap enforced by [`add_fact_impl`] — see module docs.
const MAX_FACTS_PER_PROJECT: usize = 100;

/// Per-fact character cap enforced by [`add_fact_impl`] — see module docs.
const MAX_FACT_CHARS: usize = 500;

fn default_enabled() -> bool {
    true
}

/// One durable fact remembered for a project.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Fact {
    pub id: String,
    pub text: String,
    /// `"agent"` (saved by the `remember` tool) or `"user"` (saved by hand
    /// via the Settings "Add fact" affordance, slice 4).
    pub source: String,
    pub created_at: String,
    /// Soft-disable flag (Memory Studio slice). `true` unless a memory was
    /// explicitly disabled via `memory_studio_set_enabled`. [`list_impl`]
    /// filters on this — a disabled fact stays on disk (so it can be
    /// re-enabled) but is never returned to `memory_list`, and therefore
    /// never reaches `systemPrompt.ts`'s `factsLines`. Defaults to `true` on
    /// deserialize so `memories.json` files written before this field
    /// existed keep working.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// The chat turn id `tool_remember` was called from, when known — see
    /// `tools.rs::tool_remember`'s `turn_id` parameter. `None` for facts
    /// added via `memory_add` (the Settings "Add fact" affordance, which has
    /// no turn) and for anything remembered before this field existed.
    /// Genuine provenance, not fabricated: it's the same id the frontend
    /// agent loop already generates and threads through permission prompts
    /// for that turn, just persisted here too. There is no equivalent
    /// tracking for a source *file* or source *connector* — no tool in this
    /// codebase captures either when a fact is remembered, so Memory Studio
    /// does not display those fields (see module docs).
    #[serde(default)]
    pub source_turn_id: Option<String>,
}

/// One memory with its scope resolved — the shape Memory Studio's full
/// listing ([`list_all_impl`]/`memory_list_all`) and export/import work
/// with, as opposed to the bare [`Fact`] stored under a `MemoriesFile.projects`
/// key (scope is only implicit there, via which key a fact happens to live
/// under).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MemoryEntry {
    pub id: String,
    pub text: String,
    pub source: String,
    pub created_at: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub source_turn_id: Option<String>,
    /// `"global"` or `"project"` — the only two scopes this app's memory
    /// store can actually represent (see module docs). Never `"workspace"`,
    /// `"user"`, `"device"`, or `"connector"`.
    pub scope: String,
    /// `Some(canonical root path)` when `scope == "project"`, `None` when
    /// `scope == "global"`.
    #[serde(default)]
    pub project_root: Option<String>,
}

fn to_entry(root: &str, fact: &Fact) -> MemoryEntry {
    let (scope, project_root) = if root == GLOBAL_SCOPE_KEY {
        ("global".to_string(), None)
    } else {
        ("project".to_string(), Some(root.to_string()))
    };
    MemoryEntry {
        id: fact.id.clone(),
        text: fact.text.clone(),
        source: fact.source.clone(),
        created_at: fact.created_at.clone(),
        enabled: fact.enabled,
        source_turn_id: fact.source_turn_id.clone(),
        scope,
        project_root,
    }
}

/// One exported memory: the resolved [`MemoryEntry`] plus whether its `text`
/// was redacted before being written out. The redaction pass itself runs on
/// the frontend (`src/lib/memoryStudio.ts`'s `buildMemoryExport`, reusing
/// `redactSensitiveText` from `durableRun.ts`) — this struct only needs to
/// (de)serialize the resulting shape, for [`import_impl`] to read back.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MemoryExportEntry {
    #[serde(flatten)]
    pub entry: MemoryEntry,
    /// Whether `entry.text` had one or more secret-shaped spans masked out
    /// before this file was written.
    #[serde(default)]
    pub redacted: bool,
}

/// The whole portable export/import file shape written by
/// `memoryStudio.ts`'s `buildMemoryExport` and read back by
/// [`import_impl`]/`memory_import`.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct MemoryExportFile {
    pub version: u8,
    pub exported_at: String,
    /// Whether secret-shaped values were redacted before writing (the
    /// default) — carried in the file itself so a re-import (or a human
    /// reading the JSON) knows whether `text` values are safe to treat as
    /// verbatim.
    pub redacted: bool,
    pub entries: Vec<MemoryExportEntry>,
}

#[derive(serde::Serialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryImportSummary {
    pub added: usize,
    pub skipped_duplicate: usize,
    pub errors: Vec<String>,
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
        MemoriesFile {
            version: SCHEMA_VERSION,
            projects: HashMap::new(),
        }
    }
}

pub(crate) fn memories_file_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .profile_data_dir()
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

/// Truncates fact text for embedding in an import error message, so one
/// huge pasted fact can't blow up `MemoryImportSummary.errors`.
fn truncate_for_error(text: &str) -> String {
    let trimmed = text.trim();
    let mut out: String = trimmed.chars().take(40).collect();
    if trimmed.chars().count() > 40 {
        out.push('…');
    }
    out
}

/// Core add-fact logic behind `tool_remember`/`memory_add`, parameterized by
/// plain path + root string so it's directly unit-testable and reusable from
/// `monkey-cli`. Validates the cap/length rules described in the module docs:
/// - An exact-duplicate `text` already recorded for `root` is a silent
///   success — the existing fact is returned rather than erroring or
///   inserting a second copy (weak models retrying "did that stick?" must
///   not spam duplicates). The existing fact's `enabled`/`source_turn_id`
///   are left untouched in this case.
/// - Over [`MAX_FACT_CHARS`] characters, or a project already at
///   [`MAX_FACTS_PER_PROJECT`] facts, is a real error the caller (and, for
///   `tool_remember`, the model) should see and react to.
///
/// `source_turn_id` is the chat turn a fact was remembered from, when known
/// (see [`Fact::source_turn_id`]'s doc comment) — pass `None` for
/// Settings-entered facts and any other caller with no turn to attribute.
pub fn add_fact_impl(
    path: &Path,
    root: &str,
    text: &str,
    source: &str,
    source_turn_id: Option<&str>,
) -> Result<Fact, String> {
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
        enabled: true,
        source_turn_id: source_turn_id.map(|s| s.to_string()),
    };
    project.facts.push(fact.clone());
    save_impl(path, &memories)?;
    Ok(fact)
}

/// Core update-fact logic behind `memory_update`/`memory_studio_update` —
/// the Settings fact list's inline edit (slice 4) and Memory Studio's editor.
/// Validated the same way as [`add_fact_impl`] (non-empty, [`MAX_FACT_CHARS`]
/// cap); unlike `add_fact_impl` there is no duplicate-text short-circuit here
/// — editing fact A's text to match fact B's is a legitimate (if odd) user
/// action, not a retried remember.
pub fn update_fact_impl(path: &Path, root: &str, id: &str, text: &str) -> Result<Fact, String> {
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
    let updated = {
        let project = memories
            .projects
            .get_mut(root)
            .ok_or_else(|| "Fact not found — it may have already been forgotten.".to_string())?;
        let fact = project
            .facts
            .iter_mut()
            .find(|f| f.id == id)
            .ok_or_else(|| "Fact not found — it may have already been forgotten.".to_string())?;
        fact.text = trimmed.to_string();
        fact.clone()
    };
    save_impl(path, &memories)?;
    Ok(updated)
}

/// Core enable/disable logic behind `memory_studio_set_enabled` — Memory
/// Studio's soft-off toggle. A disabled fact is left on disk (recoverable —
/// re-enabling is one call away) but, via [`list_impl`], stops being
/// returned to `memory_list` and therefore never reaches a future prompt.
/// Mirrors [`update_fact_impl`]'s "fact not found" error for an unknown
/// `root`/`id` pair.
pub fn set_enabled_impl(path: &Path, root: &str, id: &str, enabled: bool) -> Result<Fact, String> {
    let mut memories = load_impl(path)?;
    let updated = {
        let project = memories
            .projects
            .get_mut(root)
            .ok_or_else(|| "Fact not found — it may have already been forgotten.".to_string())?;
        let fact = project
            .facts
            .iter_mut()
            .find(|f| f.id == id)
            .ok_or_else(|| "Fact not found — it may have already been forgotten.".to_string())?;
        fact.enabled = enabled;
        fact.clone()
    };
    save_impl(path, &memories)?;
    Ok(updated)
}

/// Core clear-all logic behind `memory_clear` — the Settings "Clear all"
/// button (slice 4). Drops every fact for `root` in one write; a project
/// with no facts yet (or already cleared) is a no-op success, mirroring
/// `delete_fact_impl`'s "already gone" tolerance. Other projects' facts are
/// untouched.
pub fn clear_impl(path: &Path, root: &str) -> Result<(), String> {
    let mut memories = load_impl(path)?;
    if memories.projects.remove(root).is_some() {
        save_impl(path, &memories)?;
    }
    Ok(())
}

/// Core delete-fact logic behind `memory_delete`/`memory_studio_delete`.
/// Removing an id that isn't present (already forgotten, or from a
/// different project) is a no-op success rather than an error — the
/// caller's desired end state (the fact is gone) already holds.
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

/// Core logic behind `memory_list`: every `enabled` global fact, plus every
/// `enabled` fact under `project_root` (if given). **This is the exact seam
/// where storage crosses into what a future model prompt can see** —
/// `rulesStore.refresh()` calls `memory_list` once per turn, stashes the
/// result as `facts`, and `systemPrompt.ts`'s `currentSystemPrompt` reads
/// that array straight into `factsLines`. A `enabled: false` (soft-disabled)
/// or fully deleted fact is filtered out right here, so it can never appear
/// in a subsequent prompt — see this module's
/// `disabled_and_deleted_facts_are_excluded_from_list_impl` test for a
/// direct proof. Directly unit-testable without an `AppHandle`/workspace,
/// unlike the `#[tauri::command]` wrapper below.
pub fn list_impl(path: &Path, project_root: Option<&str>) -> Result<Vec<Fact>, String> {
    let memories = load_impl(path)?;
    let mut facts: Vec<Fact> = memories
        .projects
        .get(GLOBAL_SCOPE_KEY)
        .map(|p| p.facts.iter().filter(|f| f.enabled).cloned().collect())
        .unwrap_or_default();
    if let Some(root) = project_root {
        if let Some(project) = memories.projects.get(root) {
            facts.extend(project.facts.iter().filter(|f| f.enabled).cloned());
        }
    }
    Ok(facts)
}

/// Core logic behind `memory_list_all` — Memory Studio's full browse view.
/// Unlike [`list_impl`] (prompt-facing: current project + global, enabled
/// only), this returns *every* fact ever recorded across *every* project
/// root the store has ever seen, enabled or not, so a user can find and
/// re-enable/delete/export a memory that belongs to a project that isn't
/// currently open. Sorted newest-first (by `created_at`, id as a stable
/// tiebreaker) so the most recent memories are the first thing Memory
/// Studio shows.
pub fn list_all_impl(path: &Path) -> Result<Vec<MemoryEntry>, String> {
    let memories = load_impl(path)?;
    let mut entries: Vec<MemoryEntry> = memories
        .projects
        .iter()
        .flat_map(|(root, project)| project.facts.iter().map(move |f| to_entry(root, f)))
        .collect();
    entries.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| a.id.cmp(&b.id)));
    Ok(entries)
}

fn fact_count(path: &Path, root: &str) -> usize {
    load_impl(path)
        .ok()
        .and_then(|m| m.projects.get(root).map(|p| p.facts.len()))
        .unwrap_or(0)
}

/// Core import logic behind `memory_import`. Re-adds each exported entry
/// through [`add_fact_impl`] (so the same length/cap/exact-duplicate rules
/// apply as any other remember), restoring its original scope
/// (`"global"`/`"project"` + `project_root`) and, when the entry was
/// recorded as disabled, re-disabling it after insert via
/// [`set_enabled_impl`] (`add_fact_impl` always inserts a fresh fact as
/// enabled, so a disabled memory would otherwise silently come back
/// enabled). An entry whose text exactly matches an already-stored fact in
/// the same scope is counted as `skipped_duplicate`, not `added` — importing
/// the same export twice is idempotent. An entry with an unknown/missing
/// scope, or one that fails `add_fact_impl`'s validation, is recorded in
/// `errors` and does not abort the rest of the import.
pub fn import_impl(path: &Path, entries: &[MemoryExportEntry]) -> MemoryImportSummary {
    let mut added = 0usize;
    let mut skipped_duplicate = 0usize;
    let mut errors = Vec::new();

    for wrapped in entries {
        let entry = &wrapped.entry;
        let root = match entry.scope.as_str() {
            "global" => GLOBAL_SCOPE_KEY.to_string(),
            "project" => match entry.project_root.as_deref() {
                Some(r) if !r.is_empty() => r.to_string(),
                _ => {
                    errors.push(format!(
                        "\"{}\": project-scoped entry is missing project_root",
                        truncate_for_error(&entry.text)
                    ));
                    continue;
                }
            },
            other => {
                errors.push(format!(
                    "\"{}\": unknown scope \"{}\"",
                    truncate_for_error(&entry.text),
                    other
                ));
                continue;
            }
        };

        let before = fact_count(path, &root);
        match add_fact_impl(
            path,
            &root,
            &entry.text,
            &entry.source,
            entry.source_turn_id.as_deref(),
        ) {
            Ok(fact) => {
                let after = fact_count(path, &root);
                if after > before {
                    added += 1;
                    if !entry.enabled {
                        let _ = set_enabled_impl(path, &root, &fact.id, false);
                    }
                } else {
                    skipped_duplicate += 1;
                }
            }
            Err(e) => errors.push(format!("\"{}\": {}", truncate_for_error(&entry.text), e)),
        }
    }

    MemoryImportSummary {
        added,
        skipped_duplicate,
        errors,
    }
}

/// Facts remembered for the current primary workspace root, plus any
/// [`GLOBAL_SCOPE_KEY`] facts saved while no workspace was open — those are
/// always visible so a fact like "the user's name is Ahmad" recalls
/// regardless of which (if any) project is open. Only `enabled` facts are
/// returned (see [`list_impl`]'s doc comment — this is the prompt-facing
/// read). Never fails just because no workspace is open yet (mirrors
/// `rules_read`'s "no workspace = no project-scope entries" tolerance) —
/// this is called once per turn via `rulesStore.refresh()`, and a missing
/// workspace must not block a turn.
#[tauri::command]
pub fn memory_list(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<Fact>, String> {
    let root = workspace::primary_root_canon(state.inner())
        .ok()
        .map(|p| p.to_string_lossy().to_string());
    list_impl(&memories_file_path(&app)?, root.as_deref())
}

/// Every memory ever recorded, across every project root and the global
/// scope, enabled or disabled — Memory Studio's full listing. See
/// [`list_all_impl`].
#[tauri::command]
pub fn memory_list_all(app: tauri::AppHandle) -> Result<Vec<MemoryEntry>, String> {
    list_all_impl(&memories_file_path(&app)?)
}

/// The candidate scope keys a fact reachable from the current app state could
/// live under: [`GLOBAL_SCOPE_KEY`] always, plus the primary workspace root
/// when one is open. Used by [`memory_delete`]/[`memory_update`] to find
/// which bucket actually holds a given fact id, since `memory_list` merges
/// both scopes but a mutation has to target the right one underneath.
fn candidate_scopes(state: &AppState) -> Vec<String> {
    let mut scopes = vec![GLOBAL_SCOPE_KEY.to_string()];
    if let Ok(root) = workspace::primary_root_canon(state) {
        scopes.push(root.to_string_lossy().to_string());
    }
    scopes
}

/// Which of `scopes` currently holds a fact with `id`, if any.
fn owning_scope(memories: &MemoriesFile, scopes: &[String], id: &str) -> Option<String> {
    scopes
        .iter()
        .find(|scope| {
            memories
                .projects
                .get(*scope)
                .is_some_and(|p| p.facts.iter().any(|f| f.id == id))
        })
        .cloned()
}

/// Manually add a fact for the current primary root — the Settings "Add
/// fact" affordance (slice 4). Always recorded with `source: "user"` and no
/// `source_turn_id` (there is no chat turn to attribute — see
/// [`Fact::source_turn_id`]); `tool_remember` (agent-initiated) calls
/// [`add_fact_impl`] directly with `source: "agent"` and its turn id instead
/// of going through this command. Serialized against concurrent
/// `tool_remember` calls via `AppState::memory_lock`.
#[tauri::command]
pub fn memory_add(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    text: String,
) -> Result<Fact, String> {
    let root = workspace::primary_root_canon(state.inner())?;
    let _lock = state
        .memory_lock
        .lock()
        .map_err(|_| "Memory lock poisoned".to_string())?;
    add_fact_impl(
        &memories_file_path(&app)?,
        &root.to_string_lossy(),
        &text,
        "user",
        None,
    )
}

/// Delete a fact by id — used by the transcript's "Forget" button (see
/// `MessageList.tsx`'s `MemoryRow`) and, in slice 4, the Settings fact list.
/// Searches [`GLOBAL_SCOPE_KEY`] and the current primary root (whichever
/// actually holds `id` — `memory_list` merges both scopes, so a fact shown to
/// the user may live in either) rather than assuming the project root, since
/// a global fact must still be forgettable while a project happens to be
/// open. A direct, human-initiated UI action — like
/// `rules_write`/`checkpoint_revert`, intentionally NOT routed through
/// `permissions::request_permission`. For deleting a memory that belongs to
/// a project other than the one currently open, see `memory_studio_delete`.
#[tauri::command]
pub fn memory_delete(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let path = memories_file_path(&app)?;
    let _lock = state
        .memory_lock
        .lock()
        .map_err(|_| "Memory lock poisoned".to_string())?;
    let memories = load_impl(&path)?;
    let scopes = candidate_scopes(state.inner());
    match owning_scope(&memories, &scopes, &id) {
        Some(scope) => delete_fact_impl(&path, &scope, &id),
        None => Ok(()), // already gone from every reachable scope — no-op success, same as delete_fact_impl
    }
}

/// Edit a fact's text in place — the Settings fact list's inline edit (slice
/// 4). Preserves the fact's `id`, `source`, `created_at`, `enabled`, and
/// `source_turn_id`; only `text` changes. Searches [`GLOBAL_SCOPE_KEY`] and
/// the current primary root the same way [`memory_delete`] does, so editing
/// a global fact works regardless of which project (if any) is open. For
/// editing a memory that belongs to a project other than the one currently
/// open, see `memory_studio_update`.
#[tauri::command]
pub fn memory_update(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
    text: String,
) -> Result<Fact, String> {
    let path = memories_file_path(&app)?;
    let _lock = state
        .memory_lock
        .lock()
        .map_err(|_| "Memory lock poisoned".to_string())?;
    let memories = load_impl(&path)?;
    let scopes = candidate_scopes(state.inner());
    let scope = owning_scope(&memories, &scopes, &id)
        .ok_or_else(|| "Fact not found — it may have already been forgotten.".to_string())?;
    update_fact_impl(&path, &scope, &id, &text)
}

/// Delete every remembered fact for the current primary root — the Settings
/// "Clear all" button (slice 4), gated behind a confirm step in the UI since
/// it's destructive (though app-local and non-catastrophic: nothing else
/// depends on facts persisting, and re-remembering is one tool call away).
#[tauri::command]
pub fn memory_clear(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let root = workspace::primary_root_canon(state.inner())?;
    let _lock = state
        .memory_lock
        .lock()
        .map_err(|_| "Memory lock poisoned".to_string())?;
    clear_impl(&memories_file_path(&app)?, &root.to_string_lossy())
}

/// Resolves Memory Studio's `project_root: Option<String>` parameter (`None`
/// = the global scope) to the raw storage key `load_impl`/`save_impl` use
/// underneath — keeps [`GLOBAL_SCOPE_KEY`]'s sentinel string an
/// implementation detail the frontend never has to know about.
fn studio_scope(project_root: Option<String>) -> String {
    project_root.unwrap_or_else(|| GLOBAL_SCOPE_KEY.to_string())
}

/// Edit a memory that Memory Studio is showing, regardless of which project
/// (if any) is currently open — unlike [`memory_update`], which only
/// searches the global scope and the *current* primary root, this trusts the
/// `project_root` Memory Studio already knows from `memory_list_all`'s
/// result (so it can edit a memory belonging to a project that isn't open
/// right now).
#[tauri::command]
pub fn memory_studio_update(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
    project_root: Option<String>,
    text: String,
) -> Result<Fact, String> {
    let path = memories_file_path(&app)?;
    let _lock = state
        .memory_lock
        .lock()
        .map_err(|_| "Memory lock poisoned".to_string())?;
    update_fact_impl(&path, &studio_scope(project_root), &id, &text)
}

/// Enable/disable a memory that Memory Studio is showing — the "disable
/// (soft-off without deleting)" action from ROADMAP.md's Memory Studio spec.
/// See [`set_enabled_impl`] for why this is what actually keeps a disabled
/// memory out of future prompts.
#[tauri::command]
pub fn memory_studio_set_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
    project_root: Option<String>,
    enabled: bool,
) -> Result<Fact, String> {
    let path = memories_file_path(&app)?;
    let _lock = state
        .memory_lock
        .lock()
        .map_err(|_| "Memory lock poisoned".to_string())?;
    set_enabled_impl(&path, &studio_scope(project_root), &id, enabled)
}

/// Delete a memory that Memory Studio is showing, regardless of which
/// project (if any) is currently open — see [`memory_studio_update`]'s doc
/// comment for why this differs from [`memory_delete`].
#[tauri::command]
pub fn memory_studio_delete(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
    project_root: Option<String>,
) -> Result<(), String> {
    let path = memories_file_path(&app)?;
    let _lock = state
        .memory_lock
        .lock()
        .map_err(|_| "Memory lock poisoned".to_string())?;
    delete_fact_impl(&path, &studio_scope(project_root), &id)
}

/// Import memories from a file previously written by `memoryStudio.ts`'s
/// `exportMemories`. See [`import_impl`].
#[tauri::command]
pub fn memory_import(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<MemoryImportSummary, String> {
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("Failed to read import file: {}", e))?;
    let file: MemoryExportFile = serde_json::from_str(&raw)
        .map_err(|e| format!("That file doesn't look like a Memory Studio export: {}", e))?;
    let store_path = memories_file_path(&app)?;
    let _lock = state
        .memory_lock
        .lock()
        .map_err(|_| "Memory lock poisoned".to_string())?;
    Ok(import_impl(&store_path, &file.entries))
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

    /// Shared with `rulesStore.test.ts`'s canonical-fixture test (which reads
    /// the same file via a JSON import) — a single fixture, not two
    /// independently hand-typed literals, is what actually pins the
    /// TS<->Rust `Fact` schema against drift, per ROADMAP.md §3.6.
    const CANONICAL_FACT_JSON: &str = include_str!("../fixtures/memory-fact.canonical.json");

    #[test]
    fn fact_deserializes_canonical_fixture() {
        let fact: Fact = serde_json::from_str(CANONICAL_FACT_JSON).unwrap();
        assert_eq!(fact.id, "22222222-2222-4222-8222-222222222222");
        assert_eq!(
            fact.text,
            "This project uses pnpm, not npm, for all package management."
        );
        assert_eq!(fact.source, "agent");
        assert_eq!(fact.created_at, "2026-01-01T00:00:00.000Z");
        assert!(fact.enabled);
        assert_eq!(fact.source_turn_id, None);
    }

    #[test]
    fn fact_without_enabled_or_turn_id_defaults_to_enabled_true_none(
    ) {
        // A `memories.json` written before this slice never had `enabled`/
        // `source_turn_id` at all — must still deserialize, defaulting to
        // "enabled" (never silently disable pre-existing facts) and no turn.
        let legacy = r#"{"id":"1","text":"legacy fact","source":"agent","created_at":"2026-01-01T00:00:00.000Z"}"#;
        let fact: Fact = serde_json::from_str(legacy).unwrap();
        assert!(fact.enabled);
        assert_eq!(fact.source_turn_id, None);
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
        let fact = add_fact_impl(&path, "/ws/project", "Uses pnpm, not npm.", "agent", None).unwrap();

        assert_eq!(fact.text, "Uses pnpm, not npm.");
        assert_eq!(fact.source, "agent");
        assert!(!fact.id.is_empty());
        assert!(!fact.created_at.is_empty());
        assert!(fact.enabled);
        assert!(
            !path.with_extension("json.tmp").exists(),
            "temp file must not linger"
        );

        let reloaded = load_impl(&path).unwrap();
        let facts = &reloaded.projects["/ws/project"].facts;
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].id, fact.id);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn add_fact_records_the_given_source_turn_id() {
        let path = temp_path();
        let fact = add_fact_impl(&path, "/ws/project", "turn-scoped fact", "agent", Some("turn-42"))
            .unwrap();
        assert_eq!(fact.source_turn_id.as_deref(), Some("turn-42"));

        let reloaded = load_impl(&path).unwrap();
        assert_eq!(
            reloaded.projects["/ws/project"].facts[0]
                .source_turn_id
                .as_deref(),
            Some("turn-42")
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn exact_duplicate_text_is_a_silent_success_not_a_second_copy() {
        let path = temp_path();
        let first =
            add_fact_impl(&path, "/ws/project", "Build with `make build`.", "agent", None).unwrap();
        let second =
            add_fact_impl(&path, "/ws/project", "Build with `make build`.", "agent", None).unwrap();

        assert_eq!(
            first.id, second.id,
            "duplicate remember must return the existing fact, not a new one"
        );
        assert_eq!(
            load_impl(&path).unwrap().projects["/ws/project"]
                .facts
                .len(),
            1
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fact_over_the_char_cap_is_rejected() {
        let path = temp_path();
        let huge = "a".repeat(MAX_FACT_CHARS + 1);
        let err = add_fact_impl(&path, "/ws/project", &huge, "agent", None).unwrap_err();
        assert!(err.contains("character limit"), "unexpected error: {err}");
    }

    #[test]
    fn empty_fact_text_is_rejected() {
        let path = temp_path();
        let err = add_fact_impl(&path, "/ws/project", "   ", "agent", None).unwrap_err();
        assert!(err.contains("must not be empty"), "unexpected error: {err}");
    }

    #[test]
    fn project_at_the_fact_cap_rejects_a_new_distinct_fact() {
        let path = temp_path();
        for n in 0..MAX_FACTS_PER_PROJECT {
            add_fact_impl(&path, "/ws/project", &format!("fact number {n}"), "agent", None).unwrap();
        }

        let err = add_fact_impl(&path, "/ws/project", "one fact too many", "agent", None).unwrap_err();
        assert!(err.contains("100"), "unexpected error: {err}");
        assert_eq!(
            load_impl(&path).unwrap().projects["/ws/project"]
                .facts
                .len(),
            MAX_FACTS_PER_PROJECT
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn facts_are_scoped_per_project_root() {
        let path = temp_path();
        add_fact_impl(&path, "/ws/a", "fact for a", "agent", None).unwrap();
        add_fact_impl(&path, "/ws/b", "fact for b", "agent", None).unwrap();

        let memories = load_impl(&path).unwrap();
        assert_eq!(memories.projects["/ws/a"].facts.len(), 1);
        assert_eq!(memories.projects["/ws/b"].facts.len(), 1);
        assert_eq!(memories.projects["/ws/a"].facts[0].text, "fact for a");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn delete_removes_only_the_matching_fact_in_its_own_project() {
        let path = temp_path();
        let a = add_fact_impl(&path, "/ws/project", "keep me", "agent", None).unwrap();
        let b = add_fact_impl(&path, "/ws/project", "forget me", "agent", None).unwrap();

        delete_fact_impl(&path, "/ws/project", &b.id).unwrap();

        let facts = load_impl(&path).unwrap().projects["/ws/project"]
            .facts
            .clone();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].id, a.id);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn delete_of_unknown_id_is_a_no_op_success() {
        let path = temp_path();
        add_fact_impl(&path, "/ws/project", "stays put", "agent", None).unwrap();

        delete_fact_impl(&path, "/ws/project", "00000000-0000-0000-0000-000000000000").unwrap();

        assert_eq!(
            load_impl(&path).unwrap().projects["/ws/project"]
                .facts
                .len(),
            1
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_changes_text_but_preserves_id_source_and_created_at() {
        let path = temp_path();
        let original = add_fact_impl(&path, "/ws/project", "uses npm", "agent", None).unwrap();

        let updated =
            update_fact_impl(&path, "/ws/project", &original.id, "uses pnpm, not npm").unwrap();

        assert_eq!(updated.id, original.id);
        assert_eq!(updated.source, original.source);
        assert_eq!(updated.created_at, original.created_at);
        assert_eq!(updated.text, "uses pnpm, not npm");

        let reloaded = load_impl(&path).unwrap();
        assert_eq!(
            reloaded.projects["/ws/project"].facts[0].text,
            "uses pnpm, not npm"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_of_unknown_id_is_an_error() {
        let path = temp_path();
        add_fact_impl(&path, "/ws/project", "stays put", "agent", None).unwrap();

        let err = update_fact_impl(
            &path,
            "/ws/project",
            "00000000-0000-0000-0000-000000000000",
            "new text",
        )
        .unwrap_err();
        assert!(err.contains("not found"), "unexpected error: {err}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_rejects_empty_text_and_over_cap_text() {
        let path = temp_path();
        let fact = add_fact_impl(&path, "/ws/project", "original", "agent", None).unwrap();

        let empty_err = update_fact_impl(&path, "/ws/project", &fact.id, "   ").unwrap_err();
        assert!(
            empty_err.contains("must not be empty"),
            "unexpected error: {empty_err}"
        );

        let huge = "a".repeat(MAX_FACT_CHARS + 1);
        let cap_err = update_fact_impl(&path, "/ws/project", &fact.id, &huge).unwrap_err();
        assert!(
            cap_err.contains("character limit"),
            "unexpected error: {cap_err}"
        );

        // Neither rejected edit should have changed the stored text.
        assert_eq!(
            load_impl(&path).unwrap().projects["/ws/project"].facts[0].text,
            "original"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn clear_removes_all_facts_for_the_project_only() {
        let path = temp_path();
        add_fact_impl(&path, "/ws/a", "fact for a, one", "agent", None).unwrap();
        add_fact_impl(&path, "/ws/a", "fact for a, two", "user", None).unwrap();
        add_fact_impl(&path, "/ws/b", "fact for b", "agent", None).unwrap();

        clear_impl(&path, "/ws/a").unwrap();

        let memories = load_impl(&path).unwrap();
        assert!(
            !memories.projects.contains_key("/ws/a") || memories.projects["/ws/a"].facts.is_empty()
        );
        assert_eq!(memories.projects["/ws/b"].facts.len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remember_with_no_workspace_saves_under_the_global_scope() {
        let path = temp_path();
        let fact =
            add_fact_impl(&path, GLOBAL_SCOPE_KEY, "User's name is Ahmad.", "agent", None).unwrap();

        let reloaded = load_impl(&path).unwrap();
        let facts = &reloaded.projects[GLOBAL_SCOPE_KEY].facts;
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].id, fact.id);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn owning_scope_finds_a_fact_in_the_global_bucket_even_with_a_project_open() {
        let path = temp_path();
        let global_fact = add_fact_impl(&path, GLOBAL_SCOPE_KEY, "global fact", "agent", None).unwrap();
        add_fact_impl(&path, "/ws/project", "project fact", "agent", None).unwrap();

        let memories = load_impl(&path).unwrap();
        let scopes = vec![GLOBAL_SCOPE_KEY.to_string(), "/ws/project".to_string()];
        assert_eq!(
            owning_scope(&memories, &scopes, &global_fact.id),
            Some(GLOBAL_SCOPE_KEY.to_string())
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn owning_scope_is_none_when_the_id_is_in_neither_candidate_scope() {
        let path = temp_path();
        add_fact_impl(&path, "/ws/other-project", "unrelated fact", "agent", None).unwrap();

        let memories = load_impl(&path).unwrap();
        let scopes = vec![GLOBAL_SCOPE_KEY.to_string(), "/ws/project".to_string()];
        assert_eq!(owning_scope(&memories, &scopes, "not-a-real-id"), None);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn clear_of_project_with_no_facts_is_a_no_op_success() {
        let path = temp_path();
        clear_impl(&path, "/ws/never-had-facts").unwrap();
        assert!(load_impl(&path).unwrap().projects.is_empty());
    }

    #[test]
    fn timestamps_round_trip_a_few_known_epoch_days() {
        // 1970-01-01 (epoch day 0) and 2000-03-01 (a well-known leap-year
        // boundary case for the civil_from_days algorithm) both must convert
        // correctly, or every fact's created_at would be silently wrong.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
    }

    // ---- Memory Studio: enable/disable, full listing, export/import -------

    #[test]
    fn set_enabled_impl_toggles_the_flag_and_persists_it() {
        let path = temp_path();
        let fact = add_fact_impl(&path, "/ws/project", "toggle me", "agent", None).unwrap();
        assert!(fact.enabled);

        let disabled = set_enabled_impl(&path, "/ws/project", &fact.id, false).unwrap();
        assert!(!disabled.enabled);
        assert_eq!(
            load_impl(&path).unwrap().projects["/ws/project"].facts[0].enabled,
            false
        );

        let reenabled = set_enabled_impl(&path, "/ws/project", &fact.id, true).unwrap();
        assert!(reenabled.enabled);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn set_enabled_impl_of_unknown_id_is_an_error() {
        let path = temp_path();
        add_fact_impl(&path, "/ws/project", "stays put", "agent", None).unwrap();

        let err = set_enabled_impl(&path, "/ws/project", "not-a-real-id", false).unwrap_err();
        assert!(err.contains("not found"), "unexpected error: {err}");
    }

    /// The CRITICAL, independently-testable proof ROADMAP.md's Memory Studio
    /// acceptance criteria asks for: "deleting or disabling a memory
    /// prevents it from entering future prompts." `list_impl` is the exact
    /// function `memory_list` calls, which `rulesStore.facts` is populated
    /// from every turn, which `systemPrompt.ts`'s `factsLines` reads
    /// straight into the outgoing system prompt — so proving a disabled or
    /// deleted fact is absent from `list_impl`'s output proves it is absent
    /// from a subsequent prompt assembly.
    #[test]
    fn disabled_and_deleted_facts_are_excluded_from_list_impl() {
        let path = temp_path();
        let kept = add_fact_impl(&path, "/ws/project", "keep me", "agent", None).unwrap();
        let disabled = add_fact_impl(&path, "/ws/project", "disable me", "agent", None).unwrap();
        let deleted = add_fact_impl(&path, "/ws/project", "delete me", "agent", None).unwrap();
        let global_kept = add_fact_impl(&path, GLOBAL_SCOPE_KEY, "global keep me", "agent", None).unwrap();
        let global_disabled =
            add_fact_impl(&path, GLOBAL_SCOPE_KEY, "global disable me", "agent", None).unwrap();

        set_enabled_impl(&path, "/ws/project", &disabled.id, false).unwrap();
        delete_fact_impl(&path, "/ws/project", &deleted.id).unwrap();
        set_enabled_impl(&path, GLOBAL_SCOPE_KEY, &global_disabled.id, false).unwrap();

        let facts = list_impl(&path, Some("/ws/project")).unwrap();
        let ids: Vec<&str> = facts.iter().map(|f| f.id.as_str()).collect();

        assert!(ids.contains(&kept.id.as_str()), "enabled project fact must still be injected");
        assert!(ids.contains(&global_kept.id.as_str()), "enabled global fact must still be injected");
        assert!(!ids.contains(&disabled.id.as_str()), "disabled fact must not enter a future prompt");
        assert!(!ids.contains(&deleted.id.as_str()), "deleted fact must not enter a future prompt");
        assert!(
            !ids.contains(&global_disabled.id.as_str()),
            "disabled global fact must not enter a future prompt"
        );
        assert_eq!(facts.len(), 2, "only the two still-enabled facts should be returned");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn list_impl_with_no_project_root_returns_only_global_facts() {
        let path = temp_path();
        add_fact_impl(&path, GLOBAL_SCOPE_KEY, "global fact", "agent", None).unwrap();
        add_fact_impl(&path, "/ws/project", "project fact", "agent", None).unwrap();

        let facts = list_impl(&path, None).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].text, "global fact");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn list_all_impl_returns_every_fact_across_every_project_with_scope_resolved() {
        let path = temp_path();
        add_fact_impl(&path, GLOBAL_SCOPE_KEY, "global fact", "agent", None).unwrap();
        let a = add_fact_impl(&path, "/ws/a", "fact for a", "agent", None).unwrap();
        add_fact_impl(&path, "/ws/b", "fact for b", "user", None).unwrap();
        set_enabled_impl(&path, "/ws/a", &a.id, false).unwrap();

        let entries = list_all_impl(&path).unwrap();
        assert_eq!(entries.len(), 3, "list_all_impl must include disabled facts too");

        let global = entries.iter().find(|e| e.text == "global fact").unwrap();
        assert_eq!(global.scope, "global");
        assert_eq!(global.project_root, None);

        let disabled = entries.iter().find(|e| e.text == "fact for a").unwrap();
        assert_eq!(disabled.scope, "project");
        assert_eq!(disabled.project_root.as_deref(), Some("/ws/a"));
        assert!(!disabled.enabled);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn import_impl_restores_scope_and_is_idempotent_on_a_second_import() {
        let path = temp_path();
        let memories = load_impl(&path).unwrap();
        // Export an empty store just to get a well-formed entry list to
        // hand-build against — simpler to construct MemoryExportEntry values
        // directly here.
        drop(memories);

        let entries = vec![
            MemoryExportEntry {
                entry: MemoryEntry {
                    id: "imported-1".to_string(),
                    text: "Imported project fact".to_string(),
                    source: "agent".to_string(),
                    created_at: "2026-01-01T00:00:00.000Z".to_string(),
                    enabled: true,
                    source_turn_id: Some("turn-9".to_string()),
                    scope: "project".to_string(),
                    project_root: Some("/ws/imported".to_string()),
                },
                redacted: false,
            },
            MemoryExportEntry {
                entry: MemoryEntry {
                    id: "imported-2".to_string(),
                    text: "Imported global fact".to_string(),
                    source: "user".to_string(),
                    created_at: "2026-01-01T00:00:00.000Z".to_string(),
                    enabled: false,
                    source_turn_id: None,
                    scope: "global".to_string(),
                    project_root: None,
                },
                redacted: false,
            },
        ];

        let summary = import_impl(&path, &entries);
        assert_eq!(summary.added, 2);
        assert_eq!(summary.skipped_duplicate, 0);
        assert!(summary.errors.is_empty());

        let reloaded = load_impl(&path).unwrap();
        let project_fact = &reloaded.projects["/ws/imported"].facts[0];
        assert_eq!(project_fact.text, "Imported project fact");
        assert_eq!(project_fact.source_turn_id.as_deref(), Some("turn-9"));
        assert!(project_fact.enabled);

        let global_fact = &reloaded.projects[GLOBAL_SCOPE_KEY].facts[0];
        assert_eq!(global_fact.text, "Imported global fact");
        assert!(
            !global_fact.enabled,
            "an entry exported as disabled must come back disabled, not silently re-enabled"
        );

        // Importing the same file again must not create duplicates.
        let second_summary = import_impl(&path, &entries);
        assert_eq!(second_summary.added, 0);
        assert_eq!(second_summary.skipped_duplicate, 2);
        assert_eq!(
            load_impl(&path).unwrap().projects["/ws/imported"].facts.len(),
            1
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn import_impl_reports_an_error_for_an_unknown_scope_without_aborting_the_rest() {
        let path = temp_path();
        let entries = vec![
            MemoryExportEntry {
                entry: MemoryEntry {
                    id: "bad-1".to_string(),
                    text: "orphaned entry".to_string(),
                    source: "agent".to_string(),
                    created_at: "2026-01-01T00:00:00.000Z".to_string(),
                    enabled: true,
                    source_turn_id: None,
                    scope: "workspace".to_string(),
                    project_root: None,
                },
                redacted: false,
            },
            MemoryExportEntry {
                entry: MemoryEntry {
                    id: "good-1".to_string(),
                    text: "valid entry".to_string(),
                    source: "agent".to_string(),
                    created_at: "2026-01-01T00:00:00.000Z".to_string(),
                    enabled: true,
                    source_turn_id: None,
                    scope: "global".to_string(),
                    project_root: None,
                },
                redacted: false,
            },
        ];

        let summary = import_impl(&path, &entries);
        assert_eq!(summary.added, 1, "the valid entry must still import");
        assert_eq!(summary.errors.len(), 1);
        assert!(summary.errors[0].contains("unknown scope"));

        let _ = std::fs::remove_file(&path);
    }

    /// End-to-end proof of the export/import split described in the module
    /// docs: `list_all_impl` is exactly what `memoryStudio.ts`'s
    /// `exportMemories` calls (via `memory_list_all`) to build the entries it
    /// writes to a file, and wrapping each `MemoryEntry` as a
    /// `MemoryExportEntry` (`redacted: false` — no secret-shaped text here)
    /// is exactly the shape `buildMemoryExport` produces. Feeding that back
    /// through `import_impl` on a fresh store proves the whole round trip,
    /// without needing a Rust-side export function to exist.
    #[test]
    fn list_all_impl_output_round_trips_through_import_impl_on_a_fresh_store() {
        let source_path = temp_path();
        add_fact_impl(&source_path, "/ws/project", "round trip me", "agent", Some("turn-1")).unwrap();
        add_fact_impl(&source_path, GLOBAL_SCOPE_KEY, "global round trip", "user", None).unwrap();

        let listed = list_all_impl(&source_path).unwrap();
        let entries: Vec<MemoryExportEntry> = listed
            .into_iter()
            .map(|entry| MemoryExportEntry { entry, redacted: false })
            .collect();

        let dest_path = temp_path();
        let summary = import_impl(&dest_path, &entries);
        assert_eq!(summary.added, 2);
        assert!(summary.errors.is_empty());

        let dest_entries = list_all_impl(&dest_path).unwrap();
        let texts: Vec<&str> = dest_entries.iter().map(|e| e.text.as_str()).collect();
        assert!(texts.contains(&"round trip me"));
        assert!(texts.contains(&"global round trip"));

        let _ = std::fs::remove_file(&source_path);
        let _ = std::fs::remove_file(&dest_path);
    }
}
