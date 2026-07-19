//! Sandboxed execution environments.
//!
//! Runs a caller-supplied shell command inside a disposable copy of the
//! primary workspace instead of the real one: the copy lives under
//! `<app_data>/sandbox-runs/<run_id>/workspace`, the spawned process only
//! ever sees that directory as its cwd, and it never receives the parent
//! process's environment — only `PATH`/`HOME` plus whatever extra variable
//! names the caller explicitly approved. On macOS the command additionally
//! runs under a generated Seatbelt (`sandbox-exec`) profile that denies
//! network access and denies filesystem writes outside the ephemeral copy;
//! every other platform gets the restricted-cwd/env isolation only. Every
//! run reports which of the two actually applied (see [`Isolation`]) —
//! never more than what was really enforced.
//!
//! Nothing the sandboxed command writes ever reaches the real workspace
//! automatically. Copying files back out is a separate, explicit two-phase
//! action mirroring `m5_delivery`'s prepare-digest/confirm-phrase pattern:
//! [`build_promote_preview`] (exposed as `sandbox_prepare_promote`) hashes
//! the exact files the caller selected and returns a digest plus a
//! `CONFIRM <digest prefix>` phrase; [`sandbox_execute_promote`] refuses to
//! touch the real workspace unless the exact digest and phrase are replayed
//! back, then re-hashes the sandbox copy to confirm nothing changed since
//! the preview was built.
//!
//! Every run is modeled as an ordinary [`crate::run_protocol::RunSpec`] of
//! [`RunKind::Sandboxed`] and recorded through the existing
//! [`crate::run_ledger::RunLedger`] — `Queued`/`Started` on launch,
//! `CheckpointLinked` once the ephemeral copy exists, `ArtifactAdded` for
//! captured stdout/stderr, `VerificationFinished` for the exit outcome, and
//! (only once a promote is confirmed) `ExternalMutationPrepared` /
//! `ExternalMutationConfirmed` / `Completed`. The run intentionally stays
//! non-terminal after execution — the whole point is that the workspace
//! stays untouched until a human decides to promote or discard.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::Manager;

use crate::run_protocol::{
    ArtifactKind, CapabilityAssessment, CapabilityState, CheckpointKind, ClientIdentity,
    ModelCapabilitiesSnapshot, ModelTargetSnapshot, MutationKind, PermissionMode,
    PermissionPolicySnapshot, RootAccess, RootGrant, RunBudgets, RunEvent, RunKind, RunSpec,
    ToolPolicyDecision, UsageSnapshot, WorkspaceContext, RUN_PROTOCOL_SCHEMA_VERSION,
};
use crate::{permissions, workspace, AppState};

const SANDBOX_RUNS_DIR: &str = "sandbox-runs";
const MAX_COMMAND_BYTES: usize = 16 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 5 * 60 * 1_000;
const MAX_TIMEOUT_MS: u64 = 30 * 60 * 1_000;
const MIN_TIMEOUT_MS: u64 = 1_000;
const MAX_APPROVED_ENV_KEYS: usize = 16;
const MAX_ARTIFACT_BYTES_BUDGET: u64 = 128 * 1024 * 1024;
const MAX_EVENT_TEXT_EXCERPT: usize = 4_096;
const PROMOTE_PREVIEW_TTL_MS: u64 = 5 * 60 * 1_000;
const MAX_PROMOTE_FILES: usize = 500;

/// Directory/build-artifact names that are never worth copying into the
/// ephemeral sandbox: they are large, regenerable, and (for `.git`)
/// irrelevant to "run this command against these files". Comparison is
/// case-insensitive, matching `permissions::path_risk_floor`'s reasoning
/// for the same platforms.
const SKIP_DIR_NAMES: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".nuxt",
    "out",
    ".venv",
    "venv",
    "__pycache__",
    ".turbo",
    ".cache",
];

/// Base env vars every sandboxed process needs to function as a normal
/// process on its platform (locate binaries, find a home directory). Never
/// includes anything else unless the caller explicitly names it in
/// `approved_env` — see [`allowlisted_env`]'s module-level reasoning.
#[cfg(not(target_os = "windows"))]
const BASE_ENV_KEYS: &[&str] = &["PATH", "HOME"];
#[cfg(target_os = "windows")]
const BASE_ENV_KEYS: &[&str] = &["PATH", "USERPROFILE", "SystemRoot", "TEMP", "TMP"];

/// Per-process, in-memory registry of prepared-but-not-yet-confirmed promote
/// previews, keyed by digest. Unlike `m5_delivery`'s durable SQLite preview
/// store, this is deliberately not persisted: the ephemeral sandbox copy a
/// preview points at lives only under this process's app-data directory for
/// this run, so a restart already leaves nothing meaningful to promote.
#[derive(Default)]
pub struct SandboxState {
    previews: std::sync::Mutex<HashMap<String, PendingPromote>>,
}

#[derive(Debug, Clone)]
struct PendingPromote {
    run_id: String,
    files: Vec<PromoteFileEntry>,
    expires_at_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// The command ran under a generated macOS Seatbelt profile
    /// (`sandbox-exec`) in addition to the restricted cwd/env.
    OsSandboxed,
    /// Only the restricted cwd + allowlisted env applied — no OS-level
    /// sandbox exists for this platform.
    ProcessOnly,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CopyStats {
    pub files_copied: u64,
    pub bytes_copied: u64,
    pub skipped: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromoteFileEntry {
    pub path: String,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxPromotePreview {
    pub run_id: String,
    pub digest: String,
    pub confirmation_phrase: String,
    pub files: Vec<PromoteFileEntry>,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxPromoteResult {
    pub run_id: String,
    pub promoted_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxDiffEntry {
    pub path: String,
    /// `"added"` (present in the sandbox copy only) or `"modified"`
    /// (present in both, different content). Unchanged files are omitted.
    /// Deletions are never represented here and promote never deletes real
    /// files — this feature only ever copies forward.
    pub status: String,
    pub sandbox_sha256: String,
    pub workspace_sha256: Option<String>,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxRunSummary {
    pub run_id: String,
    pub isolation: Isolation,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub passed: bool,
    pub duration_ms: u64,
    pub stdout_artifact_id: String,
    pub stderr_artifact_id: String,
    pub stdout_excerpt: String,
    pub stderr_excerpt: String,
    pub files_copied: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxRunListEntry {
    pub run_id: String,
    pub status: crate::run_protocol::RunStatus,
    pub task: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone)]
struct SandboxRunRequest {
    command: String,
    timeout_ms: Option<u64>,
    allow_network: bool,
    approved_env: Vec<String>,
}

impl SandboxRunRequest {
    fn validate(&self) -> Result<(), String> {
        if self.command.trim().is_empty() {
            return Err("Enter a command to run in the sandbox".to_string());
        }
        if self.command.len() > MAX_COMMAND_BYTES {
            return Err(format!(
                "Command exceeds the {MAX_COMMAND_BYTES}-byte limit"
            ));
        }
        if self.command.contains('\0') {
            return Err("Command must not contain NUL bytes".to_string());
        }
        if self.approved_env.len() > MAX_APPROVED_ENV_KEYS {
            return Err(format!(
                "At most {MAX_APPROVED_ENV_KEYS} approved environment variables are allowed"
            ));
        }
        for key in &self.approved_env {
            let valid = !key.is_empty()
                && key.len() <= 128
                && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && !key.as_bytes()[0].is_ascii_digit();
            if !valid {
                return Err(format!("Invalid environment variable name: '{key}'"));
            }
        }
        Ok(())
    }

    fn timeout(&self) -> Duration {
        let ms = self
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);
        Duration::from_millis(ms)
    }
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

fn sha256_hex_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn hash_file(path: &Path) -> io::Result<(String, u64)> {
    let bytes = fs::read(path)?;
    let size = bytes.len() as u64;
    Ok((sha256_hex_bytes(&bytes), size))
}

fn confirmation_phrase_for(digest: &str) -> String {
    format!("CONFIRM {}", &digest[..12])
}

/// True for directory names that are never worth copying wholesale into the
/// ephemeral sandbox (see [`SKIP_DIR_NAMES`]).
fn is_skippable_dir_name(name: &str) -> bool {
    SKIP_DIR_NAMES.iter().any(|skip| skip.eq_ignore_ascii_case(name))
}

/// True for files whose content is secret-shaped (currently: `.env*`, via
/// `permissions::path_risk_floor`). Only the secrets category is excluded
/// here — script-executing manifests/lockfiles (`package.json`,
/// `Cargo.toml`, ...) and shell rc files are also flagged by that function
/// for *edit*-risk purposes, but excluding them from the sandbox copy would
/// make it impossible to actually build or test the copy, defeating the
/// point of this feature. Secrets are excluded unconditionally: the
/// sandboxed process already never inherits them via its environment (see
/// [`allowlisted_env`]), and a copied `.env` file on disk would silently
/// undo that protection for any command that reads it directly.
fn secret_shaped(path: &Path, root: &Path) -> bool {
    matches!(
        permissions::path_risk_floor(path, root),
        Some(reason) if reason.starts_with("environment/secrets file")
    )
}

/// Copies `root`'s files into `dest`, skipping [`SKIP_DIR_NAMES`]
/// directories, secret-shaped files (see [`secret_shaped`]), and symlinks
/// (never followed, so a symlink pointing outside `root` can never smuggle
/// unrelated files into the copy).
pub fn copy_workspace_into_sandbox(root: &Path, dest: &Path) -> io::Result<CopyStats> {
    fs::create_dir_all(dest)?;
    let mut stats = CopyStats::default();

    let walker = walkdir::WalkDir::new(root)
        .min_depth(1)
        .into_iter()
        .filter_entry(|entry| {
            !(entry.file_type().is_dir()
                && is_skippable_dir_name(&entry.file_name().to_string_lossy()))
        });

    for entry in walker {
        let entry = entry.map_err(io::Error::other)?;
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(path);

        if entry.file_type().is_dir() {
            fs::create_dir_all(dest.join(rel))?;
            continue;
        }
        if !entry.file_type().is_file() {
            // Symlinks and other special files are never copied.
            stats.skipped += 1;
            continue;
        }
        if secret_shaped(path, root) {
            stats.skipped += 1;
            continue;
        }

        let dest_path = dest.join(rel);
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = fs::copy(path, &dest_path)?;
        stats.files_copied += 1;
        stats.bytes_copied += bytes;
    }

    Ok(stats)
}

/// Builds an env list containing only `PATH`/`HOME` (platform base keys)
/// plus whatever extra variable names the caller explicitly approved —
/// never a blanket inheritance of the parent process's environment. Keys not
/// present in the current process's own environment are silently omitted
/// rather than passed through empty.
pub fn allowlisted_env(approved_extra: &[String]) -> Vec<(String, String)> {
    let mut keys: Vec<String> = BASE_ENV_KEYS.iter().map(|k| k.to_string()).collect();
    for extra in approved_extra {
        if !keys.iter().any(|k| k == extra) {
            keys.push(extra.clone());
        }
    }
    keys.into_iter()
        .filter_map(|key| std::env::var(&key).ok().map(|value| (key, value)))
        .collect()
}

/// Pure string builder for the macOS Seatbelt profile: deny-by-default,
/// allow reading anything (so build tools can see their toolchain and
/// dependencies), allow writes only under `sandbox_dir`, and deny network
/// unless `allow_network` was explicitly requested. Intentionally
/// conservative/best-effort — this is real OS-level containment, not a
/// substitute for not running malicious code at all. Contains no I/O and is
/// testable without ever invoking `sandbox-exec`.
pub fn build_seatbelt_profile(sandbox_dir: &Path, allow_network: bool) -> String {
    let sandbox_dir = sandbox_dir.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    let network_clause = if allow_network {
        "(allow network*)"
    } else {
        "(deny network*)"
    };
    format!(
        "(version 1)\n\
         (deny default)\n\
         (allow process-fork)\n\
         (allow process-exec)\n\
         (allow file-read*)\n\
         (allow file-write* (subpath \"{sandbox_dir}\"))\n\
         (allow file-ioctl (subpath \"{sandbox_dir}\"))\n\
         (allow sysctl-read)\n\
         (allow mach-lookup)\n\
         (allow signal (target self))\n\
         {network_clause}\n"
    )
}

pub struct SandboxExecOutcome {
    pub isolation: Isolation,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub duration_ms: u64,
}

/// Spawns `shell_command` with `cwd` set to `sandbox_dir`, an allowlisted
/// env (see [`allowlisted_env`]), and a wall-clock `timeout`. On macOS the
/// command is additionally wrapped in `sandbox-exec` with a generated
/// Seatbelt profile written to `profile_path` (a sibling of `sandbox_dir`,
/// never inside it, so it never shows up as an unexpected file when diffing
/// the copy against the real workspace). On timeout the child is killed and
/// any output captured so far is discarded (matching `tools::tool_run_shell`'s
/// existing timeout behavior) — `timed_out` is still reported accurately.
#[allow(unused_variables)]
pub async fn execute_in_sandbox(
    sandbox_dir: &Path,
    profile_path: &Path,
    shell_command: &str,
    timeout: Duration,
    allow_network: bool,
    approved_env: &[String],
) -> io::Result<SandboxExecOutcome> {
    let env = allowlisted_env(approved_env);
    let started = std::time::Instant::now();

    #[cfg(target_os = "macos")]
    let (program, args, isolation) = {
        let profile = build_seatbelt_profile(sandbox_dir, allow_network);
        fs::write(profile_path, profile)?;
        (
            "sandbox-exec".to_string(),
            vec![
                "-f".to_string(),
                profile_path.to_string_lossy().to_string(),
                "--".to_string(),
                "sh".to_string(),
                "-c".to_string(),
                shell_command.to_string(),
            ],
            Isolation::OsSandboxed,
        )
    };

    #[cfg(target_os = "windows")]
    let (program, args, isolation) = (
        "cmd".to_string(),
        vec!["/C".to_string(), shell_command.to_string()],
        Isolation::ProcessOnly,
    );

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let (program, args, isolation) = (
        "sh".to_string(),
        vec!["-c".to_string(), shell_command.to_string()],
        Isolation::ProcessOnly,
    );

    let mut command = tokio::process::Command::new(&program);
    command
        .args(&args)
        .current_dir(sandbox_dir)
        .env_clear()
        .envs(env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = command.spawn()?;
    let result = tokio::time::timeout(timeout, child.wait_with_output()).await;
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    match result {
        Ok(Ok(output)) => Ok(SandboxExecOutcome {
            isolation,
            exit_code: output.status.code(),
            timed_out: false,
            stdout: output.stdout,
            stderr: output.stderr,
            duration_ms,
        }),
        Ok(Err(error)) => Err(error),
        Err(_) => Ok(SandboxExecOutcome {
            isolation,
            exit_code: None,
            timed_out: true,
            stdout: Vec::new(),
            stderr: Vec::new(),
            duration_ms,
        }),
    }
}

fn sandbox_target_snapshot() -> ModelTargetSnapshot {
    let evidence = "Sandboxed runs execute a shell command; no model inference occurs.".to_string();
    let unsupported = || CapabilityAssessment {
        state: CapabilityState::Unsupported,
        evidence: evidence.clone(),
    };
    ModelTargetSnapshot::Ollama {
        target_id: "sandbox-shell".to_string(),
        label: "Sandboxed shell execution".to_string(),
        base_url: "http://127.0.0.1:0".to_string(),
        model: "none".to_string(),
        is_cloud: false,
        capabilities: ModelCapabilitiesSnapshot {
            tool_calling: unsupported(),
            vision: unsupported(),
            embeddings: unsupported(),
            structured_output: unsupported(),
            image_generation: unsupported(),
            audio: unsupported(),
            runtime_lifecycle: unsupported(),
            fim: unsupported(),
            code_completion: unsupported(),
            inline_edit: unsupported(),
            fim_metadata: None,
        },
        estimated_memory_bytes: None,
    }
}

fn build_sandbox_run_spec(
    run_id: &str,
    submitted_by: ClientIdentity,
    root: &Path,
    request: &SandboxRunRequest,
    created_at_ms: u64,
) -> Result<RunSpec, String> {
    let spec = RunSpec {
        schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
        run_id: run_id.to_string(),
        idempotency_key: format!("sandbox/{run_id}"),
        created_at_ms,
        kind: RunKind::Sandboxed,
        submitted_by,
        task: format!("Sandboxed shell command:\n{}", request.command),
        instructions: None,
        input_artifact_ids: Vec::new(),
        target: sandbox_target_snapshot(),
        workspace: Some(WorkspaceContext {
            workspace_id: "sandbox".to_string(),
            primary_root_id: "root-primary".to_string(),
            roots: vec![RootGrant {
                root_id: "root-primary".to_string(),
                canonical_path: root.to_string_lossy().to_string(),
                access: RootAccess::ReadOnly,
                allow_symlinks_within_root: false,
            }],
            repository_policy: None,
        }),
        permission_policy: PermissionPolicySnapshot {
            mode: PermissionMode::Auto,
            unattended: true,
            approval_timeout_ms: 60_000,
            default_tool_decision: ToolPolicyDecision::Allow,
            tool_rules: Vec::new(),
            allow_network: request.allow_network,
            allow_external_mutations: false,
        },
        budgets: RunBudgets {
            wall_time_ms: request.timeout().as_millis() as u64,
            max_iterations: 1,
            max_model_calls: 1,
            max_tool_calls: 1,
            max_input_tokens: 1,
            max_output_tokens: 1,
            max_cost_micros: None,
            max_artifact_bytes: MAX_ARTIFACT_BYTES_BUDGET,
            max_event_count: 64,
        },
    };
    spec.validate().map_err(|error| error.to_string())?;
    Ok(spec)
}

fn sandbox_run_dir(app: &tauri::AppHandle, run_id: &str) -> Result<PathBuf, String> {
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Failed to resolve app data dir: {error}"))?;
    Ok(data_dir.join(SANDBOX_RUNS_DIR).join(run_id))
}

fn require_sandboxed_run(
    app: &tauri::AppHandle,
    state: &AppState,
    run_id: &str,
) -> Result<crate::run_ledger::StoredRun, String> {
    let run = crate::run_commands::with_ledger(app, state, |ledger| {
        ledger
            .load_run(run_id)?
            .ok_or_else(|| crate::run_ledger::LedgerError::NotFound {
                entity: "run",
                id: run_id.to_string(),
            })
    })?;
    if run.spec.kind != RunKind::Sandboxed {
        return Err("Run is not a sandboxed execution".to_string());
    }
    Ok(run)
}

fn expect_matching_root(run: &crate::run_ledger::StoredRun, current_root: &Path) -> Result<(), String> {
    let recorded = run
        .spec
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.roots.first())
        .map(|root| root.canonical_path.as_str())
        .ok_or_else(|| "Sandboxed run has no recorded workspace root".to_string())?;
    if recorded != current_root.to_string_lossy() {
        return Err(
            "The primary workspace has changed since this sandbox run started".to_string(),
        );
    }
    Ok(())
}

/// Rejects absolute paths, empty paths, and any `..`/root component — the
/// only components a promote path may contain are ordinary path segments.
fn validate_relative_promote_path(candidate: &str) -> Result<PathBuf, String> {
    if candidate.is_empty() || candidate.len() > 4_096 || candidate.contains('\0') {
        return Err(format!("Invalid file path: '{candidate}'"));
    }
    let path = Path::new(candidate);
    if path.is_absolute() {
        return Err(format!("File path must be relative: '{candidate}'"));
    }
    for component in path.components() {
        match component {
            std::path::Component::Normal(_) => {}
            _ => {
                return Err(format!(
                    "File path must not contain '..' or a root component: '{candidate}'"
                ))
            }
        }
    }
    Ok(path.to_path_buf())
}

fn compute_promote_digest(run_id: &str, files: &[PromoteFileEntry]) -> String {
    let mut sorted = files.to_vec();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let mut buffer = format!("run:{run_id}\n");
    for file in &sorted {
        buffer.push_str(&format!("{}:{}:{}\n", file.path, file.sha256, file.size_bytes));
    }
    sha256_hex_bytes(buffer.as_bytes())
}

/// Validates and hashes the requested files (as they currently exist in the
/// sandbox copy) and builds the exact preview a caller must replay back
/// (digest + confirmation phrase) to promote them. Pure filesystem read —
/// never touches the real workspace and never mutates any shared state.
pub fn build_promote_preview(
    run_id: &str,
    sandbox_workspace_dir: &Path,
    files: &[String],
    now_ms: u64,
    ttl_ms: u64,
) -> Result<SandboxPromotePreview, String> {
    if files.is_empty() {
        return Err("Select at least one file to promote".to_string());
    }
    if files.len() > MAX_PROMOTE_FILES {
        return Err(format!(
            "At most {MAX_PROMOTE_FILES} files can be promoted in a single action"
        ));
    }

    let mut seen = HashSet::new();
    let mut entries = Vec::with_capacity(files.len());
    for raw in files {
        let relative = validate_relative_promote_path(raw)?;
        let normalized = relative.to_string_lossy().replace('\\', "/");
        if !seen.insert(normalized.clone()) {
            return Err(format!("Duplicate file in promote request: '{normalized}'"));
        }
        let absolute = sandbox_workspace_dir.join(&relative);
        if !absolute.is_file() {
            return Err(format!(
                "'{normalized}' was not found in the sandbox copy"
            ));
        }
        let (sha256, size_bytes) = hash_file(&absolute).map_err(|error| {
            format!("Failed to read '{normalized}' from the sandbox copy: {error}")
        })?;
        entries.push(PromoteFileEntry {
            path: normalized,
            sha256,
            size_bytes,
        });
    }

    let digest = compute_promote_digest(run_id, &entries);
    let expires_at_ms = now_ms
        .checked_add(ttl_ms)
        .ok_or_else(|| "Confirmation expiry overflow".to_string())?;

    Ok(SandboxPromotePreview {
        run_id: run_id.to_string(),
        digest: digest.clone(),
        confirmation_phrase: confirmation_phrase_for(&digest),
        files: entries,
        expires_at_ms,
    })
}

/// Checks the digest shape, the exact confirmation phrase, that a pending
/// preview for this digest actually exists, that it belongs to the claimed
/// run, and that it has not expired — all before any file is ever touched.
/// On any failure this returns `Err` and the caller (see
/// `sandbox_execute_promote`) never proceeds to read or write anything.
fn validate_promote_confirmation(
    pending: Option<&PendingPromote>,
    run_id: &str,
    digest: &str,
    confirmation_phrase: &str,
    now_ms: u64,
) -> Result<PendingPromote, String> {
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Invalid promote digest".to_string());
    }
    if confirmation_phrase != confirmation_phrase_for(digest) {
        return Err("Type the exact confirmation phrase shown in the preview".to_string());
    }
    let pending = pending.ok_or_else(|| {
        "This promote confirmation has expired or was already used; prepare it again".to_string()
    })?;
    if pending.run_id != run_id {
        return Err("This confirmation does not belong to the specified sandbox run".to_string());
    }
    if now_ms > pending.expires_at_ms {
        return Err("This promote confirmation has expired; prepare it again".to_string());
    }
    Ok(pending.clone())
}

/// Re-hashes the sandbox copy for exactly the files a preview covered and
/// confirms the digest still matches — i.e. nothing changed in the sandbox
/// between prepare and execute.
fn verify_unchanged_since_preview(
    run_id: &str,
    sandbox_workspace_dir: &Path,
    pending: &PendingPromote,
    digest: &str,
) -> Result<(), String> {
    let paths: Vec<String> = pending.files.iter().map(|file| file.path.clone()).collect();
    let fresh = build_promote_preview(run_id, sandbox_workspace_dir, &paths, 0, 0)?;
    if fresh.digest != digest {
        return Err(
            "Sandbox files changed since the promote preview was generated; prepare it again"
                .to_string(),
        );
    }
    Ok(())
}

fn atomic_write(dest: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = dest.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination has no parent directory")
    })?;
    fs::create_dir_all(parent)?;
    let file_name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let tmp_path = parent.join(format!(
        ".{file_name}.sandbox-tmp-{}",
        uuid::Uuid::new_v4().simple()
    ));
    fs::write(&tmp_path, bytes)?;
    fs::rename(&tmp_path, dest)
}

/// Copies exactly `files` from the sandbox copy into the real workspace,
/// re-verifying each file's hash immediately before writing it (defense in
/// depth on top of [`verify_unchanged_since_preview`]'s whole-set check) and
/// writing every destination atomically (temp file + rename). Stops at the
/// first failure — files already written are not rolled back, but nothing
/// is written at all unless every prior check in the caller already passed.
pub fn promote_files(
    sandbox_workspace_dir: &Path,
    real_root: &Path,
    files: &[PromoteFileEntry],
) -> Result<Vec<String>, String> {
    let mut promoted = Vec::with_capacity(files.len());
    for file in files {
        let relative = validate_relative_promote_path(&file.path)?;
        let source = sandbox_workspace_dir.join(&relative);
        let bytes = fs::read(&source).map_err(|error| {
            format!("Failed to read '{}' from the sandbox copy: {error}", file.path)
        })?;
        if sha256_hex_bytes(&bytes) != file.sha256 {
            return Err(format!(
                "'{}' changed in the sandbox copy since the preview was generated",
                file.path
            ));
        }
        let destination = real_root.join(&relative);
        atomic_write(&destination, &bytes).map_err(|error| {
            format!("Failed to write '{}' to the workspace: {error}", file.path)
        })?;
        promoted.push(file.path.clone());
    }
    Ok(promoted)
}

/// Lists files that differ between the sandbox copy and the real workspace
/// (added or modified only — see [`SandboxDiffEntry::status`]).
pub fn diff_sandbox_against_workspace(
    sandbox_workspace_dir: &Path,
    real_root: &Path,
) -> Result<Vec<SandboxDiffEntry>, String> {
    let mut entries = Vec::new();
    let walker = walkdir::WalkDir::new(sandbox_workspace_dir)
        .min_depth(1)
        .into_iter()
        .filter_entry(|entry| {
            !(entry.file_type().is_dir()
                && is_skippable_dir_name(&entry.file_name().to_string_lossy()))
        });

    for entry in walker {
        let entry = entry.map_err(|error| error.to_string())?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(sandbox_workspace_dir)
            .unwrap_or(entry.path());
        let (sandbox_sha256, size_bytes) = hash_file(entry.path()).map_err(|e| e.to_string())?;
        let real_path = real_root.join(rel);
        let workspace_sha256 = if real_path.is_file() {
            Some(hash_file(&real_path).map_err(|e| e.to_string())?.0)
        } else {
            None
        };
        let status = match &workspace_sha256 {
            None => "added",
            Some(hash) if *hash == sandbox_sha256 => continue,
            Some(_) => "modified",
        };
        entries.push(SandboxDiffEntry {
            path: rel.to_string_lossy().replace('\\', "/"),
            status: status.to_string(),
            sandbox_sha256,
            workspace_sha256,
            size_bytes,
        });
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

async fn run_sandboxed_body(
    app: &tauri::AppHandle,
    state: &AppState,
    run_id: &str,
    root: &Path,
    workspace_dir: &Path,
    profile_path: &Path,
    request: &SandboxRunRequest,
    engine: &ClientIdentity,
) -> Result<SandboxRunSummary, String> {
    let stats = copy_workspace_into_sandbox(root, workspace_dir)
        .map_err(|error| format!("Failed to create the ephemeral sandbox copy: {error}"))?;

    crate::run_commands::append_event_as(
        app,
        state,
        run_id.to_string(),
        None,
        RunEvent::CheckpointLinked {
            checkpoint_id: format!("sandbox-copy-{run_id}"),
            kind: CheckpointKind::Workspace,
            label: bounded(
                &format!(
                    "Ephemeral copy: {} file(s), {} byte(s)",
                    stats.files_copied, stats.bytes_copied
                ),
                1_024,
            ),
            content_sha256: None,
        },
        engine.clone(),
    )?;

    let outcome = execute_in_sandbox(
        workspace_dir,
        profile_path,
        &request.command,
        request.timeout(),
        request.allow_network,
        &request.approved_env,
    )
    .await
    .map_err(|error| format!("Failed to execute the sandboxed command: {error}"))?;

    let store = crate::artifact_commands::store_for(app, state)?;
    let stdout_blob = store.put(&outcome.stdout).map_err(|error| error.to_string())?;
    let stderr_blob = store.put(&outcome.stderr).map_err(|error| error.to_string())?;

    crate::run_commands::append_event_as(
        app,
        state,
        run_id.to_string(),
        None,
        RunEvent::ArtifactAdded {
            artifact_id: stdout_blob.id.clone(),
            kind: ArtifactKind::Report,
            name: "stdout.log".to_string(),
            media_type: "text/plain".to_string(),
            content_sha256: stdout_blob.id.clone(),
            size_bytes: stdout_blob.size,
        },
        engine.clone(),
    )?;
    crate::run_commands::append_event_as(
        app,
        state,
        run_id.to_string(),
        None,
        RunEvent::ArtifactAdded {
            artifact_id: stderr_blob.id.clone(),
            kind: ArtifactKind::Report,
            name: "stderr.log".to_string(),
            media_type: "text/plain".to_string(),
            content_sha256: stderr_blob.id.clone(),
            size_bytes: stderr_blob.size,
        },
        engine.clone(),
    )?;

    let passed = !outcome.timed_out && outcome.exit_code == Some(0);
    crate::run_commands::append_event_as(
        app,
        state,
        run_id.to_string(),
        None,
        RunEvent::VerificationFinished {
            verification_id: format!("sandbox-exec-{run_id}"),
            name: "Sandboxed command execution".to_string(),
            passed,
            summary: bounded(
                &format!(
                    "isolation={:?} exit_code={:?} timed_out={} duration_ms={}",
                    outcome.isolation, outcome.exit_code, outcome.timed_out, outcome.duration_ms
                ),
                MAX_EVENT_TEXT_EXCERPT,
            ),
            artifact_ids: vec![stdout_blob.id.clone(), stderr_blob.id.clone()],
            duration_ms: outcome.duration_ms,
        },
        engine.clone(),
    )?;

    Ok(SandboxRunSummary {
        run_id: run_id.to_string(),
        isolation: outcome.isolation,
        exit_code: outcome.exit_code,
        timed_out: outcome.timed_out,
        passed,
        duration_ms: outcome.duration_ms,
        stdout_artifact_id: stdout_blob.id,
        stderr_artifact_id: stderr_blob.id,
        stdout_excerpt: bounded(&String::from_utf8_lossy(&outcome.stdout), MAX_EVENT_TEXT_EXCERPT),
        stderr_excerpt: bounded(&String::from_utf8_lossy(&outcome.stderr), MAX_EVENT_TEXT_EXCERPT),
        files_copied: stats.files_copied,
    })
}

async fn run_sandboxed(
    app: &tauri::AppHandle,
    window: &tauri::Window,
    state: &AppState,
    request: SandboxRunRequest,
) -> Result<SandboxRunSummary, String> {
    request.validate()?;
    let root = workspace::primary_root_canon(state)?;
    let run_id = format!("sandbox-{}", uuid::Uuid::new_v4().simple());
    let sandbox_root = sandbox_run_dir(app, &run_id)?;
    fs::create_dir_all(&sandbox_root)
        .map_err(|error| format!("Failed to create the sandbox run directory: {error}"))?;
    let workspace_dir = sandbox_root.join("workspace");
    let profile_path = sandbox_root.join("seatbelt.sb");

    let identity = crate::run_commands::desktop_identity(app, window);
    let created_at_ms = crate::run_commands::unix_time_ms()?;
    let spec = build_sandbox_run_spec(&run_id, identity, &root, &request, created_at_ms)?;
    crate::run_commands::with_ledger(app, state, |ledger| ledger.submit_run(&spec))?;

    let engine = crate::run_commands::engine_identity(app, "sandbox");
    crate::run_commands::append_event_as(
        app,
        state,
        run_id.clone(),
        None,
        RunEvent::Queued { queue: None },
        engine.clone(),
    )?;
    crate::run_commands::append_event_as(
        app,
        state,
        run_id.clone(),
        None,
        RunEvent::Started {
            engine_id: "sandbox".to_string(),
        },
        engine.clone(),
    )?;

    let outcome = run_sandboxed_body(
        app,
        state,
        &run_id,
        &root,
        &workspace_dir,
        &profile_path,
        &request,
        &engine,
    )
    .await;

    match outcome {
        Ok(summary) => Ok(summary),
        Err(error) => {
            let _ = crate::run_commands::append_event_as(
                app,
                state,
                run_id.clone(),
                None,
                RunEvent::Failed {
                    code: "sandbox_error".to_string(),
                    message: bounded(&error, MAX_EVENT_TEXT_EXCERPT),
                    retryable: false,
                },
                engine,
            );
            Err(error)
        }
    }
}

#[tauri::command]
pub async fn sandbox_run(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
    command: String,
    timeout_ms: Option<u64>,
    allow_network: bool,
    approved_env: Vec<String>,
) -> Result<SandboxRunSummary, String> {
    let request = SandboxRunRequest {
        command,
        timeout_ms,
        allow_network,
        approved_env,
    };
    run_sandboxed(&app, &window, state.inner(), request).await
}

#[tauri::command]
pub fn sandbox_list(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<SandboxRunListEntry>, String> {
    let runs = crate::run_commands::with_ledger(&app, state.inner(), |ledger| {
        ledger.list_runs(200, false)
    })?;
    Ok(runs
        .into_iter()
        .filter(|run| run.spec.kind == RunKind::Sandboxed)
        .map(|run| SandboxRunListEntry {
            run_id: run.spec.run_id.clone(),
            status: run.status,
            task: run.spec.task.clone(),
            created_at_ms: run.spec.created_at_ms,
            updated_at_ms: run.updated_at_ms,
        })
        .collect())
}

#[tauri::command]
pub fn sandbox_diff(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    run_id: String,
) -> Result<Vec<SandboxDiffEntry>, String> {
    let run = require_sandboxed_run(&app, state.inner(), &run_id)?;
    let root = workspace::primary_root_canon(state.inner())?;
    expect_matching_root(&run, &root)?;
    let workspace_dir = sandbox_run_dir(&app, &run_id)?.join("workspace");
    if !workspace_dir.is_dir() {
        return Err("The sandbox copy for this run is no longer available".to_string());
    }
    diff_sandbox_against_workspace(&workspace_dir, &root)
}

#[tauri::command]
pub fn sandbox_prepare_promote(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    run_id: String,
    files: Vec<String>,
) -> Result<SandboxPromotePreview, String> {
    let run = require_sandboxed_run(&app, state.inner(), &run_id)?;
    let root = workspace::primary_root_canon(state.inner())?;
    expect_matching_root(&run, &root)?;
    let workspace_dir = sandbox_run_dir(&app, &run_id)?.join("workspace");
    let now = crate::run_commands::unix_time_ms()?;
    let preview = build_promote_preview(&run_id, &workspace_dir, &files, now, PROMOTE_PREVIEW_TTL_MS)?;

    {
        let mut guard = state
            .inner()
            .sandbox
            .previews
            .lock()
            .map_err(|_| "Sandbox preview lock poisoned".to_string())?;
        guard.insert(
            preview.digest.clone(),
            PendingPromote {
                run_id: run_id.clone(),
                files: preview.files.clone(),
                expires_at_ms: preview.expires_at_ms,
            },
        );
    }

    let identity = crate::run_commands::engine_identity(&app, "sandbox-promote");
    crate::run_commands::append_event_as(
        &app,
        state.inner(),
        run_id.clone(),
        None,
        RunEvent::ExternalMutationPrepared {
            mutation_id: preview.digest[..24].to_string(),
            tool_call_id: format!("promote-{run_id}"),
            kind: MutationKind::Filesystem,
            idempotency_key: Some(preview.digest.clone()),
            summary: bounded(
                &format!(
                    "Promote {} file(s) from the sandbox to the workspace",
                    preview.files.len()
                ),
                MAX_EVENT_TEXT_EXCERPT,
            ),
        },
        identity,
    )?;

    Ok(preview)
}

#[tauri::command]
pub fn sandbox_execute_promote(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    run_id: String,
    digest: String,
    confirmation_phrase: String,
) -> Result<SandboxPromoteResult, String> {
    let now = crate::run_commands::unix_time_ms()?;
    let pending_snapshot = {
        let guard = state
            .inner()
            .sandbox
            .previews
            .lock()
            .map_err(|_| "Sandbox preview lock poisoned".to_string())?;
        guard.get(&digest).cloned()
    };
    let pending = validate_promote_confirmation(
        pending_snapshot.as_ref(),
        &run_id,
        &digest,
        &confirmation_phrase,
        now,
    )?;

    let run = require_sandboxed_run(&app, state.inner(), &run_id)?;
    let root = workspace::primary_root_canon(state.inner())?;
    expect_matching_root(&run, &root)?;
    let workspace_dir = sandbox_run_dir(&app, &run_id)?.join("workspace");
    verify_unchanged_since_preview(&run_id, &workspace_dir, &pending, &digest)?;

    let promoted = promote_files(&workspace_dir, &root, &pending.files)?;

    {
        let mut guard = state
            .inner()
            .sandbox
            .previews
            .lock()
            .map_err(|_| "Sandbox preview lock poisoned".to_string())?;
        guard.remove(&digest);
    }

    let identity = crate::run_commands::engine_identity(&app, "sandbox-promote");
    crate::run_commands::append_event_as(
        &app,
        state.inner(),
        run_id.clone(),
        None,
        RunEvent::ExternalMutationConfirmed {
            mutation_id: digest[..24].to_string(),
            confirmation_ref: None,
            summary: bounded(
                &format!(
                    "Promoted {} file(s) from the sandbox to the workspace",
                    promoted.len()
                ),
                MAX_EVENT_TEXT_EXCERPT,
            ),
        },
        identity.clone(),
    )?;
    crate::run_commands::append_event_as(
        &app,
        state.inner(),
        run_id.clone(),
        None,
        RunEvent::Completed {
            summary: Some(bounded(
                &format!("Promoted {} file(s): {}", promoted.len(), promoted.join(", ")),
                MAX_EVENT_TEXT_EXCERPT,
            )),
            result_artifact_ids: Vec::new(),
            usage: UsageSnapshot {
                input_tokens: 0,
                output_tokens: 0,
                cached_input_tokens: 0,
                model_calls: 0,
                tool_calls: 1,
                cost_micros: None,
            },
        },
        identity,
    )?;

    Ok(SandboxPromoteResult {
        run_id,
        promoted_files: promoted,
    })
}

#[tauri::command]
pub fn sandbox_discard(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
    run_id: String,
    reason: Option<String>,
) -> Result<(), String> {
    require_sandboxed_run(&app, state.inner(), &run_id)?;
    crate::run_commands::append_host_event(
        &app,
        &window,
        state.inner(),
        run_id.clone(),
        None,
        RunEvent::Cancelled { reason },
    )?;

    {
        let mut guard = state
            .inner()
            .sandbox
            .previews
            .lock()
            .map_err(|_| "Sandbox preview lock poisoned".to_string())?;
        guard.retain(|_, pending| pending.run_id != run_id);
    }

    let dir = sandbox_run_dir(&app, &run_id)?;
    if dir.exists() {
        let _ = fs::remove_dir_all(&dir);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis() as u64
    }

    fn temp_dir(label: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "little-monkey-sandbox-test-{label}-{}-{counter}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, content).expect("write fixture file");
    }

    // --- copy_workspace_into_sandbox -----------------------------------

    #[test]
    fn copy_excludes_git_node_modules_target_and_secrets() {
        let root = temp_dir("copy-src");
        let dest = temp_dir("copy-dest");

        write(&root.join(".git/HEAD"), "ref: refs/heads/main");
        write(&root.join("node_modules/pkg/index.js"), "module.exports = {};");
        write(&root.join("target/debug/app"), "binary");
        write(&root.join(".env"), "API_KEY=super-secret");
        write(&root.join("src/main.rs"), "fn main() {}");
        write(&root.join("package.json"), "{\"name\":\"fixture\"}");

        let stats = copy_workspace_into_sandbox(&root, &dest).expect("copy succeeds");

        assert!(!dest.join(".git").exists(), ".git must never be copied");
        assert!(!dest.join("node_modules").exists(), "node_modules must never be copied");
        assert!(!dest.join("target").exists(), "target must never be copied");
        assert!(!dest.join(".env").exists(), "secrets must never be copied");
        assert!(dest.join("src/main.rs").is_file(), "ordinary source files must be copied");
        assert!(
            dest.join("package.json").is_file(),
            "manifests must still be copied so the sandbox copy can actually build/test"
        );
        assert!(stats.files_copied >= 2);
        assert!(stats.skipped >= 1);

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&dest);
    }

    // --- allowlisted_env -------------------------------------------------

    #[test]
    fn allowlisted_env_excludes_unapproved_secrets() {
        std::env::set_var("SANDBOX_TEST_SECRET_TOKEN", "super-secret-value");
        let env = allowlisted_env(&[]);
        assert!(!env.iter().any(|(key, _)| key == "SANDBOX_TEST_SECRET_TOKEN"));
        assert!(env.iter().any(|(key, _)| key == "PATH"));
        std::env::remove_var("SANDBOX_TEST_SECRET_TOKEN");
    }

    #[test]
    fn allowlisted_env_includes_only_explicitly_approved_extras() {
        std::env::set_var("SANDBOX_TEST_APPROVED", "yes");
        std::env::set_var("SANDBOX_TEST_UNAPPROVED", "no");
        let env = allowlisted_env(&["SANDBOX_TEST_APPROVED".to_string()]);
        assert!(env.iter().any(|(key, value)| key == "SANDBOX_TEST_APPROVED" && value == "yes"));
        assert!(!env.iter().any(|(key, _)| key == "SANDBOX_TEST_UNAPPROVED"));
        std::env::remove_var("SANDBOX_TEST_APPROVED");
        std::env::remove_var("SANDBOX_TEST_UNAPPROVED");
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn spawned_child_never_inherits_unapproved_secrets() {
        std::env::set_var("SANDBOX_TEST_CHILD_SECRET", "leak-me-not");
        let dir = temp_dir("exec-env");
        let profile_path = dir.join("seatbelt.sb");

        let outcome = execute_in_sandbox(
            &dir,
            &profile_path,
            "env",
            Duration::from_secs(10),
            false,
            &[],
        )
        .await
        .expect("command executes");

        let stdout = String::from_utf8_lossy(&outcome.stdout);
        assert!(
            !stdout.contains("SANDBOX_TEST_CHILD_SECRET"),
            "child env leaked an unapproved variable: {stdout}"
        );
        assert!(stdout.contains("PATH="), "child env should still contain PATH");

        std::env::remove_var("SANDBOX_TEST_CHILD_SECRET");
        let _ = fs::remove_dir_all(&dir);
    }

    // --- build_seatbelt_profile ------------------------------------------

    #[test]
    fn seatbelt_profile_denies_by_default_and_scopes_writes_to_sandbox_dir() {
        let dir = Path::new("/tmp/example-sandbox-dir");
        let profile = build_seatbelt_profile(dir, false);
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(allow file-write* (subpath \"/tmp/example-sandbox-dir\"))"));
        assert!(profile.contains("(deny network*)"));
        assert!(!profile.contains("(allow network*)"));
    }

    #[test]
    fn seatbelt_profile_allows_network_only_when_requested() {
        let dir = Path::new("/tmp/example-sandbox-dir");
        let profile = build_seatbelt_profile(dir, true);
        assert!(profile.contains("(allow network*)"));
        assert!(!profile.contains("(deny network*)"));
    }

    // --- promote digest / confirmation -----------------------------------

    #[test]
    fn promote_digest_changes_with_content_and_is_order_independent() {
        let a = vec![
            PromoteFileEntry { path: "a.txt".into(), sha256: "1".repeat(64), size_bytes: 1 },
            PromoteFileEntry { path: "b.txt".into(), sha256: "2".repeat(64), size_bytes: 2 },
        ];
        let shuffled = vec![a[1].clone(), a[0].clone()];
        assert_eq!(compute_promote_digest("run-1", &a), compute_promote_digest("run-1", &shuffled));

        let mut changed = a.clone();
        changed[0].sha256 = "3".repeat(64);
        assert_ne!(compute_promote_digest("run-1", &a), compute_promote_digest("run-1", &changed));

        assert_ne!(compute_promote_digest("run-1", &a), compute_promote_digest("run-2", &a));
    }

    #[test]
    fn build_promote_preview_rejects_path_traversal_and_missing_files() {
        let dir = temp_dir("preview-src");
        write(&dir.join("kept.txt"), "hello");

        let traversal = build_promote_preview("run-1", &dir, &["../escape.txt".to_string()], 0, 1_000);
        assert!(traversal.is_err());

        let missing = build_promote_preview("run-1", &dir, &["missing.txt".to_string()], 0, 1_000);
        assert!(missing.is_err());

        let ok = build_promote_preview("run-1", &dir, &["kept.txt".to_string()], 0, 1_000)
            .expect("valid file promotes");
        assert_eq!(ok.confirmation_phrase, confirmation_phrase_for(&ok.digest));

        let _ = fs::remove_dir_all(&dir);
    }

    // --- promote refuses without a valid digest+phrase, never touches disk

    #[test]
    fn validate_promote_confirmation_rejects_wrong_phrase_without_touching_anything() {
        let pending = PendingPromote {
            run_id: "run-1".to_string(),
            files: vec![PromoteFileEntry { path: "a.txt".into(), sha256: "a".repeat(64), size_bytes: 1 }],
            expires_at_ms: now_ms() + 60_000,
        };
        let digest = compute_promote_digest("run-1", &pending.files);

        let wrong_phrase = validate_promote_confirmation(
            Some(&pending),
            "run-1",
            &digest,
            "CONFIRM wrong-phrase",
            now_ms(),
        );
        assert!(wrong_phrase.is_err());

        let wrong_run = validate_promote_confirmation(
            Some(&pending),
            "some-other-run",
            &digest,
            &confirmation_phrase_for(&digest),
            now_ms(),
        );
        assert!(wrong_run.is_err());

        let expired = validate_promote_confirmation(
            Some(&PendingPromote { expires_at_ms: 0, ..pending.clone() }),
            "run-1",
            &digest,
            &confirmation_phrase_for(&digest),
            now_ms(),
        );
        assert!(expired.is_err());

        let missing = validate_promote_confirmation(None, "run-1", &digest, &confirmation_phrase_for(&digest), now_ms());
        assert!(missing.is_err());

        let ok = validate_promote_confirmation(
            Some(&pending),
            "run-1",
            &digest,
            &confirmation_phrase_for(&digest),
            now_ms(),
        );
        assert!(ok.is_ok());
    }

    #[test]
    fn promote_files_end_to_end_never_writes_on_prior_validation_failure() {
        let sandbox_dir = temp_dir("promote-sandbox");
        let real_root = temp_dir("promote-real");
        write(&sandbox_dir.join("app.txt"), "sandbox content");
        write(&real_root.join("app.txt"), "original content");

        let preview = build_promote_preview(
            "run-1",
            &sandbox_dir,
            &["app.txt".to_string()],
            now_ms(),
            60_000,
        )
        .expect("preview builds");

        // Wrong phrase: the caller-level flow must never call `promote_files`
        // at all — simulate that gate here and confirm the real file is
        // still untouched.
        let pending = PendingPromote {
            run_id: "run-1".to_string(),
            files: preview.files.clone(),
            expires_at_ms: preview.expires_at_ms,
        };
        let rejected = validate_promote_confirmation(
            Some(&pending),
            "run-1",
            &preview.digest,
            "CONFIRM 000000000000",
            now_ms(),
        );
        assert!(rejected.is_err());
        assert_eq!(
            fs::read_to_string(real_root.join("app.txt")).unwrap(),
            "original content",
            "the real file must be untouched after a rejected confirmation"
        );

        // Correct phrase: promote actually copies the sandbox content over.
        let accepted = validate_promote_confirmation(
            Some(&pending),
            "run-1",
            &preview.digest,
            &preview.confirmation_phrase,
            now_ms(),
        )
        .expect("valid confirmation is accepted");
        verify_unchanged_since_preview("run-1", &sandbox_dir, &accepted, &preview.digest)
            .expect("nothing changed since prepare");
        let promoted = promote_files(&sandbox_dir, &real_root, &accepted.files).expect("promote succeeds");
        assert_eq!(promoted, vec!["app.txt".to_string()]);
        assert_eq!(
            fs::read_to_string(real_root.join("app.txt")).unwrap(),
            "sandbox content"
        );

        let _ = fs::remove_dir_all(&sandbox_dir);
        let _ = fs::remove_dir_all(&real_root);
    }

    // --- diff --------------------------------------------------------------

    #[test]
    fn diff_reports_added_and_modified_but_not_unchanged() {
        let sandbox_dir = temp_dir("diff-sandbox");
        let real_root = temp_dir("diff-real");
        write(&sandbox_dir.join("added.txt"), "new");
        write(&sandbox_dir.join("modified.txt"), "changed");
        write(&real_root.join("modified.txt"), "original");
        write(&sandbox_dir.join("unchanged.txt"), "same");
        write(&real_root.join("unchanged.txt"), "same");

        let diff = diff_sandbox_against_workspace(&sandbox_dir, &real_root).expect("diff succeeds");
        let paths: Vec<&str> = diff.iter().map(|entry| entry.path.as_str()).collect();
        assert!(paths.contains(&"added.txt"));
        assert!(paths.contains(&"modified.txt"));
        assert!(!paths.contains(&"unchanged.txt"));

        let _ = fs::remove_dir_all(&sandbox_dir);
        let _ = fs::remove_dir_all(&real_root);
    }
}
