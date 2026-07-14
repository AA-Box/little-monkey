use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};

use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::store::{ensure_private_directory, restrict_file, DeliveryStore};
use super::{
    ChangedFile, DeliveryPolicy, DiffBundle, OwnedWorktreeRecord, OwnershipMarker,
    WorktreeCreateRequest, WorktreeInspection,
};

pub const MARKER_FILE: &str = ".little-monkey-owned-worktree.json";
const MAX_GIT_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_MARKER_BYTES: u64 = 256 * 1024;

pub fn create_owned_worktree(
    store: &mut DeliveryStore,
    request: &WorktreeCreateRequest,
    now_ms: u64,
) -> Result<OwnedWorktreeRecord, String> {
    request.validate()?;
    let repository = canonical_repository(Path::new(&request.repository_root))?;
    let common_git_dir = canonical_git_path(
        &repository,
        &git_text(&repository, &["rev-parse", "--git-common-dir"])?,
    )?;
    let repository_id = format!(
        "repo-{}",
        &sha256_hex(common_git_dir.to_string_lossy().as_bytes())[..24]
    );
    for remote in &request.allowed_remotes {
        validate_remote_identity(&repository, remote, &request.repository_slug)?;
    }
    validate_ref_name(&repository, &request.base_ref)?;
    let base_oid = git_text(
        &repository,
        &[
            "rev-parse",
            "--verify",
            &format!("{}^{{commit}}", request.base_ref),
        ],
    )?;
    validate_oid(&base_oid)?;

    let worktree_id = format!("wt-{}", Uuid::new_v4().simple());
    let suffix = &worktree_id[3..11];
    let branch = format!(
        "{}{}-{suffix}",
        request.branch_prefix,
        safe_branch_label(&request.label)
    );
    validate_ref_name(&repository, &branch)?;
    if request
        .protected_branches
        .iter()
        .any(|protected| protected == &branch)
    {
        return Err("Owned branch collides with a protected branch".to_string());
    }

    let worktree_root = store.root.join("worktrees");
    ensure_private_directory(&worktree_root)?;
    let target = worktree_root.join(&worktree_id);
    let output = git_output_os(
        &repository,
        vec![
            OsString::from("worktree"),
            OsString::from("add"),
            OsString::from("-b"),
            OsString::from(&branch),
            target.as_os_str().to_owned(),
            OsString::from(&base_oid),
        ],
        None,
    )?;
    require_success(&output, "git worktree add")?;
    let canonical_path = target
        .canonicalize()
        .map_err(|error| format!("Could not canonicalize new worktree: {error}"))?;
    let head_oid = git_text(&canonical_path, &["rev-parse", "HEAD"])?;
    if head_oid != base_oid {
        return Err("New worktree HEAD does not match the declared base".to_string());
    }

    let marker = OwnershipMarker {
        schema_version: 1,
        worktree_id: worktree_id.clone(),
        lease_nonce: Uuid::new_v4().simple().to_string(),
        repository_id,
        repository_slug: request.repository_slug.to_ascii_lowercase(),
        repository_root: repository.to_string_lossy().to_string(),
        common_git_dir: common_git_dir.to_string_lossy().to_string(),
        canonical_path: canonical_path.to_string_lossy().to_string(),
        branch,
        base_oid,
        policy: DeliveryPolicy {
            allowed_remotes: request.allowed_remotes.clone(),
            branch_prefix: request.branch_prefix.clone(),
            protected_branches: request.protected_branches.clone(),
            allow_push: request.allow_push,
            allow_create_pull_request: request.allow_create_pull_request,
            allow_review_comment: request.allow_review_comment,
            allow_fork_writes: request.allow_fork_writes,
        },
        created_at_ms: now_ms,
    };
    let record = OwnedWorktreeRecord {
        marker,
        state: "active".to_string(),
        locked: false,
        lock_reason: None,
        archive_path: None,
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    };
    write_marker(&record.marker)?;
    store.insert_worktree(&record)?;
    Ok(record)
}

pub fn recover_owned_worktrees(
    store: &mut DeliveryStore,
    now_ms: u64,
) -> Result<Vec<OwnedWorktreeRecord>, String> {
    let root = store.root.join("worktrees");
    ensure_private_directory(&root)?;
    let mut recovered = Vec::new();
    let mut count = 0usize;
    for entry in fs::read_dir(&root).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        count += 1;
        if count > 1_024 {
            return Err("Owned worktree recovery exceeds 1024 entries".to_string());
        }
        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| error.to_string())?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let marker_path = entry.path().join(MARKER_FILE);
        let Ok(marker_metadata) = fs::symlink_metadata(&marker_path) else {
            continue;
        };
        if !marker_metadata.file_type().is_file()
            || marker_metadata.file_type().is_symlink()
            || marker_metadata.len() > MAX_MARKER_BYTES
        {
            continue;
        }
        let marker: OwnershipMarker =
            serde_json::from_slice(&fs::read(&marker_path).map_err(|error| error.to_string())?)
                .map_err(|error| {
                    format!(
                        "Invalid recovery marker '{}': {error}",
                        marker_path.display()
                    )
                })?;
        if store.worktree(&marker.worktree_id)?.is_some() {
            continue;
        }
        let record = OwnedWorktreeRecord {
            created_at_ms: marker.created_at_ms,
            updated_at_ms: now_ms,
            marker,
            state: "recovered".to_string(),
            locked: true,
            lock_reason: Some("Recovered after interrupted ownership registration".to_string()),
            archive_path: None,
        };
        validate_live(store, &record)?;
        store.insert_worktree(&record)?;
        store.update_worktree_lock(
            &record.marker.worktree_id,
            true,
            record.lock_reason.as_deref(),
            now_ms,
        )?;
        recovered.push(record);
    }
    Ok(recovered)
}

pub fn validate_live(
    store: &DeliveryStore,
    record: &OwnedWorktreeRecord,
) -> Result<PathBuf, String> {
    if record.state == "cleaned" {
        return Err("Owned worktree was already cleaned".to_string());
    }
    record.marker.validate()?;
    let owned_root = store
        .root
        .join("worktrees")
        .canonicalize()
        .map_err(|error| format!("Could not canonicalize owned root: {error}"))?;
    let path = Path::new(&record.marker.canonical_path)
        .canonicalize()
        .map_err(|error| format!("Owned worktree is missing: {error}"))?;
    if !path.starts_with(&owned_root) {
        return Err("Owned worktree escaped application-owned storage".to_string());
    }
    let marker_path = path.join(MARKER_FILE);
    let metadata = fs::symlink_metadata(&marker_path)
        .map_err(|error| format!("Ownership marker is missing: {error}"))?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_MARKER_BYTES
    {
        return Err("Ownership marker is not a bounded regular file".to_string());
    }
    let marker: OwnershipMarker =
        serde_json::from_slice(&fs::read(&marker_path).map_err(|error| error.to_string())?)
            .map_err(|error| format!("Ownership marker is invalid: {error}"))?;
    if marker != record.marker {
        return Err("Ownership marker does not match the durable registry".to_string());
    }
    let branch = git_text(&path, &["branch", "--show-current"])?;
    if branch != record.marker.branch
        || !branch.starts_with(&record.marker.policy.branch_prefix)
        || record
            .marker
            .policy
            .protected_branches
            .iter()
            .any(|protected| protected == &branch)
    {
        return Err(format!(
            "Worktree branch '{branch}' violates its frozen policy"
        ));
    }
    let common = canonical_git_path(&path, &git_text(&path, &["rev-parse", "--git-common-dir"])?)?;
    if common.to_string_lossy() != record.marker.common_git_dir {
        return Err("Owned worktree now belongs to a different repository".to_string());
    }
    Ok(path)
}

pub fn inspect_owned_worktree(
    store: &DeliveryStore,
    record: &OwnedWorktreeRecord,
) -> Result<WorktreeInspection, String> {
    let path = validate_live(store, record)?;
    let head_oid = git_text(&path, &["rev-parse", "HEAD"])?;
    validate_oid(&head_oid)?;
    let files = changed_files(&path, false)?;
    let cleanup_blockers = changed_files(&path, true)?;
    let ahead_behind = git_text(
        &path,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{}...HEAD", record.marker.base_oid),
        ],
    )?;
    let mut counts = ahead_behind.split_whitespace();
    let behind = counts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let ahead = counts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let diffs = DiffBundle {
        staged: bounded_diff(git_bytes(
            &path,
            &["diff", "--cached", "--no-ext-diff", "--no-textconv", "--"],
        )?),
        unstaged: bounded_diff(git_bytes(
            &path,
            &["diff", "--no-ext-diff", "--no-textconv", "--"],
        )?),
        head: bounded_diff(git_bytes(
            &path,
            &["diff", "HEAD", "--no-ext-diff", "--no-textconv", "--"],
        )?),
    };
    Ok(WorktreeInspection {
        worktree: record.clone(),
        head_oid,
        ahead,
        behind,
        dirty: !files.is_empty(),
        cleanup_blocked: !cleanup_blockers.is_empty(),
        files,
        diffs,
    })
}

pub fn set_lock(
    store: &mut DeliveryStore,
    worktree_id: &str,
    locked: bool,
    reason: Option<&str>,
    now_ms: u64,
) -> Result<OwnedWorktreeRecord, String> {
    let record = require_worktree(store, worktree_id)?;
    validate_live(store, &record)?;
    let reason = if locked {
        let value = reason.unwrap_or("Locked by operator").trim();
        if value.is_empty() || value.len() > 512 || value.contains(['\r', '\n']) {
            return Err("Worktree lock reason must be one line up to 512 characters".to_string());
        }
        Some(value)
    } else {
        None
    };
    store.update_worktree_lock(worktree_id, locked, reason, now_ms)?;
    require_worktree(store, worktree_id)
}

pub fn stage_paths(
    store: &DeliveryStore,
    worktree_id: &str,
    paths: &[String],
) -> Result<WorktreeInspection, String> {
    let record = require_worktree(store, worktree_id)?;
    require_mutable(&record)?;
    let root = validate_live(store, &record)?;
    let paths = validate_paths(paths)?;
    let mut args = vec![OsString::from("add"), OsString::from("--")];
    args.extend(paths.into_iter().map(OsString::from));
    let output = git_output_os(&root, args, None)?;
    require_success(&output, "git add")?;
    inspect_owned_worktree(store, &record)
}

pub fn commit_paths(
    store: &DeliveryStore,
    worktree_id: &str,
    paths: &[String],
    message: &str,
) -> Result<(String, WorktreeInspection), String> {
    let record = require_worktree(store, worktree_id)?;
    require_mutable(&record)?;
    let message = message.trim();
    if message.is_empty() || message.len() > 10_000 || message.contains('\0') {
        return Err("Commit message must contain 1 to 10000 characters".to_string());
    }
    let root = validate_live(store, &record)?;
    let paths = validate_paths(paths)?;
    let mut add_args = vec![OsString::from("add"), OsString::from("--")];
    add_args.extend(paths.iter().map(OsString::from));
    require_success(&git_output_os(&root, add_args, None)?, "git add")?;

    let mut commit_args = vec![
        OsString::from("commit"),
        OsString::from("--only"),
        OsString::from("--no-verify"),
        OsString::from("-m"),
        OsString::from(message),
        OsString::from("--"),
    ];
    commit_args.extend(paths.iter().map(OsString::from));
    let output = git_output_os(&root, commit_args, None)?;
    require_success(&output, "git commit")?;
    let oid = git_text(&root, &["rev-parse", "HEAD"])?;
    validate_oid(&oid)?;
    Ok((oid, inspect_owned_worktree(store, &record)?))
}

pub fn push_owned_branch(
    store: &DeliveryStore,
    worktree_id: &str,
    remote: &str,
) -> Result<String, String> {
    let record = require_worktree(store, worktree_id)?;
    require_mutable(&record)?;
    if !record.marker.policy.allow_push {
        return Err("This worktree policy does not allow push".to_string());
    }
    if !record
        .marker
        .policy
        .allowed_remotes
        .iter()
        .any(|allowed| allowed == remote)
    {
        return Err(format!("Remote '{remote}' is outside the frozen policy"));
    }
    let root = validate_live(store, &record)?;
    validate_remote_identity(&root, remote, &record.marker.repository_slug).map_err(|error| {
        format!("Remote repository identity changed after worktree creation: {error}")
    })?;
    let refspec = format!("HEAD:refs/heads/{}", record.marker.branch);
    let output = git_output_os(
        &root,
        vec![
            OsString::from("push"),
            OsString::from("--porcelain"),
            OsString::from("--no-all"),
            OsString::from("--no-mirror"),
            OsString::from("--no-delete"),
            OsString::from("--no-tags"),
            OsString::from("--no-prune"),
            OsString::from("--no-force"),
            OsString::from("--no-force-with-lease"),
            OsString::from("--no-force-if-includes"),
            OsString::from("--no-follow-tags"),
            OsString::from("--no-push-option"),
            OsString::from("--no-recurse-submodules"),
            OsString::from("--set-upstream"),
            OsString::from(remote),
            OsString::from(refspec),
        ],
        None,
    )?;
    require_success(&output, "git push")?;
    Ok(bounded(
        &format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
        16 * 1024,
    ))
}

pub fn archive_owned_worktree(
    store: &mut DeliveryStore,
    worktree_id: &str,
    now_ms: u64,
) -> Result<OwnedWorktreeRecord, String> {
    let record = require_worktree(store, worktree_id)?;
    if record.locked {
        return Err("Unlock the worktree before archiving it".to_string());
    }
    if record.state == "archived" {
        return Ok(record);
    }
    require_mutable(&record)?;
    let root = validate_live(store, &record)?;
    if !changed_files(&root, true)?.is_empty() {
        return Err(
            "Refusing to archive a dirty worktree or one containing ignored files".to_string(),
        );
    }
    let archive_root = store.root.join("archives").join(worktree_id);
    ensure_private_directory(&archive_root)?;
    let bundle = archive_root.join("history.bundle");
    let temporary = archive_root.join(format!("history-{}.tmp", Uuid::new_v4().simple()));
    let output = git_output_os(
        &root,
        vec![
            OsString::from("bundle"),
            OsString::from("create"),
            temporary.as_os_str().to_owned(),
            OsString::from(&record.marker.branch),
        ],
        None,
    )?;
    require_success(&output, "git bundle create")?;
    require_success(
        &git_output_os(
            &root,
            vec![
                OsString::from("bundle"),
                OsString::from("verify"),
                temporary.as_os_str().to_owned(),
            ],
            None,
        )?,
        "git bundle verify",
    )?;
    fs::rename(&temporary, &bundle).map_err(|error| error.to_string())?;
    restrict_file(&bundle)?;
    let metadata_path = archive_root.join("metadata.json");
    atomic_write_json(&metadata_path, &record)?;
    let archive_text = archive_root.to_string_lossy().to_string();
    store.update_worktree_state(worktree_id, "archived", Some(&archive_text), now_ms)?;
    require_worktree(store, worktree_id)
}

pub fn cleanup_owned_worktree(
    store: &mut DeliveryStore,
    worktree_id: &str,
    now_ms: u64,
) -> Result<OwnedWorktreeRecord, String> {
    let record = require_worktree(store, worktree_id)?;
    if record.state == "cleaned" {
        return Ok(record);
    }
    if record.state != "archived" || record.archive_path.is_none() {
        return Err("Archive the worktree before cleanup".to_string());
    }
    if record.locked {
        return Err("Unlock the worktree before cleanup".to_string());
    }
    let root = validate_live(store, &record)?;
    if !changed_files(&root, true)?.is_empty() {
        return Err(
            "Refusing cleanup because tracked, untracked, or ignored files remain".to_string(),
        );
    }
    let marker_path = root.join(MARKER_FILE);
    fs::remove_file(&marker_path)
        .map_err(|error| format!("Could not stage ownership marker removal: {error}"))?;
    let repository = Path::new(&record.marker.repository_root);
    let output = git_output_os(
        repository,
        vec![
            OsString::from("worktree"),
            OsString::from("remove"),
            root.as_os_str().to_owned(),
        ],
        None,
    )?;
    if let Err(error) = require_success(&output, "git worktree remove") {
        let _ = write_marker(&record.marker);
        return Err(error);
    }
    store.update_worktree_state(worktree_id, "cleaned", None, now_ms)?;
    require_worktree(store, worktree_id)
}

pub fn require_worktree(
    store: &DeliveryStore,
    worktree_id: &str,
) -> Result<OwnedWorktreeRecord, String> {
    super::validate_id("worktree id", worktree_id)?;
    store
        .worktree(worktree_id)?
        .ok_or_else(|| format!("Unknown owned worktree '{worktree_id}'"))
}

pub fn github_slug_from_remote(value: &str) -> Result<String, String> {
    let value = value.trim();
    let path = if let Some(rest) = value.strip_prefix("git@github.com:") {
        rest
    } else if let Some(rest) = value.strip_prefix("ssh://git@github.com/") {
        rest
    } else if let Some(rest) = value.strip_prefix("https://github.com/") {
        if rest.contains(['?', '#', '@']) {
            return Err(
                "GitHub HTTPS remote must not contain credentials, query, or fragment".to_string(),
            );
        }
        rest
    } else {
        return Err(
            "Only credential-free github.com HTTPS or SSH remotes are supported".to_string(),
        );
    };
    let slug = path.trim_end_matches('/').trim_end_matches(".git");
    super::validate_repository_slug(slug)?;
    Ok(slug.to_ascii_lowercase())
}

/// Validates every configured fetch and push URL. Git permits a distinct
/// `pushurl`, so checking only the ordinary fetch URL would allow a repository
/// to redirect the confirmed push after the worktree policy was frozen.
fn validate_remote_identity(root: &Path, remote: &str, expected_slug: &str) -> Result<(), String> {
    let configured_urls = [
        (
            "fetch",
            git_text(root, &["remote", "get-url", "--all", remote])?,
        ),
        (
            "push",
            git_text(root, &["remote", "get-url", "--push", "--all", remote])?,
        ),
    ];
    for (scope, urls) in configured_urls {
        let mut count = 0usize;
        for url in urls.lines().map(str::trim).filter(|url| !url.is_empty()) {
            count += 1;
            if count > 32 {
                return Err(format!("Remote '{remote}' has too many {scope} URLs"));
            }
            let actual_slug = github_slug_from_remote(url)?;
            if !actual_slug.eq_ignore_ascii_case(expected_slug) {
                return Err(format!(
                    "Remote '{remote}' {scope} URL points to '{actual_slug}', not declared repository '{expected_slug}'"
                ));
            }
        }
        if count == 0 {
            return Err(format!("Remote '{remote}' has no {scope} URL"));
        }
    }
    Ok(())
}

pub fn git_text(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = git_output_os(root, args.iter().map(OsString::from).collect(), None)?;
    require_success(&output, &format!("git {}", args.join(" ")))?;
    if output.stdout.len() > MAX_GIT_OUTPUT {
        return Err("Git output exceeds 8 MiB".to_string());
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_string())
        .map_err(|_| "Git returned non-UTF-8 text".to_string())
}

pub fn git_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = git_output_os(root, args.iter().map(OsString::from).collect(), None)?;
    require_success(&output, &format!("git {}", args.join(" ")))?;
    Ok(output.stdout)
}

fn git_output_os(root: &Path, args: Vec<OsString>, stdin: Option<&[u8]>) -> Result<Output, String> {
    let mut command = Command::new("git");
    command
        .arg("--no-pager")
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(["-c", "commit.gpgSign=false"])
        .args(["-c", "diff.external="])
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_EXTERNAL_DIFF")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("Failed to start git: {error}"))?;
    if let Some(bytes) = stdin {
        child
            .stdin
            .take()
            .ok_or_else(|| "Git stdin is unavailable".to_string())?
            .write_all(bytes)
            .map_err(|error| format!("Could not write git stdin: {error}"))?;
    }
    child
        .wait_with_output()
        .map_err(|error| format!("Could not wait for git: {error}"))
}

fn require_success(output: &Output, label: &str) -> Result<(), String> {
    if output.status.success() {
        Ok(())
    } else {
        let error = String::from_utf8_lossy(&output.stderr);
        Err(format!("{label} failed: {}", bounded(&error, 8_192)))
    }
}

fn changed_files(root: &Path, include_ignored: bool) -> Result<Vec<ChangedFile>, String> {
    let mut args = vec!["status", "--porcelain=v1", "-z", "--untracked-files=all"];
    if include_ignored {
        args.push("--ignored=matching");
    }
    let bytes = git_bytes(root, &args)?;
    let fields = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut output = Vec::new();
    let mut index = 0usize;
    while index < fields.len() {
        let field = fields[index];
        index += 1;
        if field.is_empty() {
            continue;
        }
        if field.len() < 4 || field[2] != b' ' {
            return Err("Git status returned malformed porcelain data".to_string());
        }
        let x = field[0] as char;
        let y = field[1] as char;
        let path = String::from_utf8(field[3..].to_vec())
            .map_err(|_| "A changed path is not valid UTF-8".to_string())?;
        let renamed = matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C');
        let old_path = if renamed && index < fields.len() {
            let old = String::from_utf8(fields[index].to_vec())
                .map_err(|_| "A renamed path is not valid UTF-8".to_string())?;
            index += 1;
            Some(old)
        } else {
            None
        };
        if path == MARKER_FILE || old_path.as_deref() == Some(MARKER_FILE) {
            continue;
        }
        output.push(ChangedFile {
            path,
            old_path,
            index_status: x.to_string(),
            worktree_status: y.to_string(),
            untracked: x == '?' && y == '?',
            ignored: x == '!' && y == '!',
        });
    }
    Ok(output)
}

fn validate_paths(paths: &[String]) -> Result<Vec<String>, String> {
    if paths.is_empty() || paths.len() > 1_024 {
        return Err("Select between 1 and 1024 paths".to_string());
    }
    let mut unique = std::collections::BTreeSet::new();
    for value in paths {
        if value.is_empty() || value.len() > 4_096 || value == MARKER_FILE || value.contains('\0') {
            return Err(format!("Invalid selected path '{value}'"));
        }
        let path = Path::new(value);
        if path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(format!("Selected path escapes the worktree: '{value}'"));
        }
        unique.insert(value.clone());
    }
    Ok(unique.into_iter().collect())
}

fn require_mutable(record: &OwnedWorktreeRecord) -> Result<(), String> {
    if !matches!(record.state.as_str(), "active" | "recovered") {
        return Err(format!(
            "Worktree is not mutable in '{}' state",
            record.state
        ));
    }
    Ok(())
}

fn canonical_repository(path: &Path) -> Result<PathBuf, String> {
    let root = git_text(path, &["rev-parse", "--show-toplevel"])?;
    let canonical = PathBuf::from(root)
        .canonicalize()
        .map_err(|error| format!("Could not canonicalize repository: {error}"))?;
    if canonical != path.canonicalize().map_err(|error| error.to_string())? {
        return Err("Repository root must be the exact Git top-level directory".to_string());
    }
    Ok(canonical)
}

fn canonical_git_path(root: &Path, value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    path.canonicalize()
        .map_err(|error| format!("Could not canonicalize Git path: {error}"))
}

fn validate_ref_name(repository: &Path, value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 512 || value.starts_with('-') || value.contains('\0') {
        return Err("Git ref is invalid".to_string());
    }
    let output = git_output_os(
        repository,
        vec![
            OsString::from("check-ref-format"),
            OsString::from("--branch"),
            OsString::from(value),
        ],
        None,
    )?;
    require_success(&output, "git check-ref-format")
}

fn safe_branch_label(value: &str) -> String {
    let output = value
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let output = output.trim_matches('-');
    if output.is_empty() {
        "task".to_string()
    } else {
        output.chars().take(48).collect()
    }
}

fn validate_oid(value: &str) -> Result<(), String> {
    if !(40..=64).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Err("Git returned an invalid object id".to_string())
    } else {
        Ok(())
    }
}

fn write_marker(marker: &OwnershipMarker) -> Result<(), String> {
    let path = Path::new(&marker.canonical_path).join(MARKER_FILE);
    atomic_write_json(&path, marker)
}

fn atomic_write_json(path: &Path, value: &impl serde::Serialize) -> Result<(), String> {
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4().simple()));
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("Could not create '{}': {error}", temporary.display()))?;
    file.write_all(&bytes)
        .map_err(|error| format!("Could not write '{}': {error}", temporary.display()))?;
    file.sync_all()
        .map_err(|error| format!("Could not sync '{}': {error}", temporary.display()))?;
    restrict_file(&temporary)?;
    fs::rename(&temporary, path)
        .map_err(|error| format!("Could not publish '{}': {error}", path.display()))?;
    restrict_file(path)
}

fn bounded_diff(bytes: Vec<u8>) -> super::DiffText {
    let truncated = bytes.len() > MAX_GIT_OUTPUT;
    let bytes = if truncated {
        &bytes[..MAX_GIT_OUTPUT]
    } else {
        &bytes
    };
    let mut text = String::from_utf8_lossy(bytes).to_string();
    while !text.is_char_boundary(text.len()) {
        text.pop();
    }
    if truncated {
        text.push_str("\n… diff truncated at 8 MiB …\n");
    }
    super::DiffText { text, truncated }
}

fn bounded(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.trim().to_string();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", value[..end].trim())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::process::Command;

    struct TempRepository {
        root: PathBuf,
        repository: PathBuf,
    }

    impl TempRepository {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "little-monkey-m5-git-{label}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            let repository = root.join("repository");
            fs::create_dir_all(&repository).unwrap();
            run_git(&repository, &["init", "-b", "main"]);
            run_git(&repository, &["config", "user.name", "Fixture"]);
            run_git(
                &repository,
                &["config", "user.email", "fixture@example.invalid"],
            );
            run_git(&repository, &["config", "commit.gpgSign", "false"]);
            fs::write(repository.join("README.md"), "fixture\n").unwrap();
            run_git(&repository, &["add", "README.md"]);
            run_git(&repository, &["commit", "-m", "fixture"]);
            run_git(
                &repository,
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://github.com/owner/repo.git",
                ],
            );
            Self { root, repository }
        }

        fn request(&self, label: &str) -> WorktreeCreateRequest {
            WorktreeCreateRequest {
                repository_root: self.repository.to_string_lossy().to_string(),
                repository_slug: "owner/repo".to_string(),
                base_ref: "main".to_string(),
                label: label.to_string(),
                allowed_remotes: vec!["origin".to_string()],
                branch_prefix: "codex/fixture/".to_string(),
                protected_branches: vec!["main".to_string(), "release".to_string()],
                allow_push: false,
                allow_create_pull_request: false,
                allow_review_comment: false,
                allow_fork_writes: false,
            }
        }
    }

    impl Drop for TempRepository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn run_git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn remote_parser_accepts_only_github_https_or_ssh() {
        assert_eq!(
            github_slug_from_remote("git@github.com:Owner/Repo.git").unwrap(),
            "owner/repo"
        );
        assert_eq!(
            github_slug_from_remote("https://github.com/owner/repo.git").unwrap(),
            "owner/repo"
        );
        assert!(github_slug_from_remote("https://token@github.com/owner/repo").is_err());
        assert!(github_slug_from_remote("ext::sh -c evil").is_err());
    }

    #[test]
    fn remote_identity_validates_distinct_push_urls_before_worktree_creation() {
        let fixture = TempRepository::new("push-url-identity");
        validate_remote_identity(&fixture.repository, "origin", "owner/repo").unwrap();

        run_git(
            &fixture.repository,
            &[
                "remote",
                "set-url",
                "--push",
                "origin",
                "https://github.com/attacker/other.git",
            ],
        );
        let error =
            validate_remote_identity(&fixture.repository, "origin", "owner/repo").unwrap_err();
        assert!(error.contains("push URL"));
        assert!(error.contains("attacker/other"));

        let mut store = DeliveryStore::open_in_memory(fixture.root.join("delivery")).unwrap();
        assert!(create_owned_worktree(&mut store, &fixture.request("redirected"), 1_000).is_err());
        assert_eq!(
            git_text(&fixture.repository, &["worktree", "list", "--porcelain"])
                .unwrap()
                .matches("worktree ")
                .count(),
            1
        );
    }

    #[test]
    fn selected_paths_reject_marker_absolute_parent_and_duplicates_collapse() {
        assert!(validate_paths(&[MARKER_FILE.to_string()]).is_err());
        assert!(validate_paths(&["../escape".to_string()]).is_err());
        assert!(validate_paths(&["/escape".to_string()]).is_err());
        assert_eq!(
            validate_paths(&["src/a.rs".to_string(), "src/a.rs".to_string()]).unwrap(),
            ["src/a.rs"]
        );
    }

    #[test]
    fn four_owned_jobs_are_isolated_and_selective_commit_preserves_other_staging() {
        let fixture = TempRepository::new("four-way");
        fs::write(
            fixture.repository.join("primary-dirty.txt"),
            "primary must remain dirty\n",
        )
        .unwrap();
        let before = git_text(&fixture.repository, &["status", "--porcelain=v1"]).unwrap();
        let mut store = DeliveryStore::open_in_memory(fixture.root.join("delivery")).unwrap();
        let mut records = Vec::new();
        for index in 0..4 {
            let record = create_owned_worktree(
                &mut store,
                &fixture.request(&format!("parallel-{index}")),
                1_000 + index,
            )
            .unwrap();
            let root = Path::new(&record.marker.canonical_path);
            fs::write(
                root.join(format!("job-{index}.txt")),
                format!("job {index}\n"),
            )
            .unwrap();
            if index == 0 {
                fs::write(root.join("keep-staged.txt"), "preserve index\n").unwrap();
                git_text(root, &["add", "keep-staged.txt"]).unwrap();
            }
            let (oid, _) = commit_paths(
                &store,
                &record.marker.worktree_id,
                &[format!("job-{index}.txt")],
                &format!("test: commit parallel job {index}"),
            )
            .unwrap();
            assert_ne!(oid, record.marker.base_oid);
            records.push(record);
        }
        assert_eq!(
            records
                .iter()
                .map(|record| &record.marker.branch)
                .collect::<BTreeSet<_>>()
                .len(),
            4
        );
        assert_eq!(
            records
                .iter()
                .map(|record| &record.marker.canonical_path)
                .collect::<BTreeSet<_>>()
                .len(),
            4
        );
        let first = inspect_owned_worktree(&store, &records[0]).unwrap();
        assert!(first
            .files
            .iter()
            .any(|file| { file.path == "keep-staged.txt" && file.index_status == "A" }));
        assert!(first.diffs.staged.text.contains("keep-staged.txt"));
        assert_eq!(
            git_text(&fixture.repository, &["status", "--porcelain=v1"]).unwrap(),
            before
        );
    }

    #[test]
    fn dirty_foreign_or_unarchived_worktrees_cannot_be_deleted() {
        let fixture = TempRepository::new("cleanup-guards");
        let mut store = DeliveryStore::open_in_memory(fixture.root.join("delivery")).unwrap();
        let record = create_owned_worktree(&mut store, &fixture.request("cleanup"), 1_000).unwrap();
        let root = Path::new(&record.marker.canonical_path);
        fs::write(root.join("dirty.txt"), "unsaved\n").unwrap();
        assert!(archive_owned_worktree(&mut store, &record.marker.worktree_id, 1_100).is_err());
        assert!(root.exists());
        assert!(cleanup_owned_worktree(&mut store, &record.marker.worktree_id, 1_200).is_err());
        assert!(root.exists());
        fs::remove_file(root.join("dirty.txt")).unwrap();

        let mut forged = record.clone();
        forged.marker.canonical_path = fixture.repository.to_string_lossy().to_string();
        assert!(validate_live(&store, &forged).is_err());
        assert!(fixture.repository.exists());

        let archived =
            archive_owned_worktree(&mut store, &record.marker.worktree_id, 1_300).unwrap();
        assert_eq!(archived.state, "archived");
        let cleaned =
            cleanup_owned_worktree(&mut store, &record.marker.worktree_id, 1_400).unwrap();
        assert_eq!(cleaned.state, "cleaned");
        assert!(!root.exists());
        assert!(fixture.repository.exists());
    }

    #[test]
    fn crash_created_marker_is_recovered_locked_before_mutation() {
        let fixture = TempRepository::new("recovery");
        let delivery_root = fixture.root.join("delivery");
        let record = {
            let mut first = DeliveryStore::open_in_memory(delivery_root.clone()).unwrap();
            create_owned_worktree(&mut first, &fixture.request("recovery"), 1_000).unwrap()
        };
        let mut reopened = DeliveryStore::open_in_memory(delivery_root).unwrap();
        let recovered = recover_owned_worktrees(&mut reopened, 2_000).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].marker.worktree_id, record.marker.worktree_id);
        assert_eq!(recovered[0].state, "recovered");
        assert!(recovered[0].locked);
        assert!(recovered[0]
            .lock_reason
            .as_deref()
            .unwrap()
            .contains("interrupted"));
    }
}
