//! Single source of truth for the app's on-disk data-directory identifier.
//!
//! The desktop app resolves its app-data directory through a Tauri
//! `AppHandle` (`app.path().app_data_dir()`), which reads `tauri.conf.json`'s
//! `identifier`. `monkey-cli` has no `AppHandle` and, before this module
//! existed, independently hardcoded the exact same identifier string in
//! eight separate files (`providers_cli.rs`, `checkpoints_cli.rs`,
//! `verify_cli.rs`, `web_cli.rs`, `tools_cli.rs`, `mcp_cli.rs`,
//! `stacks_cli.rs`, `main.rs`) — a drift risk ROADMAP.md §3.5 names
//! explicitly. This is the shared, `AppHandle`-free replacement: every one of
//! those eight call sites now resolves the shared prefix through
//! [`data_dir`] and only appends its own file/subdirectory name.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Must match `identifier` in `src-tauri/tauri.conf.json`.
const APP_IDENTIFIER: &str = "com.littlemonkey.app";

/// Optional override for the user-authored agent/CLI home.
pub const AGENT_HOME_ENV: &str = "LITTLE_MONKEY_HOME";

const AGENT_HOME_DIR: &str = ".littlemonkey";

/// One profile snapshot spanning both authored and managed configuration.
///
/// Keeping the pair together prevents a concurrent profile switch from
/// resolving the home side for one profile and the legacy side for another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentConfigRoots {
    pub profile_id: String,
    pub registry_active_id: String,
    pub agent_home: PathBuf,
    pub authored: PathBuf,
    pub legacy: PathBuf,
}

impl AgentConfigRoots {
    pub fn effective_path(&self, relative: impl AsRef<Path>) -> Result<PathBuf, String> {
        let relative = validate_relative_path(relative.as_ref())?;
        Ok(preferred_path(
            &self.authored.join(relative),
            &self.legacy.join(relative),
        ))
    }

    pub fn effective_path_with_sibling(
        &self,
        primary: impl AsRef<Path>,
        sibling: impl AsRef<Path>,
    ) -> Result<PathBuf, String> {
        let primary = validate_relative_path(primary.as_ref())?;
        let sibling = validate_relative_path(sibling.as_ref())?;
        let candidates = [primary, sibling];
        Ok(preferred_root_for_candidates(&self.authored, &self.legacy, &candidates).join(primary))
    }

    pub fn ordered(&self) -> Vec<PathBuf> {
        if self.authored == self.legacy {
            vec![self.authored.clone()]
        } else {
            vec![self.authored.clone(), self.legacy.clone()]
        }
    }
}

/// The base app-data directory (e.g.
/// `~/Library/Application Support/com.littlemonkey.app` on macOS,
/// `~/.local/share/com.littlemonkey.app` on Linux), *before* the active
/// profile is applied.
///
/// Only the profile registry itself lives here — it is the file that decides
/// which profile root [`data_dir`] returns, so it cannot itself live inside
/// one. Everything else wants [`data_dir`].
pub fn base_data_dir() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join(APP_IDENTIFIER))
}

/// The active profile's data directory (K23). Callers join their own file or
/// subdirectory name onto this and create any subdirectory themselves — this
/// function only resolves the shared prefix, exactly like every one of the
/// eight call sites it replaces did individually before.
///
/// For the default profile this is [`base_data_dir`] unchanged, so an
/// installation that predates profiles keeps every path it had.
pub fn data_dir() -> Option<PathBuf> {
    let base = base_data_dir()?;
    match crate::profiles::active_root(&base) {
        Ok(root) => Some(root),
        Err(error) => {
            eprintln!("little-monkey: could not resolve the active profile: {error}");
            None
        }
    }
}

/// The portable, user-authored agent/CLI home.
///
/// Managed application state stays under [`data_dir`]. This directory is for
/// files users may reasonably edit, sync, or keep in dotfiles. An explicit
/// [`AGENT_HOME_ENV`] must be absolute so a GUI and a CLI launched from
/// different working directories cannot silently resolve different homes.
pub fn agent_home_dir() -> Result<PathBuf, String> {
    resolve_agent_home(
        std::env::var_os(AGENT_HOME_ENV).as_deref(),
        dirs::home_dir(),
    )
}

/// The active profile's authored configuration root.
///
/// The default profile uses `~/.littlemonkey` directly. Named profiles use
/// `~/.littlemonkey/profiles/<id>`, preserving the same isolation boundary as
/// [`data_dir`] without moving managed state out of the OS data directory.
pub fn agent_config_dir() -> Result<PathBuf, String> {
    Ok(agent_config_roots()?.authored)
}

/// Resolves authored and legacy roots from one active-profile read.
pub fn agent_config_roots() -> Result<AgentConfigRoots, String> {
    let home = agent_home_dir()?;
    let data_base = base_data_dir()
        .ok_or_else(|| "Could not resolve the Little Monkey app data directory".to_string())?;
    let registry = crate::profiles::load_registry(&data_base).map_err(|error| error.to_string())?;
    let profile_id = crate::profiles::selected_id(&registry).map_err(|error| error.to_string())?;
    Ok(agent_config_roots_for(
        &home,
        &data_base,
        &profile_id,
        &registry.active_id,
    ))
}

/// The profile id shared by managed app-data and authored-home resolution.
pub fn active_profile_id() -> Result<String, String> {
    let data_base = base_data_dir()
        .ok_or_else(|| "Could not resolve the Little Monkey app data directory".to_string())?;
    crate::profiles::active_id(&data_base).map_err(|error| error.to_string())
}

/// Creates the active authored configuration root with private permissions on
/// Unix. The default root is hardened automatically; an existing explicit
/// override is left untouched but must already have mode `0700`.
pub fn ensure_agent_config_dir() -> Result<PathBuf, String> {
    Ok(ensure_agent_config_roots()?.authored)
}

/// Creates the authored root while preserving the same profile snapshot for
/// callers that also need its legacy managed root.
pub fn ensure_agent_config_roots() -> Result<AgentConfigRoots, String> {
    let override_path = std::env::var_os(AGENT_HOME_ENV).filter(|value| !value.is_empty());
    let home = resolve_agent_home(override_path.as_deref(), dirs::home_dir())?;
    ensure_private_directory(&home, override_path.is_none())?;

    let data_base = base_data_dir()
        .ok_or_else(|| "Could not resolve the Little Monkey app data directory".to_string())?;
    let registry = crate::profiles::load_registry(&data_base).map_err(|error| error.to_string())?;
    let profile_id = crate::profiles::selected_id(&registry).map_err(|error| error.to_string())?;
    let roots = agent_config_roots_for(&home, &data_base, &profile_id, &registry.active_id);
    if roots.authored != home {
        ensure_private_directory(&home.join(crate::profiles::PROFILES_DIR), true)?;
        ensure_private_directory(&roots.authored, true)?;
    }
    Ok(roots)
}

/// Resolves a path below the active authored configuration root without
/// creating it.
pub fn agent_config_path(relative: impl AsRef<Path>) -> Result<PathBuf, String> {
    let relative = validate_relative_path(relative.as_ref())?;
    Ok(agent_config_roots()?.authored.join(relative))
}

/// Chooses the authored-home path when present, otherwise an existing legacy
/// app-data path. When neither exists, new writes target the authored home.
/// Legacy data is never moved or deleted implicitly.
pub fn effective_agent_config_path(relative: impl AsRef<Path>) -> Result<PathBuf, String> {
    agent_config_roots()?.effective_path(relative)
}

/// Like [`effective_agent_config_path`], but treats a sibling fallback as part
/// of the same logical setting. This keeps `MONKEY.md` and its `AGENTS.md`
/// fallback on one root instead of accidentally splitting them across home
/// and legacy app data.
pub fn effective_agent_config_path_with_sibling(
    primary: impl AsRef<Path>,
    sibling: impl AsRef<Path>,
) -> Result<PathBuf, String> {
    agent_config_roots()?.effective_path_with_sibling(primary, sibling)
}

fn resolve_agent_home(
    override_path: Option<&OsStr>,
    home_dir: Option<PathBuf>,
) -> Result<PathBuf, String> {
    let path = match override_path.filter(|value| !value.is_empty()) {
        Some(value) => PathBuf::from(value),
        None => home_dir
            .ok_or_else(|| "Could not resolve the user home directory".to_string())?
            .join(AGENT_HOME_DIR),
    };
    if !path.is_absolute() {
        return Err(format!(
            "{AGENT_HOME_ENV} must be an absolute path (got '{}')",
            path.display()
        ));
    }
    Ok(path)
}

fn agent_config_dir_for(home: &Path, profile_id: &str) -> PathBuf {
    if profile_id == crate::profiles::DEFAULT_PROFILE_ID {
        home.to_path_buf()
    } else {
        home.join(crate::profiles::PROFILES_DIR).join(profile_id)
    }
}

fn agent_config_roots_for(
    home: &Path,
    data_base: &Path,
    profile_id: &str,
    registry_active_id: &str,
) -> AgentConfigRoots {
    AgentConfigRoots {
        profile_id: profile_id.to_string(),
        registry_active_id: registry_active_id.to_string(),
        agent_home: home.to_path_buf(),
        authored: agent_config_dir_for(home, profile_id),
        legacy: crate::profiles::profile_root(data_base, profile_id),
    }
}

fn validate_relative_path(path: &Path) -> Result<&Path, String> {
    if path.as_os_str().is_empty()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "Agent configuration path '{}' must be relative and cannot contain traversal",
            path.display()
        ));
    }
    Ok(path)
}

fn preferred_path(preferred: &Path, legacy: &Path) -> PathBuf {
    match preferred.try_exists() {
        Ok(true) | Err(_) => preferred.to_path_buf(),
        Ok(false) => match legacy.try_exists() {
            Ok(false) => preferred.to_path_buf(),
            Ok(true) | Err(_) => legacy.to_path_buf(),
        },
    }
}

fn root_has_any(root: &Path, candidates: &[&Path]) -> bool {
    candidates
        .iter()
        .any(|candidate| root.join(candidate).try_exists().unwrap_or(true))
}

fn preferred_root_for_candidates(preferred: &Path, legacy: &Path, candidates: &[&Path]) -> PathBuf {
    if root_has_any(preferred, candidates) {
        preferred.to_path_buf()
    } else if root_has_any(legacy, candidates) {
        legacy.to_path_buf()
    } else {
        preferred.to_path_buf()
    }
}

fn ensure_private_directory(path: &Path, harden_existing: bool) -> Result<(), String> {
    let created = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(format!(
                "Little Monkey agent home '{}' must be a real directory",
                path.display()
            ))
        }
        Ok(_) => false,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .map_err(|error| format!("Could not create '{}': {error}", path.display()))?;
            let metadata = fs::symlink_metadata(path)
                .map_err(|error| format!("Could not inspect '{}': {error}", path.display()))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!(
                    "Little Monkey agent home '{}' must be a real directory",
                    path.display()
                ));
            }
            true
        }
        Err(error) => {
            return Err(format!("Could not inspect '{}': {error}", path.display()));
        }
    };

    #[cfg(not(unix))]
    let _ = (created, harden_existing);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if created || harden_existing {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("Could not protect '{}': {error}", path.display()))?;
        } else {
            let mode = fs::metadata(path)
                .map_err(|error| format!("Could not inspect '{}': {error}", path.display()))?
                .permissions()
                .mode()
                & 0o777;
            if mode != 0o700 {
                return Err(format!(
                    "Existing {AGENT_HOME_ENV} directory '{}' must have mode 0700 (found {mode:04o})",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn base_data_dir_ends_with_the_app_identifier() {
        let dir = base_data_dir().expect("dirs::data_dir() should resolve on any supported OS");
        assert_eq!(
            dir.file_name().and_then(|n| n.to_str()),
            Some(APP_IDENTIFIER)
        );
    }

    /// The default profile resolves to the base directory itself, which is what
    /// keeps an installation that predates K23 on exactly the paths it had.
    #[test]
    fn the_default_profile_resolves_to_the_base_directory() {
        let base = base_data_dir().expect("a base directory");
        assert_eq!(
            crate::profiles::profile_root(&base, crate::profiles::DEFAULT_PROFILE_ID),
            base
        );
    }

    #[test]
    fn agent_home_prefers_an_absolute_override() {
        let override_path = std::env::temp_dir().join("little-monkey-home");
        assert_eq!(
            resolve_agent_home(
                Some(override_path.as_os_str()),
                Some(std::env::temp_dir().join("example-home")),
            )
            .unwrap(),
            override_path
        );
    }

    #[test]
    fn empty_agent_home_override_uses_the_default_dot_directory() {
        let home = std::env::temp_dir().join("example-home");
        assert_eq!(
            resolve_agent_home(Some(OsStr::new("")), Some(home.clone())).unwrap(),
            home.join(".littlemonkey")
        );
    }

    #[test]
    fn relative_agent_home_override_is_rejected() {
        let error = resolve_agent_home(
            Some(OsStr::new("relative/home")),
            Some(PathBuf::from("/home/example")),
        )
        .unwrap_err();
        assert!(error.contains(AGENT_HOME_ENV));
        assert!(error.contains("absolute"));
    }

    #[test]
    fn named_profiles_have_separate_agent_config_roots() {
        let home = Path::new("/home/example/.littlemonkey");
        assert_eq!(
            agent_config_dir_for(home, crate::profiles::DEFAULT_PROFILE_ID),
            home
        );
        assert_eq!(
            agent_config_dir_for(home, "work"),
            home.join("profiles/work")
        );
    }

    #[test]
    fn one_profile_snapshot_drives_both_configuration_roots() {
        let home = Path::new("/home/example/.littlemonkey");
        let data = Path::new("/var/lib/littlemonkey");
        let roots = agent_config_roots_for(home, data, "work", "default");

        assert_eq!(roots.profile_id, "work");
        assert_eq!(roots.registry_active_id, "default");
        assert_eq!(roots.agent_home, home);
        assert_eq!(roots.authored, home.join("profiles/work"));
        assert_eq!(roots.legacy, data.join("profiles/work"));
        assert_eq!(roots.ordered(), vec![roots.authored, roots.legacy]);
    }

    #[test]
    fn authored_path_wins_and_legacy_is_only_a_fallback() {
        let root = TempDir::new();
        let preferred = root.path.join("home/MONKEY.md");
        let legacy = root.path.join("data/MONKEY.md");

        assert_eq!(preferred_path(&preferred, &legacy), preferred);
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "legacy").unwrap();
        assert_eq!(preferred_path(&preferred, &legacy), legacy);
        fs::create_dir_all(preferred.parent().unwrap()).unwrap();
        fs::write(&preferred, "preferred").unwrap();
        assert_eq!(preferred_path(&preferred, &legacy), preferred);
    }

    #[test]
    fn a_legacy_agents_fallback_keeps_the_whole_rules_scope_on_legacy_root() {
        let root = TempDir::new();
        let preferred = root.path.join("home");
        let legacy = root.path.join("data");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("AGENTS.md"), "legacy rules").unwrap();
        let candidates = [Path::new("MONKEY.md"), Path::new("AGENTS.md")];

        assert_eq!(
            preferred_root_for_candidates(&preferred, &legacy, &candidates),
            legacy
        );

        fs::create_dir_all(&preferred).unwrap();
        fs::write(preferred.join("MONKEY.md"), "new rules").unwrap();
        assert_eq!(
            preferred_root_for_candidates(&preferred, &legacy, &candidates),
            preferred
        );
    }

    #[test]
    fn agent_configuration_paths_cannot_escape_the_home() {
        assert!(validate_relative_path(Path::new("../MONKEY.md")).is_err());
        assert!(validate_relative_path(Path::new("/tmp/MONKEY.md")).is_err());
        assert!(validate_relative_path(Path::new("MONKEY.md")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn created_agent_home_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new();
        let home = root.path.join("nested/.littlemonkey");
        ensure_private_directory(&home, false).unwrap();
        assert_eq!(
            fs::metadata(home).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_non_private_override_is_rejected_not_rewritten() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new();
        let home = root.path.join("existing");
        fs::create_dir(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
        let error = ensure_private_directory(&home, false).unwrap_err();
        assert!(error.contains("0700"));
        assert_eq!(
            fs::metadata(home).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_writable_override_is_rejected_not_rewritten() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new();
        let home = root.path.join("writable-override");
        fs::create_dir(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o777)).unwrap();
        let error = ensure_private_directory(&home, false).unwrap_err();
        assert!(error.contains("0700"));
        assert_eq!(
            fs::metadata(home).unwrap().permissions().mode() & 0o777,
            0o777
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_default_agent_home_is_hardened() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::new();
        let home = root.path.join("existing-default");
        fs::create_dir(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_directory(&home, true).unwrap();
        assert_eq!(
            fs::metadata(home).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let sequence = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "little_monkey_app_paths_test_{}_{}",
                std::process::id(),
                sequence
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
