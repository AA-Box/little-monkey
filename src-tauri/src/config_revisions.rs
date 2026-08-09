//! Local revision history for user-authored configuration and definitions
//! (roadmap K24 / ROADMAP #3): personas, snippets, skills, and workflow
//! definitions.
//!
//! Everything the user authors in this app was last-write-wins: the newest
//! save replaced the previous bytes and nothing remembered them. This module
//! is the versioned system-configuration store that fixes it — an append-only
//! log per entity, with named branches for compare, and an
//! optimistic-concurrency check so a second window's save is *refused and
//! surfaced* instead of silently clobbering the first.
//!
//! Restore and diff are deliberately NOT operations here. Restoring is "read
//! that revision's snapshot, put it back through the owning store's normal
//! save path" — which records the restore as an ordinary revision, and can
//! never leave the history claiming a restore that the owning store rejected.
//! Diffing is `DiffViewer.tsx`, the app's one line differ.
//!
//! Storage is one JSONL file per entity, under
//! `<app_data>/config-revisions/<kind>/<entity>.jsonl`. A config document is
//! small (a persona is a paragraph; a workflow is a few KB of JSON), so each
//! revision stores the full snapshot rather than a delta chain — restoring is
//! then a read, not a replay, and a corrupt middle line costs one revision
//! instead of every revision after it.
//!
//! Deliberately Tauri-free below the `#[tauri::command]` layer: `WorkflowService`
//! (`m4_services.rs`) records into the same store from the CLI and daemon paths
//! where no `AppHandle` exists, exactly like `prompts.rs`/`checkpoints.rs`
//! already split their cores.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The branch every entity starts on. Named rather than empty so a branch
/// column is always meaningful in the UI and in the persisted record.
pub const DEFAULT_BRANCH: &str = "main";

/// Largest snapshot accepted into a revision. A persona or workflow far below
/// this is the norm; the cap stops a pathological paste from turning the
/// history file into something that can't be read back cheaply.
const MAX_CONTENT_BYTES: usize = 1_048_576;

/// How many revisions an entity keeps before the oldest are pruned. Every
/// branch head survives pruning regardless of age — losing a branch's tip
/// would break `restore`/`compare` for that branch, which is the whole point
/// of keeping the history.
const MAX_REVISIONS_PER_ENTITY: usize = 200;

/// `^[a-z0-9._-]{1,48}$` — branch names go into no path and no shell, but a
/// tight shape keeps them displayable and comparable without escaping.
const MAX_BRANCH_LEN: usize = 48;

#[derive(Debug, Clone, PartialEq)]
pub enum RevisionError {
    Io(String),
    /// The caller's `base_revision_id` is not the branch head — someone else
    /// (another window, the CLI, a daemon trigger) saved in between.
    Conflict {
        head_revision_id: Option<String>,
        head_label: Option<String>,
    },
    NotFound(String),
    Invalid(String),
}

impl std::fmt::Display for RevisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RevisionError::Io(message) => write!(f, "{message}"),
            // The `conflict:` prefix is load-bearing: it is how the frontend
            // (`configRevisionStore.ts`) tells "someone else edited this"
            // apart from every other save failure, since a `#[tauri::command]`
            // can only return a string.
            RevisionError::Conflict {
                head_revision_id, ..
            } => write!(
                f,
                "conflict: this was changed elsewhere since you loaded it (current revision {})",
                head_revision_id.as_deref().unwrap_or("none")
            ),
            RevisionError::NotFound(what) => write!(f, "not found: {what}"),
            RevisionError::Invalid(why) => write!(f, "invalid: {why}"),
        }
    }
}

impl From<std::io::Error> for RevisionError {
    fn from(error: std::io::Error) -> Self {
        RevisionError::Io(error.to_string())
    }
}

/// One stored revision: a full content snapshot plus its place in the log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Revision {
    pub revision_id: String,
    /// The revision this one was written on top of — the branch head at write
    /// time, or the branch point for the first revision of a new branch.
    #[serde(default)]
    pub parent_id: Option<String>,
    pub branch: String,
    /// Monotonic per entity (not per branch), so a revision has a short stable
    /// human label (`r7`) that is unique across branches.
    pub sequence: u64,
    pub created_at: u64,
    /// Why this revision exists, in the user's terms ("Edited", "Restored r3").
    pub label: String,
    pub content_sha256: String,
    /// The entity this revision belongs to, kept on every record so the log
    /// file is self-describing after the path has been slugified.
    pub entity_id: String,
    pub content: String,
}

/// A revision without its snapshot — what the history list renders. Sending
/// the content of 200 revisions to draw a list would be the expensive half of
/// this feature for no reason; the content is fetched per selected revision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RevisionMeta {
    pub revision_id: String,
    pub parent_id: Option<String>,
    pub branch: String,
    pub sequence: u64,
    pub created_at: u64,
    pub label: String,
    pub content_sha256: String,
    pub bytes: usize,
}

impl From<&Revision> for RevisionMeta {
    fn from(revision: &Revision) -> Self {
        RevisionMeta {
            revision_id: revision.revision_id.clone(),
            parent_id: revision.parent_id.clone(),
            branch: revision.branch.clone(),
            sequence: revision.sequence,
            created_at: revision.created_at,
            label: revision.label.clone(),
            content_sha256: revision.content_sha256.clone(),
            bytes: revision.content.len(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummary {
    pub name: String,
    pub head_revision_id: String,
    pub revision_count: usize,
    pub updated_at: u64,
}

/// What a caller supplies to [`record`].
#[derive(Debug, Clone, Default)]
pub struct RecordRequest {
    /// `None` means [`DEFAULT_BRANCH`].
    pub branch: Option<String>,
    /// The revision the caller believes is current. `Some` opts into the
    /// concurrent-edit check; `None` is an unconditional append (used by
    /// imports, restores, and the first save of a brand-new entity).
    pub base_revision_id: Option<String>,
    pub label: String,
    pub content: String,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Maps an arbitrary caller-supplied id onto a safe single path segment.
///
/// The readable prefix is for a human looking at the directory; the hash
/// suffix is what actually guarantees two distinct ids never land in the same
/// file (`a/b` and `a_b` both slugify to `a_b`). Traversal is impossible by
/// construction: every byte outside `[A-Za-z0-9._-]` is replaced, and a
/// leading `.` is replaced too, so `..` can never be produced.
fn slug(raw: &str) -> String {
    let mut readable: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    if readable.is_empty() {
        readable.push('_');
    }
    format!("{readable}-{}", &sha256_hex(raw.as_bytes())[..12])
}

fn entity_path(root: &Path, kind: &str, entity_id: &str) -> PathBuf {
    root.join(slug(kind))
        .join(format!("{}.jsonl", slug(entity_id)))
}

fn validate_ids(kind: &str, entity_id: &str) -> Result<(), RevisionError> {
    if kind.trim().is_empty() || entity_id.trim().is_empty() {
        return Err(RevisionError::Invalid(
            "revision kind and entity id cannot be empty".to_string(),
        ));
    }
    if kind.len() > 64 || entity_id.len() > 256 {
        return Err(RevisionError::Invalid(
            "revision kind or entity id is too long".to_string(),
        ));
    }
    Ok(())
}

fn validate_branch(branch: &str) -> Result<(), RevisionError> {
    if branch.is_empty() || branch.len() > MAX_BRANCH_LEN {
        return Err(RevisionError::Invalid(format!(
            "branch name must be 1-{MAX_BRANCH_LEN} characters"
        )));
    }
    if !branch
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-' || c == '_')
    {
        return Err(RevisionError::Invalid(
            "branch name may use lowercase letters, digits, '.', '-' and '_' only".to_string(),
        ));
    }
    Ok(())
}

/// Reads the whole log for one entity, in write order.
///
/// A line that no longer parses is skipped rather than failing the read: a
/// history is a convenience store, and one corrupt record must not make the
/// other 199 revisions unreachable (the same leniency stance `prompts.rs`
/// takes toward a hand-edited entry).
fn read_log(root: &Path, kind: &str, entity_id: &str) -> Result<Vec<Revision>, RevisionError> {
    validate_ids(kind, entity_id)?;
    let path = entity_path(root, kind, entity_id);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(RevisionError::Io(format!("failed to read revisions: {e}"))),
    };
    Ok(raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Revision>(line).ok())
        .collect())
}

fn write_log(
    root: &Path,
    kind: &str,
    entity_id: &str,
    revisions: &[Revision],
) -> Result<(), RevisionError> {
    let path = entity_path(root, kind, entity_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| RevisionError::Io(format!("failed to create revision dir: {e}")))?;
    }
    let mut body = String::new();
    for revision in revisions {
        let line = serde_json::to_string(revision)
            .map_err(|e| RevisionError::Io(format!("failed to encode revision: {e}")))?;
        body.push_str(&line);
        body.push('\n');
    }
    // Temp file + rename, same reasoning as `prompts::save_impl`: a crash
    // mid-rewrite must not leave a truncated history behind.
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, body)
        .map_err(|e| RevisionError::Io(format!("failed to write revisions: {e}")))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| RevisionError::Io(format!("failed to finalize revisions: {e}")))?;
    Ok(())
}

fn append_line(
    root: &Path,
    kind: &str,
    entity_id: &str,
    revision: &Revision,
) -> Result<(), RevisionError> {
    let path = entity_path(root, kind, entity_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| RevisionError::Io(format!("failed to create revision dir: {e}")))?;
    }
    let line = serde_json::to_string(revision)
        .map_err(|e| RevisionError::Io(format!("failed to encode revision: {e}")))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| RevisionError::Io(format!("failed to open revisions: {e}")))?;
    file.write_all(line.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .map_err(|e| RevisionError::Io(format!("failed to append revision: {e}")))?;
    Ok(())
}

/// The newest revision on `branch`, or `None` if the branch has none.
fn head_of<'a>(revisions: &'a [Revision], branch: &str) -> Option<&'a Revision> {
    revisions.iter().rev().find(|r| r.branch == branch)
}

/// Drops the oldest revisions once the log exceeds [`MAX_REVISIONS_PER_ENTITY`],
/// always keeping every branch head so no branch loses its tip to age.
fn prune(revisions: Vec<Revision>) -> Vec<Revision> {
    if revisions.len() <= MAX_REVISIONS_PER_ENTITY {
        return revisions;
    }
    let mut heads: Vec<String> = Vec::new();
    let mut seen_branches: Vec<String> = Vec::new();
    for revision in revisions.iter().rev() {
        if !seen_branches.contains(&revision.branch) {
            seen_branches.push(revision.branch.clone());
            heads.push(revision.revision_id.clone());
        }
    }
    let drop_count = revisions.len() - MAX_REVISIONS_PER_ENTITY;
    let mut dropped = 0usize;
    revisions
        .into_iter()
        .filter(|revision| {
            if dropped < drop_count && !heads.contains(&revision.revision_id) {
                dropped += 1;
                return false;
            }
            true
        })
        .collect()
}

/// Appends a revision, refusing the write when `base_revision_id` is stale.
///
/// Returns the existing head unchanged when the content is byte-identical to
/// it: an editor that saves on a debounce, or a store that re-persists the
/// whole library because one unrelated entry changed, must not fill the
/// history with duplicates of the same text.
pub fn record(
    root: &Path,
    kind: &str,
    entity_id: &str,
    request: RecordRequest,
) -> Result<Revision, RevisionError> {
    validate_ids(kind, entity_id)?;
    if request.content.len() > MAX_CONTENT_BYTES {
        return Err(RevisionError::Invalid(format!(
            "content is larger than the {MAX_CONTENT_BYTES}-byte revision limit"
        )));
    }
    let branch = request.branch.unwrap_or_else(|| DEFAULT_BRANCH.to_string());
    validate_branch(&branch)?;

    let revisions = read_log(root, kind, entity_id)?;
    let head = head_of(&revisions, &branch);

    if let Some(base) = request.base_revision_id.as_ref() {
        let head_id = head.map(|r| r.revision_id.as_str());
        if head_id != Some(base.as_str()) {
            return Err(RevisionError::Conflict {
                head_revision_id: head_id.map(str::to_string),
                head_label: head.map(|r| r.label.clone()),
            });
        }
    }

    let content_sha256 = sha256_hex(request.content.as_bytes());
    if let Some(head) = head {
        if head.content_sha256 == content_sha256 {
            return Ok(head.clone());
        }
    }

    let revision = Revision {
        revision_id: uuid::Uuid::new_v4().to_string(),
        parent_id: head.map(|r| r.revision_id.clone()),
        branch,
        sequence: revisions.iter().map(|r| r.sequence).max().unwrap_or(0) + 1,
        created_at: now_ms(),
        label: if request.label.trim().is_empty() {
            "Edited".to_string()
        } else {
            request.label.chars().take(120).collect()
        },
        content_sha256,
        entity_id: entity_id.to_string(),
        content: request.content,
    };

    if revisions.len() + 1 > MAX_REVISIONS_PER_ENTITY {
        let mut next = revisions;
        next.push(revision.clone());
        write_log(root, kind, entity_id, &prune(next))?;
    } else {
        append_line(root, kind, entity_id, &revision)?;
    }
    Ok(revision)
}

/// Every revision of an entity, newest first, optionally filtered to a branch.
pub fn history(
    root: &Path,
    kind: &str,
    entity_id: &str,
    branch: Option<&str>,
) -> Result<Vec<RevisionMeta>, RevisionError> {
    let mut revisions = read_log(root, kind, entity_id)?;
    if let Some(branch) = branch {
        revisions.retain(|r| r.branch == branch);
    }
    Ok(revisions.iter().rev().map(RevisionMeta::from).collect())
}

pub fn get(
    root: &Path,
    kind: &str,
    entity_id: &str,
    revision_id: &str,
) -> Result<Revision, RevisionError> {
    read_log(root, kind, entity_id)?
        .into_iter()
        .find(|r| r.revision_id == revision_id)
        .ok_or_else(|| RevisionError::NotFound(format!("revision {revision_id}")))
}

/// The current head of `branch`, or `None` when nothing has been recorded.
pub fn head(
    root: &Path,
    kind: &str,
    entity_id: &str,
    branch: Option<&str>,
) -> Result<Option<Revision>, RevisionError> {
    let branch = branch.unwrap_or(DEFAULT_BRANCH);
    Ok(head_of(&read_log(root, kind, entity_id)?, branch).cloned())
}

/// Starts a named branch from an existing revision, so two variants of the
/// same persona or workflow can be kept and compared side by side.
pub fn branch_from(
    root: &Path,
    kind: &str,
    entity_id: &str,
    from_revision_id: &str,
    new_branch: &str,
) -> Result<Revision, RevisionError> {
    validate_branch(new_branch)?;
    let source = get(root, kind, entity_id, from_revision_id)?;
    if head(root, kind, entity_id, Some(new_branch))?.is_some() {
        return Err(RevisionError::Invalid(format!(
            "branch '{new_branch}' already exists"
        )));
    }
    let revision = Revision {
        revision_id: uuid::Uuid::new_v4().to_string(),
        parent_id: Some(source.revision_id.clone()),
        branch: new_branch.to_string(),
        sequence: read_log(root, kind, entity_id)?
            .iter()
            .map(|r| r.sequence)
            .max()
            .unwrap_or(0)
            + 1,
        created_at: now_ms(),
        label: format!("Branched from r{}", source.sequence),
        content_sha256: source.content_sha256.clone(),
        entity_id: entity_id.to_string(),
        content: source.content,
    };
    append_line(root, kind, entity_id, &revision)?;
    Ok(revision)
}

pub fn branches(
    root: &Path,
    kind: &str,
    entity_id: &str,
) -> Result<Vec<BranchSummary>, RevisionError> {
    let revisions = read_log(root, kind, entity_id)?;
    let mut summaries: Vec<BranchSummary> = Vec::new();
    for revision in &revisions {
        match summaries.iter_mut().find(|s| s.name == revision.branch) {
            Some(summary) => {
                summary.revision_count += 1;
                summary.head_revision_id = revision.revision_id.clone();
                summary.updated_at = summary.updated_at.max(revision.created_at);
            }
            None => summaries.push(BranchSummary {
                name: revision.branch.clone(),
                head_revision_id: revision.revision_id.clone(),
                revision_count: 1,
                updated_at: revision.created_at,
            }),
        }
    }
    // `main` first, then the rest alphabetically — a stable order the UI can
    // render without sorting again.
    summaries.sort_by(
        |a, b| match (a.name == DEFAULT_BRANCH, b.name == DEFAULT_BRANCH) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        },
    );
    Ok(summaries)
}

/// Every entity of `kind` that has a history, newest activity first. Backs the
/// "versioned configuration" overview, which otherwise could not enumerate
/// entities whose owning store has already forgotten them.
pub fn entities(root: &Path, kind: &str) -> Result<Vec<String>, RevisionError> {
    let dir = root.join(slug(kind));
    let mut found: Vec<(u64, String)> = Vec::new();
    let read = match std::fs::read_dir(&dir) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(RevisionError::Io(format!("failed to list revisions: {e}"))),
    };
    for entry in read.flatten() {
        if entry.path().extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let mut entity_id: Option<String> = None;
        let mut updated_at = 0u64;
        for line in raw.lines().filter(|l| !l.trim().is_empty()) {
            if let Ok(revision) = serde_json::from_str::<Revision>(line) {
                entity_id = Some(revision.entity_id);
                updated_at = updated_at.max(revision.created_at);
            }
        }
        if let Some(entity_id) = entity_id {
            found.push((updated_at, entity_id));
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    Ok(found.into_iter().map(|(_, id)| id).collect())
}

// ---------------------------------------------------------------------------
// Tauri command layer
// ---------------------------------------------------------------------------

/// `<app_data>/config-revisions` — the one store every versioned config kind
/// shares, so the desktop, the CLI, and daemon-hosted workflow writes all land
/// in the same history rather than three private ones.
pub fn revision_root(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("config-revisions")
}

fn root_for(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {e}"))?;
    Ok(revision_root(&dir))
}

#[tauri::command]
pub fn config_revisions_record(
    app: tauri::AppHandle,
    kind: String,
    entity_id: String,
    label: String,
    content: String,
    branch: Option<String>,
    base_revision_id: Option<String>,
) -> Result<Revision, String> {
    record(
        &root_for(&app)?,
        &kind,
        &entity_id,
        RecordRequest {
            branch,
            base_revision_id,
            label,
            content,
        },
    )
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn config_revisions_history(
    app: tauri::AppHandle,
    kind: String,
    entity_id: String,
    branch: Option<String>,
) -> Result<Vec<RevisionMeta>, String> {
    history(&root_for(&app)?, &kind, &entity_id, branch.as_deref()).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn config_revisions_get(
    app: tauri::AppHandle,
    kind: String,
    entity_id: String,
    revision_id: String,
) -> Result<Revision, String> {
    get(&root_for(&app)?, &kind, &entity_id, &revision_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn config_revisions_head(
    app: tauri::AppHandle,
    kind: String,
    entity_id: String,
    branch: Option<String>,
) -> Result<Option<Revision>, String> {
    head(&root_for(&app)?, &kind, &entity_id, branch.as_deref()).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn config_revisions_branch(
    app: tauri::AppHandle,
    kind: String,
    entity_id: String,
    from_revision_id: String,
    new_branch: String,
) -> Result<Revision, String> {
    branch_from(
        &root_for(&app)?,
        &kind,
        &entity_id,
        &from_revision_id,
        &new_branch,
    )
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn config_revisions_branches(
    app: tauri::AppHandle,
    kind: String,
    entity_id: String,
) -> Result<Vec<BranchSummary>, String> {
    branches(&root_for(&app)?, &kind, &entity_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn config_revisions_entities(
    app: tauri::AppHandle,
    kind: String,
) -> Result<Vec<String>, String> {
    entities(&root_for(&app)?, &kind).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "little_monkey_revisions_test_{}_{}_{}",
            std::process::id(),
            n,
            nanos
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn write(root: &Path, content: &str) -> Revision {
        record(
            root,
            "prompt",
            "persona-1",
            RecordRequest {
                label: "Edited".to_string(),
                content: content.to_string(),
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn history_is_empty_before_anything_is_recorded() {
        let root = temp_root();
        assert!(history(&root, "prompt", "persona-1", None)
            .unwrap()
            .is_empty());
        assert!(head(&root, "prompt", "persona-1", None).unwrap().is_none());
    }

    #[test]
    fn each_save_appends_a_revision_newest_first() {
        let root = temp_root();
        write(&root, "one");
        write(&root, "two");
        let log = history(&root, "prompt", "persona-1", None).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].sequence, 2);
        assert_eq!(log[1].sequence, 1);
        assert_eq!(
            log[0].parent_id.as_deref(),
            Some(log[1].revision_id.as_str())
        );
    }

    #[test]
    fn an_unchanged_save_does_not_create_a_duplicate_revision() {
        let root = temp_root();
        let first = write(&root, "same");
        let second = write(&root, "same");
        assert_eq!(first.revision_id, second.revision_id);
        assert_eq!(
            history(&root, "prompt", "persona-1", None).unwrap().len(),
            1
        );
    }

    /// The whole point of K24's "concurrent edit is detected and surfaced":
    /// a second writer holding a stale base is refused, not merged silently.
    #[test]
    fn a_stale_base_revision_is_refused_as_a_conflict() {
        let root = temp_root();
        let first = write(&root, "one");
        // Another window saves in between, so `first` is no longer the head.
        write(&root, "two");
        let error = record(
            &root,
            "prompt",
            "persona-1",
            RecordRequest {
                base_revision_id: Some(first.revision_id.clone()),
                label: "Edited".to_string(),
                content: "three".to_string(),
                ..Default::default()
            },
        )
        .unwrap_err();
        match error {
            RevisionError::Conflict {
                head_revision_id, ..
            } => assert_ne!(
                head_revision_id.as_deref(),
                Some(first.revision_id.as_str())
            ),
            other => panic!("expected a conflict, got {other:?}"),
        }
        // ...and the refused content was NOT written.
        assert_eq!(
            history(&root, "prompt", "persona-1", None).unwrap().len(),
            2
        );
    }

    #[test]
    fn a_current_base_revision_is_accepted() {
        let root = temp_root();
        let first = write(&root, "one");
        let second = record(
            &root,
            "prompt",
            "persona-1",
            RecordRequest {
                base_revision_id: Some(first.revision_id.clone()),
                label: "Edited".to_string(),
                content: "two".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            second.parent_id.as_deref(),
            Some(first.revision_id.as_str())
        );
    }

    #[test]
    fn a_base_revision_on_a_brand_new_entity_conflicts_rather_than_inventing_a_parent() {
        let root = temp_root();
        let error = record(
            &root,
            "prompt",
            "never-saved",
            RecordRequest {
                base_revision_id: Some("made-up".to_string()),
                label: "Edited".to_string(),
                content: "x".to_string(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            RevisionError::Conflict {
                head_revision_id: None,
                ..
            }
        ));
    }

    #[test]
    fn any_two_revisions_can_be_fetched_for_comparison() {
        let root = temp_root();
        let first = write(&root, "alpha\nbeta");
        let second = write(&root, "alpha\ngamma");
        // The diff itself is rendered by `DiffViewer.tsx`, the app's one line
        // differ; the store's job is to make both sides addressable by id.
        assert_eq!(
            get(&root, "prompt", "persona-1", &first.revision_id)
                .unwrap()
                .content,
            "alpha\nbeta"
        );
        assert_eq!(
            get(&root, "prompt", "persona-1", &second.revision_id)
                .unwrap()
                .content,
            "alpha\ngamma"
        );
    }

    #[test]
    fn a_branch_forks_from_a_revision_and_advances_independently() {
        let root = temp_root();
        let first = write(&root, "shared");
        write(&root, "main moved on");
        branch_from(
            &root,
            "prompt",
            "persona-1",
            &first.revision_id,
            "experiment",
        )
        .unwrap();
        record(
            &root,
            "prompt",
            "persona-1",
            RecordRequest {
                branch: Some("experiment".to_string()),
                label: "Edited".to_string(),
                content: "experiment moved on".to_string(),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            head(&root, "prompt", "persona-1", None)
                .unwrap()
                .unwrap()
                .content,
            "main moved on"
        );
        assert_eq!(
            head(&root, "prompt", "persona-1", Some("experiment"))
                .unwrap()
                .unwrap()
                .content,
            "experiment moved on"
        );
        let summaries = branches(&root, "prompt", "persona-1").unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].name, DEFAULT_BRANCH);
        assert_eq!(summaries[1].name, "experiment");
    }

    /// Compare works ACROSS branches: `get` resolves a revision id without
    /// caring which branch it sits on, which is what lets the UI put one
    /// branch's head next to another's.
    #[test]
    fn a_revision_on_another_branch_is_still_addressable_for_compare() {
        let root = temp_root();
        let first = write(&root, "base");
        branch_from(&root, "prompt", "persona-1", &first.revision_id, "alt").unwrap();
        let alt = record(
            &root,
            "prompt",
            "persona-1",
            RecordRequest {
                branch: Some("alt".to_string()),
                label: "Edited".to_string(),
                content: "base\nextra".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            get(&root, "prompt", "persona-1", &alt.revision_id)
                .unwrap()
                .content,
            "base\nextra"
        );
        assert_eq!(
            get(&root, "prompt", "persona-1", &first.revision_id)
                .unwrap()
                .branch,
            DEFAULT_BRANCH
        );
    }

    #[test]
    fn a_duplicate_branch_name_is_refused() {
        let root = temp_root();
        let first = write(&root, "base");
        branch_from(&root, "prompt", "persona-1", &first.revision_id, "alt").unwrap();
        assert!(matches!(
            branch_from(&root, "prompt", "persona-1", &first.revision_id, "alt"),
            Err(RevisionError::Invalid(_))
        ));
    }

    #[test]
    fn a_malformed_branch_name_is_refused() {
        let root = temp_root();
        let first = write(&root, "base");
        for bad in [
            "",
            "Has Upper",
            "with/slash",
            &"x".repeat(MAX_BRANCH_LEN + 1),
        ] {
            assert!(
                matches!(
                    branch_from(&root, "prompt", "persona-1", &first.revision_id, bad),
                    Err(RevisionError::Invalid(_))
                ),
                "expected {bad:?} to be refused"
            );
        }
    }

    /// Two entity ids that slugify to the same readable prefix must not share
    /// a log file — the hash suffix on `slug` is what prevents it.
    #[test]
    fn entity_ids_that_slugify_alike_keep_separate_histories() {
        let root = temp_root();
        for entity in ["a/b", "a_b"] {
            record(
                &root,
                "prompt",
                entity,
                RecordRequest {
                    label: "Edited".to_string(),
                    content: format!("content for {entity}"),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        assert_eq!(
            head(&root, "prompt", "a/b", None).unwrap().unwrap().content,
            "content for a/b"
        );
        assert_eq!(
            head(&root, "prompt", "a_b", None).unwrap().unwrap().content,
            "content for a_b"
        );
    }

    #[test]
    fn a_traversal_shaped_entity_id_cannot_escape_the_revision_root() {
        let root = temp_root();
        record(
            &root,
            "prompt",
            "../../escape",
            RecordRequest {
                label: "Edited".to_string(),
                content: "x".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
        let path = entity_path(&root, "prompt", "../../escape");
        assert!(
            path.starts_with(&root),
            "{} escaped {}",
            path.display(),
            root.display()
        );
    }

    #[test]
    fn oversized_content_is_refused_rather_than_stored() {
        let root = temp_root();
        let error = record(
            &root,
            "prompt",
            "persona-1",
            RecordRequest {
                label: "Edited".to_string(),
                content: "x".repeat(MAX_CONTENT_BYTES + 1),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, RevisionError::Invalid(_)));
        assert!(history(&root, "prompt", "persona-1", None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn pruning_caps_the_log_but_never_drops_a_branch_head() {
        let root = temp_root();
        let first = write(&root, "seed");
        branch_from(&root, "prompt", "persona-1", &first.revision_id, "keepme").unwrap();
        for i in 0..MAX_REVISIONS_PER_ENTITY + 5 {
            write(&root, &format!("content {i}"));
        }
        let log = history(&root, "prompt", "persona-1", None).unwrap();
        assert!(
            log.len() <= MAX_REVISIONS_PER_ENTITY,
            "log grew to {}",
            log.len()
        );
        assert!(head(&root, "prompt", "persona-1", Some("keepme"))
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_corrupt_line_does_not_hide_the_surviving_revisions() {
        let root = temp_root();
        write(&root, "one");
        write(&root, "two");
        let path = entity_path(&root, "prompt", "persona-1");
        let raw = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{{not json\n{raw}")).unwrap();
        assert_eq!(
            history(&root, "prompt", "persona-1", None).unwrap().len(),
            2
        );
    }

    #[test]
    fn entities_lists_every_id_that_has_a_history() {
        let root = temp_root();
        write(&root, "one");
        record(
            &root,
            "prompt",
            "persona-2",
            RecordRequest {
                label: "Edited".to_string(),
                content: "two".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
        let mut listed = entities(&root, "prompt").unwrap();
        listed.sort();
        assert_eq!(
            listed,
            vec!["persona-1".to_string(), "persona-2".to_string()]
        );
        assert!(entities(&root, "workflow").unwrap().is_empty());
    }

    #[test]
    fn the_conflict_message_carries_the_prefix_the_frontend_matches_on() {
        let error = RevisionError::Conflict {
            head_revision_id: Some("abc".to_string()),
            head_label: None,
        };
        assert!(error.to_string().starts_with("conflict:"));
    }
}
