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

/// Staged-state changes carried alongside a portable Git worktree patch.
/// Git's unified patch format has no representation for an index-only change,
/// so these records are kept in a versioned artifact trailer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceIndexDelta {
    pub path: String,
    pub before_state: Vec<u8>,
    pub after_state: Vec<u8>,
    pub before_metadata: Vec<u8>,
    pub after_metadata: Vec<u8>,
    #[serde(default)]
    pub before_record: Vec<u8>,
    #[serde(default)]
    pub after_record: Vec<u8>,
}

const INDEX_DELTA_MAGIC: &[u8] = b"LMIDX-V1\0";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct WorkspaceSnapshotEntry {
    path: String,
    existed: bool,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    symlink_target: Option<String>,
    #[serde(default)]
    mode: u32,
    #[serde(default)]
    index_state: Vec<u8>,
    #[serde(default)]
    index_metadata: Vec<u8>,
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
    let output = run_git(
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    if !output.status.success() {
        return Err(format!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut files = Vec::new();
    let mut skip_rename_source = false;
    for record in output.stdout.split(|byte| *byte == 0) {
        if skip_rename_source {
            skip_rename_source = false;
            continue;
        }
        if record.len() < 4 {
            continue;
        }
        if matches!(record[0], b'R' | b'C') {
            skip_rename_source = true;
        }
        let value = String::from_utf8_lossy(&record[3..]).to_string();
        if value != MARKER_FILE {
            files.push(value);
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkspacePathState {
    Missing,
    File(Vec<u8>),
    Symlink(Vec<u8>),
    Other,
}

impl WorkspacePathState {
    fn bytes(self) -> Vec<u8> {
        match self {
            Self::Missing => b"missing".to_vec(),
            Self::File(bytes) => bytes,
            Self::Symlink(bytes) => [b"symlink:".as_slice(), &bytes].concat(),
            Self::Other => b"other".to_vec(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspacePathSnapshot {
    state: WorkspacePathState,
    mode: u32,
    index_state: Vec<u8>,
    index_metadata: Vec<u8>,
}

#[cfg(unix)]
fn workspace_file_mode(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn workspace_file_mode(metadata: &std::fs::Metadata) -> u32 {
    if metadata.permissions().readonly() {
        0o444
    } else {
        0o666
    }
}

fn safe_workspace_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let relative = relative_path(relative)?;
    let components = relative.components().collect::<Vec<_>>();
    let mut current = root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        if index + 1 == components.len() {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(format!(
                        "Workspace path traverses a symlink: '{relative:?}'"
                    ));
                }
                if !metadata.is_dir() {
                    return Err(format!(
                        "Workspace path traverses a non-directory: '{relative:?}'"
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("Could not inspect workspace path: {error}")),
        }
    }
    Ok(current)
}

fn workspace_path_state(root: &Path, relative: &str) -> Result<WorkspacePathState, String> {
    let absolute = safe_workspace_path(root, relative)?;
    let metadata = match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorkspacePathState::Missing);
        }
        Err(error) => {
            return Err(format!(
                "Could not inspect workspace path '{relative}': {error}"
            ))
        }
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        let target = std::fs::read_link(&absolute)
            .map_err(|error| format!("Could not read workspace symlink '{relative}': {error}"))?;
        return Ok(WorkspacePathState::Symlink(
            target.to_string_lossy().as_bytes().to_vec(),
        ));
    }
    if metadata.is_file() {
        return Ok(WorkspacePathState::File(std::fs::read(&absolute).map_err(
            |error| format!("Could not read workspace file '{relative}': {error}"),
        )?));
    }
    Ok(WorkspacePathState::Other)
}

fn workspace_index_state(root: &Path, relative: &str) -> Result<Vec<u8>, String> {
    let output = run_git(
        root,
        &["diff", "--cached", "--binary", "--no-color", "--", relative],
    )?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(format!(
            "Could not inspect staged state for '{relative}': {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

fn workspace_index_metadata(root: &Path, relative: &str) -> Result<Vec<u8>, String> {
    let output = run_git(root, &["ls-files", "--debug", "--", relative])?;
    if !output.status.success() {
        return Err(format!(
            "Could not inspect Git index metadata for '{relative}': {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

fn workspace_index_records(root: &Path) -> Result<HashMap<String, Vec<u8>>, String> {
    let output = run_git(root, &["ls-files", "-s", "-v", "-z"])?;
    if !output.status.success() {
        return Err(format!(
            "Could not inspect the Git index: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut records = HashMap::new();
    for record in output.stdout.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        let Some(separator) = record.iter().position(|byte| *byte == b'\t') else {
            return Err("Git index record omitted its path separator".to_string());
        };
        let path = String::from_utf8_lossy(&record[separator + 1..]).to_string();
        records.insert(path, record[..separator].to_vec());
    }
    Ok(records)
}

fn copy_workspace_tree(source: &Path, destination: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        if name == std::ffi::OsStr::new(".git") {
            continue;
        }
        let source_path = entry.path();
        let destination_path = destination.join(&name);
        let metadata =
            std::fs::symlink_metadata(&source_path).map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&source_path).map_err(|error| error.to_string())?;
            if let Some(parent) = destination_path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            if std::fs::metadata(&source_path).is_ok_and(|target| target.is_dir()) {
                create_workspace_directory_symlink(&target.to_string_lossy(), &destination_path)?;
            } else {
                create_workspace_symlink(&target.to_string_lossy(), &destination_path)?;
            }
        } else if metadata.is_dir() {
            std::fs::create_dir(&destination_path).map_err(|error| error.to_string())?;
            copy_workspace_tree(&source_path, &destination_path)?;
        } else if metadata.is_file() {
            if let Some(parent) = destination_path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            std::fs::copy(&source_path, &destination_path).map_err(|error| error.to_string())?;
            std::fs::set_permissions(&destination_path, metadata.permissions())
                .map_err(|error| error.to_string())?;
        } else {
            return Err(format!(
                "Cannot transport unsupported workspace entry '{}'",
                source_path.display()
            ));
        }
    }
    Ok(())
}

/// Creates a self-contained Git workspace for an isolated container.
///
/// The copy is committed with every file force-added, including ignored files,
/// so the returned patch describes only the container's mutation and can be
/// applied to the unchanged host workspace. The caller owns the returned path.
pub fn prepare_container_workspace(workspace_root: &Path) -> Result<PathBuf, String> {
    let source = workspace_root
        .canonicalize()
        .map_err(|error| format!("Could not resolve workspace root: {error}"))?;
    let destination = std::env::temp_dir().join(format!(
        "little-monkey-docker-workspace-{}",
        uuid::Uuid::new_v4().simple()
    ));
    if let Err(error) = (|| {
        std::fs::create_dir(&destination).map_err(|error| error.to_string())?;
        copy_workspace_tree(&source, &destination)?;
        for (args, operation) in [
            (vec!["init", "-q"], "initialize"),
            (
                vec!["config", "user.email", "transport@little-monkey.invalid"],
                "configure email",
            ),
            (
                vec!["config", "user.name", "Little Monkey transport"],
                "configure name",
            ),
            (
                vec!["config", "core.autocrlf", "false"],
                "configure line endings",
            ),
            (vec!["add", "-A", "-f", "--", "."], "stage files"),
            (
                vec!["commit", "-q", "--allow-empty", "-m", "container baseline"],
                "commit baseline",
            ),
        ] {
            let output = run_git(&destination, &args)?;
            if !output.status.success() {
                return Err(format!(
                    "Could not {operation} for container workspace: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
        }
        Ok::<(), String>(())
    })() {
        let _ = std::fs::remove_dir_all(&destination);
        return Err(error);
    }
    Ok(destination)
}

pub fn append_index_deltas(
    mut patch: Vec<u8>,
    deltas: &[WorkspaceIndexDelta],
) -> Result<Vec<u8>, String> {
    if deltas.is_empty() {
        return Ok(patch);
    }
    let payload = serde_json::to_vec(deltas)
        .map_err(|error| format!("Could not encode index delta: {error}"))?;
    let payload_len = u32::try_from(payload.len())
        .map_err(|_| "Index delta payload is too large to encode".to_string())?;
    if !patch.ends_with(b"\n") {
        patch.push(b'\n');
    }
    patch.extend_from_slice(INDEX_DELTA_MAGIC);
    patch.extend_from_slice(&payload);
    patch.extend_from_slice(&payload_len.to_le_bytes());
    Ok(patch)
}

pub fn split_index_deltas(artifact: &[u8]) -> Result<(Vec<u8>, Vec<WorkspaceIndexDelta>), String> {
    let trailer_len = INDEX_DELTA_MAGIC.len() + std::mem::size_of::<u32>();
    if artifact.len() < trailer_len {
        return Ok((artifact.to_vec(), Vec::new()));
    }
    let payload_len_start = artifact.len() - std::mem::size_of::<u32>();
    let payload_len = u32::from_le_bytes(
        artifact[payload_len_start..]
            .try_into()
            .expect("payload length has four bytes"),
    ) as usize;
    let payload_end = payload_len_start;
    let Some(payload_start) = payload_end.checked_sub(payload_len) else {
        return Ok((artifact.to_vec(), Vec::new()));
    };
    let Some(magic_start) = payload_start.checked_sub(INDEX_DELTA_MAGIC.len()) else {
        return Ok((artifact.to_vec(), Vec::new()));
    };
    if &artifact[magic_start..payload_start] != INDEX_DELTA_MAGIC {
        return Ok((artifact.to_vec(), Vec::new()));
    }
    let deltas = serde_json::from_slice(&artifact[payload_start..payload_end])
        .map_err(|error| format!("Could not decode index delta: {error}"))?;
    let mut patch = artifact[..magic_start].to_vec();
    while patch.last().is_some_and(u8::is_ascii_whitespace) {
        patch.pop();
    }
    Ok((patch, deltas))
}

fn workspace_path_snapshot(root: &Path, relative: &str) -> Result<WorkspacePathSnapshot, String> {
    let absolute = safe_workspace_path(root, relative)?;
    let metadata = std::fs::symlink_metadata(&absolute).ok();
    Ok(WorkspacePathSnapshot {
        state: workspace_path_state(root, relative)?,
        mode: metadata
            .as_ref()
            .map(workspace_file_mode)
            .unwrap_or_default(),
        index_state: workspace_index_state(root, relative)?,
        index_metadata: workspace_index_metadata(root, relative)?,
    })
}

fn remove_workspace_path(root: &Path, relative: &str) -> Result<(), String> {
    let absolute = safe_workspace_path(root, relative)?;
    let metadata = match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "Could not inspect workspace path '{relative}': {error}"
            ))
        }
    };
    if metadata.file_type().is_symlink() || metadata.is_file() {
        std::fs::remove_file(&absolute).map_err(|error| error.to_string())
    } else if metadata.is_dir() {
        std::fs::remove_dir_all(&absolute).map_err(|error| error.to_string())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn create_workspace_symlink(target: &str, destination: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(target, destination).map_err(|error| error.to_string())
}

#[cfg(windows)]
fn create_workspace_symlink(target: &str, destination: &Path) -> Result<(), String> {
    std::os::windows::fs::symlink_file(target, destination).map_err(|error| error.to_string())
}

#[cfg(unix)]
fn create_workspace_directory_symlink(target: &str, destination: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(target, destination).map_err(|error| error.to_string())
}

#[cfg(windows)]
fn create_workspace_directory_symlink(target: &str, destination: &Path) -> Result<(), String> {
    std::os::windows::fs::symlink_dir(target, destination).map_err(|error| error.to_string())
}

fn restore_workspace_state(
    root: &Path,
    relative: &str,
    state: &WorkspacePathState,
) -> Result<(), String> {
    let absolute = safe_workspace_path(root, relative)?;
    remove_workspace_path(root, relative)?;
    match state {
        WorkspacePathState::Missing => Ok(()),
        WorkspacePathState::File(bytes) => {
            if let Some(parent) = absolute.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            std::fs::write(absolute, bytes).map_err(|error| error.to_string())
        }
        WorkspacePathState::Symlink(target) => {
            if let Some(parent) = absolute.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            let target = String::from_utf8_lossy(target);
            create_workspace_symlink(&target, &absolute)
        }
        WorkspacePathState::Other => {
            Err(format!("Unsupported workspace path type for '{relative}'"))
        }
    }
}

#[cfg(unix)]
fn restore_workspace_mode(root: &Path, relative: &str, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let absolute = safe_workspace_path(root, relative)?;
    if let Ok(metadata) = std::fs::symlink_metadata(&absolute) {
        if !metadata.file_type().is_symlink() {
            std::fs::set_permissions(absolute, std::fs::Permissions::from_mode(mode))
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn restore_workspace_mode(root: &Path, relative: &str, mode: u32) -> Result<(), String> {
    let absolute = safe_workspace_path(root, relative)?;
    if let Ok(metadata) = std::fs::symlink_metadata(&absolute) {
        if !metadata.file_type().is_symlink() {
            let mut permissions = metadata.permissions();
            permissions.set_readonly(mode & 0o200 == 0);
            std::fs::set_permissions(absolute, permissions).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn index_has_intent_to_add(index_metadata: &[u8]) -> bool {
    let marker = b"flags: ";
    let Some(start) = index_metadata
        .windows(marker.len())
        .position(|window| window == marker)
    else {
        return false;
    };
    let value = &index_metadata[start + marker.len()..];
    u32::from_str_radix(
        std::str::from_utf8(
            value
                .split(|byte| *byte == b'\n')
                .next()
                .unwrap_or_default(),
        )
        .unwrap_or_default()
        .trim(),
        16,
    )
    .is_ok_and(|flags| flags & 0x2000_0000 != 0)
}

fn restore_workspace_index(
    root: &Path,
    relative: &str,
    index_state: &[u8],
    index_metadata: &[u8],
    index_record: &[u8],
) -> Result<(), String> {
    let _ = run_git(root, &["restore", "--staged", "--", relative]);
    if index_state.is_empty() {
        if index_has_intent_to_add(index_metadata) {
            let output = run_git(root, &["add", "-N", "--", relative])?;
            if !output.status.success() {
                return Err(format!(
                    "Could not restore intent-to-add state for '{relative}': {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
        }
        return restore_workspace_index_flags(root, relative, index_record);
    }
    use std::io::Write;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["apply", "--cached", "--"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| "Could not open git index restore input".to_string())?
        .write_all(index_state)
        .map_err(|error| error.to_string())?;
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "Could not restore staged state for '{relative}': {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    restore_workspace_index_flags(root, relative, index_record)
}

pub fn restore_workspace_index_flags(
    root: &Path,
    relative: &str,
    index_record: &[u8],
) -> Result<(), String> {
    if index_record.is_empty() {
        return Ok(());
    }
    let reset = run_git(
        root,
        &[
            "update-index",
            "--no-assume-unchanged",
            "--no-skip-worktree",
            "--",
            relative,
        ],
    )?;
    if !reset.status.success() {
        return Err(format!(
            "Could not reset Git index flags for '{relative}': {}",
            String::from_utf8_lossy(&reset.stderr).trim()
        ));
    }
    let flag = index_record.first().copied().unwrap_or(b'H');
    let option = match flag {
        b'h' => "--assume-unchanged",
        b'S' | b's' => "--skip-worktree",
        _ => return Ok(()),
    };
    let output = run_git(root, &["update-index", option, "--", relative])?;
    if !output.status.success() {
        return Err(format!(
            "Could not restore Git index flags for '{relative}': {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

fn snapshot_entries(
    data_root: &Path,
    snapshot_id: &str,
) -> Result<(PathBuf, Vec<WorkspaceSnapshotEntry>), String> {
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
    let index_records = workspace_index_records(&root)?;
    let mut entries = Vec::new();
    // Clean tracked paths need no byte-for-byte baseline: bulk Git status
    // identifies them if they become dirty, and restore falls back to HEAD.
    // Only pre-existing dirty paths need content and index preservation.
    for relative in &changed_files {
        let relative_path = relative_path(relative)?;
        let snapshot = workspace_path_snapshot(&root, relative)?;
        let mode = snapshot.mode;
        let index_state = snapshot.index_state;
        let index_metadata = snapshot.index_metadata;
        let (existed, kind, symlink_target) = match snapshot.state {
            WorkspacePathState::Missing => (false, "missing".to_string(), None),
            WorkspacePathState::File(bytes) => {
                let destination = files_dir.join(&relative_path);
                if let Some(parent) = destination.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| {
                        format!("Could not create snapshot parent for '{relative}': {error}")
                    })?;
                }
                std::fs::write(destination, bytes).map_err(|error| {
                    format!("Could not snapshot workspace file '{relative}': {error}")
                })?;
                (true, "file".to_string(), None)
            }
            WorkspacePathState::Symlink(target) => (
                true,
                "symlink".to_string(),
                Some(String::from_utf8_lossy(&target).to_string()),
            ),
            WorkspacePathState::Other => (true, "other".to_string(), None),
        };
        entries.push(WorkspaceSnapshotEntry {
            path: relative.clone(),
            existed,
            kind,
            symlink_target,
            mode,
            index_state,
            index_metadata,
        });
    }
    let entries_json = serde_json::to_vec(&entries).map_err(|error| error.to_string())?;
    std::fs::write(dir.join("entries.json"), entries_json)
        .map_err(|error| format!("Could not persist workspace snapshot: {error}"))?;
    let index_json = serde_json::to_vec(&index_records).map_err(|error| error.to_string())?;
    std::fs::write(dir.join("index.json"), index_json)
        .map_err(|error| format!("Could not persist workspace index snapshot: {error}"))?;
    let revision = workspace_revision(data_root, &root)?;
    Ok(WorkspaceSnapshot {
        id,
        revision,
        changed_files,
    })
}

fn snapshot_index_records(dir: &Path) -> Result<HashMap<String, Vec<u8>>, String> {
    match std::fs::read(dir.join("index.json")) {
        Ok(raw) => serde_json::from_slice(&raw)
            .map_err(|error| format!("Could not decode workspace index snapshot: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(error) => Err(format!("Could not read workspace index snapshot: {error}")),
    }
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
    let before_index = snapshot_index_records(&dir)?;
    let after_index = workspace_index_records(&root)?;
    let current = workspace_changed_files(&root)?;
    let mut paths = entries
        .iter()
        .map(|entry| entry.path.clone())
        .collect::<HashSet<_>>();
    paths.extend(current);
    let mut changed_index_paths = before_index
        .keys()
        .chain(after_index.keys())
        .cloned()
        .collect::<HashSet<_>>();
    changed_index_paths.retain(|path| before_index.get(path) != after_index.get(path));
    paths.extend(changed_index_paths);
    let mut changed = Vec::new();
    for relative in paths {
        let before_entry = entries.iter().find(|entry| entry.path == relative);
        let before = before_entry
            .map(|entry| {
                if !entry.existed {
                    WorkspacePathSnapshot {
                        state: WorkspacePathState::Missing,
                        mode: entry.mode,
                        index_state: entry.index_state.clone(),
                        index_metadata: entry.index_metadata.clone(),
                    }
                } else if entry.kind == "symlink" {
                    WorkspacePathSnapshot {
                        state: WorkspacePathState::Symlink(
                            entry
                                .symlink_target
                                .clone()
                                .unwrap_or_default()
                                .into_bytes(),
                        ),
                        mode: entry.mode,
                        index_state: entry.index_state.clone(),
                        index_metadata: entry.index_metadata.clone(),
                    }
                } else if entry.kind == "other" {
                    WorkspacePathSnapshot {
                        state: WorkspacePathState::Other,
                        mode: entry.mode,
                        index_state: entry.index_state.clone(),
                        index_metadata: entry.index_metadata.clone(),
                    }
                } else {
                    WorkspacePathSnapshot {
                        state: WorkspacePathState::File(
                            std::fs::read(dir.join("files").join(&relative)).unwrap_or_default(),
                        ),
                        mode: entry.mode,
                        index_state: entry.index_state.clone(),
                        index_metadata: entry.index_metadata.clone(),
                    }
                }
            })
            .or_else(|| {
                before_index.get(&relative).map(|_| {
                    let state =
                        git_head_state(&root, &relative).unwrap_or(WorkspacePathState::Missing);
                    let mode = git_head_mode(&root, &relative).unwrap_or_default();
                    WorkspacePathSnapshot {
                        state,
                        mode,
                        index_state: Vec::new(),
                        index_metadata: Vec::new(),
                    }
                })
            })
            .unwrap_or(WorkspacePathSnapshot {
                state: WorkspacePathState::Missing,
                mode: 0,
                index_state: Vec::new(),
                index_metadata: Vec::new(),
            });
        let after = workspace_path_snapshot(&root, &relative)?;
        let index_changed = before_index.get(&relative) != after_index.get(&relative);
        let detailed_index_changed = before_entry.is_some_and(|entry| {
            entry.index_state != after.index_state || entry.index_metadata != after.index_metadata
        });
        if before.state != after.state
            || git_patch_mode(&before.state, before.mode)
                != git_patch_mode(&after.state, after.mode)
            || index_changed
            || detailed_index_changed
        {
            changed.push(relative);
        }
    }
    changed.sort();
    Ok(changed)
}

fn git_head_state(root: &Path, relative: &str) -> Result<WorkspacePathState, String> {
    let tree = run_git(root, &["ls-tree", "-z", "HEAD", "--", relative])?;
    if !tree.status.success() || tree.stdout.is_empty() {
        return Ok(WorkspacePathState::Missing);
    }
    let record = tree
        .stdout
        .split(|byte| *byte == 0)
        .find(|record| !record.is_empty())
        .unwrap_or_default();
    let mode = record
        .split(|byte| *byte == b' ')
        .next()
        .unwrap_or_default();
    let bytes = run_git(root, &["show", &format!("HEAD:{relative}")])?;
    if !bytes.status.success() {
        return Err(format!("Could not read HEAD state for '{relative}'"));
    }
    if mode == b"120000" {
        Ok(WorkspacePathState::Symlink(bytes.stdout))
    } else {
        Ok(WorkspacePathState::File(bytes.stdout))
    }
}

fn git_head_mode(root: &Path, relative: &str) -> Result<u32, String> {
    let tree = run_git(root, &["ls-tree", "-z", "HEAD", "--", relative])?;
    let record = tree
        .stdout
        .split(|byte| *byte == 0)
        .find(|record| !record.is_empty())
        .unwrap_or_default();
    Ok(record
        .split(|byte| *byte == b' ')
        .next()
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| u32::from_str_radix(value, 8).ok())
        .unwrap_or_default())
}

fn git_patch_mode(state: &WorkspacePathState, filesystem_mode: u32) -> u32 {
    match state {
        WorkspacePathState::Missing => 0,
        WorkspacePathState::Symlink(_) => 0o120000,
        WorkspacePathState::File(_) => {
            if filesystem_mode & 0o111 != 0 {
                0o100755
            } else {
                0o100644
            }
        }
        WorkspacePathState::Other => 0,
    }
}

fn materialize_patch_state(path: &Path, state: &WorkspacePathState) -> Result<bool, String> {
    match state {
        WorkspacePathState::Missing => Ok(false),
        WorkspacePathState::File(bytes) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            std::fs::write(path, bytes).map_err(|error| error.to_string())?;
            Ok(true)
        }
        WorkspacePathState::Symlink(target) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            create_workspace_symlink(&String::from_utf8_lossy(target), path)?;
            Ok(true)
        }
        WorkspacePathState::Other => {
            Err("Unsupported workspace path type in autonomous patch".to_string())
        }
    }
}

fn rewrite_patch_paths(
    patch: Vec<u8>,
    relative: &str,
    before_exists: bool,
    after_exists: bool,
) -> Vec<u8> {
    let mut rewritten = Vec::new();
    let mut first = true;
    let mut old_header = false;
    let mut new_header = false;
    for line in patch.split_inclusive(|byte| *byte == b'\n') {
        if first {
            rewritten
                .extend_from_slice(format!("diff --git a/{relative} b/{relative}\n").as_bytes());
            first = false;
        } else if line.starts_with(b"--- ") && !old_header {
            rewritten.extend_from_slice(
                if before_exists {
                    format!("--- a/{relative}\n").into_bytes()
                } else {
                    b"--- /dev/null\n".to_vec()
                }
                .as_slice(),
            );
            old_header = true;
        } else if line.starts_with(b"+++ ") && !new_header {
            rewritten.extend_from_slice(
                if after_exists {
                    format!("+++ b/{relative}\n").into_bytes()
                } else {
                    b"+++ /dev/null\n".to_vec()
                }
                .as_slice(),
            );
            new_header = true;
        } else {
            rewritten.extend_from_slice(line);
        }
    }
    rewritten
}

pub fn patch_bytes_since_snapshot(
    data_root: &Path,
    workspace_root: &Path,
    snapshot_id: &str,
) -> Result<Vec<u8>, String> {
    let (dir, entries) = snapshot_entries(data_root, snapshot_id)?;
    let before_index = snapshot_index_records(&dir)?;
    let root = workspace_root
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let after_index = workspace_index_records(&root)?;
    let changed = changed_files_since_snapshot(data_root, &root, snapshot_id)?;
    // Keep both operands relative to the worktree. Git for Windows rejects
    // some absolute no-index operands even though the same paths are valid
    // from the repository root.
    let temp = root.join(format!(
        "lm-autonomous-patch-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir(&temp)
        .map_err(|error| format!("Could not create autonomous patch staging directory: {error}"))?;
    let mut patch = Vec::new();
    let mut index_deltas = Vec::new();
    for relative in changed {
        let before_entry = entries.iter().find(|entry| entry.path == relative);
        let before_state = if let Some(entry) = before_entry {
            if !entry.existed {
                WorkspacePathState::Missing
            } else if entry.kind == "symlink" {
                WorkspacePathState::Symlink(
                    entry
                        .symlink_target
                        .clone()
                        .unwrap_or_default()
                        .into_bytes(),
                )
            } else if entry.kind == "other" {
                WorkspacePathState::Other
            } else {
                WorkspacePathState::File(
                    std::fs::read(dir.join("files").join(&relative))
                        .map_err(|error| error.to_string())?,
                )
            }
        } else {
            git_head_state(&root, &relative)?
        };
        let after = workspace_path_snapshot(&root, &relative)?;
        let before_mode = before_entry
            .map(|entry| git_patch_mode(&before_state, entry.mode))
            .unwrap_or(git_head_mode(&root, &relative)?);
        let after_mode = git_patch_mode(&after.state, after.mode);
        let index_changed = before_entry.is_some_and(|entry| {
            entry.index_state != after.index_state || entry.index_metadata != after.index_metadata
        }) || before_index.get(&relative) != after_index.get(&relative);
        if index_changed {
            index_deltas.push(WorkspaceIndexDelta {
                path: relative.clone(),
                before_state: before_entry
                    .map(|entry| entry.index_state.clone())
                    .unwrap_or_default(),
                after_state: after.index_state.clone(),
                before_metadata: before_entry
                    .map(|entry| entry.index_metadata.clone())
                    .unwrap_or_default(),
                after_metadata: after.index_metadata.clone(),
                before_record: before_index.get(&relative).cloned().unwrap_or_default(),
                after_record: after_index.get(&relative).cloned().unwrap_or_default(),
            });
        }
        if before_state == after.state && before_mode == after_mode {
            continue;
        }
        let before_path = temp.join("before").join(&relative);
        let before_exists = materialize_patch_state(&before_path, &before_state)?;
        let after_exists = !matches!(after.state, WorkspacePathState::Missing);
        let before_arg = if before_exists {
            before_path
                .strip_prefix(&root)
                .map_err(|error| error.to_string())?
                .to_string_lossy()
                .into_owned()
        } else {
            let empty_path = temp.join("empty-before").join(&relative);
            if let Some(parent) = empty_path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            std::fs::write(&empty_path, []).map_err(|error| error.to_string())?;
            empty_path
                .strip_prefix(&root)
                .map_err(|error| error.to_string())?
                .to_string_lossy()
                .into_owned()
        };
        let after_arg = if after_exists {
            relative.clone()
        } else {
            let empty_path = temp.join("empty-after").join(&relative);
            if let Some(parent) = empty_path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            std::fs::write(&empty_path, []).map_err(|error| error.to_string())?;
            empty_path
                .strip_prefix(&root)
                .map_err(|error| error.to_string())?
                .to_string_lossy()
                .into_owned()
        };
        let output = run_git(
            &root,
            &[
                "diff",
                "--no-index",
                "--binary",
                "--no-prefix",
                "--",
                &before_arg,
                &after_arg,
            ],
        )?;
        if !output.status.success() && output.status.code() != Some(1) {
            let _ = std::fs::remove_dir_all(&temp);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim().chars().take(2_048).collect::<String>();
            return Err(format!(
                "Could not collect autonomous patch for '{relative}': git diff --no-index exited with status {:?}: {}",
                output.status.code(), stderr
            ));
        }
        let mut rewritten =
            rewrite_patch_paths(output.stdout, &relative, before_exists, after_exists);
        if before_mode != after_mode {
            if rewritten.is_empty() {
                rewritten = format!("diff --git a/{relative} b/{relative}\n").into_bytes();
            }
            let header = if before_mode == 0 {
                format!("new file mode {after_mode:o}\n")
            } else if after_mode == 0 {
                format!("deleted file mode {before_mode:o}\n")
            } else {
                format!("old mode {before_mode:o}\nnew mode {after_mode:o}\n")
            };
            let insert_at = rewritten
                .iter()
                .position(|byte| *byte == b'\n')
                .map(|index| index + 1)
                .unwrap_or(rewritten.len());
            rewritten.splice(insert_at..insert_at, header.as_bytes().iter().copied());
        }
        patch.extend(rewritten);
    }
    let _ = std::fs::remove_dir_all(&temp);
    append_index_deltas(patch, &index_deltas)
}

pub fn restore_workspace_paths(
    data_root: &Path,
    workspace_root: &Path,
    snapshot_id: &str,
    paths: &[String],
) -> Result<(), String> {
    let (dir, entries) = snapshot_entries(data_root, snapshot_id)?;
    let before_index = snapshot_index_records(&dir)?;
    let root = workspace_root
        .canonicalize()
        .map_err(|error| format!("Could not resolve workspace root: {error}"))?;
    for value in paths {
        let relative = relative_path(value)?;
        if let Some(entry) = entries.iter().find(|entry| entry.path == *value) {
            let state = if !entry.existed {
                WorkspacePathState::Missing
            } else if entry.kind == "symlink" {
                WorkspacePathState::Symlink(
                    entry
                        .symlink_target
                        .clone()
                        .unwrap_or_default()
                        .into_bytes(),
                )
            } else if entry.kind == "other" {
                WorkspacePathState::Other
            } else {
                WorkspacePathState::File(
                    std::fs::read(dir.join("files").join(&relative))
                        .map_err(|error| error.to_string())?,
                )
            };
            restore_workspace_state(&root, value, &state)?;
            restore_workspace_mode(&root, value, entry.mode)?;
            restore_workspace_index(
                &root,
                value,
                &entry.index_state,
                &entry.index_metadata,
                before_index
                    .get(value)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            )?;
            continue;
        }
        let tracked = run_git(&root, &["ls-files", "--error-unmatch", "--", value])
            .map(|output| output.status.success())
            .unwrap_or(false);
        if tracked {
            run_git_ok(
                &root,
                &[
                    "restore",
                    "--source=HEAD",
                    "--staged",
                    "--worktree",
                    "--",
                    value,
                ],
            )?;
            restore_workspace_index_flags(
                &root,
                value,
                before_index
                    .get(value)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            )?;
        } else {
            remove_workspace_path(&root, value)?;
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
fn worktree_patch_bytes(root: &Path) -> Result<String, String> {
    // Intent-to-add makes untracked files part of the same canonical Git patch
    // as tracked content and mode changes. The marker is then explicitly
    // unstaged so it can never enter the artifact.
    run_git_ok(root, &["add", "-A", "-N"])?;
    let _ = run_git(root, &["reset", "--", MARKER_FILE]);
    run_git_ok(root, &["diff", "HEAD", "--binary"])
}

pub fn status(data_root: &Path, path: &str) -> Result<AgentWorktreeStatus, String> {
    let record = require_registered(data_root, path)?;
    let wt = Path::new(&record.path);
    let changed_files = workspace_changed_files(wt)?;
    let dirty = !changed_files.is_empty();
    let mut diffstat = run_git_ok(wt, &["diff", "--stat", "HEAD"])
        .unwrap_or_default()
        .trim_end()
        .to_string();
    for path in &changed_files {
        let tracked = run_git(wt, &["ls-files", "--error-unmatch", "--", path])
            .map(|output| output.status.success())
            .unwrap_or(false);
        if !tracked {
            diffstat.push_str(&format!("\n{path} (untracked)"));
        }
    }
    let base_revision = run_git_ok(wt, &["rev-parse", "HEAD"])?;
    let patch = worktree_patch_bytes(wt)?;
    Ok(AgentWorktreeStatus {
        dirty,
        diffstat: diffstat.trim().to_string(),
        changed_files,
        base_revision,
        patch_digest: format!("{:x}", Sha256::digest(patch.as_bytes())),
    })
}

pub fn workspace_revision(data_root: &Path, workspace_root: &Path) -> Result<String, String> {
    let root = workspace_root
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let head = run_git_ok(&root, &["rev-parse", "HEAD"])?;
    let diff = run_git_ok(&root, &["diff", "--binary", "HEAD", "--"])?;
    let mut hasher = Sha256::new();
    hasher.update(head.as_bytes());
    hasher.update(diff.as_bytes());
    let index_records = workspace_index_records(&root)?;
    let mut index_paths = index_records.keys().cloned().collect::<Vec<_>>();
    index_paths.sort();
    for path in index_paths {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        if let Some(record) = index_records.get(&path) {
            hasher.update(record);
        }
        hasher.update([0]);
    }
    for path in workspace_changed_files(&root)? {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        let snapshot = workspace_path_snapshot(&root, &path)?;
        hasher.update(snapshot.state.bytes());
        hasher.update(snapshot.mode.to_le_bytes());
        hasher.update(snapshot.index_state);
        hasher.update(snapshot.index_metadata);
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

    let patch = worktree_patch_bytes(wt)?;
    if patch.trim().is_empty() {
        return Ok(Vec::new());
    }
    let files = run_git_ok(wt, &["diff", "HEAD", "--name-only"])?
        .lines()
        .map(str::to_string)
        .filter(|f| f != MARKER_FILE)
        .collect::<Vec<_>>();

    apply_patch(&workspace_root, patch.as_bytes(), true)?;
    apply_patch(&workspace_root, patch.as_bytes(), false)?;
    Ok(files)
}

/// Validates and applies a portable patch artifact, including any staged-state
/// trailer that unified Git patches cannot represent themselves.
pub fn apply_patch_artifact(root: &Path, artifact: &[u8]) -> Result<(), String> {
    let (patch, index_deltas) = split_index_deltas(artifact)?;
    if patch.iter().any(|byte| !byte.is_ascii_whitespace()) {
        apply_patch(root, &patch, true)?;
        apply_patch(root, &patch, false)?;
    }
    for delta in index_deltas {
        restore_workspace_index(
            root,
            &delta.path,
            &delta.after_state,
            &delta.after_metadata,
            &delta.after_record,
        )?;
    }
    Ok(())
}

fn apply_patch(root: &Path, patch: &[u8], check: bool) -> Result<(), String> {
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
        .write_all(patch)
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
        assert_eq!(
            std::fs::read_to_string(repo.join("a.txt")).unwrap(),
            "allowed\n"
        );
        assert!(!repo.join("secret.txt").exists());
        discard_snapshot(&data, &saved.id).unwrap();
        assert!(!snapshot_dir(&data, &saved.id).unwrap().exists());
    }

    #[test]
    fn workspace_snapshot_enumerates_preexisting_untracked_directories_file_by_file() {
        let data = temp_dir("data-untracked-tree");
        let repo = init_repo("untracked-tree");
        std::fs::create_dir_all(repo.join("generated")).unwrap();
        std::fs::write(repo.join("generated/preexisting.json"), "before\n").unwrap();
        let saved = snapshot(&data, &repo).unwrap();
        assert_eq!(saved.changed_files, vec!["generated/preexisting.json"]);
        std::fs::write(repo.join("generated/secret.json"), "secret\n").unwrap();
        assert_eq!(
            changed_files_since_snapshot(&data, &repo, &saved.id).unwrap(),
            vec!["generated/secret.json"]
        );
        restore_workspace_paths(
            &data,
            &repo,
            &saved.id,
            &["generated/secret.json".to_string()],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.join("generated/preexisting.json")).unwrap(),
            "before\n"
        );
        assert!(!repo.join("generated/secret.json").exists());
    }

    #[test]
    fn workspace_snapshot_patch_contains_only_the_exact_node_delta() {
        let data = temp_dir("data-exact-patch");
        let repo = init_repo("exact-patch");
        std::fs::write(repo.join("a.txt"), "user dirty\n").unwrap();
        let saved = snapshot(&data, &repo).unwrap();
        std::fs::write(repo.join("a.txt"), "worker change\n").unwrap();
        std::fs::write(repo.join("node.txt"), "node only\n").unwrap();
        let patch = patch_bytes_since_snapshot(&data, &repo, &saved.id).unwrap();
        let text = String::from_utf8_lossy(&patch);
        assert!(text.contains("-user dirty"), "{text}");
        assert!(text.contains("+worker change"), "{text}");
        assert!(text.contains("node.txt"), "{text}");
        assert!(
            !text.contains("hello"),
            "pre-node HEAD content leaked into patch: {text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_snapshot_detects_mode_only_mutations() {
        use std::os::unix::fs::PermissionsExt;
        let data = temp_dir("data-mode");
        let repo = init_repo("mode");
        let file = repo.join("a.txt");
        let mut permissions = std::fs::metadata(&file).unwrap().permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&file, permissions).unwrap();
        let saved = snapshot(&data, &repo).unwrap();
        let mut permissions = std::fs::metadata(&file).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&file, permissions).unwrap();
        let patch = patch_bytes_since_snapshot(&data, &repo, &saved.id).unwrap();
        let text = String::from_utf8_lossy(&patch);
        assert!(text.contains("old mode 100644"), "{text}");
        assert!(text.contains("new mode 100755"), "{text}");
        assert_eq!(
            changed_files_since_snapshot(&data, &repo, &saved.id).unwrap(),
            vec!["a.txt".to_string()]
        );
        restore_workspace_paths(&data, &repo, &saved.id, &["a.txt".to_string()]).unwrap();
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o7777,
            0o644
        );
    }

    #[test]
    fn workspace_snapshot_detects_and_restores_index_only_mutations() {
        let data = temp_dir("data-index");
        let repo = init_repo("index");
        std::fs::write(repo.join("a.txt"), "staged\n").unwrap();
        git(&repo, &["add", "a.txt"]);
        let saved = snapshot(&data, &repo).unwrap();
        git(&repo, &["reset", "-q", "HEAD", "--", "a.txt"]);
        assert_eq!(
            changed_files_since_snapshot(&data, &repo, &saved.id).unwrap(),
            vec!["a.txt".to_string()]
        );
        let patch = patch_bytes_since_snapshot(&data, &repo, &saved.id).unwrap();
        let (worktree_patch, index_deltas) = split_index_deltas(&patch).unwrap();
        assert!(worktree_patch.is_empty(), "{worktree_patch:?}");
        assert_eq!(index_deltas.len(), 1);
        assert_eq!(index_deltas[0].path, "a.txt");
        git(&repo, &["add", "a.txt"]);
        apply_patch_artifact(&repo, &patch).unwrap();
        assert!(run_git_ok(&repo, &["diff", "--cached", "--", "a.txt"])
            .unwrap()
            .is_empty());
        restore_workspace_paths(&data, &repo, &saved.id, &["a.txt".to_string()]).unwrap();
        let staged = run_git_ok(&repo, &["diff", "--cached", "--", "a.txt"]).unwrap();
        assert!(staged.contains("+staged"), "{staged}");
    }

    #[test]
    fn workspace_snapshot_detects_clean_file_index_flags_without_status_entries() {
        let data = temp_dir("data-index-flags");
        let repo = init_repo("index-flags");
        let saved = snapshot(&data, &repo).unwrap();
        git(
            &repo,
            &["update-index", "--assume-unchanged", "--", "a.txt"],
        );
        assert_eq!(
            changed_files_since_snapshot(&data, &repo, &saved.id).unwrap(),
            vec!["a.txt".to_string()]
        );
        let artifact = patch_bytes_since_snapshot(&data, &repo, &saved.id).unwrap();
        let verifier = init_repo("index-flags-verifier");
        apply_patch_artifact(&verifier, &artifact).unwrap();
        assert!(run_git_ok(&verifier, &["ls-files", "-s", "-v"])
            .unwrap()
            .starts_with('h'));
        git(
            &repo,
            &["update-index", "--no-assume-unchanged", "--", "a.txt"],
        );
        assert!(changed_files_since_snapshot(&data, &repo, &saved.id)
            .unwrap()
            .is_empty());
        git(&repo, &["update-index", "--skip-worktree", "--", "a.txt"]);
        assert_eq!(
            changed_files_since_snapshot(&data, &repo, &saved.id).unwrap(),
            vec!["a.txt".to_string()]
        );
        git(
            &repo,
            &["update-index", "--no-skip-worktree", "--", "a.txt"],
        );
    }

    #[test]
    fn framed_index_artifact_ignores_marker_text_inside_patch_content() {
        let plain_patch = b"diff --git a/a.txt b/a.txt\n+plain source\n".to_vec();
        assert_eq!(
            split_index_deltas(&plain_patch).unwrap(),
            (plain_patch.clone(), Vec::new())
        );
        let deltas = vec![WorkspaceIndexDelta {
            path: "a.txt".to_string(),
            before_state: Vec::new(),
            after_state: Vec::new(),
            before_metadata: Vec::new(),
            after_metadata: Vec::new(),
            before_record: Vec::new(),
            after_record: Vec::new(),
        }];
        let patch = b"+LMIDX-V1\0 inside source text\n".to_vec();
        let artifact = append_index_deltas(patch.clone(), &deltas).unwrap();
        let (decoded_patch, decoded_deltas) = split_index_deltas(&artifact).unwrap();
        assert_eq!(decoded_patch, patch[..patch.len() - 1].to_vec());
        assert_eq!(decoded_deltas, deltas);
    }

    #[test]
    fn workspace_snapshot_detects_and_restores_intent_to_add_index_state() {
        let data = temp_dir("data-intent-to-add");
        let repo = init_repo("intent-to-add");
        std::fs::write(repo.join("planned.txt"), "planned\n").unwrap();
        git(&repo, &["add", "-N", "--", "planned.txt"]);
        let saved = snapshot(&data, &repo).unwrap();
        git(&repo, &["add", "--", "planned.txt"]);
        assert_eq!(
            changed_files_since_snapshot(&data, &repo, &saved.id).unwrap(),
            vec!["planned.txt".to_string()]
        );
        restore_workspace_paths(&data, &repo, &saved.id, &["planned.txt".to_string()]).unwrap();
        let indexed = run_git_ok(&repo, &["ls-files", "--debug", "--", "planned.txt"]).unwrap();
        assert!(indexed.contains("flags: 20004000"), "{indexed:?}");
    }

    #[cfg(unix)]
    #[test]
    fn workspace_snapshot_restores_symlink_identity_without_following_target() {
        let data = temp_dir("data-symlink");
        let repo = init_repo("symlink");
        let outside = temp_dir("symlink-target");
        std::fs::write(outside.join("secret.txt"), "outside\n").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), repo.join("link.txt")).unwrap();
        let saved = snapshot(&data, &repo).unwrap();
        std::fs::remove_file(repo.join("link.txt")).unwrap();
        std::fs::write(repo.join("link.txt"), "attacker\n").unwrap();
        restore_workspace_paths(&data, &repo, &saved.id, &["link.txt".to_string()]).unwrap();
        assert!(std::fs::symlink_metadata(repo.join("link.txt"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_to_string(outside.join("secret.txt")).unwrap(),
            "outside\n"
        );
    }

    #[test]
    fn apply_of_a_clean_worktree_is_an_empty_no_op() {
        let data = temp_dir("data-noop");
        let repo = init_repo("noop");
        let record = create(&data, &repo).unwrap();
        assert!(apply(&data, &record.path).unwrap().is_empty());
    }
}
