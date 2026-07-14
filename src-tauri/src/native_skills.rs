//! Data-only `SKILL.md` runtime shared by desktop commands and headless clients.
//!
//! Skills are prompt/instruction bundles, never executable plugins. Discovery
//! accepts one bounded directory tree per skill, rejects symbolic links and
//! special files, hashes every retained byte, and evaluates declarative OS,
//! binary, and environment requirements without revealing environment values.
//! Managed installs use digest approval and same-filesystem rename activation;
//! signed M4 package skills are merged through a small external descriptor so
//! this core remains independent of Tauri and the marketplace implementation.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

pub const NATIVE_SKILL_SCHEMA_VERSION: u32 = 1;
pub const MAX_DISCOVERED_SKILLS: usize = 256;
pub const MAX_SKILL_FILES: usize = 128;
pub const MAX_SKILL_TOTAL_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_SKILL_FILE_BYTES: u64 = 512 * 1024;
pub const MAX_SKILL_MD_BYTES: u64 = 256 * 1024;
pub const MAX_FRONTMATTER_BYTES: usize = 32 * 1024;
pub const MAX_SKILL_DEPTH: usize = 8;
pub const MAX_RELATIVE_PATH_BYTES: usize = 240;
pub const MAX_HISTORY: usize = 8;
pub const GIT_OPERATION_TIMEOUT: Duration = Duration::from_secs(60);

const STATE_FILE: &str = ".littlemonkey-skills-state-v1.json";
const HISTORY_DIR: &str = ".littlemonkey-history-v1";
const STAGING_PREFIX: &str = ".littlemonkey-staging-";
const ACQUISITION_DIR: &str = "acquisitions";
const RESERVED_SLASH_COMMANDS: &[&str] = &[
    "status", "tools", "skills", "plugins", "model", "new", "compact", "stop", "usage", "learn",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillError {
    Invalid(String),
    Conflict(String),
    NotFound(String),
    Approval(String),
    Io(String),
    Git(String),
}

impl fmt::Display for SkillError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(formatter, "invalid skill: {message}"),
            Self::Conflict(message) => write!(formatter, "skill conflict: {message}"),
            Self::NotFound(message) => write!(formatter, "skill not found: {message}"),
            Self::Approval(message) => write!(formatter, "skill approval: {message}"),
            Self::Io(message) => write!(formatter, "skill storage: {message}"),
            Self::Git(message) => write!(formatter, "skill Git acquisition: {message}"),
        }
    }
}

impl std::error::Error for SkillError {}

impl From<io::Error> for SkillError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SkillScope {
    Global,
    Workspace,
}

impl SkillScope {
    fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Workspace => "workspace",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum SupportedOs {
    Macos,
    Linux,
    Windows,
}

impl SupportedOs {
    fn current() -> Option<Self> {
        #[cfg(target_os = "macos")]
        {
            return Some(Self::Macos);
        }
        #[cfg(target_os = "linux")]
        {
            return Some(Self::Linux);
        }
        #[cfg(target_os = "windows")]
        {
            return Some(Self::Windows);
        }
        #[allow(unreachable_code)]
        None
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RawRequirements {
    #[serde(default)]
    bins: Vec<String>,
    #[serde(default)]
    env: Vec<String>,
}

/// Frontmatter schema. Unknown top-level keys are deliberately tolerated
/// (no `deny_unknown_fields`): the ecosystem SKILL.md format used by Claude
/// Code, ponytail, and similar skill repos carries extra keys such as
/// `argument-hint`, `license`, `allowed-tools`, and `metadata` that this
/// runtime has no use for but must not reject. `command` and `version` are
/// optional for the same reason — ecosystem skills derive the slash command
/// from `name` and often carry no version. Nested `requires` stays strict.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct RawSkillManifest {
    name: String,
    description: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    os: Vec<SupportedOs>,
    #[serde(default)]
    requires: RawRequirements,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillRequirements {
    pub bins: BTreeSet<String>,
    pub env: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillManifest {
    pub name: String,
    pub description: String,
    /// Slash command without the leading slash.
    pub command: String,
    pub version: String,
    /// Empty means all supported operating systems.
    pub os: BTreeSet<SupportedOs>,
    pub requires: SkillRequirements,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillEligibility {
    pub eligible: bool,
    pub current_os: String,
    pub unsupported_os: bool,
    pub missing_bins: Vec<String>,
    pub missing_env: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SkillSource {
    Global { path: String },
    Workspace { path: String },
    SignedPackage { package_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillDescriptor {
    pub name: String,
    pub description: String,
    pub command: String,
    pub version: String,
    pub instructions: String,
    pub sha256: String,
    pub file_count: usize,
    pub total_bytes: u64,
    pub enabled: bool,
    pub eligibility: SkillEligibility,
    pub supported_os: BTreeSet<SupportedOs>,
    pub requirements: SkillRequirements,
    pub source: SkillSource,
    /// Only signed packages can declare capability permissions. Native
    /// `SKILL.md` folders remain data-only and therefore always return none.
    pub permissions: BTreeSet<String>,
    /// The Git repository this skill was installed from (bulk or single
    /// `install_git`), so the UI can group same-repo skills into one card
    /// with shared enable/disable/uninstall/rollback. `None` for local
    /// installs and signed packages.
    pub git_repository: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExternalSignedSkill {
    pub package_id: String,
    pub name: String,
    pub description: String,
    pub command: String,
    pub version: String,
    pub instructions: String,
    pub sha256: String,
    pub permissions: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillInstallPreview {
    pub scope: SkillScope,
    pub name: String,
    pub description: String,
    pub command: String,
    pub version: String,
    pub sha256: String,
    pub file_count: usize,
    pub total_bytes: u64,
    pub eligibility: SkillEligibility,
    pub supported_os: BTreeSet<SupportedOs>,
    pub requirements: SkillRequirements,
    pub approval_digest: String,
    pub origin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillMutationResult {
    pub command: String,
    pub scope: SkillScope,
    pub active_sha256: Option<String>,
    pub enabled: bool,
    pub history_sha256: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitSkillRequest {
    pub repository_url: String,
    /// A 40-hex commit SHA, a branch/tag name, or empty for the remote's
    /// default branch (`HEAD`). Non-SHA values are resolved to a pinned
    /// commit via `git ls-remote` at preview time; the preview reports the
    /// resolved commit and the approval digest binds to it, so an install
    /// after upstream moved fails closed.
    #[serde(default)]
    pub commit: String,
    #[serde(default)]
    pub subdirectory: Option<String>,
}

/// One installable skill folder found while scanning a Git checkout whose
/// root has no `SKILL.md`. Carries a full install preview (including the
/// approval digest) so the caller can install any subset — or all of them —
/// without another preview round-trip per skill.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitSkillCandidate {
    /// Checkout-relative path to pass back as `GitSkillRequest.subdirectory`.
    pub subdirectory: String,
    pub preview: SkillInstallPreview,
}

/// One skill of a bulk Git install: the subdirectory previewed as a
/// candidate plus the approval digest from that candidate's preview.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitBulkApproval {
    pub subdirectory: String,
    pub approval_digest: String,
}

/// Result of [`NativeSkillManager::preview_git`]: either a full install
/// preview (skill root found) or the list of skill folders discovered in the
/// checkout when no subdirectory was given and the root has no `SKILL.md`.
/// `pinned_commit` is always the fully resolved 40-hex SHA — callers must
/// pass it back in the install request so the approval digest verifies
/// against the exact snapshot that was previewed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum GitSkillPreviewOutcome {
    Preview {
        pinned_commit: String,
        preview: SkillInstallPreview,
    },
    Candidates {
        pinned_commit: String,
        candidates: Vec<GitSkillCandidate>,
    },
}

#[derive(Debug, Clone)]
struct ParsedSkill {
    manifest: SkillManifest,
    instructions: String,
}

#[derive(Debug, Clone)]
struct ScannedFile {
    relative: PathBuf,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
struct ScannedSkill {
    parsed: ParsedSkill,
    sha256: String,
    files: Vec<ScannedFile>,
    total_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct HistoryRecord {
    sha256: String,
    directory: String,
    version: String,
    stored_at_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ManagedSkillState {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    managed: bool,
    #[serde(default)]
    active_sha256: Option<String>,
    #[serde(default)]
    history: Vec<HistoryRecord>,
    /// Set when this skill was installed via `install_git`/`install_git_bulk`
    /// — the repository URL it came from, purely for UI grouping. Left
    /// untouched by `set_enabled`/`uninstall`/`rollback` so the group
    /// membership survives those operations.
    #[serde(default)]
    origin_repository: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RootState {
    schema_version: u32,
    #[serde(default)]
    skills: BTreeMap<String, ManagedSkillState>,
}

impl Default for RootState {
    fn default() -> Self {
        Self {
            schema_version: NATIVE_SKILL_SCHEMA_VERSION,
            skills: BTreeMap::new(),
        }
    }
}

/// Filesystem-backed native skill manager. Its mutation lock protects the
/// preview-recheck/activation sequence inside one process; approval digests
/// and atomic publication remain the cross-layer integrity boundary.
pub struct NativeSkillManager {
    global_root: PathBuf,
    acquisition_root: PathBuf,
    git_binary: PathBuf,
    mutation: Mutex<()>,
}

impl NativeSkillManager {
    pub fn new(app_data_dir: impl AsRef<Path>) -> Result<Self, SkillError> {
        let app_data_dir = app_data_dir.as_ref();
        ensure_plain_directory(app_data_dir, "application data")?;
        let base_root = app_data_dir.join("native-skills-v1");
        ensure_plain_directory(&base_root, "native skill root")?;
        let global_root = base_root.join("global");
        ensure_plain_directory(&global_root, "global skill root")?;
        let acquisition_root = base_root.join(ACQUISITION_DIR);
        ensure_plain_directory(&acquisition_root, "skill acquisition root")?;
        set_private_directory_permissions(&base_root)?;
        set_private_directory_permissions(&global_root)?;
        set_private_directory_permissions(&acquisition_root)?;
        Ok(Self {
            global_root,
            acquisition_root,
            git_binary: PathBuf::from("git"),
            mutation: Mutex::new(()),
        })
    }

    /// Opens an already-initialized native-skill store without creating
    /// directories or tightening permissions. Security/audit callers use
    /// this read-only constructor so an ordinary audit cannot mutate state.
    /// `None` means the native-skill runtime has never been initialized.
    pub fn open_existing(app_data_dir: impl AsRef<Path>) -> Result<Option<Self>, SkillError> {
        let app_data_dir = app_data_dir.as_ref();
        let app_metadata = fs::symlink_metadata(app_data_dir)
            .map_err(|error| io_at("inspect application data", app_data_dir, error))?;
        if app_metadata.file_type().is_symlink() || !app_metadata.is_dir() {
            return Err(SkillError::Invalid(format!(
                "application data {} must be a real directory",
                app_data_dir.display()
            )));
        }
        let base_root = app_data_dir.join("native-skills-v1");
        let global_root = base_root.join("global");
        for (path, label) in [
            (&base_root, "native skill root"),
            (&global_root, "global skill root"),
        ] {
            match fs::symlink_metadata(path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(io_at(&format!("inspect {label}"), path, error)),
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                    return Err(SkillError::Invalid(format!(
                        "{label} {} must be a real directory",
                        path.display()
                    )))
                }
                Ok(_) => {}
            }
        }
        let acquisition_root = base_root.join(ACQUISITION_DIR);
        if let Ok(metadata) = fs::symlink_metadata(&acquisition_root) {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(SkillError::Invalid(format!(
                    "skill acquisition root {} must be a real directory",
                    acquisition_root.display()
                )));
            }
        }
        Ok(Some(Self {
            global_root,
            acquisition_root,
            git_binary: PathBuf::from("git"),
            mutation: Mutex::new(()),
        }))
    }

    #[cfg(test)]
    fn with_git_binary(mut self, git_binary: impl Into<PathBuf>) -> Self {
        self.git_binary = git_binary.into();
        self
    }

    pub fn global_root(&self) -> &Path {
        &self.global_root
    }

    pub fn discover(
        &self,
        primary_workspace: Option<&Path>,
        signed_packages: &[ExternalSignedSkill],
    ) -> Result<Vec<SkillDescriptor>, SkillError> {
        let _guard = self.lock()?;
        let mut skills = Vec::<SkillDescriptor>::new();
        let mut active_commands = BTreeMap::<String, String>::new();
        self.discover_root(
            SkillScope::Global,
            &self.global_root,
            &mut skills,
            &mut active_commands,
        )?;
        if let Some(workspace) = primary_workspace {
            if let Some(root) =
                self.scope_root_if_present(SkillScope::Workspace, Some(workspace))?
            {
                self.discover_root(
                    SkillScope::Workspace,
                    &root,
                    &mut skills,
                    &mut active_commands,
                )?;
            }
        }
        for package in signed_packages {
            let command = validate_command(&package.command)?;
            if let Some(existing) = active_commands.get(&command) {
                return Err(SkillError::Conflict(format!(
                    "/{command} is provided by both {existing} and signed package {}",
                    package.package_id,
                )));
            }
            if package.instructions.trim().is_empty() {
                return Err(SkillError::Invalid(format!(
                    "signed package {} has empty instructions",
                    package.package_id
                )));
            }
            validate_sha256(&package.sha256, "signed package skill digest")?;
            active_commands.insert(
                command.clone(),
                format!("signed package {}", package.package_id),
            );
            skills.push(SkillDescriptor {
                name: bounded_trimmed(&package.name, "name", 1, 96)?,
                description: bounded_trimmed(&package.description, "description", 1, 1024)?,
                command,
                version: validate_version(&package.version)?,
                instructions: package.instructions.trim().to_string(),
                sha256: package.sha256.to_ascii_lowercase(),
                file_count: 1,
                total_bytes: package.instructions.len() as u64,
                enabled: true,
                eligibility: eligible_everywhere(),
                supported_os: BTreeSet::new(),
                requirements: SkillRequirements {
                    bins: BTreeSet::new(),
                    env: BTreeSet::new(),
                },
                source: SkillSource::SignedPackage {
                    package_id: package.package_id.clone(),
                },
                permissions: package.permissions.clone(),
                git_repository: None,
            });
        }
        skills.sort_by(|left, right| {
            left.command
                .cmp(&right.command)
                .then_with(|| source_label(&left.source).cmp(&source_label(&right.source)))
        });
        Ok(skills)
    }

    pub fn preview_local(
        &self,
        source_folder: &Path,
        scope: SkillScope,
        primary_workspace: Option<&Path>,
    ) -> Result<SkillInstallPreview, SkillError> {
        // Resolve the destination as part of preview so a workspace symlink or
        // escape cannot be approved and swapped in during install.
        let _ = self.scope_root(scope, primary_workspace)?;
        let scanned = scan_skill_folder(source_folder)?;
        Ok(preview_for(
            &scanned,
            scope,
            format!("local:{}", canonical_display(source_folder)?),
        ))
    }

    pub fn install_local(
        &self,
        source_folder: &Path,
        scope: SkillScope,
        primary_workspace: Option<&Path>,
        approval_digest: &str,
        approved: bool,
    ) -> Result<SkillMutationResult, SkillError> {
        let _guard = self.lock()?;
        let root = self.scope_root(scope, primary_workspace)?;
        let scanned = scan_skill_folder(source_folder)?;
        let preview = preview_for(
            &scanned,
            scope,
            format!("local:{}", canonical_display(source_folder)?),
        );
        authorize(&preview, approval_digest, approved)?;
        self.activate(&root, scope, scanned, None)
    }

    pub fn preview_git(
        &self,
        request: &GitSkillRequest,
        scope: SkillScope,
        primary_workspace: Option<&Path>,
    ) -> Result<GitSkillPreviewOutcome, SkillError> {
        let _ = self.scope_root(scope, primary_workspace)?;
        let validated = validate_git_request(request)?;
        let (checkout, pinned_commit) = self.acquire_git(&validated)?;
        if checkout.skill_root.join("SKILL.md").is_file() || validated.subdirectory.is_some() {
            let scanned = scan_skill_folder(&checkout.skill_root)?;
            let preview = preview_for(&scanned, scope, git_origin(&validated, &pinned_commit));
            return Ok(GitSkillPreviewOutcome::Preview {
                pinned_commit,
                preview,
            });
        }
        // No subdirectory given and no SKILL.md at the repository root:
        // scan the checkout for installable skill folders and preview each
        // one so the caller can install any of them — or all at once —
        // without typing paths or previewing again. Folders that fail the
        // full scan are skipped; repositories that ship the same command
        // twice keep only the shallowest copy.
        let mut candidates = Vec::new();
        let mut seen_commands = HashSet::new();
        for relative in discover_skill_folders(&checkout.root) {
            let Ok(scanned) = scan_skill_folder(&checkout.root.join(&relative)) else {
                continue;
            };
            if !seen_commands.insert(scanned.parsed.manifest.command.clone()) {
                continue;
            }
            let subdirectory = relative.to_string_lossy().replace('\\', "/");
            let origin =
                git_candidate_origin(&validated.repository_url, &pinned_commit, &subdirectory);
            candidates.push(GitSkillCandidate {
                subdirectory,
                preview: preview_for(&scanned, scope, origin),
            });
        }
        if candidates.is_empty() {
            return Err(SkillError::Invalid(
                "the repository contains no folder with a valid SKILL.md".to_string(),
            ));
        }
        candidates.sort_by(|left, right| left.subdirectory.cmp(&right.subdirectory));
        Ok(GitSkillPreviewOutcome::Candidates {
            pinned_commit,
            candidates,
        })
    }

    /// Installs several previewed candidates from one repository snapshot in
    /// a single Git acquisition. Every entry re-scans its folder and must
    /// match the approval digest from its candidate preview — one stale or
    /// tampered skill fails the whole batch before anything is activated.
    pub fn install_git_bulk(
        &self,
        request: &GitSkillRequest,
        scope: SkillScope,
        primary_workspace: Option<&Path>,
        approvals: &[GitBulkApproval],
        approved: bool,
    ) -> Result<Vec<SkillMutationResult>, SkillError> {
        if approvals.is_empty() {
            return Err(SkillError::Invalid(
                "bulk install requires at least one approved skill".to_string(),
            ));
        }
        if approvals.len() > MAX_SKILL_CANDIDATES {
            return Err(SkillError::Invalid(format!(
                "bulk install is capped at {MAX_SKILL_CANDIDATES} skills"
            )));
        }
        let _guard = self.lock()?;
        let root = self.scope_root(scope, primary_workspace)?;
        let validated = validate_git_request(request)?;
        let (checkout, pinned_commit) = self.acquire_git(&validated)?;

        // Verify every skill against its digest before activating any, so a
        // failed batch never leaves a partial install behind.
        let canonical_checkout = fs::canonicalize(&checkout.root)
            .map_err(|error| io_at("canonicalize Git checkout", &checkout.root, error))?;
        let mut verified = Vec::new();
        let mut seen_commands = HashSet::new();
        for approval in approvals {
            let relative = validate_relative_subdirectory(approval.subdirectory.trim())?;
            let skill_root = checkout.root.join(&relative);
            let canonical_skill = fs::canonicalize(&skill_root)
                .map_err(|error| io_at("canonicalize Git skill folder", &skill_root, error))?;
            if !canonical_skill.starts_with(&canonical_checkout) {
                return Err(SkillError::Invalid(
                    "Git skill subdirectory escapes the checkout".to_string(),
                ));
            }
            let scanned = scan_skill_folder(&canonical_skill)?;
            if !seen_commands.insert(scanned.parsed.manifest.command.clone()) {
                return Err(SkillError::Conflict(format!(
                    "bulk install contains /{} more than once",
                    scanned.parsed.manifest.command
                )));
            }
            let subdirectory = relative.to_string_lossy().replace('\\', "/");
            let origin =
                git_candidate_origin(&validated.repository_url, &pinned_commit, &subdirectory);
            let preview = preview_for(&scanned, scope, origin);
            authorize(&preview, &approval.approval_digest, approved)?;
            verified.push(scanned);
        }
        let mut results = Vec::new();
        for scanned in verified {
            results.push(self.activate(&root, scope, scanned, Some(&validated.repository_url))?);
        }
        Ok(results)
    }

    pub fn install_git(
        &self,
        request: &GitSkillRequest,
        scope: SkillScope,
        primary_workspace: Option<&Path>,
        approval_digest: &str,
        approved: bool,
    ) -> Result<SkillMutationResult, SkillError> {
        let _guard = self.lock()?;
        let root = self.scope_root(scope, primary_workspace)?;
        let validated = validate_git_request(request)?;
        let (checkout, pinned_commit) = self.acquire_git(&validated)?;
        let scanned = scan_skill_folder(&checkout.skill_root)?;
        let preview = preview_for(&scanned, scope, git_origin(&validated, &pinned_commit));
        authorize(&preview, approval_digest, approved)?;
        self.activate(&root, scope, scanned, Some(&validated.repository_url))
    }

    pub fn set_enabled(
        &self,
        scope: SkillScope,
        primary_workspace: Option<&Path>,
        command: &str,
        enabled: bool,
    ) -> Result<SkillMutationResult, SkillError> {
        let _guard = self.lock()?;
        self.set_enabled_locked(scope, primary_workspace, command, enabled)
    }

    /// Same-repo group version of [`Self::set_enabled`] — one lock for the
    /// whole batch. Stops at the first failure; commands already applied
    /// stay applied (each is independently durable), so the caller sees
    /// exactly how far the batch got via the error and the partial results.
    pub fn set_enabled_many(
        &self,
        scope: SkillScope,
        primary_workspace: Option<&Path>,
        commands: &[String],
        enabled: bool,
    ) -> Result<Vec<SkillMutationResult>, SkillError> {
        let _guard = self.lock()?;
        commands
            .iter()
            .map(|command| self.set_enabled_locked(scope, primary_workspace, command, enabled))
            .collect()
    }

    fn set_enabled_locked(
        &self,
        scope: SkillScope,
        primary_workspace: Option<&Path>,
        command: &str,
        enabled: bool,
    ) -> Result<SkillMutationResult, SkillError> {
        let root = self.scope_root(scope, primary_workspace)?;
        let command = validate_command(command)?;
        let active = root.join(&command);
        if !plain_directory_exists(&active, "active skill")? {
            return Err(SkillError::NotFound(format!(
                "/{command} in {} skills",
                scope.label()
            )));
        }
        let scanned = scan_skill_folder(&active)?;
        let mut state = load_state(&root)?;
        verify_managed_active(&state, &command, &scanned.sha256)?;
        let entry = state
            .skills
            .entry(command.clone())
            .or_insert(ManagedSkillState {
                enabled: true,
                managed: false,
                active_sha256: None,
                history: Vec::new(),
                origin_repository: None,
            });
        entry.enabled = enabled;
        save_state(&root, &state)?;
        mutation_result(&state, &command, scope)
    }

    pub fn uninstall(
        &self,
        scope: SkillScope,
        primary_workspace: Option<&Path>,
        command: &str,
    ) -> Result<SkillMutationResult, SkillError> {
        let _guard = self.lock()?;
        self.uninstall_locked(scope, primary_workspace, command)
    }

    /// Same-repo group version of [`Self::uninstall`] — see
    /// [`Self::set_enabled_many`] for the partial-progress-on-error contract.
    pub fn uninstall_many(
        &self,
        scope: SkillScope,
        primary_workspace: Option<&Path>,
        commands: &[String],
    ) -> Result<Vec<SkillMutationResult>, SkillError> {
        let _guard = self.lock()?;
        commands
            .iter()
            .map(|command| self.uninstall_locked(scope, primary_workspace, command))
            .collect()
    }

    fn uninstall_locked(
        &self,
        scope: SkillScope,
        primary_workspace: Option<&Path>,
        command: &str,
    ) -> Result<SkillMutationResult, SkillError> {
        let root = self.scope_root(scope, primary_workspace)?;
        let command = validate_command(command)?;
        let active = root.join(&command);
        if !plain_directory_exists(&active, "active skill")? {
            return Err(SkillError::NotFound(format!(
                "/{command} in {} skills",
                scope.label()
            )));
        }
        let scanned = scan_skill_folder(&active)?;
        let mut state = load_state(&root)?;
        verify_managed_active(&state, &command, &scanned.sha256)?;
        let history = archive_active(&root, &command, &active, &scanned)?;
        let archived_path = history_path(&root, &command, &history)?;
        let entry = state
            .skills
            .entry(command.clone())
            .or_insert(ManagedSkillState {
                enabled: false,
                managed: true,
                active_sha256: None,
                history: Vec::new(),
                origin_repository: None,
            });
        entry.enabled = false;
        entry.managed = true;
        entry.active_sha256 = None;
        entry.history.push(history);
        let pruned = prune_history(&root, &command, entry)?;
        if let Err(error) = save_state(&root, &state) {
            if let Err(restore_error) = fs::rename(&archived_path, &active) {
                return Err(SkillError::Conflict(format!(
                    "{error}; additionally could not restore the active skill: {restore_error}"
                )));
            }
            return Err(error);
        }
        cleanup_pruned_history(pruned);
        mutation_result(&state, &command, scope)
    }

    pub fn rollback(
        &self,
        scope: SkillScope,
        primary_workspace: Option<&Path>,
        command: &str,
    ) -> Result<SkillMutationResult, SkillError> {
        let _guard = self.lock()?;
        self.rollback_locked(scope, primary_workspace, command)
    }

    /// Same-repo group version of [`Self::rollback`] — see
    /// [`Self::set_enabled_many`] for the partial-progress-on-error contract.
    pub fn rollback_many(
        &self,
        scope: SkillScope,
        primary_workspace: Option<&Path>,
        commands: &[String],
    ) -> Result<Vec<SkillMutationResult>, SkillError> {
        let _guard = self.lock()?;
        commands
            .iter()
            .map(|command| self.rollback_locked(scope, primary_workspace, command))
            .collect()
    }

    fn rollback_locked(
        &self,
        scope: SkillScope,
        primary_workspace: Option<&Path>,
        command: &str,
    ) -> Result<SkillMutationResult, SkillError> {
        let root = self.scope_root(scope, primary_workspace)?;
        let command = validate_command(command)?;
        let mut state = load_state(&root)?;
        let entry = state
            .skills
            .get_mut(&command)
            .ok_or_else(|| SkillError::NotFound(format!("rollback history for /{command}")))?;
        let selected_index = entry
            .history
            .iter()
            .rposition(|record| history_path(&root, &command, record).is_ok_and(|p| p.is_dir()))
            .ok_or_else(|| SkillError::NotFound(format!("rollback history for /{command}")))?;
        let selected = entry.history.remove(selected_index);
        validate_history_record(&selected)?;
        let selected_path = history_path(&root, &command, &selected)?;
        let selected_scan = scan_skill_folder(&selected_path)?;
        if selected_scan.sha256 != selected.sha256 {
            return Err(SkillError::Conflict(format!(
                "rollback snapshot for /{command} failed its stored digest"
            )));
        }

        let active = root.join(&command);
        let archived_current = if plain_directory_exists(&active, "active skill")? {
            let current = scan_skill_folder(&active)?;
            if entry.managed && entry.active_sha256.as_deref() != Some(current.sha256.as_str()) {
                return Err(SkillError::Conflict(format!(
                    "managed skill /{command} changed outside the approved install flow"
                )));
            }
            Some(archive_active(&root, &command, &active, &current)?)
        } else {
            None
        };
        if let Err(error) = fs::rename(&selected_path, &active) {
            if let Some(current) = &archived_current {
                if let Ok(path) = history_path(&root, &command, current) {
                    let _ = fs::rename(path, &active);
                }
            }
            return Err(io_at("activate rollback", &active, error));
        }
        let archived_current_for_restore = archived_current.clone();
        if let Some(current) = archived_current {
            entry.history.push(current);
        }
        entry.enabled = true;
        entry.managed = true;
        entry.active_sha256 = Some(selected.sha256);
        let pruned = prune_history(&root, &command, entry)?;
        if let Err(error) = save_state(&root, &state) {
            let mut restore_errors = Vec::new();
            if let Err(restore_error) = fs::rename(&active, &selected_path) {
                restore_errors.push(format!("restore selected history: {restore_error}"));
            }
            if let Some(current) = &archived_current_for_restore {
                match history_path(&root, &command, current) {
                    Ok(path) => {
                        if let Err(restore_error) = fs::rename(path, &active) {
                            restore_errors
                                .push(format!("restore prior active skill: {restore_error}"));
                        }
                    }
                    Err(restore_error) => restore_errors.push(restore_error.to_string()),
                }
            }
            if restore_errors.is_empty() {
                return Err(error);
            }
            return Err(SkillError::Conflict(format!(
                "{error}; rollback recovery also failed: {}",
                restore_errors.join("; ")
            )));
        }
        cleanup_pruned_history(pruned);
        mutation_result(&state, &command, scope)
    }

    fn activate(
        &self,
        root: &Path,
        scope: SkillScope,
        scanned: ScannedSkill,
        origin_repository: Option<&str>,
    ) -> Result<SkillMutationResult, SkillError> {
        let command = scanned.parsed.manifest.command.clone();
        let destination = root.join(&command);
        let staging = root.join(format!("{STAGING_PREFIX}{}", Uuid::new_v4()));
        copy_scanned_tree(&scanned, &staging)?;
        let staged_scan = scan_skill_folder(&staging)?;
        if staged_scan.sha256 != scanned.sha256 {
            let _ = remove_plain_tree(&staging);
            return Err(SkillError::Conflict(
                "staged skill bytes did not match the approved digest".to_string(),
            ));
        }

        let mut state = load_state(root)?;
        let old_history = if plain_directory_exists(&destination, "active skill")? {
            let old = scan_skill_folder(&destination)?;
            if let Err(error) = verify_managed_active(&state, &command, &old.sha256) {
                let _ = remove_plain_tree(&staging);
                return Err(error);
            }
            if old.sha256 == scanned.sha256 {
                remove_plain_tree(&staging)?;
                let entry = state
                    .skills
                    .entry(command.clone())
                    .or_insert(ManagedSkillState {
                        enabled: true,
                        managed: true,
                        active_sha256: Some(scanned.sha256.clone()),
                        history: Vec::new(),
                        origin_repository: origin_repository.map(str::to_string),
                    });
                entry.enabled = true;
                entry.managed = true;
                entry.active_sha256 = Some(scanned.sha256);
                entry.origin_repository = origin_repository.map(str::to_string);
                save_state(root, &state)?;
                return mutation_result(&state, &command, scope);
            }
            Some(archive_active(root, &command, &destination, &old)?)
        } else {
            None
        };

        if let Err(error) = fs::rename(&staging, &destination) {
            if let Some(record) = &old_history {
                if let Ok(previous) = history_path(root, &command, record) {
                    let _ = fs::rename(previous, &destination);
                }
            }
            let _ = remove_plain_tree(&staging);
            return Err(io_at("activate skill", &destination, error));
        }

        let entry = state
            .skills
            .entry(command.clone())
            .or_insert(ManagedSkillState {
                enabled: true,
                managed: true,
                active_sha256: None,
                history: Vec::new(),
                origin_repository: origin_repository.map(str::to_string),
            });
        entry.origin_repository = origin_repository.map(str::to_string);
        let old_history_for_restore = old_history.clone();
        if let Some(record) = old_history {
            entry.history.push(record);
        }
        entry.enabled = true;
        entry.managed = true;
        entry.active_sha256 = Some(scanned.sha256);
        let pruned = prune_history(root, &command, entry)?;
        if let Err(error) = save_state(root, &state) {
            let mut restore_errors = Vec::new();
            if let Err(restore_error) = fs::rename(&destination, &staging) {
                restore_errors.push(format!("remove newly activated skill: {restore_error}"));
            }
            if let Some(previous) = &old_history_for_restore {
                match history_path(root, &command, previous) {
                    Ok(path) => {
                        if let Err(restore_error) = fs::rename(path, &destination) {
                            restore_errors.push(format!("restore previous skill: {restore_error}"));
                        }
                    }
                    Err(restore_error) => restore_errors.push(restore_error.to_string()),
                }
            }
            let _ = remove_plain_tree(&staging);
            if restore_errors.is_empty() {
                return Err(error);
            }
            return Err(SkillError::Conflict(format!(
                "{error}; activation recovery also failed: {}",
                restore_errors.join("; ")
            )));
        }
        cleanup_pruned_history(pruned);
        mutation_result(&state, &command, scope)
    }

    fn discover_root(
        &self,
        scope: SkillScope,
        root: &Path,
        skills: &mut Vec<SkillDescriptor>,
        active_commands: &mut BTreeMap<String, String>,
    ) -> Result<(), SkillError> {
        let state = load_state(root)?;
        let mut entries = fs::read_dir(root)
            .map_err(|error| io_at("read skill root", root, error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| io_at("read skill root entry", root, error))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            if skills.len() >= MAX_DISCOVERED_SKILLS {
                return Err(SkillError::Invalid(format!(
                    "more than {MAX_DISCOVERED_SKILLS} skills were discovered"
                )));
            }
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|error| io_at("inspect skill entry", &entry.path(), error))?;
            if metadata.file_type().is_symlink() {
                return Err(SkillError::Invalid(format!(
                    "skill entry {} is a symbolic link",
                    entry.path().display()
                )));
            }
            if !metadata.is_dir() {
                return Err(SkillError::Invalid(format!(
                    "unexpected file {} in skill root",
                    entry.path().display()
                )));
            }
            let scanned = scan_skill_folder(&entry.path())?;
            let command = scanned.parsed.manifest.command.clone();
            if name.as_ref() != command {
                return Err(SkillError::Invalid(format!(
                    "skill folder {name} must match command {command}"
                )));
            }
            let managed = state.skills.get(&command);
            if managed.is_some_and(|record| {
                record.managed
                    && record
                        .active_sha256
                        .as_deref()
                        .is_some_and(|expected| expected != scanned.sha256)
            }) {
                return Err(SkillError::Conflict(format!(
                    "managed skill /{command} changed outside the approved install flow"
                )));
            }
            let source = match scope {
                SkillScope::Global => SkillSource::Global {
                    path: entry.path().to_string_lossy().to_string(),
                },
                SkillScope::Workspace => SkillSource::Workspace {
                    path: entry.path().to_string_lossy().to_string(),
                },
            };
            let eligibility = evaluate_requirements(&scanned.parsed.manifest);
            let descriptor = SkillDescriptor {
                name: scanned.parsed.manifest.name.clone(),
                description: scanned.parsed.manifest.description.clone(),
                command: command.clone(),
                version: scanned.parsed.manifest.version.clone(),
                instructions: scanned.parsed.instructions.clone(),
                sha256: scanned.sha256,
                file_count: scanned.files.len(),
                total_bytes: scanned.total_bytes,
                enabled: managed.is_none_or(|entry| entry.enabled),
                eligibility,
                supported_os: scanned.parsed.manifest.os.clone(),
                requirements: scanned.parsed.manifest.requires.clone(),
                source,
                permissions: BTreeSet::new(),
                git_repository: managed.and_then(|record| record.origin_repository.clone()),
            };
            if descriptor.enabled {
                let current_source = source_label(&descriptor.source);
                if let Some(existing) =
                    active_commands.insert(command.clone(), current_source.clone())
                {
                    return Err(SkillError::Conflict(format!(
                        "/{command} is provided by both {existing} and {current_source}"
                    )));
                }
            }
            skills.push(descriptor);
        }
        Ok(())
    }

    fn scope_root(
        &self,
        scope: SkillScope,
        primary_workspace: Option<&Path>,
    ) -> Result<PathBuf, SkillError> {
        match scope {
            SkillScope::Global => {
                ensure_plain_directory(&self.global_root, "global skill root")?;
                Ok(self.global_root.clone())
            }
            SkillScope::Workspace => {
                let workspace = primary_workspace.ok_or_else(|| {
                    SkillError::Invalid(
                        "a primary workspace is required for workspace skills".to_string(),
                    )
                })?;
                workspace_skill_root(workspace, true)?.ok_or_else(|| {
                    SkillError::Io("could not create workspace skill root".to_string())
                })
            }
        }
    }

    fn scope_root_if_present(
        &self,
        scope: SkillScope,
        primary_workspace: Option<&Path>,
    ) -> Result<Option<PathBuf>, SkillError> {
        match scope {
            SkillScope::Global => Ok(Some(self.global_root.clone())),
            SkillScope::Workspace => {
                let Some(workspace) = primary_workspace else {
                    return Ok(None);
                };
                workspace_skill_root(workspace, false)
            }
        }
    }

    /// Resolves a branch/tag/`HEAD` spec to a pinned 40-hex commit via
    /// `git ls-remote`, without touching the filesystem. Pinned SHAs pass
    /// through untouched.
    fn resolve_git_commit(&self, request: &ValidatedGitRequest) -> Result<String, SkillError> {
        let pattern = match &request.commit {
            GitCommitSpec::Pinned(sha) => return Ok(sha.clone()),
            GitCommitSpec::Reference(name) => name.as_str(),
            GitCommitSpec::DefaultHead => "HEAD",
        };
        let output = git_checked(
            &self.git_binary,
            None,
            [
                "ls-remote",
                request.repository_url.as_str(),
                pattern,
            ],
            &self.acquisition_root,
        )?;
        let listing = String::from_utf8(output.stdout)
            .map_err(|_| SkillError::Git("git ls-remote returned non-UTF-8 output".to_string()))?;
        select_ls_remote_commit(&listing, &request.commit)
    }

    /// Fetches the pinned commit into a fresh temporary checkout and returns
    /// it together with the resolved 40-hex SHA that was actually verified.
    fn acquire_git(
        &self,
        request: &ValidatedGitRequest,
    ) -> Result<(GitCheckout, String), SkillError> {
        let resolved_commit = self.resolve_git_commit(request)?;
        ensure_plain_directory(&self.acquisition_root, "skill acquisition root")?;
        let checkout_root = self
            .acquisition_root
            .join(format!("checkout-{}", Uuid::new_v4()));
        ensure_plain_directory(&checkout_root, "Git checkout")?;
        let mut checkout = GitCheckout {
            root: checkout_root,
            skill_root: PathBuf::new(),
        };

        git_checked(
            &self.git_binary,
            Some(&checkout.root),
            ["init", "--quiet"],
            &self.acquisition_root,
        )?;
        git_checked(
            &self.git_binary,
            Some(&checkout.root),
            ["remote", "add", "origin", request.repository_url.as_str()],
            &self.acquisition_root,
        )?;
        git_checked(
            &self.git_binary,
            Some(&checkout.root),
            [
                "fetch",
                "--quiet",
                "--depth=1",
                "--no-tags",
                "origin",
                resolved_commit.as_str(),
            ],
            &self.acquisition_root,
        )?;
        git_checked(
            &self.git_binary,
            Some(&checkout.root),
            ["checkout", "--quiet", "--detach", "--force", "FETCH_HEAD"],
            &self.acquisition_root,
        )?;
        let output = git_checked(
            &self.git_binary,
            Some(&checkout.root),
            ["rev-parse", "--verify", "HEAD"],
            &self.acquisition_root,
        )?;
        let head = String::from_utf8(output.stdout)
            .map_err(|_| SkillError::Git("git rev-parse returned non-UTF-8 output".to_string()))?;
        if head.trim().to_ascii_lowercase() != resolved_commit {
            return Err(SkillError::Git(format!(
                "checked out {}, expected {}",
                head.trim(),
                resolved_commit
            )));
        }
        checkout.skill_root = match &request.subdirectory {
            Some(relative) => checkout.root.join(relative),
            None => checkout.root.clone(),
        };
        let canonical_checkout = fs::canonicalize(&checkout.root)
            .map_err(|error| io_at("canonicalize Git checkout", &checkout.root, error))?;
        let canonical_skill = fs::canonicalize(&checkout.skill_root)
            .map_err(|error| io_at("canonicalize Git skill folder", &checkout.skill_root, error))?;
        if !canonical_skill.starts_with(&canonical_checkout) {
            return Err(SkillError::Invalid(
                "Git skill subdirectory escapes the checkout".to_string(),
            ));
        }
        checkout.skill_root = canonical_skill;
        Ok((checkout, resolved_commit))
    }

    fn lock(&self) -> Result<MutexGuard<'_, ()>, SkillError> {
        self.mutation
            .lock()
            .map_err(|_| SkillError::Io("native skill manager lock poisoned".to_string()))
    }
}

struct GitCheckout {
    root: PathBuf,
    skill_root: PathBuf,
}

impl Drop for GitCheckout {
    fn drop(&mut self) {
        let _ = remove_plain_tree(&self.root);
    }
}

/// What the caller asked to check out. Anything that isn't already a pinned
/// 40-hex SHA is resolved to one via `git ls-remote` before fetching — the
/// runtime only ever fetches and verifies exact commits.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GitCommitSpec {
    Pinned(String),
    Reference(String),
    DefaultHead,
}

#[derive(Debug, Clone)]
struct ValidatedGitRequest {
    repository_url: String,
    commit: GitCommitSpec,
    subdirectory: Option<PathBuf>,
}

fn validate_git_request(request: &GitSkillRequest) -> Result<ValidatedGitRequest, SkillError> {
    let url = Url::parse(request.repository_url.trim())
        .map_err(|error| SkillError::Invalid(format!("invalid Git URL: {error}")))?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err(SkillError::Invalid(
            "Git skill sources must use an absolute HTTPS URL".to_string(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(SkillError::Invalid(
            "Git skill URLs cannot contain credentials or fragments".to_string(),
        ));
    }
    let commit_input = request.commit.trim();
    let commit = if commit_input.is_empty() {
        GitCommitSpec::DefaultHead
    } else if commit_input.len() == 40 && commit_input.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        GitCommitSpec::Pinned(commit_input.to_ascii_lowercase())
    } else {
        GitCommitSpec::Reference(validate_git_reference(commit_input)?)
    };
    let subdirectory = request
        .subdirectory
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(validate_relative_subdirectory)
        .transpose()?;
    Ok(ValidatedGitRequest {
        repository_url: url.to_string(),
        commit,
        subdirectory,
    })
}

/// Accepts plain branch/tag names (`main`, `v1.2.0`, `release/2024`). Kept
/// far stricter than git's own ref rules: a conservative charset, no leading
/// separator or dash (so a ref can never be parsed as a git option), and no
/// `..`/`//`/`@{` sequences.
fn validate_git_reference(value: &str) -> Result<String, SkillError> {
    let valid_byte =
        |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-');
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(valid_byte)
        || value.starts_with(['-', '/', '.'])
        || value.ends_with('/')
        || value.contains("..")
        || value.contains("//")
    {
        return Err(SkillError::Invalid(
            "Git skill commit must be a 40-hex SHA, a valid branch/tag name, or empty for the default branch".to_string(),
        ));
    }
    Ok(value.to_string())
}

fn validate_relative_subdirectory(value: &str) -> Result<PathBuf, SkillError> {
    if value.len() > MAX_RELATIVE_PATH_BYTES {
        return Err(SkillError::Invalid(
            "Git skill subdirectory is too long".to_string(),
        ));
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return Err(SkillError::Invalid(
            "Git skill subdirectory must be relative".to_string(),
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) if part != ".git" => normalized.push(part),
            _ => {
                return Err(SkillError::Invalid(
                    "Git skill subdirectory contains a forbidden path component".to_string(),
                ))
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(SkillError::Invalid(
            "Git skill subdirectory is empty".to_string(),
        ));
    }
    Ok(normalized)
}

/// Picks the commit a `git ls-remote` listing resolves to. Branch refs win
/// over tags; annotated tags prefer the peeled `^{}` commit (fetching the
/// unpeeled tag-object SHA would fail the post-checkout `rev-parse HEAD`
/// verification, which always reports the commit).
fn select_ls_remote_commit(listing: &str, spec: &GitCommitSpec) -> Result<String, SkillError> {
    let mut rows = Vec::new();
    for line in listing.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(sha), Some(name)) = (parts.next(), parts.next()) {
            if sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                rows.push((sha.to_ascii_lowercase(), name.to_string()));
            }
        }
    }
    let lookup = |target: String| rows.iter().find(|(_, name)| *name == target);
    let selected = match spec {
        GitCommitSpec::Pinned(sha) => return Ok(sha.clone()),
        GitCommitSpec::DefaultHead => lookup("HEAD".to_string()),
        GitCommitSpec::Reference(reference) => lookup(format!("refs/heads/{reference}"))
            .or_else(|| lookup(format!("refs/tags/{reference}^{{}}")))
            .or_else(|| lookup(format!("refs/tags/{reference}"))),
    };
    match selected {
        Some((sha, _)) => Ok(sha.clone()),
        None => Err(SkillError::Git(match spec {
            GitCommitSpec::DefaultHead => {
                "the remote did not advertise a default branch (HEAD)".to_string()
            }
            GitCommitSpec::Reference(reference) => {
                format!("the remote has no branch or tag named '{reference}'")
            }
            GitCommitSpec::Pinned(_) => unreachable!("pinned commits return early"),
        })),
    }
}

/// Cap on skill folders reported from one repository scan.
const MAX_SKILL_CANDIDATES: usize = 64;
/// Cap on directories visited while scanning a checkout for candidates.
const MAX_CANDIDATE_SCAN_DIRS: usize = 4_096;

/// Breadth-first scan of a Git checkout for folders containing a `SKILL.md`.
/// Dot-directories (`.git`, `.claude`, per-editor rule dirs) and symlinks
/// are skipped. Returns checkout-relative paths ordered shallowest-first so
/// that when a repository ships the same skill twice (e.g. `skills/x` and a
/// generated `plugins/y/skills/x` copy), command dedup keeps the shallower,
/// canonical one.
fn discover_skill_folders(checkout_root: &Path) -> Vec<PathBuf> {
    let mut folders = Vec::new();
    let mut pending = VecDeque::new();
    pending.push_back((checkout_root.to_path_buf(), PathBuf::new(), 0usize));
    let mut visited = 0usize;
    while let Some((directory, relative, depth)) = pending.pop_front() {
        visited += 1;
        if visited > MAX_CANDIDATE_SCAN_DIRS || folders.len() >= MAX_SKILL_CANDIDATES {
            break;
        }
        let skill_md = directory.join("SKILL.md");
        if let Ok(metadata) = fs::symlink_metadata(&skill_md) {
            if metadata.is_file() && !relative.as_os_str().is_empty() {
                folders.push(relative);
                // A skill folder's subtree can't contain another skill.
                continue;
            }
        }
        if depth >= MAX_SKILL_DEPTH {
            continue;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        let mut children = Vec::new();
        for entry in entries.flatten() {
            let name_text = entry.file_name().to_string_lossy().into_owned();
            if name_text.starts_with('.') {
                continue;
            }
            let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if metadata.is_dir() {
                children.push((entry.path(), relative.join(&name_text), depth + 1));
            }
        }
        // Deterministic order within one level.
        children.sort_by(|left, right| left.1.cmp(&right.1));
        pending.extend(children);
    }
    folders
}

fn git_origin(request: &ValidatedGitRequest, resolved_commit: &str) -> String {
    match &request.subdirectory {
        Some(path) => git_candidate_origin(
            &request.repository_url,
            resolved_commit,
            &path.to_string_lossy(),
        ),
        None => format!("git:{}@{}", request.repository_url, resolved_commit),
    }
}

/// Origins always use forward slashes for the subdirectory so approval
/// digests match across the preview (candidate) and install paths on every
/// platform.
fn git_candidate_origin(repository_url: &str, commit: &str, subdirectory: &str) -> String {
    let subdirectory = subdirectory.replace('\\', "/");
    format!("git:{repository_url}@{commit}:{subdirectory}")
}

fn git_checked<'a, I>(
    git_binary: &Path,
    working_directory: Option<&Path>,
    args: I,
    safe_hooks_root: &Path,
) -> Result<Output, SkillError>
where
    I: IntoIterator<Item = &'a str>,
{
    let hooks = safe_hooks_root.join("empty-hooks");
    ensure_plain_directory(&hooks, "empty Git hooks directory")?;
    let mut command = Command::new(git_binary);
    if let Some(directory) = working_directory {
        command.arg("-C").arg(directory);
    }
    command
        .arg("-c")
        .arg(format!("core.hooksPath={}", hooks.to_string_lossy()))
        .arg("-c")
        .arg("protocol.allow=never")
        .arg("-c")
        .arg("protocol.https.allow=always")
        .arg("-c")
        .arg("http.followRedirects=initial")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .env("GIT_PROTOCOL_FROM_USER", "0")
        .env("GIT_ALLOW_PROTOCOL", "https")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| SkillError::Git(format!("could not start git: {error}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SkillError::Git("could not capture git stdout".to_string()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| SkillError::Git("could not capture git stderr".to_string()))?;
    let stdout_reader = std::thread::spawn(move || read_bounded_process_output(stdout));
    let stderr_reader = std::thread::spawn(move || read_bounded_process_output(stderr));
    let deadline = Instant::now() + GIT_OPERATION_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(SkillError::Git(format!(
                    "git exceeded its {} second timeout",
                    GIT_OPERATION_TIMEOUT.as_secs()
                )));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(SkillError::Git(format!("could not monitor git: {error}")));
            }
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| SkillError::Git("git stdout reader panicked".to_string()))?
        .map_err(|error| SkillError::Git(format!("could not read git stdout: {error}")))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| SkillError::Git("git stderr reader panicked".to_string()))?
        .map_err(|error| SkillError::Git(format!("could not read git stderr: {error}")))?;
    let output = Output {
        status,
        stdout,
        stderr,
    };
    if output.status.success() {
        return Ok(output);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let summary = stderr.lines().take(4).collect::<Vec<_>>().join("\n");
    Err(SkillError::Git(if summary.is_empty() {
        format!("git exited with {}", output.status)
    } else {
        summary
    }))
}

fn read_bounded_process_output(mut reader: impl Read) -> io::Result<Vec<u8>> {
    const MAX_PROCESS_OUTPUT_BYTES: u64 = 64 * 1024;
    let mut output = Vec::new();
    reader
        .by_ref()
        .take(MAX_PROCESS_OUTPUT_BYTES + 1)
        .read_to_end(&mut output)?;
    output.truncate(MAX_PROCESS_OUTPUT_BYTES as usize);
    Ok(output)
}

fn scan_skill_folder(root: &Path) -> Result<ScannedSkill, SkillError> {
    let supplied =
        fs::symlink_metadata(root).map_err(|error| io_at("inspect skill folder", root, error))?;
    if supplied.file_type().is_symlink() || !supplied.is_dir() {
        return Err(SkillError::Invalid(format!(
            "{} must be a real directory, not a symlink or special file",
            root.display()
        )));
    }
    let canonical_root =
        fs::canonicalize(root).map_err(|error| io_at("canonicalize skill folder", root, error))?;
    let mut pending = vec![(canonical_root.clone(), PathBuf::new(), 0usize)];
    let mut files = Vec::<ScannedFile>::new();
    let mut total_bytes = 0u64;
    while let Some((directory, relative_directory, depth)) = pending.pop() {
        if depth > MAX_SKILL_DEPTH {
            return Err(SkillError::Invalid(format!(
                "skill directory depth exceeds {MAX_SKILL_DEPTH}"
            )));
        }
        let mut entries = fs::read_dir(&directory)
            .map_err(|error| io_at("read skill directory", &directory, error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| io_at("read skill entry", &directory, error))?;
        entries.sort_by_key(|entry| entry.file_name());
        // Reverse because pending is a stack and the final digest is sorted
        // again; this keeps traversal deterministic for early limit errors.
        for entry in entries.into_iter().rev() {
            let file_name = entry.file_name();
            if depth == 0 && file_name == ".git" {
                continue;
            }
            let relative = relative_directory.join(&file_name);
            let relative_text = normalized_relative_path(&relative)?;
            if relative_text.len() > MAX_RELATIVE_PATH_BYTES {
                return Err(SkillError::Invalid(format!(
                    "skill path {relative_text} exceeds {MAX_RELATIVE_PATH_BYTES} bytes"
                )));
            }
            let path = entry.path();
            let before = fs::symlink_metadata(&path)
                .map_err(|error| io_at("inspect skill tree entry", &path, error))?;
            if before.file_type().is_symlink() {
                return Err(SkillError::Invalid(format!(
                    "symbolic links are forbidden in skills: {relative_text}"
                )));
            }
            if before.is_dir() {
                let canonical = fs::canonicalize(&path)
                    .map_err(|error| io_at("canonicalize skill directory", &path, error))?;
                if !canonical.starts_with(&canonical_root) {
                    return Err(SkillError::Invalid(format!(
                        "skill directory escapes its root: {relative_text}"
                    )));
                }
                pending.push((canonical, relative, depth + 1));
                continue;
            }
            if !before.is_file() {
                return Err(SkillError::Invalid(format!(
                    "special files are forbidden in skills: {relative_text}"
                )));
            }
            if files.len() >= MAX_SKILL_FILES {
                return Err(SkillError::Invalid(format!(
                    "skill contains more than {MAX_SKILL_FILES} files"
                )));
            }
            let limit = if relative == Path::new("SKILL.md") {
                MAX_SKILL_MD_BYTES
            } else {
                MAX_SKILL_FILE_BYTES
            };
            if before.len() > limit {
                return Err(SkillError::Invalid(format!(
                    "skill file {relative_text} exceeds {limit} bytes"
                )));
            }
            let remaining = MAX_SKILL_TOTAL_BYTES.saturating_sub(total_bytes);
            if before.len() > remaining {
                return Err(SkillError::Invalid(format!(
                    "skill exceeds {MAX_SKILL_TOTAL_BYTES} total bytes"
                )));
            }
            let canonical = fs::canonicalize(&path)
                .map_err(|error| io_at("canonicalize skill file", &path, error))?;
            if !canonical.starts_with(&canonical_root) {
                return Err(SkillError::Invalid(format!(
                    "skill file escapes its root: {relative_text}"
                )));
            }
            let mut bytes = Vec::with_capacity(before.len() as usize);
            File::open(&path)
                .map_err(|error| io_at("open skill file", &path, error))?
                .take(remaining.saturating_add(1))
                .read_to_end(&mut bytes)
                .map_err(|error| io_at("read skill file", &path, error))?;
            let after = fs::symlink_metadata(&path)
                .map_err(|error| io_at("reinspect skill file", &path, error))?;
            if after.file_type().is_symlink()
                || !after.is_file()
                || after.len() != bytes.len() as u64
            {
                return Err(SkillError::Conflict(format!(
                    "skill file changed while it was being read: {relative_text}"
                )));
            }
            if bytes.len() as u64 > limit {
                return Err(SkillError::Invalid(format!(
                    "skill file {relative_text} exceeds {limit} bytes"
                )));
            }
            total_bytes = total_bytes.saturating_add(bytes.len() as u64);
            if total_bytes > MAX_SKILL_TOTAL_BYTES {
                return Err(SkillError::Invalid(format!(
                    "skill exceeds {MAX_SKILL_TOTAL_BYTES} total bytes"
                )));
            }
            files.push(ScannedFile { relative, bytes });
        }
    }
    files.sort_by(|left, right| left.relative.cmp(&right.relative));
    let skill_md = files
        .iter()
        .find(|file| file.relative == Path::new("SKILL.md"))
        .ok_or_else(|| SkillError::Invalid("skill root must contain SKILL.md".to_string()))?;
    let parsed = parse_skill_md(&skill_md.bytes)?;
    let sha256 = hash_tree(&files)?;
    Ok(ScannedSkill {
        parsed,
        sha256,
        files,
        total_bytes,
    })
}

fn parse_skill_md(bytes: &[u8]) -> Result<ParsedSkill, SkillError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| SkillError::Invalid("SKILL.md must be UTF-8".to_string()))?;
    if text.starts_with('\u{feff}') {
        return Err(SkillError::Invalid(
            "SKILL.md cannot start with a byte-order mark".to_string(),
        ));
    }
    let mut offset = 0usize;
    let mut first_end = None;
    let mut closing = None;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if index == 0 {
            if trimmed != "---" {
                return Err(SkillError::Invalid(
                    "SKILL.md must start with YAML frontmatter delimited by ---".to_string(),
                ));
            }
            first_end = Some(line.len());
        } else if trimmed == "---" {
            closing = Some((offset, offset + line.len()));
            break;
        }
        offset += line.len();
    }
    let first_end = first_end
        .ok_or_else(|| SkillError::Invalid("SKILL.md is missing frontmatter".to_string()))?;
    let (frontmatter_end, body_start) = closing.ok_or_else(|| {
        SkillError::Invalid("SKILL.md frontmatter is missing its closing ---".to_string())
    })?;
    if frontmatter_end < first_end {
        return Err(SkillError::Invalid(
            "SKILL.md frontmatter delimiter is malformed".to_string(),
        ));
    }
    let frontmatter = &text[first_end..frontmatter_end];
    if frontmatter.len() > MAX_FRONTMATTER_BYTES {
        return Err(SkillError::Invalid(format!(
            "SKILL.md frontmatter exceeds {MAX_FRONTMATTER_BYTES} bytes"
        )));
    }
    if frontmatter.contains('\t') || frontmatter.chars().any(is_forbidden_control) {
        return Err(SkillError::Invalid(
            "SKILL.md frontmatter contains forbidden control characters".to_string(),
        ));
    }
    // The schema does not need YAML aliases, anchors, merge keys, includes, or
    // multiple documents. Reject them and use tight parser budgets so a tiny
    // frontmatter block cannot amplify into expensive replay work.
    let yaml_options = serde_saphyr::options! {
        budget: serde_saphyr::budget! {
            max_events: 512,
            max_aliases: 0,
            max_anchors: 0,
            max_depth: 8,
            max_inclusion_depth: 0,
            max_documents: 1,
            max_nodes: 256,
            max_total_scalar_bytes: MAX_FRONTMATTER_BYTES,
            max_total_comment_bytes: 8 * 1024,
            max_merge_keys: 0,
        },
        duplicate_keys: serde_saphyr::options::DuplicateKeyPolicy::Error,
        merge_keys: serde_saphyr::options::MergeKeyPolicy::Error,
        alias_limits: serde_saphyr::alias_limits! {
            max_total_replayed_events: 0,
            max_replay_stack_depth: 0,
            max_alias_expansions_per_anchor: 0,
        },
        strict_booleans: true,
    };
    let raw: RawSkillManifest = serde_saphyr::from_str_with_options(frontmatter, yaml_options)
        .map_err(|error| SkillError::Invalid(format!("frontmatter schema error: {error}")))?;
    let instructions = text[body_start..].trim();
    if instructions.is_empty() {
        return Err(SkillError::Invalid(
            "SKILL.md instruction body cannot be empty".to_string(),
        ));
    }
    if instructions.chars().any(is_forbidden_control) {
        return Err(SkillError::Invalid(
            "SKILL.md instructions contain forbidden control characters".to_string(),
        ));
    }
    let manifest = normalize_manifest(raw)?;
    Ok(ParsedSkill {
        manifest,
        instructions: instructions.to_string(),
    })
}

fn is_forbidden_control(character: char) -> bool {
    character.is_control() && !matches!(character, '\n' | '\r' | '\t')
}

fn normalize_manifest(raw: RawSkillManifest) -> Result<SkillManifest, SkillError> {
    if raw.os.len() > 3 || raw.requires.bins.len() > 64 || raw.requires.env.len() > 64 {
        return Err(SkillError::Invalid(
            "frontmatter requirement lists exceed their bounded sizes".to_string(),
        ));
    }
    let name = bounded_trimmed(&raw.name, "name", 1, 96)?;
    let description = bounded_trimmed(&raw.description, "description", 1, 1024)?;
    let command = match &raw.command {
        Some(command) => validate_command(command)?,
        // Ecosystem SKILL.md files (Claude Code format) have no `command`
        // key — the slash command is the skill's name. Slugify the common
        // benign divergences (case, spaces) before validating.
        None => validate_command(&name.to_ascii_lowercase().replace(' ', "-")).map_err(|error| {
            SkillError::Invalid(format!(
                "frontmatter has no 'command' and its 'name' is not usable as a slash command: {error}"
            ))
        })?,
    };
    let version = match &raw.version {
        Some(version) => validate_version(version)?,
        None => "0.0.0".to_string(),
    };
    let os = unique_set(raw.os, "os")?;
    let bins = raw
        .requires
        .bins
        .into_iter()
        .map(|value| validate_requirement_name(&value, "binary", true))
        .collect::<Result<Vec<_>, _>>()?;
    let env = raw
        .requires
        .env
        .into_iter()
        .map(|value| validate_requirement_name(&value, "environment variable", false))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(SkillManifest {
        name,
        description,
        command,
        version,
        os,
        requires: SkillRequirements {
            bins: unique_set(bins, "requires.bins")?,
            env: unique_set(env, "requires.env")?,
        },
    })
}

fn unique_set<T: Ord + Clone>(values: Vec<T>, label: &str) -> Result<BTreeSet<T>, SkillError> {
    let len = values.len();
    let set = values.into_iter().collect::<BTreeSet<_>>();
    if set.len() != len {
        return Err(SkillError::Invalid(format!("{label} contains duplicates")));
    }
    Ok(set)
}

fn bounded_trimmed(
    value: &str,
    label: &str,
    minimum: usize,
    maximum: usize,
) -> Result<String, SkillError> {
    let value = value.trim();
    if !(minimum..=maximum).contains(&value.len()) {
        return Err(SkillError::Invalid(format!(
            "{label} must contain {minimum} to {maximum} UTF-8 bytes"
        )));
    }
    if value.chars().any(is_forbidden_control) {
        return Err(SkillError::Invalid(format!(
            "{label} contains forbidden control characters"
        )));
    }
    Ok(value.to_string())
}

fn validate_command(value: &str) -> Result<String, SkillError> {
    let value = value.trim().strip_prefix('/').unwrap_or(value.trim());
    if value.is_empty() || value.len() > 32 {
        return Err(SkillError::Invalid(
            "command must contain 1 to 32 ASCII characters".to_string(),
        ));
    }
    let mut bytes = value.bytes();
    let first = bytes.next().unwrap_or_default();
    if !first.is_ascii_lowercase()
        || !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || value.ends_with('-')
        || value.contains("--")
    {
        return Err(SkillError::Invalid(
            "command must match [a-z][a-z0-9-]{0,31} without repeated/trailing dashes".to_string(),
        ));
    }
    if RESERVED_SLASH_COMMANDS.contains(&value) {
        return Err(SkillError::Conflict(format!(
            "/{value} is reserved by Little Monkey"
        )));
    }
    Ok(value.to_string())
}

fn validate_version(value: &str) -> Result<String, SkillError> {
    let value = value.trim();
    let components = value.split('.').collect::<Vec<_>>();
    if components.len() != 3
        || components.iter().any(|component| {
            component.is_empty()
                || !component.bytes().all(|byte| byte.is_ascii_digit())
                || (component.len() > 1 && component.starts_with('0'))
                || component.parse::<u32>().is_err()
        })
    {
        return Err(SkillError::Invalid(
            "version must be a canonical MAJOR.MINOR.PATCH value".to_string(),
        ));
    }
    Ok(value.to_string())
}

fn validate_requirement_name(
    value: &str,
    label: &str,
    allow_dash_dot: bool,
) -> Result<String, SkillError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 96 {
        return Err(SkillError::Invalid(format!(
            "{label} name must contain 1 to 96 ASCII characters"
        )));
    }
    let valid = value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || byte == b'_'
            || (allow_dash_dot && matches!(byte, b'-' | b'.' | b'+'))
    });
    if !valid || (!allow_dash_dot && value.as_bytes()[0].is_ascii_digit()) {
        return Err(SkillError::Invalid(format!(
            "{label} name contains unsafe characters"
        )));
    }
    Ok(value.to_string())
}

fn evaluate_requirements(manifest: &SkillManifest) -> SkillEligibility {
    let current = SupportedOs::current();
    let unsupported_os =
        !manifest.os.is_empty() && current.is_none_or(|current| !manifest.os.contains(&current));
    let missing_bins = manifest
        .requires
        .bins
        .iter()
        .filter(|binary| !binary_on_path(binary))
        .cloned()
        .collect::<Vec<_>>();
    let missing_env = manifest
        .requires
        .env
        .iter()
        .filter(|name| std::env::var_os(name).is_none())
        .cloned()
        .collect::<Vec<_>>();
    SkillEligibility {
        eligible: !unsupported_os && missing_bins.is_empty() && missing_env.is_empty(),
        current_os: current
            .map(|value| format!("{value:?}").to_ascii_lowercase())
            .unwrap_or_else(|| std::env::consts::OS.to_string()),
        unsupported_os,
        missing_bins,
        missing_env,
    }
}

fn eligible_everywhere() -> SkillEligibility {
    SkillEligibility {
        eligible: true,
        current_os: SupportedOs::current()
            .map(|value| format!("{value:?}").to_ascii_lowercase())
            .unwrap_or_else(|| std::env::consts::OS.to_string()),
        unsupported_os: false,
        missing_bins: Vec::new(),
        missing_env: Vec::new(),
    }
}

fn binary_on_path(binary: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    #[cfg(windows)]
    let extensions = std::env::var_os("PATHEXT")
        .map(|value| {
            value
                .to_string_lossy()
                .split(';')
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| vec![".COM".into(), ".EXE".into(), ".BAT".into(), ".CMD".into()]);
    std::env::split_paths(&path).any(|directory| {
        let direct = directory.join(binary);
        if direct.is_file() {
            return true;
        }
        #[cfg(windows)]
        {
            return extensions
                .iter()
                .any(|extension| directory.join(format!("{binary}{extension}")).is_file());
        }
        #[cfg(not(windows))]
        false
    })
}

fn preview_for(scanned: &ScannedSkill, scope: SkillScope, origin: String) -> SkillInstallPreview {
    let approval_digest = install_approval_digest(
        scope,
        &origin,
        &scanned.parsed.manifest.command,
        &scanned.parsed.manifest.version,
        &scanned.sha256,
    );
    SkillInstallPreview {
        scope,
        name: scanned.parsed.manifest.name.clone(),
        description: scanned.parsed.manifest.description.clone(),
        command: scanned.parsed.manifest.command.clone(),
        version: scanned.parsed.manifest.version.clone(),
        sha256: scanned.sha256.clone(),
        file_count: scanned.files.len(),
        total_bytes: scanned.total_bytes,
        eligibility: evaluate_requirements(&scanned.parsed.manifest),
        supported_os: scanned.parsed.manifest.os.clone(),
        requirements: scanned.parsed.manifest.requires.clone(),
        approval_digest,
        origin,
    }
}

fn authorize(
    preview: &SkillInstallPreview,
    approval_digest: &str,
    approved: bool,
) -> Result<(), SkillError> {
    if !approved {
        return Err(SkillError::Approval(
            "installation was not approved".to_string(),
        ));
    }
    validate_sha256(approval_digest, "approval digest")?;
    if approval_digest != preview.approval_digest {
        return Err(SkillError::Approval(
            "skill bytes, source, scope, or metadata changed after preview".to_string(),
        ));
    }
    Ok(())
}

fn install_approval_digest(
    scope: SkillScope,
    origin: &str,
    command: &str,
    version: &str,
    tree_digest: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"little-monkey-skill-install-approval-v1\0");
    for value in [scope.label(), origin, command, version, tree_digest] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn hash_tree(files: &[ScannedFile]) -> Result<String, SkillError> {
    let mut hasher = Sha256::new();
    hasher.update(b"little-monkey-skill-tree-v1\0");
    for file in files {
        let path = normalized_relative_path(&file.relative)?;
        hasher.update((path.len() as u64).to_be_bytes());
        hasher.update(path.as_bytes());
        hasher.update((file.bytes.len() as u64).to_be_bytes());
        hasher.update(&file.bytes);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn normalized_relative_path(path: &Path) -> Result<String, SkillError> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let text = part.to_str().ok_or_else(|| {
                    SkillError::Invalid("skill paths must be valid UTF-8".to_string())
                })?;
                if text.is_empty() || text == "." || text == ".." {
                    return Err(SkillError::Invalid(
                        "skill path contains an invalid component".to_string(),
                    ));
                }
                components.push(text);
            }
            _ => {
                return Err(SkillError::Invalid(
                    "skill path must be relative and normalized".to_string(),
                ))
            }
        }
    }
    if components.is_empty() {
        return Err(SkillError::Invalid("skill path is empty".to_string()));
    }
    Ok(components.join("/"))
}

fn copy_scanned_tree(scanned: &ScannedSkill, destination: &Path) -> Result<(), SkillError> {
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            return Err(SkillError::Conflict(format!(
                "staging path {} already exists",
                destination.display()
            )))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_at("inspect skill staging path", destination, error)),
    }
    ensure_plain_directory(destination, "skill staging directory")?;
    for file in &scanned.files {
        let path = destination.join(&file.relative);
        if !path.starts_with(destination) {
            return Err(SkillError::Invalid(
                "staged skill path escapes its destination".to_string(),
            ));
        }
        if let Some(parent) = path.parent() {
            ensure_plain_directory(parent, "skill staging subdirectory")?;
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut output = options
            .open(&path)
            .map_err(|error| io_at("create staged skill file", &path, error))?;
        set_private_file_permissions(&path)?;
        output
            .write_all(&file.bytes)
            .map_err(|error| io_at("write staged skill file", &path, error))?;
        output
            .sync_all()
            .map_err(|error| io_at("sync staged skill file", &path, error))?;
    }
    sync_directory(destination)?;
    Ok(())
}

fn workspace_skill_root(workspace: &Path, create: bool) -> Result<Option<PathBuf>, SkillError> {
    let supplied = fs::symlink_metadata(workspace)
        .map_err(|error| io_at("inspect primary workspace", workspace, error))?;
    if supplied.file_type().is_symlink() || !supplied.is_dir() {
        return Err(SkillError::Invalid(
            "primary workspace must be a real directory".to_string(),
        ));
    }
    let canonical_workspace = fs::canonicalize(workspace)
        .map_err(|error| io_at("canonicalize primary workspace", workspace, error))?;
    let little_monkey = canonical_workspace.join(".littlemonkey");
    let skills = little_monkey.join("skills");
    for (path, label) in [
        (&little_monkey, "workspace .littlemonkey directory"),
        (&skills, "workspace skill directory"),
    ] {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(SkillError::Invalid(format!(
                    "{label} cannot be a symlink or file"
                )))
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
                if let Err(create_error) = fs::create_dir(path) {
                    if create_error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(io_at("create workspace skill path", path, create_error));
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_at("inspect workspace skill path", path, error)),
        }
        let verified = fs::symlink_metadata(path)
            .map_err(|error| io_at("verify workspace skill path", path, error))?;
        if verified.file_type().is_symlink() || !verified.is_dir() {
            return Err(SkillError::Invalid(format!(
                "{label} cannot be a symlink or file"
            )));
        }
    }
    let canonical = fs::canonicalize(&skills)
        .map_err(|error| io_at("canonicalize workspace skill directory", &skills, error))?;
    if !canonical.starts_with(&canonical_workspace) {
        return Err(SkillError::Invalid(
            "workspace skill directory escapes the primary workspace".to_string(),
        ));
    }
    Ok(Some(canonical))
}

fn ensure_plain_directory(path: &Path, label: &str) -> Result<(), SkillError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(SkillError::Invalid(format!(
                "{label} {} must be a real directory",
                path.display()
            )))
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .map_err(|error| io_at(&format!("create {label}"), path, error))?;
        }
        Err(error) => return Err(io_at(&format!("inspect {label}"), path, error)),
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| io_at(&format!("verify {label}"), path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SkillError::Invalid(format!(
            "{label} {} must be a real directory",
            path.display()
        )));
    }
    Ok(())
}

fn plain_directory_exists(path: &Path, label: &str) -> Result<bool, SkillError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_at(&format!("inspect {label}"), path, error)),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(SkillError::Invalid(format!(
                "{label} {} must be a real directory",
                path.display()
            )))
        }
        Ok(_) => Ok(true),
    }
}

fn load_state(root: &Path) -> Result<RootState, SkillError> {
    let path = root.join(STATE_FILE);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(RootState::default()),
        Err(error) => Err(io_at("inspect skill state", &path, error)),
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(SkillError::Invalid(
                    "skill state must be a regular file".to_string(),
                ));
            }
            if metadata.len() > 1024 * 1024 {
                return Err(SkillError::Invalid(
                    "skill state exceeds one MiB".to_string(),
                ));
            }
            let bytes = fs::read(&path).map_err(|error| io_at("read skill state", &path, error))?;
            let state: RootState = serde_json::from_slice(&bytes)
                .map_err(|error| SkillError::Invalid(format!("invalid skill state: {error}")))?;
            if state.schema_version != NATIVE_SKILL_SCHEMA_VERSION {
                return Err(SkillError::Invalid(format!(
                    "unsupported skill state schema {}",
                    state.schema_version
                )));
            }
            validate_state(&state)?;
            Ok(state)
        }
    }
}

fn validate_state(state: &RootState) -> Result<(), SkillError> {
    if state.skills.len() > MAX_DISCOVERED_SKILLS {
        return Err(SkillError::Invalid(
            "skill state has too many entries".to_string(),
        ));
    }
    for (command, entry) in &state.skills {
        if validate_command(command)? != *command || entry.history.len() > MAX_HISTORY {
            return Err(SkillError::Invalid(format!(
                "invalid state record for /{command}"
            )));
        }
        if let Some(digest) = &entry.active_sha256 {
            validate_sha256(digest, "active skill digest")?;
        }
        for record in &entry.history {
            validate_history_record(record)?;
        }
    }
    Ok(())
}

fn verify_managed_active(
    state: &RootState,
    command: &str,
    actual_sha256: &str,
) -> Result<(), SkillError> {
    if state
        .skills
        .get(command)
        .is_some_and(|entry| entry.managed && entry.active_sha256.as_deref() != Some(actual_sha256))
    {
        return Err(SkillError::Conflict(format!(
            "managed skill /{command} changed outside the approved install flow"
        )));
    }
    Ok(())
}

fn save_state(root: &Path, state: &RootState) -> Result<(), SkillError> {
    validate_state(state)?;
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|error| SkillError::Io(format!("serialize skill state: {error}")))?;
    let path = root.join(STATE_FILE);
    let temporary = root.join(format!("{STATE_FILE}.tmp-{}", Uuid::new_v4()));
    let backup = root.join(format!("{STATE_FILE}.bak-{}", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| io_at("create temporary skill state", &temporary, error))?;
    set_private_file_permissions(&temporary)?;
    file.write_all(&bytes)
        .map_err(|error| io_at("write temporary skill state", &temporary, error))?;
    file.sync_all()
        .map_err(|error| io_at("sync temporary skill state", &temporary, error))?;
    let existing_metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(io_at("inspect current skill state", &path, error)),
    };
    let had_existing = existing_metadata.is_some();
    if let Some(metadata) = existing_metadata {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            let _ = fs::remove_file(&temporary);
            return Err(SkillError::Invalid(
                "skill state destination is not a regular file".to_string(),
            ));
        }
        fs::rename(&path, &backup)
            .map_err(|error| io_at("stage previous skill state", &path, error))?;
    }
    if let Err(error) = fs::rename(&temporary, &path) {
        if had_existing {
            let _ = fs::rename(&backup, &path);
        }
        let _ = fs::remove_file(&temporary);
        return Err(io_at("publish skill state", &path, error));
    }
    if let Err(error) = sync_directory(root) {
        let failed = root.join(format!("{STATE_FILE}.failed-{}", Uuid::new_v4()));
        if fs::rename(&path, &failed).is_err() {
            fs::remove_file(&path)
                .map_err(|recovery| io_at("remove uncommitted skill state", &path, recovery))?;
        }
        if had_existing {
            fs::rename(&backup, &path)
                .map_err(|recovery| io_at("restore previous skill state", &path, recovery))?;
        }
        let _ = fs::remove_file(&failed);
        let _ = sync_directory(root);
        return Err(error);
    }
    if had_existing {
        let _ = fs::remove_file(&backup);
        let _ = sync_directory(root);
    }
    Ok(())
}

fn archive_active(
    root: &Path,
    command: &str,
    active: &Path,
    scanned: &ScannedSkill,
) -> Result<HistoryRecord, SkillError> {
    let command_history = history_command_root(root, command, true)?.ok_or_else(|| {
        SkillError::Io("could not create the skill history directory".to_string())
    })?;
    let directory = format!("{}-{}", scanned.sha256, Uuid::new_v4());
    let destination = command_history.join(&directory);
    fs::rename(active, &destination)
        .map_err(|error| io_at("archive active skill", active, error))?;
    Ok(HistoryRecord {
        sha256: scanned.sha256.clone(),
        directory,
        version: scanned.parsed.manifest.version.clone(),
        stored_at_unix_ms: now_unix_ms(),
    })
}

fn history_path(root: &Path, command: &str, record: &HistoryRecord) -> Result<PathBuf, SkillError> {
    validate_history_record(record)?;
    let command_history = history_command_root(root, command, false)?
        .unwrap_or_else(|| root.join(HISTORY_DIR).join(command));
    let path = command_history.join(&record.directory);
    if path.parent() != Some(command_history.as_path()) {
        return Err(SkillError::Invalid(
            "skill history path escapes its root".to_string(),
        ));
    }
    Ok(path)
}

fn history_command_root(
    root: &Path,
    command: &str,
    create: bool,
) -> Result<Option<PathBuf>, SkillError> {
    validate_command(command)?;
    if !plain_directory_exists(root, "skill root")? {
        return Err(SkillError::NotFound("skill root".to_string()));
    }
    let canonical_root =
        fs::canonicalize(root).map_err(|error| io_at("canonicalize skill root", root, error))?;
    let Some(history) = checked_direct_child_directory(&canonical_root, HISTORY_DIR, create)?
    else {
        return Ok(None);
    };
    checked_direct_child_directory(&history, command, create)
}

fn checked_direct_child_directory(
    parent: &Path,
    name: &str,
    create: bool,
) -> Result<Option<PathBuf>, SkillError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\'])
        || Path::new(name).components().count() != 1
    {
        return Err(SkillError::Invalid(
            "invalid managed skill directory component".to_string(),
        ));
    }
    let path = parent.join(name);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound && !create => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Err(create_error) = fs::create_dir(&path) {
                if create_error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(io_at("create managed skill directory", &path, create_error));
                }
            }
        }
        Err(error) => return Err(io_at("inspect managed skill directory", &path, error)),
        Ok(_) => {}
    }
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| io_at("verify managed skill directory", &path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SkillError::Invalid(format!(
            "managed skill path {} must be a real directory",
            path.display()
        )));
    }
    let canonical_parent = fs::canonicalize(parent)
        .map_err(|error| io_at("canonicalize managed skill parent", parent, error))?;
    let canonical = fs::canonicalize(&path)
        .map_err(|error| io_at("canonicalize managed skill directory", &path, error))?;
    if canonical.parent() != Some(canonical_parent.as_path()) {
        return Err(SkillError::Invalid(
            "managed skill directory escapes its parent".to_string(),
        ));
    }
    Ok(Some(canonical))
}

fn validate_history_record(record: &HistoryRecord) -> Result<(), SkillError> {
    validate_sha256(&record.sha256, "history digest")?;
    validate_version(&record.version)?;
    if record.directory.len() < 66
        || record.directory.len() > 128
        || record.directory.contains(['/', '\\'])
        || !record.directory.starts_with(&record.sha256)
    {
        return Err(SkillError::Invalid(
            "invalid skill history directory name".to_string(),
        ));
    }
    Ok(())
}

fn prune_history(
    root: &Path,
    command: &str,
    entry: &mut ManagedSkillState,
) -> Result<Vec<PathBuf>, SkillError> {
    let mut pruned = Vec::new();
    while entry.history.len() > MAX_HISTORY {
        let record = entry.history.remove(0);
        pruned.push(history_path(root, command, &record)?);
    }
    Ok(pruned)
}

fn cleanup_pruned_history(paths: Vec<PathBuf>) {
    // State publication is the transaction boundary. Cleanup happens only
    // afterward, and failures deliberately leave an inert hidden orphan
    // instead of turning an already-committed mutation into a false failure.
    for path in paths {
        let _ = remove_plain_tree(&path);
    }
}

fn mutation_result(
    state: &RootState,
    command: &str,
    scope: SkillScope,
) -> Result<SkillMutationResult, SkillError> {
    let entry = state
        .skills
        .get(command)
        .ok_or_else(|| SkillError::NotFound(format!("state for /{command}")))?;
    Ok(SkillMutationResult {
        command: command.to_string(),
        scope,
        active_sha256: entry.active_sha256.clone(),
        enabled: entry.enabled,
        history_sha256: entry
            .history
            .iter()
            .map(|record| record.sha256.clone())
            .collect(),
    })
}

fn remove_plain_tree(path: &Path) -> Result<(), SkillError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_at("inspect removable skill tree", path, error)),
        Ok(metadata) if metadata.file_type().is_symlink() => Err(SkillError::Invalid(format!(
            "refusing to remove symbolic link {}",
            path.display()
        ))),
        Ok(metadata) if metadata.is_file() => {
            fs::remove_file(path).map_err(|error| io_at("remove skill file", path, error))
        }
        Ok(metadata) if metadata.is_dir() => {
            for entry in fs::read_dir(path)
                .map_err(|error| io_at("read removable skill tree", path, error))?
            {
                let entry =
                    entry.map_err(|error| io_at("read removable skill entry", path, error))?;
                remove_plain_tree(&entry.path())?;
            }
            fs::remove_dir(path).map_err(|error| io_at("remove skill directory", path, error))
        }
        Ok(_) => Err(SkillError::Invalid(format!(
            "refusing to remove special file {}",
            path.display()
        ))),
    }
}

fn validate_sha256(value: &str, label: &str) -> Result<(), SkillError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(SkillError::Invalid(format!(
            "{label} must be 64 hexadecimal characters"
        )));
    }
    Ok(())
}

fn canonical_display(path: &Path) -> Result<String, SkillError> {
    fs::canonicalize(path)
        .map(|path| path.to_string_lossy().to_string())
        .map_err(|error| io_at("canonicalize skill source", path, error))
}

fn source_label(source: &SkillSource) -> String {
    match source {
        SkillSource::Global { path } => format!("global skill {path}"),
        SkillSource::Workspace { path } => format!("workspace skill {path}"),
        SkillSource::SignedPackage { package_id } => format!("signed package {package_id}"),
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn io_at(operation: &str, path: &Path, error: io::Error) -> SkillError {
    SkillError::Io(format!("{operation} {}: {error}", path.display()))
}

fn sync_directory(path: &Path) -> Result<(), SkillError> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|error| io_at("sync directory", path, error))?;
    }
    Ok(())
}

fn set_private_directory_permissions(path: &Path) -> Result<(), SkillError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| io_at("secure directory", path, error))?;
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<(), SkillError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| io_at("secure file", path, error))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "little-monkey-native-skills-{label}-{}",
                Uuid::new_v4()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = remove_plain_tree(&self.0);
        }
    }

    fn write_skill(root: &Path, command: &str, version: &str, extra: &str) -> PathBuf {
        let skill = root.join(format!("source-{command}-{version}"));
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            format!(
                "---\nname: Test {command}\ndescription: Test skill\ncommand: {command}\nversion: {version}\n{extra}---\nUse this skill carefully.\n"
            ),
        )
        .unwrap();
        fs::create_dir_all(skill.join("references")).unwrap();
        fs::write(skill.join("references/info.md"), "reference").unwrap();
        skill
    }

    #[test]
    fn strict_frontmatter_and_requirements_are_reported() {
        let root = TestDirectory::new("parse");
        let skill = write_skill(
            root.path(),
            "review",
            "1.2.3",
            "requires:\n  bins: [definitely-not-a-real-lm-binary]\n  env: [LITTLE_MONKEY_TEST_MISSING_ENV_9A7B]\n",
        );
        let scanned = scan_skill_folder(&skill).unwrap();
        assert_eq!(scanned.parsed.manifest.command, "review");
        let eligibility = evaluate_requirements(&scanned.parsed.manifest);
        assert!(!eligibility.eligible);
        assert_eq!(
            eligibility.missing_bins,
            vec!["definitely-not-a-real-lm-binary"]
        );
        assert_eq!(
            eligibility.missing_env,
            vec!["LITTLE_MONKEY_TEST_MISSING_ENV_9A7B"]
        );

        // Unknown top-level keys are tolerated for ecosystem SKILL.md compat
        // (argument-hint, license, allowed-tools, metadata, ...).
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: X\ndescription: X\ncommand: x\nversion: 1.0.0\nunknown: true\n---\nDo x.\n",
        )
        .unwrap();
        assert_eq!(scan_skill_folder(&skill).unwrap().parsed.manifest.command, "x");

        // ...but unknown keys nested under `requires` still fail closed.
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: X\ndescription: X\ncommand: x\nversion: 1.0.0\nrequires:\n  bogus: [x]\n---\nDo x.\n",
        )
        .unwrap();
        assert!(scan_skill_folder(&skill)
            .unwrap_err()
            .to_string()
            .contains("unknown field"));

        fs::write(
            skill.join("SKILL.md"),
            "---\nname: &shared X\ndescription: *shared\ncommand: x\nversion: 1.0.0\n---\nDo x.\n",
        )
        .unwrap();
        assert!(scan_skill_folder(&skill).is_err());

        fs::write(
            skill.join("SKILL.md"),
            "---\nname: X\ndescription: X\ncommand: x\ncommand: y\nversion: 1.0.0\n---\nDo x.\n",
        )
        .unwrap();
        assert!(scan_skill_folder(&skill).is_err());
    }

    #[test]
    fn approval_rechecks_every_source_byte() {
        let root = TestDirectory::new("approval");
        let app_data = root.path().join("app-data");
        let source = write_skill(root.path(), "summarize", "1.0.0", "");
        let manager = NativeSkillManager::new(&app_data).unwrap();
        let preview = manager
            .preview_local(&source, SkillScope::Global, None)
            .unwrap();
        fs::write(source.join("references/info.md"), "changed").unwrap();
        let error = manager
            .install_local(
                &source,
                SkillScope::Global,
                None,
                &preview.approval_digest,
                true,
            )
            .unwrap_err();
        assert!(matches!(error, SkillError::Approval(_)));
    }

    #[test]
    fn managed_skill_tampering_cannot_be_enabled_archived_or_rolled_into_history() {
        let root = TestDirectory::new("tamper");
        let app_data = root.path().join("app-data");
        let source = write_skill(root.path(), "summarize", "1.0.0", "");
        let replacement = write_skill(root.path(), "summarize", "2.0.0", "");
        let manager = NativeSkillManager::new(&app_data).unwrap();
        let preview = manager
            .preview_local(&source, SkillScope::Global, None)
            .unwrap();
        manager
            .install_local(
                &source,
                SkillScope::Global,
                None,
                &preview.approval_digest,
                true,
            )
            .unwrap();
        fs::write(
            manager.global_root().join("summarize/references/info.md"),
            "tampered",
        )
        .unwrap();
        assert!(matches!(
            manager.set_enabled(SkillScope::Global, None, "summarize", false),
            Err(SkillError::Conflict(_))
        ));
        assert!(matches!(
            manager.uninstall(SkillScope::Global, None, "summarize"),
            Err(SkillError::Conflict(_))
        ));
        let replacement_preview = manager
            .preview_local(&replacement, SkillScope::Global, None)
            .unwrap();
        assert!(matches!(
            manager.install_local(
                &replacement,
                SkillScope::Global,
                None,
                &replacement_preview.approval_digest,
                true,
            ),
            Err(SkillError::Conflict(_))
        ));
    }

    #[test]
    fn install_disable_update_rollback_and_uninstall_are_durable() {
        let root = TestDirectory::new("lifecycle");
        let app_data = root.path().join("app-data");
        let v1 = write_skill(root.path(), "summarize", "1.0.0", "");
        let v2 = write_skill(root.path(), "summarize", "2.0.0", "");
        fs::write(v2.join("references/info.md"), "version two").unwrap();
        let manager = NativeSkillManager::new(&app_data).unwrap();

        let preview = manager
            .preview_local(&v1, SkillScope::Global, None)
            .unwrap();
        manager
            .install_local(
                &v1,
                SkillScope::Global,
                None,
                &preview.approval_digest,
                true,
            )
            .unwrap();
        assert_eq!(manager.discover(None, &[]).unwrap()[0].version, "1.0.0");
        manager
            .set_enabled(SkillScope::Global, None, "summarize", false)
            .unwrap();
        assert!(!manager.discover(None, &[]).unwrap()[0].enabled);

        let preview = manager
            .preview_local(&v2, SkillScope::Global, None)
            .unwrap();
        manager
            .install_local(
                &v2,
                SkillScope::Global,
                None,
                &preview.approval_digest,
                true,
            )
            .unwrap();
        assert_eq!(manager.discover(None, &[]).unwrap()[0].version, "2.0.0");
        manager
            .rollback(SkillScope::Global, None, "summarize")
            .unwrap();
        assert_eq!(manager.discover(None, &[]).unwrap()[0].version, "1.0.0");
        let result = manager
            .uninstall(SkillScope::Global, None, "summarize")
            .unwrap();
        assert!(result.active_sha256.is_none());
        assert!(manager.discover(None, &[]).unwrap().is_empty());
        manager
            .rollback(SkillScope::Global, None, "summarize")
            .unwrap();
        assert_eq!(manager.discover(None, &[]).unwrap()[0].version, "1.0.0");
    }

    #[test]
    fn workspace_and_signed_package_collisions_fail_closed() {
        let root = TestDirectory::new("collision");
        let app_data = root.path().join("app-data");
        let workspace = root.path().join("workspace");
        fs::create_dir_all(workspace.join(".littlemonkey/skills/review")).unwrap();
        let authored = write_skill(root.path(), "review", "1.0.0", "");
        fs::copy(
            authored.join("SKILL.md"),
            workspace.join(".littlemonkey/skills/review/SKILL.md"),
        )
        .unwrap();
        let manager = NativeSkillManager::new(&app_data).unwrap();
        assert_eq!(manager.discover(Some(&workspace), &[]).unwrap().len(), 1);
        let package = ExternalSignedSkill {
            package_id: "com.example.review".to_string(),
            name: "Review".to_string(),
            description: "Review things".to_string(),
            command: "review".to_string(),
            version: "1.0.0".to_string(),
            instructions: "Review safely.".to_string(),
            sha256: "a".repeat(64),
            permissions: BTreeSet::new(),
        };
        manager
            .set_enabled(SkillScope::Workspace, Some(&workspace), "review", false)
            .unwrap();
        let merged = manager
            .discover(Some(&workspace), std::slice::from_ref(&package))
            .unwrap();
        assert_eq!(merged.len(), 2);
        assert_eq!(merged.iter().filter(|skill| skill.enabled).count(), 1);
        manager
            .set_enabled(SkillScope::Workspace, Some(&workspace), "review", true)
            .unwrap();
        assert!(matches!(
            manager.discover(Some(&workspace), &[package]),
            Err(SkillError::Conflict(_))
        ));
    }

    #[test]
    fn git_sources_require_https_and_safe_commit_specs() {
        for request in [
            GitSkillRequest {
                repository_url: "http://example.com/skill.git".to_string(),
                commit: "a".repeat(40),
                subdirectory: None,
            },
            GitSkillRequest {
                repository_url: "https://user:secret@example.com/skill.git".to_string(),
                commit: "a".repeat(40),
                subdirectory: None,
            },
        ] {
            assert!(validate_git_request(&request).is_err());
        }
        // Option-injection and traversal shapes are rejected as references.
        for commit in ["-evil", "a..b", "a//b", "/main", ".hidden", "main/", "ref with space"] {
            assert!(
                validate_git_request(&GitSkillRequest {
                    repository_url: "https://example.com/skill.git".to_string(),
                    commit: commit.to_string(),
                    subdirectory: None,
                })
                .is_err(),
                "commit spec '{commit}' should be rejected"
            );
        }
        // Pinned SHA, branch/tag names, and empty (default branch) all pass.
        for commit in [
            "0123456789abcdef0123456789abcdef01234567",
            "main",
            "v1.2.0",
            "release/2024",
            "",
        ] {
            assert!(
                validate_git_request(&GitSkillRequest {
                    repository_url: "https://example.com/skill.git".to_string(),
                    commit: commit.to_string(),
                    subdirectory: Some("skills/review".to_string()),
                })
                .is_ok(),
                "commit spec '{commit}' should be accepted"
            );
        }
    }

    #[test]
    fn ls_remote_selection_prefers_branches_then_peeled_tags() {
        let listing = "1111111111111111111111111111111111111111\tHEAD\n\
                       2222222222222222222222222222222222222222\trefs/heads/main\n\
                       3333333333333333333333333333333333333333\trefs/tags/main\n\
                       4444444444444444444444444444444444444444\trefs/tags/v1\n\
                       5555555555555555555555555555555555555555\trefs/tags/v1^{}\n";
        assert_eq!(
            select_ls_remote_commit(listing, &GitCommitSpec::DefaultHead).unwrap(),
            "1111111111111111111111111111111111111111"
        );
        assert_eq!(
            select_ls_remote_commit(listing, &GitCommitSpec::Reference("main".to_string()))
                .unwrap(),
            "2222222222222222222222222222222222222222"
        );
        // Annotated tag: the peeled commit wins over the tag object.
        assert_eq!(
            select_ls_remote_commit(listing, &GitCommitSpec::Reference("v1".to_string())).unwrap(),
            "5555555555555555555555555555555555555555"
        );
        assert!(select_ls_remote_commit(
            listing,
            &GitCommitSpec::Reference("missing".to_string())
        )
        .is_err());
        assert!(select_ls_remote_commit("", &GitCommitSpec::DefaultHead).is_err());
    }

    #[test]
    fn ecosystem_frontmatter_without_command_or_version_parses() {
        let root = TestDirectory::new("ecosystem");
        let skill = root.path().join("ponytail");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: ponytail\ndescription: >\n  Forces the laziest solution that works.\nargument-hint: \"[lite|full|ultra]\"\nlicense: MIT\n---\nYou are a lazy senior developer.\n",
        )
        .unwrap();
        let scanned = scan_skill_folder(&skill).unwrap();
        assert_eq!(scanned.parsed.manifest.command, "ponytail");
        assert_eq!(scanned.parsed.manifest.version, "0.0.0");

        // A display-style name slugifies into the derived command.
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: Code Review\ndescription: Reviews code.\n---\nReview the diff.\n",
        )
        .unwrap();
        assert_eq!(
            scan_skill_folder(&skill).unwrap().parsed.manifest.command,
            "code-review"
        );
    }

    #[test]
    fn folder_discovery_is_shallowest_first_and_skips_dot_dirs() {
        let root = TestDirectory::new("candidates");
        for path in [
            "skills/ponytail",
            "skills/ponytail-review",
            "plugins/nested/skills/ponytail",
            ".openclaw/skills/ponytail",
        ] {
            let dir = root.path().join(path);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("SKILL.md"),
                "---\nname: x\ndescription: Test.\n---\nBody.\n",
            )
            .unwrap();
        }
        let folders = discover_skill_folders(root.path());
        assert_eq!(
            folders
                .iter()
                .map(|folder| folder.to_string_lossy().replace('\\', "/"))
                .collect::<Vec<_>>(),
            // Breadth-first: the two-level skills/* copies come before the
            // deeper plugins/nested/skills/* duplicate; dot-dirs never appear.
            vec![
                "skills/ponytail",
                "skills/ponytail-review",
                "plugins/nested/skills/ponytail"
            ]
        );
    }

    #[test]
    fn built_in_slash_commands_are_reserved() {
        for command in RESERVED_SLASH_COMMANDS {
            assert!(matches!(
                validate_command(command),
                Err(SkillError::Conflict(_))
            ));
        }
        assert_eq!(
            validate_command("project-review").unwrap(),
            "project-review"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pinned_git_preview_and_install_verify_head_end_to_end() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDirectory::new("git-install");
        let fake_git = root.path().join("fixed-git-fixture");
        fs::write(
            &fake_git,
            r#"#!/bin/sh
set -eu
cwd=""
if [ "${1:-}" = "-C" ]; then
  cwd="$2"
  shift 2
fi
while [ "${1:-}" = "-c" ]; do
  shift 2
done
verb="${1:-}"
shift || true
case "$verb" in
  init|remote) exit 0 ;;
  fetch)
    last=""
    for item in "$@"; do last="$item"; done
    printf '%s' "$last" > "$cwd/.fake-commit"
    ;;
  checkout)
    printf '%s\n' '---' 'name: Git Review' 'description: Review from a pinned repository' 'command: git-review' 'version: 1.0.0' '---' 'Review the supplied project.' > "$cwd/SKILL.md"
    ;;
  rev-parse) cat "$cwd/.fake-commit" ;;
  *) exit 2 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o700)).unwrap();
        let manager = NativeSkillManager::new(root.path().join("app-data"))
            .unwrap()
            .with_git_binary(&fake_git);
        let request = GitSkillRequest {
            repository_url: "https://example.com/team/skills.git".to_string(),
            commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
            subdirectory: None,
        };
        let outcome = manager
            .preview_git(&request, SkillScope::Global, None)
            .unwrap();
        let GitSkillPreviewOutcome::Preview {
            pinned_commit,
            preview,
        } = outcome
        else {
            panic!("expected a direct preview, got {outcome:?}");
        };
        assert_eq!(pinned_commit, request.commit);
        assert_eq!(preview.command, "git-review");
        let installed = manager
            .install_git(
                &request,
                SkillScope::Global,
                None,
                &preview.approval_digest,
                true,
            )
            .unwrap();
        assert_eq!(
            installed.active_sha256.as_deref(),
            Some(preview.sha256.as_str())
        );
        assert_eq!(
            manager.discover(None, &[]).unwrap()[0].command,
            "git-review"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unpinned_git_preview_resolves_head_and_offers_candidates() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDirectory::new("git-resolve");
        let fake_git = root.path().join("resolving-git-fixture");
        fs::write(
            &fake_git,
            r#"#!/bin/sh
set -eu
cwd=""
if [ "${1:-}" = "-C" ]; then
  cwd="$2"
  shift 2
fi
while [ "${1:-}" = "-c" ]; do
  shift 2
done
verb="${1:-}"
shift || true
case "$verb" in
  init|remote) exit 0 ;;
  ls-remote)
    printf 'fedcba9876543210fedcba9876543210fedcba98\tHEAD\n'
    printf 'fedcba9876543210fedcba9876543210fedcba98\trefs/heads/main\n'
    ;;
  fetch)
    last=""
    for item in "$@"; do last="$item"; done
    printf '%s' "$last" > "$cwd/.fake-commit"
    ;;
  checkout)
    mkdir -p "$cwd/skills/alpha" "$cwd/skills/beta" "$cwd/plugins/pack/skills/alpha"
    printf '%s\n' '---' 'name: alpha' 'description: First skill.' '---' 'Do alpha.' > "$cwd/skills/alpha/SKILL.md"
    printf '%s\n' '---' 'name: beta' 'description: Second skill.' '---' 'Do beta.' > "$cwd/skills/beta/SKILL.md"
    printf '%s\n' '---' 'name: alpha' 'description: Duplicate copy.' '---' 'Do alpha.' > "$cwd/plugins/pack/skills/alpha/SKILL.md"
    ;;
  rev-parse) cat "$cwd/.fake-commit" ;;
  *) exit 2 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o700)).unwrap();
        let manager = NativeSkillManager::new(root.path().join("app-data"))
            .unwrap()
            .with_git_binary(&fake_git);

        // Empty commit + no subdirectory: resolves HEAD, offers candidates.
        // The deeper plugins/pack/skills/alpha duplicate is deduped away.
        let request = GitSkillRequest {
            repository_url: "https://example.com/team/skills.git".to_string(),
            commit: String::new(),
            subdirectory: None,
        };
        let outcome = manager
            .preview_git(&request, SkillScope::Global, None)
            .unwrap();
        let GitSkillPreviewOutcome::Candidates {
            pinned_commit,
            candidates,
        } = outcome
        else {
            panic!("expected candidates, got {outcome:?}");
        };
        assert_eq!(pinned_commit, "fedcba9876543210fedcba9876543210fedcba98");
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| (
                    candidate.subdirectory.as_str(),
                    candidate.preview.command.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![("skills/alpha", "alpha"), ("skills/beta", "beta")]
        );

        // Installing every candidate in one bulk call uses the per-skill
        // digests from the candidate previews.
        let pinned_request = GitSkillRequest {
            repository_url: request.repository_url.clone(),
            commit: pinned_commit,
            subdirectory: None,
        };
        let approvals = candidates
            .iter()
            .map(|candidate| GitBulkApproval {
                subdirectory: candidate.subdirectory.clone(),
                approval_digest: candidate.preview.approval_digest.clone(),
            })
            .collect::<Vec<_>>();
        let results = manager
            .install_git_bulk(&pinned_request, SkillScope::Global, None, &approvals, true)
            .unwrap();
        assert_eq!(
            results
                .iter()
                .map(|result| result.command.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
        assert_eq!(manager.discover(None, &[]).unwrap().len(), 2);

        // A wrong digest fails the whole batch.
        let mut stale = approvals.clone();
        stale[0].approval_digest = "0".repeat(64);
        assert!(matches!(
            manager.install_git_bulk(&pinned_request, SkillScope::Global, None, &stale, true),
            Err(SkillError::Approval(_))
        ));

        // Single-candidate install (per-row button) still works.
        let single_request = GitSkillRequest {
            repository_url: request.repository_url.clone(),
            commit: pinned_request.commit.clone(),
            subdirectory: Some("skills/alpha".to_string()),
        };
        let installed = manager
            .install_git(
                &single_request,
                SkillScope::Global,
                None,
                &candidates[0].preview.approval_digest,
                true,
            )
            .unwrap();
        assert_eq!(installed.command, "alpha");

        // A branch name that the remote does not advertise fails closed.
        let missing = GitSkillRequest {
            repository_url: request.repository_url,
            commit: "does-not-exist".to_string(),
            subdirectory: None,
        };
        assert!(manager
            .preview_git(&missing, SkillScope::Global, None)
            .unwrap_err()
            .to_string()
            .contains("no branch or tag"));
    }

    #[cfg(unix)]
    #[test]
    fn git_installed_skills_are_tagged_with_their_repository_for_ui_grouping() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDirectory::new("git-grouping");
        let fake_git = root.path().join("grouping-git-fixture");
        fs::write(
            &fake_git,
            r#"#!/bin/sh
set -eu
cwd=""
if [ "${1:-}" = "-C" ]; then
  cwd="$2"
  shift 2
fi
while [ "${1:-}" = "-c" ]; do
  shift 2
done
verb="${1:-}"
shift || true
case "$verb" in
  init|remote) exit 0 ;;
  fetch)
    last=""
    for item in "$@"; do last="$item"; done
    printf '%s' "$last" > "$cwd/.fake-commit"
    ;;
  checkout)
    mkdir -p "$cwd/skills/alpha" "$cwd/skills/beta"
    printf '%s\n' '---' 'name: alpha' 'description: First skill.' '---' 'Do alpha.' > "$cwd/skills/alpha/SKILL.md"
    printf '%s\n' '---' 'name: beta' 'description: Second skill.' '---' 'Do beta.' > "$cwd/skills/beta/SKILL.md"
    ;;
  rev-parse) cat "$cwd/.fake-commit" ;;
  *) exit 2 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o700)).unwrap();
        let manager = NativeSkillManager::new(root.path().join("app-data"))
            .unwrap()
            .with_git_binary(&fake_git);

        let request = GitSkillRequest {
            repository_url: "https://example.com/team/skills.git".to_string(),
            commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
            subdirectory: None,
        };
        let outcome = manager
            .preview_git(&request, SkillScope::Global, None)
            .unwrap();
        let GitSkillPreviewOutcome::Candidates { candidates, .. } = outcome else {
            panic!("expected candidates");
        };
        let approvals = candidates
            .iter()
            .map(|candidate| GitBulkApproval {
                subdirectory: candidate.subdirectory.clone(),
                approval_digest: candidate.preview.approval_digest.clone(),
            })
            .collect::<Vec<_>>();
        manager
            .install_git_bulk(&request, SkillScope::Global, None, &approvals, true)
            .unwrap();

        let discovered = manager.discover(None, &[]).unwrap();
        assert_eq!(discovered.len(), 2);
        for skill in &discovered {
            assert_eq!(
                skill.git_repository.as_deref(),
                Some("https://example.com/team/skills.git")
            );
        }

        // A locally-installed skill (no repository) carries no group tag.
        let local_source = write_skill(root.path(), "solo", "1.0.0", "");
        let local_preview = manager
            .preview_local(&local_source, SkillScope::Global, None)
            .unwrap();
        manager
            .install_local(
                &local_source,
                SkillScope::Global,
                None,
                &local_preview.approval_digest,
                true,
            )
            .unwrap();
        let solo = manager
            .discover(None, &[])
            .unwrap()
            .into_iter()
            .find(|skill| skill.command == "solo")
            .unwrap();
        assert_eq!(solo.git_repository, None);

        // Group operations act on every command in the batch under one
        // lock; disabling both, then rolling back one and uninstalling the
        // other, all through the *_many entry points the UI uses for a
        // repo-grouped card.
        let commands = discovered
            .iter()
            .map(|skill| skill.command.clone())
            .collect::<Vec<_>>();
        let disabled = manager
            .set_enabled_many(SkillScope::Global, None, &commands, false)
            .unwrap();
        assert!(disabled.iter().all(|result| !result.enabled));

        let reenabled = manager
            .set_enabled_many(SkillScope::Global, None, &commands, true)
            .unwrap();
        assert!(reenabled.iter().all(|result| result.enabled));

        let uninstalled = manager
            .uninstall_many(SkillScope::Global, None, &commands)
            .unwrap();
        assert_eq!(uninstalled.len(), 2);
        assert!(manager
            .discover(None, &[])
            .unwrap()
            .iter()
            .all(|skill| skill.command == "solo"));

        let rolled_back = manager
            .rollback_many(SkillScope::Global, None, &commands)
            .unwrap();
        assert_eq!(rolled_back.len(), 2);
        let after_rollback = manager.discover(None, &[]).unwrap();
        assert_eq!(
            after_rollback
                .iter()
                .filter(|skill| skill.command != "solo")
                .count(),
            2
        );

        // A group op naming an unknown command fails without touching the
        // commands listed before it.
        let mixed = vec![commands[0].clone(), "does-not-exist".to_string()];
        assert!(manager
            .set_enabled_many(SkillScope::Global, None, &mixed, false)
            .is_err());
        let after_partial = manager.discover(None, &[]).unwrap();
        assert!(!after_partial
            .iter()
            .find(|skill| skill.command == commands[0])
            .unwrap()
            .enabled);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_rejected_in_skill_trees_and_workspace_roots() {
        use std::os::unix::fs::symlink;

        let root = TestDirectory::new("symlink");
        let source = write_skill(root.path(), "review", "1.0.0", "");
        symlink("/etc/hosts", source.join("references/escape")).unwrap();
        assert!(scan_skill_folder(&source)
            .unwrap_err()
            .to_string()
            .contains("symbolic links"));
        fs::remove_file(source.join("references/escape")).unwrap();

        let workspace = root.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        symlink(root.path(), workspace.join(".littlemonkey")).unwrap();
        assert!(workspace_skill_root(&workspace, false).is_err());
        fs::remove_file(workspace.join(".littlemonkey")).unwrap();

        let app_data = root.path().join("app-data");
        let manager = NativeSkillManager::new(&app_data).unwrap();
        symlink(root.path(), manager.global_root().join(HISTORY_DIR)).unwrap();
        assert!(history_command_root(manager.global_root(), "review", true).is_err());
        fs::remove_file(manager.global_root().join(HISTORY_DIR)).unwrap();
    }
}
