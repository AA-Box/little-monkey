//! Git status/commit commands backing the workspace panel's git status bar.
//!
//! These are direct, human-initiated UI actions (a status row plus a
//! "Commit" button), not model-invoked agent tools — exactly like the
//! existing [`crate::workspace::set_primary_workspace_root`], they are
//! intentionally NOT routed through the permission system in
//! `permissions.rs`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::{workspace, AppState};

/// Snapshot of the open workspace's git status, for the workspace panel's
/// git status bar. Scoped to the primary workspace root only — secondary
/// attached folders don't get their own branch/worktree chip (see
/// `workspace.rs`).
#[derive(serde::Serialize)]
pub struct GitStatusPayload {
    pub is_repo: bool,
    pub branch: Option<String>,
    pub added: u32,
    pub deleted: u32,
    pub changed_files: u32,
    /// Whether the primary root is a linked `git worktree` checkout (as
    /// opposed to the main working tree of its repo, or not a repo at all).
    pub is_worktree: bool,
    /// The linked worktree's name, when `is_worktree` is true.
    pub worktree_name: Option<String>,
}

/// Resolve the currently-open primary workspace root.
fn workspace_root(state: &AppState) -> Result<PathBuf, String> {
    workspace::primary_root_canon(state)
}

/// Run `git` with `args` inside `root` (via `-C`, no shell involved), and
/// return the raw output.
fn run_git(root: &Path, args: &[&str]) -> Result<Output, String> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| format!("Failed to run git: {}", e))
}

/// Parse the one-line output of `git diff --shortstat`, e.g.
/// `" 3 files changed, 42 insertions(+), 7 deletions(-)"`. Any clause
/// (files/insertions/deletions) may be absent, and the whole string may be
/// empty if there are no changes at all — everything defaults to 0.
fn parse_shortstat(line: &str) -> (u32, u32, u32) {
    let mut changed_files = 0;
    let mut added = 0;
    let mut deleted = 0;

    for part in line.split(',') {
        let part = part.trim();
        let leading_number: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
        let Ok(n) = leading_number.parse::<u32>() else {
            continue;
        };

        if part.contains("file") {
            changed_files = n;
        } else if part.contains("insertion") {
            added = n;
        } else if part.contains("deletion") {
            deleted = n;
        }
    }

    (changed_files, added, deleted)
}

/// Count of untracked files and the total lines they'd add if committed
/// (i.e. what `git diff`'s insertion count for each would be if diffed
/// against `/dev/null`). Binary files count toward the file total but
/// contribute no lines, mirroring `git diff`'s "Bin ... differs" — no bogus
/// line count from decoding binary content as text.
fn untracked_line_counts(root: &Path) -> Result<(u32, u32), String> {
    let output = run_git(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    if !output.status.success() {
        return Ok((0, 0));
    }

    let mut files = 0u32;
    let mut added = 0u32;
    for raw_path in output.stdout.split(|&b| b == 0) {
        if raw_path.is_empty() {
            continue;
        }
        files += 1;

        let rel_path = String::from_utf8_lossy(raw_path);
        let Ok(bytes) = std::fs::read(root.join(rel_path.as_ref())) else {
            continue; // vanished between listing and reading — don't fail the whole status over it
        };
        if !is_binary(&bytes) {
            added += count_lines(&bytes);
        }
    }

    Ok((files, added))
}

/// Git's own heuristic for "binary": a NUL byte anywhere in the first 8000
/// bytes (see `buffer_is_binary` in git's source).
fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|&b| b == 0)
}

/// Line count a brand-new file would add: one per newline, plus a trailing
/// line if there's content after the last one.
fn count_lines(bytes: &[u8]) -> u32 {
    if bytes.is_empty() {
        return 0;
    }
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count() as u32;
    if bytes.last() == Some(&b'\n') {
        newlines
    } else {
        newlines + 1
    }
}

/// Report the open workspace's git status: whether it's a repo at all, its
/// current branch, and a shortstat-style change summary.
#[tauri::command]
pub fn git_status(state: tauri::State<'_, AppState>) -> Result<GitStatusPayload, String> {
    let root = workspace_root(state.inner())?;

    let not_a_repo = GitStatusPayload {
        is_repo: false,
        branch: None,
        added: 0,
        deleted: 0,
        changed_files: 0,
        is_worktree: false,
        worktree_name: None,
    };

    // Not being a repo (or `git` failing outright) is a normal, expected
    // state here, not an error — the caller just gets `is_repo: false`.
    let is_repo_output = run_git(&root, &["rev-parse", "--is-inside-work-tree"])?;
    if !is_repo_output.status.success() {
        return Ok(not_a_repo);
    }
    if String::from_utf8_lossy(&is_repo_output.stdout).trim() != "true" {
        return Ok(not_a_repo);
    }

    let branch_output = run_git(&root, &["branch", "--show-current"])?;
    let branch_name = String::from_utf8_lossy(&branch_output.stdout)
        .trim()
        .to_string();
    let branch = if branch_name.is_empty() {
        // Detached HEAD: `branch --show-current` prints nothing.
        let short_hash_output = run_git(&root, &["rev-parse", "--short", "HEAD"])?;
        let short_hash = String::from_utf8_lossy(&short_hash_output.stdout)
            .trim()
            .to_string();
        Some(format!("detached@{}", short_hash))
    } else {
        Some(branch_name)
    };

    // A brand-new repo with zero commits has no HEAD at all, so `git diff
    // HEAD` fails outright ("ambiguous argument 'HEAD'"). Fall back to git's
    // well-known empty-tree hash in that case, so a never-committed repo's
    // tracked/staged content still shows up as insertions (and the Commit
    // button correctly enables for the very first commit) instead of always
    // reading as "no changes".
    const EMPTY_TREE_HASH: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let head_exists = run_git(&root, &["rev-parse", "--verify", "-q", "HEAD"])?
        .status
        .success();
    let diff_target = if head_exists { "HEAD" } else { EMPTY_TREE_HASH };

    // `git diff --shortstat <target>` covers staged + unstaged changes to
    // tracked files relative to HEAD (or the empty tree if unborn), but never
    // sees untracked files — diffing a tree can't show content git doesn't
    // know about yet. Fold those in separately via `untracked_line_counts` so
    // a brand-new, never-committed project (all "??") still reports its size
    // instead of always reading as "no changes".
    let shortstat_output = run_git(&root, &["diff", "--shortstat", diff_target])?;
    let shortstat = String::from_utf8_lossy(&shortstat_output.stdout);
    let (tracked_files, tracked_added, deleted) = parse_shortstat(&shortstat);

    let (untracked_files, untracked_added) = untracked_line_counts(&root)?;
    let changed_files = tracked_files + untracked_files;
    let added = tracked_added + untracked_added;

    // A linked `git worktree` checkout has a `--git-dir` distinct from its
    // repo's `--git-common-dir` (the former points at
    // `<main-repo>/.git/worktrees/<name>`, the latter always at the shared
    // `<main-repo>/.git`); the main working tree's are the same path.
    let git_dir_output = run_git(&root, &["rev-parse", "--git-dir"])?;
    let common_dir_output = run_git(&root, &["rev-parse", "--git-common-dir"])?;
    let worktree_name = detect_worktree_name(
        String::from_utf8_lossy(&git_dir_output.stdout).trim(),
        String::from_utf8_lossy(&common_dir_output.stdout).trim(),
    );

    Ok(GitStatusPayload {
        is_repo: true,
        branch,
        added,
        deleted,
        changed_files,
        is_worktree: worktree_name.is_some(),
        worktree_name,
    })
}

/// Given `git rev-parse --git-dir` and `--git-common-dir` output for the
/// same repo, return the linked worktree's name if they differ (a linked
/// worktree's git-dir is always `.../.git/worktrees/<name>`), or `None` if
/// they're the same path (the main working tree).
fn detect_worktree_name(git_dir: &str, common_dir: &str) -> Option<String> {
    let git_dir = git_dir.trim_end_matches('/');
    let common_dir = common_dir.trim_end_matches('/');

    if git_dir == common_dir || git_dir.is_empty() {
        return None;
    }

    Path::new(git_dir)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
}

/// Stage everything and commit with `message`. Mirrors the default behavior
/// of a UI "Commit" button like VS Code's Source Control panel (stage all,
/// then commit) rather than requiring a separate staging step.
#[tauri::command]
pub fn git_commit(state: tauri::State<'_, AppState>, message: String) -> Result<String, String> {
    let root = workspace_root(state.inner())?;

    if message.trim().is_empty() {
        return Err("Commit message cannot be empty".to_string());
    }

    let add_output = run_git(&root, &["add", "-A"])?;
    if !add_output.status.success() {
        return Err(String::from_utf8_lossy(&add_output.stderr)
            .trim()
            .to_string());
    }

    // The message is passed as a single `Command::arg`, never
    // shell-concatenated, so special characters in it are not an injection
    // risk.
    let commit_output = Command::new("git")
        .arg("-C")
        .arg(&root)
        .arg("commit")
        .arg("-m")
        .arg(&message)
        .output()
        .map_err(|e| format!("Failed to run git: {}", e))?;

    if !commit_output.status.success() {
        return Err(String::from_utf8_lossy(&commit_output.stderr)
            .trim()
            .to_string());
    }

    let stdout = String::from_utf8_lossy(&commit_output.stdout);
    let summary_line = stdout
        .lines()
        .find(|line| line.trim_start().starts_with('['))
        .map(|line| line.trim().to_string());

    Ok(summary_line.unwrap_or_else(|| "Committed successfully".to_string()))
}

/// One changed file in a [`ReviewPayload`]: full before/after content so the
/// frontend can render unified or split diffs (and collapse unmodified
/// hunks) without a second round-trip per file.
#[derive(serde::Serialize)]
pub struct ReviewFilePayload {
    pub path: String,
    pub old_content: String,
    pub new_content: String,
    pub added: u32,
    pub deleted: u32,
    /// Binary or oversized files carry no content — the UI shows a stub row.
    pub binary: bool,
}

/// Snapshot backing the Review panel: every change between the review base
/// and the working tree, plus branch/target labels and a compare URL.
#[derive(serde::Serialize)]
pub struct ReviewPayload {
    pub is_repo: bool,
    pub branch: Option<String>,
    /// The upstream/default target this branch is compared against in
    /// "branch" mode (e.g. `origin/develop`), when one can be resolved.
    pub target: Option<String>,
    pub total_added: u32,
    pub total_deleted: u32,
    pub files: Vec<ReviewFilePayload>,
    /// A web URL for opening a compare/PR page for this branch, when the
    /// `origin` remote is a recognizable GitHub/GitLab-style HTTPS/SSH URL.
    pub pr_url: Option<String>,
}

/// Per-file content above this size is not shipped to the frontend — the
/// row still appears with its numstat counts, flagged like a binary.
const MAX_REVIEW_FILE_BYTES: usize = 1024 * 1024;
/// Hard cap on files in one review payload, keeping the IPC message sane on
/// pathological trees.
const MAX_REVIEW_FILES: usize = 300;

/// Resolves the branch's comparison target: its configured upstream if any,
/// otherwise the remote's default branch (`origin/HEAD`), otherwise `None`.
fn review_target(root: &Path) -> Option<String> {
    let upstream = run_git(root, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"]).ok()?;
    if upstream.status.success() {
        let name = String::from_utf8_lossy(&upstream.stdout).trim().to_string();
        if !name.is_empty() {
            return Some(name);
        }
    }
    let origin_head = run_git(root, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]).ok()?;
    if origin_head.status.success() {
        let name = String::from_utf8_lossy(&origin_head.stdout).trim().to_string();
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

/// Builds a GitHub-style compare URL from `origin`'s remote URL, tolerating
/// both HTTPS and `git@host:owner/repo.git` SSH forms. Returns `None` for
/// anything unrecognizable rather than guessing.
fn compare_url(remote: &str, target: &str, branch: &str) -> Option<String> {
    let remote = remote.trim();
    let https = if let Some(rest) = remote.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        format!("https://{host}/{path}")
    } else if remote.starts_with("https://") || remote.starts_with("http://") {
        remote.to_string()
    } else {
        return None;
    };
    let base = https.trim_end_matches('/').trim_end_matches(".git");
    // Strip the remote name off the target ("origin/develop" -> "develop").
    let target_branch = target.split_once('/').map_or(target, |(_, b)| b);
    Some(format!("{base}/compare/{target_branch}...{branch}?expand=1"))
}

/// Full-content review snapshot. `mode` is `"branch"` (merge-base of the
/// upstream target vs the working tree — the "what would this PR contain"
/// view) or `"working"` (HEAD vs the working tree — uncommitted changes
/// only). Like [`git_status`], a direct human-initiated UI read, not an
/// agent tool — no permission gate.
#[tauri::command]
pub fn git_review(state: tauri::State<'_, AppState>, mode: String) -> Result<ReviewPayload, String> {
    let root = workspace_root(state.inner())?;

    let empty = ReviewPayload {
        is_repo: false,
        branch: None,
        target: None,
        total_added: 0,
        total_deleted: 0,
        files: Vec::new(),
        pr_url: None,
    };

    let is_repo_output = run_git(&root, &["rev-parse", "--is-inside-work-tree"])?;
    if !is_repo_output.status.success()
        || String::from_utf8_lossy(&is_repo_output.stdout).trim() != "true"
    {
        return Ok(empty);
    }

    let branch_output = run_git(&root, &["branch", "--show-current"])?;
    let branch_name = String::from_utf8_lossy(&branch_output.stdout).trim().to_string();
    let branch = if branch_name.is_empty() { None } else { Some(branch_name.clone()) };

    let target = review_target(&root);

    const EMPTY_TREE_HASH: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let head_exists = run_git(&root, &["rev-parse", "--verify", "-q", "HEAD"])?
        .status
        .success();

    // The diff base: merge-base(target, HEAD) in branch mode (falling back
    // to HEAD when there's no target), HEAD in working mode, and the empty
    // tree for a repo with no commits yet in either mode.
    let base = if !head_exists {
        EMPTY_TREE_HASH.to_string()
    } else if mode == "branch" {
        match &target {
            Some(target_ref) => {
                let merge_base = run_git(&root, &["merge-base", target_ref, "HEAD"])?;
                if merge_base.status.success() {
                    String::from_utf8_lossy(&merge_base.stdout).trim().to_string()
                } else {
                    "HEAD".to_string()
                }
            }
            None => "HEAD".to_string(),
        }
    } else {
        "HEAD".to_string()
    };

    // Tracked changes (staged + unstaged) against the base, rename detection
    // off so every entry is a plain single-path add/modify/delete.
    let numstat = run_git(&root, &["diff", "--numstat", "--no-renames", "-z", &base])?;
    if !numstat.status.success() {
        return Err(String::from_utf8_lossy(&numstat.stderr).trim().to_string());
    }

    let mut files = Vec::new();
    let mut total_added = 0u32;
    let mut total_deleted = 0u32;

    // `--numstat -z` records: "added\tdeleted\tpath\0" (binary = "-\t-\t").
    for record in numstat.stdout.split(|&b| b == 0) {
        if record.is_empty() || files.len() >= MAX_REVIEW_FILES {
            continue;
        }
        let record = String::from_utf8_lossy(record);
        let mut parts = record.splitn(3, '\t');
        let (Some(added_raw), Some(deleted_raw), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let added = added_raw.parse::<u32>().unwrap_or(0);
        let deleted = deleted_raw.parse::<u32>().unwrap_or(0);
        let numstat_binary = added_raw == "-";
        total_added += added;
        total_deleted += deleted;

        let old_output = run_git(&root, &["show", &format!("{base}:{path}")])?;
        let old_bytes = if old_output.status.success() { old_output.stdout } else { Vec::new() };
        let new_bytes = std::fs::read(root.join(path)).unwrap_or_default();

        let binary = numstat_binary
            || is_binary(&old_bytes)
            || is_binary(&new_bytes)
            || old_bytes.len() > MAX_REVIEW_FILE_BYTES
            || new_bytes.len() > MAX_REVIEW_FILE_BYTES;

        files.push(ReviewFilePayload {
            path: path.to_string(),
            old_content: if binary { String::new() } else { String::from_utf8_lossy(&old_bytes).to_string() },
            new_content: if binary { String::new() } else { String::from_utf8_lossy(&new_bytes).to_string() },
            added,
            deleted,
            binary,
        });
    }

    // Untracked files: additions the tree diff can't see.
    let untracked = run_git(&root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    if untracked.status.success() {
        for raw_path in untracked.stdout.split(|&b| b == 0) {
            if raw_path.is_empty() || files.len() >= MAX_REVIEW_FILES {
                continue;
            }
            let path = String::from_utf8_lossy(raw_path).to_string();
            let bytes = std::fs::read(root.join(&path)).unwrap_or_default();
            let binary = is_binary(&bytes) || bytes.len() > MAX_REVIEW_FILE_BYTES;
            let added = if binary { 0 } else { count_lines(&bytes) };
            total_added += added;
            files.push(ReviewFilePayload {
                path,
                old_content: String::new(),
                new_content: if binary { String::new() } else { String::from_utf8_lossy(&bytes).to_string() },
                added,
                deleted: 0,
                binary,
            });
        }
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));

    let pr_url = match (&target, &branch) {
        (Some(target_ref), Some(branch_ref)) => {
            let remote = run_git(&root, &["remote", "get-url", "origin"])?;
            if remote.status.success() {
                compare_url(&String::from_utf8_lossy(&remote.stdout), target_ref, branch_ref)
            } else {
                None
            }
        }
        _ => None,
    };

    Ok(ReviewPayload {
        is_repo: true,
        branch,
        target,
        total_added,
        total_deleted,
        files,
        pr_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_shortstat_handles_full_line() {
        let (files, added, deleted) =
            parse_shortstat(" 3 files changed, 42 insertions(+), 7 deletions(-)");
        assert_eq!(files, 3);
        assert_eq!(added, 42);
        assert_eq!(deleted, 7);
    }

    #[test]
    fn parse_shortstat_handles_singular_file_and_insertion() {
        let (files, added, deleted) = parse_shortstat(" 1 file changed, 1 insertion(+)");
        assert_eq!(files, 1);
        assert_eq!(added, 1);
        assert_eq!(deleted, 0);
    }

    #[test]
    fn parse_shortstat_handles_deletions_only() {
        let (files, added, deleted) = parse_shortstat(" 2 files changed, 5 deletions(-)");
        assert_eq!(files, 2);
        assert_eq!(added, 0);
        assert_eq!(deleted, 5);
    }

    #[test]
    fn parse_shortstat_handles_empty_output() {
        let (files, added, deleted) = parse_shortstat("");
        assert_eq!(files, 0);
        assert_eq!(added, 0);
        assert_eq!(deleted, 0);
    }

    #[test]
    fn parse_shortstat_handles_trailing_newline() {
        let (files, added, deleted) =
            parse_shortstat(" 1 file changed, 2 insertions(+), 1 deletion(-)\n");
        assert_eq!(files, 1);
        assert_eq!(added, 2);
        assert_eq!(deleted, 1);
    }

    #[test]
    fn detect_worktree_name_none_when_dirs_match() {
        let name = detect_worktree_name("/repo/.git", "/repo/.git");
        assert_eq!(name, None);
    }

    #[test]
    fn detect_worktree_name_extracts_name_when_dirs_differ() {
        let name = detect_worktree_name("/repo/.git/worktrees/feature-x", "/repo/.git");
        assert_eq!(name, Some("feature-x".to_string()));
    }

    #[test]
    fn detect_worktree_name_ignores_trailing_slash() {
        let name = detect_worktree_name("/repo/.git/worktrees/feature-x/", "/repo/.git/");
        assert_eq!(name, Some("feature-x".to_string()));
    }

    #[test]
    fn detect_worktree_name_none_when_git_dir_empty() {
        let name = detect_worktree_name("", "/repo/.git");
        assert_eq!(name, None);
    }
}
