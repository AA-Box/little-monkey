//! M5.4 owned-worktree, GitHub delivery, and local review boundary.
//!
//! The desktop never receives an arbitrary `git` or `gh` executor. Every
//! mutation is represented by a closed typed enum, previewed as an exact
//! SHA-256 digest, confirmed once within five minutes, and appended to the
//! local audit ledger. Repository identity, branch prefixes, remote names,
//! and write capabilities are frozen into an application-owned worktree
//! marker at creation time. There is deliberately no merge, rebase, branch
//! deletion, or force-push operation in this module.

mod git;
mod github;
mod reviewer;
mod store;

use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::AppState;
use store::DeliveryStore;

const PREVIEW_TTL_MS: u64 = 5 * 60 * 1_000;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliveryPolicy {
    pub allowed_remotes: Vec<String>,
    pub branch_prefix: String,
    pub protected_branches: Vec<String>,
    pub allow_push: bool,
    pub allow_create_pull_request: bool,
    pub allow_review_comment: bool,
    #[serde(default)]
    pub allow_fork_writes: bool,
}

impl DeliveryPolicy {
    fn validate(&self) -> Result<(), String> {
        validate_branch_prefix(&self.branch_prefix)?;
        if self.allowed_remotes.is_empty() || self.allowed_remotes.len() > 16 {
            return Err("Choose between 1 and 16 allowed remotes".to_string());
        }
        let mut seen = std::collections::BTreeSet::new();
        for remote in &self.allowed_remotes {
            validate_git_token("remote", remote)?;
            if !seen.insert(remote) {
                return Err(format!("Remote '{remote}' is listed more than once"));
            }
        }
        if self.protected_branches.len() > 128 {
            return Err("Protected branch list exceeds 128 entries".to_string());
        }
        for branch in &self.protected_branches {
            validate_git_token("protected branch", branch)?;
        }
        if (self.allow_create_pull_request || self.allow_review_comment) && !self.allow_push {
            return Err(
                "Pull-request/comment writes require owned-branch push permission".to_string(),
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorktreeCreateRequest {
    pub repository_root: String,
    pub repository_slug: String,
    pub base_ref: String,
    pub label: String,
    pub allowed_remotes: Vec<String>,
    pub branch_prefix: String,
    pub protected_branches: Vec<String>,
    pub allow_push: bool,
    pub allow_create_pull_request: bool,
    pub allow_review_comment: bool,
    #[serde(default)]
    pub allow_fork_writes: bool,
}

impl WorktreeCreateRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_repository_slug(&self.repository_slug)?;
        validate_git_token("base ref", &self.base_ref)?;
        validate_text("worktree label", &self.label, 1, 160, false)?;
        DeliveryPolicy {
            allowed_remotes: self.allowed_remotes.clone(),
            branch_prefix: self.branch_prefix.clone(),
            protected_branches: self.protected_branches.clone(),
            allow_push: self.allow_push,
            allow_create_pull_request: self.allow_create_pull_request,
            allow_review_comment: self.allow_review_comment,
            allow_fork_writes: self.allow_fork_writes,
        }
        .validate()?;
        let root = Path::new(&self.repository_root);
        if !root.is_absolute() || self.repository_root.contains('\0') {
            return Err("Repository root must be an absolute path".to_string());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OwnershipMarker {
    pub schema_version: u32,
    pub worktree_id: String,
    pub lease_nonce: String,
    pub repository_id: String,
    pub repository_slug: String,
    pub repository_root: String,
    pub common_git_dir: String,
    pub canonical_path: String,
    pub branch: String,
    pub base_oid: String,
    pub policy: DeliveryPolicy,
    pub created_at_ms: u64,
}

impl OwnershipMarker {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err("Unsupported owned-worktree marker schema".to_string());
        }
        validate_id("worktree id", &self.worktree_id)?;
        validate_id("lease nonce", &self.lease_nonce)?;
        validate_id("repository id", &self.repository_id)?;
        validate_repository_slug(&self.repository_slug)?;
        validate_git_token("branch", &self.branch)?;
        self.policy.validate()?;
        if !self.branch.starts_with(&self.policy.branch_prefix)
            || self
                .policy
                .protected_branches
                .iter()
                .any(|protected| protected == &self.branch)
        {
            return Err("Owned branch violates its frozen policy".to_string());
        }
        for value in [
            &self.repository_root,
            &self.common_git_dir,
            &self.canonical_path,
        ] {
            if !Path::new(value).is_absolute() || value.contains('\0') {
                return Err("Owned-worktree marker contains an invalid path".to_string());
            }
        }
        if !(40..=64).contains(&self.base_oid.len())
            || !self.base_oid.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("Owned-worktree marker contains an invalid base object".to_string());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OwnedWorktreeRecord {
    pub marker: OwnershipMarker,
    pub state: String,
    pub locked: bool,
    pub lock_reason: Option<String>,
    pub archive_path: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChangedFile {
    pub path: String,
    pub old_path: Option<String>,
    pub index_status: String,
    pub worktree_status: String,
    pub untracked: bool,
    pub ignored: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiffText {
    pub text: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiffBundle {
    pub staged: DiffText,
    pub unstaged: DiffText,
    pub head: DiffText,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorktreeInspection {
    pub worktree: OwnedWorktreeRecord,
    pub head_oid: String,
    pub ahead: u32,
    pub behind: u32,
    pub dirty: bool,
    pub cleanup_blocked: bool,
    pub files: Vec<ChangedFile>,
    pub diffs: DiffBundle,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewFinding {
    pub finding_id: String,
    pub severity: String,
    pub path: String,
    pub line: u32,
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewReport {
    pub report_id: String,
    pub repository_slug: String,
    pub pr_number: u32,
    pub head_oid: String,
    pub model: String,
    pub summary: String,
    pub findings: Vec<ReviewFinding>,
    pub report_digest: String,
    pub published_comment_id: Option<u64>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuditEntry {
    pub audit_id: u64,
    pub occurred_at_ms: u64,
    pub action: String,
    pub target: Option<String>,
    pub request_digest: String,
    pub outcome: String,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MutationExecutionRecord {
    pub request_digest: String,
    pub action: String,
    pub target: String,
    pub external: bool,
    pub state: String,
    pub executor_instance: String,
    pub confirmed_at_ms: u64,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub resolution: Option<String>,
    pub resolution_note: Option<String>,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "payload",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DeliveryMutation {
    CreateWorktree(WorktreeCreateRequest),
    SetLock {
        worktree_id: String,
        locked: bool,
        reason: Option<String>,
    },
    Stage {
        worktree_id: String,
        paths: Vec<String>,
    },
    Commit {
        worktree_id: String,
        paths: Vec<String>,
        message: String,
    },
    Push {
        worktree_id: String,
        remote: String,
    },
    ArchiveWorktree {
        worktree_id: String,
    },
    CleanupWorktree {
        worktree_id: String,
    },
    CreateDraftPr {
        worktree_id: String,
        base: String,
        title: String,
        body: String,
    },
    UpdateDraftPr {
        worktree_id: String,
        pr_number: u32,
        title: String,
        body: String,
    },
    PublishReview {
        worktree_id: String,
        report_id: String,
    },
    QueuePatchTask {
        worktree_id: String,
        pr_number: u32,
        comment_id: u64,
        model: String,
    },
    ResolveReconciliation {
        request_digest: String,
        resolution: String,
        note: String,
    },
}

impl DeliveryMutation {
    fn action(&self) -> &'static str {
        match self {
            Self::CreateWorktree(_) => "create_worktree",
            Self::SetLock { .. } => "set_lock",
            Self::Stage { .. } => "stage",
            Self::Commit { .. } => "commit",
            Self::Push { .. } => "push",
            Self::ArchiveWorktree { .. } => "archive_worktree",
            Self::CleanupWorktree { .. } => "cleanup_worktree",
            Self::CreateDraftPr { .. } => "create_draft_pr",
            Self::UpdateDraftPr { .. } => "update_draft_pr",
            Self::PublishReview { .. } => "publish_review",
            Self::QueuePatchTask { .. } => "queue_patch_task",
            Self::ResolveReconciliation { .. } => "resolve_reconciliation",
        }
    }

    fn worktree_id(&self) -> Option<&str> {
        match self {
            Self::CreateWorktree(_) | Self::ResolveReconciliation { .. } => None,
            Self::SetLock { worktree_id, .. }
            | Self::Stage { worktree_id, .. }
            | Self::Commit { worktree_id, .. }
            | Self::Push { worktree_id, .. }
            | Self::ArchiveWorktree { worktree_id }
            | Self::CleanupWorktree { worktree_id }
            | Self::CreateDraftPr { worktree_id, .. }
            | Self::UpdateDraftPr { worktree_id, .. }
            | Self::PublishReview { worktree_id, .. }
            | Self::QueuePatchTask { worktree_id, .. } => Some(worktree_id),
        }
    }

    fn is_external(&self) -> bool {
        matches!(
            self,
            Self::Push { .. }
                | Self::CreateDraftPr { .. }
                | Self::UpdateDraftPr { .. }
                | Self::PublishReview { .. }
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfirmationPreview {
    pub digest: String,
    pub action: String,
    pub summary: String,
    pub impact: String,
    pub repository_slug: String,
    pub branch: Option<String>,
    pub external: bool,
    pub expires_at_ms: u64,
    pub confirmation_phrase: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewPullRequestRequest {
    pub worktree_id: String,
    pub pr_number: u32,
    pub model: String,
}

fn open_store() -> Result<DeliveryStore, String> {
    let root = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve the application data directory".to_string())?;
    let mut store = DeliveryStore::open(&root)?;
    let now = now_ms()?;
    store.import_reconciliation_fallback(now)?;
    store.recover_interrupted_executions(executor_instance(), now)?;
    Ok(store)
}

fn executor_instance() -> &'static str {
    static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INSTANCE
        .get_or_init(|| {
            format!(
                "desktop-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            )
        })
        .as_str()
}

fn now_ms() -> Result<u64, String> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .map_err(|error| error.to_string())
}

fn exact_workspace_root(state: &AppState, requested: &str) -> Result<(), String> {
    let workspace = crate::workspace::primary_root_canon(state)?;
    let requested = Path::new(requested)
        .canonicalize()
        .map_err(|error| format!("Could not canonicalize requested repository: {error}"))?;
    if workspace != requested {
        return Err(
            "Owned worktrees can only be created from the current primary workspace".to_string(),
        );
    }
    Ok(())
}

fn mutation_bytes(mutation: &DeliveryMutation) -> Result<Vec<u8>, String> {
    serde_json::to_vec(mutation).map_err(|error| error.to_string())
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn confirmation_phrase(digest: &str) -> String {
    format!("CONFIRM {}", &digest[..12])
}

fn validate_mutation(
    store: &DeliveryStore,
    mutation: &DeliveryMutation,
    state: &AppState,
) -> Result<(String, Option<String>, String, String), String> {
    match mutation {
        DeliveryMutation::CreateWorktree(request) => {
            request.validate()?;
            exact_workspace_root(state, &request.repository_root)?;
            Ok((
                request.repository_slug.to_ascii_lowercase(),
                None,
                format!("Create an isolated worktree from {}", request.base_ref),
                "Creates a new app-owned Git branch and worktree; the primary worktree is unchanged."
                    .to_string(),
            ))
        }
        DeliveryMutation::ResolveReconciliation {
            request_digest,
            resolution,
            note,
        } => {
            validate_digest(request_digest)?;
            if !matches!(resolution.as_str(), "completed" | "not_applied") {
                return Err("Resolution must be 'completed' or 'not_applied'".to_string());
            }
            validate_text("reconciliation note", note, 1, 4_096, true)?;
            let execution = store
                .execution(request_digest)?
                .ok_or_else(|| "Unknown mutation execution".to_string())?;
            if execution.state != "needs_reconciliation" {
                return Err("Mutation execution is not awaiting reconciliation".to_string());
            }
            let (repository, branch) = execution
                .target
                .split_once(':')
                .map(|(repository, branch)| (repository.to_string(), Some(branch.to_string())))
                .unwrap_or_else(|| (execution.target.clone(), None));
            validate_repository_slug(&repository)?;
            Ok((
                repository,
                branch,
                format!("Resolve {} as {}", &request_digest[..12], resolution),
                "Records an operator-verified outcome without retrying the original mutation"
                    .to_string(),
            ))
        }
        _ => {
            let id = mutation
                .worktree_id()
                .ok_or_else(|| "Mutation is missing a worktree id".to_string())?;
            let record = git::require_worktree(store, id)?;
            git::validate_live(store, &record)?;
            let (summary, impact) = mutation_description(mutation)?;
            Ok((
                record.marker.repository_slug.clone(),
                Some(record.marker.branch.clone()),
                summary,
                impact,
            ))
        }
    }
}

fn mutation_description(mutation: &DeliveryMutation) -> Result<(String, String), String> {
    let pair = match mutation {
        DeliveryMutation::SetLock { locked, reason, .. } => {
            if *locked {
                if let Some(reason) = reason {
                    validate_text("lock reason", reason, 1, 512, false)?;
                }
                (
                    "Lock the owned worktree",
                    "Prevents archive and cleanup until unlocked",
                )
            } else {
                (
                    "Unlock the owned worktree",
                    "Allows later archive and safe cleanup",
                )
            }
        }
        DeliveryMutation::Stage { paths, .. } => {
            validate_selected_paths_shape(paths)?;
            (
                "Stage selected paths",
                "Changes the owned worktree Git index only",
            )
        }
        DeliveryMutation::Commit { paths, message, .. } => {
            validate_selected_paths_shape(paths)?;
            validate_text("commit message", message, 1, 10_000, true)?;
            (
                "Commit selected paths",
                "Creates a local commit on the owned branch only",
            )
        }
        DeliveryMutation::Push { remote, .. } => {
            validate_git_token("remote", remote)?;
            (
                "Push the owned branch",
                "Writes commits to the declared GitHub repository",
            )
        }
        DeliveryMutation::ArchiveWorktree { .. } => (
            "Archive the owned worktree",
            "Creates and verifies a local Git bundle; dirty worktrees are refused",
        ),
        DeliveryMutation::CleanupWorktree { .. } => (
            "Clean up the archived worktree",
            "Removes only a clean, unlocked, app-owned worktree after archival",
        ),
        DeliveryMutation::CreateDraftPr {
            base, title, body, ..
        } => {
            validate_git_token("base branch", base)?;
            validate_text("pull-request title", title, 1, 512, false)?;
            validate_text("pull-request body", body, 0, 64 * 1024, true)?;
            (
                "Create a draft pull request",
                "Creates a new draft PR on GitHub",
            )
        }
        DeliveryMutation::UpdateDraftPr {
            pr_number,
            title,
            body,
            ..
        } => {
            validate_number("pull request", *pr_number)?;
            validate_text("pull-request title", title, 1, 512, false)?;
            validate_text("pull-request body", body, 0, 64 * 1024, true)?;
            (
                "Update the draft pull request",
                "Changes title/body of the exact owned-branch draft PR",
            )
        }
        DeliveryMutation::PublishReview { report_id, .. } => {
            validate_id("review report id", report_id)?;
            (
                "Publish the local review report",
                "Creates or updates one deduplicated GitHub PR comment",
            )
        }
        DeliveryMutation::QueuePatchTask {
            pr_number,
            comment_id,
            model,
            ..
        } => {
            validate_number("pull request", *pr_number)?;
            if *comment_id == 0 {
                return Err("Review comment id must be positive".to_string());
            }
            validate_model(model)?;
            ("Queue an isolated patch task", "Reads the selected comment and creates a daemon-owned worktree task; it does not push")
        }
        DeliveryMutation::ResolveReconciliation { .. } => unreachable!(),
        DeliveryMutation::CreateWorktree(_) => unreachable!(),
    };
    Ok((pair.0.to_string(), pair.1.to_string()))
}

#[tauri::command]
pub fn m5_delivery_prepare_mutation(
    mutation: DeliveryMutation,
    state: tauri::State<'_, AppState>,
) -> Result<ConfirmationPreview, String> {
    prepare_mutation_impl(mutation, state.inner())
}

pub fn prepare_mutation_impl(
    mutation: DeliveryMutation,
    state: &AppState,
) -> Result<ConfirmationPreview, String> {
    let mut store = open_store()?;
    let (repository_slug, branch, summary, impact) = validate_mutation(&store, &mutation, state)?;
    let bytes = mutation_bytes(&mutation)?;
    let digest = digest_bytes(&bytes);
    let now = now_ms()?;
    let expires_at_ms = now
        .checked_add(PREVIEW_TTL_MS)
        .ok_or_else(|| "Confirmation expiry overflow".to_string())?;
    store.save_preview(&digest, mutation.action(), &bytes, now, expires_at_ms)?;
    Ok(ConfirmationPreview {
        confirmation_phrase: confirmation_phrase(&digest),
        digest,
        action: mutation.action().to_string(),
        summary,
        impact,
        repository_slug,
        branch,
        external: mutation.is_external(),
        expires_at_ms,
    })
}

#[tauri::command]
pub async fn m5_delivery_execute_mutation(
    mutation: DeliveryMutation,
    digest: String,
    confirmation: String,
    state: tauri::State<'_, AppState>,
) -> Result<Value, String> {
    execute_mutation_impl(mutation, digest, confirmation, state.inner()).await
}

pub async fn execute_mutation_impl(
    mutation: DeliveryMutation,
    digest: String,
    confirmation: String,
    state: &AppState,
) -> Result<Value, String> {
    validate_digest(&digest)?;
    if confirmation != confirmation_phrase(&digest) {
        return Err("Type the exact confirmation phrase shown in the preview".to_string());
    }
    let bytes = mutation_bytes(&mutation)?;
    if digest_bytes(&bytes) != digest {
        return Err("Confirmation digest does not match the exact mutation request".to_string());
    }
    let now = now_ms()?;
    let mut store = open_store()?;
    let (repository_slug, branch, _, _) = validate_mutation(&store, &mutation, state)?;
    let target = branch
        .as_deref()
        .map(|branch| format!("{repository_slug}:{branch}"))
        .unwrap_or_else(|| repository_slug.clone());
    let action = mutation.action().to_string();
    let external = mutation.is_external();
    // This transaction is the hard side-effect boundary: confirmation
    // consumption, immutable execution intent, and a pending audit must all
    // commit before dispatch is allowed to begin.
    store.confirm_and_begin_execution(
        &digest,
        &bytes,
        &action,
        &target,
        external,
        executor_instance(),
        now,
    )?;
    let result = execute_mutation(&mut store, mutation, now).await;
    let finished_at = now_ms().unwrap_or(now);
    match store.finish_execution(&digest, &result, external, finished_at) {
        Ok(execution) => match result {
            Ok(value) => Ok(value),
            Err(error) if execution.state == "needs_reconciliation" => Err(format!(
                "{error}. The external outcome is ambiguous; execution {} requires reconciliation and will not be retried automatically.",
                &digest[..12]
            )),
            Err(error) => Err(error),
        },
        Err(finalize_error) => {
            // The operation was already dispatched. Never report a plain
            // retryable failure: first force the durable state, then fsync an
            // independent fallback marker that the next command imports.
            let operation_detail = match &result {
                Ok(value) => format!(
                    "Mutation returned success but completion audit failed ({finalize_error}); result={}",
                    bounded(&serde_json::to_string(value).unwrap_or_default(), 4_096)
                ),
                Err(error) => format!(
                    "Mutation returned an error and completion audit failed ({finalize_error}); operation={}",
                    bounded(error, 4_096)
                ),
            };
            let forced = store.force_needs_reconciliation(&digest, &operation_detail, finished_at);
            let fallback = store.append_reconciliation_fallback(
                &digest,
                &action,
                &target,
                &operation_detail,
                finished_at,
            );
            Err(format!(
                "Mutation may have changed state, but durable completion failed. Reconciliation {} is required and automatic retry is blocked. ledger={} fallback={}",
                &digest[..12],
                forced
                    .err()
                    .unwrap_or_else(|| "marked".to_string()),
                fallback
                    .err()
                    .unwrap_or_else(|| "fsynced".to_string())
            ))
        }
    }
}

async fn execute_mutation(
    store: &mut DeliveryStore,
    mutation: DeliveryMutation,
    now: u64,
) -> Result<Value, String> {
    match mutation {
        DeliveryMutation::CreateWorktree(request) => {
            let record = git::create_owned_worktree(store, &request, now)?;
            serde_json::to_value(record).map_err(|error| error.to_string())
        }
        DeliveryMutation::SetLock {
            worktree_id,
            locked,
            reason,
        } => serde_json::to_value(git::set_lock(
            store,
            &worktree_id,
            locked,
            reason.as_deref(),
            now,
        )?)
        .map_err(|error| error.to_string()),
        DeliveryMutation::Stage { worktree_id, paths } => {
            serde_json::to_value(git::stage_paths(store, &worktree_id, &paths)?)
                .map_err(|error| error.to_string())
        }
        DeliveryMutation::Commit {
            worktree_id,
            paths,
            message,
        } => {
            let (oid, inspection) = git::commit_paths(store, &worktree_id, &paths, &message)?;
            Ok(json!({ "oid": oid, "inspection": inspection }))
        }
        DeliveryMutation::Push {
            worktree_id,
            remote,
        } => {
            let record = git::require_worktree(store, &worktree_id)?;
            github::require_repository_write_allowed(
                &record.marker.repository_slug,
                record.marker.policy.allow_fork_writes,
            )?;
            let detail = git::push_owned_branch(store, &worktree_id, &remote)?;
            Ok(json!({ "pushed": true, "detail": detail }))
        }
        DeliveryMutation::ArchiveWorktree { worktree_id } => {
            serde_json::to_value(git::archive_owned_worktree(store, &worktree_id, now)?)
                .map_err(|error| error.to_string())
        }
        DeliveryMutation::CleanupWorktree { worktree_id } => {
            serde_json::to_value(git::cleanup_owned_worktree(store, &worktree_id, now)?)
                .map_err(|error| error.to_string())
        }
        DeliveryMutation::CreateDraftPr {
            worktree_id,
            base,
            title,
            body,
        } => {
            let record = git::require_worktree(store, &worktree_id)?;
            require_capability(
                record.marker.policy.allow_create_pull_request,
                "draft pull-request creation",
            )?;
            git::validate_live(store, &record)?;
            github::create_draft_pr(&record, &base, &title, &body)
        }
        DeliveryMutation::UpdateDraftPr {
            worktree_id,
            pr_number,
            title,
            body,
        } => {
            let record = git::require_worktree(store, &worktree_id)?;
            require_capability(
                record.marker.policy.allow_create_pull_request,
                "draft pull-request update",
            )?;
            git::validate_live(store, &record)?;
            github::update_draft_pr(&record, pr_number, &title, &body)
        }
        DeliveryMutation::PublishReview {
            worktree_id,
            report_id,
        } => {
            let record = git::require_worktree(store, &worktree_id)?;
            require_capability(
                record.marker.policy.allow_review_comment,
                "review comment publication",
            )?;
            git::validate_live(store, &record)?;
            let report = store
                .review(&report_id)?
                .ok_or_else(|| format!("Unknown review report '{report_id}'"))?;
            if report.repository_slug != record.marker.repository_slug {
                return Err("Review report belongs to a different repository".to_string());
            }
            let comment_id = github::publish_review_report(&record, &report)?;
            store.mark_review_published(&report_id, comment_id, now)?;
            Ok(json!({ "reportId": report_id, "commentId": comment_id }))
        }
        DeliveryMutation::QueuePatchTask {
            worktree_id,
            pr_number,
            comment_id,
            model,
        } => {
            let record = git::require_worktree(store, &worktree_id)?;
            git::validate_live(store, &record)?;
            reviewer::queue_selected_comment_patch(
                store, &record, pr_number, comment_id, &model, now,
            )
            .await
        }
        DeliveryMutation::ResolveReconciliation {
            request_digest,
            resolution,
            note,
        } => serde_json::to_value(store.resolve_reconciliation(
            &request_digest,
            &resolution,
            &note,
            now,
        )?)
        .map_err(|error| error.to_string()),
    }
}

#[tauri::command]
pub fn m5_delivery_list_worktrees() -> Result<Vec<OwnedWorktreeRecord>, String> {
    let mut store = open_store()?;
    let _ = git::recover_owned_worktrees(&mut store, now_ms()?)?;
    store.worktrees()
}

#[tauri::command]
pub fn m5_delivery_inspect_worktree(worktree_id: String) -> Result<WorktreeInspection, String> {
    let store = open_store()?;
    let record = git::require_worktree(&store, &worktree_id)?;
    git::inspect_owned_worktree(&store, &record)
}

#[tauri::command]
pub fn m5_delivery_audit(limit: Option<u32>) -> Result<Vec<AuditEntry>, String> {
    open_store()?.audit_entries(limit.unwrap_or(100))
}

#[tauri::command]
pub fn m5_delivery_reconciliations() -> Result<Vec<MutationExecutionRecord>, String> {
    open_store()?.reconciliations()
}

#[tauri::command]
pub fn m5_github_auth_status() -> Result<github::GitHubAuthStatus, String> {
    github::auth_status()
}

/// Non-worktree-scoped `gh api <path>` bridge for other in-process Rust
/// callers — Knowledge Sync's `GitHubRepo` connector (`knowledge_service.rs`)
/// uses this for repo metadata/commit/compare/tree/blob reads. Deliberately
/// not a `#[tauri::command]`: nothing outside the Rust backend calls it
/// directly, unlike every other function in this file.
pub fn m5_github_api_get(path: &str) -> Result<Value, String> {
    github::gh_api_json(path)
}

#[tauri::command]
pub fn m5_github_issue(worktree_id: String, number: u32) -> Result<Value, String> {
    let store = open_store()?;
    let record = git::require_worktree(&store, &worktree_id)?;
    validate_number("issue", number)?;
    github::read_issue(&record.marker.repository_slug, number)
}

#[tauri::command]
pub fn m5_github_pull_request(worktree_id: String, number: u32) -> Result<Value, String> {
    let store = open_store()?;
    let record = git::require_worktree(&store, &worktree_id)?;
    validate_number("pull request", number)?;
    github::read_pull_request(&record.marker.repository_slug, number)
}

#[tauri::command]
pub fn m5_github_review_threads(worktree_id: String, number: u32) -> Result<Value, String> {
    let store = open_store()?;
    let record = git::require_worktree(&store, &worktree_id)?;
    validate_number("pull request", number)?;
    github::read_review_threads(&record.marker.repository_slug, number)
}

#[tauri::command]
pub fn m5_github_checks(worktree_id: String, number: u32) -> Result<Value, String> {
    let store = open_store()?;
    let record = git::require_worktree(&store, &worktree_id)?;
    validate_number("pull request", number)?;
    github::read_checks(&record.marker.repository_slug, number)
}

#[tauri::command]
pub async fn m5_review_pull_request(
    request: ReviewPullRequestRequest,
) -> Result<ReviewReport, String> {
    validate_number("pull request", request.pr_number)?;
    validate_model(&request.model)?;
    let mut store = open_store()?;
    let record = git::require_worktree(&store, &request.worktree_id)?;
    git::validate_live(&store, &record)?;
    let report = reviewer::review_pull_request(
        &record.marker.repository_slug,
        request.pr_number,
        &request.model,
        now_ms()?,
    )
    .await?;
    store.save_review(&report)?;
    Ok(report)
}

#[tauri::command]
pub fn m5_review_reports(worktree_id: String, pr_number: u32) -> Result<Vec<ReviewReport>, String> {
    validate_number("pull request", pr_number)?;
    let store = open_store()?;
    let record = git::require_worktree(&store, &worktree_id)?;
    store.reviews_for_pr(&record.marker.repository_slug, pr_number)
}

fn require_capability(allowed: bool, action: &str) -> Result<(), String> {
    if allowed {
        Ok(())
    } else {
        Err(format!(
            "The frozen worktree policy does not allow {action}"
        ))
    }
}

pub fn validate_repository_slug(value: &str) -> Result<(), String> {
    if value.len() > 200 || value.matches('/').count() != 1 {
        return Err("Repository must be exactly 'owner/name'".to_string());
    }
    let (owner, name) = value
        .split_once('/')
        .ok_or_else(|| "Repository must be exactly 'owner/name'".to_string())?;
    for part in [owner, name] {
        if part.is_empty()
            || part.starts_with('.')
            || part.ends_with('.')
            || !part.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            })
        {
            return Err("Repository contains unsupported characters".to_string());
        }
    }
    Ok(())
}

pub fn validate_git_token(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 512
        || value.starts_with('-')
        || value.contains("..")
        || value.contains(['\0', '\r', '\n'])
        || value.chars().any(char::is_control)
    {
        Err(format!("Invalid {label}"))
    } else {
        Ok(())
    }
}

pub fn validate_id(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 256
        || value.contains("..")
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        Err(format!("Invalid {label}"))
    } else {
        Ok(())
    }
}

pub fn validate_model(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 256
        || value.starts_with('-')
        || value.chars().any(char::is_control)
        || value.contains(['\0', '\r', '\n'])
    {
        Err("Invalid local reviewer model".to_string())
    } else {
        Ok(())
    }
}

fn validate_digest(value: &str) -> Result<(), String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Err("Confirmation digest is invalid".to_string())
    } else {
        Ok(())
    }
}

fn validate_branch_prefix(value: &str) -> Result<(), String> {
    validate_git_token("branch prefix", value)?;
    if !value.ends_with('/') || value.starts_with('/') || value.contains("//") {
        return Err("Branch prefix must be safe and end in '/'".to_string());
    }
    Ok(())
}

fn validate_number(label: &str, value: u32) -> Result<(), String> {
    if value == 0 {
        Err(format!("{label} number must be positive"))
    } else {
        Ok(())
    }
}

fn validate_text(
    label: &str,
    value: &str,
    min: usize,
    max: usize,
    multiline: bool,
) -> Result<(), String> {
    let length = value.chars().count();
    if length < min
        || length > max
        || value.contains('\0')
        || (!multiline && value.contains(['\r', '\n']))
    {
        Err(format!("Invalid {label}"))
    } else {
        Ok(())
    }
}

fn validate_selected_paths_shape(paths: &[String]) -> Result<(), String> {
    if paths.is_empty() || paths.len() > 1_024 {
        return Err("Select between 1 and 1024 paths".to_string());
    }
    for value in paths {
        let path = Path::new(value);
        if value.is_empty()
            || value.len() > 4_096
            || value.contains('\0')
            || path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(format!("Selected path escapes the worktree: '{value}'"));
        }
    }
    Ok(())
}

fn bounded(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> WorktreeCreateRequest {
        WorktreeCreateRequest {
            repository_root: "/tmp/repo".to_string(),
            repository_slug: "owner/repo".to_string(),
            base_ref: "main".to_string(),
            label: "fixture".to_string(),
            allowed_remotes: vec!["origin".to_string()],
            branch_prefix: "codex/".to_string(),
            protected_branches: vec!["main".to_string()],
            allow_push: true,
            allow_create_pull_request: true,
            allow_review_comment: true,
            allow_fork_writes: false,
        }
    }

    #[test]
    fn mutation_digest_is_exact_and_confirmation_is_bound_to_it() {
        let first = DeliveryMutation::CreateWorktree(request());
        let mut changed = request();
        changed.label = "other".to_string();
        let second = DeliveryMutation::CreateWorktree(changed);
        let first_digest = digest_bytes(&mutation_bytes(&first).unwrap());
        let second_digest = digest_bytes(&mutation_bytes(&second).unwrap());
        assert_ne!(first_digest, second_digest);
        assert_eq!(
            confirmation_phrase(&first_digest).len(),
            "CONFIRM ".len() + 12
        );
    }

    #[test]
    fn policies_have_no_merge_or_force_push_and_forks_default_read_only() {
        let value = serde_json::to_value(request()).unwrap();
        assert_eq!(value["allowForkWrites"], false);
        assert!(value.get("allowMerge").is_none());
        assert!(value.get("allowForcePush").is_none());
    }

    #[test]
    fn repository_and_branch_validators_reject_ambiguous_values() {
        assert!(validate_repository_slug("owner/repo").is_ok());
        assert!(validate_repository_slug("https://github.com/owner/repo").is_err());
        assert!(validate_repository_slug("owner/repo/extra").is_err());
        assert!(validate_branch_prefix("codex/review/").is_ok());
        assert!(validate_branch_prefix("main").is_err());
        assert!(validate_git_token("remote", "--upload-pack=evil").is_err());
    }
}
