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
//!    back, files that didn't exist before the turn are deleted. Before each
//!    file is touched, its current (post-turn) content is snapshotted into
//!    `<dir>/redo/<n>.bak`, and the manifest's `reverted` flag flips to
//!    `true` (persisted via an atomic rewrite, best-effort so revert still
//!    succeeds even if that write fails).
//! 5. `checkpoint_reapply` (the "Re-apply" button shown once a checkpoint is
//!    reverted) is the inverse: it plays the `redo/` backups back over the
//!    files revert touched and flips `reverted` back to `false`.
//!
//! Revert and reapply are both idempotent (a repeat call on an
//! already-reverted/-reapplied checkpoint is a no-op) and serialized per id
//! via `AppState::checkpoint_locks` — see `revert_impl`/`reapply_impl` and
//! `acquire_revert_lock`. Without both of those, a checkpoint reachable from
//! two UI surfaces at once (the transcript's `CheckpointRow` and the
//! timeline's `TimelineRow`), or re-reverted via "Restore to here" after an
//! individual revert, would silently corrupt its `redo/` backup with the
//! wrong content.
//!
//! Checkpoints live under `<app_data>/checkpoints/<uuid>/` and the newest
//! [`MAX_CHECKPOINTS`] (ranked by manifest `created_at_ms`, never filesystem
//! mtime — see `prune_old`) are kept, so old turns remain revertable across
//! app restarts without growing unboundedly. A checkpoint whose turn is
//! still in flight (no manifest yet) is never counted against that cap —
//! only swept separately once it's old enough to be certain it was
//! abandoned by a crash rather than genuinely still running.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tauri::Manager;

use crate::AppState;

/// How many finished checkpoints to keep on disk before pruning the oldest,
/// when the caller doesn't pass an explicit `max_keep`.
const MAX_CHECKPOINTS: usize = 20;

const MANIFEST_FILE: &str = "manifest.json";

/// Current on-disk manifest schema version — see [`CheckpointManifest`].
const MANIFEST_VERSION: u8 = 2;

/// One file recorded in a checkpoint.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct CheckpointEntry {
    /// Absolute path of the workspace file that was (about to be) mutated.
    pub path: String,
    /// Backup file name inside the checkpoint directory, when the file
    /// existed before the turn. `None` means the file was newly created by
    /// the turn, so reverting deletes it.
    pub backup: Option<String>,
    /// Backup file name inside `<dir>/redo/`, snapshotting this file's
    /// post-turn (pre-revert) content — written by `revert_impl` right
    /// before it overwrites/deletes the file, so `checkpoint_reapply` can
    /// play the turn's changes back. `None` before the first revert, or if
    /// the file was unexpectedly already missing at revert time (nothing to
    /// snapshot). Absent from manifests written before this field existed,
    /// hence `serde(default)` for read-compatibility.
    #[serde(default)]
    pub redo: Option<String>,
}

/// The versioned on-disk `manifest.json` (v2). v1 manifests were a bare
/// `Vec<CheckpointEntry>` with no metadata — [`parse_manifest`] falls back
/// to them and synthesizes defaults, so checkpoints written before the
/// upgrade stay revertable without any migration pass.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct CheckpointManifest {
    pub version: u8,
    /// Unix millis when the checkpoint's turn began.
    pub created_at_ms: u64,
    /// The session whose turn this checkpoint belongs to.
    pub session_id: String,
    /// Index of the turn's user message in the transcript — the target for
    /// conversation rewind.
    pub anchor_index: usize,
    /// First ~120 chars of the user prompt, for timeline labels and for
    /// validating that `anchor_index` still points at the same message.
    pub label: String,
    /// True if `tool_run_shell` executed during this turn — revert coverage
    /// is then only partial (shell side effects are not snapshotted).
    pub shell_ran: bool,
    /// Set on revert so list/timeline UIs can show state and offer Re-apply.
    pub reverted: bool,
    /// Id of whatever was this session's newest surviving checkpoint at the
    /// moment this one's turn began — a backward link (like a git parent
    /// commit) that lets the timeline detect a pruned gap in "Restore to
    /// here"'s newest-to-target chain: if a checkpoint's `prev_id` doesn't
    /// match the id of the next-older surviving checkpoint in that session,
    /// something in between was pruned. `None` for a session's first
    /// checkpoint, and for manifests written before this field existed
    /// (`serde(default)`) — those simply can't report a gap, which is a safe
    /// (if less informative) fallback, not a false positive.
    #[serde(default)]
    pub prev_id: Option<String>,
    pub entries: Vec<CheckpointEntry>,
}

/// An open checkpoint for one in-flight turn. Lives in `AppState::checkpoints`
/// keyed by its id until the turn's `checkpoint_end` removes it.
pub struct ActiveCheckpoint {
    pub dir: PathBuf,
    pub entries: Vec<CheckpointEntry>,
    /// Manifest metadata captured at `checkpoint_begin` time — written out
    /// (and echoed back to the frontend) by `checkpoint_end`.
    pub created_at_ms: u64,
    pub session_id: String,
    pub anchor_index: usize,
    pub label: String,
    /// Flipped by `record_shell` (future slice) when `tool_run_shell` runs
    /// during the turn. Always `false` until then.
    pub shell_ran: bool,
    /// Captured at `checkpoint_begin` time — see `CheckpointManifest::prev_id`.
    pub prev_id: Option<String>,
}

/// Summary returned to the frontend by `checkpoint_end`. The renamed fields
/// mirror the camelCase `CheckpointNotice` payload in `src/lib/agentLoop.ts`,
/// which stores this verbatim inside the transcript's checkpoint notice.
#[derive(serde::Serialize)]
pub struct CheckpointSummary {
    pub id: String,
    /// Absolute paths of every file the turn mutated. Empty means nothing
    /// was recorded and the checkpoint was discarded.
    pub files: Vec<String>,
    #[serde(rename = "anchorIndex")]
    pub anchor_index: usize,
    pub label: String,
    #[serde(rename = "shellRan")]
    pub shell_ran: bool,
}

/// Summary of one checkpoint on disk, returned by `checkpoint_list` for the
/// timeline UI. Lighter than [`CheckpointManifest`] (no per-file paths) —
/// `files` is just a count, which is all "N files changed" rows need.
#[derive(serde::Serialize, Clone)]
pub struct CheckpointInfo {
    pub id: String,
    #[serde(rename = "createdAtMs")]
    pub created_at_ms: u64,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "anchorIndex")]
    pub anchor_index: usize,
    pub label: String,
    pub files: usize,
    #[serde(rename = "shellRan")]
    pub shell_ran: bool,
    pub reverted: bool,
    /// True once `checkpoint_reapply` actually has something to play back —
    /// the checkpoint has been reverted AND at least one entry recorded a
    /// `redo` backup. Lets the timeline hide a "Re-apply" that would be a
    /// silent no-op (e.g. a reverted v1 checkpoint predating redo support).
    pub reapplyable: bool,
    /// Mirrors `CheckpointManifest::prev_id` — lets the timeline detect a
    /// pruned gap in a session's chain (see that field's doc comment).
    #[serde(rename = "prevId")]
    pub prev_id: Option<String>,
}

impl CheckpointInfo {
    fn from_manifest(id: String, manifest: &CheckpointManifest) -> Self {
        let reapplyable = manifest.reverted && manifest.entries.iter().any(|e| e.redo.is_some());
        CheckpointInfo {
            id,
            created_at_ms: manifest.created_at_ms,
            session_id: manifest.session_id.clone(),
            anchor_index: manifest.anchor_index,
            label: manifest.label.clone(),
            files: manifest.entries.len(),
            shell_ran: manifest.shell_ran,
            reverted: manifest.reverted,
            reapplyable,
            prev_id: manifest.prev_id.clone(),
        }
    }
}

fn checkpoints_base_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {}", e))?
        .join("checkpoints");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create checkpoints dir: {}", e))?;
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

/// A manifest-less directory older than this is treated as abandoned by a
/// crashed/killed turn rather than genuinely in flight — no legitimate turn
/// plausibly stays open this long — and becomes eligible for cleanup so a
/// crash doesn't leak its checkpoint directory forever.
const ABANDONED_IN_FLIGHT_MAX_AGE_MS: u64 = 24 * 60 * 60 * 1000;

/// Delete the oldest *finished* checkpoint directories beyond `max_keep`,
/// ranked by the immutable `created_at_ms` recorded in each one's manifest —
/// never by filesystem mtime, which `revert_impl`/`reapply_impl` bump every
/// time they rewrite the manifest (a revert of an old checkpoint must not
/// make it look newer than one that was never touched).
///
/// A directory with no readable manifest is never counted against
/// `max_keep` — that's a checkpoint whose turn is still in flight
/// (`checkpoint_end` hasn't written `manifest.json` yet), so deleting it out
/// from under the turn would corrupt or abort it. This mirrors `list_impl`'s
/// own "no manifest = skip" treatment of in-flight checkpoints, just applied
/// to pruning instead of listing. It's only ever removed separately, and
/// only once [`ABANDONED_IN_FLIGHT_MAX_AGE_MS`] has passed with no
/// `checkpoint_end` — i.e. once it can no longer plausibly be a real
/// in-flight turn, just a crash's leftovers.
///
/// Best-effort: pruning failures never fail the turn that triggered them.
fn prune_old(base_dir: &Path, max_keep: usize) {
    let Ok(read_dir) = std::fs::read_dir(base_dir) else {
        return;
    };

    let now = now_ms();
    let mut finished: Vec<(u64, PathBuf)> = Vec::new();

    for entry in read_dir.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        if let Ok(manifest) = read_manifest(base_dir, &id) {
            finished.push((manifest.created_at_ms, path));
            continue;
        }

        let age_ms = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| now.saturating_sub(d.as_millis() as u64));
        if age_ms.is_some_and(|age| age > ABANDONED_IN_FLIGHT_MAX_AGE_MS) {
            let _ = std::fs::remove_dir_all(&path);
        }
    }

    if finished.len() <= max_keep {
        return;
    }

    finished.sort_by_key(|(created_at_ms, _)| *created_at_ms);
    let excess = finished.len() - max_keep;
    for (_, path) in finished.into_iter().take(excess) {
        let _ = std::fs::remove_dir_all(path);
    }
}

/// Process-lifetime high-water mark for [`now_ms`] — see that function's own
/// doc comment for why this exists.
static LAST_NOW_MS: AtomicU64 = AtomicU64::new(0);

/// Wall-clock milliseconds since the epoch, but guaranteed strictly
/// increasing across calls within this process (never ties, never goes
/// backwards even across a clock adjustment). `begin_impl` stamps every
/// checkpoint with this as `created_at_ms`, and `list_impl` sorts purely on
/// that value for its newest-first ordering — millisecond wall-clock
/// resolution is coarse enough that two checkpoints created back-to-back
/// (routine in fast test runs, and possible on a fast enough machine in
/// normal use) can land in the same millisecond. A tie there falls back to
/// `std::fs::read_dir`'s iteration order, which is unspecified and differs
/// by platform/filesystem — that's what made
/// `list_exposes_prev_id_so_the_timeline_can_detect_a_pruned_gap` fail
/// reliably on Linux CI runners while always passing locally on macOS.
fn now_ms() -> u64 {
    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut last = LAST_NOW_MS.load(Ordering::SeqCst);
    loop {
        let next = wall.max(last + 1);
        match LAST_NOW_MS.compare_exchange_weak(last, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return next,
            Err(actual) => last = actual,
        }
    }
}

/// Core begin logic, parameterized by base dir for testability. The metadata
/// (session, transcript anchor, prompt label) is frontend-supplied and rides
/// along in the [`ActiveCheckpoint`] until `checkpoint_end` persists it into
/// the manifest; `max_keep` feeds retention pruning (defaults to
/// [`MAX_CHECKPOINTS`]) so a settings-driven value needs no backend state.
pub fn begin_impl(
    state: &AppState,
    base_dir: &Path,
    session_id: String,
    anchor_index: usize,
    label: String,
    max_keep: Option<usize>,
) -> Result<String, String> {
    prune_old(base_dir, max_keep.unwrap_or(MAX_CHECKPOINTS).max(1));

    // The current head of this session's chain, if any — recorded as this
    // checkpoint's `prev_id` so the timeline can later detect a pruned gap
    // (see `CheckpointManifest::prev_id`). Best-effort: an unreadable
    // checkpoints dir just means no known predecessor, not a hard failure.
    let prev_id = list_impl(base_dir, Some(&session_id))
        .ok()
        .and_then(|list| list.into_iter().next())
        .map(|info| info.id);

    let id = uuid::Uuid::new_v4().to_string();
    let dir = base_dir.join(&id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create checkpoint dir: {}", e))?;

    // A checkpoint whose turn crashed mid-flight (its `checkpoint_end` never
    // arrives) stays in the map until app restart — a few stray entries at
    // most, and its manifest-less directory on disk is swept by `prune_old`
    // once it's old enough to no longer look like a real in-flight turn.
    state
        .checkpoints
        .lock()
        .map_err(|_| "Checkpoint lock poisoned".to_string())?
        .insert(
            id.clone(),
            ActiveCheckpoint {
                dir,
                entries: Vec::new(),
                created_at_ms: now_ms(),
                session_id,
                anchor_index,
                label,
                shell_ran: false,
                prev_id,
            },
        );

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
        std::fs::copy(resolved, active.dir.join(&backup_name)).map_err(|e| {
            format!(
                "Failed to back up '{}' before modifying it: {}",
                path_str, e
            )
        })?;
        Some(backup_name)
    } else {
        None
    };

    active.entries.push(CheckpointEntry {
        path: path_str,
        backup,
        redo: None,
    });
    Ok(())
}

/// Marks checkpoint `id`'s turn as having run `tool_run_shell` (no-op if `id`
/// is `None` or unknown — mirrors [`record_original`]'s tolerance). Called by
/// `tool_run_shell` BEFORE spawning the command. No snapshotting happens here:
/// shell side effects are never captured, so this only makes the manifest's
/// `shell_ran` flag (and therefore the UI's revert-coverage caveat) honest.
pub fn record_shell(state: &AppState, id: Option<&str>) -> Result<(), String> {
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

    active.shell_ran = true;
    Ok(())
}

/// Parses a raw `manifest.json`, falling back from the versioned v2 struct
/// to the bare v1 `Vec<CheckpointEntry>` shape and synthesizing defaults
/// (creation time from the checkpoint dir's mtime, empty session/label) so
/// pre-upgrade checkpoints keep working with no migration pass.
fn parse_manifest(raw: &str, dir: &Path, id: &str) -> Result<CheckpointManifest, String> {
    if let Ok(manifest) = serde_json::from_str::<CheckpointManifest>(raw) {
        return Ok(manifest);
    }

    let entries: Vec<CheckpointEntry> = serde_json::from_str(raw)
        .map_err(|e| format!("Checkpoint '{}' manifest is corrupt: {}", id, e))?;
    let created_at_ms = std::fs::metadata(dir)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    Ok(CheckpointManifest {
        version: 1,
        created_at_ms,
        session_id: String::new(),
        anchor_index: 0,
        label: String::new(),
        shell_ran: false,
        reverted: false,
        prev_id: None,
        entries,
    })
}

/// Reads and parses checkpoint `id`'s manifest from its directory.
fn read_manifest(base_dir: &Path, id: &str) -> Result<CheckpointManifest, String> {
    let dir = base_dir.join(id);
    let raw = std::fs::read_to_string(dir.join(MANIFEST_FILE))
        .map_err(|e| format!("Checkpoint '{}' not found or unreadable: {}", id, e))?;
    parse_manifest(&raw, &dir, id)
}

/// Atomic manifest write: sibling temp file + rename, same idiom as
/// `sessions.rs`, so a crash mid-write can never leave a truncated manifest
/// that would make the checkpoint unrevertable.
fn write_manifest(dir: &Path, manifest: &CheckpointManifest) -> Result<(), String> {
    let payload = serde_json::to_string_pretty(manifest)
        .map_err(|e| format!("Failed to serialize checkpoint manifest: {}", e))?;
    let path = dir.join(MANIFEST_FILE);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, payload)
        .map_err(|e| format!("Failed to write checkpoint manifest: {}", e))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("Failed to finalize checkpoint manifest: {}", e))?;
    Ok(())
}

/// Core end logic: close checkpoint `id`, persist its manifest (or discard
/// the empty directory), and report what was touched plus the metadata the
/// frontend embeds in the transcript's checkpoint notice.
pub fn end_impl(state: &AppState, id: &str) -> Result<CheckpointSummary, String> {
    let taken = state
        .checkpoints
        .lock()
        .map_err(|_| "Checkpoint lock poisoned".to_string())?
        .remove(id);

    let Some(active) = taken else {
        return Ok(CheckpointSummary {
            id: String::new(),
            files: Vec::new(),
            anchor_index: 0,
            label: String::new(),
            shell_ran: false,
        });
    };

    if active.entries.is_empty() {
        let _ = std::fs::remove_dir_all(&active.dir);
        return Ok(CheckpointSummary {
            id: id.to_string(),
            files: Vec::new(),
            anchor_index: active.anchor_index,
            label: active.label,
            shell_ran: active.shell_ran,
        });
    }

    let manifest = CheckpointManifest {
        version: MANIFEST_VERSION,
        created_at_ms: active.created_at_ms,
        session_id: active.session_id,
        anchor_index: active.anchor_index,
        label: active.label.clone(),
        shell_ran: active.shell_ran,
        reverted: false,
        prev_id: active.prev_id,
        entries: active.entries.clone(),
    };
    write_manifest(&active.dir, &manifest)?;

    Ok(CheckpointSummary {
        id: id.to_string(),
        files: active.entries.iter().map(|e| e.path.clone()).collect(),
        anchor_index: active.anchor_index,
        label: active.label,
        shell_ran: active.shell_ran,
    })
}

/// Subdirectory (inside a checkpoint's own dir) holding redo backups —
/// snapshots of each file's post-turn content taken right before `revert`
/// overwrites/deletes it, so `checkpoint_reapply` can play the turn back.
const REDO_DIR: &str = "redo";

/// Core revert logic: restore every file recorded in checkpoint `id`.
/// Returns how many files were restored/removed.
///
/// Before mutating each file, snapshots its current (post-turn) content into
/// `redo/<n>.bak` — best-effort: a failed snapshot leaves `entry.redo` as
/// `None`, which degrades that file to a no-op on a later `checkpoint_reapply`
/// rather than blocking the revert itself. The flipped `reverted` flag and
/// the redo backups are persisted via an atomic manifest rewrite; that write
/// is likewise best-effort so a read-only app-data dir (or any other
/// persistence failure) never prevents the revert from actually happening.
///
/// Idempotent: if `id` is already reverted, this is a no-op that returns
/// `Ok(0)` rather than re-running the snapshot-then-restore steps. Without
/// this guard, a second revert of an already-reverted checkpoint would
/// snapshot the file's *current* (already-restored, pre-turn) content into
/// `redo/<n>.bak`, clobbering the true post-turn content the first revert
/// recorded there and permanently losing the turn's real changes out from
/// under a later `checkpoint_reapply`. Two independent, ordinary-usage call
/// sites can otherwise reach this: `CheckpointTimeline.tsx`'s "Restore to
/// here" re-reverts every checkpoint newest→target unconditionally
/// (including ones the user already reverted individually earlier), and the
/// CLI's `/revert`/`monkey revert` can simply be invoked twice on the same id.
pub fn revert_impl(base_dir: &Path, id: &str) -> Result<u32, String> {
    validate_id(id)?;

    let dir = base_dir.join(id);
    let mut manifest = read_manifest(base_dir, id)?;
    if manifest.reverted {
        return Ok(0);
    }
    let redo_dir = dir.join(REDO_DIR);

    let mut reverted = 0u32;
    let mut errors: Vec<String> = Vec::new();

    for (i, entry) in manifest.entries.iter_mut().enumerate() {
        let target = Path::new(&entry.path);

        // Snapshot the post-turn content before it's overwritten/deleted.
        // Best-effort: a snapshot failure must not block the revert.
        if target.is_file() && std::fs::create_dir_all(&redo_dir).is_ok() {
            let redo_name = format!("{}.bak", i);
            if std::fs::copy(target, redo_dir.join(&redo_name)).is_ok() {
                entry.redo = Some(redo_name);
            }
        }

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

    manifest.reverted = true;
    let _ = write_manifest(&dir, &manifest);

    if !errors.is_empty() {
        return Err(format!(
            "Reverted {} of {} files; failures: {}",
            reverted,
            manifest.entries.len(),
            errors.join("; ")
        ));
    }

    Ok(reverted)
}

/// Core reapply ("redo") logic: undoes a previous `revert_impl` on checkpoint
/// `id` by playing its `redo/` backups back over the files revert touched —
/// the inverse operation, restoring the turn's own changes. Returns how many
/// files were reapplied. Persists `reverted: false` the same best-effort way
/// `revert_impl` persists `reverted: true`.
///
/// Per entry: if a redo backup was recorded, it always wins — copying it
/// back over the target recreates a turn-created file exactly as `revert`
/// left it, or re-mutates a turn-modified file back to its post-turn state.
/// Only when there is no redo backup *and* the file was newly created by the
/// turn (`backup: None`) does reapply fall back to deleting the target — a
/// file that never existed before the turn and had nothing to snapshot at
/// revert time (i.e. was already gone) has nothing to recreate. A
/// pre-existing file (`backup: Some`) with no redo backup is left untouched:
/// that's an anomaly (the file should have existed at revert time), and
/// deleting a restored original would be strictly worse than a no-op.
///
/// Idempotent, mirroring `revert_impl`: if `id` isn't currently reverted,
/// this is a no-op returning `Ok(0)` rather than replaying redo backups over
/// files that were never touched by a revert.
pub fn reapply_impl(base_dir: &Path, id: &str) -> Result<u32, String> {
    validate_id(id)?;

    let dir = base_dir.join(id);
    let mut manifest = read_manifest(base_dir, id)?;
    if !manifest.reverted {
        return Ok(0);
    }
    let redo_dir = dir.join(REDO_DIR);

    let mut reapplied = 0u32;
    let mut errors: Vec<String> = Vec::new();

    for entry in &manifest.entries {
        let target = Path::new(&entry.path);
        let result = match (&entry.redo, &entry.backup) {
            (Some(redo_name), _) => std::fs::copy(redo_dir.join(redo_name), target).map(|_| ()),
            (None, None) => match std::fs::remove_file(target) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            },
            (None, Some(_)) => Ok(()),
        };
        match result {
            Ok(()) => reapplied += 1,
            Err(e) => errors.push(format!("{}: {}", entry.path, e)),
        }
    }

    manifest.reverted = false;
    let _ = write_manifest(&dir, &manifest);

    if !errors.is_empty() {
        return Err(format!(
            "Re-applied {} of {} files; failures: {}",
            reapplied,
            manifest.entries.len(),
            errors.join("; ")
        ));
    }

    Ok(reapplied)
}

/// Core list logic, parameterized by base dir for testability: scans every
/// checkpoint directory under `base_dir`, reads its manifest (v2, or the v1
/// fallback via [`parse_manifest`]), and returns a [`CheckpointInfo`] per
/// finished checkpoint, newest-first. A directory with no manifest yet (a
/// turn still in flight — `checkpoint_end` hasn't run) or an unreadable/
/// corrupt one is silently skipped rather than failing the whole list.
/// `session_id` optionally restricts the result to one session's checkpoints
/// (used by the timeline's "Restore to here" chain, which is only
/// well-defined within a single session).
pub fn list_impl(base_dir: &Path, session_id: Option<&str>) -> Result<Vec<CheckpointInfo>, String> {
    let read_dir = std::fs::read_dir(base_dir)
        .map_err(|e| format!("Failed to read checkpoints dir: {}", e))?;

    let mut infos: Vec<CheckpointInfo> = Vec::new();
    for entry in read_dir.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        let Ok(manifest) = read_manifest(base_dir, &id) else {
            continue;
        };
        if let Some(filter) = session_id {
            if manifest.session_id != filter {
                continue;
            }
        }
        infos.push(CheckpointInfo::from_manifest(id, &manifest));
    }

    infos.sort_by(|a, b| b.created_at_ms.cmp(&a.created_at_ms));
    Ok(infos)
}

/// Lists checkpoints newest-first for the timeline UI, optionally filtered to
/// one session. Read-only UI plumbing — like `checkpoint_revert`,
/// intentionally NOT routed through the permission system.
#[tauri::command]
pub fn checkpoint_list(
    app: tauri::AppHandle,
    session_id: Option<String>,
) -> Result<Vec<CheckpointInfo>, String> {
    list_impl(&checkpoints_base_dir(&app)?, session_id.as_deref())
}

/// Open a new per-turn checkpoint and return its id. `session_id`,
/// `anchor_index` (the turn's user-message index in the transcript) and
/// `label` (prompt prefix) are supplied by the frontend agent loop and end
/// up in the manifest; `max_keep` overrides the default retention cap.
#[tauri::command]
pub fn checkpoint_begin(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    session_id: String,
    anchor_index: usize,
    label: String,
    max_keep: Option<usize>,
) -> Result<String, String> {
    begin_impl(
        state.inner(),
        &checkpoints_base_dir(&app)?,
        session_id,
        anchor_index,
        label,
        max_keep,
    )
}

/// Close checkpoint `id`; returns the touched files (empty = nothing was
/// mutated this turn and the checkpoint was discarded).
#[tauri::command]
pub fn checkpoint_end(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<CheckpointSummary, String> {
    end_impl(state.inner(), &id)
}

/// RAII guard for one entry in `AppState::checkpoint_locks`: removes `id`
/// from the in-progress set on drop (including on early return via `?`), so
/// a lock can never get stuck if revert/reapply errors out or panics.
struct RevertLockGuard<'a> {
    state: &'a AppState,
    id: String,
}

impl Drop for RevertLockGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut locks) = self.state.checkpoint_locks.lock() {
            locks.remove(&self.id);
        }
    }
}

/// Claims the revert/reapply lock for checkpoint `id`, erroring if another
/// revert or reapply for the same id is already in progress — see
/// `AppState::checkpoint_locks`'s doc comment for why this is needed.
fn acquire_revert_lock<'a>(state: &'a AppState, id: &str) -> Result<RevertLockGuard<'a>, String> {
    let mut locks = state
        .checkpoint_locks
        .lock()
        .map_err(|_| "Checkpoint lock poisoned".to_string())?;
    if !locks.insert(id.to_string()) {
        return Err(format!("Checkpoint '{}' is already being restored", id));
    }
    Ok(RevertLockGuard {
        state,
        id: id.to_string(),
    })
}

/// Restore every file recorded in checkpoint `id` to its pre-turn state.
/// A direct, human-initiated UI action (the transcript's "Revert" button) —
/// like `git_commit`, intentionally NOT routed through the permission system.
#[tauri::command]
pub fn checkpoint_revert(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<u32, String> {
    let _lock = acquire_revert_lock(state.inner(), &id)?;
    revert_impl(&checkpoints_base_dir(&app)?, &id)
}

/// Undo a previous revert of checkpoint `id`: plays its `redo/` backups back
/// over the files revert touched, restoring the turn's own changes. Like
/// `checkpoint_revert`, a direct human-initiated UI action, not permission-gated.
#[tauri::command]
pub fn checkpoint_reapply(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<u32, String> {
    let _lock = acquire_revert_lock(state.inner(), &id)?;
    reapply_impl(&checkpoints_base_dir(&app)?, &id)
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

    /// `begin_impl` with default metadata, for tests that don't care about it.
    fn begin(state: &AppState, base_dir: &Path) -> String {
        begin_impl(
            state,
            base_dir,
            "test-session".to_string(),
            0,
            "test prompt".to_string(),
            None,
        )
        .unwrap()
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
    fn record_shell_sets_the_flag_and_is_a_noop_without_a_checkpoint_id() {
        let state = AppState::default();
        let base = TempDir::new("base");

        // No-op cases: no id, or an id with no active checkpoint.
        record_shell(&state, None).unwrap();
        record_shell(&state, Some("0000-unknown-id")).unwrap();

        let id = begin(&state, &base.path);
        assert!(!state.checkpoints.lock().unwrap()[&id].shell_ran);

        record_shell(&state, Some(&id)).unwrap();
        assert!(
            state.checkpoints.lock().unwrap()[&id].shell_ran,
            "shell_ran must flip to true"
        );

        // The flag must survive into the persisted manifest via checkpoint_end.
        let ws = TempDir::new("ws");
        let file = ws.path.join("a.txt");
        std::fs::write(&file, "x").unwrap();
        record_original(&state, Some(&id), &file).unwrap();
        let summary = end_impl(&state, &id).unwrap();
        assert!(summary.shell_ran);
        assert!(read_manifest(&base.path, &id).unwrap().shell_ran);
    }

    #[test]
    fn end_discards_an_empty_checkpoint() {
        let state = AppState::default();
        let base = TempDir::new("base");

        let id = begin(&state, &base.path);
        let summary = end_impl(&state, &id).unwrap();

        assert_eq!(summary.id, id);
        assert!(summary.files.is_empty());
        assert!(
            !base.path.join(&id).exists(),
            "empty checkpoint dir must be removed"
        );
    }

    #[test]
    fn revert_restores_modified_file_and_deletes_created_file() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let existing = ws.path.join("existing.txt");
        std::fs::write(&existing, "original").unwrap();
        let created = ws.path.join("created.txt");

        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &existing).unwrap();
        std::fs::write(&existing, "mutated").unwrap();
        record_original(&state, Some(&id), &created).unwrap();
        std::fs::write(&created, "brand new").unwrap();
        let summary = end_impl(&state, &id).unwrap();

        assert_eq!(summary.files.len(), 2);

        let reverted = revert_impl(&base.path, &id).unwrap();
        assert_eq!(reverted, 2);
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "original");
        assert!(
            !created.exists(),
            "file created during the turn must be deleted on revert"
        );
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
        let id_a = begin(&state, &base.path);
        let id_b = begin(&state, &base.path);
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

        let id = begin(&state, &base.path);
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
        assert!(
            err.contains("Invalid checkpoint id"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn revert_errors_for_unknown_id() {
        let base = TempDir::new("base");
        let err = revert_impl(&base.path, "0000-does-not-exist").unwrap_err();
        assert!(err.contains("not found"), "unexpected error: {err}");
    }

    #[test]
    fn end_writes_a_v2_manifest_with_metadata_and_echoes_it() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file = ws.path.join("a.txt");
        std::fs::write(&file, "original").unwrap();

        let before_ms = now_ms();
        let id = begin_impl(
            &state,
            &base.path,
            "session-42".to_string(),
            7,
            "fix the login bug".to_string(),
            None,
        )
        .unwrap();
        record_original(&state, Some(&id), &file).unwrap();
        std::fs::write(&file, "mutated").unwrap();
        let summary = end_impl(&state, &id).unwrap();

        // The summary echoes the metadata the frontend embeds in its notice.
        assert_eq!(summary.anchor_index, 7);
        assert_eq!(summary.label, "fix the login bug");
        assert!(!summary.shell_ran);

        let manifest = read_manifest(&base.path, &id).unwrap();
        assert_eq!(manifest.version, MANIFEST_VERSION);
        assert_eq!(manifest.session_id, "session-42");
        assert_eq!(manifest.anchor_index, 7);
        assert_eq!(manifest.label, "fix the login bug");
        assert!(!manifest.shell_ran);
        assert!(!manifest.reverted);
        assert!(
            manifest.created_at_ms >= before_ms,
            "created_at_ms must be set at begin time"
        );
        assert_eq!(manifest.entries.len(), 1);
    }

    #[test]
    fn v1_manifest_on_disk_still_reverts() {
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let existing = ws.path.join("existing.txt");
        std::fs::write(&existing, "mutated").unwrap();
        let created = ws.path.join("created.txt");
        std::fs::write(&created, "brand new").unwrap();

        // A real pre-upgrade checkpoint: a bare entry array, exactly as the
        // old `end_impl` serialized it.
        let id = "00000000-0000-4000-8000-00000000v1ok";
        let dir = base.path.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("0.bak"), "original").unwrap();
        // Hand-written JSON (no "redo" key at all, unlike anything this
        // version of the code would serialize) so the fallback path is
        // genuinely exercised, not just an equivalent Rust struct.
        let raw = format!(
            r#"[{{"path":{:?},"backup":"0.bak"}},{{"path":{:?},"backup":null}}]"#,
            existing.to_string_lossy(),
            created.to_string_lossy()
        );
        std::fs::write(dir.join(MANIFEST_FILE), raw).unwrap();

        let reverted = revert_impl(&base.path, id).unwrap();
        assert_eq!(reverted, 2);
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "original");
        assert!(
            !created.exists(),
            "file created during the v1 turn must be deleted on revert"
        );
    }

    #[test]
    fn v1_manifest_reads_with_synthesized_defaults() {
        let base = TempDir::new("base");

        let id = "00000000-0000-4000-8000-0000000v1meta";
        let dir = base.path.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let raw = r#"[{"path":"/tmp/a.txt","backup":"0.bak"}]"#;
        std::fs::write(dir.join(MANIFEST_FILE), raw).unwrap();

        let manifest = read_manifest(&base.path, id).unwrap();
        assert_eq!(manifest.version, 1);
        assert!(manifest.session_id.is_empty());
        assert_eq!(manifest.anchor_index, 0);
        assert!(manifest.label.is_empty());
        assert!(!manifest.shell_ran);
        assert!(!manifest.reverted);
        assert!(
            manifest.created_at_ms > 0,
            "created_at_ms must be synthesized from the dir mtime"
        );
        assert_eq!(manifest.entries.len(), 1);
    }

    #[test]
    fn begin_prunes_to_the_supplied_max_keep() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        // Four *finished* checkpoints (each with a real manifest.json),
        // oldest first by created_at_ms.
        let mut ids = Vec::new();
        for n in 0..4 {
            let file = ws.path.join(format!("f{n}.txt"));
            std::fs::write(&file, "x").unwrap();
            let id = begin_impl(
                &state,
                &base.path,
                "s".to_string(),
                0,
                "p".to_string(),
                None,
            )
            .unwrap();
            record_original(&state, Some(&id), &file).unwrap();
            std::fs::write(&file, "y").unwrap();
            end_impl(&state, &id).unwrap();
            ids.push(id);
            std::thread::sleep(std::time::Duration::from_millis(15));
        }

        let id = begin_impl(
            &state,
            &base.path,
            "s".to_string(),
            0,
            "p".to_string(),
            Some(2),
        )
        .unwrap();

        let remaining: Vec<String> = std::fs::read_dir(&base.path)
            .unwrap()
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        // The two oldest were pruned; the two newest plus the just-opened
        // checkpoint remain.
        assert_eq!(remaining.len(), 3, "remaining: {remaining:?}");
        assert!(
            !remaining.contains(&ids[0]),
            "oldest checkpoint must be pruned"
        );
        assert!(
            !remaining.contains(&ids[1]),
            "second-oldest checkpoint must be pruned"
        );
        assert!(remaining.contains(&ids[2]));
        assert!(remaining.contains(&ids[3]));
        assert!(remaining.contains(&id));
    }

    #[test]
    fn prune_never_deletes_an_in_flight_checkpoint_regardless_of_age() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        // Turn A begins and stays open (no checkpoint_end) — its directory
        // has no manifest.json yet.
        let id_a = begin_impl(
            &state,
            &base.path,
            "s".to_string(),
            0,
            "p".to_string(),
            None,
        )
        .unwrap();
        assert!(base.path.join(&id_a).is_dir());

        // Several other turns finish afterwards, each bumping the total
        // count of on-disk directories past a very small max_keep.
        for n in 0..5 {
            let file = ws.path.join(format!("f{n}.txt"));
            std::fs::write(&file, "x").unwrap();
            let id = begin_impl(
                &state,
                &base.path,
                "s".to_string(),
                0,
                "p".to_string(),
                Some(1),
            )
            .unwrap();
            record_original(&state, Some(&id), &file).unwrap();
            end_impl(&state, &id).unwrap();
        }

        // Turn A's directory must have survived every intervening prune_old
        // call, since it never got a manifest and is still open.
        assert!(
            base.path.join(&id_a).is_dir(),
            "in-flight checkpoint dir must never be pruned"
        );
        assert!(state.checkpoints.lock().unwrap().contains_key(&id_a));

        // And a mutation recorded against it afterwards must still succeed.
        let file_a = ws.path.join("a.txt");
        std::fs::write(&file_a, "original").unwrap();
        record_original(&state, Some(&id_a), &file_a).unwrap();
    }

    #[test]
    fn prune_sweeps_an_abandoned_in_flight_checkpoint_once_old_enough() {
        let base = TempDir::new("base");

        // A manifest-less directory whose mtime is far older than the
        // abandoned-in-flight cutoff — simulating a turn that crashed before
        // ever calling checkpoint_end.
        let stale_id = "00000000-0000-4000-8000-0000000stale1";
        let stale_dir = base.path.join(stale_id);
        std::fs::create_dir_all(&stale_dir).unwrap();
        let ancient = std::time::SystemTime::now()
            - std::time::Duration::from_millis(ABANDONED_IN_FLIGHT_MAX_AGE_MS + 60_000);
        set_dir_mtime(&stale_dir, ancient);

        prune_old(&base.path, 20);

        assert!(
            !stale_dir.exists(),
            "an old-enough manifest-less directory must be swept as abandoned"
        );
    }

    /// Backdates `dir`'s mtime for the abandoned-in-flight sweep test above.
    fn set_dir_mtime(dir: &Path, t: std::time::SystemTime) {
        let file = std::fs::File::open(dir).expect("open dir for mtime update");
        let times = std::fs::FileTimes::new().set_modified(t);
        file.set_times(times).expect("set directory mtime");
    }

    #[test]
    fn prune_ranks_by_manifest_created_at_ms_not_filesystem_mtime() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        // A (oldest), B, C (newest) by created_at_ms.
        let make = |n: u64| {
            let file = ws.path.join(format!("f{n}.txt"));
            std::fs::write(&file, "x").unwrap();
            let id = begin_impl(
                &state,
                &base.path,
                "s".to_string(),
                0,
                "p".to_string(),
                None,
            )
            .unwrap();
            record_original(&state, Some(&id), &file).unwrap();
            end_impl(&state, &id).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(15));
            id
        };
        let id_a = make(0);
        let id_b = make(1);
        let id_c = make(2);

        // Reverting A rewrites its manifest.json, which would bump its
        // directory's mtime — but must NOT make it look newer than B.
        revert_impl(&base.path, &id_a).unwrap();

        // With max_keep=2, the correct "keep newest 2 by created_at_ms" is
        // {B, C}; a mtime-based ranking would instead keep {A, C} since A
        // was just touched by revert.
        let _ = begin_impl(
            &state,
            &base.path,
            "s".to_string(),
            0,
            "p".to_string(),
            Some(2),
        )
        .unwrap();

        assert!(
            !base.path.join(&id_a).exists(),
            "reverted-but-genuinely-oldest checkpoint must still be pruned"
        );
        assert!(
            base.path.join(&id_b).exists(),
            "genuinely newer checkpoint must survive pruning"
        );
        assert!(base.path.join(&id_c).exists());
    }

    #[test]
    fn revert_then_reapply_roundtrips_a_turn_created_file() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let created = ws.path.join("created.txt");

        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &created).unwrap();
        std::fs::write(&created, "brand new").unwrap();
        end_impl(&state, &id).unwrap();

        revert_impl(&base.path, &id).unwrap();
        assert!(
            !created.exists(),
            "revert must delete the turn-created file"
        );
        assert!(
            read_manifest(&base.path, &id).unwrap().reverted,
            "revert must persist reverted: true"
        );

        let reapplied = reapply_impl(&base.path, &id).unwrap();
        assert_eq!(reapplied, 1);
        assert_eq!(
            std::fs::read_to_string(&created).unwrap(),
            "brand new",
            "reapply must recreate the turn-created file with its post-turn content"
        );
        assert!(
            !read_manifest(&base.path, &id).unwrap().reverted,
            "reapply must persist reverted: false"
        );
    }

    #[test]
    fn revert_then_reapply_roundtrips_a_turn_modified_file() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let existing = ws.path.join("existing.txt");
        std::fs::write(&existing, "original").unwrap();

        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &existing).unwrap();
        std::fs::write(&existing, "mutated").unwrap();
        end_impl(&state, &id).unwrap();

        revert_impl(&base.path, &id).unwrap();
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "original");

        let reapplied = reapply_impl(&base.path, &id).unwrap();
        assert_eq!(reapplied, 1);
        assert_eq!(
            std::fs::read_to_string(&existing).unwrap(),
            "mutated",
            "reapply must restore the turn's mutated content"
        );
    }

    #[test]
    fn revert_is_idempotent_and_does_not_corrupt_the_redo_backup() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file = ws.path.join("f.txt");
        std::fs::write(&file, "v1").unwrap();

        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &file).unwrap();
        std::fs::write(&file, "v2").unwrap();
        end_impl(&state, &id).unwrap();

        // First revert: file goes back to "v1", redo/0.bak correctly holds "v2".
        let first = revert_impl(&base.path, &id).unwrap();
        assert_eq!(first, 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1");

        // A second revert of the SAME (already-reverted) checkpoint — e.g.
        // CheckpointTimeline's "Restore to here" re-reverting a checkpoint
        // the user already reverted individually, or `/revert` run twice —
        // must be a no-op, not re-snapshot the current ("v1") content over
        // the true post-turn ("v2") redo backup.
        let second = revert_impl(&base.path, &id).unwrap();
        assert_eq!(
            second, 0,
            "a repeat revert of an already-reverted checkpoint must be a no-op"
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "v1",
            "the file itself must be unaffected"
        );

        let reapplied = reapply_impl(&base.path, &id).unwrap();
        assert_eq!(reapplied, 1);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "v2",
            "reapply after a redundant revert must still restore the turn's true post-turn content"
        );
    }

    #[test]
    fn reapply_is_idempotent() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file = ws.path.join("f.txt");
        std::fs::write(&file, "v1").unwrap();

        let id = begin(&state, &base.path);
        record_original(&state, Some(&id), &file).unwrap();
        std::fs::write(&file, "v2").unwrap();
        end_impl(&state, &id).unwrap();

        // Reapply before any revert has ever happened: nothing to redo yet.
        let noop = reapply_impl(&base.path, &id).unwrap();
        assert_eq!(
            noop, 0,
            "reapply on a never-reverted checkpoint must be a no-op"
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "v2",
            "must not touch the file"
        );

        revert_impl(&base.path, &id).unwrap();
        reapply_impl(&base.path, &id).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v2");

        // A second reapply after the checkpoint is already re-applied
        // (reverted: false) must likewise be a no-op.
        let second = reapply_impl(&base.path, &id).unwrap();
        assert_eq!(second, 0, "a repeat reapply must be a no-op");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v2");
    }

    #[test]
    fn begin_records_prev_id_as_the_sessions_current_head() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let make = |n: u64| {
            let file = ws.path.join(format!("f{n}.txt"));
            std::fs::write(&file, "x").unwrap();
            let id = begin_impl(
                &state,
                &base.path,
                "s1".to_string(),
                0,
                "p".to_string(),
                None,
            )
            .unwrap();
            record_original(&state, Some(&id), &file).unwrap();
            end_impl(&state, &id).unwrap();
            id
        };
        let id_a = make(0);
        let id_b = make(1);

        let manifest_a = read_manifest(&base.path, &id_a).unwrap();
        assert_eq!(
            manifest_a.prev_id, None,
            "the session's first checkpoint has no predecessor"
        );

        let manifest_b = read_manifest(&base.path, &id_b).unwrap();
        assert_eq!(
            manifest_b.prev_id,
            Some(id_a.clone()),
            "must link to the session's previous checkpoint"
        );

        // A checkpoint in a different session must not be treated as a
        // predecessor.
        let file_c = ws.path.join("c.txt");
        std::fs::write(&file_c, "x").unwrap();
        let id_other_session = begin_impl(
            &state,
            &base.path,
            "s2".to_string(),
            0,
            "p".to_string(),
            None,
        )
        .unwrap();
        record_original(&state, Some(&id_other_session), &file_c).unwrap();
        end_impl(&state, &id_other_session).unwrap();
        assert_eq!(
            read_manifest(&base.path, &id_other_session)
                .unwrap()
                .prev_id,
            None
        );
    }

    #[test]
    fn list_exposes_prev_id_so_the_timeline_can_detect_a_pruned_gap() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let make = |n: u64| {
            let file = ws.path.join(format!("f{n}.txt"));
            std::fs::write(&file, "x").unwrap();
            let id = begin_impl(
                &state,
                &base.path,
                "s1".to_string(),
                0,
                "p".to_string(),
                None,
            )
            .unwrap();
            record_original(&state, Some(&id), &file).unwrap();
            end_impl(&state, &id).unwrap();
            id
        };
        let id_a = make(0);
        let id_b = make(1);
        let id_c = make(2);

        // Simulate B being pruned (or otherwise removed) independently of A/C.
        std::fs::remove_dir_all(base.path.join(&id_b)).unwrap();

        let infos = list_impl(&base.path, Some("s1")).unwrap();
        assert_eq!(infos.len(), 2, "B must no longer be listed");
        // Newest-first: C, then A.
        assert_eq!(infos[0].id, id_c);
        assert_eq!(infos[1].id, id_a);
        // C's recorded predecessor (B) doesn't match the next surviving
        // entry (A) — that mismatch is exactly the pruned-gap signal the
        // timeline's "Restore to here" must key off of.
        assert_eq!(
            infos[0].prev_id,
            Some(id_b),
            "C's prev_id still points at the pruned B"
        );
        assert_ne!(
            infos[0].prev_id,
            Some(infos[1].id.clone()),
            "mismatch signals a gap"
        );
    }

    #[test]
    fn acquire_revert_lock_rejects_a_second_concurrent_claim() {
        let state = AppState::default();
        let id = "some-checkpoint-id";

        let guard = acquire_revert_lock(&state, id).unwrap();
        let err = match acquire_revert_lock(&state, id) {
            Ok(_) => panic!("a second concurrent claim on the same id must be rejected"),
            Err(e) => e,
        };
        assert!(
            err.contains("already being restored"),
            "unexpected error: {err}"
        );

        drop(guard);
        // Once released, a new claim must succeed.
        assert!(acquire_revert_lock(&state, id).is_ok());
    }

    #[test]
    fn reapply_rejects_traversal_ids() {
        let base = TempDir::new("base");
        let err = reapply_impl(&base.path, "../outside").unwrap_err();
        assert!(
            err.contains("Invalid checkpoint id"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn reapply_errors_for_unknown_id() {
        let base = TempDir::new("base");
        let err = reapply_impl(&base.path, "0000-does-not-exist").unwrap_err();
        assert!(err.contains("not found"), "unexpected error: {err}");
    }

    #[test]
    fn list_returns_newest_first_and_skips_in_flight_checkpoints() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file_a = ws.path.join("a.txt");
        std::fs::write(&file_a, "a").unwrap();
        let id_older = begin_impl(
            &state,
            &base.path,
            "s1".to_string(),
            0,
            "older turn".to_string(),
            None,
        )
        .unwrap();
        record_original(&state, Some(&id_older), &file_a).unwrap();
        end_impl(&state, &id_older).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(5));

        let file_b = ws.path.join("b.txt");
        std::fs::write(&file_b, "b").unwrap();
        let id_newer = begin_impl(
            &state,
            &base.path,
            "s1".to_string(),
            1,
            "newer turn".to_string(),
            None,
        )
        .unwrap();
        record_original(&state, Some(&id_newer), &file_b).unwrap();
        end_impl(&state, &id_newer).unwrap();

        // A checkpoint whose turn is still running (no `checkpoint_end` yet,
        // so no manifest.json on disk) must not appear in the list.
        let _in_flight = begin_impl(
            &state,
            &base.path,
            "s1".to_string(),
            2,
            "still running".to_string(),
            None,
        )
        .unwrap();

        let infos = list_impl(&base.path, None).unwrap();
        assert_eq!(
            infos.len(),
            2,
            "in-flight checkpoint must be skipped: {:?}",
            infos.iter().map(|i| &i.id).collect::<Vec<_>>()
        );
        assert_eq!(infos[0].id, id_newer, "newest checkpoint must sort first");
        assert_eq!(infos[1].id, id_older);
        assert_eq!(infos[0].files, 1);
        assert_eq!(infos[0].label, "newer turn");
        assert!(!infos[0].reverted);
        assert!(!infos[0].reapplyable);
    }

    #[test]
    fn list_filters_by_session_id() {
        let state = AppState::default();
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let file_a = ws.path.join("a.txt");
        std::fs::write(&file_a, "a").unwrap();
        let id_s1 = begin_impl(
            &state,
            &base.path,
            "session-1".to_string(),
            0,
            "s1 turn".to_string(),
            None,
        )
        .unwrap();
        record_original(&state, Some(&id_s1), &file_a).unwrap();
        end_impl(&state, &id_s1).unwrap();

        let file_b = ws.path.join("b.txt");
        std::fs::write(&file_b, "b").unwrap();
        let id_s2 = begin_impl(
            &state,
            &base.path,
            "session-2".to_string(),
            0,
            "s2 turn".to_string(),
            None,
        )
        .unwrap();
        record_original(&state, Some(&id_s2), &file_b).unwrap();
        end_impl(&state, &id_s2).unwrap();

        let all = list_impl(&base.path, None).unwrap();
        assert_eq!(all.len(), 2);

        let filtered = list_impl(&base.path, Some("session-1")).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, id_s1);
        assert_eq!(filtered[0].session_id, "session-1");
    }

    #[test]
    fn list_includes_v1_manifests_and_marks_reapplyable_after_revert() {
        let base = TempDir::new("base");
        let ws = TempDir::new("ws");

        let existing = ws.path.join("existing.txt");
        std::fs::write(&existing, "mutated").unwrap();

        // A v1 (bare-array) manifest on disk, same as `v1_manifest_on_disk_still_reverts`.
        let v1_id = "00000000-0000-4000-8000-00000000v1lst";
        let v1_dir = base.path.join(v1_id);
        std::fs::create_dir_all(&v1_dir).unwrap();
        std::fs::write(v1_dir.join("0.bak"), "original").unwrap();
        let raw = format!(
            r#"[{{"path":{:?},"backup":"0.bak"}}]"#,
            existing.to_string_lossy()
        );
        std::fs::write(v1_dir.join(MANIFEST_FILE), raw).unwrap();

        let before_revert = list_impl(&base.path, None).unwrap();
        assert_eq!(before_revert.len(), 1);
        assert_eq!(
            before_revert[0].session_id, "",
            "v1 manifests synthesize an empty session id"
        );
        assert!(!before_revert[0].reverted);
        assert!(
            !before_revert[0].reapplyable,
            "not reverted yet, so nothing to re-apply"
        );

        revert_impl(&base.path, v1_id).unwrap();

        let after_revert = list_impl(&base.path, None).unwrap();
        assert_eq!(after_revert.len(), 1);
        assert!(after_revert[0].reverted);
        assert!(
            after_revert[0].reapplyable,
            "revert_impl recorded a redo backup, so re-apply is now meaningful"
        );
    }

    /// Reproduces (and pins the fix for) the concurrent-write race a code
    /// review flagged against `tool_write_file`/`tool_edit_file`: two
    /// `code`-profile subagents (or any two concurrent mutating tool calls)
    /// resolving to the SAME workspace path can, without serialization,
    /// interleave past `record_original`'s dedup and each other's
    /// `std::fs::write`, silently discarding one write. This mirrors the
    /// review's own throwaway repro exactly — `record_original` followed by
    /// a forced yield (standing in for `request_permission`'s real `.await`)
    /// followed by `std::fs::write` — but now wrapped in `AppState::
    /// file_write_lock` (see `tool_write_file`'s doc comment), the same lock
    /// `tools.rs` now acquires around its own backup+write critical section.
    /// Run across many trials on a genuinely multi-threaded runtime (real OS
    /// parallelism, not just cooperative interleaving) so a regression that
    /// reintroduces the race would show up as either a corrupted/mixed file
    /// or more than one recorded checkpoint entry for the same path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writes_to_the_same_path_are_serialized_by_file_write_lock() {
        use std::sync::Arc;

        for _ in 0..50 {
            let base = TempDir::new("base");
            let ws = TempDir::new("ws");
            let file = ws.path.join("shared.txt");
            std::fs::write(&file, "original").unwrap();

            let state = Arc::new(AppState::default());
            let checkpoint_id = begin(&state, &base.path);

            let run = |state: Arc<AppState>,
                       checkpoint_id: String,
                       file: PathBuf,
                       content: &'static str| {
                tokio::spawn(async move {
                    // Force genuine interleaving opportunity across the two
                    // tasks before either takes the lock — standing in for
                    // `request_permission`'s real `.await` between path
                    // resolution and the backup+write critical section.
                    tokio::task::yield_now().await;

                    let _guard = state.file_write_lock.lock().unwrap();
                    record_original(&state, Some(&checkpoint_id), &file).unwrap();
                    std::fs::write(&file, content).unwrap();
                })
            };

            let a = run(
                state.clone(),
                checkpoint_id.clone(),
                file.clone(),
                "from writer A",
            );
            let b = run(
                state.clone(),
                checkpoint_id.clone(),
                file.clone(),
                "from writer B",
            );
            a.await.unwrap();
            b.await.unwrap();

            let final_content = std::fs::read_to_string(&file).unwrap();
            assert!(
                final_content == "from writer A" || final_content == "from writer B",
                "file content was corrupted/interleaved rather than a clean win by one writer: {final_content:?}"
            );

            // Only the FIRST writer's pre-mutation backup should ever be
            // recorded for this path — serialization means the second
            // writer's `record_original` call always sees an existing entry
            // and skips (this dedup was always mutex-protected; the fix is
            // that the two writers' backup+write pairs can no longer
            // interleave with each other).
            let entries = state.checkpoints.lock().unwrap()[&checkpoint_id]
                .entries
                .clone();
            let file_str = file.to_string_lossy().to_string();
            assert_eq!(entries.iter().filter(|e| e.path == file_str).count(), 1);
        }
    }
}
