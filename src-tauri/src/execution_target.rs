//! Shared execution-target and workspace-transfer contracts.
//!
//! The autonomous coordinator only schedules work. This module owns the
//! transport-neutral contract that lets a local process, an OCI container, a
//! paired Little Monkey node, or `monkey runner serve --stdio` execute the same
//! frozen run. It deliberately contains no Tauri state and never accepts a
//! shell command string.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::{Display, Formatter};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

pub const EXECUTION_PROTOCOL_VERSION: u32 = 1;
pub const MAX_TRANSFER_FILES: usize = 100_000;
pub const MAX_TRANSFER_FILE_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_TRANSFER_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const MAX_TRANSFER_PATH_BYTES: usize = 4 * 1024;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or_default()
}

pub fn execution_now_ms() -> u64 {
    now_ms()
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

fn normalized_path(value: &str) -> String {
    value.nfkc().flat_map(char::to_lowercase).collect()
}

fn same_path(left: &Path, right: &Path) -> bool {
    let left = left.canonicalize().unwrap_or_else(|_| left.to_path_buf());
    let right = right.canonicalize().unwrap_or_else(|_| right.to_path_buf());
    if left == right {
        return true;
    }
    #[cfg(windows)]
    {
        let normalize = |path: &Path| {
            path.to_string_lossy()
                .replace('\\', "/")
                .trim_end_matches('/')
                .to_ascii_lowercase()
        };
        normalize(&left) == normalize(&right)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn command_output(
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
) -> Result<Vec<u8>, TargetError> {
    let mut command = Command::new(program);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command.output().map_err(|error| {
        TargetError::target_unreachable(format!("could not start {program}: {error}"))
    })?;
    if !output.status.success() {
        return Err(TargetError::target_unreachable(format!(
            "{program} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

fn validate_environment(environment: &BTreeMap<String, String>) -> Result<(), TargetError> {
    for key in environment.keys() {
        if key.is_empty()
            || !key
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return Err(TargetError::invalid(
                "runner environment contains an unsafe variable name",
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetCapabilities {
    pub durable_background_execution: bool,
    pub shell: bool,
    pub browser: bool,
    pub gpu: bool,
    pub local_models: bool,
    pub git: bool,
    pub persistent_workspace: bool,
    pub disposable_workspace: bool,
    pub suspend: bool,
    pub migration: bool,
    pub desktop_control: bool,
    pub outbound_network: bool,
    pub max_ram_bytes: Option<u64>,
    pub max_cpu_cores: Option<u32>,
    pub max_artifact_size: Option<u64>,
}

impl TargetCapabilities {
    pub fn local() -> Self {
        Self {
            shell: true,
            browser: true,
            local_models: true,
            git: true,
            persistent_workspace: true,
            disposable_workspace: true,
            suspend: true,
            desktop_control: true,
            outbound_network: true,
            ..Self::default()
        }
    }

    pub fn docker() -> Self {
        Self {
            durable_background_execution: true,
            shell: true,
            git: true,
            disposable_workspace: true,
            outbound_network: false,
            max_ram_bytes: Some(2 * 1024 * 1024 * 1024),
            max_cpu_cores: Some(2),
            max_artifact_size: Some(32 * 1024 * 1024),
            ..Self::default()
        }
    }

    pub fn missing(&self, required: &RequiredCapabilities) -> Vec<String> {
        let mut missing = Vec::new();
        let checks = [
            (
                required.durable_background_execution,
                self.durable_background_execution,
                "durable background execution",
            ),
            (required.shell, self.shell, "shell"),
            (required.browser, self.browser, "browser"),
            (required.gpu, self.gpu, "GPU"),
            (required.local_models, self.local_models, "local models"),
            (required.git, self.git, "Git"),
            (
                required.persistent_workspace,
                self.persistent_workspace,
                "persistent workspace",
            ),
            (
                required.disposable_workspace,
                self.disposable_workspace,
                "disposable workspace",
            ),
            (required.suspend, self.suspend, "suspend"),
            (required.migration, self.migration, "migration"),
            (
                required.desktop_control,
                self.desktop_control,
                "desktop control",
            ),
            (
                required.outbound_network,
                self.outbound_network,
                "outbound network",
            ),
        ];
        for (wanted, available, label) in checks {
            if wanted && !available {
                missing.push(label.to_string());
            }
        }
        if let Some(max) = required.min_ram_bytes {
            if self.max_ram_bytes.is_none_or(|available| available < max) {
                missing.push(format!("at least {max} bytes of RAM"));
            }
        }
        if let Some(max) = required.min_cpu_cores {
            if self.max_cpu_cores.is_none_or(|available| available < max) {
                missing.push(format!("at least {max} CPU cores"));
            }
        }
        missing
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequiredCapabilities {
    pub durable_background_execution: bool,
    pub shell: bool,
    pub browser: bool,
    pub gpu: bool,
    pub local_models: bool,
    pub git: bool,
    pub persistent_workspace: bool,
    pub disposable_workspace: bool,
    pub suspend: bool,
    pub migration: bool,
    pub desktop_control: bool,
    pub outbound_network: bool,
    pub min_ram_bytes: Option<u64>,
    pub min_cpu_cores: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionTargetKind {
    Local,
    Docker,
    RemoteNode,
    SshRunner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetTrustState {
    Unverified,
    Verified,
    Changed,
    Revoked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetIdentity {
    pub stable_id: String,
    pub display_name: String,
    pub kind: ExecutionTargetKind,
    pub endpoint: Option<String>,
    pub verified_identity: Option<String>,
    pub platform: String,
    pub runner_version: String,
    pub protocol_version: u32,
    pub capabilities: TargetCapabilities,
    pub last_successful_probe_ms: Option<u64>,
    pub trust_state: TargetTrustState,
}

impl TargetIdentity {
    pub fn validate(&self) -> Result<(), TargetError> {
        if !valid_id(&self.stable_id) {
            return Err(TargetError::invalid("target stable id is invalid"));
        }
        if self.display_name.trim().is_empty() || self.display_name.len() > 256 {
            return Err(TargetError::invalid("target display name is invalid"));
        }
        if self.protocol_version != EXECUTION_PROTOCOL_VERSION {
            return Err(TargetError::protocol_incompatible(format!(
                "target speaks protocol {}, this runner speaks {}",
                self.protocol_version, EXECUTION_PROTOCOL_VERSION
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionTargetSnapshot {
    pub identity: TargetIdentity,
    pub probed_at_ms: u64,
    pub capability_digest: String,
}

impl ExecutionTargetSnapshot {
    pub fn freeze(mut identity: TargetIdentity, probed_at_ms: u64) -> Result<Self, TargetError> {
        identity.last_successful_probe_ms = Some(probed_at_ms);
        identity.validate()?;
        let capabilities = serde_json::to_vec(&identity.capabilities)
            .map_err(|error| TargetError::invalid(error.to_string()))?;
        Ok(Self {
            identity,
            probed_at_ms,
            capability_digest: digest(&capabilities),
        })
    }

    pub fn validate(&self) -> Result<(), TargetError> {
        self.identity.validate()?;
        let capabilities = serde_json::to_vec(&self.identity.capabilities)
            .map_err(|error| TargetError::invalid(error.to_string()))?;
        if digest(&capabilities) != self.capability_digest {
            return Err(TargetError::TargetIdentityChanged(
                "frozen capability snapshot does not match its digest".into(),
            ));
        }
        if self.probed_at_ms == 0 {
            return Err(TargetError::invalid("target probe timestamp is missing"));
        }
        Ok(())
    }

    pub fn require(&self, required: &RequiredCapabilities) -> Result<(), TargetError> {
        self.validate()?;
        let missing = self.identity.capabilities.missing(required);
        if missing.is_empty() {
            Ok(())
        } else {
            Err(TargetError::capability_unavailable(missing.join(", ")))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "code", content = "detail")]
pub enum TargetError {
    TargetUnreachable(String),
    TargetIdentityChanged(String),
    HostKeyChanged(String),
    ProtocolIncompatible(String),
    WorkspaceTransferFailed(String),
    WorkspaceConflict(String),
    RunnerLost(String),
    RunnerRestarted(String),
    CapabilityUnavailable(String),
    ResultRetrievalFailed(String),
    InvalidInput(String),
    Unsupported(String),
    Io(String),
}

impl TargetError {
    fn target_unreachable(detail: impl Into<String>) -> Self {
        Self::TargetUnreachable(detail.into())
    }
    fn workspace_transfer_failed(detail: impl Into<String>) -> Self {
        Self::WorkspaceTransferFailed(detail.into())
    }
    fn workspace_conflict(detail: impl Into<String>) -> Self {
        Self::WorkspaceConflict(detail.into())
    }
    fn runner_lost(detail: impl Into<String>) -> Self {
        Self::RunnerLost(detail.into())
    }
    fn protocol_incompatible(detail: impl Into<String>) -> Self {
        Self::ProtocolIncompatible(detail.into())
    }
    fn capability_unavailable(detail: impl Into<String>) -> Self {
        Self::CapabilityUnavailable(detail.into())
    }
    fn result_retrieval_failed(detail: impl Into<String>) -> Self {
        Self::ResultRetrievalFailed(detail.into())
    }
    fn unsupported(detail: impl Into<String>) -> Self {
        Self::Unsupported(detail.into())
    }
    fn invalid(detail: impl Into<String>) -> Self {
        Self::InvalidInput(detail.into())
    }
    pub fn code(&self) -> &'static str {
        match self {
            Self::TargetUnreachable(_) => "TARGET_UNREACHABLE",
            Self::TargetIdentityChanged(_) => "TARGET_IDENTITY_CHANGED",
            Self::HostKeyChanged(_) => "HOST_KEY_CHANGED",
            Self::ProtocolIncompatible(_) => "PROTOCOL_INCOMPATIBLE",
            Self::WorkspaceTransferFailed(_) => "WORKSPACE_TRANSFER_FAILED",
            Self::WorkspaceConflict(_) => "WORKSPACE_CONFLICT",
            Self::RunnerLost(_) => "RUNNER_LOST",
            Self::RunnerRestarted(_) => "RUNNER_RESTARTED",
            Self::CapabilityUnavailable(_) => "CAPABILITY_UNAVAILABLE",
            Self::ResultRetrievalFailed(_) => "RESULT_RETRIEVAL_FAILED",
            Self::InvalidInput(_) => "INVALID_INPUT",
            Self::Unsupported(_) => "UNSUPPORTED",
            Self::Io(_) => "IO_ERROR",
        }
    }
}

impl Display for TargetError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: ", self.code())?;
        match self {
            Self::TargetUnreachable(value)
            | Self::TargetIdentityChanged(value)
            | Self::HostKeyChanged(value)
            | Self::ProtocolIncompatible(value)
            | Self::WorkspaceTransferFailed(value)
            | Self::WorkspaceConflict(value)
            | Self::RunnerLost(value)
            | Self::RunnerRestarted(value)
            | Self::CapabilityUnavailable(value)
            | Self::ResultRetrievalFailed(value)
            | Self::InvalidInput(value)
            | Self::Unsupported(value)
            | Self::Io(value) => formatter.write_str(value),
        }
    }
}

impl std::error::Error for TargetError {}

impl From<std::io::Error> for TargetError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspacePolicy {
    Ephemeral,
    Cached,
    Persistent,
}

impl Default for WorkspacePolicy {
    fn default() -> Self {
        Self::Cached
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceTransferKind {
    CleanGit {
        canonical_remote_url: Option<String>,
        base_commit: String,
        branch: Option<String>,
        sparse_scope: Vec<String>,
    },
    DirtyGit {
        base_commit: String,
        branch: Option<String>,
        tracked_diff_digest: String,
        untracked_paths: Vec<String>,
    },
    ContentSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceManifestEntry {
    pub relative_path: String,
    pub file_type: String,
    pub size: u64,
    pub sha256: String,
    pub executable: bool,
    pub symlink_target: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceObject {
    pub sha256: String,
    #[serde(with = "serde_bytes")]
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceTransferLimits {
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_path_bytes: usize,
    pub max_symlink_depth: usize,
}

impl Default for WorkspaceTransferLimits {
    fn default() -> Self {
        Self {
            max_files: MAX_TRANSFER_FILES,
            max_file_bytes: MAX_TRANSFER_FILE_BYTES,
            max_total_bytes: MAX_TRANSFER_TOTAL_BYTES,
            max_path_bytes: MAX_TRANSFER_PATH_BYTES,
            max_symlink_depth: 32,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceTransfer {
    pub schema_version: u32,
    pub workspace_id: String,
    pub snapshot_id: String,
    pub base_snapshot_digest: String,
    pub kind: WorkspaceTransferKind,
    pub manifest: Vec<WorkspaceManifestEntry>,
    pub objects: Vec<WorkspaceObject>,
    #[serde(default)]
    pub object_hashes: BTreeSet<String>,
    pub tracked_diff: Option<WorkspaceObject>,
    /// The exact frozen Git base, transported as a bundle and checked out at
    /// the recorded commit on the executor.
    #[serde(default)]
    pub git_bundle: Option<WorkspaceObject>,
    #[serde(default)]
    pub tracked_diff_hash: Option<String>,
    #[serde(default)]
    pub git_bundle_hash: Option<String>,
    #[serde(default)]
    pub policy: WorkspacePolicy,
    pub limits: WorkspaceTransferLimits,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceManifestRequest {
    pub schema_version: u32,
    pub workspace_id: String,
    pub snapshot_id: String,
    pub base_snapshot_digest: String,
    pub kind: WorkspaceTransferKind,
    pub manifest: Vec<WorkspaceManifestEntry>,
    pub object_hashes: BTreeSet<String>,
    pub tracked_diff_hash: Option<String>,
    pub git_bundle_hash: Option<String>,
    pub limits: WorkspaceTransferLimits,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceMissingObjects {
    pub workspace_id: String,
    pub snapshot_id: String,
    pub missing_hashes: BTreeSet<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceCacheMarker {
    workspace_id: String,
    snapshot_id: String,
    base_snapshot_digest: String,
}

impl WorkspaceTransfer {
    fn cache_marker_path(destination: &Path) -> PathBuf {
        let name = destination
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("workspace");
        destination
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!(".{name}.transfer.json"))
    }

    pub fn mark_cached(&self, destination: &Path) -> Result<(), TargetError> {
        let marker = WorkspaceCacheMarker {
            workspace_id: self.workspace_id.clone(),
            snapshot_id: self.snapshot_id.clone(),
            base_snapshot_digest: self.base_snapshot_digest.clone(),
        };
        let bytes =
            serde_json::to_vec(&marker).map_err(|error| TargetError::invalid(error.to_string()))?;
        fs::write(Self::cache_marker_path(destination), bytes)?;
        Ok(())
    }

    pub fn cached_matches(&self, destination: &Path) -> Result<bool, TargetError> {
        let marker_path = Self::cache_marker_path(destination);
        if marker_path.exists() {
            let marker: WorkspaceCacheMarker = serde_json::from_slice(&fs::read(marker_path)?)
                .map_err(|error| TargetError::workspace_transfer_failed(error.to_string()))?;
            if marker.workspace_id != self.workspace_id
                || marker.snapshot_id != self.snapshot_id
                || marker.base_snapshot_digest != self.base_snapshot_digest
            {
                return Ok(false);
            }
            // A Persistent workspace is a task-owned mutable execution surface.
            // Its marker freezes the original transfer identity while later nodes
            // intentionally build on changes produced by earlier nodes. The path
            // is app-owned and task-scoped by the placement service.
            if matches!(self.policy, WorkspacePolicy::Persistent) {
                return Ok(true);
            }
        }
        let current = Self::from_workspace(destination, &self.workspace_id)?;
        Ok(manifest_digest(&current.manifest, &self.kind) == self.base_snapshot_digest)
    }

    pub fn from_workspace(root: &Path, workspace_id: &str) -> Result<Self, TargetError> {
        Self::from_workspace_with_limits(root, workspace_id, WorkspaceTransferLimits::default())
    }

    pub fn from_workspace_with_limits(
        root: &Path,
        workspace_id: &str,
        limits: WorkspaceTransferLimits,
    ) -> Result<Self, TargetError> {
        if !valid_id(workspace_id) {
            return Err(TargetError::invalid("workspace id is invalid"));
        }
        let root = root
            .canonicalize()
            .map_err(|error| TargetError::workspace_transfer_failed(error.to_string()))?;
        if !root.is_dir() {
            return Err(TargetError::workspace_transfer_failed(
                "workspace root is not a directory",
            ));
        }
        let git_root = command_output("git", &["rev-parse", "--show-toplevel"], Some(&root)).ok();
        let git_scope = git_root
            .as_ref()
            .filter(|value| same_path(&PathBuf::from(String::from_utf8_lossy(value).trim()), &root))
            .map(|_| {
                command_output(
                    "git",
                    &[
                        "ls-files",
                        "--cached",
                        "--others",
                        "--exclude-standard",
                        "-z",
                    ],
                    Some(&root),
                )
                .unwrap_or_default()
                .split(|byte| *byte == 0)
                .filter_map(|value| {
                    (!value.is_empty()).then(|| String::from_utf8_lossy(value).replace('\\', "/"))
                })
                .collect::<BTreeSet<_>>()
            });
        let mut manifest = Vec::new();
        let mut objects = Vec::new();
        let mut seen_paths = BTreeSet::new();
        let mut seen_normalized = BTreeSet::new();
        let mut total = 0u64;
        let git_scope_ref = git_scope.as_ref();
        for entry in WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                let relative = match entry.path().strip_prefix(&root) {
                    Ok(relative) => relative,
                    Err(_) => return false,
                };
                if relative.as_os_str().is_empty() {
                    return true;
                }
                if relative.components().next()
                    == Some(Component::Normal(std::ffi::OsStr::new(".git")))
                {
                    return false;
                }
                let Some(files) = git_scope_ref else {
                    return true;
                };
                let relative = relative.to_string_lossy().replace('\\', "/");
                if entry.file_type().is_dir() {
                    let prefix = format!("{relative}/");
                    files.iter().any(|file| file.starts_with(&prefix))
                } else {
                    files.contains(&relative)
                }
            })
        {
            let entry =
                entry.map_err(|error| TargetError::workspace_transfer_failed(error.to_string()))?;
            let relative = entry
                .path()
                .strip_prefix(&root)
                .map_err(|error| TargetError::workspace_transfer_failed(error.to_string()))?;
            if relative.as_os_str().is_empty() {
                continue;
            }
            if relative.components().next() == Some(Component::Normal(std::ffi::OsStr::new(".git")))
            {
                continue;
            }
            let relative = relative.to_string_lossy().replace('\\', "/");
            validate_relative_path(&relative, limits.max_path_bytes)?;
            let normalized = normalized_path(&relative);
            if !seen_paths.insert(relative.clone()) || !seen_normalized.insert(normalized) {
                return Err(TargetError::workspace_transfer_failed(format!(
                    "workspace contains a normalization/case collision at '{relative}'"
                )));
            }
            if manifest.len() >= limits.max_files {
                return Err(TargetError::workspace_transfer_failed(
                    "workspace file count exceeds transfer limit",
                ));
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            let file_type = metadata.file_type();
            let (kind, bytes, symlink_target) = if file_type.is_symlink() {
                let target = fs::read_link(entry.path())?.to_string_lossy().to_string();
                validate_symlink_target(&target, limits.max_symlink_depth)?;
                (
                    "symlink".to_string(),
                    target.as_bytes().to_vec(),
                    Some(target),
                )
            } else if file_type.is_file() {
                let bytes = fs::read(entry.path())?;
                ("file".to_string(), bytes, None)
            } else if file_type.is_dir() {
                ("directory".to_string(), Vec::new(), None)
            } else {
                return Err(TargetError::workspace_transfer_failed(format!(
                    "unsupported filesystem node '{relative}'"
                )));
            };
            let size = bytes.len() as u64;
            if size > limits.max_file_bytes || total.saturating_add(size) > limits.max_total_bytes {
                return Err(TargetError::workspace_transfer_failed(
                    "workspace byte limit exceeded",
                ));
            }
            total = total.saturating_add(size);
            let sha256 = digest(&bytes);
            let executable = is_executable(&metadata);
            manifest.push(WorkspaceManifestEntry {
                relative_path: relative,
                file_type: kind.clone(),
                size,
                sha256: sha256.clone(),
                executable,
                symlink_target,
            });
            if kind == "file" || kind == "symlink" {
                objects.push(WorkspaceObject { sha256, bytes });
            }
        }
        manifest.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        objects.sort_by(|left, right| left.sha256.cmp(&right.sha256));
        let (kind, tracked_diff, git_bundle) = if let Some(git_root) = git_root {
            if same_path(
                &PathBuf::from(String::from_utf8_lossy(&git_root).trim()),
                &root,
            ) {
                let base_commit = String::from_utf8_lossy(&command_output(
                    "git",
                    &["rev-parse", "HEAD"],
                    Some(&root),
                )?)
                .trim()
                .to_string();
                let branch = command_output("git", &["branch", "--show-current"], Some(&root))
                    .ok()
                    .map(|value| String::from_utf8_lossy(&value).trim().to_string())
                    .filter(|value| !value.is_empty());
                let remote = command_output("git", &["remote", "get-url", "origin"], Some(&root))
                    .ok()
                    .map(|value| String::from_utf8_lossy(&value).trim().to_string())
                    .filter(|value| !value.is_empty() && !value.contains('@'));
                let diff = command_output(
                    "git",
                    &["diff", "HEAD", "--binary", "--no-color"],
                    Some(&root),
                )
                .unwrap_or_default();
                let dirty = !command_output("git", &["status", "--porcelain=v1"], Some(&root))
                    .unwrap_or_default()
                    .is_empty();
                let untracked_paths = command_output(
                    "git",
                    &["ls-files", "--others", "--exclude-standard"],
                    Some(&root),
                )
                .unwrap_or_default()
                .split(|byte| *byte == b'\n')
                .filter_map(|value| {
                    let value = String::from_utf8_lossy(value).trim().to_string();
                    (!value.is_empty()).then_some(value)
                })
                .collect::<Vec<_>>();
                let bundle =
                    command_output("git", &["bundle", "create", "-", "--all"], Some(&root))?;
                let kind = if dirty {
                    WorkspaceTransferKind::DirtyGit {
                        base_commit,
                        branch,
                        tracked_diff_digest: digest(&diff),
                        untracked_paths,
                    }
                } else {
                    WorkspaceTransferKind::CleanGit {
                        canonical_remote_url: remote,
                        base_commit,
                        branch,
                        sparse_scope: Vec::new(),
                    }
                };
                let tracked_diff = (!diff.is_empty()).then(|| WorkspaceObject {
                    sha256: digest(&diff),
                    bytes: diff,
                });
                let git_bundle = Some(WorkspaceObject {
                    sha256: digest(&bundle),
                    bytes: bundle,
                });
                (kind, tracked_diff, git_bundle)
            } else {
                (WorkspaceTransferKind::ContentSnapshot, None, None)
            }
        } else {
            (WorkspaceTransferKind::ContentSnapshot, None, None)
        };
        let base_snapshot_digest = manifest_digest(&manifest, &kind);
        let snapshot_id = format!("snapshot-{}", &base_snapshot_digest[..24]);
        let transfer = Self {
            schema_version: 1,
            workspace_id: workspace_id.to_string(),
            snapshot_id,
            base_snapshot_digest,
            kind,
            manifest,
            objects,
            object_hashes: BTreeSet::new(),
            tracked_diff,
            git_bundle,
            tracked_diff_hash: None,
            git_bundle_hash: None,
            policy: WorkspacePolicy::Cached,
            limits,
        };
        transfer.validate()?;
        Ok(transfer)
    }

    pub fn validate(&self) -> Result<(), TargetError> {
        if self.schema_version != 1 || !valid_id(&self.workspace_id) || !valid_id(&self.snapshot_id)
        {
            return Err(TargetError::workspace_transfer_failed(
                "invalid workspace transfer identity",
            ));
        }
        if !valid_digest(&self.base_snapshot_digest) {
            return Err(TargetError::workspace_transfer_failed(
                "invalid workspace snapshot digest",
            ));
        }
        if self.manifest.len() > self.limits.max_files {
            return Err(TargetError::workspace_transfer_failed(
                "manifest exceeds file limit",
            ));
        }
        let objects = self
            .objects
            .iter()
            .map(|object| (object.sha256.as_str(), object))
            .collect::<HashMap<_, _>>();
        let mut total = 0u64;
        let mut paths = BTreeSet::new();
        let mut normalized = BTreeSet::new();
        for entry in &self.manifest {
            validate_relative_path(&entry.relative_path, self.limits.max_path_bytes)?;
            if !matches!(entry.file_type.as_str(), "file" | "directory" | "symlink") {
                return Err(TargetError::workspace_transfer_failed(format!(
                    "unsupported manifest file type for {}",
                    entry.relative_path
                )));
            }
            if !paths.insert(entry.relative_path.clone())
                || !normalized.insert(normalized_path(&entry.relative_path))
            {
                return Err(TargetError::workspace_transfer_failed(
                    "manifest contains a path collision",
                ));
            }
            if entry.size > self.limits.max_file_bytes
                || total.saturating_add(entry.size) > self.limits.max_total_bytes
            {
                return Err(TargetError::workspace_transfer_failed(
                    "manifest exceeds byte limits",
                ));
            }
            total = total.saturating_add(entry.size);
            if entry.file_type == "file" || entry.file_type == "symlink" {
                let object = objects.get(entry.sha256.as_str()).ok_or_else(|| {
                    TargetError::workspace_transfer_failed(format!(
                        "missing object {}",
                        entry.sha256
                    ))
                })?;
                if digest(&object.bytes) != entry.sha256 || object.bytes.len() as u64 != entry.size
                {
                    return Err(TargetError::workspace_transfer_failed(format!(
                        "object digest mismatch for {}",
                        entry.relative_path
                    )));
                }
            }
            if entry.file_type == "symlink" {
                validate_symlink_target(
                    entry.symlink_target.as_deref().unwrap_or_default(),
                    self.limits.max_symlink_depth,
                )?;
            }
        }
        if let Some(diff) = &self.tracked_diff {
            if digest(&diff.bytes) != diff.sha256
                || diff.bytes.len() as u64 > self.limits.max_total_bytes
            {
                return Err(TargetError::workspace_transfer_failed(
                    "tracked diff digest mismatch",
                ));
            }
        }
        if let Some(bundle) = &self.git_bundle {
            if digest(&bundle.bytes) != bundle.sha256
                || bundle.bytes.len() as u64 > self.limits.max_total_bytes
            {
                return Err(TargetError::workspace_transfer_failed(
                    "Git bundle digest or size is invalid",
                ));
            }
        }
        Ok(())
    }

    pub fn manifest_request(&self) -> WorkspaceManifestRequest {
        let object_hashes = if self.object_hashes.is_empty() {
            self.objects
                .iter()
                .map(|object| object.sha256.clone())
                .collect::<BTreeSet<_>>()
        } else {
            self.object_hashes.clone()
        }
        .into_iter()
        .chain(
            self.tracked_diff
                .as_ref()
                .map(|object| object.sha256.clone())
                .or_else(|| self.tracked_diff_hash.clone()),
        )
        .chain(
            self.git_bundle
                .as_ref()
                .map(|object| object.sha256.clone())
                .or_else(|| self.git_bundle_hash.clone()),
        )
        .collect();
        WorkspaceManifestRequest {
            schema_version: self.schema_version,
            workspace_id: self.workspace_id.clone(),
            snapshot_id: self.snapshot_id.clone(),
            base_snapshot_digest: self.base_snapshot_digest.clone(),
            kind: self.kind.clone(),
            manifest: self.manifest.clone(),
            object_hashes,
            tracked_diff_hash: self
                .tracked_diff
                .as_ref()
                .map(|object| object.sha256.clone())
                .or_else(|| self.tracked_diff_hash.clone()),
            git_bundle_hash: self
                .git_bundle
                .as_ref()
                .map(|object| object.sha256.clone())
                .or_else(|| self.git_bundle_hash.clone()),
            limits: self.limits.clone(),
        }
    }

    pub fn all_objects(&self) -> impl Iterator<Item = &WorkspaceObject> {
        self.objects
            .iter()
            .chain(self.tracked_diff.iter())
            .chain(self.git_bundle.iter())
    }

    pub fn missing_objects_for(&self, available: &BTreeSet<String>) -> Vec<WorkspaceObject> {
        self.all_objects()
            .filter(|object| !available.contains(&object.sha256))
            .cloned()
            .collect()
    }

    pub fn missing_objects<'a>(&'a self, hashes: &BTreeSet<String>) -> Vec<&'a WorkspaceObject> {
        self.objects
            .iter()
            .filter(|object| !hashes.contains(&object.sha256))
            .collect()
    }

    pub fn materialize(&self, destination: &Path) -> Result<(), TargetError> {
        self.validate()?;
        if destination.exists() {
            return Err(TargetError::workspace_transfer_failed(
                "workspace destination already exists",
            ));
        }
        fs::create_dir_all(destination)?;
        let is_git = matches!(
            self.kind,
            WorkspaceTransferKind::CleanGit { .. } | WorkspaceTransferKind::DirtyGit { .. }
        );
        if is_git {
            let bundle = self.git_bundle.as_ref().ok_or_else(|| {
                TargetError::workspace_transfer_failed(
                    "Git transfer omitted the exact base commit bundle",
                )
            })?;
            let bundle_path = destination.join(".little-monkey-base.bundle");
            fs::write(&bundle_path, &bundle.bytes)?;
            command_output("git", &["init", "-q"], Some(destination))?;
            let base_commit = match &self.kind {
                WorkspaceTransferKind::CleanGit { base_commit, .. }
                | WorkspaceTransferKind::DirtyGit { base_commit, .. } => base_commit,
                WorkspaceTransferKind::ContentSnapshot => unreachable!(),
            };
            command_output(
                "git",
                &[
                    "fetch",
                    "-q",
                    bundle_path.to_string_lossy().as_ref(),
                    base_commit,
                ],
                Some(destination),
            )?;
            command_output(
                "git",
                &["checkout", "-q", "--detach", base_commit],
                Some(destination),
            )?;
            let _ = fs::remove_file(&bundle_path);
        }
        let object_map = self
            .objects
            .iter()
            .map(|object| (object.sha256.as_str(), object))
            .collect::<HashMap<_, _>>();
        for entry in &self.manifest {
            let path = safe_join(
                destination,
                &entry.relative_path,
                self.limits.max_path_bytes,
            )?;
            match entry.file_type.as_str() {
                "directory" => fs::create_dir_all(&path)?,
                "file" => {
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&path, &object_map[entry.sha256.as_str()].bytes)?;
                    set_executable(&path, entry.executable)?;
                }
                "symlink" => {
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    create_symlink(entry.symlink_target.as_deref().unwrap_or_default(), &path)?;
                }
                _ => {
                    return Err(TargetError::workspace_transfer_failed(
                        "unsupported manifest file type",
                    ))
                }
            }
        }
        Ok(())
    }
}

fn manifest_digest(manifest: &[WorkspaceManifestEntry], kind: &WorkspaceTransferKind) -> String {
    let payload = serde_json::to_vec(&(manifest, kind)).unwrap_or_default();
    digest(&payload)
}

fn validate_relative_path(value: &str, max_bytes: usize) -> Result<(), TargetError> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.starts_with('/')
        || value.starts_with('\\')
        || value.contains('\0')
    {
        return Err(TargetError::workspace_transfer_failed(format!(
            "unsafe workspace path '{value}'"
        )));
    }
    let path = Path::new(value);
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(TargetError::workspace_transfer_failed(format!(
            "unsafe workspace path '{value}'"
        )));
    }
    Ok(())
}

fn validate_symlink_target(value: &str, max_depth: usize) -> Result<(), TargetError> {
    if value.is_empty()
        || value.len() > MAX_TRANSFER_PATH_BYTES
        || value.starts_with('/')
        || value.starts_with('\\')
        || Path::new(value).is_absolute()
    {
        return Err(TargetError::workspace_transfer_failed(
            "unsafe symlink target",
        ));
    }
    let mut depth = 0usize;
    for component in Path::new(value).components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(TargetError::workspace_transfer_failed(
                "symlink escapes workspace",
            ));
        }
        if matches!(component, Component::Normal(_)) {
            depth += 1;
        }
    }
    if depth > max_depth {
        return Err(TargetError::workspace_transfer_failed(
            "symlink depth exceeds transfer limit",
        ));
    }
    Ok(())
}

fn safe_join(root: &Path, relative: &str, max_bytes: usize) -> Result<PathBuf, TargetError> {
    validate_relative_path(relative, max_bytes)?;
    let path = root.join(relative);
    if !path.starts_with(root) {
        return Err(TargetError::workspace_transfer_failed(
            "workspace path escapes destination",
        ));
    }
    Ok(path)
}

fn safe_apply_join(root: &Path, relative: &str) -> Result<PathBuf, TargetError> {
    let path = safe_join(root, relative, MAX_TRANSFER_PATH_BYTES)?;
    let canonical_root = root
        .canonicalize()
        .map_err(|error| TargetError::WorkspaceConflict(error.to_string()))?;
    if let Some(parent) = path.parent() {
        if parent.exists() {
            let canonical_parent = parent
                .canonicalize()
                .map_err(|error| TargetError::WorkspaceConflict(error.to_string()))?;
            if !canonical_parent.starts_with(&canonical_root) {
                return Err(TargetError::WorkspaceConflict(format!(
                    "result path escapes workspace: {relative}"
                )));
            }
        }
    }
    Ok(path)
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn set_executable(path: &Path, executable: bool) -> Result<(), TargetError> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    let mode = permissions.mode();
    permissions.set_mode(if executable {
        mode | 0o111
    } else {
        mode & !0o111
    });
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_: &Path, _: bool) -> Result<(), TargetError> {
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &str, path: &Path) -> Result<(), TargetError> {
    std::os::unix::fs::symlink(target, path).map_err(TargetError::from)
}

#[cfg(windows)]
fn create_symlink(target: &str, path: &Path) -> Result<(), TargetError> {
    std::os::windows::fs::symlink_file(target, path).map_err(TargetError::from)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceHandle {
    pub workspace_id: String,
    pub snapshot_id: String,
    pub path: PathBuf,
    pub policy: WorkspacePolicy,
    pub base_snapshot_digest: String,
    #[serde(skip)]
    pub base_transfer: Option<WorkspaceTransfer>,
}

pub fn app_owned_workspace(
    runner_data: &Path,
    workspace_id: &str,
    snapshot_id: &str,
) -> Result<PathBuf, TargetError> {
    if !valid_id(workspace_id) || !valid_id(snapshot_id) {
        return Err(TargetError::invalid("invalid workspace identity"));
    }
    let root = runner_data
        .join("workspaces")
        .join(workspace_id)
        .join(snapshot_id);
    if !root.starts_with(runner_data) {
        return Err(TargetError::invalid("workspace path escapes runner data"));
    }
    Ok(root)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactDescriptor {
    pub artifact_id: String,
    pub label: String,
    pub media_type: String,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceResultFile {
    pub path: String,
    pub sha256: String,
    #[serde(with = "serde_bytes")]
    pub bytes: Vec<u8>,
    pub executable: bool,
    #[serde(default = "default_result_file_type")]
    pub file_type: String,
    #[serde(default)]
    pub symlink_target: Option<String>,
}

fn default_result_file_type() -> String {
    "file".to_string()
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationEvidence {
    pub label: String,
    pub command: Option<String>,
    pub passed: bool,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceResult {
    pub base_snapshot_digest: String,
    pub resulting_snapshot_digest: String,
    #[serde(with = "serde_bytes")]
    pub git_diff: Vec<u8>,
    pub new_files: Vec<WorkspaceResultFile>,
    pub deleted_files: Vec<String>,
    pub binary_changes: Vec<String>,
    pub artifacts: Vec<ArtifactDescriptor>,
    pub verification_evidence: Vec<VerificationEvidence>,
}

pub fn workspace_result_id(result: &WorkspaceResult) -> Result<String, TargetError> {
    result.validate()?;
    let bytes = serde_json::to_vec(result)
        .map_err(|error| TargetError::result_retrieval_failed(error.to_string()))?;
    Ok(format!("result-{}", &digest(&bytes)[..32]))
}

pub fn persist_workspace_result(
    data_dir: &Path,
    result: &WorkspaceResult,
) -> Result<String, TargetError> {
    let id = workspace_result_id(result)?;
    let directory = data_dir.join("execution-results");
    fs::create_dir_all(&directory)?;
    let bytes = serde_json::to_vec_pretty(result)
        .map_err(|error| TargetError::result_retrieval_failed(error.to_string()))?;
    let path = directory.join(format!("{id}.json"));
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, bytes)?;
    fs::rename(temporary, path)?;
    Ok(id)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchResult {
    pub kind: String,
    pub base_snapshot_digest: String,
    #[serde(with = "serde_bytes")]
    pub patch: Vec<u8>,
    pub patch_sha256: String,
}

impl PatchResult {
    pub fn validate(&self) -> Result<(), TargetError> {
        if self.kind != "git_patch" || !valid_digest(&self.base_snapshot_digest) {
            return Err(TargetError::result_retrieval_failed(
                "invalid persisted patch result",
            ));
        }
        if self.patch.is_empty() || self.patch.len() as u64 > MAX_TRANSFER_TOTAL_BYTES {
            return Err(TargetError::result_retrieval_failed(
                "persisted patch result exceeds transfer bounds",
            ));
        }
        if digest(&self.patch) != self.patch_sha256 {
            return Err(TargetError::result_retrieval_failed(
                "persisted patch digest mismatch",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "result", rename_all = "camelCase")]
pub enum PersistedExecutionResult {
    Workspace(WorkspaceResult),
    Patch(PatchResult),
}

pub fn persist_patch_result(
    data_dir: &Path,
    base_snapshot_digest: &str,
    patch: Vec<u8>,
) -> Result<String, TargetError> {
    let result = PatchResult {
        kind: "git_patch".to_string(),
        base_snapshot_digest: base_snapshot_digest.to_string(),
        patch_sha256: digest(&patch),
        patch,
    };
    result.validate()?;
    let id = format!(
        "result-{}",
        &digest(
            &serde_json::to_vec(&result)
                .map_err(|error| TargetError::result_retrieval_failed(error.to_string()))?
        )[..32]
    );
    let directory = data_dir.join("execution-results");
    fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{id}.json"));
    let temporary = path.with_extension("json.tmp");
    fs::write(
        &temporary,
        serde_json::to_vec_pretty(&result)
            .map_err(|error| TargetError::result_retrieval_failed(error.to_string()))?,
    )?;
    fs::rename(temporary, path)?;
    Ok(id)
}

pub fn load_workspace_result(
    data_dir: &Path,
    result_id: &str,
) -> Result<WorkspaceResult, TargetError> {
    if !valid_id(result_id) {
        return Err(TargetError::invalid("result id is invalid"));
    }
    let result: WorkspaceResult = serde_json::from_slice(&fs::read(
        data_dir
            .join("execution-results")
            .join(format!("{result_id}.json")),
    )?)
    .map_err(|error| TargetError::result_retrieval_failed(error.to_string()))?;
    result.validate()?;
    Ok(result)
}

pub fn load_execution_result(
    data_dir: &Path,
    result_id: &str,
) -> Result<PersistedExecutionResult, TargetError> {
    if !valid_id(result_id) {
        return Err(TargetError::invalid("result id is invalid"));
    }
    let bytes = fs::read(
        data_dir
            .join("execution-results")
            .join(format!("{result_id}.json")),
    )?;
    if let Ok(result) = serde_json::from_slice::<WorkspaceResult>(&bytes) {
        result.validate()?;
        return Ok(PersistedExecutionResult::Workspace(result));
    }
    let result: PatchResult = serde_json::from_slice(&bytes)
        .map_err(|error| TargetError::result_retrieval_failed(error.to_string()))?;
    result.validate()?;
    Ok(PersistedExecutionResult::Patch(result))
}

pub fn apply_execution_result(
    root: &Path,
    result: &PersistedExecutionResult,
) -> Result<(), TargetError> {
    match result {
        PersistedExecutionResult::Workspace(result) => {
            apply_workspace_result(root, &result.base_snapshot_digest, result)
        }
        PersistedExecutionResult::Patch(result) => {
            result.validate()?;
            let current =
                WorkspaceTransfer::from_workspace(root, "apply-check")?.base_snapshot_digest;
            if current != result.base_snapshot_digest {
                return Err(TargetError::WorkspaceConflict(
                    "local workspace changed since the remote snapshot".to_string(),
                ));
            }
            let mut check = Command::new("git")
                .args(["apply", "--check", "--binary", "--"])
                .current_dir(root)
                .stdin(Stdio::piped())
                .spawn()?;
            check
                .stdin
                .take()
                .ok_or_else(|| TargetError::Io("could not open git apply check stdin".to_string()))?
                .write_all(&result.patch)?;
            if !check.wait()?.success() {
                return Err(TargetError::WorkspaceConflict(
                    "git apply check failed".to_string(),
                ));
            }
            let mut apply = Command::new("git")
                .args(["apply", "--binary", "--"])
                .current_dir(root)
                .stdin(Stdio::piped())
                .spawn()?;
            apply
                .stdin
                .take()
                .ok_or_else(|| TargetError::Io("could not open git apply stdin".to_string()))?
                .write_all(&result.patch)?;
            if !apply.wait()?.success() {
                return Err(TargetError::WorkspaceConflict(
                    "git apply failed".to_string(),
                ));
            }
            Ok(())
        }
    }
}

pub fn discard_workspace_result(data_dir: &Path, result_id: &str) -> Result<(), TargetError> {
    if !valid_id(result_id) {
        return Err(TargetError::invalid("result id is invalid"));
    }
    let path = data_dir
        .join("execution-results")
        .join(format!("{result_id}.json"));
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

impl WorkspaceResult {
    pub fn validate(&self) -> Result<(), TargetError> {
        if !valid_digest(&self.base_snapshot_digest)
            || !valid_digest(&self.resulting_snapshot_digest)
        {
            return Err(TargetError::result_retrieval_failed(
                "invalid workspace result snapshot digest",
            ));
        }
        if self.git_diff.len() as u64 > MAX_TRANSFER_TOTAL_BYTES {
            return Err(TargetError::result_retrieval_failed(
                "workspace result diff exceeds transfer bounds",
            ));
        }
        let mut file_bytes = 0u64;
        let mut paths = BTreeSet::new();
        let mut normalized_paths = BTreeSet::new();
        for path in &self.deleted_files {
            validate_relative_path(path, MAX_TRANSFER_PATH_BYTES)?;
            if !paths.insert(path.clone()) || !normalized_paths.insert(normalized_path(path)) {
                return Err(TargetError::result_retrieval_failed(
                    "workspace result contains a path collision",
                ));
            }
        }
        for file in &self.new_files {
            validate_relative_path(&file.path, MAX_TRANSFER_PATH_BYTES)?;
            if !paths.insert(file.path.clone())
                || !normalized_paths.insert(normalized_path(&file.path))
            {
                return Err(TargetError::result_retrieval_failed(
                    "workspace result contains a path collision",
                ));
            }
            if digest(&file.bytes) != file.sha256 {
                return Err(TargetError::result_retrieval_failed(format!(
                    "result digest mismatch for {}",
                    file.path
                )));
            }
            if file.bytes.len() as u64 > MAX_TRANSFER_FILE_BYTES
                || file_bytes.saturating_add(file.bytes.len() as u64) > MAX_TRANSFER_TOTAL_BYTES
            {
                return Err(TargetError::result_retrieval_failed(
                    "workspace result files exceed transfer bounds",
                ));
            }
            file_bytes = file_bytes.saturating_add(file.bytes.len() as u64);
            if !matches!(file.file_type.as_str(), "file" | "symlink") {
                return Err(TargetError::result_retrieval_failed(format!(
                    "unsupported result file type for {}",
                    file.path
                )));
            }
            if file.file_type == "symlink" {
                validate_symlink_target(file.symlink_target.as_deref().unwrap_or_default(), 32)?;
            }
        }
        for path in &self.binary_changes {
            validate_relative_path(path, MAX_TRANSFER_PATH_BYTES)?;
        }
        let mut artifact_bytes = 0u64;
        for artifact in &self.artifacts {
            if !valid_id(&artifact.artifact_id)
                || artifact.label.trim().is_empty()
                || artifact.media_type.trim().is_empty()
            {
                return Err(TargetError::result_retrieval_failed(
                    "invalid artifact descriptor",
                ));
            }
            if !valid_digest(&artifact.sha256) || artifact.size_bytes > MAX_TRANSFER_FILE_BYTES {
                return Err(TargetError::result_retrieval_failed(
                    "invalid artifact bounds or digest",
                ));
            }
            artifact_bytes = artifact_bytes.saturating_add(artifact.size_bytes);
            if artifact_bytes > MAX_TRANSFER_TOTAL_BYTES {
                return Err(TargetError::result_retrieval_failed(
                    "artifact result exceeds transfer bounds",
                ));
            }
        }
        Ok(())
    }
}

/// Build a result as a bounded snapshot delta. Files are included rather than
/// blindly replaying `git diff`, because the base may itself contain local
/// modifications that were transferred to the executor before the run.
pub fn workspace_result_from_workspace(
    base: &WorkspaceTransfer,
    root: &Path,
) -> Result<WorkspaceResult, TargetError> {
    let current = WorkspaceTransfer::from_workspace(root, &base.workspace_id)?;
    let base_entries = base
        .manifest
        .iter()
        .map(|entry| (entry.relative_path.as_str(), entry))
        .collect::<HashMap<_, _>>();
    let current_objects = current
        .objects
        .iter()
        .map(|object| (object.sha256.as_str(), object))
        .collect::<HashMap<_, _>>();
    let mut new_files = Vec::new();
    let mut current_paths = BTreeSet::new();
    for entry in &current.manifest {
        current_paths.insert(entry.relative_path.clone());
        if entry.file_type == "directory" {
            continue;
        }
        let unchanged = base_entries
            .get(entry.relative_path.as_str())
            .is_some_and(|old| old.sha256 == entry.sha256 && old.file_type == entry.file_type);
        if unchanged {
            continue;
        }
        let bytes = current_objects
            .get(entry.sha256.as_str())
            .map(|object| object.bytes.clone())
            .unwrap_or_default();
        new_files.push(WorkspaceResultFile {
            path: entry.relative_path.clone(),
            sha256: entry.sha256.clone(),
            bytes,
            executable: entry.executable,
            file_type: entry.file_type.clone(),
            symlink_target: entry.symlink_target.clone(),
        });
    }
    let deleted_files = base
        .manifest
        .iter()
        .filter(|entry| {
            entry.file_type != "directory" && !current_paths.contains(&entry.relative_path)
        })
        .map(|entry| entry.relative_path.clone())
        .collect::<Vec<_>>();
    let git_diff = match current.kind {
        WorkspaceTransferKind::CleanGit { .. } | WorkspaceTransferKind::DirtyGit { .. } => {
            command_output(
                "git",
                &["diff", "HEAD", "--binary", "--no-color"],
                Some(root),
            )
            .unwrap_or_default()
        }
        WorkspaceTransferKind::ContentSnapshot => Vec::new(),
    };
    let binary_changes = new_files
        .iter()
        .filter(|file| std::str::from_utf8(&file.bytes).is_err())
        .map(|file| file.path.clone())
        .collect();
    let artifacts = new_files
        .iter()
        .map(|file| ArtifactDescriptor {
            artifact_id: format!("workspace-{}", file.sha256),
            label: file.path.clone(),
            media_type: if file.file_type == "symlink" {
                "inode/symlink".into()
            } else {
                "application/octet-stream".into()
            },
            sha256: file.sha256.clone(),
            size_bytes: file.bytes.len() as u64,
        })
        .collect();
    Ok(WorkspaceResult {
        base_snapshot_digest: base.base_snapshot_digest.clone(),
        resulting_snapshot_digest: current.base_snapshot_digest,
        git_diff,
        new_files,
        deleted_files,
        binary_changes,
        artifacts,
        verification_evidence: Vec::new(),
    })
}

pub fn apply_workspace_result(
    root: &Path,
    expected_base_digest: &str,
    result: &WorkspaceResult,
) -> Result<(), TargetError> {
    result.validate()?;
    let current = WorkspaceTransfer::from_workspace(root, "apply-check")?.base_snapshot_digest;
    if current != expected_base_digest || current != result.base_snapshot_digest {
        return Err(TargetError::WorkspaceConflict(
            "local workspace changed since the remote snapshot".to_string(),
        ));
    }
    if !result.git_diff.is_empty() {
        let mut check = Command::new("git")
            .args(["apply", "--check", "--binary", "--"])
            .current_dir(root)
            .stdin(Stdio::piped())
            .spawn()?;
        check
            .stdin
            .take()
            .ok_or_else(|| TargetError::Io("could not open git apply check stdin".to_string()))?
            .write_all(&result.git_diff)?;
        if !check.wait()?.success() {
            return Err(TargetError::WorkspaceConflict(
                "git apply check failed".to_string(),
            ));
        }
        let mut apply = Command::new("git")
            .args(["apply", "--binary", "--"])
            .current_dir(root)
            .stdin(Stdio::piped())
            .spawn()?;
        apply
            .stdin
            .take()
            .ok_or_else(|| TargetError::Io("could not open git apply stdin".to_string()))?
            .write_all(&result.git_diff)?;
        if !apply.wait()?.success() {
            return Err(TargetError::WorkspaceConflict(
                "git apply failed".to_string(),
            ));
        }
    }
    for path in &result.deleted_files {
        let path = safe_apply_join(root, path)?;
        if path.is_file() || path.is_symlink() {
            fs::remove_file(path)?;
        } else if path.is_dir() {
            fs::remove_dir_all(path)?;
        }
    }
    for file in &result.new_files {
        let path = safe_apply_join(root, &file.path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        match file.file_type.as_str() {
            "file" => {
                if fs::symlink_metadata(&path)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    fs::remove_file(&path)?;
                }
                fs::write(&path, &file.bytes)?;
                set_executable(&path, file.executable)?;
            }
            "symlink" => {
                if fs::symlink_metadata(&path).is_ok() {
                    fs::remove_file(&path)?;
                }
                validate_symlink_target(file.symlink_target.as_deref().unwrap_or_default(), 32)?;
                create_symlink(file.symlink_target.as_deref().unwrap_or_default(), &path)?;
            }
            _ => {
                return Err(TargetError::result_retrieval_failed(
                    "unsupported result file type",
                ))
            }
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRequest {
    pub run_id: String,
    pub target: ExecutionTargetSnapshot,
    pub required_capabilities: RequiredCapabilities,
    pub workspace: WorkspaceHandle,
    pub command: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub wall_time_ms: u64,
    pub max_artifact_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_transfer: Option<WorkspaceTransfer>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_files: Vec<WorkspaceResultFile>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetRunHandle {
    pub run_id: String,
    pub remote_id: String,
    pub target_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetRunStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Lost,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetEvent {
    pub sequence: u64,
    pub run_id: String,
    pub kind: String,
    pub message: String,
    pub at_ms: u64,
}

/// Stable extension seam used by the coordinator. Implementations must probe
/// and negotiate before `submit_run`; callers must not infer support from kind.
pub trait ExecutionTarget: Send + Sync {
    fn probe(&self) -> Result<ExecutionTargetSnapshot, TargetError>;
    fn capabilities(&self) -> Result<TargetCapabilities, TargetError>;
    fn prepare_workspace(
        &self,
        transfer: &WorkspaceTransfer,
        policy: WorkspacePolicy,
    ) -> Result<WorkspaceHandle, TargetError>;
    fn submit_run(&self, request: RunRequest) -> Result<TargetRunHandle, TargetError>;
    fn attach_run(&self, run_id: &str) -> Result<TargetRunHandle, TargetError> {
        if !valid_id(run_id) {
            return Err(TargetError::invalid("run id is invalid"));
        }
        Ok(TargetRunHandle {
            run_id: run_id.to_string(),
            remote_id: run_id.to_string(),
            target_id: self.probe()?.identity.stable_id,
        })
    }
    fn events(
        &self,
        handle: &TargetRunHandle,
        after_sequence: u64,
    ) -> Result<Vec<TargetEvent>, TargetError>;
    fn status(&self, handle: &TargetRunHandle) -> Result<TargetRunStatus, TargetError>;
    fn cancel(&self, handle: &TargetRunHandle) -> Result<(), TargetError>;
    fn pause(&self, handle: &TargetRunHandle) -> Result<(), TargetError>;
    fn resume(&self, handle: &TargetRunHandle) -> Result<(), TargetError>;
    fn artifacts(&self, handle: &TargetRunHandle) -> Result<Vec<ArtifactDescriptor>, TargetError>;
    fn workspace_result(&self, handle: &TargetRunHandle) -> Result<WorkspaceResult, TargetError>;
    fn cleanup(&self, workspace: &WorkspaceHandle) -> Result<(), TargetError>;
}

#[derive(Clone)]
pub struct LocalExecutionTarget {
    identity: TargetIdentity,
}

impl LocalExecutionTarget {
    pub fn new() -> Self {
        Self {
            identity: TargetIdentity {
                stable_id: "local".into(),
                display_name: "Local".into(),
                kind: ExecutionTargetKind::Local,
                endpoint: None,
                verified_identity: None,
                platform: std::env::consts::OS.into(),
                runner_version: env!("CARGO_PKG_VERSION").into(),
                protocol_version: EXECUTION_PROTOCOL_VERSION,
                capabilities: TargetCapabilities::local(),
                last_successful_probe_ms: None,
                trust_state: TargetTrustState::Verified,
            },
        }
    }
}

impl Default for LocalExecutionTarget {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecutionTarget for LocalExecutionTarget {
    fn probe(&self) -> Result<ExecutionTargetSnapshot, TargetError> {
        ExecutionTargetSnapshot::freeze(self.identity.clone(), now_ms())
    }
    fn capabilities(&self) -> Result<TargetCapabilities, TargetError> {
        Ok(self.identity.capabilities.clone())
    }
    fn prepare_workspace(
        &self,
        transfer: &WorkspaceTransfer,
        policy: WorkspacePolicy,
    ) -> Result<WorkspaceHandle, TargetError> {
        Ok(WorkspaceHandle {
            workspace_id: transfer.workspace_id.clone(),
            snapshot_id: transfer.snapshot_id.clone(),
            path: PathBuf::from("."),
            policy,
            base_snapshot_digest: transfer.base_snapshot_digest.clone(),
            base_transfer: Some(transfer.clone()),
        })
    }
    fn submit_run(&self, _: RunRequest) -> Result<TargetRunHandle, TargetError> {
        Err(TargetError::unsupported(
            "local execution is owned by the existing agent coordinator",
        ))
    }
    fn events(&self, _: &TargetRunHandle, _: u64) -> Result<Vec<TargetEvent>, TargetError> {
        Ok(Vec::new())
    }
    fn status(&self, _: &TargetRunHandle) -> Result<TargetRunStatus, TargetError> {
        Err(TargetError::unsupported(
            "local execution status is owned by the existing run ledger",
        ))
    }
    fn cancel(&self, _: &TargetRunHandle) -> Result<(), TargetError> {
        Err(TargetError::unsupported(
            "local cancellation is owned by the existing run ledger",
        ))
    }
    fn pause(&self, _: &TargetRunHandle) -> Result<(), TargetError> {
        Err(TargetError::unsupported(
            "local pause is owned by the existing run ledger",
        ))
    }
    fn resume(&self, _: &TargetRunHandle) -> Result<(), TargetError> {
        Err(TargetError::unsupported(
            "local resume is owned by the existing run ledger",
        ))
    }
    fn artifacts(&self, _: &TargetRunHandle) -> Result<Vec<ArtifactDescriptor>, TargetError> {
        Ok(Vec::new())
    }
    fn workspace_result(&self, _: &TargetRunHandle) -> Result<WorkspaceResult, TargetError> {
        Err(TargetError::unsupported(
            "local result is already in the run ledger",
        ))
    }
    fn cleanup(&self, _: &WorkspaceHandle) -> Result<(), TargetError> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct DockerExecutionTarget {
    identity: TargetIdentity,
    docker_binary: PathBuf,
    image: String,
    runner_data: PathBuf,
    processes: Arc<Mutex<HashMap<String, String>>>,
    workspaces: Arc<Mutex<HashMap<String, WorkspaceHandle>>>,
}

#[derive(Clone, Serialize, Deserialize)]
struct DockerRunRecord {
    run_id: String,
    remote_id: String,
    workspace: WorkspaceHandle,
    base_transfer: WorkspaceTransfer,
    #[serde(default)]
    transient_inputs: Vec<String>,
}

impl DockerExecutionTarget {
    pub fn new(
        stable_id: String,
        display_name: String,
        image: String,
        runner_data: PathBuf,
    ) -> Result<Self, TargetError> {
        if !valid_id(&stable_id) || image.trim().is_empty() || image.starts_with('-') {
            return Err(TargetError::invalid("invalid Docker target configuration"));
        }
        Ok(Self {
            identity: TargetIdentity {
                stable_id,
                display_name,
                kind: ExecutionTargetKind::Docker,
                endpoint: Some("docker://local".into()),
                verified_identity: None,
                platform: "oci".into(),
                runner_version: env!("CARGO_PKG_VERSION").into(),
                protocol_version: EXECUTION_PROTOCOL_VERSION,
                capabilities: TargetCapabilities::docker(),
                last_successful_probe_ms: None,
                trust_state: TargetTrustState::Unverified,
            },
            docker_binary: PathBuf::from("docker"),
            image,
            runner_data,
            processes: Arc::new(Mutex::new(HashMap::new())),
            workspaces: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn container_name(run_id: &str) -> String {
        format!(
            "little-monkey-run-{}",
            run_id
                .chars()
                .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
                .take(48)
                .collect::<String>()
        )
    }

    fn state_path(&self) -> PathBuf {
        self.runner_data.join("docker-runs.json")
    }

    fn load_records(&self) -> Result<BTreeMap<String, DockerRunRecord>, TargetError> {
        let path = self.state_path();
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        serde_json::from_slice(&fs::read(path)?).map_err(|error| TargetError::Io(error.to_string()))
    }

    fn save_records(&self, records: &BTreeMap<String, DockerRunRecord>) -> Result<(), TargetError> {
        fs::create_dir_all(&self.runner_data)?;
        let path = self.state_path();
        let temporary = path.with_extension("json.tmp");
        fs::write(
            &temporary,
            serde_json::to_vec_pretty(records)
                .map_err(|error| TargetError::Io(error.to_string()))?,
        )?;
        fs::rename(temporary, path)?;
        Ok(())
    }

    fn remove_transient_inputs(root: &Path, inputs: &[String]) {
        for relative in inputs {
            if let Ok(path) = safe_apply_join(root, relative) {
                let _ = fs::remove_file(path);
            }
        }
    }
}

impl ExecutionTarget for DockerExecutionTarget {
    fn probe(&self) -> Result<ExecutionTargetSnapshot, TargetError> {
        let output = Command::new(&self.docker_binary)
            .args(["version", "--format", "{{.Server.Version}}"])
            .output()
            .map_err(|error| TargetError::target_unreachable(error.to_string()))?;
        if !output.status.success() {
            return Err(TargetError::target_unreachable(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        let mut identity = self.identity.clone();
        let image = Command::new(&self.docker_binary)
            .args(["image", "inspect", "--format", "{{.Id}}", &self.image])
            .output()
            .map_err(|error| TargetError::target_unreachable(error.to_string()))?;
        if !image.status.success() {
            return Err(TargetError::target_unreachable(format!(
                "Docker image '{}' is unavailable: {}",
                self.image,
                String::from_utf8_lossy(&image.stderr).trim()
            )));
        }
        identity.verified_identity = Some(format!(
            "docker-server:{};image:{}",
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&image.stdout).trim()
        ));
        identity.trust_state = TargetTrustState::Verified;
        ExecutionTargetSnapshot::freeze(identity, now_ms())
    }
    fn capabilities(&self) -> Result<TargetCapabilities, TargetError> {
        Ok(self.identity.capabilities.clone())
    }
    fn prepare_workspace(
        &self,
        transfer: &WorkspaceTransfer,
        policy: WorkspacePolicy,
    ) -> Result<WorkspaceHandle, TargetError> {
        let path = app_owned_workspace(
            &self.runner_data,
            &transfer.workspace_id,
            &transfer.snapshot_id,
        )?;
        if !path.exists() {
            transfer.materialize(&path)?;
            transfer.mark_cached(&path)?;
        } else if !transfer.cached_matches(&path)? {
            return Err(TargetError::WorkspaceConflict(
                "cached Docker workspace does not match the requested snapshot".into(),
            ));
        }
        Ok(WorkspaceHandle {
            workspace_id: transfer.workspace_id.clone(),
            snapshot_id: transfer.snapshot_id.clone(),
            path,
            policy,
            base_snapshot_digest: transfer.base_snapshot_digest.clone(),
            base_transfer: Some(transfer.clone()),
        })
    }
    fn submit_run(&self, request: RunRequest) -> Result<TargetRunHandle, TargetError> {
        request.target.require(&request.required_capabilities)?;
        validate_environment(&request.environment)?;
        if request.command.is_empty()
            || request
                .command
                .iter()
                .any(|argument| argument.contains('\0'))
        {
            return Err(TargetError::invalid(
                "Docker run command is empty or contains NUL",
            ));
        }
        if request.input_files.len() > 1_024 {
            return Err(TargetError::workspace_transfer_failed(
                "Docker input file count exceeds transfer bounds",
            ));
        }
        let mut input_bytes = 0u64;
        let mut transient_inputs = Vec::new();
        for file in &request.input_files {
            if file.file_type != "file" {
                return Err(TargetError::invalid(
                    "Docker input files must be regular files",
                ));
            }
            if digest(&file.bytes) != file.sha256 {
                return Err(TargetError::invalid(format!(
                    "Docker input digest mismatch for {}",
                    file.path
                )));
            }
            if file.bytes.len() as u64 > MAX_TRANSFER_FILE_BYTES
                || input_bytes.saturating_add(file.bytes.len() as u64) > MAX_TRANSFER_TOTAL_BYTES
            {
                return Err(TargetError::workspace_transfer_failed(
                    "Docker input files exceed transfer bounds",
                ));
            }
            input_bytes = input_bytes.saturating_add(file.bytes.len() as u64);
            let input_path = safe_apply_join(&request.workspace.path, &file.path)?;
            if input_path.exists() || fs::symlink_metadata(&input_path).is_ok() {
                return Err(TargetError::workspace_conflict(format!(
                    "Docker input would overwrite workspace content: {}",
                    file.path
                )));
            }
            if let Some(parent) = input_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&input_path, &file.bytes)?;
            set_executable(&input_path, file.executable)?;
            transient_inputs.push(file.path.clone());
        }
        let name = Self::container_name(&request.run_id);
        let mut command = Command::new(&self.docker_binary);
        // Docker's default PID namespace is private. Do not pass the
        // non-portable `private` value: older Docker engines reject it even
        // though they already provide the required default.
        command.args(["run", "--detach", "--name", &name]);
        command.args([
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges=true",
        ]);
        command.args(["--pids-limit", "512"]);
        command.args(["--read-only"]);
        command.args(["--tmpfs", "/tmp:rw,nosuid,nodev,noexec,size=256m"]);
        if !request.target.identity.capabilities.outbound_network {
            command.args(["--network", "none"]);
        }
        command.args([
            "--cpus",
            &request
                .target
                .identity
                .capabilities
                .max_cpu_cores
                .map(|value| value.to_string())
                .unwrap_or_else(|| "2".into()),
        ]);
        if let Some(ram) = request.target.identity.capabilities.max_ram_bytes {
            command.args(["--memory", &ram.to_string()]);
        }
        command.args([
            "--mount",
            &format!(
                "type=bind,source={},target=/workspace",
                request.workspace.path.display()
            ),
            "-w",
            "/workspace",
        ]);
        command.args(["-e", "PATH=/usr/local/bin:/usr/bin:/bin"]);
        for (key, value) in &request.environment {
            if key
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
            {
                command.args(["-e", &format!("{key}={value}")]);
            }
        }
        command.args([&self.image]);
        command.args(&request.command);
        let output = match command.output() {
            Ok(output) => output,
            Err(error) => {
                Self::remove_transient_inputs(&request.workspace.path, &transient_inputs);
                return Err(TargetError::target_unreachable(error.to_string()));
            }
        };
        if !output.status.success() {
            Self::remove_transient_inputs(&request.workspace.path, &transient_inputs);
            return Err(TargetError::target_unreachable(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        let remote_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let base_transfer = match request.workspace.base_transfer.clone() {
            Some(base_transfer) => base_transfer,
            None => {
                Self::remove_transient_inputs(&request.workspace.path, &transient_inputs);
                let _ = Command::new(&self.docker_binary)
                    .args(["rm", "--force", &remote_id])
                    .output();
                return Err(TargetError::workspace_transfer_failed(
                    "Docker run omitted its base transfer",
                ));
            }
        };
        let record = DockerRunRecord {
            run_id: request.run_id.clone(),
            remote_id: remote_id.clone(),
            workspace: request.workspace.clone(),
            base_transfer,
            transient_inputs: transient_inputs.clone(),
        };
        let mut records = match self.load_records() {
            Ok(records) => records,
            Err(error) => {
                Self::remove_transient_inputs(&request.workspace.path, &transient_inputs);
                let _ = Command::new(&self.docker_binary)
                    .args(["rm", "--force", &remote_id])
                    .output();
                return Err(error);
            }
        };
        records.insert(request.run_id.clone(), record);
        if let Err(error) = self.save_records(&records) {
            Self::remove_transient_inputs(
                &request.workspace.path,
                &records[&request.run_id].transient_inputs,
            );
            let _ = Command::new(&self.docker_binary)
                .args(["rm", "--force", &remote_id])
                .output();
            return Err(error);
        }
        self.processes
            .lock()
            .map_err(|_| TargetError::Io("Docker process registry poisoned".into()))?
            .insert(request.run_id.clone(), remote_id.clone());
        self.workspaces
            .lock()
            .map_err(|_| TargetError::Io("Docker workspace registry poisoned".into()))?
            .insert(request.run_id.clone(), request.workspace.clone());
        let docker = self.docker_binary.clone();
        let container = remote_id.clone();
        let wall_time_ms = request.wall_time_ms;
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(wall_time_ms));
            let _ = Command::new(&docker)
                .args(["inspect", &container])
                .output()
                .and_then(|inspect| {
                    if inspect.status.success() {
                        Command::new(&docker)
                            .args(["rm", "--force", &container])
                            .output()
                            .map(|_| ())
                    } else {
                        Ok(())
                    }
                });
        });
        Ok(TargetRunHandle {
            run_id: request.run_id,
            remote_id,
            target_id: self.identity.stable_id.clone(),
        })
    }
    fn attach_run(&self, run_id: &str) -> Result<TargetRunHandle, TargetError> {
        let record = self
            .load_records()?
            .get(run_id)
            .cloned()
            .ok_or_else(|| TargetError::runner_lost("Docker run record was not found"))?;
        Ok(TargetRunHandle {
            run_id: run_id.to_string(),
            remote_id: record.remote_id,
            target_id: self.identity.stable_id.clone(),
        })
    }
    fn events(
        &self,
        handle: &TargetRunHandle,
        after_sequence: u64,
    ) -> Result<Vec<TargetEvent>, TargetError> {
        if after_sequence > 0 {
            return Ok(Vec::new());
        }
        let output = Command::new(&self.docker_binary)
            .args(["logs", &handle.remote_id])
            .output()
            .map_err(|error| TargetError::result_retrieval_failed(error.to_string()))?;
        if !output.status.success() {
            return Err(TargetError::result_retrieval_failed(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        let mut message = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty() {
            if !message.is_empty() {
                message.push('\n');
            }
            message.push_str(stderr.trim());
        }
        if let Ok(inspect) = Command::new(&self.docker_binary)
            .args(["inspect", "--format", "{{.State.Error}}", &handle.remote_id])
            .output()
        {
            let error = String::from_utf8_lossy(&inspect.stdout).trim().to_string();
            if !error.is_empty() {
                if !message.is_empty() {
                    message.push('\n');
                }
                message.push_str(&error);
            }
        }
        Ok(vec![TargetEvent {
            sequence: 1,
            run_id: handle.run_id.clone(),
            kind: "log".into(),
            message,
            at_ms: now_ms(),
        }])
    }
    fn status(&self, handle: &TargetRunHandle) -> Result<TargetRunStatus, TargetError> {
        let output = Command::new(&self.docker_binary)
            .args([
                "inspect",
                "--format",
                "{{.State.Status}} {{.State.ExitCode}} {{.State.OOMKilled}}",
                &handle.remote_id,
            ])
            .output()
            .map_err(|error| TargetError::runner_lost(error.to_string()))?;
        if !output.status.success() {
            return Ok(TargetRunStatus::Lost);
        }
        Ok(docker_status_from_inspect(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }
    fn cancel(&self, handle: &TargetRunHandle) -> Result<(), TargetError> {
        let output = Command::new(&self.docker_binary)
            .args(["rm", "--force", &handle.remote_id])
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(TargetError::runner_lost(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ))
        }
    }
    fn pause(&self, handle: &TargetRunHandle) -> Result<(), TargetError> {
        let output = Command::new(&self.docker_binary)
            .args(["pause", &handle.remote_id])
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(TargetError::runner_lost(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ))
        }
    }
    fn resume(&self, handle: &TargetRunHandle) -> Result<(), TargetError> {
        let output = Command::new(&self.docker_binary)
            .args(["unpause", &handle.remote_id])
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(TargetError::runner_lost(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ))
        }
    }
    fn artifacts(&self, _: &TargetRunHandle) -> Result<Vec<ArtifactDescriptor>, TargetError> {
        Ok(Vec::new())
    }
    fn workspace_result(&self, handle: &TargetRunHandle) -> Result<WorkspaceResult, TargetError> {
        let persisted = self.load_records()?.get(&handle.run_id).cloned();
        let workspace = self
            .workspaces
            .lock()
            .map_err(|_| TargetError::Io("Docker workspace registry poisoned".into()))?
            .get(&handle.run_id)
            .cloned()
            .or_else(|| persisted.as_ref().map(|record| record.workspace.clone()))
            .ok_or_else(|| {
                TargetError::result_retrieval_failed("Docker run workspace is no longer registered")
            })?;
        if let Some(record) = persisted.as_ref() {
            Self::remove_transient_inputs(&workspace.path, &record.transient_inputs);
        }
        let base = workspace
            .base_transfer
            .as_ref()
            .or_else(|| persisted.as_ref().map(|record| &record.base_transfer))
            .ok_or_else(|| {
                TargetError::result_retrieval_failed("Docker run omitted its workspace snapshot")
            })?;
        workspace_result_from_workspace(base, &workspace.path)
    }
    fn cleanup(&self, workspace: &WorkspaceHandle) -> Result<(), TargetError> {
        if matches!(workspace.policy, WorkspacePolicy::Ephemeral) && workspace.path.exists() {
            fs::remove_dir_all(&workspace.path).map_err(TargetError::from)?;
            let _ = fs::remove_file(WorkspaceTransfer::cache_marker_path(&workspace.path));
        }
        let mut records = self.load_records()?;
        let removed = records
            .iter()
            .filter(|(_, record)| {
                record.workspace.workspace_id == workspace.workspace_id
                    && record.workspace.snapshot_id == workspace.snapshot_id
            })
            .map(|(_, record)| record.remote_id.clone())
            .collect::<Vec<_>>();
        records.retain(|_, record| {
            record.workspace.workspace_id != workspace.workspace_id
                || record.workspace.snapshot_id != workspace.snapshot_id
        });
        for remote_id in removed {
            let _ = Command::new(&self.docker_binary)
                .args(["rm", "--force", &remote_id])
                .output();
        }
        self.save_records(&records)?;
        Ok(())
    }
}

fn docker_status_from_inspect(value: &str) -> TargetRunStatus {
    let fields = value.split_whitespace().collect::<Vec<_>>();
    let state = fields.first().copied().unwrap_or_default();
    let exit_code = fields.get(1).and_then(|value| value.parse::<i32>().ok());
    let oom_killed = fields.get(2).copied() == Some("true");
    match state {
        "created" => TargetRunStatus::Queued,
        "running" | "restarting" | "paused" => TargetRunStatus::Running,
        "exited" if exit_code == Some(0) => TargetRunStatus::Succeeded,
        "exited" => TargetRunStatus::Failed,
        "dead" if oom_killed => TargetRunStatus::Failed,
        "dead" => TargetRunStatus::Lost,
        _ => TargetRunStatus::Lost,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshRunnerConfig {
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub key_file: Option<PathBuf>,
    pub known_hosts: PathBuf,
    pub jump_host: Option<String>,
    pub runner_binary: String,
}

impl SshRunnerConfig {
    pub fn validate(&self) -> Result<(), TargetError> {
        if self.host.trim().is_empty()
            || self.host.chars().any(|character| character.is_whitespace())
        {
            return Err(TargetError::invalid("SSH host is invalid"));
        }
        if self.known_hosts.as_os_str().is_empty() || !self.known_hosts.is_absolute() {
            return Err(TargetError::invalid(
                "SSH known_hosts must be an absolute path",
            ));
        }
        if self.runner_binary.is_empty() || self.runner_binary.contains('/') {
            return Err(TargetError::invalid(
                "SSH runner binary must be a command name",
            ));
        }
        if let Some(path) = &self.key_file {
            if !path.is_absolute() {
                return Err(TargetError::invalid("SSH key reference must be absolute"));
            }
        }
        Ok(())
    }

    pub fn ssh_args(&self) -> Result<Vec<String>, TargetError> {
        self.validate()?;
        let mut args = vec![
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "StrictHostKeyChecking=yes".into(),
            "-o".into(),
            format!("UserKnownHostsFile={}", self.known_hosts.display()),
        ];
        if let Some(user) = &self.user {
            if user.is_empty()
                || user.contains('@')
                || user.chars().any(|character| character.is_whitespace())
            {
                return Err(TargetError::invalid("SSH user is invalid"));
            }
            args.extend(["-l".into(), user.clone()]);
        }
        if let Some(port) = self.port {
            args.extend(["-p".into(), port.to_string()]);
        }
        if let Some(key) = &self.key_file {
            args.extend(["-i".into(), key.to_string_lossy().into_owned()]);
        }
        if let Some(jump) = &self.jump_host {
            if jump.is_empty() || jump.contains(' ') {
                return Err(TargetError::invalid("SSH jump host is invalid"));
            }
            args.extend(["-J".into(), jump.clone()]);
        }
        args.push(self.host.clone());
        Ok(args)
    }
}

#[derive(Clone)]
pub struct SshRunnerTarget {
    identity: TargetIdentity,
    config: SshRunnerConfig,
    session: Arc<Mutex<Option<SshSession>>>,
}

struct SshSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl SshRunnerTarget {
    pub fn new(
        stable_id: String,
        display_name: String,
        config: SshRunnerConfig,
        runner_data: PathBuf,
    ) -> Result<Self, TargetError> {
        config.validate()?;
        if !valid_id(&stable_id) {
            return Err(TargetError::invalid("invalid SSH target id"));
        }
        let _ = runner_data;
        Ok(Self {
            identity: TargetIdentity {
                stable_id,
                display_name,
                kind: ExecutionTargetKind::SshRunner,
                endpoint: Some(config.host.clone()),
                verified_identity: None,
                platform: "remote".into(),
                runner_version: "unknown".into(),
                protocol_version: EXECUTION_PROTOCOL_VERSION,
                capabilities: TargetCapabilities {
                    durable_background_execution: true,
                    shell: true,
                    git: true,
                    disposable_workspace: true,
                    persistent_workspace: true,
                    outbound_network: false,
                    ..TargetCapabilities::default()
                },
                last_successful_probe_ms: None,
                trust_state: TargetTrustState::Unverified,
            },
            config,
            session: Arc::new(Mutex::new(None)),
        })
    }
    fn runner_command(&self, mode: &str) -> Result<Command, TargetError> {
        let mut command = Command::new("ssh");
        command.args(self.config.ssh_args()?);
        command.args([self.config.runner_binary.as_str(), "runner", mode]);
        if mode == "serve" {
            command.arg("--stdio");
        }
        Ok(command)
    }

    fn request(&self, frame: serde_json::Value) -> Result<serde_json::Value, TargetError> {
        let mut session = self
            .session
            .lock()
            .map_err(|_| TargetError::Io("SSH runner session poisoned".into()))?;
        if session.is_none() {
            let mut command = self.runner_command("serve")?;
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command
                .spawn()
                .map_err(|error| TargetError::target_unreachable(error.to_string()))?;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| TargetError::runner_lost("runner stdin unavailable"))?;
            let stdout = BufReader::new(
                child
                    .stdout
                    .take()
                    .ok_or_else(|| TargetError::runner_lost("runner stdout unavailable"))?,
            );
            *session = Some(SshSession {
                child,
                stdin,
                stdout,
            });
        }
        let current = session.as_mut().expect("SSH session initialized");
        let bytes =
            serde_json::to_vec(&frame).map_err(|error| TargetError::invalid(error.to_string()))?;
        if current
            .stdin
            .write_all(&bytes)
            .and_then(|_| current.stdin.write_all(b"\n"))
            .and_then(|_| current.stdin.flush())
            .is_err()
        {
            let _ = current.child.kill();
            *session = None;
            return Err(TargetError::runner_lost(
                "runner connection closed while sending request",
            ));
        }
        let mut line = String::new();
        if current
            .stdout
            .read_line(&mut line)
            .map_err(|error| TargetError::runner_lost(error.to_string()))?
            == 0
        {
            let _ = current.child.kill();
            *session = None;
            return Err(TargetError::RunnerRestarted(
                "runner closed the protocol session".into(),
            ));
        }
        serde_json::from_str(line.trim())
            .map_err(|error| TargetError::protocol_incompatible(error.to_string()))
    }
}

impl ExecutionTarget for SshRunnerTarget {
    fn probe(&self) -> Result<ExecutionTargetSnapshot, TargetError> {
        let mut command = self.runner_command("probe")?;
        let output = command
            .output()
            .map_err(|error| TargetError::target_unreachable(error.to_string()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
                return Err(TargetError::HostKeyChanged(stderr.trim().into()));
            }
            return Err(TargetError::target_unreachable(stderr.trim()));
        }
        let remote: ExecutionTargetSnapshot =
            serde_json::from_slice(&output.stdout).map_err(|error| {
                TargetError::ProtocolIncompatible(format!(
                    "remote monkey runner returned invalid probe data: {error}"
                ))
            })?;
        if remote.identity.protocol_version != EXECUTION_PROTOCOL_VERSION {
            return Err(TargetError::ProtocolIncompatible(format!(
                "remote runner protocol {} is incompatible with {}",
                remote.identity.protocol_version, EXECUTION_PROTOCOL_VERSION
            )));
        }
        let mut identity = self.identity.clone();
        identity.verified_identity = remote.identity.verified_identity;
        identity.runner_version = remote.identity.runner_version;
        identity.platform = remote.identity.platform;
        identity.capabilities = remote.identity.capabilities;
        identity.protocol_version = remote.identity.protocol_version;
        identity.trust_state = TargetTrustState::Verified;
        ExecutionTargetSnapshot::freeze(identity, now_ms())
    }
    fn capabilities(&self) -> Result<TargetCapabilities, TargetError> {
        Ok(self.identity.capabilities.clone())
    }
    fn prepare_workspace(
        &self,
        transfer: &WorkspaceTransfer,
        policy: WorkspacePolicy,
    ) -> Result<WorkspaceHandle, TargetError> {
        // The path is deliberately only a transport placeholder. The remote
        // runner chooses its own app-owned path from the transferred identity.
        Ok(WorkspaceHandle {
            workspace_id: transfer.workspace_id.clone(),
            snapshot_id: transfer.snapshot_id.clone(),
            path: PathBuf::from("."),
            policy,
            base_snapshot_digest: transfer.base_snapshot_digest.clone(),
            base_transfer: Some(transfer.clone()),
        })
    }
    fn submit_run(&self, request: RunRequest) -> Result<TargetRunHandle, TargetError> {
        request.target.require(&request.required_capabilities)?;
        let mut request = request;
        let transfer = request
            .workspace_transfer
            .or_else(|| request.workspace.base_transfer.clone())
            .ok_or_else(|| {
                TargetError::workspace_transfer_failed("SSH run omitted workspace transfer")
            })?;
        let manifest = transfer.manifest_request();
        let prepared = self.request(serde_json::json!({
            "type": "workspace_prepare",
            "manifest": manifest
        }))?;
        if let Some(error) = prepared.get("error") {
            return Err(TargetError::workspace_transfer_failed(error.to_string()));
        }
        let missing = prepared
            .get("missing")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                TargetError::protocol_incompatible("workspace prepare omitted missing hashes")
            })?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        let objects = transfer
            .missing_objects_for(
                &transfer
                    .all_objects()
                    .filter(|object| !missing.contains(&object.sha256))
                    .map(|object| object.sha256.clone())
                    .collect(),
            )
            .into_iter()
            .filter(|object| missing.contains(&object.sha256))
            .collect::<Vec<_>>();
        if !objects.is_empty() {
            let uploaded = self.request(serde_json::json!({
                "type": "workspace_upload",
                "workspaceId": transfer.workspace_id,
                "snapshotId": transfer.snapshot_id,
                "objects": objects
            }))?;
            if let Some(error) = uploaded.get("error") {
                return Err(TargetError::workspace_transfer_failed(error.to_string()));
            }
        }
        let mut cas_transfer = transfer;
        cas_transfer.object_hashes = manifest.object_hashes.clone();
        cas_transfer.objects.clear();
        cas_transfer.tracked_diff = None;
        cas_transfer.git_bundle = None;
        cas_transfer.tracked_diff_hash = manifest.tracked_diff_hash;
        cas_transfer.git_bundle_hash = manifest.git_bundle_hash;
        request.workspace_transfer = Some(cas_transfer);
        let response = self.request(serde_json::json!({"type":"submit_run","request":request}))?;
        if response.get("error").is_some() {
            let code = response
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("RUNNER_LOST");
            let detail = response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("runner rejected run")
                .to_string();
            return Err(match code {
                "CAPABILITY_UNAVAILABLE" => TargetError::capability_unavailable(detail),
                "PROTOCOL_INCOMPATIBLE" => TargetError::protocol_incompatible(detail),
                "WORKSPACE_CONFLICT" => TargetError::workspace_conflict(detail),
                "WORKSPACE_TRANSFER_FAILED" => TargetError::workspace_transfer_failed(detail),
                _ => TargetError::runner_lost(detail),
            });
        }
        Ok(TargetRunHandle {
            run_id: request.run_id,
            remote_id: response
                .get("remoteId")
                .and_then(|value| value.as_str())
                .unwrap_or("ssh-run")
                .into(),
            target_id: self.identity.stable_id.clone(),
        })
    }
    fn events(
        &self,
        handle: &TargetRunHandle,
        after_sequence: u64,
    ) -> Result<Vec<TargetEvent>, TargetError> {
        let response = self.request(serde_json::json!({"type":"events","runId":handle.remote_id,"afterSequence":after_sequence}))?;
        serde_json::from_value(
            response
                .get("events")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([])),
        )
        .map_err(|error| TargetError::result_retrieval_failed(error.to_string()))
    }
    fn status(&self, handle: &TargetRunHandle) -> Result<TargetRunStatus, TargetError> {
        let response =
            self.request(serde_json::json!({"type":"status","runId":handle.remote_id}))?;
        serde_json::from_value(
            response
                .get("status")
                .cloned()
                .unwrap_or_else(|| serde_json::json!("lost")),
        )
        .map_err(|error| TargetError::runner_lost(error.to_string()))
    }
    fn cancel(&self, handle: &TargetRunHandle) -> Result<(), TargetError> {
        let response =
            self.request(serde_json::json!({"type":"cancel","runId":handle.remote_id}))?;
        if let Some(error) = response.get("error") {
            return Err(TargetError::runner_lost(
                error.as_str().unwrap_or("runner rejected cancellation"),
            ));
        }
        Ok(())
    }
    fn pause(&self, handle: &TargetRunHandle) -> Result<(), TargetError> {
        let response =
            self.request(serde_json::json!({"type":"pause","runId":handle.remote_id}))?;
        if let Some(error) = response.get("error") {
            return Err(TargetError::unsupported(
                error.as_str().unwrap_or("runner rejected pause"),
            ));
        }
        Ok(())
    }
    fn resume(&self, handle: &TargetRunHandle) -> Result<(), TargetError> {
        let response =
            self.request(serde_json::json!({"type":"resume","runId":handle.remote_id}))?;
        if let Some(error) = response.get("error") {
            return Err(TargetError::unsupported(
                error.as_str().unwrap_or("runner rejected resume"),
            ));
        }
        Ok(())
    }
    fn artifacts(&self, handle: &TargetRunHandle) -> Result<Vec<ArtifactDescriptor>, TargetError> {
        let response =
            self.request(serde_json::json!({"type":"artifacts","runId":handle.remote_id}))?;
        serde_json::from_value(
            response
                .get("artifacts")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([])),
        )
        .map_err(|error| TargetError::result_retrieval_failed(error.to_string()))
    }
    fn workspace_result(&self, handle: &TargetRunHandle) -> Result<WorkspaceResult, TargetError> {
        let response =
            self.request(serde_json::json!({"type":"workspace_result","runId":handle.remote_id}))?;
        serde_json::from_value(response.get("result").cloned().unwrap_or(response))
            .map_err(|error| TargetError::result_retrieval_failed(error.to_string()))
    }
    fn cleanup(&self, workspace: &WorkspaceHandle) -> Result<(), TargetError> {
        self.request(serde_json::json!({"type":"cleanup","workspaceId":workspace.workspace_id,"snapshotId":workspace.snapshot_id})).map(|_| ())
    }
}

#[derive(Clone)]
pub struct RemoteNodeTarget {
    snapshot: ExecutionTargetSnapshot,
}

impl RemoteNodeTarget {
    pub fn from_snapshot(snapshot: ExecutionTargetSnapshot) -> Result<Self, TargetError> {
        if snapshot.identity.kind != ExecutionTargetKind::RemoteNode {
            return Err(TargetError::invalid("snapshot is not a remote-node target"));
        }
        Ok(Self { snapshot })
    }
}

impl ExecutionTarget for RemoteNodeTarget {
    fn probe(&self) -> Result<ExecutionTargetSnapshot, TargetError> {
        Ok(self.snapshot.clone())
    }
    fn capabilities(&self) -> Result<TargetCapabilities, TargetError> {
        Ok(self.snapshot.identity.capabilities.clone())
    }
    fn prepare_workspace(
        &self,
        _transfer: &WorkspaceTransfer,
        _policy: WorkspacePolicy,
    ) -> Result<WorkspaceHandle, TargetError> {
        Err(TargetError::unsupported(format!(
            "remote node {} must provision through the existing signed placement plane",
            self.snapshot.identity.stable_id
        )))
    }
    fn submit_run(&self, _: RunRequest) -> Result<TargetRunHandle, TargetError> {
        Err(TargetError::unsupported(
            "remote node submission is delegated to the existing signed placement plane",
        ))
    }
    fn events(&self, _: &TargetRunHandle, _: u64) -> Result<Vec<TargetEvent>, TargetError> {
        Ok(Vec::new())
    }
    fn status(&self, _: &TargetRunHandle) -> Result<TargetRunStatus, TargetError> {
        Err(TargetError::unsupported(
            "remote node status is owned by the existing placement plane",
        ))
    }
    fn cancel(&self, _: &TargetRunHandle) -> Result<(), TargetError> {
        Err(TargetError::unsupported(
            "remote node cancellation is owned by the existing placement plane",
        ))
    }
    fn pause(&self, _: &TargetRunHandle) -> Result<(), TargetError> {
        Err(TargetError::unsupported(
            "remote node pause is owned by the existing placement plane",
        ))
    }
    fn resume(&self, _: &TargetRunHandle) -> Result<(), TargetError> {
        Err(TargetError::unsupported(
            "remote node resume is owned by the existing placement plane",
        ))
    }
    fn artifacts(&self, _: &TargetRunHandle) -> Result<Vec<ArtifactDescriptor>, TargetError> {
        Ok(Vec::new())
    }
    fn workspace_result(&self, _: &TargetRunHandle) -> Result<WorkspaceResult, TargetError> {
        Err(TargetError::result_retrieval_failed(
            "remote node result is owned by the existing placement plane",
        ))
    }
    fn cleanup(&self, _: &WorkspaceHandle) -> Result<(), TargetError> {
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum TargetConfig {
    Local {
        identity: TargetIdentity,
    },
    Docker {
        identity: TargetIdentity,
        image: String,
        runner_data: PathBuf,
    },
    RemoteNode {
        identity: TargetIdentity,
    },
    SshRunner {
        identity: TargetIdentity,
        config: SshRunnerConfig,
        runner_data: PathBuf,
    },
}

impl TargetConfig {
    pub fn identity(&self) -> &TargetIdentity {
        match self {
            Self::Local { identity }
            | Self::Docker { identity, .. }
            | Self::RemoteNode { identity }
            | Self::SshRunner { identity, .. } => identity,
        }
    }
    pub fn identity_mut(&mut self) -> &mut TargetIdentity {
        match self {
            Self::Local { identity }
            | Self::Docker { identity, .. }
            | Self::RemoteNode { identity }
            | Self::SshRunner { identity, .. } => identity,
        }
    }
    pub fn validate(&self) -> Result<(), TargetError> {
        self.identity().validate()?;
        if let Self::SshRunner { config, .. } = self {
            config.validate()?;
        }
        Ok(())
    }
    pub fn target(&self) -> Result<Box<dyn ExecutionTarget>, TargetError> {
        self.validate()?;
        match self {
            Self::Local { .. } => Ok(Box::new(LocalExecutionTarget::new())),
            Self::Docker {
                identity,
                image,
                runner_data,
            } => Ok(Box::new(DockerExecutionTarget::new(
                identity.stable_id.clone(),
                identity.display_name.clone(),
                image.clone(),
                runner_data.clone(),
            )?)),
            Self::RemoteNode { identity } => Ok(Box::new(RemoteNodeTarget::from_snapshot(
                ExecutionTargetSnapshot::freeze(identity.clone(), now_ms())?,
            )?)),
            Self::SshRunner {
                identity,
                config,
                runner_data,
            } => Ok(Box::new(SshRunnerTarget::new(
                identity.stable_id.clone(),
                identity.display_name.clone(),
                config.clone(),
                runner_data.clone(),
            )?)),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetRegistry {
    pub targets: BTreeMap<String, TargetConfig>,
}

impl TargetRegistry {
    fn with_local(mut registry: Self) -> Self {
        registry
            .targets
            .entry("local".into())
            .or_insert_with(|| TargetConfig::Local {
                identity: LocalExecutionTarget::new().identity,
            });
        registry
    }
    pub fn load(path: &Path) -> Result<Self, TargetError> {
        if !path.exists() {
            return Ok(Self::with_local(Self::default()));
        }
        let bytes = fs::read(path)?;
        let registry: Self = serde_json::from_slice(&bytes)
            .map_err(|error| TargetError::invalid(error.to_string()))?;
        Ok(Self::with_local(registry))
    }
    pub fn save(&self, path: &Path) -> Result<(), TargetError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| TargetError::invalid(error.to_string()))?;
        let temporary = path.with_extension("tmp");
        fs::write(&temporary, bytes)?;
        fs::rename(temporary, path)?;
        Ok(())
    }
    pub fn add(&mut self, config: TargetConfig) -> Result<(), TargetError> {
        config.validate()?;
        let id = config.identity().stable_id.clone();
        self.targets.insert(id, config);
        Ok(())
    }
    pub fn remove(&mut self, id: &str) -> Result<TargetConfig, TargetError> {
        self.targets
            .remove(id)
            .ok_or_else(|| TargetError::invalid(format!("unknown target '{id}'")))
    }
    pub fn get(&self, id: &str) -> Result<&TargetConfig, TargetError> {
        self.targets
            .get(id)
            .ok_or_else(|| TargetError::invalid(format!("unknown target '{id}'")))
    }
}

/// Runner protocol used by SSH. The remote binary remains app-owned; no shell
/// is involved and the runner never receives private-key bytes.
pub fn runner_probe() -> Result<ExecutionTargetSnapshot, TargetError> {
    let identity = TargetIdentity {
        stable_id: "ssh-runner".into(),
        display_name: "Little Monkey runner".into(),
        kind: ExecutionTargetKind::SshRunner,
        endpoint: None,
        verified_identity: Some(env!("CARGO_PKG_VERSION").into()),
        platform: std::env::consts::OS.into(),
        runner_version: env!("CARGO_PKG_VERSION").into(),
        protocol_version: EXECUTION_PROTOCOL_VERSION,
        capabilities: TargetCapabilities {
            durable_background_execution: true,
            shell: true,
            git: true,
            disposable_workspace: true,
            persistent_workspace: true,
            suspend: cfg!(unix),
            ..TargetCapabilities::default()
        },
        last_successful_probe_ms: Some(now_ms()),
        trust_state: TargetTrustState::Verified,
    };
    ExecutionTargetSnapshot::freeze(identity, now_ms())
}

struct RunnerProcess {
    child: Option<Child>,
    pid: u32,
    workspace: WorkspaceHandle,
    base_transfer: WorkspaceTransfer,
    transient_inputs: Vec<PathBuf>,
    outcome_path: PathBuf,
    terminal: Option<RunnerTerminalOutcome>,
    started_at_ms: u64,
    wall_time_ms: u64,
    cancelled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunnerTerminalOutcome {
    status: TargetRunStatus,
    finished_at_ms: u64,
    exit_code: Option<i32>,
    termination_reason: Option<String>,
    result_digest: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct PersistedRunnerProcess {
    run_id: String,
    workspace: WorkspaceHandle,
    base_transfer: WorkspaceTransfer,
    transient_inputs: Vec<PathBuf>,
    #[serde(default)]
    outcome_path: Option<PathBuf>,
    #[serde(default)]
    terminal: Option<RunnerTerminalOutcome>,
    started_at_ms: u64,
    wall_time_ms: u64,
    pid: u32,
    cancelled: bool,
}

fn runner_data_directory() -> Result<PathBuf, TargetError> {
    crate::app_paths::data_dir()
        .map(|path| path.join("execution-runner"))
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|path| path.join(".little-monkey-runner"))
        })
        .ok_or_else(|| TargetError::Io("could not resolve runner data directory".into()))
}

fn runner_state_path(runner_data: &Path) -> PathBuf {
    runner_data.join("runs.json")
}

fn persist_runner_processes(
    runner_data: &Path,
    runs: &HashMap<String, RunnerProcess>,
) -> Result<(), TargetError> {
    fs::create_dir_all(runner_data)?;
    let records = runs
        .iter()
        .map(|(run_id, process)| {
            (
                run_id.clone(),
                PersistedRunnerProcess {
                    run_id: run_id.clone(),
                    workspace: process.workspace.clone(),
                    base_transfer: process.base_transfer.clone(),
                    transient_inputs: process.transient_inputs.clone(),
                    outcome_path: Some(process.outcome_path.clone()),
                    terminal: process.terminal.clone(),
                    started_at_ms: process.started_at_ms,
                    wall_time_ms: process.wall_time_ms,
                    pid: process.pid,
                    cancelled: process.cancelled,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let bytes =
        serde_json::to_vec_pretty(&records).map_err(|error| TargetError::Io(error.to_string()))?;
    let temporary = runner_state_path(runner_data).with_extension("json.tmp");
    fs::write(&temporary, bytes)?;
    fs::rename(temporary, runner_state_path(runner_data))?;
    Ok(())
}

fn load_runner_processes(
    runner_data: &Path,
) -> Result<HashMap<String, RunnerProcess>, TargetError> {
    let path = runner_state_path(runner_data);
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let records: BTreeMap<String, PersistedRunnerProcess> =
        serde_json::from_slice(&fs::read(path)?)
            .map_err(|error| TargetError::Io(error.to_string()))?;
    Ok(records
        .into_iter()
        .map(|(run_id, record)| {
            let outcome_run_id = run_id.clone();
            (
                run_id,
                RunnerProcess {
                    child: None,
                    pid: record.pid,
                    workspace: record.workspace,
                    base_transfer: record.base_transfer,
                    transient_inputs: record.transient_inputs,
                    outcome_path: record.outcome_path.unwrap_or_else(|| {
                        runner_data
                            .join("outcomes")
                            .join(format!("{outcome_run_id}.json"))
                    }),
                    terminal: record.terminal,
                    started_at_ms: record.started_at_ms,
                    wall_time_ms: record.wall_time_ms,
                    cancelled: record.cancelled,
                },
            )
        })
        .collect())
}

fn read_runner_outcome(
    process: &RunnerProcess,
) -> Result<Option<RunnerTerminalOutcome>, TargetError> {
    if !process.outcome_path.exists() {
        return Ok(None);
    }
    serde_json::from_slice(&fs::read(&process.outcome_path)?)
        .map(Some)
        .map_err(|error| TargetError::runner_lost(format!("invalid runner outcome: {error}")))
}

fn write_runner_outcome(
    outcome_path: &Path,
    status: TargetRunStatus,
    exit_code: Option<i32>,
    termination_reason: Option<String>,
) -> Result<RunnerTerminalOutcome, TargetError> {
    if let Some(parent) = outcome_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let outcome = RunnerTerminalOutcome {
        status,
        finished_at_ms: now_ms(),
        exit_code,
        termination_reason,
        result_digest: None,
    };
    let temporary = outcome_path.with_extension("json.tmp");
    fs::write(
        &temporary,
        serde_json::to_vec_pretty(&outcome).map_err(|error| TargetError::Io(error.to_string()))?,
    )?;
    fs::rename(temporary, outcome_path)?;
    Ok(outcome)
}

fn set_runner_result_digest(outcome_path: &Path, result_digest: String) -> Result<(), TargetError> {
    let Some(mut outcome) =
        serde_json::from_slice::<RunnerTerminalOutcome>(&fs::read(outcome_path)?).ok()
    else {
        return Err(TargetError::result_retrieval_failed(
            "runner outcome disappeared before result persistence",
        ));
    };
    outcome.result_digest = Some(result_digest);
    let temporary = outcome_path.with_extension("json.tmp");
    fs::write(
        &temporary,
        serde_json::to_vec_pretty(&outcome).map_err(|error| TargetError::Io(error.to_string()))?,
    )?;
    fs::rename(temporary, outcome_path)?;
    Ok(())
}

fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // The runner is deliberately a transport process. PID liveness lets a
        // fresh stdio connection observe a child that outlived the transport.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn runner_cas_directory(
    runner_data: &Path,
    workspace_id: &str,
    snapshot_id: &str,
) -> Result<PathBuf, TargetError> {
    let path = runner_data.join("cas").join(workspace_id).join(snapshot_id);
    if !path.starts_with(runner_data) {
        return Err(TargetError::invalid("runner CAS path escapes runner data"));
    }
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn cas_object_path(cas: &Path, hash: &str) -> Result<PathBuf, TargetError> {
    if !valid_digest(hash) {
        return Err(TargetError::workspace_transfer_failed(
            "CAS object hash is invalid",
        ));
    }
    Ok(cas.join(hash))
}

fn validate_manifest_request(request: &WorkspaceManifestRequest) -> Result<(), TargetError> {
    if request.schema_version != 1
        || !valid_id(&request.workspace_id)
        || !valid_id(&request.snapshot_id)
        || !valid_digest(&request.base_snapshot_digest)
    {
        return Err(TargetError::workspace_transfer_failed(
            "invalid workspace manifest identity",
        ));
    }
    let mut paths = BTreeSet::new();
    let mut normalized = BTreeSet::new();
    for entry in &request.manifest {
        validate_relative_path(&entry.relative_path, request.limits.max_path_bytes)?;
        if !paths.insert(entry.relative_path.clone())
            || !normalized.insert(normalized_path(&entry.relative_path))
        {
            return Err(TargetError::workspace_transfer_failed(
                "workspace manifest contains a path collision",
            ));
        }
        if !matches!(entry.file_type.as_str(), "file" | "directory" | "symlink") {
            return Err(TargetError::workspace_transfer_failed(
                "workspace manifest contains an unsupported file type",
            ));
        }
    }
    if request.object_hashes.iter().any(|hash| !valid_digest(hash))
        || request
            .tracked_diff_hash
            .as_deref()
            .is_some_and(|hash| !valid_digest(hash))
        || request
            .git_bundle_hash
            .as_deref()
            .is_some_and(|hash| !valid_digest(hash))
    {
        return Err(TargetError::workspace_transfer_failed(
            "workspace manifest contains an invalid object hash",
        ));
    }
    Ok(())
}

fn runner_prepare_workspace(
    value: &serde_json::Value,
) -> Result<WorkspaceMissingObjects, TargetError> {
    let request: WorkspaceManifestRequest = serde_json::from_value(
        value
            .get("manifest")
            .cloned()
            .ok_or_else(|| TargetError::protocol_incompatible("workspace manifest omitted"))?,
    )
    .map_err(|error| TargetError::protocol_incompatible(error.to_string()))?;
    validate_manifest_request(&request)?;
    let runner_data = runner_data_directory()?;
    let cas = runner_cas_directory(&runner_data, &request.workspace_id, &request.snapshot_id)?;
    let missing_hashes = request
        .object_hashes
        .iter()
        .filter(|hash| !cas_object_path(&cas, hash).is_ok_and(|path| path.is_file()))
        .cloned()
        .collect();
    Ok(WorkspaceMissingObjects {
        workspace_id: request.workspace_id,
        snapshot_id: request.snapshot_id,
        missing_hashes,
    })
}

fn runner_upload_workspace(
    value: &serde_json::Value,
) -> Result<WorkspaceMissingObjects, TargetError> {
    let workspace_id = value
        .get("workspaceId")
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            TargetError::protocol_incompatible("workspace upload omitted workspaceId")
        })?;
    let snapshot_id = value
        .get("snapshotId")
        .and_then(|value| value.as_str())
        .ok_or_else(|| TargetError::protocol_incompatible("workspace upload omitted snapshotId"))?;
    if !valid_id(workspace_id) || !valid_id(snapshot_id) {
        return Err(TargetError::invalid("workspace upload identity is invalid"));
    }
    let objects: Vec<WorkspaceObject> =
        serde_json::from_value(value.get("objects").cloned().ok_or_else(|| {
            TargetError::protocol_incompatible("workspace upload omitted objects")
        })?)
        .map_err(|error| TargetError::protocol_incompatible(error.to_string()))?;
    let runner_data = runner_data_directory()?;
    let cas = runner_cas_directory(&runner_data, workspace_id, snapshot_id)?;
    for object in objects {
        if !valid_digest(&object.sha256) || digest(&object.bytes) != object.sha256 {
            return Err(TargetError::workspace_transfer_failed(
                "workspace upload object digest mismatch",
            ));
        }
        let path = cas_object_path(&cas, &object.sha256)?;
        if !path.exists() {
            let temporary = path.with_extension("tmp");
            fs::write(&temporary, object.bytes)?;
            fs::rename(temporary, path)?;
        }
    }
    Ok(WorkspaceMissingObjects {
        workspace_id: workspace_id.to_string(),
        snapshot_id: snapshot_id.to_string(),
        missing_hashes: BTreeSet::new(),
    })
}

fn resolve_transfer_from_cas(
    mut transfer: WorkspaceTransfer,
    runner_data: &Path,
) -> Result<WorkspaceTransfer, TargetError> {
    let manifest = transfer.manifest_request();
    let cas = runner_cas_directory(runner_data, &manifest.workspace_id, &manifest.snapshot_id)?;
    if transfer.objects.is_empty() && !manifest.object_hashes.is_empty() {
        let mut objects = Vec::new();
        for hash in &manifest.object_hashes {
            let path = cas_object_path(&cas, hash)?;
            let bytes = fs::read(&path).map_err(|error| {
                TargetError::workspace_transfer_failed(format!(
                    "CAS object {hash} unavailable: {error}"
                ))
            })?;
            if digest(&bytes) != *hash {
                return Err(TargetError::workspace_transfer_failed(
                    "CAS object changed after negotiation",
                ));
            }
            objects.push(WorkspaceObject {
                sha256: hash.clone(),
                bytes,
            });
        }
        let tracked_hash = transfer.tracked_diff_hash.clone().or_else(|| {
            transfer
                .tracked_diff
                .as_ref()
                .map(|object| object.sha256.clone())
        });
        let bundle_hash = transfer.git_bundle_hash.clone().or_else(|| {
            transfer
                .git_bundle
                .as_ref()
                .map(|object| object.sha256.clone())
        });
        transfer.objects = objects
            .iter()
            .filter(|object| {
                Some(object.sha256.as_str()) != tracked_hash.as_deref()
                    && Some(object.sha256.as_str()) != bundle_hash.as_deref()
            })
            .cloned()
            .collect();
        transfer.tracked_diff = tracked_hash.map(|hash| WorkspaceObject {
            sha256: hash.clone(),
            bytes: objects
                .iter()
                .find(|object| object.sha256 == hash)
                .map(|object| object.bytes.clone())
                .unwrap_or_default(),
        });
        transfer.git_bundle = bundle_hash.map(|hash| WorkspaceObject {
            sha256: hash.clone(),
            bytes: objects
                .iter()
                .find(|object| object.sha256 == hash)
                .map(|object| object.bytes.clone())
                .unwrap_or_default(),
        });
    }
    transfer.validate()?;
    Ok(transfer)
}

fn runner_status(process: &mut RunnerProcess) -> Result<TargetRunStatus, TargetError> {
    if process.cancelled {
        return Ok(TargetRunStatus::Cancelled);
    }
    if let Some(outcome) = process.terminal.as_ref() {
        return Ok(outcome.status.clone());
    }
    if let Some(outcome) = read_runner_outcome(process)? {
        let status = outcome.status.clone();
        process.terminal = Some(outcome);
        return Ok(status);
    }
    if now_ms().saturating_sub(process.started_at_ms) > process.wall_time_ms {
        if crate::os_signal::kill_process_group(process.pid).is_err() {
            if let Some(child) = process.child.as_mut() {
                child
                    .kill()
                    .map_err(|error| TargetError::runner_lost(error.to_string()))?;
            }
        }
        process.terminal = Some(write_runner_outcome(
            &process.outcome_path,
            TargetRunStatus::Failed,
            None,
            Some("wall-time budget exceeded".to_string()),
        )?);
        return Ok(TargetRunStatus::Failed);
    }
    if let Some(child) = process.child.as_mut() {
        return match child.try_wait()? {
            None => Ok(TargetRunStatus::Running),
            Some(status) => {
                let fallback = write_runner_outcome(
                    &process.outcome_path,
                    if status.success() {
                        TargetRunStatus::Succeeded
                    } else {
                        TargetRunStatus::Failed
                    },
                    status.code(),
                    (!status.success() && status.code().is_none())
                        .then(|| "runner child terminated by a signal".to_string()),
                )?;
                let terminal = read_runner_outcome(process)?.unwrap_or(fallback);
                let result = terminal.status.clone();
                process.terminal = Some(terminal);
                Ok(result)
            }
        };
    }
    // A reconnected runner can only report a terminal state when the child
    // durably wrote one. A dead PID without an outcome is a lost run, never a
    // successful run.
    Ok(if process_alive(process.pid) {
        TargetRunStatus::Running
    } else {
        TargetRunStatus::Lost
    })
}

/// Executes one runner command in a child process and writes its terminal
/// outcome before exiting. The outcome file is what makes a run observable
/// after the stdio transport that launched it has disappeared.
pub fn runner_child(
    outcome_path: &Path,
    environment: &BTreeMap<String, String>,
    command: &[String],
) -> Result<i32, TargetError> {
    if command.is_empty()
        || command
            .iter()
            .any(|part| part.is_empty() || part.contains('\0'))
    {
        return Err(TargetError::invalid("runner child command is invalid"));
    }
    validate_environment(environment)?;
    let mut child = Command::new(&command[0]);
    child
        .args(&command[1..])
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .current_dir(std::env::current_dir().map_err(TargetError::from)?);
    for (key, value) in environment {
        child.env(key, value);
    }
    let status = match child.status() {
        Ok(status) => status,
        Err(error) => {
            let _ = write_runner_outcome(
                outcome_path,
                TargetRunStatus::Failed,
                None,
                Some(format!("command could not start: {error}")),
            );
            return Ok(127);
        }
    };
    let exit_code = status.code();
    let run_status = if status.success() {
        TargetRunStatus::Succeeded
    } else {
        TargetRunStatus::Failed
    };
    write_runner_outcome(
        outcome_path,
        run_status,
        exit_code,
        (!status.success() && exit_code.is_none())
            .then(|| "command terminated by a signal".to_string()),
    )?;
    Ok(exit_code.unwrap_or(1))
}

fn runner_submit(value: &serde_json::Value) -> Result<(String, RunnerProcess), TargetError> {
    let mut request: RunRequest = serde_json::from_value(
        value
            .get("request")
            .cloned()
            .ok_or_else(|| TargetError::protocol_incompatible("submit_run omitted request"))?,
    )
    .map_err(|error| TargetError::protocol_incompatible(error.to_string()))?;
    request.target.require(&request.required_capabilities)?;
    validate_environment(&request.environment)?;
    if request.command.is_empty()
        || request
            .command
            .iter()
            .any(|part| part.is_empty() || part.contains('\0'))
    {
        return Err(TargetError::invalid(
            "runner command is empty or contains NUL",
        ));
    }
    let transfer = request.workspace_transfer.take().ok_or_else(|| {
        TargetError::workspace_transfer_failed("runner request omitted workspace transfer")
    })?;
    let runner_data = runner_data_directory()?;
    let transfer = resolve_transfer_from_cas(transfer, &runner_data)?;
    let path = app_owned_workspace(&runner_data, &transfer.workspace_id, &transfer.snapshot_id)?;
    if path.exists() {
        if !transfer.cached_matches(&path)? {
            return Err(TargetError::WorkspaceConflict(
                "cached runner workspace does not match the requested snapshot".into(),
            ));
        }
    } else {
        transfer.materialize(&path)?;
        transfer.mark_cached(&path)?;
    }
    let mut transient_inputs = Vec::new();
    let mut input_bytes = 0u64;
    if request.input_files.len() > 1_024 {
        return Err(TargetError::workspace_transfer_failed(
            "runner input file count exceeds transfer bounds",
        ));
    }
    for file in &request.input_files {
        if file.file_type != "file" {
            return Err(TargetError::invalid(
                "runner input files must be regular files",
            ));
        }
        if digest(&file.bytes) != file.sha256 {
            return Err(TargetError::invalid(format!(
                "runner input digest mismatch for {}",
                file.path
            )));
        }
        if file.bytes.len() as u64 > MAX_TRANSFER_FILE_BYTES
            || input_bytes.saturating_add(file.bytes.len() as u64) > MAX_TRANSFER_TOTAL_BYTES
        {
            return Err(TargetError::workspace_transfer_failed(
                "runner input files exceed transfer bounds",
            ));
        }
        input_bytes = input_bytes.saturating_add(file.bytes.len() as u64);
        let input_path = safe_apply_join(&path, &file.path)?;
        if input_path.exists() || fs::symlink_metadata(&input_path).is_ok() {
            return Err(TargetError::workspace_conflict(format!(
                "runner input would overwrite workspace content: {}",
                file.path
            )));
        }
        if let Some(parent) = input_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&input_path, &file.bytes)?;
        set_executable(&input_path, file.executable)?;
        transient_inputs.push(input_path);
    }
    let workspace = WorkspaceHandle {
        workspace_id: transfer.workspace_id.clone(),
        snapshot_id: transfer.snapshot_id.clone(),
        path: path.clone(),
        policy: transfer.policy.clone(),
        base_snapshot_digest: transfer.base_snapshot_digest.clone(),
        base_transfer: Some(transfer.clone()),
    };
    let run_id = request.run_id.clone();
    let outcome_path = runner_data.join("outcomes").join(format!("{run_id}.json"));
    let runner_executable = std::env::current_exe().map_err(|error| {
        TargetError::runner_lost(format!("runner executable unavailable: {error}"))
    })?;
    let environment = serde_json::to_string(&request.environment)
        .map_err(|error| TargetError::invalid(error.to_string()))?;
    let mut command = Command::new(runner_executable);
    command.args([
        "runner",
        "child",
        "--outcome",
        outcome_path.to_string_lossy().as_ref(),
        "--environment",
        &environment,
        "--",
    ]);
    command.args(&request.command);
    command
        .current_dir(&path)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Put the wrapper and the command it spawns in one process group so
        // cancel/pause/resume control the actual workload, not only the wrapper.
        command.process_group(0);
    }
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            for path in &transient_inputs {
                let _ = fs::remove_file(path);
            }
            return Err(TargetError::runner_lost(format!(
                "could not start remote command: {error}"
            )));
        }
    };
    Ok((
        run_id,
        RunnerProcess {
            pid: child.id(),
            child: Some(child),
            workspace,
            base_transfer: transfer,
            transient_inputs,
            outcome_path,
            terminal: None,
            started_at_ms: now_ms(),
            wall_time_ms: request.wall_time_ms.max(1),
            cancelled: false,
        },
    ))
}

pub fn runner_serve_stdio() -> Result<(), TargetError> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let runner_data = runner_data_directory()?;
    let mut runs = load_runner_processes(&runner_data)?;
    for line in stdin.lock().lines() {
        let line = line.map_err(TargetError::from)?;
        if line.trim().is_empty() {
            continue;
        }
        let request: serde_json::Value = serde_json::from_str(&line)
            .map_err(|error| TargetError::protocol_incompatible(error.to_string()))?;
        let response = match request.get("type").and_then(|value| value.as_str()) {
            Some("probe") => serde_json::to_value(runner_probe()?)
                .map_err(|error| TargetError::invalid(error.to_string()))?,
            Some("workspace_prepare") => match runner_prepare_workspace(&request) {
                Ok(missing) => serde_json::json!({"missing": missing.missing_hashes}),
                Err(error) => serde_json::json!({"code":error.code(),"error":error.to_string()}),
            },
            Some("workspace_upload") => match runner_upload_workspace(&request) {
                Ok(_) => serde_json::json!({"ok":true}),
                Err(error) => serde_json::json!({"code":error.code(),"error":error.to_string()}),
            },
            Some("submit_run") => match runner_submit(&request) {
                Ok((run_id, process)) => {
                    runs.insert(run_id.clone(), process);
                    persist_runner_processes(&runner_data, &runs)?;
                    serde_json::json!({"remoteId":run_id,"status":"running"})
                }
                Err(error) => serde_json::json!({"code":error.code(),"error":error.to_string()}),
            },
            Some("status") => {
                let id = request
                    .get("runId")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default();
                let status = runs
                    .get_mut(id)
                    .ok_or_else(|| TargetError::runner_lost("unknown run"))
                    .and_then(runner_status);
                if status.is_ok() {
                    persist_runner_processes(&runner_data, &runs)?;
                }
                match status {
                    Ok(status) => serde_json::json!({"status":status}),
                    Err(error) => {
                        serde_json::json!({"code":error.code(),"error":error.to_string()})
                    }
                }
            }
            Some("events") => serde_json::json!({"events": []}),
            Some("cancel") => {
                let id = request
                    .get("runId")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default();
                match runs
                    .get_mut(id)
                    .ok_or_else(|| TargetError::runner_lost("unknown run"))
                    .and_then(|process| {
                        if crate::os_signal::kill_process_group(process.pid).is_err() {
                            if let Some(child) = process.child.as_mut() {
                                child.kill()?;
                            }
                        }
                        process.cancelled = true;
                        Ok(())
                    }) {
                    Ok(()) => {
                        if let Some(process) = runs.get_mut(id) {
                            process.terminal = Some(write_runner_outcome(
                                &process.outcome_path,
                                TargetRunStatus::Cancelled,
                                None,
                                Some("cancelled by client".to_string()),
                            )?);
                        }
                        persist_runner_processes(&runner_data, &runs)?;
                        serde_json::json!({"ok":true})
                    }
                    Err(error) => {
                        serde_json::json!({"code":error.code(),"error":error.to_string()})
                    }
                }
            }
            Some("pause") | Some("resume") => {
                let id = request
                    .get("runId")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default();
                let pause = request.get("type").and_then(|value| value.as_str()) == Some("pause");
                let controlled = runs
                    .get_mut(id)
                    .ok_or_else(|| TargetError::runner_lost("unknown run"))
                    .and_then(|process| {
                        if !process_alive(process.pid) {
                            return Err(TargetError::runner_lost(
                                "runner process is no longer alive",
                            ));
                        }
                        if pause {
                            crate::os_signal::suspend_process_group(process.pid)
                                .map_err(TargetError::unsupported)
                        } else {
                            crate::os_signal::resume_process_group(process.pid)
                                .map_err(TargetError::unsupported)
                        }
                    });
                match controlled {
                    Ok(()) => {
                        persist_runner_processes(&runner_data, &runs)?;
                        serde_json::json!({"ok":true})
                    }
                    Err(error) => {
                        serde_json::json!({"code":error.code(),"error":error.to_string()})
                    }
                }
            }
            Some("artifacts") => serde_json::json!({"artifacts": []}),
            Some("workspace_result") => {
                let id = request
                    .get("runId")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default();
                match runs
                    .get_mut(id)
                    .ok_or_else(|| TargetError::runner_lost("unknown run"))
                    .and_then(|process| {
                        if runner_status(process)? != TargetRunStatus::Succeeded {
                            return Err(TargetError::result_retrieval_failed(
                                "remote run has not completed successfully",
                            ));
                        }
                        for path in &process.transient_inputs {
                            if path.is_file() || path.is_symlink() {
                                let _ = fs::remove_file(path);
                            }
                        }
                        let result = workspace_result_from_workspace(
                            &process.base_transfer,
                            &process.workspace.path,
                        )?;
                        let result_id = workspace_result_id(&result)?;
                        if let Some(outcome) = process.terminal.as_mut() {
                            outcome.result_digest = Some(result_id.clone());
                        }
                        set_runner_result_digest(&process.outcome_path, result_id)?;
                        Ok(result)
                    }) {
                    Ok(result) => {
                        persist_runner_processes(&runner_data, &runs)?;
                        serde_json::json!({"result":result})
                    }
                    Err(error) => {
                        serde_json::json!({"code":error.code(),"error":error.to_string()})
                    }
                }
            }
            Some("cleanup") => {
                let workspace_id = request
                    .get("workspaceId")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default();
                let snapshot_id = request
                    .get("snapshotId")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default();
                let removed = runs
                    .iter()
                    .find(|(_, process)| {
                        process.workspace.workspace_id == workspace_id
                            && process.workspace.snapshot_id == snapshot_id
                    })
                    .map(|(id, _)| id.clone());
                if let Some(id) = removed {
                    if let Some(process) = runs.remove(&id) {
                        if matches!(process.workspace.policy, WorkspacePolicy::Ephemeral)
                            && process.workspace.path.exists()
                        {
                            let _ = fs::remove_dir_all(process.workspace.path);
                        }
                    }
                }
                persist_runner_processes(&runner_data, &runs)?;
                serde_json::json!({"ok":true})
            }
            Some("shutdown") => break,
            _ => {
                serde_json::json!({"code":"PROTOCOL_INCOMPATIBLE","error":"unknown runner request"})
            }
        };
        writeln!(
            stdout,
            "{}",
            serde_json::to_string(&response)
                .map_err(|error| TargetError::invalid(error.to_string()))?
        )
        .map_err(TargetError::from)?;
        stdout.flush().map_err(TargetError::from)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_negotiation_fails_before_submission() {
        let snapshot = LocalExecutionTarget::new().probe().unwrap();
        let required = RequiredCapabilities {
            gpu: true,
            ..RequiredCapabilities::default()
        };
        assert_eq!(
            snapshot.require(&required).unwrap_err().code(),
            "CAPABILITY_UNAVAILABLE"
        );
    }

    #[test]
    fn transfer_rejects_path_traversal_and_absolute_symlinks() {
        assert!(validate_relative_path("../escape", 100).is_err());
        assert!(validate_symlink_target("/tmp/escape", 32).is_err());
        assert!(validate_symlink_target("../../escape", 32).is_err());
    }

    #[test]
    fn app_owned_workspace_cannot_escape_runner_data() {
        let path = app_owned_workspace(Path::new("/tmp/runner-data"), "workspace-1", "snapshot-1")
            .unwrap();
        assert!(path.starts_with("/tmp/runner-data"));
        assert!(
            app_owned_workspace(Path::new("/tmp/runner-data"), "../escape", "snapshot-1").is_err()
        );
    }

    #[test]
    fn transfer_round_trip_preserves_binary_executable_and_safe_links() {
        let root = std::env::temp_dir().join(format!("little-monkey-transfer-{}", now_ms()));
        let destination = root.with_extension("materialized");
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin/run"), b"#!/bin/sh\n\0binary").unwrap();
        set_executable(&root.join("bin/run"), true).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("bin/run", root.join("entrypoint")).unwrap();
        let transfer = WorkspaceTransfer::from_workspace(&root, "workspace-roundtrip").unwrap();
        transfer.materialize(&destination).unwrap();
        assert_eq!(
            fs::read(destination.join("bin/run")).unwrap(),
            b"#!/bin/sh\n\0binary"
        );
        #[cfg(unix)]
        assert_eq!(
            fs::read_link(destination.join("entrypoint")).unwrap(),
            PathBuf::from("bin/run")
        );
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(destination);
    }

    #[test]
    fn workspace_result_applies_only_when_the_base_is_unchanged() {
        let root = std::env::temp_dir().join(format!("little-monkey-result-{}", now_ms()));
        let remote = root.with_extension("remote");
        let local = root.with_extension("local");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("note.txt"), b"before").unwrap();
        let base = WorkspaceTransfer::from_workspace(&root, "workspace-result").unwrap();
        base.materialize(&remote).unwrap();
        base.materialize(&local).unwrap();
        fs::write(remote.join("note.txt"), b"after").unwrap();
        let result = workspace_result_from_workspace(&base, &remote).unwrap();
        apply_workspace_result(&local, &base.base_snapshot_digest, &result).unwrap();
        assert_eq!(fs::read(local.join("note.txt")).unwrap(), b"after");
        fs::write(local.join("note.txt"), b"conflict").unwrap();
        assert_eq!(
            apply_workspace_result(&local, &base.base_snapshot_digest, &result)
                .unwrap_err()
                .code(),
            "WORKSPACE_CONFLICT"
        );
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(remote);
        let _ = fs::remove_dir_all(local);
    }

    #[test]
    fn git_transfer_preserves_the_exact_base_commit_for_result_application() {
        let root = std::env::temp_dir().join(format!("little-monkey-git-{}", now_ms()));
        let remote = root.with_extension("remote");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("note.txt"), b"before").unwrap();
        command_output("git", &["init", "-q"], Some(&root)).unwrap();
        command_output("git", &["add", "--all"], Some(&root)).unwrap();
        command_output(
            "git",
            &[
                "-c",
                "user.name=Little Monkey test",
                "-c",
                "user.email=test@little-monkey.invalid",
                "commit",
                "-qm",
                "base",
            ],
            Some(&root),
        )
        .unwrap();
        let base = WorkspaceTransfer::from_workspace(&root, "workspace-git").unwrap();
        assert!(matches!(base.kind, WorkspaceTransferKind::CleanGit { .. }));
        base.materialize(&remote).unwrap();
        let base_commit = match &base.kind {
            WorkspaceTransferKind::CleanGit { base_commit, .. } => base_commit,
            _ => unreachable!(),
        };
        assert_eq!(
            String::from_utf8_lossy(
                &command_output("git", &["rev-parse", "HEAD"], Some(&remote)).unwrap()
            )
            .trim(),
            base_commit
        );
        assert!(command_output("git", &["diff", "HEAD", "--exit-code"], Some(&remote)).is_ok());
        fs::write(remote.join("note.txt"), b"after").unwrap();
        let result = workspace_result_from_workspace(&base, &remote).unwrap();
        assert!(!result.git_diff.is_empty());
        apply_workspace_result(&root, &base.base_snapshot_digest, &result).unwrap();
        assert_eq!(fs::read(root.join("note.txt")).unwrap(), b"after");
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(remote);
    }

    #[test]
    fn manifest_negotiation_returns_only_objects_missing_at_the_target() {
        let root = std::env::temp_dir().join(format!("little-monkey-cas-{}", now_ms()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), b"a").unwrap();
        fs::write(root.join("b.bin"), [0, 1, 2, 255]).unwrap();
        let transfer = WorkspaceTransfer::from_workspace(&root, "workspace-cas").unwrap();
        let available = transfer
            .objects
            .first()
            .map(|object| BTreeSet::from([object.sha256.clone()]))
            .unwrap_or_default();
        let missing = transfer.missing_objects_for(&available);
        assert_eq!(
            missing.len(),
            transfer.objects.len().saturating_sub(available.len())
                + usize::from(transfer.tracked_diff.is_some())
                + usize::from(transfer.git_bundle.is_some())
        );
        assert!(missing
            .iter()
            .all(|object| !available.contains(&object.sha256)));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn persisted_result_is_reviewable_and_discardable_without_touching_workspace() {
        let data_dir = std::env::temp_dir().join(format!("little-monkey-results-{}", now_ms()));
        let result = WorkspaceResult {
            base_snapshot_digest: "a".repeat(64),
            resulting_snapshot_digest: "b".repeat(64),
            git_diff: Vec::new(),
            new_files: Vec::new(),
            deleted_files: Vec::new(),
            binary_changes: Vec::new(),
            artifacts: Vec::new(),
            verification_evidence: Vec::new(),
        };
        let id = persist_workspace_result(&data_dir, &result).unwrap();
        assert_eq!(load_workspace_result(&data_dir, &id).unwrap(), result);
        discard_workspace_result(&data_dir, &id).unwrap();
        assert!(load_workspace_result(&data_dir, &id).is_err());
        let patch_id =
            persist_patch_result(&data_dir, &"c".repeat(64), b"git patch".to_vec()).unwrap();
        assert!(matches!(
            load_execution_result(&data_dir, &patch_id).unwrap(),
            PersistedExecutionResult::Patch(_)
        ));
        discard_workspace_result(&data_dir, &patch_id).unwrap();
        assert!(load_execution_result(&data_dir, &patch_id).is_err());
        let _ = fs::remove_dir_all(data_dir);
    }

    #[test]
    fn docker_status_uses_exit_code_not_container_state_alone() {
        assert_eq!(
            docker_status_from_inspect("exited\t0\tfalse"),
            TargetRunStatus::Succeeded
        );
        assert_eq!(
            docker_status_from_inspect("paused\t0\tfalse"),
            TargetRunStatus::Running
        );
        assert_eq!(
            docker_status_from_inspect("exited\t42\tfalse"),
            TargetRunStatus::Failed
        );
        assert_eq!(
            docker_status_from_inspect("running\t0\tfalse"),
            TargetRunStatus::Running
        );
    }

    #[test]
    fn runner_reconnect_never_infers_success_from_a_dead_pid() {
        let root = std::env::temp_dir().join(format!("little-monkey-runner-lost-{}", now_ms()));
        fs::create_dir_all(&root).unwrap();
        let transfer = WorkspaceTransfer::from_workspace(&root, "workspace-lost").unwrap();
        let mut process = RunnerProcess {
            child: None,
            pid: std::process::id().saturating_add(1_000_000),
            workspace: WorkspaceHandle {
                workspace_id: transfer.workspace_id.clone(),
                snapshot_id: transfer.snapshot_id.clone(),
                path: root.clone(),
                policy: WorkspacePolicy::Ephemeral,
                base_snapshot_digest: transfer.base_snapshot_digest.clone(),
                base_transfer: Some(transfer.clone()),
            },
            base_transfer: transfer,
            transient_inputs: Vec::new(),
            outcome_path: root.join("outcome.json"),
            terminal: None,
            started_at_ms: now_ms(),
            wall_time_ms: 60_000,
            cancelled: false,
        };
        assert_eq!(runner_status(&mut process).unwrap(), TargetRunStatus::Lost);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn runner_reconnect_uses_durable_nonzero_outcome() {
        let root = std::env::temp_dir().join(format!("little-monkey-runner-failed-{}", now_ms()));
        fs::create_dir_all(&root).unwrap();
        let transfer = WorkspaceTransfer::from_workspace(&root, "workspace-failed").unwrap();
        let outcome_path = root.join("outcome.json");
        write_runner_outcome(
            &outcome_path,
            TargetRunStatus::Failed,
            Some(42),
            Some("command exited nonzero".to_string()),
        )
        .unwrap();
        let mut process = RunnerProcess {
            child: None,
            pid: std::process::id().saturating_add(1_000_001),
            workspace: WorkspaceHandle {
                workspace_id: transfer.workspace_id.clone(),
                snapshot_id: transfer.snapshot_id.clone(),
                path: root.clone(),
                policy: WorkspacePolicy::Ephemeral,
                base_snapshot_digest: transfer.base_snapshot_digest.clone(),
                base_transfer: Some(transfer.clone()),
            },
            base_transfer: transfer,
            transient_inputs: Vec::new(),
            outcome_path,
            terminal: None,
            started_at_ms: now_ms(),
            wall_time_ms: 60_000,
            cancelled: false,
        };
        assert_eq!(
            runner_status(&mut process).unwrap(),
            TargetRunStatus::Failed
        );
        assert_eq!(process.terminal.unwrap().exit_code, Some(42));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn real_runner_docker_protocol_reports_success_and_nonzero_failure() {
        let required =
            std::env::var("LITTLE_MONKEY_REQUIRE_RUNNER_DOCKER_E2E").as_deref() == Ok("1");
        let Some(image) = std::env::var_os("LITTLE_MONKEY_RUNNER_DOCKER_E2E_IMAGE") else {
            assert!(
                !required,
                "runner Docker E2E image is required but not configured"
            );
            eprintln!("SKIPPED: LITTLE_MONKEY_RUNNER_DOCKER_E2E_IMAGE is not configured");
            return;
        };
        let image = image.to_string_lossy().into_owned();
        let docker_ready = Command::new("docker")
            .args(["info", "--format", "{{.ServerVersion}}"])
            .output()
            .is_ok_and(|output| output.status.success());
        if !docker_ready {
            assert!(!required, "Docker daemon is required but unavailable");
            eprintln!("SKIPPED: Docker daemon is unavailable");
            return;
        }

        fn run_protocol_command(
            image: &str,
        ) -> (
            std::process::Child,
            std::process::ChildStdin,
            BufReader<std::process::ChildStdout>,
        ) {
            let mut child = Command::new("docker")
                .args(["run", "--rm", "-i", image])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("runner container should start");
            let stdin = child.stdin.take().expect("runner stdin");
            let stdout = BufReader::new(child.stdout.take().expect("runner stdout"));
            (child, stdin, stdout)
        }
        fn request(
            stdin: &mut std::process::ChildStdin,
            stdout: &mut BufReader<std::process::ChildStdout>,
            value: serde_json::Value,
        ) -> serde_json::Value {
            writeln!(stdin, "{}", serde_json::to_string(&value).unwrap()).unwrap();
            stdin.flush().unwrap();
            let mut line = String::new();
            stdout.read_line(&mut line).unwrap();
            serde_json::from_str(line.trim()).unwrap()
        }
        fn wait_status(
            stdin: &mut std::process::ChildStdin,
            stdout: &mut BufReader<std::process::ChildStdout>,
            run_id: &str,
        ) -> TargetRunStatus {
            for _ in 0..100 {
                let response = request(
                    stdin,
                    stdout,
                    serde_json::json!({"type":"status","runId":run_id}),
                );
                let status: TargetRunStatus =
                    serde_json::from_value(response["status"].clone()).unwrap();
                if matches!(
                    status,
                    TargetRunStatus::Succeeded
                        | TargetRunStatus::Failed
                        | TargetRunStatus::Cancelled
                        | TargetRunStatus::Lost
                ) {
                    return status;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            panic!("runner did not reach a terminal state");
        }
        fn transfer_for_run(suffix: &str) -> WorkspaceTransfer {
            let root = std::env::temp_dir().join(format!("little-monkey-runner-e2e-{suffix}"));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("baseline.txt"), b"baseline\n").unwrap();
            let transfer =
                WorkspaceTransfer::from_workspace(&root, &format!("runner-e2e-{suffix}")).unwrap();
            let _ = fs::remove_dir_all(root);
            transfer
        }
        fn submit(
            stdin: &mut std::process::ChildStdin,
            stdout: &mut BufReader<std::process::ChildStdout>,
            run_id: &str,
            command: Vec<String>,
            transfer: WorkspaceTransfer,
        ) -> TargetRunStatus {
            let probe: ExecutionTargetSnapshot =
                serde_json::from_value(request(stdin, stdout, serde_json::json!({"type":"probe"})))
                    .unwrap();
            let manifest = transfer.manifest_request();
            let prepared = request(
                stdin,
                stdout,
                serde_json::json!({"type":"workspace_prepare","manifest":manifest}),
            );
            let missing = prepared["missing"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<BTreeSet<_>>();
            let objects = transfer
                .all_objects()
                .filter(|object| missing.contains(&object.sha256))
                .cloned()
                .collect::<Vec<_>>();
            request(
                stdin,
                stdout,
                serde_json::json!({
                    "type":"workspace_upload",
                    "workspaceId":transfer.workspace_id,
                    "snapshotId":transfer.snapshot_id,
                    "objects":objects
                }),
            );
            let mut cas_transfer = transfer.clone();
            cas_transfer.object_hashes = manifest.object_hashes;
            cas_transfer.objects.clear();
            cas_transfer.tracked_diff = None;
            cas_transfer.git_bundle = None;
            cas_transfer.tracked_diff_hash = manifest.tracked_diff_hash;
            cas_transfer.git_bundle_hash = manifest.git_bundle_hash;
            let workspace = WorkspaceHandle {
                workspace_id: transfer.workspace_id.clone(),
                snapshot_id: transfer.snapshot_id.clone(),
                path: PathBuf::from("."),
                policy: WorkspacePolicy::Ephemeral,
                base_snapshot_digest: transfer.base_snapshot_digest.clone(),
                base_transfer: Some(transfer),
            };
            let response = request(
                stdin,
                stdout,
                serde_json::json!({
                    "type":"submit_run",
                    "request": RunRequest {
                        run_id: run_id.to_string(),
                        target: probe,
                        required_capabilities: RequiredCapabilities { shell: true, ..Default::default() },
                        workspace,
                        command,
                        environment: BTreeMap::new(),
                        wall_time_ms: 10_000,
                        max_artifact_bytes: 1_000_000,
                        workspace_transfer: Some(cas_transfer),
                        input_files: Vec::new(),
                    }
                }),
            );
            assert!(
                response.get("remoteId").is_some(),
                "runner submission failed: {response}"
            );
            wait_status(stdin, stdout, run_id)
        }

        let (mut child, mut stdin, mut stdout) = run_protocol_command(&image);
        let success_transfer = transfer_for_run("success");
        assert_eq!(
            submit(
                &mut stdin,
                &mut stdout,
                "runner-e2e-success",
                vec![
                    "sh".into(),
                    "-c".into(),
                    "printf changed > result.txt".into()
                ],
                success_transfer,
            ),
            TargetRunStatus::Succeeded
        );
        let failure_transfer = transfer_for_run("failure");
        assert_eq!(
            submit(
                &mut stdin,
                &mut stdout,
                "runner-e2e-failure",
                vec!["sh".into(), "-c".into(), "exit 42".into()],
                failure_transfer,
            ),
            TargetRunStatus::Failed
        );
        writeln!(stdin, "{{\"type\":\"shutdown\"}}").unwrap();
        let _ = stdin.flush();
        drop(stdin);
        let _ = child.wait();
    }
}
