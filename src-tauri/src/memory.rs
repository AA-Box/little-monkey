//! Persistent per-project "remembered facts": structured app-data JSON at
//! `<app_data>/memories.json`, distinct from the plain-markdown `rules.rs`
//! module (see that module's docs for why the two stores are deliberately
//! separate homes).
//!
//! Schema (v2): `{ "version": 2, "projects": { "<canonical primary root
//! path>": { "facts": [ { "id", "text", "source": "agent"|"user",
//! "created_at", "enabled", "source_turn_id", "pinned", "expires_at",
//! "last_used_at", "merged_from", "merged_into", "retired_at" } ] } } }`,
//! keyed by the canonical
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
//! ## Memory Studio
//!
//! This app's only durable memory store is the one described above, and it
//! has exactly two real scopes: `"global"` (applies to every project, keyed
//! under [`GLOBAL_SCOPE_KEY`]) and `"project"` (keyed by canonical
//! workspace root). There is no `"workspace"`, `"user"`, `"device"`, or
//! connector scope and there never has been — there is exactly one local
//! app-data store per install, so "user"/"device" collapse into `"global"`,
//! and nothing ever writes a memory tagged as coming from a workspace
//! distinct from a project root. There is likewise no confidence score and
//! no source-file/source-connector provenance: no tool in this codebase
//! captures either at remember time, so Memory Studio does not display
//! fields it would have to fabricate.
//!
//! What *is* real is the v2 lifecycle, all of it enforced in this module so
//! the desktop app, `tool_remember` and `monkey-cli` share one set of
//! guarantees:
//!
//! - `enabled` — soft-disable. A disabled fact stays on disk and is never
//!   returned by [`list_impl`].
//! - `pinned` ([`set_pinned_impl`]) — a pinned fact is folded into the
//!   prompt **first** (stable sort in [`list_impl`]), is exempt from expiry,
//!   and does not count toward [`MAX_FACTS_PER_PROJECT`]. It has its own
//!   ceiling instead, [`MAX_PINNED_PER_PROJECT`], so "exempt from the cap"
//!   never means "unbounded".
//! - `expires_at` ([`set_expiry_impl`]) — an RFC 3339 UTC stamp evaluated
//!   **lazily, at read time** in [`list_impl`]. Nothing in this app runs a
//!   timer over `memories.json`, so an expired fact stops reaching prompts
//!   immediately but stays on disk until [`purge_expired_impl`] is called.
//! - `merged_from`/`merged_into`/`retired_at` ([`merge_impl`],
//!   [`unmerge_impl`]) — merging N facts writes one new fact naming its
//!   parent ids and *soft-retires* the parents (kept on disk, excluded from
//!   [`list_impl`]) so the merge can be undone. Only facts that currently
//!   reach the prompt can be merged, and only within one scope.
//! - `last_used_at` ([`mark_used_impl`]) — stamped when a system prompt is
//!   actually assembled from a fact (`systemPrompt.ts`'s
//!   `currentSystemPrompt` and `monkey-cli`'s `compose_system_prompt_impl`),
//!   not per model call, and throttled to [`MARK_USED_THROTTLE_SECS`] so a
//!   tool-calling loop does not rewrite the whole file every iteration. A
//!   queued daemon turn replaying a frozen prompt never re-reads memory and
//!   so never updates it.
//!
//! Provenance in Memory Studio is answered from the stored record alone —
//! `source` (`"agent"` vs `"user"`), `source_turn_id` (set when
//! `tool_remember` ran inside a specific chat turn), `merged_from`, and
//! `last_used_at`. Chat answers still do not report which memory they drew
//! on, so there is no "why do you know this" link from an answer back to a
//! fact.
//!
//! [`list_impl`] — the exact function `memory_list` (and therefore
//! `rulesStore.facts` and `systemPrompt.ts`'s `factsLines`) calls, and the
//! one `monkey-cli`'s `compose_system_prompt_impl` calls — is the single
//! filter and ordering point, so a disabled, expired or merge-retired fact
//! is provably excluded from every subsequent prompt (see this module's
//! tests). A `memories.json` written at v1 loads unchanged: every v2 field
//! is `#[serde(default)]` and [`load_impl`] re-stamps the version.
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

use crate::profiles::ProfileScopedPaths;
use crate::{workspace, AppState};

const MEMORIES_FILE: &str = "memories.json";

/// Reserved `projects` key for facts remembered while no workspace folder is
/// open (`tool_remember` used to hard-fail in that case — see
/// ROADMAP/session notes on the "remember with no workspace" bug). Never
/// collides with a real project root: canonicalized workspace roots are
/// always absolute filesystem paths, which this string is not. Global facts
/// are included in every `memory_list` call regardless of which (if any)
/// project is open, in addition to that project's own facts.
pub(crate) const GLOBAL_SCOPE_KEY: &str = "__global__";

/// Current on-disk schema version. v1 (id/text/source/created_at/enabled/
/// source_turn_id) still loads unchanged — every v2 field is
/// `#[serde(default)]` — and [`load_impl`] re-stamps the version so the next
/// write records v2. See `a_v1_memories_file_loads_unchanged_and_resaves_at_v2`.
const SCHEMA_VERSION: u8 = 2;

/// Per-scope cap on *un-pinned, un-retired* facts, enforced by
/// [`add_fact_impl`] — see module docs.
const MAX_FACTS_PER_PROJECT: usize = 100;

/// Per-scope cap on pinned facts, enforced by [`set_pinned_impl`]. Pinned
/// facts are exempt from [`MAX_FACTS_PER_PROJECT`], so without this "exempt
/// from the cap" would mean "unbounded"; the real per-scope ceiling is
/// therefore 100 + 20 = 120. A chosen number, not a measured one.
const MAX_PINNED_PER_PROJECT: usize = 20;

/// How stale a fact's `last_used_at` must be before [`mark_used_impl`] will
/// rewrite `memories.json` for it. `currentSystemPrompt`/
/// `compose_system_prompt_impl` run once per tool-calling iteration, and each
/// mark is a whole-file read-modify-write racing `tool_remember` from another
/// process; the stamp is rendered as a date, so an hour of slack costs
/// nothing visible and turns a per-turn write into a per-hour one.
const MARK_USED_THROTTLE_SECS: u64 = 3_600;

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
    /// Pinned: folded into the prompt first, exempt from `expires_at` and
    /// from [`MAX_FACTS_PER_PROJECT`] (see [`MAX_PINNED_PER_PROJECT`]).
    #[serde(default)]
    pub pinned: bool,
    /// Optional RFC 3339 UTC expiry. Once reached, [`list_impl`] stops
    /// returning the fact (unless it is `pinned`), but nothing deletes it —
    /// see [`purge_expired_impl`].
    #[serde(default)]
    pub expires_at: Option<String>,
    /// When a system prompt was last actually assembled from this fact —
    /// see [`mark_used_impl`], and [`MARK_USED_THROTTLE_SECS`] for the
    /// precision this really has.
    #[serde(default)]
    pub last_used_at: Option<String>,
    /// For a fact produced by [`merge_impl`]: the ids of the facts it was
    /// merged from. Empty for every other fact.
    #[serde(default)]
    pub merged_from: Vec<String>,
    /// For a fact retired by [`merge_impl`]: the id of the merged fact that
    /// replaced it. Set together with `retired_at`.
    #[serde(default)]
    pub merged_into: Option<String>,
    /// When this fact was soft-retired by a merge. A retired fact is kept on
    /// disk purely so [`unmerge_impl`] can restore it; it never reaches a
    /// prompt.
    #[serde(default)]
    pub retired_at: Option<String>,
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
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub last_used_at: Option<String>,
    #[serde(default)]
    pub merged_from: Vec<String>,
    #[serde(default)]
    pub merged_into: Option<String>,
    #[serde(default)]
    pub retired_at: Option<String>,
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
        pinned: fact.pinned,
        expires_at: fact.expires_at.clone(),
        last_used_at: fact.last_used_at.clone(),
        merged_from: fact.merged_from.clone(),
        merged_into: fact.merged_into.clone(),
        retired_at: fact.retired_at.clone(),
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
        Ok(raw) => {
            let mut parsed: MemoriesFile =
                serde_json::from_str(&raw).map_err(|e| format!("Corrupt memories file: {}", e))?;
            // v1 -> v2 migration in one line: every v2 field is
            // `#[serde(default)]`, so a v1 document already parsed correctly
            // above; all that is left is re-stamping the version so the next
            // write records v2. Deliberately no version *check* — this never
            // had one, and refusing to load a v1 file would lose memories.
            parsed.version = SCHEMA_VERSION;
            Ok(parsed)
        }
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
pub fn now_rfc3339() -> String {
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
///   [`MAX_FACTS_PER_PROJECT`] *un-pinned, un-retired* facts, is a real
///   error the caller (and, for `tool_remember`, the model) should see and
///   react to. Pinned facts are exempt from this cap and bounded by
///   [`MAX_PINNED_PER_PROJECT`] instead; merge-retired facts are bookkeeping
///   for [`unmerge_impl`], not live memories, and are not counted either.
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

    // Skip merge-retired facts: a merged-away original must not silently
    // block re-remembering that text. (`import_impl` does its own duplicate
    // check *including* retired facts — see its doc comment for why the two
    // paths differ.)
    if let Some(existing) = project
        .facts
        .iter()
        .find(|f| f.text == trimmed && f.retired_at.is_none())
    {
        return Ok(existing.clone());
    }

    let unpinned = project
        .facts
        .iter()
        .filter(|f| !f.pinned && f.retired_at.is_none())
        .count();
    if unpinned >= MAX_FACTS_PER_PROJECT {
        return Err(format!(
            "This project already has {} un-pinned remembered facts (the limit) — forget one, or pin one, before adding another.",
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
        pinned: false,
        expires_at: None,
        last_used_at: None,
        merged_from: Vec::new(),
        merged_into: None,
        retired_at: None,
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

/// Core pin/unpin logic behind `memory_studio_set_pinned`. A pinned fact is
/// folded into the prompt first ([`list_impl`]), never expires, and does not
/// count toward [`MAX_FACTS_PER_PROJECT`] — so pinning is capped by
/// [`MAX_PINNED_PER_PROJECT`] per scope instead. Pinning an already-pinned
/// fact is an idempotent success (same stance as `set_enabled_impl`), so the
/// ceiling is only checked on a real transition.
pub fn set_pinned_impl(path: &Path, root: &str, id: &str, pinned: bool) -> Result<Fact, String> {
    let mut memories = load_impl(path)?;
    let updated = {
        let project = memories
            .projects
            .get_mut(root)
            .ok_or_else(|| "Fact not found — it may have already been forgotten.".to_string())?;
        let already_pinned = project.facts.iter().filter(|f| f.pinned).count();
        let fact = project
            .facts
            .iter_mut()
            .find(|f| f.id == id)
            .ok_or_else(|| "Fact not found — it may have already been forgotten.".to_string())?;
        if pinned && !fact.pinned && already_pinned >= MAX_PINNED_PER_PROJECT {
            return Err(format!(
                "This scope already has {} pinned memories (the limit) — unpin one before pinning another.",
                MAX_PINNED_PER_PROJECT
            ));
        }
        fact.pinned = pinned;
        fact.clone()
    };
    save_impl(path, &memories)?;
    Ok(updated)
}

/// Normalizes a user-supplied expiry into the fixed-width RFC 3339 UTC shape
/// [`now_rfc3339`] produces, which is what makes [`is_expired`]'s
/// lexicographic comparison correct. Accepts either a bare `YYYY-MM-DD` —
/// expanded to the **end** of that day (`T23:59:59.999Z`), so "expires
/// 2026-12-31" means "valid through the 31st" rather than expiring the
/// instant it is saved — or an already-full `YYYY-MM-DDTHH:MM:SS...Z`.
/// Byte-position checks, no regex crate: this is a shape check, not a
/// calendar validator.
fn normalize_expiry(input: &str) -> Result<String, String> {
    const BAD: &str = "Expiry must be a date (2026-12-31) or an RFC 3339 UTC timestamp ending in Z.";
    let trimmed = input.trim();
    let b = trimmed.as_bytes();
    let date_shaped = b.len() >= 10
        && b[..10].iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                *c == b'-'
            } else {
                c.is_ascii_digit()
            }
        });
    if !date_shaped {
        return Err(BAD.to_string());
    }
    if b.len() == 10 {
        return Ok(format!("{trimmed}T23:59:59.999Z"));
    }
    // A full timestamp: `T`/space separator, `HH:MM:SS` and a trailing `Z`.
    let ok = b.len() >= 20
        && (b[10] == b'T' || b[10] == b' ')
        && b[11].is_ascii_digit()
        && b[12].is_ascii_digit()
        && b[13] == b':'
        && b[14].is_ascii_digit()
        && b[15].is_ascii_digit()
        && b[16] == b':'
        && b[17].is_ascii_digit()
        && b[18].is_ascii_digit()
        && *b.last().unwrap() == b'Z';
    if !ok {
        return Err(BAD.to_string());
    }
    let mut out = trimmed.to_string();
    out.replace_range(10..11, "T");
    Ok(out)
}

/// Core set/clear-expiry logic behind `memory_studio_set_expiry`. `None`
/// clears the expiry. Expiry is evaluated lazily by [`list_impl`] when a
/// prompt is assembled — nothing in this app runs a timer over
/// `memories.json` — so an expired fact stops reaching prompts at once but
/// stays on disk until [`purge_expired_impl`] removes it.
pub fn set_expiry_impl(
    path: &Path,
    root: &str,
    id: &str,
    expires_at: Option<&str>,
) -> Result<Fact, String> {
    let normalized = match expires_at {
        Some(raw) => Some(normalize_expiry(raw)?),
        None => None,
    };
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
        fact.expires_at = normalized;
        fact.clone()
    };
    save_impl(path, &memories)?;
    Ok(updated)
}

/// Hard-delete every expired fact across every scope — the explicit purge
/// action behind `memory_studio_purge_expired`. Deliberately scope-less: it
/// purges the whole store in one pass, not the caller's current filter.
/// Pinned facts are exempt (they never expire), and merge-retired facts are
/// never purged here whatever their expiry — they are [`unmerge_impl`]'s
/// undo material. Nothing expired is `Ok(0)`, not an error, and writes
/// nothing.
pub fn purge_expired_impl(path: &Path) -> Result<usize, String> {
    let mut memories = load_impl(path)?;
    let now = now_rfc3339();
    let mut removed = 0usize;
    for project in memories.projects.values_mut() {
        let before = project.facts.len();
        project
            .facts
            .retain(|f| f.pinned || f.retired_at.is_some() || !is_expired(f, &now));
        removed += before - project.facts.len();
    }
    if removed > 0 {
        save_impl(path, &memories)?;
    }
    Ok(removed)
}

/// Core merge logic behind `memory_studio_merge`: combine two or more facts
/// in one scope into a single new fact that records its parents' ids in
/// `merged_from`, and soft-retire the parents (`merged_into` + `retired_at`,
/// left on disk) so [`unmerge_impl`] can put them back.
///
/// Rules, and why:
/// - At least two distinct ids. One id is not a merge.
/// - Every parent must currently reach the prompt (`enabled`, not expired,
///   not already retired). Merging a disabled or expired fact would launder
///   its text into a live, never-expiring memory — the exact guarantee
///   [`list_impl`] exists to make.
/// - One `root` per call, so a cross-scope merge is structurally impossible
///   rather than checked for.
/// - `text` is the combined memory; omitted, the parents' texts are joined
///   with a space. Same non-empty/[`MAX_FACT_CHARS`] validation as
///   [`add_fact_impl`].
/// - `source` is inherited when every parent agrees and only falls back to
///   `"user"` for a mixed merge — recording a human author for two facts the
///   agent remembered would be fabricated provenance.
/// - `pinned` is inherited if any parent was pinned (a pinned fact must not
///   lose its always-in-prompt guarantee by being merged); the pin ceiling is
///   then counted *after* excluding the parents being retired, so merging
///   two pins in a full scope lowers the count rather than being refused.
/// - `expires_at` is `None`: a merge is a fresh statement by the user, not an
///   inheritance of the shortest parent deadline.
pub fn merge_impl(
    path: &Path,
    root: &str,
    ids: &[String],
    text: Option<&str>,
) -> Result<Fact, String> {
    let mut unique: Vec<&String> = Vec::new();
    for id in ids {
        if !unique.iter().any(|existing| *existing == id) {
            unique.push(id);
        }
    }
    if unique.len() < 2 {
        return Err("Merging needs at least two memories.".to_string());
    }

    let mut memories = load_impl(path)?;
    let now = now_rfc3339();
    let project = memories
        .projects
        .get_mut(root)
        .ok_or_else(|| "Fact not found — it may have already been forgotten.".to_string())?;

    let mut parents: Vec<Fact> = Vec::new();
    for id in &unique {
        let fact = project
            .facts
            .iter()
            .find(|f| &&f.id == id)
            .ok_or_else(|| "Fact not found — it may have already been forgotten.".to_string())?;
        if !reaches_prompt(fact, &now) {
            return Err(
                "Only memories that currently reach the prompt can be merged — enable or clear the expiry on the ones you selected first."
                    .to_string(),
            );
        }
        parents.push(fact.clone());
    }

    let merged_text = match text {
        Some(t) => t.trim().to_string(),
        None => parents
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join(" "),
    };
    if merged_text.is_empty() {
        return Err("Fact text must not be empty".to_string());
    }
    if merged_text.chars().count() > MAX_FACT_CHARS {
        return Err(format!(
            "The combined memory is {} characters, over the {}-character limit — write a shorter combined memory instead of joining the originals.",
            merged_text.chars().count(),
            MAX_FACT_CHARS
        ));
    }

    let pinned = parents.iter().any(|p| p.pinned);
    if pinned {
        // Count pins that will still exist once the parents are retired.
        let surviving = project
            .facts
            .iter()
            .filter(|f| f.pinned && !parents.iter().any(|p| p.id == f.id))
            .count();
        if surviving >= MAX_PINNED_PER_PROJECT {
            return Err(format!(
                "This scope already has {} pinned memories (the limit) — unpin one before merging pinned memories.",
                MAX_PINNED_PER_PROJECT
            ));
        }
    }
    let first_source = parents[0].source.clone();
    let source = if parents.iter().all(|p| p.source == first_source) {
        first_source
    } else {
        "user".to_string()
    };

    let merged = Fact {
        id: uuid::Uuid::new_v4().to_string(),
        text: merged_text,
        source,
        created_at: now.clone(),
        enabled: true,
        source_turn_id: None,
        pinned,
        expires_at: None,
        last_used_at: None,
        merged_from: parents.iter().map(|p| p.id.clone()).collect(),
        merged_into: None,
        retired_at: None,
    };

    for fact in project.facts.iter_mut() {
        if parents.iter().any(|p| p.id == fact.id) {
            fact.merged_into = Some(merged.id.clone());
            fact.retired_at = Some(now.clone());
        }
    }
    project.facts.push(merged.clone());
    save_impl(path, &memories)?;
    Ok(merged)
}

/// Undo a merge: restore every still-present parent named by the merged
/// fact's `merged_from` (clearing `merged_into`/`retired_at`, so they reach
/// prompts again) and remove the merged fact itself. Returns how many
/// parents were restored. An unknown id, or a fact with an empty
/// `merged_from`, is a no-op success (`Ok(0)`) — the caller's desired end
/// state already holds.
///
/// Note the direction: this *discards* the merged fact, so any edit made to
/// the merged text after the merge is lost. There is no revision history in
/// this module (unlike `rules.rs`), and the soft-retire is the whole undo.
pub fn unmerge_impl(path: &Path, root: &str, id: &str) -> Result<usize, String> {
    let mut memories = load_impl(path)?;
    let Some(project) = memories.projects.get_mut(root) else {
        return Ok(0);
    };
    let Some(merged) = project.facts.iter().find(|f| f.id == id).cloned() else {
        return Ok(0);
    };
    if merged.merged_from.is_empty() {
        return Ok(0);
    }
    let mut restored = 0usize;
    for fact in project.facts.iter_mut() {
        if merged.merged_from.contains(&fact.id) {
            fact.merged_into = None;
            fact.retired_at = None;
            restored += 1;
        }
    }
    project.facts.retain(|f| f.id != id);
    save_impl(path, &memories)?;
    Ok(restored)
}

/// Stamp `last_used_at` on the named facts, across every scope, and report
/// how many were stamped. Called when a system prompt is actually assembled
/// from a fact list (`systemPrompt.ts`'s `currentSystemPrompt`,
/// `monkey-cli`'s `compose_system_prompt_impl`) — not per model call, and
/// never for a queued daemon turn, which replays a frozen rendered prompt and
/// re-reads no memory.
///
/// Throttled by [`MARK_USED_THROTTLE_SECS`]: a fact whose stamp is already
/// newer than that is left alone, and when nothing needs stamping this
/// writes nothing at all. That matters for more than IO — this is an
/// unlocked whole-file read-modify-write, and `state.memory_lock` is an
/// in-process mutex that does not serialize against the CLI or daemon in
/// another process, so a per-turn write would turn a rare lost-`remember`
/// race into a per-turn one.
pub fn mark_used_impl(path: &Path, ids: &[String]) -> Result<usize, String> {
    if ids.is_empty() {
        return Ok(0);
    }
    let mut memories = load_impl(path)?;
    let now = now_rfc3339();
    let cutoff = stale_before(MARK_USED_THROTTLE_SECS);
    let mut stamped = 0usize;
    for project in memories.projects.values_mut() {
        for fact in project.facts.iter_mut() {
            if !ids.iter().any(|id| id == &fact.id) {
                continue;
            }
            if fact
                .last_used_at
                .as_deref()
                .is_some_and(|prev| prev > cutoff.as_str())
            {
                continue;
            }
            fact.last_used_at = Some(now.clone());
            stamped += 1;
        }
    }
    if stamped > 0 {
        save_impl(path, &memories)?;
    }
    Ok(stamped)
}

/// The [`now_rfc3339`]-shaped timestamp `secs` seconds ago — anything older
/// than this is "stale enough to re-stamp" for [`mark_used_impl`].
fn stale_before(secs: u64) -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_sub(std::time::Duration::from_secs(secs));
    let total = dur.as_secs();
    let (year, month, day) = civil_from_days((total / 86_400) as i64);
    let rem = total % 86_400;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year,
        month,
        day,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        dur.subsec_millis()
    )
}

/// Which storage key holds `id`, scanning every scope — what the CLI uses so
/// a user never has to type a canonical project root. `Ok(None)` when no
/// scope holds it.
pub fn scope_of_impl(path: &Path, id: &str) -> Result<Option<String>, String> {
    let memories = load_impl(path)?;
    Ok(memories
        .projects
        .iter()
        .find(|(_, project)| project.facts.iter().any(|f| f.id == id))
        .map(|(root, _)| root.clone()))
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
///
/// Deleting a *merged* fact cascades to the originals it retired. Every
/// delete path routes through here (the transcript's Forget button,
/// `memory_studio_delete`, checkpoint compensation), so this is the one
/// place the rule can live. The alternative — restoring the parents — would
/// make "forget" *add* two memories to the next prompt, and would also make
/// deletion of the merged fact the only way to strand a parent with a
/// dangling `merged_into`. Undo is [`unmerge_impl`]'s job, not delete's; the
/// UI's confirm text says so.
pub fn delete_fact_impl(path: &Path, root: &str, id: &str) -> Result<(), String> {
    let mut memories = load_impl(path)?;
    if let Some(project) = memories.projects.get_mut(root) {
        let before = project.facts.len();
        project
            .facts
            .retain(|f| f.id != id && f.merged_into.as_deref() != Some(id));
        if project.facts.len() != before {
            save_impl(path, &memories)?;
        }
    }
    Ok(())
}

/// Whether `fact`'s expiry has passed as of `now`. Both sides are
/// [`now_rfc3339`]-shaped: fixed-width `YYYY-MM-DDTHH:MM:SS.mmmZ` in UTC, so
/// a plain lexicographic `<=` is a correct chronological comparison and no
/// date/time crate is needed. [`normalize_expiry`] is what guarantees the
/// stored side has that shape.
fn is_expired(fact: &Fact, now: &str) -> bool {
    fact.expires_at.as_deref().is_some_and(|e| e <= now)
}

/// Whether `fact` may be folded into a model prompt as of `now` — the single
/// definition of "reaches the prompt", used by [`list_impl`] and mirrored
/// (for display badges only) by `memoryStudio.ts`'s `wouldReachPrompt`.
fn reaches_prompt(fact: &Fact, now: &str) -> bool {
    fact.enabled && fact.retired_at.is_none() && (fact.pinned || !is_expired(fact, now))
}

/// Core logic behind `memory_list`: every prompt-eligible global fact, plus
/// every prompt-eligible fact under `project_root` (if given), pinned facts
/// first. **This is the exact seam where storage crosses into what a future
/// model prompt can see** — `rulesStore.refresh()` calls `memory_list` once
/// per turn, stashes the result as `facts`, and `systemPrompt.ts`'s
/// `currentSystemPrompt` reads that array straight into `factsLines`;
/// `monkey-cli`'s `compose_system_prompt_impl` calls this same function.
/// Three lifecycle states are filtered out right here, so none of them can
/// ever appear in a subsequent prompt on either surface:
/// - `enabled: false` (soft-disabled via [`set_enabled_impl`]),
/// - expired (`expires_at` reached — unless the fact is `pinned`, which is
///   exempt), and
/// - merge-retired (`retired_at` set by [`merge_impl`]; the merged fact
///   carrying their combined text is what reaches the prompt instead).
///
/// Ordering: `sort_by_key(|f| !f.pinned)` on a stable sort, so pinned facts
/// come first and global-before-project order is preserved within each
/// group. This is the ONLY filter/ordering point — `systemPrompt.ts` and the
/// CLI deliberately add none of their own, so there is one truth. Proofs:
/// `disabled_and_deleted_facts_are_excluded_from_list_impl`,
/// `expired_and_merge_retired_facts_are_excluded_from_list_impl`,
/// `pinned_facts_are_listed_first_by_list_impl`.
pub fn list_impl(path: &Path, project_root: Option<&str>) -> Result<Vec<Fact>, String> {
    let memories = load_impl(path)?;
    let now = now_rfc3339();
    let mut facts: Vec<Fact> = memories
        .projects
        .get(GLOBAL_SCOPE_KEY)
        .map(|p| {
            p.facts
                .iter()
                .filter(|f| reaches_prompt(f, &now))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if let Some(root) = project_root {
        if let Some(project) = memories.projects.get(root) {
            facts.extend(
                project
                    .facts
                    .iter()
                    .filter(|f| reaches_prompt(f, &now))
                    .cloned(),
            );
        }
    }
    facts.sort_by_key(|f| !f.pinned);
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
    entries.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(entries)
}

/// Core import logic behind `memory_import`. Re-adds each exported entry
/// through [`add_fact_impl`] (so the same length/cap rules apply as any
/// other remember), restoring its original scope (`"global"`/`"project"` +
/// `project_root`), and then makes ONE fix-up pass that restores the
/// lifecycle state `add_fact_impl` cannot know about: `enabled`, `pinned`,
/// `expires_at`, `last_used_at`, `retired_at`, and the merge relationship,
/// with `merged_from`/`merged_into` remapped from the export's ids to the
/// freshly generated ones. Without that remap an export taken after a merge
/// would re-import the retired originals as live facts and the merged
/// content would reach the prompt twice.
///
/// The duplicate check is done here, against **every** fact already in the
/// target root including merge-retired ones, rather than relying on
/// `add_fact_impl`'s (which deliberately ignores retired facts so the
/// `remember` tool can re-save merged-away text). Re-importing the same
/// export must stay idempotent: matching text is counted as
/// `skipped_duplicate`, not `added`.
///
/// An import file is untrusted data like any other. `expires_at` is run back
/// through [`normalize_expiry`] and dropped (with an `errors` entry) if it
/// is not a real timestamp — an unvalidated one would silently make the
/// memory permanently invisible or permanently immortal — and `pinned` is
/// applied only while the target scope is under [`MAX_PINNED_PER_PROJECT`],
/// so an import cannot manufacture 100 pins against a stated ceiling of 20.
/// An entry with an unknown/missing scope, or one that fails validation, is
/// recorded in `errors` and does not abort the rest of the import.
pub fn import_impl(path: &Path, entries: &[MemoryExportEntry]) -> MemoryImportSummary {
    let mut added = 0usize;
    let mut skipped_duplicate = 0usize;
    let mut errors = Vec::new();
    // (exported id, newly generated id, storage root, source entry)
    let mut restored: Vec<(String, String, String, &MemoryEntry)> = Vec::new();

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

        if text_already_stored(path, &root, entry.text.trim()) {
            skipped_duplicate += 1;
            continue;
        }

        match add_fact_impl(
            path,
            &root,
            &entry.text,
            &entry.source,
            entry.source_turn_id.as_deref(),
        ) {
            Ok(fact) => {
                added += 1;
                restored.push((entry.id.clone(), fact.id, root, entry));
            }
            Err(e) => errors.push(format!("\"{}\": {}", truncate_for_error(&entry.text), e)),
        }
    }

    if !restored.is_empty() {
        if let Err(e) = apply_imported_lifecycle(path, &restored, &mut errors) {
            errors.push(e);
        }
    }

    MemoryImportSummary {
        added,
        skipped_duplicate,
        errors,
    }
}

/// Whether `root` already holds a fact with exactly this text — including
/// merge-retired ones, unlike [`add_fact_impl`]'s own check. See
/// [`import_impl`]'s doc comment for why the two differ.
fn text_already_stored(path: &Path, root: &str, text: &str) -> bool {
    load_impl(path)
        .ok()
        .and_then(|m| {
            m.projects
                .get(root)
                .map(|p| p.facts.iter().any(|f| f.text == text))
        })
        .unwrap_or(false)
}

/// [`import_impl`]'s single fix-up pass: re-apply the lifecycle fields
/// `add_fact_impl` always resets, and remap the merge relationship from the
/// export's ids to the newly generated ones.
fn apply_imported_lifecycle(
    path: &Path,
    restored: &[(String, String, String, &MemoryEntry)],
    errors: &mut Vec<String>,
) -> Result<(), String> {
    let remap: HashMap<&str, &str> = restored
        .iter()
        .map(|(old, new, _, _)| (old.as_str(), new.as_str()))
        .collect();
    let mut memories = load_impl(path)?;
    // Pins already on disk before this import still count toward the ceiling.
    let mut pin_counts: HashMap<String, usize> = HashMap::new();
    for (root, project) in memories.projects.iter() {
        pin_counts.insert(root.clone(), project.facts.iter().filter(|f| f.pinned).count());
    }

    for (_, new_id, root, entry) in restored {
        let expires_at = match entry.expires_at.as_deref() {
            Some(raw) => match normalize_expiry(raw) {
                Ok(value) => Some(value),
                Err(e) => {
                    errors.push(format!("\"{}\": {}", truncate_for_error(&entry.text), e));
                    None
                }
            },
            None => None,
        };
        let mut pinned = entry.pinned;
        if pinned {
            let count = pin_counts.entry(root.clone()).or_insert(0);
            if *count >= MAX_PINNED_PER_PROJECT {
                pinned = false;
                errors.push(format!(
                    "\"{}\": imported un-pinned — this scope is already at the {}-pin limit.",
                    truncate_for_error(&entry.text),
                    MAX_PINNED_PER_PROJECT
                ));
            } else {
                *count += 1;
            }
        }
        let Some(project) = memories.projects.get_mut(root) else {
            continue;
        };
        let Some(fact) = project.facts.iter_mut().find(|f| &f.id == new_id) else {
            continue;
        };
        fact.enabled = entry.enabled;
        fact.pinned = pinned;
        fact.expires_at = expires_at;
        fact.last_used_at = entry.last_used_at.clone();
        fact.retired_at = entry.retired_at.clone();
        fact.merged_from = entry
            .merged_from
            .iter()
            .filter_map(|old| remap.get(old.as_str()).map(|s| s.to_string()))
            .collect();
        fact.merged_into = entry
            .merged_into
            .as_deref()
            .and_then(|old| remap.get(old).map(|s| s.to_string()));
        // A retired fact whose merged child did not survive the import would
        // be invisible forever with no way back — restore it instead.
        if fact.merged_into.is_none() {
            fact.retired_at = None;
        }
    }
    save_impl(path, &memories)
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

/// Pin/unpin a memory Memory Studio is showing. A pinned memory is folded
/// into the prompt first and is exempt from expiry and the per-scope fact
/// cap — see [`set_pinned_impl`].
#[tauri::command]
pub fn memory_studio_set_pinned(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
    project_root: Option<String>,
    pinned: bool,
) -> Result<Fact, String> {
    let path = memories_file_path(&app)?;
    let _lock = state
        .memory_lock
        .lock()
        .map_err(|_| "Memory lock poisoned".to_string())?;
    set_pinned_impl(&path, &studio_scope(project_root), &id, pinned)
}

/// Set (or, with `None`, clear) a memory's expiry — see [`set_expiry_impl`].
#[tauri::command]
pub fn memory_studio_set_expiry(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
    project_root: Option<String>,
    expires_at: Option<String>,
) -> Result<Fact, String> {
    let path = memories_file_path(&app)?;
    let _lock = state
        .memory_lock
        .lock()
        .map_err(|_| "Memory lock poisoned".to_string())?;
    set_expiry_impl(
        &path,
        &studio_scope(project_root),
        &id,
        expires_at.as_deref(),
    )
}

/// Combine several memories in one scope into a single memory that keeps
/// their ids — see [`merge_impl`].
#[tauri::command]
pub fn memory_studio_merge(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    ids: Vec<String>,
    project_root: Option<String>,
    text: Option<String>,
) -> Result<Fact, String> {
    let path = memories_file_path(&app)?;
    let _lock = state
        .memory_lock
        .lock()
        .map_err(|_| "Memory lock poisoned".to_string())?;
    merge_impl(&path, &studio_scope(project_root), &ids, text.as_deref())
}

/// Undo a merge: restore the originals and drop the merged memory — see
/// [`unmerge_impl`].
#[tauri::command]
pub fn memory_studio_unmerge(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
    project_root: Option<String>,
) -> Result<usize, String> {
    let path = memories_file_path(&app)?;
    let _lock = state
        .memory_lock
        .lock()
        .map_err(|_| "Memory lock poisoned".to_string())?;
    unmerge_impl(&path, &studio_scope(project_root), &id)
}

/// Hard-delete every expired memory in every scope — see
/// [`purge_expired_impl`]. Deliberately not scoped to Memory Studio's
/// current filter.
#[tauri::command]
pub fn memory_studio_purge_expired(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<usize, String> {
    let path = memories_file_path(&app)?;
    let _lock = state
        .memory_lock
        .lock()
        .map_err(|_| "Memory lock poisoned".to_string())?;
    purge_expired_impl(&path)
}

/// Record that a system prompt was just assembled from these memories —
/// called by `systemPrompt.ts`'s `currentSystemPrompt`, the single funnel
/// every desktop prompt build goes through. See [`mark_used_impl`] for the
/// throttle and for what this timestamp does and does not mean.
#[tauri::command]
pub fn memory_mark_used(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    ids: Vec<String>,
) -> Result<usize, String> {
    let path = memories_file_path(&app)?;
    let _lock = state
        .memory_lock
        .lock()
        .map_err(|_| "Memory lock poisoned".to_string())?;
    mark_used_impl(&path, &ids)
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
    let raw =
        std::fs::read_to_string(&path).map_err(|e| format!("Failed to read import file: {}", e))?;
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

    /// The fixture is deliberately still v1-shaped (no `pinned`,
    /// `expires_at`, `last_used_at`, `merged_from`, `merged_into`,
    /// `retired_at`): it doubles as the migration proof shared with
    /// `rulesStore.test.ts`, which asserts `toEqual` against the same file.
    /// The v2 fields' TS<->Rust shape is therefore pinned by
    /// `memoryStudio.test.ts`'s wrapper payload assertions instead, and this
    /// test asserts their defaults below so the gap is deliberate, not
    /// forgotten.
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
        assert!(!fact.pinned);
        assert_eq!(fact.expires_at, None);
        assert_eq!(fact.last_used_at, None);
        assert!(fact.merged_from.is_empty());
        assert_eq!(fact.merged_into, None);
        assert_eq!(fact.retired_at, None);
    }

    #[test]
    fn fact_without_enabled_or_turn_id_defaults_to_enabled_true_none() {
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
        let fact =
            add_fact_impl(&path, "/ws/project", "Uses pnpm, not npm.", "agent", None).unwrap();

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
        let fact = add_fact_impl(
            &path,
            "/ws/project",
            "turn-scoped fact",
            "agent",
            Some("turn-42"),
        )
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
        let first = add_fact_impl(
            &path,
            "/ws/project",
            "Build with `make build`.",
            "agent",
            None,
        )
        .unwrap();
        let second = add_fact_impl(
            &path,
            "/ws/project",
            "Build with `make build`.",
            "agent",
            None,
        )
        .unwrap();

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
            add_fact_impl(
                &path,
                "/ws/project",
                &format!("fact number {n}"),
                "agent",
                None,
            )
            .unwrap();
        }

        let err =
            add_fact_impl(&path, "/ws/project", "one fact too many", "agent", None).unwrap_err();
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
        let fact = add_fact_impl(
            &path,
            GLOBAL_SCOPE_KEY,
            "User's name is Ahmad.",
            "agent",
            None,
        )
        .unwrap();

        let reloaded = load_impl(&path).unwrap();
        let facts = &reloaded.projects[GLOBAL_SCOPE_KEY].facts;
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].id, fact.id);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn owning_scope_finds_a_fact_in_the_global_bucket_even_with_a_project_open() {
        let path = temp_path();
        let global_fact =
            add_fact_impl(&path, GLOBAL_SCOPE_KEY, "global fact", "agent", None).unwrap();
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
        let global_kept =
            add_fact_impl(&path, GLOBAL_SCOPE_KEY, "global keep me", "agent", None).unwrap();
        let global_disabled =
            add_fact_impl(&path, GLOBAL_SCOPE_KEY, "global disable me", "agent", None).unwrap();

        set_enabled_impl(&path, "/ws/project", &disabled.id, false).unwrap();
        delete_fact_impl(&path, "/ws/project", &deleted.id).unwrap();
        set_enabled_impl(&path, GLOBAL_SCOPE_KEY, &global_disabled.id, false).unwrap();

        let facts = list_impl(&path, Some("/ws/project")).unwrap();
        let ids: Vec<&str> = facts.iter().map(|f| f.id.as_str()).collect();

        assert!(
            ids.contains(&kept.id.as_str()),
            "enabled project fact must still be injected"
        );
        assert!(
            ids.contains(&global_kept.id.as_str()),
            "enabled global fact must still be injected"
        );
        assert!(
            !ids.contains(&disabled.id.as_str()),
            "disabled fact must not enter a future prompt"
        );
        assert!(
            !ids.contains(&deleted.id.as_str()),
            "deleted fact must not enter a future prompt"
        );
        assert!(
            !ids.contains(&global_disabled.id.as_str()),
            "disabled global fact must not enter a future prompt"
        );
        assert_eq!(
            facts.len(),
            2,
            "only the two still-enabled facts should be returned"
        );

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
        assert_eq!(
            entries.len(),
            3,
            "list_all_impl must include disabled facts too"
        );

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
                    pinned: false,
                    expires_at: None,
                    last_used_at: None,
                    merged_from: Vec::new(),
                    merged_into: None,
                    retired_at: None,
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
                    pinned: false,
                    expires_at: None,
                    last_used_at: None,
                    merged_from: Vec::new(),
                    merged_into: None,
                    retired_at: None,
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
            load_impl(&path).unwrap().projects["/ws/imported"]
                .facts
                .len(),
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
                    pinned: false,
                    expires_at: None,
                    last_used_at: None,
                    merged_from: Vec::new(),
                    merged_into: None,
                    retired_at: None,
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
                    pinned: false,
                    expires_at: None,
                    last_used_at: None,
                    merged_from: Vec::new(),
                    merged_into: None,
                    retired_at: None,
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
        add_fact_impl(
            &source_path,
            "/ws/project",
            "round trip me",
            "agent",
            Some("turn-1"),
        )
        .unwrap();
        add_fact_impl(
            &source_path,
            GLOBAL_SCOPE_KEY,
            "global round trip",
            "user",
            None,
        )
        .unwrap();

        let listed = list_all_impl(&source_path).unwrap();
        let entries: Vec<MemoryExportEntry> = listed
            .into_iter()
            .map(|entry| MemoryExportEntry {
                entry,
                redacted: false,
            })
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
    const PAST: &str = "2000-01-01T00:00:00.000Z";
    const FUTURE: &str = "2999-01-01T00:00:00.000Z";

    #[test]
    fn a_v1_memories_file_loads_unchanged_and_resaves_at_v2() {
        // A literal v1 document, exactly as an installation predating this
        // slice wrote it — not a struct, so this really is the migration.
        let path = temp_path();
        std::fs::write(
            &path,
            r#"{"version":1,"projects":{"/ws/project":{"facts":[
              {"id":"a","text":"v1 fact","source":"agent","created_at":"2026-01-01T00:00:00.000Z"},
              {"id":"b","text":"disabled v1 fact","source":"user","created_at":"2026-01-02T00:00:00.000Z","enabled":false,"source_turn_id":"turn-9"}
            ]}}}"#,
        )
        .unwrap();

        let loaded = load_impl(&path).unwrap();
        let facts = &loaded.projects["/ws/project"].facts;
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].id, "a");
        assert_eq!(facts[0].text, "v1 fact");
        assert_eq!(facts[0].source, "agent");
        assert_eq!(facts[0].created_at, "2026-01-01T00:00:00.000Z");
        assert!(facts[0].enabled, "a v1 fact must never load as disabled");
        assert_eq!(facts[0].source_turn_id, None);
        assert!(!facts[1].enabled, "an explicit v1 `enabled:false` survives");
        assert_eq!(facts[1].source_turn_id.as_deref(), Some("turn-9"));
        for fact in facts {
            assert!(!fact.pinned);
            assert_eq!(fact.expires_at, None);
            assert_eq!(fact.last_used_at, None);
            assert!(fact.merged_from.is_empty());
            assert_eq!(fact.merged_into, None);
            assert_eq!(fact.retired_at, None);
        }

        save_impl(&path, &loaded).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let on_disk: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(on_disk["version"], 2, "a re-save must stamp v2");
        let reread = load_impl(&path).unwrap();
        assert_eq!(reread.projects["/ws/project"].facts, *facts);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pinned_facts_are_listed_first_by_list_impl() {
        let path = temp_path();
        add_fact_impl(&path, GLOBAL_SCOPE_KEY, "ordinary global", "agent", None).unwrap();
        add_fact_impl(&path, "/ws/project", "ordinary project", "agent", None).unwrap();
        let pinned =
            add_fact_impl(&path, "/ws/project", "pinned project", "agent", None).unwrap();
        set_pinned_impl(&path, "/ws/project", &pinned.id, true).unwrap();

        let facts = list_impl(&path, Some("/ws/project")).unwrap();
        assert_eq!(facts.len(), 3);
        assert_eq!(facts[0].id, pinned.id, "a pinned fact is folded in first");
        // Stable sort: the two unpinned facts keep global-before-project order.
        assert_eq!(facts[1].text, "ordinary global");
        assert_eq!(facts[2].text, "ordinary project");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn expired_and_merge_retired_facts_are_excluded_from_list_impl() {
        let path = temp_path();
        let kept = add_fact_impl(&path, "/ws/project", "keep me", "agent", None).unwrap();
        let expired = add_fact_impl(&path, "/ws/project", "expired", "agent", None).unwrap();
        let one = add_fact_impl(&path, "/ws/project", "merge one", "agent", None).unwrap();
        let two = add_fact_impl(&path, "/ws/project", "merge two", "agent", None).unwrap();
        set_expiry_impl(&path, "/ws/project", &expired.id, Some(PAST)).unwrap();
        let merged = merge_impl(
            &path,
            "/ws/project",
            &[one.id.clone(), two.id.clone()],
            Some("merged one and two"),
        )
        .unwrap();

        let facts = list_impl(&path, Some("/ws/project")).unwrap();
        let ids: Vec<&str> = facts.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&kept.id.as_str()));
        assert!(ids.contains(&merged.id.as_str()));
        assert!(
            !ids.contains(&expired.id.as_str()),
            "an expired fact must not enter a future prompt"
        );
        assert!(
            !ids.contains(&one.id.as_str()) && !ids.contains(&two.id.as_str()),
            "merge-retired originals must not enter a future prompt"
        );
        assert_eq!(facts.len(), 2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_pinned_fact_is_exempt_from_expiry() {
        let path = temp_path();
        let fact = add_fact_impl(&path, "/ws/project", "pinned and stale", "agent", None).unwrap();
        set_expiry_impl(&path, "/ws/project", &fact.id, Some(PAST)).unwrap();
        assert!(list_impl(&path, Some("/ws/project")).unwrap().is_empty());

        set_pinned_impl(&path, "/ws/project", &fact.id, true).unwrap();
        let facts = list_impl(&path, Some("/ws/project")).unwrap();
        assert_eq!(facts.len(), 1, "pinning exempts a fact from its own expiry");
        assert_eq!(facts[0].id, fact.id);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pinned_facts_do_not_count_toward_the_project_cap_and_have_their_own_ceiling() {
        let path = temp_path();
        let mut ids = Vec::new();
        for n in 0..MAX_FACTS_PER_PROJECT {
            ids.push(
                add_fact_impl(&path, "/ws/project", &format!("fact {n}"), "agent", None)
                    .unwrap()
                    .id,
            );
        }
        assert!(add_fact_impl(&path, "/ws/project", "one too many", "agent", None).is_err());

        for id in ids.iter().take(MAX_PINNED_PER_PROJECT) {
            set_pinned_impl(&path, "/ws/project", id, true).unwrap();
        }
        // 20 of the 100 are now pinned, so only 80 count — room for 20 more.
        add_fact_impl(&path, "/ws/project", "now there is room", "agent", None).unwrap();

        let err = set_pinned_impl(&path, "/ws/project", &ids[MAX_PINNED_PER_PROJECT], true)
            .unwrap_err();
        assert!(err.contains("pinned memories"), "unexpected error: {err}");
        // Re-pinning an already-pinned fact stays an idempotent success.
        set_pinned_impl(&path, "/ws/project", &ids[0], true).unwrap();

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn merge_creates_one_fact_with_parent_ids_and_leaves_the_originals_on_disk() {
        let path = temp_path();
        let one = add_fact_impl(&path, "/ws/project", "prefers pnpm", "agent", None).unwrap();
        let two = add_fact_impl(&path, "/ws/project", "never npm", "agent", None).unwrap();

        let merged = merge_impl(&path, "/ws/project", &[one.id.clone(), two.id.clone()], None)
            .unwrap();
        assert_eq!(merged.text, "prefers pnpm never npm");
        assert_eq!(merged.merged_from, vec![one.id.clone(), two.id.clone()]);
        assert_eq!(
            merged.source, "agent",
            "provenance is inherited when the parents agree, never fabricated as \"user\""
        );

        let stored = load_impl(&path).unwrap();
        let facts = &stored.projects["/ws/project"].facts;
        assert_eq!(facts.len(), 3, "the originals are kept, not deleted");
        for parent in [&one, &two] {
            let kept = facts.iter().find(|f| f.id == parent.id).unwrap();
            assert_eq!(kept.merged_into.as_deref(), Some(merged.id.as_str()));
            assert!(kept.retired_at.is_some());
        }
        // Memory Studio's browse view still shows them.
        let all = list_all_impl(&path).unwrap();
        assert_eq!(all.len(), 3);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_mixed_source_merge_falls_back_to_user() {
        let path = temp_path();
        let one = add_fact_impl(&path, "/ws/project", "agent said", "agent", None).unwrap();
        let two = add_fact_impl(&path, "/ws/project", "user said", "user", None).unwrap();
        let merged =
            merge_impl(&path, "/ws/project", &[one.id, two.id], Some("both said")).unwrap();
        assert_eq!(merged.source, "user");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unmerge_restores_the_originals_to_list_impl_and_removes_the_merged_fact() {
        let path = temp_path();
        let one = add_fact_impl(&path, "/ws/project", "first", "agent", None).unwrap();
        let two = add_fact_impl(&path, "/ws/project", "second", "agent", None).unwrap();
        let merged =
            merge_impl(&path, "/ws/project", &[one.id.clone(), two.id.clone()], None).unwrap();

        assert_eq!(unmerge_impl(&path, "/ws/project", &merged.id).unwrap(), 2);

        let facts = list_impl(&path, Some("/ws/project")).unwrap();
        let ids: Vec<&str> = facts.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&one.id.as_str()) && ids.contains(&two.id.as_str()));
        assert!(!ids.contains(&merged.id.as_str()));
        assert_eq!(facts.len(), 2);

        // An id with no merge behind it is a no-op success, not an error.
        assert_eq!(unmerge_impl(&path, "/ws/project", &one.id).unwrap(), 0);
        assert_eq!(unmerge_impl(&path, "/ws/project", "nope").unwrap(), 0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn merge_refuses_a_single_id_and_an_id_from_another_scope() {
        let path = temp_path();
        let mine = add_fact_impl(&path, "/ws/project", "mine", "agent", None).unwrap();
        let theirs = add_fact_impl(&path, GLOBAL_SCOPE_KEY, "theirs", "agent", None).unwrap();

        let err = merge_impl(&path, "/ws/project", &[mine.id.clone()], None).unwrap_err();
        assert!(err.contains("at least two"), "unexpected error: {err}");
        let err = merge_impl(
            &path,
            "/ws/project",
            &[mine.id.clone(), mine.id.clone()],
            None,
        )
        .unwrap_err();
        assert!(err.contains("at least two"), "the same id twice is one id");

        // Cross-scope is structurally impossible: one root per call, so the
        // global fact simply isn't found in the project scope.
        let err = merge_impl(&path, "/ws/project", &[mine.id, theirs.id], None).unwrap_err();
        assert!(err.contains("Fact not found"), "unexpected error: {err}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn merge_refuses_a_disabled_or_expired_parent() {
        // Without this, merging would launder a memory the user disabled (or
        // one that expired) straight back into the prompt as a live,
        // never-expiring fact.
        let path = temp_path();
        let live = add_fact_impl(&path, "/ws/project", "live", "agent", None).unwrap();
        let off = add_fact_impl(&path, "/ws/project", "disabled", "agent", None).unwrap();
        let stale = add_fact_impl(&path, "/ws/project", "expired", "agent", None).unwrap();
        set_enabled_impl(&path, "/ws/project", &off.id, false).unwrap();
        set_expiry_impl(&path, "/ws/project", &stale.id, Some(PAST)).unwrap();

        for parent in [&off, &stale] {
            let err =
                merge_impl(&path, "/ws/project", &[live.id.clone(), parent.id.clone()], None)
                    .unwrap_err();
            assert!(err.contains("currently reach the prompt"), "unexpected: {err}");
        }
        assert_eq!(load_impl(&path).unwrap().projects["/ws/project"].facts.len(), 3);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn deleting_a_merged_fact_also_deletes_the_originals_it_retired() {
        // "Forget" must not silently ADD two memories back into the next
        // prompt. Undo is `unmerge_impl`'s job; delete deletes.
        let path = temp_path();
        let one = add_fact_impl(&path, "/ws/project", "first", "agent", None).unwrap();
        let two = add_fact_impl(&path, "/ws/project", "second", "agent", None).unwrap();
        let merged =
            merge_impl(&path, "/ws/project", &[one.id.clone(), two.id.clone()], None).unwrap();

        delete_fact_impl(&path, "/ws/project", &merged.id).unwrap();
        assert!(
            load_impl(&path).unwrap().projects["/ws/project"]
                .facts
                .is_empty(),
            "no orphaned retired originals with a dangling merged_into"
        );
        assert!(list_impl(&path, Some("/ws/project")).unwrap().is_empty());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mark_used_impl_stamps_only_the_named_ids() {
        let path = temp_path();
        let used = add_fact_impl(&path, "/ws/project", "used", "agent", None).unwrap();
        let unused = add_fact_impl(&path, "/ws/project", "unused", "agent", None).unwrap();
        let global = add_fact_impl(&path, GLOBAL_SCOPE_KEY, "global used", "agent", None).unwrap();

        assert_eq!(
            mark_used_impl(&path, &[used.id.clone(), global.id.clone()]).unwrap(),
            2
        );
        let stored = load_impl(&path).unwrap();
        let find = |root: &str, id: &str| {
            stored.projects[root]
                .facts
                .iter()
                .find(|f| f.id == id)
                .unwrap()
                .last_used_at
                .clone()
        };
        assert!(find("/ws/project", &used.id).is_some());
        assert!(find(GLOBAL_SCOPE_KEY, &global.id).is_some());
        assert_eq!(find("/ws/project", &unused.id), None);

        // Throttled: a second mark inside the window writes nothing.
        assert_eq!(mark_used_impl(&path, &[used.id]).unwrap(), 0);
        assert_eq!(mark_used_impl(&path, &[]).unwrap(), 0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn set_expiry_impl_accepts_a_bare_date_and_rejects_junk() {
        let path = temp_path();
        let fact = add_fact_impl(&path, "/ws/project", "expiring", "agent", None).unwrap();

        let set = set_expiry_impl(&path, "/ws/project", &fact.id, Some("2026-12-31")).unwrap();
        assert_eq!(
            set.expires_at.as_deref(),
            Some("2026-12-31T23:59:59.999Z"),
            "a bare date expires at the END of that day, so picking today is still valid today"
        );
        // The same-day case the <input type=\"date\"> control actually produces.
        let today = &now_rfc3339()[..10];
        set_expiry_impl(&path, "/ws/project", &fact.id, Some(today)).unwrap();
        assert_eq!(
            list_impl(&path, Some("/ws/project")).unwrap().len(),
            1,
            "expiring today must not expire the memory the moment it is saved"
        );

        let full = set_expiry_impl(
            &path,
            "/ws/project",
            &fact.id,
            Some("2026-12-31T09:00:00.000Z"),
        )
        .unwrap();
        assert_eq!(full.expires_at.as_deref(), Some("2026-12-31T09:00:00.000Z"));

        for junk in ["1", "2026-1-1", "tomorrow", "2026-12-31T09:00:00"] {
            assert!(
                set_expiry_impl(&path, "/ws/project", &fact.id, Some(junk)).is_err(),
                "{junk} should be rejected"
            );
        }
        let cleared = set_expiry_impl(&path, "/ws/project", &fact.id, None).unwrap();
        assert_eq!(cleared.expires_at, None);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn purge_expired_impl_deletes_expired_facts_but_never_a_pinned_or_retired_one() {
        let path = temp_path();
        let expired = add_fact_impl(&path, "/ws/project", "expired", "agent", None).unwrap();
        let future = add_fact_impl(&path, "/ws/project", "not yet", "agent", None).unwrap();
        let pinned = add_fact_impl(&path, "/ws/project", "pinned stale", "agent", None).unwrap();
        let one = add_fact_impl(&path, GLOBAL_SCOPE_KEY, "one", "agent", None).unwrap();
        let two = add_fact_impl(&path, GLOBAL_SCOPE_KEY, "two", "agent", None).unwrap();
        set_expiry_impl(&path, "/ws/project", &expired.id, Some(PAST)).unwrap();
        set_expiry_impl(&path, "/ws/project", &future.id, Some(FUTURE)).unwrap();
        set_expiry_impl(&path, "/ws/project", &pinned.id, Some(PAST)).unwrap();
        set_pinned_impl(&path, "/ws/project", &pinned.id, true).unwrap();
        let merged =
            merge_impl(&path, GLOBAL_SCOPE_KEY, &[one.id.clone(), two.id.clone()], None).unwrap();
        set_expiry_impl(&path, GLOBAL_SCOPE_KEY, &one.id, Some(PAST)).unwrap();

        assert_eq!(purge_expired_impl(&path).unwrap(), 1);
        let stored = load_impl(&path).unwrap();
        let project_ids: Vec<&str> = stored.projects["/ws/project"]
            .facts
            .iter()
            .map(|f| f.id.as_str())
            .collect();
        assert!(!project_ids.contains(&expired.id.as_str()));
        assert!(project_ids.contains(&future.id.as_str()));
        assert!(project_ids.contains(&pinned.id.as_str()));
        assert_eq!(
            stored.projects[GLOBAL_SCOPE_KEY].facts.len(),
            3,
            "a merge-retired original is never purged — it is the undo material"
        );
        // Nothing left to purge is Ok(0), not an error.
        assert_eq!(purge_expired_impl(&path).unwrap(), 0);
        // ...and the retired original is still there to be restored.
        assert_eq!(unmerge_impl(&path, GLOBAL_SCOPE_KEY, &merged.id).unwrap(), 2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn scope_of_impl_finds_the_owning_scope_for_any_id() {
        let path = temp_path();
        let project = add_fact_impl(&path, "/ws/project", "mine", "agent", None).unwrap();
        let global = add_fact_impl(&path, GLOBAL_SCOPE_KEY, "theirs", "agent", None).unwrap();
        assert_eq!(
            scope_of_impl(&path, &project.id).unwrap().as_deref(),
            Some("/ws/project")
        );
        assert_eq!(
            scope_of_impl(&path, &global.id).unwrap().as_deref(),
            Some(GLOBAL_SCOPE_KEY)
        );
        assert_eq!(scope_of_impl(&path, "nope").unwrap(), None);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn import_impl_restores_pinned_expiry_and_the_merge_relationship() {
        let source_path = temp_path();
        let pinned =
            add_fact_impl(&source_path, "/ws/project", "pinned one", "agent", None).unwrap();
        set_pinned_impl(&source_path, "/ws/project", &pinned.id, true).unwrap();
        set_expiry_impl(&source_path, "/ws/project", &pinned.id, Some(FUTURE)).unwrap();
        let one = add_fact_impl(&source_path, "/ws/project", "part one", "agent", None).unwrap();
        let two = add_fact_impl(&source_path, "/ws/project", "part two", "agent", None).unwrap();
        let merged = merge_impl(
            &source_path,
            "/ws/project",
            &[one.id.clone(), two.id.clone()],
            Some("both parts"),
        )
        .unwrap();

        let entries: Vec<MemoryExportEntry> = list_all_impl(&source_path)
            .unwrap()
            .into_iter()
            .map(|entry| MemoryExportEntry {
                entry,
                redacted: false,
            })
            .collect();

        let dest_path = temp_path();
        let summary = import_impl(&dest_path, &entries);
        assert_eq!(summary.added, 4);
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);

        let dest = load_impl(&dest_path).unwrap();
        let facts = &dest.projects["/ws/project"].facts;
        let by_text = |t: &str| facts.iter().find(|f| f.text == t).unwrap();
        assert!(by_text("pinned one").pinned);
        assert_eq!(
            by_text("pinned one").expires_at.as_deref(),
            Some(FUTURE),
            "expiry survives the round trip"
        );
        let new_merged = by_text("both parts");
        let new_one = by_text("part one");
        let new_two = by_text("part two");
        assert_ne!(new_one.id, one.id, "ids are regenerated on import");
        assert_eq!(
            new_merged.merged_from,
            vec![new_one.id.clone(), new_two.id.clone()],
            "merged_from must name the NEW parent ids, not the export's"
        );
        assert_eq!(new_one.merged_into.as_deref(), Some(new_merged.id.as_str()));
        assert!(new_one.retired_at.is_some() && new_two.retired_at.is_some());

        // The merged content reaches the prompt exactly once.
        let listed = list_impl(&dest_path, Some("/ws/project")).unwrap();
        let texts: Vec<&str> = listed.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(texts.len(), 2);
        assert!(texts.contains(&"both parts") && texts.contains(&"pinned one"));
        assert!(!texts.contains(&"part one"));

        // Re-importing the same export is idempotent even across the merge.
        let again = import_impl(&dest_path, &entries);
        assert_eq!(again.added, 0);
        assert_eq!(again.skipped_duplicate, 4);
        assert_eq!(list_impl(&dest_path, Some("/ws/project")).unwrap().len(), 2);
        let _ = merged;

        let _ = std::fs::remove_file(&source_path);
        let _ = std::fs::remove_file(&dest_path);
    }

    #[test]
    fn import_impl_rejects_a_junk_expiry_and_caps_imported_pins() {
        let path = temp_path();
        let mut entries: Vec<MemoryExportEntry> = Vec::new();
        for n in 0..(MAX_PINNED_PER_PROJECT + 2) {
            entries.push(MemoryExportEntry {
                entry: MemoryEntry {
                    id: format!("import-{n}"),
                    text: format!("imported fact {n}"),
                    source: "user".to_string(),
                    created_at: "2026-01-01T00:00:00.000Z".to_string(),
                    enabled: true,
                    source_turn_id: None,
                    pinned: true,
                    expires_at: if n == 0 {
                        Some("not-a-date".to_string())
                    } else {
                        None
                    },
                    last_used_at: None,
                    merged_from: Vec::new(),
                    merged_into: None,
                    retired_at: None,
                    scope: "global".to_string(),
                    project_root: None,
                },
                redacted: false,
            });
        }

        let summary = import_impl(&path, &entries);
        assert_eq!(summary.added, MAX_PINNED_PER_PROJECT + 2);
        assert_eq!(
            summary.errors.len(),
            3,
            "one junk expiry plus two refused pins: {:?}",
            summary.errors
        );
        let stored = load_impl(&path).unwrap();
        let facts = &stored.projects[GLOBAL_SCOPE_KEY].facts;
        assert_eq!(
            facts.iter().filter(|f| f.pinned).count(),
            MAX_PINNED_PER_PROJECT,
            "an import must not manufacture pins past the stated ceiling"
        );
        assert!(
            facts.iter().all(|f| f.expires_at.is_none()),
            "an unparseable expiry is dropped, not stored"
        );

        let _ = std::fs::remove_file(&path);
    }
}
