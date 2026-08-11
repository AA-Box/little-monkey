//! Local multi-profile identity (K23).
//!
//! One machine, one user, several *identities*: a work profile and a personal
//! one, each with its own sessions, run history, artifacts, packages,
//! credentials, quota and share of the machine. This is local isolation only —
//! there is no account service, no RBAC, and no login. A profile is a directory
//! plus a row in a registry, and the person switching between them already owns
//! both.
//!
//! # Isolation is structural, not a filter
//!
//! Every managed store this app has — the run ledger, artifact store, sessions,
//! prompts, stacks, daemon queue, and package set — is under the app-data
//! directory. Portable authored configuration uses the same profile id under
//! the agent home. A profile therefore owns both roots, and isolation remains
//! structural rather than depending on a forgotten `WHERE profile_id = ?`.
//!
//! Two consequences worth stating:
//!
//! - **The default profile keeps the legacy layout.** `default` resolves to the
//!   app data directory itself, so an existing installation is the default
//!   profile already and no data is moved on upgrade. Additional profiles live
//!   under both `<app data>/profiles/<id>` and `<agent home>/profiles/<id>`.
//! - **Credentials are namespaced by keychain *service*.** The OS keychain is
//!   not a directory, so path scoping cannot reach it. [`keychain_service`]
//!   suffixes the service name with the profile id, which makes a second
//!   profile's entries a different keychain item — unreadable by the first
//!   without impersonating its service name. `default` is unsuffixed, so
//!   existing credentials keep working.
//!
//! # Switching restarts the app, on purpose
//!
//! The active profile is read from the registry on every path resolution, and
//! the process caches nothing about it. What *is* cached is everything built
//! from a path: an open ledger connection, the artifact store handle, a running
//! daemon, an MCP server's environment. Swapping the active id under those
//! live handles is precisely how a cross-profile leak gets written, so
//! [`switch_profile`] only records the choice and the desktop layer restarts —
//! a new process opens the new profile's files and nothing survives that was
//! built from the old one's.
//!
//! ponytail: the registry is re-read per resolution rather than cached in a
//! `OnceLock`. It is a sub-kilobyte JSON file next to files being opened
//! anyway, and a cache would have to be keyed by base directory because tests
//! run many mock apps, each with its own, in one process.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The profile every installation already has: the legacy, unscoped layout.
pub const DEFAULT_PROFILE_ID: &str = "default";

/// Overrides the registry's active profile for one process, so
/// `LITTLE_MONKEY_PROFILE=work monkey daemon start` runs against that profile
/// without switching the desktop app's.
pub const PROFILE_ENV_VAR: &str = "LITTLE_MONKEY_PROFILE";

/// Registry file name, held in the *base* app data directory rather than in a
/// profile root: it is the thing that decides which root to use.
pub const REGISTRY_FILE: &str = "profiles.json";

/// Directory holding every non-default profile's data root.
pub const PROFILES_DIR: &str = "profiles";

/// Registry schema version, so a future layout change can be detected rather
/// than silently misread.
pub const REGISTRY_VERSION: u32 = 1;

/// A bound, not a policy: the registry is read on every path resolution, and an
/// unbounded list would make that read unbounded too.
pub const MAX_PROFILES: usize = 32;

const MAX_ID_BYTES: usize = 32;
const MAX_NAME_BYTES: usize = 64;
const MAX_REGISTRY_BYTES: u64 = 256 * 1024;
static DELETE_STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Weight bounds. Zero would mean "never gets any of the machine", which is a
/// disabled profile rather than a share, and an unbounded weight makes every
/// other profile's share round to nothing.
pub const MIN_FAIR_SHARE_WEIGHT: f64 = 0.05;
pub const MAX_FAIR_SHARE_WEIGHT: f64 = 20.0;

#[derive(Debug)]
pub enum ProfileError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Json(serde_json::Error),
    Invalid(String),
    UnknownProfile(String),
    DuplicateProfile(String),
    TooManyProfiles,
    UnsupportedVersion(u32),
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "failed to {operation} {}: {source}", path.display()),
            Self::Json(error) => write!(f, "invalid profile registry JSON: {error}"),
            Self::Invalid(message) => write!(f, "{message}"),
            Self::UnknownProfile(id) => write!(f, "no profile with id '{id}'"),
            Self::DuplicateProfile(id) => write!(f, "a profile with id '{id}' already exists"),
            Self::TooManyProfiles => {
                write!(f, "at most {MAX_PROFILES} local profiles are supported")
            }
            Self::UnsupportedVersion(version) => write!(
                f,
                "profile registry version {version} is newer than this app understands ({REGISTRY_VERSION})"
            ),
        }
    }
}

impl std::error::Error for ProfileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ProfileError> for io::Error {
    fn from(value: ProfileError) -> Self {
        io::Error::other(value.to_string())
    }
}

pub type ProfileResult<T> = Result<T, ProfileError>;

/// Absolute ceilings this profile's work may not cross, enforced by the daemon
/// (K4). `None` means "no profile-level ceiling", which is the default and
/// leaves the existing per-kind limits in charge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileQuota {
    /// Hard cap on concurrently dispatched jobs, applied on top of the daemon's
    /// configured concurrency (the lower of the two wins).
    pub max_concurrent_runs: Option<u32>,
    /// Ceiling on the system memory this profile's admitted jobs may hold at
    /// once. A single job that cannot fit under it is rejected rather than held
    /// forever.
    pub max_memory_bytes: Option<u64>,
    /// Ceiling on a single job's wall clock, applied on top of the job's own
    /// `max_runtime_ms` (the lower of the two wins).
    pub max_runtime_ms: Option<u64>,
}

impl ProfileQuota {
    /// The lower of two optional ceilings, where `None` is "unbounded".
    fn tighter<T: Ord + Copy>(left: Option<T>, right: Option<T>) -> Option<T> {
        match (left, right) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (value, None) | (None, value) => value,
        }
    }

    /// Applies this profile's concurrency ceiling to a configured value.
    #[must_use]
    pub fn clamp_concurrency(&self, configured: u32) -> u32 {
        Self::tighter(Some(configured), self.max_concurrent_runs)
            .unwrap_or(configured)
            .max(1)
    }

    /// Applies this profile's wall-clock ceiling to a job's own budget.
    #[must_use]
    pub fn clamp_runtime_ms(&self, configured: Option<u64>) -> Option<u64> {
        Self::tighter(configured, self.max_runtime_ms)
    }

    fn validate(&self) -> ProfileResult<()> {
        if self.max_concurrent_runs == Some(0) {
            return Err(ProfileError::Invalid(
                "maxConcurrentRuns must be at least 1".to_string(),
            ));
        }
        if self.max_memory_bytes == Some(0) {
            return Err(ProfileError::Invalid(
                "maxMemoryBytes must be positive".to_string(),
            ));
        }
        if self.max_runtime_ms == Some(0) {
            return Err(ProfileError::Invalid(
                "maxRuntimeMs must be positive".to_string(),
            ));
        }
        Ok(())
    }
}

const fn unit_weight() -> f64 {
    1.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub created_at_ms: u64,
    /// This profile's share of the machine relative to the other profiles
    /// (K8). Two profiles at 1.0 and 3.0 split contended memory 25/75.
    #[serde(default = "unit_weight")]
    pub fair_share_weight: f64,
    #[serde(default)]
    pub quota: ProfileQuota,
}

impl Profile {
    fn new(id: String, name: String, created_at_ms: u64) -> Self {
        Self {
            id,
            name,
            created_at_ms,
            fair_share_weight: unit_weight(),
            quota: ProfileQuota::default(),
        }
    }

    fn validate(&self) -> ProfileResult<()> {
        validate_id(&self.id)?;
        validate_name(&self.name)?;
        validate_weight(self.fair_share_weight)?;
        self.quota.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileRegistry {
    pub version: u32,
    pub active_id: String,
    pub profiles: Vec<Profile>,
}

impl Default for ProfileRegistry {
    /// A registry that has never been written: one default profile, active.
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            active_id: DEFAULT_PROFILE_ID.to_string(),
            profiles: vec![Profile::new(
                DEFAULT_PROFILE_ID.to_string(),
                "Default".to_string(),
                0,
            )],
        }
    }
}

impl ProfileRegistry {
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.id == id)
    }

    /// The active profile, or the default one if the recorded active id no
    /// longer exists — a deleted profile must not leave the app with no
    /// resolvable data root.
    #[must_use]
    pub fn active(&self) -> Profile {
        self.get(&self.active_id)
            .or_else(|| self.get(DEFAULT_PROFILE_ID))
            .cloned()
            .unwrap_or_else(|| {
                Profile::new(DEFAULT_PROFILE_ID.to_string(), "Default".to_string(), 0)
            })
    }

    /// Sum of every profile's weight, used to turn one weight into a fraction
    /// of the machine. Never zero: weights are bounded below.
    #[must_use]
    pub fn total_weight(&self) -> f64 {
        let total: f64 = self
            .profiles
            .iter()
            .map(|profile| profile.fair_share_weight.max(MIN_FAIR_SHARE_WEIGHT))
            .sum();
        if total.is_finite() && total > 0.0 {
            total
        } else {
            unit_weight()
        }
    }

    /// The fraction of a contended machine resource `id` may claim.
    ///
    /// A single-profile installation gets `1.0` — the whole machine — so this
    /// changes nothing until a second profile exists, which is the only honest
    /// default: the share is *relative*, and there is nothing to be relative to
    /// on a machine with one identity.
    #[must_use]
    pub fn share_of(&self, id: &str) -> f64 {
        let Some(profile) = self.get(id) else {
            return 1.0;
        };
        if self.profiles.len() < 2 {
            return 1.0;
        }
        (profile.fair_share_weight.max(MIN_FAIR_SHARE_WEIGHT) / self.total_weight()).clamp(0.0, 1.0)
    }

    fn validate(&self) -> ProfileResult<()> {
        if self.version > REGISTRY_VERSION {
            return Err(ProfileError::UnsupportedVersion(self.version));
        }
        if self.profiles.len() > MAX_PROFILES {
            return Err(ProfileError::TooManyProfiles);
        }
        for profile in &self.profiles {
            profile.validate()?;
        }
        for (index, profile) in self.profiles.iter().enumerate() {
            if self.profiles[..index]
                .iter()
                .any(|other| other.id == profile.id)
            {
                return Err(ProfileError::DuplicateProfile(profile.id.clone()));
            }
        }
        if self.get(DEFAULT_PROFILE_ID).is_none() {
            return Err(ProfileError::Invalid(
                "the default profile may not be removed from the registry".to_string(),
            ));
        }
        if self.get(&self.active_id).is_none() {
            return Err(ProfileError::UnknownProfile(self.active_id.clone()));
        }
        Ok(())
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// Profile ids appear in filesystem paths and in keychain service names, so the
/// character set is deliberately narrower than "a valid file name": lowercase
/// ASCII, digits and dashes only. That rules out `..`, path separators, drive
/// letters, leading dots, and every Windows reserved character in one rule
/// rather than in a list of special cases.
pub fn validate_id(id: &str) -> ProfileResult<()> {
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        return Err(ProfileError::Invalid(format!(
            "profile id must be 1..={MAX_ID_BYTES} characters"
        )));
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(ProfileError::Invalid(
            "profile id may contain only lowercase letters, digits and dashes".to_string(),
        ));
    }
    if id.starts_with('-') || id.ends_with('-') {
        return Err(ProfileError::Invalid(
            "profile id may not start or end with a dash".to_string(),
        ));
    }
    Ok(())
}

fn validate_name(name: &str) -> ProfileResult<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() || name.len() > MAX_NAME_BYTES {
        return Err(ProfileError::Invalid(format!(
            "profile name must be 1..={MAX_NAME_BYTES} bytes of non-blank text"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(ProfileError::Invalid(
            "profile name may not contain control characters".to_string(),
        ));
    }
    Ok(())
}

fn validate_weight(weight: f64) -> ProfileResult<()> {
    if !weight.is_finite() || !(MIN_FAIR_SHARE_WEIGHT..=MAX_FAIR_SHARE_WEIGHT).contains(&weight) {
        return Err(ProfileError::Invalid(format!(
            "fair-share weight must be between {MIN_FAIR_SHARE_WEIGHT} and {MAX_FAIR_SHARE_WEIGHT}"
        )));
    }
    Ok(())
}

/// Turns a display name into a candidate id. Returns `None` when nothing usable
/// survives, which is how "🙂" is rejected rather than becoming an empty id.
#[must_use]
pub fn slugify(name: &str) -> Option<String> {
    let mut slug = String::new();
    let mut last_dash = true;
    for character in name.chars() {
        let lowered = character.to_ascii_lowercase();
        if lowered.is_ascii_lowercase() || lowered.is_ascii_digit() {
            slug.push(lowered);
            last_dash = false;
        } else if !last_dash && slug.len() < MAX_ID_BYTES {
            slug.push('-');
            last_dash = true;
        }
        if slug.len() >= MAX_ID_BYTES {
            break;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        None
    } else {
        Some(slug)
    }
}

#[must_use]
pub fn registry_path(base: &Path) -> PathBuf {
    base.join(REGISTRY_FILE)
}

/// Where a profile's data lives.
///
/// `default` is the app data directory itself so that an installation that
/// predates profiles *is* the default profile, with nothing moved. Everything
/// else is a directory under it.
#[must_use]
pub fn profile_root(base: &Path, id: &str) -> PathBuf {
    if id == DEFAULT_PROFILE_ID {
        base.to_path_buf()
    } else {
        base.join(PROFILES_DIR).join(id)
    }
}

/// Reads the registry, failing on anything it cannot understand.
///
/// A missing file is not a failure — it is an installation that has never
/// created a second profile — but a corrupt or newer-versioned one is, because
/// the alternative is silently writing this session's work into the wrong
/// profile's directory.
pub fn load_registry(base: &Path) -> ProfileResult<ProfileRegistry> {
    let path = registry_path(base);
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ProfileRegistry::default())
        }
        Err(source) => {
            return Err(ProfileError::Io {
                operation: "inspect",
                path,
                source,
            })
        }
    };
    if metadata.len() > MAX_REGISTRY_BYTES {
        return Err(ProfileError::Invalid(format!(
            "{} is {} bytes, exceeding the {MAX_REGISTRY_BYTES} byte limit",
            path.display(),
            metadata.len()
        )));
    }
    let raw = fs::read_to_string(&path).map_err(|source| ProfileError::Io {
        operation: "read",
        path: path.clone(),
        source,
    })?;
    let registry: ProfileRegistry = serde_json::from_str(&raw).map_err(ProfileError::Json)?;
    registry.validate()?;
    Ok(registry)
}

/// Writes the registry atomically, so a crash mid-write cannot leave a file
/// that resolves to no profile at all.
///
/// ponytail: read-modify-write with no cross-process lock. Two processes
/// creating a profile in the same instant can lose one of the two entries —
/// never corrupt the file, because the rename is atomic and the write is
/// validated first. The operations here are all human-driven and rare; a lock
/// file is the upgrade if that ever stops being true.
pub fn save_registry(base: &Path, registry: &ProfileRegistry) -> ProfileResult<()> {
    registry.validate()?;
    fs::create_dir_all(base).map_err(|source| ProfileError::Io {
        operation: "create",
        path: base.to_path_buf(),
        source,
    })?;
    let path = registry_path(base);
    let temp = path.with_extension("json.tmp");
    let serialized = serde_json::to_string_pretty(registry).map_err(ProfileError::Json)?;
    fs::write(&temp, serialized.as_bytes()).map_err(|source| ProfileError::Io {
        operation: "write",
        path: temp.clone(),
        source,
    })?;
    restrict_file(&temp)?;
    fs::rename(&temp, &path).map_err(|source| ProfileError::Io {
        operation: "publish",
        path: path.clone(),
        source,
    })?;
    Ok(())
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> ProfileResult<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
        ProfileError::Io {
            operation: "protect",
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> ProfileResult<()> {
    Ok(())
}

/// The id of the profile this process is running as.
///
/// [`PROFILE_ENV_VAR`] wins over the registry so one command can run against
/// another profile without switching the app's — but only for a profile that
/// exists, because an env var that silently creates an empty identity is a
/// typo that looks like data loss.
pub fn active_id(base: &Path) -> ProfileResult<String> {
    let registry = load_registry(base)?;
    selected_id(&registry)
}

pub(crate) fn selected_id(registry: &ProfileRegistry) -> ProfileResult<String> {
    match std::env::var(PROFILE_ENV_VAR) {
        Ok(requested) if !requested.trim().is_empty() => {
            let requested = requested.trim().to_string();
            validate_id(&requested)?;
            if registry.get(&requested).is_none() {
                return Err(ProfileError::UnknownProfile(requested));
            }
            Ok(requested)
        }
        _ => Ok(registry.active().id),
    }
}

/// The active profile's data root, created if it does not exist.
pub fn active_root(base: &Path) -> ProfileResult<PathBuf> {
    let root = profile_root(base, &active_id(base)?);
    fs::create_dir_all(&root).map_err(|source| ProfileError::Io {
        operation: "create",
        path: root.clone(),
        source,
    })?;
    Ok(root)
}

/// The full record of the profile this process is running as.
pub fn active_profile(base: &Path) -> ProfileResult<Profile> {
    let registry = load_registry(base)?;
    let id = active_id(base)?;
    registry
        .get(&id)
        .cloned()
        .ok_or(ProfileError::UnknownProfile(id))
}

/// Namespaces an OS keychain service name for the active profile.
///
/// The keychain is not a directory, so the path scoping that isolates every
/// other store cannot reach it. Suffixing the *service* makes a second
/// profile's secrets a distinct keychain item that the first profile's code
/// never names. `default` is returned unchanged so existing credentials keep
/// resolving after an upgrade.
///
/// A failure to resolve the profile falls back to the default service rather
/// than to a made-up one: the fallback must be the identity whose data root
/// [`active_root`] would also fall back to reporting an error about, and a
/// wrong-but-unreadable service name would silently look like "no credentials
/// stored" instead of an error.
#[must_use]
pub fn keychain_service(base_service: &str) -> String {
    let Some(base) = crate::app_paths::base_data_dir() else {
        return base_service.to_string();
    };
    keychain_service_in(&base, base_service)
}

/// [`keychain_service`] against an explicit base directory, so the rule is
/// testable without touching the developer's real app data directory.
#[must_use]
pub fn keychain_service_in(base: &Path, base_service: &str) -> String {
    match active_id(base) {
        Ok(id) if id != DEFAULT_PROFILE_ID => format!("{base_service}.profile.{id}"),
        Ok(_) => base_service.to_string(),
        Err(error) => {
            eprintln!("little-monkey: falling back to the default keychain service: {error}");
            base_service.to_string()
        }
    }
}

/// Creates a profile with its own data root and returns it.
pub fn create_profile(base: &Path, name: &str) -> ProfileResult<Profile> {
    validate_name(name)?;
    let mut registry = load_registry(base)?;
    if registry.profiles.len() >= MAX_PROFILES {
        return Err(ProfileError::TooManyProfiles);
    }
    let slug = slugify(name).ok_or_else(|| {
        ProfileError::Invalid("profile name must contain at least one letter or digit".to_string())
    })?;
    let id = unique_id(&registry, &slug)?;
    let profile = Profile::new(id, name.trim().to_string(), now_ms());
    profile.validate()?;
    let root = profile_root(base, &profile.id);
    fs::create_dir_all(&root).map_err(|source| ProfileError::Io {
        operation: "create",
        path: root.clone(),
        source,
    })?;
    registry.profiles.push(profile.clone());
    save_registry(base, &registry)?;
    Ok(profile)
}

/// `work`, then `work-2`, `work-3`… — a display name may repeat, an id may not.
fn unique_id(registry: &ProfileRegistry, slug: &str) -> ProfileResult<String> {
    if slug != DEFAULT_PROFILE_ID && registry.get(slug).is_none() {
        validate_id(slug)?;
        return Ok(slug.to_string());
    }
    for suffix in 2..=MAX_PROFILES + 1 {
        let mut candidate = format!("{slug}-{suffix}");
        if candidate.len() > MAX_ID_BYTES {
            let keep = MAX_ID_BYTES - format!("-{suffix}").len();
            candidate = format!("{}-{suffix}", &slug[..keep]);
        }
        let candidate = candidate.trim_matches('-').to_string();
        if registry.get(&candidate).is_none() {
            validate_id(&candidate)?;
            return Ok(candidate);
        }
    }
    Err(ProfileError::DuplicateProfile(slug.to_string()))
}

pub fn rename_profile(base: &Path, id: &str, name: &str) -> ProfileResult<Profile> {
    validate_name(name)?;
    let mut registry = load_registry(base)?;
    let profile = registry
        .profiles
        .iter_mut()
        .find(|profile| profile.id == id)
        .ok_or_else(|| ProfileError::UnknownProfile(id.to_string()))?;
    profile.name = name.trim().to_string();
    let renamed = profile.clone();
    save_registry(base, &registry)?;
    Ok(renamed)
}

/// Sets a profile's quota and share weight.
pub fn set_limits(
    base: &Path,
    id: &str,
    quota: ProfileQuota,
    fair_share_weight: f64,
) -> ProfileResult<Profile> {
    quota.validate()?;
    validate_weight(fair_share_weight)?;
    let mut registry = load_registry(base)?;
    let profile = registry
        .profiles
        .iter_mut()
        .find(|profile| profile.id == id)
        .ok_or_else(|| ProfileError::UnknownProfile(id.to_string()))?;
    profile.quota = quota;
    profile.fair_share_weight = fair_share_weight;
    let updated = profile.clone();
    save_registry(base, &registry)?;
    Ok(updated)
}

/// Records which profile the *next* process start runs as.
///
/// Deliberately does not touch anything already open — see the module header:
/// the caller restarts, and a fresh process is what makes the switch total.
pub fn switch_profile(base: &Path, id: &str) -> ProfileResult<Profile> {
    let mut registry = load_registry(base)?;
    let profile = registry
        .get(id)
        .cloned()
        .ok_or_else(|| ProfileError::UnknownProfile(id.to_string()))?;
    registry.active_id = profile.id.clone();
    save_registry(base, &registry)?;
    let root = profile_root(base, &profile.id);
    fs::create_dir_all(&root).map_err(|source| ProfileError::Io {
        operation: "create",
        path: root.clone(),
        source,
    })?;
    Ok(profile)
}

/// Deletes a profile and everything under its managed and authored roots.
///
/// Refused for the default profile (it is the installation itself) and for the
/// active one (the running process holds its files open). A profile selected by
/// this process through [`PROFILE_ENV_VAR`] is active too. *Another* process
/// running a profile override is not detectable from here and is not protected
/// against — the registry records which profile is active, not which ones are
/// open.
///
/// ponytail: the profile's keychain entries are *orphaned*, not deleted — the
/// `keyring` crate cannot enumerate a service's items, so there is no list to
/// walk. They become unreachable, because nothing will ever name that service
/// again, and a reused profile id would need the same name to reach them; the
/// upgrade path is a per-profile index of stored credential references, which
/// no caller keeps today.
pub fn delete_profile(base: &Path, agent_home: &Path, id: &str) -> ProfileResult<()> {
    if id == DEFAULT_PROFILE_ID {
        return Err(ProfileError::Invalid(
            "the default profile cannot be deleted".to_string(),
        ));
    }
    validate_id(id)?;
    let mut registry = load_registry(base)?;
    let profile_exists = registry.get(id).is_some();
    let recovered_staging = reconcile_staged_profile_deletion(
        &[agent_home.to_path_buf(), base.to_path_buf()],
        id,
        profile_exists,
    )?;
    if !profile_exists {
        if recovered_staging {
            return Ok(());
        }
        return Err(ProfileError::UnknownProfile(id.to_string()));
    }
    let process_active_id = active_id(base)?;
    if deletion_targets_active_profile(&registry, &process_active_id, id) {
        return Err(ProfileError::Invalid(
            "switch to another profile before deleting this one".to_string(),
        ));
    }
    refuse_installed_profile_daemon(base, id)?;

    // Validate both roots before moving either one. In particular, inspecting
    // `<root>/profiles` alone would follow a symlinked `<root>` first.
    let roots = [
        profile_child_for_deletion(agent_home, id)?,
        profile_child_for_deletion(base, id)?,
    ]
    .into_iter()
    .flatten()
    .fold(Vec::new(), |mut unique, root| {
        if !unique.contains(&root) {
            unique.push(root);
        }
        unique
    });
    let mut staged = Vec::new();
    for root in roots {
        match stage_profile_root(&root, id) {
            Ok(staged_root) => staged.push(staged_root),
            Err(error) => return Err(error_with_rollback(error, &staged)),
        }
    }

    registry.profiles.retain(|profile| profile.id != id);
    if let Err(error) = save_registry(base, &registry) {
        return Err(error_with_rollback(error, &staged));
    }

    cleanup_staged_profile_roots(&staged)
}

fn refuse_installed_profile_daemon(base: &Path, id: &str) -> ProfileResult<()> {
    let config = profile_root(base, id).join("daemon").join("config.json");
    match fs::symlink_metadata(&config) {
        Ok(_) => Err(ProfileError::Invalid(format!(
            "profile '{id}' has an installed daemon; run `monkey --profile {id} daemon uninstall` before deleting it"
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ProfileError::Io {
            operation: "inspect",
            path: config,
            source,
        }),
    }
}

fn deletion_targets_active_profile(
    registry: &ProfileRegistry,
    process_active_id: &str,
    id: &str,
) -> bool {
    registry.active_id == id || process_active_id == id
}

fn real_directory_exists(path: &Path) -> ProfileResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(ProfileError::Invalid(format!(
                "profile parent '{}' must be a real directory",
                path.display()
            )))
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(ProfileError::Io {
            operation: "inspect",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn reconcile_staged_profile_deletion(
    roots: &[PathBuf],
    id: &str,
    profile_exists: bool,
) -> ProfileResult<bool> {
    let prefix = format!(".{id}.delete-");
    let mut parents = Vec::new();
    for root in roots {
        if !real_directory_exists(root)? {
            continue;
        }
        let parent = root.join(PROFILES_DIR);
        if !real_directory_exists(&parent)? || parents.contains(&parent) {
            continue;
        }
        parents.push(parent);
    }

    let mut found_any = false;
    for parent in parents {
        let mut staged = Vec::new();
        let entries = fs::read_dir(&parent).map_err(|source| ProfileError::Io {
            operation: "read",
            path: parent.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| ProfileError::Io {
                operation: "read",
                path: parent.clone(),
                source,
            })?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(&prefix))
            {
                staged.push(entry.path());
            }
        }
        if staged.is_empty() {
            continue;
        }
        found_any = true;
        if profile_exists {
            if staged.len() != 1 {
                return Err(ProfileError::Invalid(format!(
                    "profile '{id}' has multiple interrupted deletion stages under '{}'",
                    parent.display()
                )));
            }
            let original = parent.join(id);
            match fs::symlink_metadata(&original) {
                Ok(_) => {
                    return Err(ProfileError::Invalid(format!(
                        "cannot restore interrupted deletion for '{id}': '{}' already exists",
                        original.display()
                    )))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(ProfileError::Io {
                        operation: "inspect",
                        path: original,
                        source,
                    })
                }
            }
            fs::rename(&staged[0], &original).map_err(|source| ProfileError::Io {
                operation: "restore",
                path: original,
                source,
            })?;
        } else {
            for path in staged {
                remove_staged_profile_root(&path)?;
            }
        }
    }
    Ok(found_any)
}

fn profile_child_for_deletion(root: &Path, id: &str) -> ProfileResult<Option<PathBuf>> {
    if !real_directory_exists(root)? {
        return Ok(None);
    }
    let parent = root.join(PROFILES_DIR);
    if !real_directory_exists(&parent)? {
        return Ok(None);
    }
    let parent = fs::canonicalize(&parent).map_err(|source| ProfileError::Io {
        operation: "canonicalize",
        path: parent,
        source,
    })?;
    let child = parent.join(id);
    match fs::symlink_metadata(&child) {
        Ok(_) => Ok(Some(child)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ProfileError::Io {
            operation: "inspect",
            path: child,
            source,
        }),
    }
}

#[derive(Debug)]
struct StagedProfileRoot {
    original: PathBuf,
    staged: PathBuf,
}

fn stage_profile_root(root: &Path, id: &str) -> ProfileResult<StagedProfileRoot> {
    let parent = root.parent().ok_or_else(|| {
        ProfileError::Invalid(format!("profile root '{}' has no parent", root.display()))
    })?;
    let sequence = DELETE_STAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let staged = parent.join(format!(
        ".{id}.delete-{}-{}-{sequence}",
        std::process::id(),
        now_ms()
    ));
    match fs::symlink_metadata(&staged) {
        Ok(_) => {
            return Err(ProfileError::Invalid(format!(
                "profile deletion staging path '{}' already exists",
                staged.display()
            )))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(ProfileError::Io {
                operation: "inspect",
                path: staged,
                source,
            })
        }
    }
    fs::rename(root, &staged).map_err(|source| ProfileError::Io {
        operation: "stage delete",
        path: root.to_path_buf(),
        source,
    })?;
    Ok(StagedProfileRoot {
        original: root.to_path_buf(),
        staged,
    })
}

fn restore_staged_profile_roots(staged: &[StagedProfileRoot]) -> ProfileResult<()> {
    let mut first_error = None;
    for root in staged.iter().rev() {
        let result = match fs::symlink_metadata(&root.original) {
            Ok(_) => Err(ProfileError::Invalid(format!(
                "cannot restore '{}': a new path already exists",
                root.original.display()
            ))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::rename(&root.staged, &root.original).map_err(|source| ProfileError::Io {
                    operation: "restore",
                    path: root.original.clone(),
                    source,
                })
            }
            Err(source) => Err(ProfileError::Io {
                operation: "inspect",
                path: root.original.clone(),
                source,
            }),
        };
        if first_error.is_none() {
            first_error = result.err();
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn error_with_rollback(error: ProfileError, staged: &[StagedProfileRoot]) -> ProfileError {
    match restore_staged_profile_roots(staged) {
        Ok(()) => error,
        Err(rollback_error) => {
            ProfileError::Invalid(format!("{error}; rollback failed: {rollback_error}"))
        }
    }
}

fn remove_staged_profile_root(path: &Path) -> ProfileResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            #[cfg(windows)]
            {
                use std::os::windows::fs::FileTypeExt;
                if metadata.file_type().is_symlink_dir() {
                    fs::remove_dir(path)
                } else {
                    fs::remove_file(path)
                }
            }
            #[cfg(not(windows))]
            {
                fs::remove_file(path)
            }
        }
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(ProfileError::Io {
                operation: "inspect",
                path: path.to_path_buf(),
                source,
            })
        }
    }
    .map_err(|source| ProfileError::Io {
        operation: "remove",
        path: path.to_path_buf(),
        source,
    })
}

fn cleanup_staged_profile_roots(staged: &[StagedProfileRoot]) -> ProfileResult<()> {
    let mut first_error = None;
    for root in staged {
        if let Err(error) = remove_staged_profile_root(&root.staged) {
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// What one profile's daemon must hold itself to on a machine it shares with
/// the other profiles.
///
/// Two different kinds of number, deliberately in one place because they are
/// enforced together: the **quota** is absolute and comes from K4 (a ceiling
/// the operator set), the **share** is relative and comes from K8 (this
/// profile's weight against every other profile's). Whichever binds first
/// wins.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProfileLimits {
    pub quota: ProfileQuota,
    /// Fraction of a contended machine resource this profile may claim, in
    /// `0.0..=1.0`. Exactly `1.0` on a single-profile installation.
    ///
    /// A ceiling rather than a work-conserving share, and the difference is
    /// worth naming: an idle profile's fraction is *not* lent to a busy one,
    /// because each profile's daemon has its own queue and neither can see
    /// that the other is idle. Lending would need an arbiter above both.
    pub memory_share: f64,
}

impl Default for ProfileLimits {
    fn default() -> Self {
        Self::unbounded()
    }
}

impl ProfileLimits {
    /// The whole machine and no quota — what a single-profile installation has
    /// always had, and what a test daemon gets unless it says otherwise.
    #[must_use]
    pub const fn unbounded() -> Self {
        Self {
            quota: ProfileQuota {
                max_concurrent_runs: None,
                max_memory_bytes: None,
                max_runtime_ms: None,
            },
            memory_share: 1.0,
        }
    }

    /// The limits of the profile this process is running as.
    pub fn for_active(base: &Path) -> ProfileResult<Self> {
        let registry = load_registry(base)?;
        let id = active_id(base)?;
        let profile = registry
            .get(&id)
            .ok_or_else(|| ProfileError::UnknownProfile(id.clone()))?;
        Ok(Self {
            quota: profile.quota,
            memory_share: registry.share_of(&id),
        })
    }

    /// System memory this profile's admitted work may hold at once, or `None`
    /// when nothing bounds it — a single-profile installation with no quota,
    /// which is every installation until someone creates a second profile.
    #[must_use]
    pub fn memory_ceiling_bytes(&self, total_ram_bytes: u64) -> Option<u64> {
        let share = if self.memory_share >= 1.0 {
            None
        } else {
            let share = (total_ram_bytes as f64 * self.memory_share.clamp(0.0, 1.0)) as u64;
            Some(share)
        };
        match (share, self.quota.max_memory_bytes) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (value, None) | (None, value) => value,
        }
    }
}

/// Resolves the active profile's data root from an `AppHandle`.
///
/// This is the desktop half of the isolation boundary: every call site that
/// used to ask Tauri for `app_data_dir()` asks this instead, so a store added
/// later is profile-scoped by construction rather than by remembering.
pub trait ProfileScopedPaths<R: tauri::Runtime> {
    fn profile_data_dir(&self) -> tauri::Result<PathBuf>;
}

/// Implemented for every `Manager` — `AppHandle`, `App`, and a `Window` alike —
/// because the call sites this replaces used all three, and a boundary that
/// only covers one of them is a boundary with a hole in it.
impl<R: tauri::Runtime, M: tauri::Manager<R>> ProfileScopedPaths<R> for M {
    fn profile_data_dir(&self) -> tauri::Result<PathBuf> {
        let base = self.path().app_data_dir()?;
        active_root(&base).map_err(|error| tauri::Error::Io(error.into()))
    }
}

/// One row of the profile switcher: the record plus the two things only the
/// registry as a whole can answer — whether this is the identity the process is
/// running as, and what fraction of the machine it is entitled to.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSummary {
    #[serde(flatten)]
    pub profile: Profile,
    pub active: bool,
    pub root: PathBuf,
    pub share: f64,
}

/// The registry's own directory — the *unscoped* app data directory.
///
/// Every other command resolves through the active profile; these ones must
/// not, because the registry is what decides which profile that is.
fn registry_base<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<PathBuf, String> {
    use tauri::Manager as _;
    app.path()
        .app_data_dir()
        .map_err(|error| format!("Failed to resolve app data dir: {error}"))
}

fn summaries(base: &Path) -> Result<Vec<ProfileSummary>, String> {
    let registry = load_registry(base).map_err(|error| error.to_string())?;
    let active = active_id(base).map_err(|error| error.to_string())?;
    Ok(registry
        .profiles
        .iter()
        .map(|profile| ProfileSummary {
            active: profile.id == active,
            root: profile_root(base, &profile.id),
            share: registry.share_of(&profile.id),
            profile: profile.clone(),
        })
        .collect())
}

#[tauri::command]
pub fn profiles_list(app: tauri::AppHandle) -> Result<Vec<ProfileSummary>, String> {
    summaries(&registry_base(&app)?)
}

#[tauri::command]
pub fn profiles_create(app: tauri::AppHandle, name: String) -> Result<Vec<ProfileSummary>, String> {
    let base = registry_base(&app)?;
    create_profile(&base, &name).map_err(|error| error.to_string())?;
    summaries(&base)
}

#[tauri::command]
pub fn profiles_rename(
    app: tauri::AppHandle,
    id: String,
    name: String,
) -> Result<Vec<ProfileSummary>, String> {
    let base = registry_base(&app)?;
    rename_profile(&base, &id, &name).map_err(|error| error.to_string())?;
    summaries(&base)
}

#[tauri::command]
pub fn profiles_set_limits(
    app: tauri::AppHandle,
    id: String,
    quota: ProfileQuota,
    fair_share_weight: f64,
) -> Result<Vec<ProfileSummary>, String> {
    let base = registry_base(&app)?;
    set_limits(&base, &id, quota, fair_share_weight).map_err(|error| error.to_string())?;
    summaries(&base)
}

#[tauri::command]
pub fn profiles_delete(app: tauri::AppHandle, id: String) -> Result<Vec<ProfileSummary>, String> {
    let base = registry_base(&app)?;
    let agent_home = crate::app_paths::agent_home_dir()?;
    delete_profile(&base, &agent_home, &id).map_err(|error| error.to_string())?;
    summaries(&base)
}

/// Switches profile and restarts the app.
///
/// The restart is the switch, not a convenience on top of it: an open ledger
/// connection, a cached artifact store, a running MCP server and a spawned
/// daemon were all built from the *previous* profile's paths, and swapping the
/// registry entry underneath them would leave this profile's work writing into
/// that profile's files. A new process has none of them. The registry write
/// happens first and is atomic, so a failure to restart leaves the choice
/// recorded rather than half-applied.
#[tauri::command]
pub fn profiles_switch(app: tauri::AppHandle, id: String) -> Result<(), String> {
    let base = registry_base(&app)?;
    switch_profile(&base, &id).map_err(|error| error.to_string())?;
    app.restart();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_base(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "little_monkey_profiles_{label}_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst),
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn an_installation_without_a_registry_is_the_default_profile() {
        let base = temp_base("legacy");
        let registry = load_registry(&base).unwrap();
        assert_eq!(registry.active_id, DEFAULT_PROFILE_ID);
        assert_eq!(active_root(&base).unwrap(), base);
        assert_eq!(
            keychain_service_in(&base, "com.littlemonkey.app"),
            "com.littlemonkey.app"
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_created_profile_gets_its_own_root_and_the_default_keeps_the_legacy_one() {
        let base = temp_base("roots");
        let work = create_profile(&base, "Work").unwrap();
        assert_eq!(work.id, "work");
        assert_eq!(
            profile_root(&base, &work.id),
            base.join("profiles").join("work")
        );
        assert!(base.join("profiles").join("work").is_dir());
        assert_eq!(profile_root(&base, DEFAULT_PROFILE_ID), base);
        assert_ne!(
            profile_root(&base, &work.id),
            profile_root(&base, DEFAULT_PROFILE_ID)
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn switching_only_changes_which_root_the_next_resolution_returns() {
        let base = temp_base("switch");
        let work = create_profile(&base, "Work").unwrap();
        assert_eq!(active_root(&base).unwrap(), base);
        switch_profile(&base, &work.id).unwrap();
        assert_eq!(
            active_root(&base).unwrap(),
            base.join("profiles").join("work")
        );
        assert_eq!(active_profile(&base).unwrap().id, "work");
        assert_eq!(
            keychain_service_in(&base, "com.littlemonkey.app"),
            "com.littlemonkey.app.profile.work"
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn ids_are_unique_even_when_display_names_repeat() {
        let base = temp_base("unique");
        let first = create_profile(&base, "Work").unwrap();
        let second = create_profile(&base, "Work").unwrap();
        assert_eq!(first.id, "work");
        assert_eq!(second.id, "work-2");
        assert_eq!(create_profile(&base, "Default").unwrap().id, "default-2");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn an_id_can_never_escape_the_profiles_directory() {
        for hostile in [
            "../evil", "..", "a/b", "C:", ".hidden", "UPPER", "-lead", "trail-",
        ] {
            assert!(validate_id(hostile).is_err(), "{hostile} must be rejected");
        }
        assert!(validate_id("work-2").is_ok());
        assert_eq!(slugify("../../etc/passwd").unwrap(), "etc-passwd");
        assert!(slugify("🙂").is_none());
    }

    #[test]
    fn the_default_profile_cannot_be_deleted_and_neither_can_the_active_one() {
        let base = temp_base("delete");
        let agent_home = temp_base("delete-agent-home");
        let work = create_profile(&base, "Work").unwrap();
        fs::write(profile_root(&base, &work.id).join("secret.txt"), b"x").unwrap();
        let authored_root = agent_home.join(PROFILES_DIR).join(&work.id);
        fs::create_dir_all(&authored_root).unwrap();
        fs::write(authored_root.join("hooks.json"), b"{}").unwrap();
        assert!(delete_profile(&base, &agent_home, DEFAULT_PROFILE_ID).is_err());
        switch_profile(&base, &work.id).unwrap();
        assert!(delete_profile(&base, &agent_home, &work.id).is_err());
        switch_profile(&base, DEFAULT_PROFILE_ID).unwrap();
        delete_profile(&base, &agent_home, &work.id).unwrap();
        assert!(!profile_root(&base, "work").exists());
        assert!(!authored_root.exists());
        assert!(load_registry(&base).unwrap().get("work").is_none());
        fs::remove_dir_all(&base).unwrap();
        fs::remove_dir_all(&agent_home).unwrap();
    }

    #[test]
    fn a_process_profile_override_counts_as_active_for_deletion() {
        let base = temp_base("delete-process-active");
        let work = create_profile(&base, "Work").unwrap();
        let registry = load_registry(&base).unwrap();

        assert!(deletion_targets_active_profile(
            &registry, &work.id, &work.id
        ));
        assert!(!deletion_targets_active_profile(
            &registry,
            DEFAULT_PROFILE_ID,
            &work.id
        ));

        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn profile_delete_restores_both_roots_when_registry_persistence_fails() {
        let base = temp_base("delete-rollback-base");
        let agent_home = temp_base("delete-rollback-home");
        let work = create_profile(&base, "Work").unwrap();
        let managed_file = profile_root(&base, &work.id).join("secret.txt");
        fs::write(&managed_file, b"managed").unwrap();
        let authored_file = agent_home
            .join(PROFILES_DIR)
            .join(&work.id)
            .join("hooks.json");
        fs::create_dir_all(authored_file.parent().unwrap()).unwrap();
        fs::write(&authored_file, b"{}").unwrap();

        // `save_registry` writes this path before publishing it. A directory at
        // that path forces the persistence step to fail after both roots stage.
        let registry_temp = registry_path(&base).with_extension("json.tmp");
        fs::create_dir(&registry_temp).unwrap();

        assert!(delete_profile(&base, &agent_home, &work.id).is_err());
        assert_eq!(fs::read(&managed_file).unwrap(), b"managed");
        assert_eq!(fs::read(&authored_file).unwrap(), b"{}");
        assert!(load_registry(&base).unwrap().get(&work.id).is_some());

        fs::remove_dir_all(base).unwrap();
        fs::remove_dir_all(agent_home).unwrap();
    }

    #[test]
    fn profile_delete_recovers_a_pre_commit_interrupted_stage() {
        let base = temp_base("delete-recover-pre-commit");
        let agent_home = temp_base("delete-recover-pre-commit-home");
        let work = create_profile(&base, "Work").unwrap();
        let managed_root = profile_root(&base, &work.id);
        let authored_root = agent_home.join(PROFILES_DIR).join(&work.id);
        fs::create_dir_all(&authored_root).unwrap();
        fs::write(authored_root.join("MONKEY.md"), b"rules").unwrap();
        let staged_managed = stage_profile_root(&managed_root, &work.id).unwrap();
        let staged_authored = stage_profile_root(&authored_root, &work.id).unwrap();

        delete_profile(&base, &agent_home, &work.id).unwrap();

        assert!(!managed_root.exists());
        assert!(!authored_root.exists());
        assert!(!staged_managed.staged.exists());
        assert!(!staged_authored.staged.exists());
        assert!(load_registry(&base).unwrap().get(&work.id).is_none());
        fs::remove_dir_all(base).unwrap();
        fs::remove_dir_all(agent_home).unwrap();
    }

    #[test]
    fn profile_delete_retry_cleans_a_post_commit_tombstone() {
        let base = temp_base("delete-recover-post-commit");
        let agent_home = temp_base("delete-recover-post-commit-home");
        let work = create_profile(&base, "Work").unwrap();
        let staged = stage_profile_root(&profile_root(&base, &work.id), &work.id).unwrap();
        let mut registry = load_registry(&base).unwrap();
        registry.profiles.retain(|profile| profile.id != work.id);
        save_registry(&base, &registry).unwrap();

        delete_profile(&base, &agent_home, &work.id).unwrap();

        assert!(!staged.staged.exists());
        fs::remove_dir_all(base).unwrap();
        fs::remove_dir_all(agent_home).unwrap();
    }

    #[test]
    fn profile_delete_deduplicates_identical_authored_and_managed_roots() {
        let base = temp_base("delete-shared-root");
        let work = create_profile(&base, "Work").unwrap();
        let file = profile_root(&base, &work.id).join("MONKEY.md");
        fs::write(&file, b"rules").unwrap();

        delete_profile(&base, &base, &work.id).unwrap();

        assert!(!file.exists());
        assert!(load_registry(&base).unwrap().get(&work.id).is_none());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn profile_delete_deduplicates_aliased_authored_and_managed_roots() {
        let base = temp_base("delete-aliased-root");
        let work = create_profile(&base, "Work").unwrap();
        let file = profile_root(&base, &work.id).join("MONKEY.md");
        fs::write(&file, b"rules").unwrap();
        fs::create_dir(base.join("alias")).unwrap();
        let aliased_home = base.join("alias").join("..");

        delete_profile(&base, &aliased_home, &work.id).unwrap();

        assert!(!file.exists());
        assert!(load_registry(&base).unwrap().get(&work.id).is_none());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn profile_delete_refuses_an_installed_profile_daemon() {
        let base = temp_base("delete-installed-daemon");
        let agent_home = temp_base("delete-installed-daemon-home");
        let work = create_profile(&base, "Work").unwrap();
        let managed_root = profile_root(&base, &work.id);
        let daemon_config = managed_root.join("daemon/config.json");
        fs::create_dir_all(daemon_config.parent().unwrap()).unwrap();
        fs::write(&daemon_config, b"{}").unwrap();

        let error = delete_profile(&base, &agent_home, &work.id).unwrap_err();

        assert!(error.to_string().contains("daemon uninstall"));
        assert!(daemon_config.exists());
        assert!(load_registry(&base).unwrap().get(&work.id).is_some());
        fs::remove_dir_all(base).unwrap();
        fs::remove_dir_all(agent_home).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn profile_delete_refuses_a_symlinked_authored_parent() {
        use std::os::unix::fs::symlink;

        let base = temp_base("delete-symlink-base");
        let agent_home = temp_base("delete-symlink-home");
        let external = temp_base("delete-symlink-external");
        let work = create_profile(&base, "Work").unwrap();
        let external_profile = external.join(&work.id);
        fs::create_dir_all(&external_profile).unwrap();
        fs::write(external_profile.join("keep.txt"), b"keep").unwrap();
        symlink(&external, agent_home.join(PROFILES_DIR)).unwrap();

        assert!(delete_profile(&base, &agent_home, &work.id).is_err());
        assert!(external_profile.join("keep.txt").exists());
        assert!(profile_root(&base, &work.id).exists());
        assert!(load_registry(&base).unwrap().get(&work.id).is_some());

        fs::remove_file(agent_home.join(PROFILES_DIR)).unwrap();
        fs::remove_dir_all(base).unwrap();
        fs::remove_dir_all(agent_home).unwrap();
        fs::remove_dir_all(external).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn profile_delete_preflights_a_symlinked_managed_parent_before_staging_authored_data() {
        use std::os::unix::fs::symlink;

        let base = temp_base("delete-managed-symlink-base");
        let agent_home = temp_base("delete-managed-symlink-home");
        let external = temp_base("delete-managed-symlink-external");
        let work = create_profile(&base, "Work").unwrap();
        let authored_file = agent_home
            .join(PROFILES_DIR)
            .join(&work.id)
            .join("hooks.json");
        fs::create_dir_all(authored_file.parent().unwrap()).unwrap();
        fs::write(&authored_file, b"{}").unwrap();

        fs::remove_dir_all(base.join(PROFILES_DIR)).unwrap();
        let external_profile = external.join(&work.id);
        fs::create_dir_all(&external_profile).unwrap();
        fs::write(external_profile.join("keep.txt"), b"keep").unwrap();
        symlink(&external, base.join(PROFILES_DIR)).unwrap();

        assert!(delete_profile(&base, &agent_home, &work.id).is_err());
        assert!(authored_file.exists());
        assert!(external_profile.join("keep.txt").exists());
        assert!(load_registry(&base).unwrap().get(&work.id).is_some());

        fs::remove_file(base.join(PROFILES_DIR)).unwrap();
        fs::remove_dir_all(base).unwrap();
        fs::remove_dir_all(agent_home).unwrap();
        fs::remove_dir_all(external).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn staged_windows_directory_symlink_cleanup_removes_only_the_link() {
        use std::os::windows::fs::symlink_dir;

        let parent = temp_base("delete-windows-link-parent");
        let target = temp_base("delete-windows-link-target");
        fs::write(target.join("keep.txt"), b"keep").unwrap();
        let link = parent.join("staged-link");
        match symlink_dir(&target, &link) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                fs::remove_dir_all(parent).unwrap();
                fs::remove_dir_all(target).unwrap();
                return;
            }
            Err(error) => panic!("failed to create directory symlink: {error}"),
        }

        remove_staged_profile_root(&link).unwrap();
        assert!(fs::symlink_metadata(&link).is_err());
        assert_eq!(fs::read(target.join("keep.txt")).unwrap(), b"keep");

        fs::remove_dir_all(parent).unwrap();
        fs::remove_dir_all(target).unwrap();
    }

    #[test]
    fn a_corrupt_registry_is_an_error_rather_than_a_silent_default() {
        let base = temp_base("corrupt");
        fs::write(registry_path(&base), b"{not json").unwrap();
        assert!(load_registry(&base).is_err());
        assert!(active_root(&base).is_err());

        fs::write(
            registry_path(&base),
            br#"{"version":99,"activeId":"default","profiles":[]}"#,
        )
        .unwrap();
        assert!(matches!(
            load_registry(&base),
            Err(ProfileError::UnsupportedVersion(99))
        ));
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_single_profile_owns_the_whole_machine_and_two_split_it_by_weight() {
        let base = temp_base("share");
        assert_eq!(
            load_registry(&base).unwrap().share_of(DEFAULT_PROFILE_ID),
            1.0
        );

        let work = create_profile(&base, "Work").unwrap();
        set_limits(&base, &work.id, ProfileQuota::default(), 3.0).unwrap();
        let registry = load_registry(&base).unwrap();
        assert_eq!(registry.share_of(DEFAULT_PROFILE_ID), 0.25);
        assert_eq!(registry.share_of(&work.id), 0.75);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_quota_only_ever_tightens_what_the_daemon_already_configured() {
        let quota = ProfileQuota {
            max_concurrent_runs: Some(2),
            max_memory_bytes: Some(1024),
            max_runtime_ms: Some(60_000),
        };
        assert_eq!(quota.clamp_concurrency(8), 2);
        assert_eq!(quota.clamp_concurrency(1), 1);
        assert_eq!(quota.clamp_runtime_ms(Some(10_000)), Some(10_000));
        assert_eq!(quota.clamp_runtime_ms(Some(600_000)), Some(60_000));
        assert_eq!(quota.clamp_runtime_ms(None), Some(60_000));

        let unbounded = ProfileQuota::default();
        assert_eq!(unbounded.clamp_concurrency(8), 8);
        assert_eq!(unbounded.clamp_runtime_ms(Some(10)), Some(10));
        assert_eq!(unbounded.clamp_runtime_ms(None), None);
        assert!(ProfileQuota {
            max_concurrent_runs: Some(0),
            ..ProfileQuota::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn a_lone_profile_is_bounded_by_its_quota_only_and_a_share_binds_when_tighter() {
        const MACHINE: u64 = 16 * 1024 * 1024 * 1024;

        // Every installation until a second profile exists.
        assert_eq!(
            ProfileLimits::unbounded().memory_ceiling_bytes(MACHINE),
            None
        );

        // A quota with the whole machine still bounds.
        let quota_only = ProfileLimits {
            quota: ProfileQuota {
                max_memory_bytes: Some(4 * 1024 * 1024 * 1024),
                ..ProfileQuota::default()
            },
            memory_share: 1.0,
        };
        assert_eq!(
            quota_only.memory_ceiling_bytes(MACHINE),
            Some(4 * 1024 * 1024 * 1024)
        );

        // A quarter share of a 16 GiB machine is 4 GiB; the 8 GiB quota is
        // looser, so the share is what binds.
        let shared = ProfileLimits {
            quota: ProfileQuota {
                max_memory_bytes: Some(8 * 1024 * 1024 * 1024),
                ..ProfileQuota::default()
            },
            memory_share: 0.25,
        };
        assert_eq!(
            shared.memory_ceiling_bytes(MACHINE),
            Some(4 * 1024 * 1024 * 1024)
        );
    }

    #[test]
    fn the_active_profiles_limits_come_from_the_registry() {
        let base = temp_base("limits");
        assert_eq!(
            ProfileLimits::for_active(&base).unwrap(),
            ProfileLimits::unbounded()
        );

        let work = create_profile(&base, "Work").unwrap();
        let quota = ProfileQuota {
            max_concurrent_runs: Some(2),
            max_memory_bytes: None,
            max_runtime_ms: None,
        };
        set_limits(&base, &work.id, quota, 3.0).unwrap();
        switch_profile(&base, &work.id).unwrap();

        let limits = ProfileLimits::for_active(&base).unwrap();
        assert_eq!(limits.quota, quota);
        assert_eq!(limits.memory_share, 0.75);
        fs::remove_dir_all(&base).unwrap();
    }

    /// **K23's acceptance clause**, against the real stores rather than against
    /// the path arithmetic that produces them: one profile's run history,
    /// artifacts and credentials must be unreachable from another.
    ///
    /// Every leg opens the *same* store type the app opens, resolved the way
    /// the app resolves it, and then asks it for the other profile's data. The
    /// ledger leg is the load-bearing one: a run submitted as `default` is not
    /// merely filtered out of `work`'s listing, it is in a database `work`
    /// never opens.
    #[test]
    fn one_profile_cannot_read_anothers_runs_artifacts_or_credentials() {
        use crate::artifact_store::ArtifactStore;
        use crate::run_ledger::RunLedger;

        const LEDGER: &str = "profile-v1.sqlite3";
        const ARTIFACTS: &str = "artifacts";

        let base = temp_base("cross_profile");
        let work = create_profile(&base, "Work").unwrap();

        // As the default profile: one run and one artifact.
        let default_root = active_root(&base).unwrap();
        assert_eq!(default_root, base);
        let mut ledger = RunLedger::open(default_root.join(LEDGER)).unwrap();
        ledger.submit_run(&isolation_spec("run-default")).unwrap();
        assert_eq!(ledger.list_runs(16, true).unwrap().len(), 1);
        let artifacts = ArtifactStore::new(default_root.join(ARTIFACTS)).unwrap();
        let secret = artifacts
            .put(b"default profile's private artifact")
            .unwrap();
        assert!(artifacts.exists(&secret.id).unwrap());
        drop(ledger);

        // Switch. Everything the app resolves now resolves somewhere else.
        switch_profile(&base, &work.id).unwrap();
        let work_root = active_root(&base).unwrap();
        assert_ne!(work_root, default_root);

        // Run history: a fresh ledger, not a filtered view of the other one.
        let mut work_ledger = RunLedger::open(work_root.join(LEDGER)).unwrap();
        assert!(
            work_ledger.list_runs(16, true).unwrap().is_empty(),
            "the work profile must not see the default profile's runs"
        );
        assert!(
            work_ledger.load_run("run-default").unwrap().is_none(),
            "naming the other profile's run id directly must not reach it"
        );
        // …and its own run does not appear in the default profile's history.
        work_ledger.submit_run(&isolation_spec("run-work")).unwrap();
        drop(work_ledger);
        let reopened_default = RunLedger::open(default_root.join(LEDGER)).unwrap();
        let default_runs = reopened_default.list_runs(16, true).unwrap();
        assert_eq!(default_runs.len(), 1);
        assert_eq!(default_runs[0].spec.run_id, "run-default");

        // Artifacts: the digest is content-addressed and therefore *guessable*,
        // which is exactly why this leg matters — knowing the id is not access.
        let work_artifacts = ArtifactStore::new(work_root.join(ARTIFACTS)).unwrap();
        assert!(
            !work_artifacts.exists(&secret.id).unwrap(),
            "the work profile must not resolve the default profile's artifact id"
        );
        assert!(work_artifacts.read(&secret.id).is_err());

        // Credentials: a different keychain service, so the same account name
        // addresses a different item.
        assert_eq!(
            keychain_service_in(&base, "com.littlemonkey.app"),
            "com.littlemonkey.app.profile.work"
        );

        // And the daemon queue, sessions and package set follow the same root.
        assert!(work_root.starts_with(base.join(PROFILES_DIR)));

        // Windows refuses to remove a directory holding an open file, and the
        // ledger connection is one — so close it before the cleanup, and treat
        // the cleanup itself as best-effort rather than as an assertion about
        // the host's file locking.
        drop(reopened_default);
        let _ = fs::remove_dir_all(&base);
    }

    fn isolation_spec(run_id: &str) -> crate::run_protocol::RunSpec {
        use crate::run_protocol::*;
        let capability = || CapabilityAssessment {
            state: CapabilityState::Supported,
            evidence: "profile isolation fixture".to_string(),
        };
        RunSpec {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            run_id: run_id.to_string(),
            idempotency_key: format!("{run_id}-key"),
            created_at_ms: 1_000,
            kind: RunKind::Background,
            submitted_by: ClientIdentity {
                client_id: "profile-test".to_string(),
                instance_id: "instance-01".to_string(),
                kind: ClientKind::Test,
                version: "1.0.0-test".to_string(),
            },
            task: "prove the profile boundary".to_string(),
            instructions: None,
            input_artifact_ids: Vec::new(),
            target: ModelTargetSnapshot::Ollama {
                target_id: "ollama-test".to_string(),
                label: "Ollama test".to_string(),
                base_url: "http://127.0.0.1:11434".to_string(),
                model: "qwen-test".to_string(),
                is_cloud: false,
                capabilities: ModelCapabilitiesSnapshot {
                    tool_calling: capability(),
                    vision: capability(),
                    embeddings: capability(),
                    structured_output: capability(),
                    image_generation: capability(),
                    audio: capability(),
                    runtime_lifecycle: capability(),
                    fim: capability(),
                    code_completion: capability(),
                    inline_edit: capability(),
                    fim_metadata: None,
                },
                estimated_memory_bytes: Some(1),
            },
            workspace: None,
            permission_policy: PermissionPolicySnapshot {
                mode: PermissionMode::Manual,
                unattended: false,
                approval_timeout_ms: 60_000,
                default_tool_decision: ToolPolicyDecision::Prompt,
                tool_rules: Vec::new(),
                allow_network: false,
                allow_external_mutations: false,
                egress_allowlist: None,
            },
            budgets: RunBudgets {
                wall_time_ms: 60_000,
                max_iterations: 10,
                max_model_calls: 10,
                max_tool_calls: 10,
                max_input_tokens: 10_000,
                max_output_tokens: 10_000,
                max_cost_micros: None,
                max_artifact_bytes: 1_000_000,
                max_event_count: 1_000,
            },
        }
    }

    #[test]
    fn weights_outside_the_bounds_are_refused() {
        let base = temp_base("weights");
        let work = create_profile(&base, "Work").unwrap();
        for hostile in [0.0, -1.0, f64::NAN, f64::INFINITY, 1_000.0] {
            assert!(set_limits(&base, &work.id, ProfileQuota::default(), hostile).is_err());
        }
        assert!(set_limits(&base, &work.id, ProfileQuota::default(), 2.5).is_ok());
        fs::remove_dir_all(&base).unwrap();
    }
}
