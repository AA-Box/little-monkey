use std::path::{Path, PathBuf};
use std::process::Command;

use little_monkey_lib::run_protocol::RepositoryPolicy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::store::{restrict_file, DaemonPaths};

const MARKER_FILE: &str = ".little-monkey-owned-worktree.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OwnedWorktree {
    pub schema_version: u32,
    pub job_id: String,
    pub lease_id: String,
    pub repository_id: String,
    pub repository_root: String,
    pub common_git_dir: String,
    pub canonical_path: String,
    pub branch: String,
    pub base_oid: String,
    pub expected_head: String,
}

#[derive(Debug, Clone)]
pub struct WorktreeRequest {
    pub repository: PathBuf,
    pub branch_prefix: String,
    pub allowed_remote_names: Vec<String>,
    pub allow_commit: bool,
    pub allow_push: bool,
    pub allow_create_pull_request: bool,
    pub allow_review_comment: bool,
}

impl OwnedWorktree {
    pub fn create(
        paths: &DaemonPaths,
        job_id: &str,
        request: &WorktreeRequest,
    ) -> Result<Self, String> {
        validate_branch_prefix(&request.branch_prefix)?;
        if request.allowed_remote_names.len() > 32 {
            return Err("No more than 32 remotes may be allowed".to_string());
        }
        for remote in &request.allowed_remote_names {
            validate_git_token("remote", remote)?;
        }
        let repository_root = git(&request.repository, &["rev-parse", "--show-toplevel"])?;
        let repository_root = PathBuf::from(repository_root)
            .canonicalize()
            .map_err(|error| format!("Cannot canonicalize repository root: {error}"))?;
        let common = git(&repository_root, &["rev-parse", "--git-common-dir"])?;
        let common = {
            let path = PathBuf::from(common);
            let path = if path.is_absolute() {
                path
            } else {
                repository_root.join(path)
            };
            path.canonicalize()
                .map_err(|error| format!("Cannot canonicalize common git dir: {error}"))?
        };
        let base_oid = git(&repository_root, &["rev-parse", "HEAD"])?;
        validate_oid(&base_oid)?;
        let short = &sha256_hex(job_id.as_bytes())[..16];
        let branch = format!("{}daemon-{short}", request.branch_prefix);
        let target = paths.worktrees.join(job_id);
        if target.exists() {
            return Err(format!(
                "Owned worktree path '{}' already exists; recovery must inspect it",
                target.display()
            ));
        }
        let output = Command::new("git")
            .arg("-C")
            .arg(&repository_root)
            .args(["worktree", "add", "-b"])
            .arg(&branch)
            .arg(&target)
            .arg(&base_oid)
            .output()
            .map_err(|error| format!("Failed to start git worktree add: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let canonical_path = target
            .canonicalize()
            .map_err(|error| format!("Cannot canonicalize new worktree: {error}"))?;
        let expected_head = git(&canonical_path, &["rev-parse", "HEAD"])?;
        if expected_head != base_oid {
            return Err("New worktree HEAD does not match its requested base".to_string());
        }
        let repository_id = format!(
            "repo-{}",
            &sha256_hex(common.to_string_lossy().as_bytes())[..24]
        );
        let owned = Self {
            schema_version: 1,
            job_id: job_id.to_string(),
            lease_id: format!("worktree-{}", &sha256_hex(job_id.as_bytes())[..24]),
            repository_id,
            repository_root: repository_root.to_string_lossy().to_string(),
            common_git_dir: common.to_string_lossy().to_string(),
            canonical_path: canonical_path.to_string_lossy().to_string(),
            branch,
            base_oid,
            expected_head,
        };
        owned.write_marker()?;
        Ok(owned)
    }

    pub fn repository_policy(&self, request: &WorktreeRequest) -> RepositoryPolicy {
        RepositoryPolicy {
            root_id: "root-primary".to_string(),
            owned_worktree_required: true,
            allowed_remote_names: request.allowed_remote_names.clone(),
            allowed_branch_prefixes: vec![request.branch_prefix.clone()],
            allow_commit: request.allow_commit,
            allow_push: request.allow_push,
            allow_create_pull_request: request.allow_create_pull_request,
            allow_review_comment: request.allow_review_comment,
            allow_merge: false,
            allow_force_push: false,
        }
    }

    pub fn validate_live(
        &self,
        paths: &DaemonPaths,
        policy: &RepositoryPolicy,
    ) -> Result<(), String> {
        policy.validate().map_err(|error| error.to_string())?;
        if !policy.owned_worktree_required {
            return Err(
                "Daemon-owned worktree policy unexpectedly permits the primary worktree"
                    .to_string(),
            );
        }
        let root = paths
            .worktrees
            .canonicalize()
            .map_err(|error| format!("Cannot canonicalize owned worktree root: {error}"))?;
        let path = PathBuf::from(&self.canonical_path)
            .canonicalize()
            .map_err(|error| format!("Owned worktree is missing: {error}"))?;
        if !path.starts_with(&root) {
            return Err("Owned worktree escaped the daemon worktree root".to_string());
        }
        let marker: Self = serde_json::from_slice(
            &std::fs::read(path.join(MARKER_FILE))
                .map_err(|error| format!("Owned worktree marker is missing: {error}"))?,
        )
        .map_err(|error| format!("Owned worktree marker is invalid: {error}"))?;
        if marker != *self {
            return Err("Owned worktree marker does not match the durable lease".to_string());
        }
        let branch = git(&path, &["branch", "--show-current"])?;
        if branch != self.branch
            || !policy
                .allowed_branch_prefixes
                .iter()
                .any(|prefix| branch.starts_with(prefix))
        {
            return Err(format!(
                "Owned worktree branch '{branch}' is outside policy"
            ));
        }
        let common = git(&path, &["rev-parse", "--git-common-dir"])?;
        let common = {
            let candidate = PathBuf::from(common);
            if candidate.is_absolute() {
                candidate
            } else {
                path.join(candidate)
            }
        }
        .canonicalize()
        .map_err(|error| format!("Cannot resolve owned worktree git dir: {error}"))?;
        if common.to_string_lossy() != self.common_git_dir {
            return Err("Owned worktree now belongs to a different repository".to_string());
        }
        Ok(())
    }

    pub fn safe_cleanup(&self, paths: &DaemonPaths) -> Result<bool, String> {
        let path = PathBuf::from(&self.canonical_path);
        if !path.exists() {
            return Ok(true);
        }
        let root = paths
            .worktrees
            .canonicalize()
            .map_err(|error| format!("Cannot canonicalize worktree root: {error}"))?;
        let canonical = path
            .canonicalize()
            .map_err(|error| format!("Cannot canonicalize worktree: {error}"))?;
        if !canonical.starts_with(root) {
            return Err("Refusing to remove a worktree outside daemon-owned storage".to_string());
        }
        let marker: Self = serde_json::from_slice(
            &std::fs::read(canonical.join(MARKER_FILE))
                .map_err(|error| format!("Refusing cleanup without ownership marker: {error}"))?,
        )
        .map_err(|error| format!("Refusing cleanup with invalid marker: {error}"))?;
        if marker.job_id != self.job_id || marker.lease_id != self.lease_id {
            return Err("Refusing cleanup because ownership marker does not match".to_string());
        }
        let dirty = git(&canonical, &["status", "--porcelain"])?;
        if !dirty.is_empty() {
            return Ok(false);
        }
        let repository = Path::new(&self.repository_root);
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(["worktree", "remove"])
            .arg(&canonical)
            .output()
            .map_err(|error| format!("Failed to start git worktree remove: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "git worktree remove failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(true)
    }

    fn write_marker(&self) -> Result<(), String> {
        let path = Path::new(&self.canonical_path).join(MARKER_FILE);
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        std::fs::write(&path, bytes)
            .map_err(|error| format!("Failed to write owned worktree marker: {error}"))?;
        restrict_file(&path)
    }
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|error| format!("Failed to run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn validate_branch_prefix(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value.ends_with('/')
        || value.starts_with('/')
        || value.contains("..")
        || value.chars().any(|ch| {
            ch.is_control() || matches!(ch, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        })
    {
        Err("Owned branch prefix must be a safe git prefix ending in '/'".to_string())
    } else {
        Ok(())
    }
}

fn validate_git_token(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        Err(format!("Invalid git {label} '{value}'"))
    } else {
        Ok(())
    }
}

fn validate_oid(value: &str) -> Result<(), String> {
    if !(40..=64).contains(&value.len()) || !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        Err("git returned an invalid HEAD object id".to_string())
    } else {
        Ok(())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_prefix_rejects_git_ref_injection() {
        for value in ["", "/codex/", "codex", "codex/../evil/", "codex task/"] {
            assert!(validate_branch_prefix(value).is_err(), "{value}");
        }
        assert!(validate_branch_prefix("codex/daemon/").is_ok());
    }
}
