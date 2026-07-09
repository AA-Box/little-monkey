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
    let branch_name = String::from_utf8_lossy(&branch_output.stdout).trim().to_string();
    let branch = if branch_name.is_empty() {
        // Detached HEAD: `branch --show-current` prints nothing.
        let short_hash_output = run_git(&root, &["rev-parse", "--short", "HEAD"])?;
        let short_hash = String::from_utf8_lossy(&short_hash_output.stdout).trim().to_string();
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

    Path::new(git_dir).file_name().map(|n| n.to_string_lossy().to_string())
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
        return Err(String::from_utf8_lossy(&add_output.stderr).trim().to_string());
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
        return Err(String::from_utf8_lossy(&commit_output.stderr).trim().to_string());
    }

    let stdout = String::from_utf8_lossy(&commit_output.stdout);
    let summary_line = stdout
        .lines()
        .find(|line| line.trim_start().starts_with('['))
        .map(|line| line.trim().to_string());

    Ok(summary_line.unwrap_or_else(|| "Committed successfully".to_string()))
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
