//! Managed git worktrees for isolated `code`-profile subagents (p3 phase 2).
//!
//! A subagent dispatched with `isolation: "worktree"` runs every tool call
//! against a fresh `git worktree` of the primary workspace root instead of
//! the shared checkout, so parallel code agents can never collide on files.
//! The worktrees live under `<profile data dir>/agent-worktrees/` (resolved
//! through the same profile chokepoint as every other persistent store —
//! see `profiles.rs::ProfileScopedPaths`), on a fresh `agent/<uuid>` branch
//! at the workspace's current `HEAD`.
//!
//! ## The fail-closed deletion contract
//!
//! `worktree_remove` (and `worktree_apply`/`worktree_status`) operate ONLY
//! on paths this module itself created, enforced two ways at once:
//! membership in the JSON registry persisted next to the worktrees (so an
//! app restart cannot orphan the delete path), AND a marker file written
//! into the worktree at creation. A path failing either check is refused —
//! this is a deletion API, and "not provably ours" means "not deletable".
//!
//! ## The per-call root override
//!
//! `resolve_with_override` is how a child's tool calls are pointed at its
//! worktree: `turnEngine.ts` injects the frontend-owned
//! `workspace_root_override` reserved arg (scrubbed from model output like
//! every other reserved arg), and the file/shell tool commands route their
//! path resolution through here. The override is honoured ONLY when it names
//! a registered, marker-verified agent worktree — a forged value can at
//! worst point tools at a directory this app itself created for exactly this
//! purpose, never at an arbitrary filesystem path. Resolution inside the
//! worktree then uses the exact same escape-proof sandbox as the workspace
//! roots (`workspace::resolve_in_root`).
//!
//! Deliberately NOT built on `m5_delivery`'s owned-worktree store: that
//! subsystem's records, recovery and archival semantics are the delivery
//! pipeline's own, and agent worktrees appearing in delivery listings would
//! be a category error. The techniques (private dir, marker file, dirty-tree
//! refusal) are mirrored instead.

use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use crate::{workspace, AppState};

const DIR_NAME: &str = "agent-worktrees";
const REGISTRY_FILE: &str = "registry.json";
const MARKER_FILE: &str = ".little-monkey-agent-worktree.json";

/// Serializes every read-modify-write of the registry file across the
/// (thread-pooled) command invocations. A single process-wide lock is enough:
/// registry operations are a JSON read/write, never long-running git work.
static REGISTRY_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentWorktreeRecord {
    /// Canonical worktree path — also the registry key.
    pub path: String,
    /// The `agent/<uuid>` branch the worktree was created on.
    pub branch: String,
    /// Canonical path of the workspace root the worktree was cut from —
    /// where `git worktree remove` and `worktree_apply` run.
    pub workspace_root: String,
    pub created_at_ms: u64,
}

#[derive(Debug, serde::Serialize)]
pub struct AgentWorktreeStatus {
    pub dirty: bool,
    /// `git diff --stat HEAD` output plus a line per untracked file.
    pub diffstat: String,
    pub changed_files: Vec<String>,
    pub base_revision: String,
    pub patch_digest: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkspaceSnapshot {
    pub id: String,
    pub revision: String,
    pub changed_files: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct WorkspaceSnapshotEntry {
    path: String,
    existed: bool,
}

fn base_dir(data_root: &Path) -> PathBuf {
    data_root.join(DIR_NAME)
}

fn snapshot_dir(data_root: &Path, snapshot_id: &str) -> Result<PathBuf, String> {
    if snapshot_id.is_empty()
        || snapshot_id.len() > 128
        || !snapshot_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err("Invalid workspace snapshot id".to_string());
    }
    Ok(data_root.join("workspace-snapshots").join(snapshot_id))
}

fn relative_path(value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(format!("Workspace path escapes its root: '{value}'"));
    }
    Ok(path.to_path_buf())
}

fn workspace_changed_files(root: &Path) -> Result<Vec<String>, String> {
    let porcelain = run_git_ok(root, &["status", "--porcelain=v1"])?;
    let mut files = porcelain
        .lines()
        .filter_map(|line| {
            if line.len() < 4 {
                return None;
            }
            let value = line[3..].trim();
            if value.ends_with(MARKER_FILE) {
                return None;
            }
            Some(
                value
                    .rsplit_once(" -> ")
                    .map_or(value, |(_, renamed)| renamed)
                    .to_string(),
            )
        })
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    Ok(files)
}

fn snapshot_entries(data_root: &Path, snapshot_id: &str) -> Result<(PathBuf, Vec<WorkspaceSnapshotEntry>), String> {
    let dir = snapshot_dir(data_root, snapshot_id)?;
    let raw = std::fs::read_to_string(dir.join("entries.json"))
        .map_err(|error| format!("Could not read workspace snapshot: {error}"))?;
    let entries = serde_json::from_str(&raw)
        .map_err(|error| format!("Could not decode workspace snapshot: {error}"))?;
    Ok((dir, entries))
}

pub fn snapshot(data_root: &Path, workspace_root: &Path) -> Result<WorkspaceSnapshot, String> {
    let root = workspace_root
        .canonicalize()
        .map_err(|error| format!("Could not resolve workspace root: {error}"))?;
    let id = uuid::Uuid::new_v4().simple().to_string();
    let dir = snapshot_dir(data_root, &id)?;
    let files_dir = dir.join("files");
    std::fs::create_dir_all(&files_dir)
        .map_err(|error| format!("Could not create workspace snapshot: {error}"))?;
    let changed_files = workspace_changed_files(&root)?;
    let mut entries = Vec::new();
    for relative in &changed_files {
        let relative_path = relative_path(relative)?;
        let source = root.join(&relative_path);
        let existed = source.is_file();
        if existed {
            let destination = files_dir.join(&relative_path);
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    format!("Could not create snapshot parent for '{relative}': {error}")
                })?;
            }
            std::fs::copy(&source, &destination).map_err(|error| {
                format!("Could not snapshot workspace file '{relative}': {error}")
            })?;
        }
        entries.push(WorkspaceSnapshotEntry {
            path: relative.clone(),
            existed,
        });
    }
    let entries_json = serde_json::to_vec(&entries).map_err(|error| error.to_string())?;
    std::fs::write(dir.join("entries.json"), entries_json)
        .map_err(|error| format!("Could not persist workspace snapshot: {error}"))?;
    let revision = workspace_revision(data_root, &root)?;
    Ok(WorkspaceSnapshot {
        id,
        revision,
        changed_files,
    })
}

pub fn changed_files_since_snapshot(
    data_root: &Path,
    workspace_root: &Path,
    snapshot_id: &str,
) -> Result<Vec<String>, String> {
    let (dir, entries) = snapshot_entries(data_root, snapshot_id)?;
    let root = workspace_root
        .canonicalize()
        .map_err(|error| format!("Could not resolve workspace root: {error}"))?;
    let current = workspace_changed_files(&root)?;
    let mut paths = entries.iter().map(|entry| entry.path.clone()).collect::<HashSet<_>>();
    paths.extend(current);
    let mut changed = paths
        .into_iter()
        .filter(|relative| {
            let before_entry = entries.iter().find(|entry| entry.path == *relative);
            let before = before_entry.and_then(|entry| {
                entry.existed.then(|| std::fs::read(dir.join("files").join(relative)).ok())
            }).flatten();
            let after = std::fs::read(root.join(relative)).ok();
            before != after
        })
        .collect::<Vec<_>>();
    changed.sort();
    Ok(changed)
}

pub fn restore_workspace_paths(
    data_root: &Path,
    workspace_root: &Path,
    snapshot_id: &str,
    paths: &[String],
) -> Result<(), String> {
    let (dir, entries) = snapshot_entries(data_root, snapshot_id)?;
    let root = workspace_root
        .canonicalize()
        .map_err(|error| format!("Could not resolve workspace root: {error}"))?;
    for value in paths {
        let relative = relative_path(value)?;
        let absolute = root.join(&relative);
        if let Some(entry) = entries.iter().find(|entry| entry.path == *value) {
            if entry.existed {
                let source = dir.join("files").join(&relative);
                if let Some(parent) = absolute.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                }
                std::fs::copy(source, &absolute).map_err(|error| {
                    format!("Could not restore workspace path '{value}': {error}")
                })?;
            } else if absolute.is_file() || absolute.is_symlink() {
                std::fs::remove_file(&absolute).map_err(|error| {
                    format!("Could not remove workspace path '{value}': {error}")
                })?;
            } else if absolute.is_dir() {
                std::fs::remove_dir_all(&absolute).map_err(|error| {
                    format!("Could not remove workspace path '{value}': {error}")
                })?;
            }
            continue;
        }
        let tracked = run_git(&root, &["ls-files", "--error-unmatch", "--", value])
            .map(|output| output.status.success())
            .unwrap_or(false);
        if tracked {
            run_git_ok(
                &root,
                &["restore", "--source=HEAD", "--staged", "--worktree", "--", value],
            )?;
        } else if absolute.is_file() || absolute.is_symlink() {
            std::fs::remove_file(&absolute).map_err(|error| error.to_string())?;
        } else if absolute.is_dir() {
            std::fs::remove_dir_all(&absolute).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

pub fn discard_snapshot(data_root: &Path, snapshot_id: &str) -> Result<(), String> {
    let dir = snapshot_dir(data_root, snapshot_id)?;
    if dir.exists() {
        std::fs::remove_dir_all(dir).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn registry_path(data_root: &Path) -> PathBuf {
    base_dir(data_root).join(REGISTRY_FILE)
}

fn load_registry(data_root: &Path) -> HashMap<String, AgentWorktreeRecord> {
    let raw = match std::fs::read_to_string(registry_path(data_root)) {
        Ok(raw) => raw,
        Err(_) => return HashMap::new(),
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save_registry(
    data_root: &Path,
    registry: &HashMap<String, AgentWorktreeRecord>,
) -> Result<(), String> {
    let dir = base_dir(data_root);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create the agent-worktrees dir: {e}"))?;
    let json = serde_json::to_string_pretty(registry)
        .map_err(|e| format!("Failed to serialize the worktree registry: {e}"))?;
    std::fs::write(registry_path(data_root), json)
        .map_err(|e| format!("Failed to write the worktree registry: {e}"))
}

/// `git -C <root> <args>`, no shell — same pattern as `git.rs::run_git`,
/// duplicated as a tiny private helper so this module never depends on the
/// UI git panel's module.
fn run_git(root: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| format!("Failed to run git: {e}"))
}

fn run_git_ok(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = run_git(root, args)?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Strips Windows' extended-length (`\\?\C:\...`) prefix: git rejects such
/// paths outright (`could not create leading directories of '//?/C:/...':
/// Invalid argument`) — and since the canonical string is also the registry
/// key, keeping the prefix would break every later `worktree_remove`/`apply`
/// too. Drive-letter verbatim paths are rewritten to their plain form;
/// verbatim UNC (`\\?\UNC\...`) is left alone (rewriting it changes meaning,
/// and git cannot use it either way). Applied unconditionally rather than
/// under `cfg(windows)`: no Unix path starts with `\\?\`, and portable code
/// stays testable on every platform.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => path,
    }
}

/// `canonicalize` + [`strip_verbatim`] — every path that reaches git or the
/// registry goes through here.
fn canonicalize_for_git(path: &Path) -> std::io::Result<PathBuf> {
    path.canonicalize().map(strip_verbatim)
}

/// Creates a managed worktree of `workspace_root` at `HEAD` on a fresh
/// `agent/<uuid>` branch. Core function with explicit roots so the tests
/// never touch the shared profile data dir.
pub fn create(data_root: &Path, workspace_root: &Path) -> Result<AgentWorktreeRecord, String> {
    let workspace_canon = canonicalize_for_git(workspace_root)
        .map_err(|e| format!("Workspace root is not accessible: {e}"))?;
    run_git_ok(&workspace_canon, &["rev-parse", "--is-inside-work-tree"]).map_err(|_| {
        "The workspace is not a git repository, so a worktree cannot be created.".to_string()
    })?;

    let id = uuid::Uuid::new_v4().simple().to_string();
    let branch = format!("agent/{id}");
    let dir = base_dir(data_root);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create the agent-worktrees dir: {e}"))?;
    // Stripped BEFORE the git call: the target does not exist yet (so
    // `canonicalize_for_git` can't run), but a verbatim `data_root` would
    // make `git worktree add` fail on the joined path just the same.
    let target = strip_verbatim(dir.join(format!("wt-{id}")));

    run_git_ok(
        &workspace_canon,
        &[
            "worktree",
            "add",
            &target.to_string_lossy(),
            "-b",
            &branch,
            "HEAD",
        ],
    )?;

    let path_canon = canonicalize_for_git(&target)
        .map_err(|e| format!("Failed to canonicalize the new worktree: {e}"))?;
    let record = AgentWorktreeRecord {
        path: path_canon.to_string_lossy().to_string(),
        branch,
        workspace_root: workspace_canon.to_string_lossy().to_string(),
        created_at_ms: now_ms(),
    };

    std::fs::write(
        path_canon.join(MARKER_FILE),
        serde_json::to_string_pretty(&record).unwrap_or_default(),
    )
    .map_err(|e| format!("Failed to write the worktree marker: {e}"))?;

    let _guard = REGISTRY_LOCK
        .lock()
        .map_err(|_| "Worktree registry lock poisoned".to_string())?;
    let mut registry = load_registry(data_root);
    registry.insert(record.path.clone(), record.clone());
    save_registry(data_root, &registry)?;
    Ok(record)
}

/// The gate every destructive/override operation goes through: `path` must
/// canonicalize, be present in the registry, AND still carry the creation
/// marker. Anything else is refused — see the module doc's deletion contract.
pub fn require_registered(data_root: &Path, path: &str) -> Result<AgentWorktreeRecord, String> {
    let canon = canonicalize_for_git(Path::new(path))
        .map_err(|_| format!("'{path}' is not a managed agent worktree."))?;
    let key = canon.to_string_lossy().to_string();
    let registry = load_registry(data_root);
    let record = registry
        .get(&key)
        .cloned()
        .ok_or_else(|| format!("'{path}' is not a managed agent worktree."))?;
    if !canon.join(MARKER_FILE).is_file() {
        return Err(format!(
            "'{path}' is missing its agent-worktree marker and was not touched."
        ));
    }
    Ok(record)
}

/// Dirty flag + human-readable diffstat for a managed worktree. The marker
/// file is excluded from both, so a fresh worktree reads as clean.
pub fn status(data_root: &Path, path: &str) -> Result<AgentWorktreeStatus, String> {
    let record = require_registered(data_root, path)?;
    let wt = Path::new(&record.path);
    let porcelain = run_git_ok(wt, &["status", "--porcelain"])?;
    let interesting: Vec<&str> = porcelain
        .lines()
        .filter(|line| !line.ends_with(MARKER_FILE))
        .collect();
    let dirty = !interesting.is_empty();
    let mut diffstat = run_git_ok(wt, &["diff", "--stat", "HEAD"])
        .unwrap_or_default()
        .trim_end()
        .to_string();
    for line in &interesting {
        if line.starts_with("??") {
            diffstat.push_str(&format!("\n{} (untracked)", line[2..].trim()));
        }
    }
    let base_revision = run_git_ok(wt, &["rev-parse", "HEAD"])?;
    let mut hasher = Sha256::new();
    hasher.update(base_revision.as_bytes());
    hasher.update(
        run_git_ok(wt, &["diff", "HEAD", "--binary"])
            .unwrap_or_default()
            .as_bytes(),
    );
    let changed_files = interesting
        .iter()
        .filter_map(|line| line.get(3..))
        .map(|path| path.trim().trim_matches('"').to_string())
        .filter(|path| !path.is_empty())
        .collect::<Vec<_>>();
    for path in &changed_files {
        hasher.update(path.as_bytes());
        if let Ok(bytes) = std::fs::read(wt.join(path)) {
            hasher.update(bytes);
        }
    }
    Ok(AgentWorktreeStatus {
        dirty,
        diffstat: diffstat.trim().to_string(),
        changed_files,
        base_revision,
        patch_digest: format!("{:x}", hasher.finalize()),
    })
}

pub fn workspace_revision(data_root: &Path, workspace_root: &Path) -> Result<String, String> {
    let root = workspace_root
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let head = run_git_ok(&root, &["rev-parse", "HEAD"])?;
    let status = run_git_ok(&root, &["status", "--porcelain"])?;
    let mut hasher = Sha256::new();
    hasher.update(head.as_bytes());
    hasher.update(status.as_bytes());
    hasher.update(
        run_git_ok(&root, &["diff", "HEAD", "--binary"])
            .unwrap_or_default()
            .as_bytes(),
    );
    if let Ok(untracked) = run_git_ok(&root, &["ls-files", "--others", "--exclude-standard"]) {
        for path in untracked.lines() {
            hasher.update(path.as_bytes());
            if let Ok(bytes) = std::fs::read(root.join(path)) {
                hasher.update(bytes);
            }
        }
    }
    let _ = data_root;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Removes a managed worktree (and its `agent/<uuid>` branch). Without
/// `force`, a dirty tree is refused — the caller is expected to have applied
/// or deliberately discarded the changes first.
pub fn remove(data_root: &Path, path: &str, force: bool) -> Result<(), String> {
    let record = require_registered(data_root, path)?;
    if !force && status(data_root, path)?.dirty {
        return Err(
            "The worktree has uncommitted changes; pass force to discard them.".to_string(),
        );
    }
    let workspace_root = PathBuf::from(&record.workspace_root);
    // The marker is ours, not content — drop it so a non-force `git worktree
    // remove` of an otherwise-clean tree doesn't refuse over it.
    let _ = std::fs::remove_file(Path::new(&record.path).join(MARKER_FILE));
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&record.path);
    run_git_ok(&workspace_root, &args)?;
    let _ = run_git(&workspace_root, &["branch", "-D", &record.branch]);

    let _guard = REGISTRY_LOCK
        .lock()
        .map_err(|_| "Worktree registry lock poisoned".to_string())?;
    let mut registry = load_registry(data_root);
    registry.remove(&record.path);
    save_registry(data_root, &registry)
}

/// Applies the worktree's full diff (tracked changes + untracked files, via
/// intent-to-add) onto its origin workspace root. Validated with
/// `git apply --check` first: on any conflict the command errors and the
/// worktree is left exactly in place. Returns the touched files.
pub fn apply(data_root: &Path, path: &str) -> Result<Vec<String>, String> {
    let record = require_registered(data_root, path)?;
    let wt = Path::new(&record.path);
    let workspace_root = PathBuf::from(&record.workspace_root);

    // Intent-to-add makes untracked files show up in `git diff HEAD`; the
    // marker file is immediately unstaged again so it never joins the patch.
    run_git_ok(wt, &["add", "-A", "-N"])?;
    let _ = run_git(wt, &["reset", "--", MARKER_FILE]);
    let patch = run_git_ok(wt, &["diff", "HEAD", "--binary"])?;
    if patch.trim().is_empty() {
        return Ok(Vec::new());
    }
    let files = run_git_ok(wt, &["diff", "HEAD", "--name-only"])?
        .lines()
        .map(str::to_string)
        .filter(|f| f != MARKER_FILE)
        .collect::<Vec<_>>();

    apply_patch(&workspace_root, &patch, true)?;
    apply_patch(&workspace_root, &patch, false)?;
    Ok(files)
}

fn apply_patch(root: &Path, patch: &str, check: bool) -> Result<(), String> {
    use std::io::Write;
    let mut args = vec!["-C"];
    let root_str = root.to_string_lossy().to_string();
    args.push(&root_str);
    args.push("apply");
    if check {
        args.push("--check");
    }
    let mut child = Command::new("git")
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to run git apply: {e}"))?;
    child
        .stdin
        .as_mut()
        .ok_or("Failed to open git apply stdin")?
        .write_all(patch.as_bytes())
        .map_err(|e| format!("Failed to write the patch to git apply: {e}"))?;
    let output = child
        .wait_with_output()
        .map_err(|e| format!("git apply did not finish: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{}: {}",
            if check {
                "The changes no longer apply cleanly to the workspace (conflict); the worktree was left in place"
            } else {
                "git apply failed"
            },
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// The tool-dispatch path resolver — `workspace::resolve_path_and_root`
/// unless a `workspace_root_override` names a registered agent worktree, in
/// which case resolution is sandboxed inside THAT root instead. See the
/// module doc's override contract.
pub fn resolve_with_override(
    state: &AppState,
    raw: &str,
    override_root: Option<&str>,
) -> Result<(PathBuf, PathBuf), String> {
    match override_root {
        None => workspace::resolve_path_and_root(state, raw),
        Some(root) => {
            let data_root = crate::app_paths::data_dir()
                .ok_or_else(|| "Could not resolve the application data directory".to_string())?;
            // Two kinds of directory this app creates for tools to be pointed
            // at: a managed agent worktree, and a disposable learning
            // evaluation sandbox. Both are marker-verified inside app-owned
            // roots, so an override can still only ever name one of them.
            let canon = match require_registered(&data_root, root) {
                Ok(record) => PathBuf::from(&record.path),
                Err(worktree_error) => {
                    crate::skill_learning::require_eval_sandbox(&data_root, root)
                        .map_err(|_| worktree_error)?
                }
            };
            let resolved = workspace::resolve_in_root(&canon, raw)?;
            Ok((resolved, canon))
        }
    }
}

fn profile_data_root(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    use crate::profiles::ProfileScopedPaths;
    app.profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {e}"))
}

fn audit(
    app: &tauri::AppHandle,
    action: &str,
    outcome: crate::run_ledger::SubsystemOutcome,
    detail: Option<serde_json::Value>,
) {
    crate::subsystem_audit::SubsystemAudit::desktop(app.clone()).record(
        crate::subsystem_audit::SubsystemAction {
            subsystem: crate::run_ledger::Subsystem::Worktree,
            action: action.to_string(),
            turn_id: None,
            permission_request_id: None,
            outcome,
            detail,
        },
    );
}

#[tauri::command]
pub fn worktree_create(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<AgentWorktreeRecord, String> {
    let data_root = profile_data_root(&app)?;
    let workspace_root = workspace::primary_root_canon(state.inner())?;
    let result = create(&data_root, &workspace_root);
    audit(
        &app,
        "create",
        if result.is_ok() {
            crate::run_ledger::SubsystemOutcome::Succeeded
        } else {
            crate::run_ledger::SubsystemOutcome::Failed
        },
        result
            .as_ref()
            .ok()
            .map(|r| serde_json::json!({ "path": r.path, "branch": r.branch })),
    );
    result
}

#[tauri::command]
pub fn worktree_status(app: tauri::AppHandle, path: String) -> Result<AgentWorktreeStatus, String> {
    let data_root = profile_data_root(&app)?;
    status(&data_root, &path)
}

#[tauri::command]
pub fn worktree_workspace_revision(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let data_root = profile_data_root(&app)?;
    let workspace_root = workspace::primary_root_canon(state.inner())?;
    workspace_revision(&data_root, &workspace_root)
}

#[tauri::command]
pub fn worktree_workspace_snapshot(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<WorkspaceSnapshot, String> {
    let data_root = profile_data_root(&app)?;
    let workspace_root = workspace::primary_root_canon(state.inner())?;
    snapshot(&data_root, &workspace_root)
}

#[tauri::command]
pub fn worktree_workspace_changed_files_since_snapshot(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    snapshot_id: String,
) -> Result<Vec<String>, String> {
    let data_root = profile_data_root(&app)?;
    let workspace_root = workspace::primary_root_canon(state.inner())?;
    changed_files_since_snapshot(&data_root, &workspace_root, &snapshot_id)
}

#[tauri::command]
pub fn worktree_workspace_restore_paths(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    snapshot_id: String,
    paths: Vec<String>,
) -> Result<(), String> {
    let data_root = profile_data_root(&app)?;
    let workspace_root = workspace::primary_root_canon(state.inner())?;
    restore_workspace_paths(&data_root, &workspace_root, &snapshot_id, &paths)
}

#[tauri::command]
pub fn worktree_workspace_snapshot_discard(
    app: tauri::AppHandle,
    snapshot_id: String,
) -> Result<(), String> {
    let data_root = profile_data_root(&app)?;
    discard_snapshot(&data_root, &snapshot_id)
}

#[tauri::command]
pub fn worktree_remove(app: tauri::AppHandle, path: String, force: bool) -> Result<(), String> {
    let data_root = profile_data_root(&app)?;
    let result = remove(&data_root, &path, force);
    audit(
        &app,
        "remove",
        if result.is_ok() {
            crate::run_ledger::SubsystemOutcome::Succeeded
        } else {
            crate::run_ledger::SubsystemOutcome::Failed
        },
        Some(serde_json::json!({ "path": path, "force": force })),
    );
    result
}

#[tauri::command]
pub fn worktree_apply(app: tauri::AppHandle, path: String) -> Result<Vec<String>, String> {
    let data_root = profile_data_root(&app)?;
    let result = apply(&data_root, &path);
    audit(
        &app,
        "apply",
        if result.is_ok() {
            crate::run_ledger::SubsystemOutcome::Succeeded
        } else {
            crate::run_ledger::SubsystemOutcome::Failed
        },
        Some(serde_json::json!({ "path": path })),
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lm-agentwt-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo(tag: &str) -> PathBuf {
        let repo = temp_dir(&format!("repo-{tag}"));
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        git(&repo, &["config", "user.name", "t"]);
        // Windows CI's system git ships core.autocrlf=true, which rewrites
        // the fixture's \n to \r\n during apply and fails byte-equality
        // asserts. The tests assert exact bytes, so pin conversion off —
        // worktrees share the repo's config, so this covers them too.
        git(&repo, &["config", "core.autocrlf", "false"]);
        std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        repo
    }

    #[test]
    fn strip_verbatim_rewrites_drive_letter_paths_and_nothing_else() {
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\C:\Users\x\wt-1")),
            PathBuf::from(r"C:\Users\x\wt-1")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\UNC\server\share\wt-1")),
            PathBuf::from(r"\\?\UNC\server\share\wt-1")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from("/tmp/plain/unix")),
            PathBuf::from("/tmp/plain/unix")
        );
    }

    #[test]
    fn create_then_remove_clean_worktree() {
        let data = temp_dir("data-clean");
        let repo = init_repo("clean");
        let record = create(&data, &repo).unwrap();
        assert!(Path::new(&record.path).join("a.txt").is_file());
        assert!(record.branch.starts_with("agent/"));
        assert!(
            !status(&data, &record.path).unwrap().dirty,
            "fresh worktree must read clean despite the marker"
        );
        remove(&data, &record.path, false).unwrap();
        assert!(!Path::new(&record.path).exists());
        assert!(load_registry(&data).is_empty());
    }

    #[test]
    fn non_managed_paths_are_refused_by_every_operation() {
        let data = temp_dir("data-refuse");
        let victim = init_repo("victim");
        let victim_str = victim.to_string_lossy().to_string();
        for result in [
            remove(&data, &victim_str, true).err(),
            status(&data, &victim_str).err(),
            apply(&data, &victim_str).err(),
        ] {
            let message = result.expect("operation on a non-managed path must fail");
            assert!(
                message.contains("not a managed agent worktree"),
                "{message}"
            );
        }
        assert!(
            victim.exists(),
            "the non-managed directory must be untouched"
        );
    }

    #[test]
    fn registry_entry_without_marker_is_refused() {
        let data = temp_dir("data-marker");
        let repo = init_repo("marker");
        let record = create(&data, &repo).unwrap();
        std::fs::remove_file(Path::new(&record.path).join(MARKER_FILE)).unwrap();
        let err = remove(&data, &record.path, true).unwrap_err();
        assert!(err.contains("marker"), "{err}");
    }

    #[test]
    fn dirty_worktree_refuses_non_force_remove_and_reports_diffstat() {
        let data = temp_dir("data-dirty");
        let repo = init_repo("dirty");
        let record = create(&data, &repo).unwrap();
        std::fs::write(Path::new(&record.path).join("a.txt"), "changed\n").unwrap();
        std::fs::write(Path::new(&record.path).join("new.txt"), "brand new\n").unwrap();

        let st = status(&data, &record.path).unwrap();
        assert!(st.dirty);
        assert!(st.diffstat.contains("a.txt"), "{}", st.diffstat);
        assert!(st.diffstat.contains("new.txt"), "{}", st.diffstat);

        let err = remove(&data, &record.path, false).unwrap_err();
        assert!(err.contains("uncommitted"), "{err}");
        assert!(Path::new(&record.path).exists());

        remove(&data, &record.path, true).unwrap();
        assert!(!Path::new(&record.path).exists());
    }

    #[test]
    fn apply_lands_tracked_and_untracked_changes_in_the_origin_repo() {
        let data = temp_dir("data-apply");
        let repo = init_repo("apply");
        let record = create(&data, &repo).unwrap();
        std::fs::write(Path::new(&record.path).join("a.txt"), "hello\nworld\n").unwrap();
        std::fs::write(Path::new(&record.path).join("new.txt"), "brand new\n").unwrap();

        let mut files = apply(&data, &record.path).unwrap();
        files.sort();
        assert_eq!(files, vec!["a.txt".to_string(), "new.txt".to_string()]);
        assert_eq!(
            std::fs::read_to_string(repo.join("a.txt")).unwrap(),
            "hello\nworld\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("new.txt")).unwrap(),
            "brand new\n"
        );
        assert!(
            Path::new(&record.path).exists(),
            "apply never deletes the worktree itself"
        );
    }

    #[test]
    fn apply_conflict_errors_and_leaves_both_sides_alone() {
        let data = temp_dir("data-conflict");
        let repo = init_repo("conflict");
        let record = create(&data, &repo).unwrap();
        std::fs::write(Path::new(&record.path).join("a.txt"), "agent version\n").unwrap();
        // Conflicting change in the origin repo AFTER the worktree was cut.
        std::fs::write(repo.join("a.txt"), "user version\n").unwrap();

        let err = apply(&data, &record.path).unwrap_err();
        assert!(err.contains("conflict"), "{err}");
        assert_eq!(
            std::fs::read_to_string(repo.join("a.txt")).unwrap(),
            "user version\n"
        );
        assert_eq!(
            std::fs::read_to_string(Path::new(&record.path).join("a.txt")).unwrap(),
            "agent version\n"
        );
    }

    #[test]
    fn workspace_snapshot_restores_only_the_unauthorized_delta() {
        let data = temp_dir("data-snapshot");
        let repo = init_repo("snapshot");
        let saved = snapshot(&data, &repo).unwrap();
        std::fs::write(repo.join("a.txt"), "allowed\n").unwrap();
        std::fs::write(repo.join("secret.txt"), "secret\n").unwrap();
        let changed = changed_files_since_snapshot(&data, &repo, &saved.id).unwrap();
        assert_eq!(changed, vec!["a.txt".to_string(), "secret.txt".to_string()]);
        restore_workspace_paths(&data, &repo, &saved.id, &["secret.txt".to_string()]).unwrap();
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "allowed\n");
        assert!(!repo.join("secret.txt").exists());
        discard_snapshot(&data, &saved.id).unwrap();
        assert!(!snapshot_dir(&data, &saved.id).unwrap().exists());
    }

    #[test]
    fn apply_of_a_clean_worktree_is_an_empty_no_op() {
        let data = temp_dir("data-noop");
        let repo = init_repo("noop");
        let record = create(&data, &repo).unwrap();
        assert!(apply(&data, &record.path).unwrap().is_empty());
    }
}
