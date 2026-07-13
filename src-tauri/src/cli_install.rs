//! Auto-installs the bundled `monkey-cli` sidecar onto the user's `PATH` the
//! first time the app runs, so a fresh install makes `monkey` available in a
//! terminal with no separate CLI download, installer checkbox, or manual
//! "Install shell command" click — closest in spirit to how Ollama's
//! menu-bar app self-installs its `ollama` CLI. Never elevates (no
//! sudo/osascript/pkexec admin prompt) and never edits the user's shell rc
//! files: it only ever touches locations this process can already write to
//! as the current user (a symlink in `~/.local/bin` or, if already
//! writable, `/usr/local/bin` on macOS/Linux; a copy in
//! `%LOCALAPPDATA%\Programs\monkey-cli` plus the user-scope
//! `HKCU\Environment\Path` registry value on Windows). Best-effort and
//! silent — failures are logged, never surfaced as an error dialog, and
//! never block app startup (see `lib.rs`'s `setup` hook, which spawns this
//! and ignores the result).
//!
//! [`install_if_needed`] is deliberately cache-aware rather than redoing the
//! symlink/registry work on every single launch: a small marker file (see
//! [`marker`]) records the last successful install, and a launch only
//! re-does real work when that record is missing or stale (the app's own
//! bundled path changed — an update moved the bundle). An ordinary launch
//! after the first is a single marker-file read plus one `Path::exists`
//! check, not a write attempt.
//!
//! If the bundled path is *unchanged* but the recorded install has vanished
//! (the user deleted the symlink because they don't want it), that's
//! deliberately **not** treated as damage to self-heal — silently putting
//! it back every launch would override a real user choice. `installed:
//! false` is reported instead, and reinstalling only happens through an
//! explicit action: [`cli_install_status`] (the Tauri command the Settings
//! "Automation" panel's CLI section calls for a manual reinstall/verify)
//! always bypasses the cache and does the real work, since that call is
//! user-triggered by definition.
//!
//! On top of that implicit "respect a manual delete" behavior, there's an
//! explicit on/off switch too (see [`settings`], persisted to
//! `cli_install_settings.json`, default **on**) — [`cli_install_set_enabled`]
//! is the Settings toggle's Tauri command. Turning it off doesn't just stop
//! future auto-installs, it actively [`uninstall`]s the current one
//! immediately (removing the symlink/registry entry and clearing the
//! marker), so the toggle's state and reality never drift apart. Turning it
//! back on immediately reinstalls, for the same reason.
//!
//! The `monkey-cli` binary itself is bundled via Tauri's `externalBin`
//! sidecar mechanism (see `tauri.conf.json`'s `bundle.externalBin` and
//! `scripts/stage-cli-sidecar.mjs`, which builds and stages it before every
//! `tauri dev`/`tauri build`) and lands next to the app's own executable at
//! runtime — [`bundled_cli_path`] is the only place that layout is assumed.

use std::path::{Path, PathBuf};

/// Live snapshot of the CLI's install state, returned by both
/// [`install_if_needed`] (which may serve it from the cached marker) and
/// [`cli_install_status`] (which always recomputes it for real).
#[derive(Debug, Clone, serde::Serialize)]
pub struct CliInstallStatus {
    /// The Settings toggle's current value (default `true`). When `false`,
    /// every other field is a plain "nothing's installed" placeholder — no
    /// bundled-sidecar/PATH probing is even attempted, since the user has
    /// explicitly said they don't want this.
    pub enabled: bool,
    /// Whether a bundled `monkey-cli` sidecar exists next to this running
    /// executable at all. False in an unbundled dev run with nothing
    /// staged, and deliberately never treated as an error — see the module
    /// doc.
    pub bundled: bool,
    /// Whether an install this call recognizes as "ours" (a symlink/copy of
    /// the bundled binary, with its directory present in `PATH` per the
    /// registry on Windows) is currently in place.
    pub installed: bool,
    /// Where `monkey` was installed, for display.
    pub install_path: Option<String>,
    /// Whether that location is actually on this process's `PATH` right
    /// now. Can be false even when `installed` is true (e.g. `~/.local/bin`
    /// isn't on `PATH` on a stock macOS shell) — this can only ever be
    /// discovered by asking, never fixed by editing the user's shell rc
    /// files.
    pub on_path: bool,
    pub error: Option<String>,
}

fn sidecar_file_name() -> &'static str {
    if cfg!(windows) {
        "monkey-cli.exe"
    } else {
        "monkey-cli"
    }
}

/// Locates the bundled sidecar next to the running app's own executable —
/// where Tauri's `externalBin` copies it (stripped of its target-triple
/// suffix) in a built app. `None` in an unbundled dev run with nothing
/// staged there.
pub fn bundled_cli_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let candidate = dir.join(sidecar_file_name());
    candidate.is_file().then_some(candidate)
}

fn dir_on_path(dir: &Path) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|entry| entry == dir))
        .unwrap_or(false)
}

fn status_from_install_path(installed_path: &Path) -> CliInstallStatus {
    let dir = installed_path.parent().unwrap_or(installed_path);
    CliInstallStatus {
        enabled: true,
        bundled: true,
        installed: true,
        install_path: Some(installed_path.display().to_string()),
        on_path: dir_on_path(dir),
        error: None,
    }
}

fn disabled_status() -> CliInstallStatus {
    CliInstallStatus {
        enabled: false,
        bundled: false,
        installed: false,
        install_path: None,
        on_path: false,
        error: None,
    }
}

fn not_bundled_status() -> CliInstallStatus {
    CliInstallStatus {
        enabled: true,
        bundled: false,
        installed: false,
        install_path: None,
        on_path: false,
        error: None,
    }
}

fn removed_by_user_status() -> CliInstallStatus {
    CliInstallStatus {
        enabled: true,
        bundled: true,
        installed: false,
        install_path: None,
        on_path: false,
        error: None,
    }
}

/// Called once, fire-and-forget, from `lib.rs`'s `setup` hook on every
/// launch. Bails out immediately (one small settings-file read, nothing
/// else) if the Settings toggle is off. Otherwise trusts the cached
/// [`marker`] when it's still accurate — an ordinary launch after the first
/// successful install is one marker-file read plus one `Path::exists`
/// check, never a symlink/registry write attempt. Only falls through to
/// [`force_install`] (the real work) on the first-ever launch, or when
/// `bundled_cli_path` itself has changed (an app update moved the bundle —
/// this is the only case still worth self-healing, since it's clearly not a
/// user choice).
///
/// If the bundled path is unchanged but the recorded install has vanished,
/// that's read as the user (or something else) having deliberately removed
/// it, not as damage to repair — reinstalling here would silently override
/// that choice every single launch. The status still reports
/// `installed: false` so the Settings CLI section can offer an explicit
/// "reinstall" action ([`cli_install_status`], which always bypasses this
/// cache) for a user who changes their mind.
pub fn install_if_needed() -> CliInstallStatus {
    if !settings::load().enabled {
        return disabled_status();
    }

    let Some(target) = bundled_cli_path() else {
        return not_bundled_status();
    };

    let cached = marker::read();
    let install_exists = cached
        .as_ref()
        .is_some_and(|m| Path::new(&m.install_path).exists());

    match cache_decision(cached.as_ref(), &target, install_exists) {
        CacheDecision::Hit(install_path) => status_from_install_path(&install_path),
        CacheDecision::RemovedByUser => removed_by_user_status(),
        CacheDecision::NeedsInstall => force_install(&target),
    }
}

/// What [`install_if_needed`] should do given the cached marker (if any),
/// the current bundled sidecar path, and whether the marker's recorded
/// install still exists on disk. Pure and fully unit-tested — the only
/// non-pure inputs (`marker::read`, `Path::exists`) are computed by the
/// caller and passed in, so this never has to touch a real app-data
/// directory or filesystem itself.
#[derive(Debug, PartialEq, Eq)]
enum CacheDecision {
    /// Cache is accurate and the install is still there — do nothing.
    Hit(PathBuf),
    /// Bundled path unchanged, but the recorded install is gone — read as
    /// the user removing it on purpose, not damage to repair. Do nothing
    /// (but report `installed: false`), rather than silently reinstalling.
    RemovedByUser,
    /// No marker yet, or the bundled path changed (an app update moved the
    /// bundle) — do the real install.
    NeedsInstall,
}

fn cache_decision(
    cached: Option<&marker::Marker>,
    target: &Path,
    install_exists: bool,
) -> CacheDecision {
    let Some(cached) = cached else {
        return CacheDecision::NeedsInstall;
    };
    if cached.bundled_target != target.to_string_lossy() {
        return CacheDecision::NeedsInstall;
    }
    if install_exists {
        CacheDecision::Hit(PathBuf::from(&cached.install_path))
    } else {
        CacheDecision::RemovedByUser
    }
}

/// Does the real symlink/copy-and-registry work unconditionally (no cache
/// check) and, on success, refreshes the marker so the next ordinary launch
/// can skip straight back to the cache path above.
fn force_install(target: &Path) -> CliInstallStatus {
    match platform::install(target) {
        Ok(installed_path) => {
            marker::write(&marker::Marker {
                bundled_target: target.to_string_lossy().to_string(),
                install_path: installed_path.display().to_string(),
            });
            status_from_install_path(&installed_path)
        }
        Err(e) => CliInstallStatus {
            enabled: true,
            bundled: true,
            installed: false,
            install_path: None,
            on_path: false,
            error: Some(e),
        },
    }
}

/// Removes whatever [`marker`] says is currently installed (the symlink on
/// macOS/Linux, or the copy + `PATH` registry entry on Windows) and clears
/// the marker so a later re-enable does a real reinstall rather than a
/// stale cache hit. A no-op (not an error) if there's no marker at all —
/// nothing recorded means nothing to remove.
fn uninstall() -> Result<(), String> {
    let Some(cached) = marker::read() else {
        return Ok(());
    };
    platform::uninstall(Path::new(&cached.install_path))?;
    marker::clear();
    Ok(())
}

/// Tauri command for the Settings CLI section's "reinstall"/"verify"
/// action — unlike [`install_if_needed`], this always bypasses the marker
/// cache and does the real check when enabled, since a user explicitly
/// asking to verify/repair should never be served a stale cached answer.
/// Respects the toggle: reports the plain disabled status rather than
/// reinstalling if the user has turned this off (that's what
/// [`cli_install_set_enabled`] is for).
#[tauri::command]
pub fn cli_install_status() -> CliInstallStatus {
    if !settings::load().enabled {
        return disabled_status();
    }
    let Some(target) = bundled_cli_path() else {
        return not_bundled_status();
    };
    force_install(&target)
}

/// The Settings toggle's Tauri command. Persists `enabled` and immediately
/// applies it (installs or uninstalls right away) rather than waiting for
/// the next launch, so the switch and reality never visibly disagree.
#[tauri::command]
pub fn cli_install_set_enabled(enabled: bool) -> Result<CliInstallStatus, String> {
    settings::save(&settings::Settings { enabled });

    if !enabled {
        uninstall()?;
        return Ok(disabled_status());
    }

    let Some(target) = bundled_cli_path() else {
        return Ok(not_bundled_status());
    };
    Ok(force_install(&target))
}

/// Best-effort JSON read, shared by [`marker`] and [`settings`] — `None` on
/// any error (missing file, corrupt JSON), which both callers treat as
/// "nothing recorded yet" rather than propagating an error.
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Best-effort write-then-rename JSON write, shared by [`marker`] and
/// [`settings`] — same pattern as web.rs's `web_set_settings`, so a crash
/// mid-write can never leave a torn file behind for the next read to fail
/// on. Silently gives up on any error: both callers' data is a disposable
/// cache/preference, never worth surfacing a write failure as an app error.
fn write_json_atomic<T: serde::Serialize>(path: &Path, value: &T) {
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(json) = serde_json::to_vec_pretty(value) else {
        return;
    };
    let tmp_path = path.with_extension("json.tmp");
    let Ok(mut file) = std::fs::File::create(&tmp_path) else {
        return;
    };
    use std::io::Write;
    if file.write_all(&json).is_err() {
        return;
    }
    drop(file);
    let _ = std::fs::rename(&tmp_path, path);
}

/// The on-disk cache [`install_if_needed`] consults so an ordinary launch
/// after the first doesn't redo the symlink/registry work — see the module
/// doc for why this exists at all. `read`/`write` resolve the real
/// `cli_install.json` path in the app's data dir; `read_at`/`write_at` take
/// an explicit path so tests can round-trip against a temp file instead of
/// ever touching a developer's actual app-data directory.
mod marker {
    use super::{read_json, write_json_atomic};
    use serde::{Deserialize, Serialize};
    use std::path::Path;

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    pub struct Marker {
        /// The exact [`super::bundled_cli_path`] this marker was recorded
        /// for, as a string. A mismatch (an app update moved the bundle to
        /// a new path) means the marker is stale and must not be trusted.
        pub bundled_target: String,
        pub install_path: String,
    }

    fn marker_path() -> Option<std::path::PathBuf> {
        Some(crate::app_paths::data_dir()?.join("cli_install.json"))
    }

    pub fn read() -> Option<Marker> {
        read_at(&marker_path()?)
    }

    pub(super) fn read_at(path: &Path) -> Option<Marker> {
        read_json(path)
    }

    /// Best-effort: a failure to persist the marker just means the next
    /// launch redoes the (idempotent, cheap-to-repeat) real install instead
    /// of hitting the cache — never worth surfacing as an error.
    pub fn write(marker: &Marker) {
        if let Some(path) = marker_path() {
            write_json_atomic(&path, marker);
        }
    }

    #[cfg_attr(not(test), allow(dead_code))] // only exercised by tests — production code always goes through `write`
    pub(super) fn write_at(path: &Path, marker: &Marker) {
        write_json_atomic(path, marker);
    }

    /// Removes the marker entirely — called by [`super::uninstall`] so a
    /// later re-enable does a real reinstall instead of a stale cache hit
    /// pointing at an install that no longer exists.
    pub fn clear() {
        if let Some(path) = marker_path() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The Settings toggle's persisted state — separate file from [`marker`]
/// (`cli_install_settings.json`, not `cli_install.json`) since this is a
/// user preference, not derived cache state; the two have different
/// lifecycles (this survives `uninstall`, the marker doesn't).
mod settings {
    use super::{read_json, write_json_atomic};
    use serde::{Deserialize, Serialize};
    use std::path::Path;

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    pub struct Settings {
        pub enabled: bool,
    }

    impl Default for Settings {
        fn default() -> Self {
            Settings { enabled: true }
        }
    }

    fn settings_path() -> Option<std::path::PathBuf> {
        Some(crate::app_paths::data_dir()?.join("cli_install_settings.json"))
    }

    /// Defaults to `enabled: true` (never a `bundled`/`Option` split like
    /// `marker::read`) — an unreadable/missing/corrupt settings file must
    /// never be mistaken for the user having opted out.
    pub fn load() -> Settings {
        settings_path()
            .and_then(|p| read_at(&p))
            .unwrap_or_default()
    }

    pub(super) fn read_at(path: &Path) -> Option<Settings> {
        read_json(path)
    }

    pub fn save(settings: &Settings) {
        if let Some(path) = settings_path() {
            write_json_atomic(&path, settings);
        }
    }

    #[cfg_attr(not(test), allow(dead_code))] // only exercised by tests — production code always goes through `save`
    pub(super) fn write_at(path: &Path, settings: &Settings) {
        write_json_atomic(path, settings);
    }
}

/// The symlink-into-a-writable-`PATH`-directory logic shared by the unix
/// `platform::install` (real candidate dirs) and its own unit tests
/// (isolated temp dirs) — kept OS-generic (works identically on any `unix`
/// target) so tests never have to touch a real `~/.local/bin` or
/// `/usr/local/bin`.
#[cfg(unix)]
fn symlink_into_first_writable(
    target: &Path,
    dirs: &[PathBuf],
    auto_create: &[PathBuf],
) -> Result<PathBuf, String> {
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn writable_probe(dir: &Path) -> bool {
        let probe = dir.join(format!(".monkey-cli-write-test-{}", std::process::id()));
        let ok = std::fs::File::create(&probe).is_ok();
        let _ = std::fs::remove_file(&probe);
        ok
    }

    for dir in dirs {
        if dir.exists() {
            if !writable_probe(dir) {
                continue;
            }
        } else if auto_create.contains(dir) {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            if let Ok(meta) = std::fs::metadata(dir) {
                let mut perm = meta.permissions();
                perm.set_mode(0o755);
                let _ = std::fs::set_permissions(dir, perm);
            }
        } else {
            // Not auto-created (e.g. a missing `/usr/local/bin` would need
            // root to create) — skip to the next candidate.
            continue;
        }

        let link = dir.join("monkey");
        if let Ok(existing) = std::fs::read_link(&link) {
            if existing == target {
                return Ok(link); // already correct — nothing to do
            }
            // A symlink left by a previous version of this app (e.g. after
            // an update moved the bundle) — safe to replace.
            std::fs::remove_file(&link).map_err(|e| e.to_string())?;
        } else if link.exists() {
            // A real file already lives here that isn't our symlink — never
            // clobber something the user (or another tool) put there
            // themselves.
            continue;
        }

        symlink(target, &link).map_err(|e| e.to_string())?;
        return Ok(link);
    }
    Err("no writable PATH directory found".into())
}

/// Pure string logic for the Windows `HKCU\Environment\Path` update:
/// whether `dir` is already present in `current`, and if not, what the new
/// value should be. Splits on a hardcoded `;` rather than
/// `std::env::split_paths` deliberately — that function uses the *host*
/// platform's separator (`:` on unix), which would make this always model
/// the wrong separator when unit-tested from a non-Windows machine even
/// though the logic itself is meant to be platform-independent. Kept
/// OS-generic and unit-tested on any platform for exactly that reason — the
/// actual registry read/write/broadcast only compiles on Windows and isn't
/// locally testable from here.
#[cfg_attr(not(windows), allow(dead_code))] // only called by the Windows `platform::install`; exercised by tests on every OS
fn compute_path_value_with_dir(current: &str, dir: &Path) -> Option<String> {
    let dir_str = dir.to_string_lossy();
    let already_present = current
        .split(';')
        .any(|entry| entry.trim() == dir_str.as_ref());
    if already_present {
        return None;
    }
    Some(if current.trim().is_empty() {
        dir_str.to_string()
    } else if current.trim_end().ends_with(';') {
        format!("{current}{dir_str}")
    } else {
        format!("{current};{dir_str}")
    })
}

/// The `HKCU\Environment\Path` counterpart to
/// [`compute_path_value_with_dir`], for [`uninstall`] on Windows: drops
/// every `;`-separated segment equal to `dir` (normally exactly one) and
/// rejoins the rest. Returns `None` when `dir` wasn't present at all, so
/// the caller can skip a pointless registry write — same "no-op when
/// nothing changed" contract as its append counterpart. Kept OS-generic and
/// unit-tested on any platform for the same reason: splitting on a
/// hardcoded `;` rather than `std::env::split_paths` models the real
/// Windows separator regardless of which OS runs the test.
#[cfg_attr(not(windows), allow(dead_code))] // only called by the Windows `platform::uninstall`; exercised by tests on every OS
fn remove_dir_from_path_value(current: &str, dir: &Path) -> Option<String> {
    let dir_str = dir.to_string_lossy();
    let mut changed = false;
    let kept: Vec<&str> = current
        .split(';')
        .filter(|entry| {
            let matches = entry.trim() == dir_str.as_ref();
            changed |= matches;
            !matches
        })
        .collect();
    changed.then(|| kept.join(";"))
}

#[cfg(unix)]
mod platform {
    use super::symlink_into_first_writable;
    use std::path::{Path, PathBuf};

    /// `/usr/local/bin` first (macOS only — it's on the default `PATH` per
    /// `/etc/paths` with no shell rc changes needed, matching where
    /// Ollama/Homebrew both install) if already writable as the current
    /// user; `~/.local/bin` otherwise (created if missing), since it's
    /// always user-writable and Debian/Ubuntu's default `.profile` already
    /// adds it to `PATH` automatically. Deliberately never falls back to a
    /// `sudo`/`pkexec`/`osascript`-elevated write — see the module doc.
    fn candidate_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if cfg!(target_os = "macos") {
            dirs.push(PathBuf::from("/usr/local/bin"));
        }
        if let Some(home) = dirs::home_dir() {
            dirs.push(home.join(".local").join("bin"));
        }
        dirs
    }

    pub fn install(target: &Path) -> Result<PathBuf, String> {
        let dirs = candidate_dirs();
        // Only the user-owned candidate(s) — never `/usr/local/bin` — are
        // auto-created if missing; see `candidate_dirs`'s doc comment.
        let auto_create: Vec<PathBuf> = dirs
            .iter()
            .filter(|d| *d != &PathBuf::from("/usr/local/bin"))
            .cloned()
            .collect();
        symlink_into_first_writable(target, &dirs, &auto_create)
    }

    /// Removes `install_path` only if it's still a symlink (never a real
    /// file — same "never clobber something the user or another tool put
    /// there" rule `symlink_into_first_writable` follows on install).
    /// Already-gone is success, not an error: uninstalling something
    /// that's already uninstalled is a no-op, not a failure.
    pub fn uninstall(install_path: &Path) -> Result<(), String> {
        match std::fs::symlink_metadata(install_path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                std::fs::remove_file(install_path).map_err(|e| e.to_string())
            }
            Ok(_) => Ok(()), // a real file lives here, not ours — leave it alone
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{compute_path_value_with_dir, remove_dir_from_path_value};
    use std::path::{Path, PathBuf};
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
    use winreg::RegKey;

    fn open_user_environment_key() -> Result<RegKey, String> {
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags("Environment", KEY_READ | KEY_WRITE)
            .map_err(|e| e.to_string())
    }

    /// Windows has no unprivileged, always-reliable equivalent of a Unix
    /// symlink (`CreateSymbolicLink` needs Developer Mode or admin), and the
    /// app's own install directory isn't guaranteed writable at runtime (a
    /// per-machine NSIS/MSI install lands in `Program Files`). So instead of
    /// linking in place, this copies the sidecar into a directory this
    /// process can always write regardless of install mode —
    /// `%LOCALAPPDATA%\Programs\monkey-cli\monkey.exe` — and adds that
    /// directory (never the app's own) to `PATH`.
    fn shim_dir() -> Result<PathBuf, String> {
        let base =
            dirs::data_local_dir().ok_or_else(|| "could not resolve %LOCALAPPDATA%".to_string())?;
        Ok(base.join("Programs").join("monkey-cli"))
    }

    fn needs_copy(target: &Path, dest: &Path) -> bool {
        let (Ok(src_meta), Ok(dst_meta)) = (std::fs::metadata(target), std::fs::metadata(dest))
        else {
            return true;
        };
        src_meta.len() != dst_meta.len()
    }

    pub fn install(target: &Path) -> Result<PathBuf, String> {
        let dir = shim_dir()?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let dest = dir.join("monkey.exe");
        if needs_copy(target, &dest) {
            std::fs::copy(target, &dest).map_err(|e| e.to_string())?;
        }

        let env = open_user_environment_key()?;
        let current: String = env.get_value("Path").unwrap_or_default();
        if let Some(new_value) = compute_path_value_with_dir(&current, &dir) {
            env.set_value("Path", &new_value)
                .map_err(|e| e.to_string())?;
            broadcast_environment_change();
        }

        Ok(dest)
    }

    /// Removes `monkey.exe` (best-effort — an already-missing file is
    /// success, not an error) and drops its directory from `PATH` if
    /// present. `install_path` is the `monkey.exe` path itself, matching
    /// what `install` returned and what the marker recorded — its parent is
    /// the directory `PATH` needs cleared.
    pub fn uninstall(install_path: &Path) -> Result<(), String> {
        match std::fs::remove_file(install_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }

        let Some(dir) = install_path.parent() else {
            return Ok(());
        };
        let env = open_user_environment_key()?;
        let current: String = env.get_value("Path").unwrap_or_default();
        if let Some(new_value) = remove_dir_from_path_value(&current, dir) {
            env.set_value("Path", &new_value)
                .map_err(|e| e.to_string())?;
            broadcast_environment_change();
        }
        Ok(())
    }

    /// Tells already-open shells/Explorer the environment changed, so a
    /// freshly opened terminal picks up the new `PATH` immediately instead
    /// of only after the next login — the same broadcast Windows installers
    /// send after modifying `HKCU\Environment`. Best-effort: a failure here
    /// still leaves the registry change in place for the next login.
    fn broadcast_environment_change() {
        use windows_sys::Win32::Foundation::{LPARAM, WPARAM};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
        };

        let param: Vec<u16> = "Environment\0".encode_utf16().collect();
        let mut result: usize = 0;
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0 as WPARAM,
                param.as_ptr() as LPARAM,
                SMTO_ABORTIFHUNG,
                5000,
                &mut result as *mut usize,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors checkpoints.rs's own `TempDir` test helper: a real directory
    /// under the OS temp dir, removed on drop, so filesystem tests never
    /// touch a developer's actual `~/.local/bin`/`/usr/local/bin`.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "little-monkey-cli-install-test-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn sidecar_file_name_matches_platform() {
        assert_eq!(sidecar_file_name().ends_with(".exe"), cfg!(windows));
    }

    fn fake_marker(target: &str, install: &str) -> marker::Marker {
        marker::Marker {
            bundled_target: target.into(),
            install_path: install.into(),
        }
    }

    #[test]
    fn cache_decision_needs_install_when_no_marker_yet() {
        let target = Path::new("/Applications/Little Monkey.app/monkey-cli");
        assert_eq!(
            cache_decision(None, target, false),
            CacheDecision::NeedsInstall
        );
    }

    #[test]
    fn cache_decision_is_a_hit_when_target_matches_and_install_exists() {
        let target = Path::new("/Applications/Little Monkey.app/monkey-cli");
        let cached = fake_marker(&target.to_string_lossy(), "/Users/x/.local/bin/monkey");
        assert_eq!(
            cache_decision(Some(&cached), target, true),
            CacheDecision::Hit(PathBuf::from("/Users/x/.local/bin/monkey"))
        );
    }

    /// The exact scenario a user asked about directly: they removed the CLI
    /// after the first launch and don't want it back. The app must not
    /// silently reinstall it just because it noticed the symlink is gone.
    #[test]
    fn cache_decision_respects_a_user_deleted_install_instead_of_reinstalling() {
        let target = Path::new("/Applications/Little Monkey.app/monkey-cli");
        let cached = fake_marker(&target.to_string_lossy(), "/Users/x/.local/bin/monkey");
        assert_eq!(
            cache_decision(Some(&cached), target, false),
            CacheDecision::RemovedByUser
        );
    }

    #[test]
    fn cache_decision_self_heals_when_the_app_bundle_moved() {
        let old_target = Path::new("/Applications/Little Monkey.app/monkey-cli");
        let new_target = Path::new("/Applications/Little Monkey 2.app/monkey-cli");
        let cached = fake_marker(&old_target.to_string_lossy(), "/Users/x/.local/bin/monkey");
        // Even though the old install still exists on disk (`true`), a
        // changed bundled path means the marker is stale and must trigger
        // a real reinstall pointing at the new target — this is the one
        // case still worth self-healing.
        assert_eq!(
            cache_decision(Some(&cached), new_target, true),
            CacheDecision::NeedsInstall
        );
    }

    #[test]
    fn marker_round_trips_through_read_write_at() {
        let dir = TempDir::new("marker-roundtrip");
        let path = dir.path.join("cli_install.json");
        assert!(marker::read_at(&path).is_none(), "nothing written yet");

        let written = marker::Marker {
            bundled_target: "/Applications/Little Monkey.app/Contents/MacOS/monkey-cli".into(),
            install_path: "/Users/x/.local/bin/monkey".into(),
        };
        marker::write_at(&path, &written);

        let read_back = marker::read_at(&path).expect("just-written marker should read back");
        assert_eq!(read_back, written);
    }

    #[test]
    fn marker_write_at_creates_missing_parent_dirs() {
        let dir = TempDir::new("marker-nested");
        let path = dir
            .path
            .join("nested")
            .join("dirs")
            .join("cli_install.json");
        marker::write_at(
            &path,
            &marker::Marker {
                bundled_target: "target".into(),
                install_path: "install".into(),
            },
        );
        assert!(path.exists());
    }

    #[test]
    fn marker_write_at_is_atomic_via_rename_never_leaving_a_tmp_file_on_success() {
        let dir = TempDir::new("marker-atomic");
        let path = dir.path.join("cli_install.json");
        marker::write_at(
            &path,
            &marker::Marker {
                bundled_target: "t".into(),
                install_path: "i".into(),
            },
        );
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn marker_read_at_returns_none_for_corrupt_json() {
        let dir = TempDir::new("marker-corrupt");
        let path = dir.path.join("cli_install.json");
        std::fs::write(&path, b"not json").unwrap();
        assert!(marker::read_at(&path).is_none());
    }

    #[test]
    fn settings_defaults_to_enabled_when_nothing_written_yet() {
        let dir = TempDir::new("settings-default");
        let path = dir.path.join("cli_install_settings.json");
        assert_eq!(settings::read_at(&path), None);
        // `load()`'s own default (used when `read_at` returns `None`) is
        // what actually matters for `install_if_needed` — pinned directly
        // since `load()` always resolves the real app-data path and can't
        // be pointed at a temp dir.
        assert!(settings::Settings::default().enabled);
    }

    #[test]
    fn settings_round_trips_through_read_write_at() {
        let dir = TempDir::new("settings-roundtrip");
        let path = dir.path.join("cli_install_settings.json");
        settings::write_at(&path, &settings::Settings { enabled: false });
        assert_eq!(
            settings::read_at(&path),
            Some(settings::Settings { enabled: false })
        );
    }

    #[test]
    fn settings_read_at_returns_none_for_corrupt_json() {
        let dir = TempDir::new("settings-corrupt");
        let path = dir.path.join("cli_install_settings.json");
        std::fs::write(&path, b"not json").unwrap();
        assert!(settings::read_at(&path).is_none());
    }

    #[test]
    fn disabled_status_has_every_field_at_its_placeholder_value() {
        let status = disabled_status();
        assert!(!status.enabled);
        assert!(!status.bundled);
        assert!(!status.installed);
        assert_eq!(status.install_path, None);
        assert!(!status.on_path);
        assert_eq!(status.error, None);
    }

    #[test]
    fn path_value_removal_drops_a_present_dir() {
        let dir = std::path::Path::new(r"C:\Users\me\AppData\Local\Programs\monkey-cli");
        let current = r"C:\Windows\system32;C:\Users\me\AppData\Local\Programs\monkey-cli;C:\bin";
        assert_eq!(
            remove_dir_from_path_value(current, dir),
            Some(r"C:\Windows\system32;C:\bin".to_string())
        );
    }

    #[test]
    fn path_value_removal_is_none_when_dir_absent() {
        let dir = std::path::Path::new(r"C:\Users\me\AppData\Local\Programs\monkey-cli");
        let current = r"C:\Windows\system32;C:\bin";
        assert_eq!(remove_dir_from_path_value(current, dir), None);
    }

    #[cfg(unix)]
    #[test]
    fn unix_platform_uninstall_removes_a_symlink_it_created() {
        let target = TempDir::new("uninstall-target");
        let bin = target.path.join("monkey-cli");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        let dir = TempDir::new("uninstall-dir");

        let link = symlink_into_first_writable(&bin, &[dir.path.clone()], &[]).unwrap();
        assert!(link.exists());
        platform::uninstall(&link).unwrap();
        assert!(!link.exists());
        // Idempotent: uninstalling an already-gone symlink is success, not
        // an error — same "no-op, not a failure" contract `install_if_needed`
        // relies on when the user removed it manually before ever calling
        // `uninstall` themselves.
        platform::uninstall(&link).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unix_platform_uninstall_never_removes_a_real_file() {
        let dir = TempDir::new("uninstall-real-file");
        let real_file = dir.path.join("monkey");
        std::fs::write(&real_file, b"not ours").unwrap();

        platform::uninstall(&real_file).unwrap();
        assert!(
            real_file.exists(),
            "a real (non-symlink) file must survive uninstall"
        );
    }

    #[test]
    fn bundled_cli_path_is_none_when_nothing_staged() {
        // `cargo test`'s own test binary never has a `monkey-cli[.exe]`
        // sitting next to it, so this just pins the "unbundled dev run"
        // no-op path the module doc promises.
        assert!(bundled_cli_path().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_into_first_existing_writable_dir() {
        let target = TempDir::new("target");
        let bin = target.path.join("monkey-cli");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();

        let missing = TempDir::new("missing-parent").path.join("does-not-exist");
        let writable = TempDir::new("writable");

        let link =
            symlink_into_first_writable(&bin, &[missing.clone(), writable.path.clone()], &[])
                .expect("should skip the missing dir and use the writable one");
        assert_eq!(link, writable.path.join("monkey"));
        assert_eq!(std::fs::read_link(&link).unwrap(), bin);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_install_is_idempotent() {
        let target = TempDir::new("target2");
        let bin = target.path.join("monkey-cli");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        let dir = TempDir::new("idempotent");

        let first = symlink_into_first_writable(&bin, &[dir.path.clone()], &[]).unwrap();
        let second = symlink_into_first_writable(&bin, &[dir.path.clone()], &[]).unwrap();
        assert_eq!(first, second);
        assert_eq!(std::fs::read_link(&second).unwrap(), bin);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_install_never_clobbers_a_real_file() {
        let target = TempDir::new("target3");
        let bin = target.path.join("monkey-cli");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        let dir = TempDir::new("occupied");
        let real_file = dir.path.join("monkey");
        std::fs::write(&real_file, b"not ours").unwrap();

        let err = symlink_into_first_writable(&bin, &[dir.path.clone()], &[])
            .expect_err("the only candidate dir has a real (non-symlink) `monkey` already");
        assert!(err.contains("no writable"));
        assert_eq!(std::fs::read(&real_file).unwrap(), b"not ours");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_install_auto_creates_only_listed_dirs() {
        let target = TempDir::new("target4");
        let bin = target.path.join("monkey-cli");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        let parent = TempDir::new("auto-create-parent");
        let not_created = parent.path.join("should-not-exist");
        let created = parent.path.join("should-exist");

        let link = symlink_into_first_writable(
            &bin,
            &[not_created.clone(), created.clone()],
            &[created.clone()],
        )
        .unwrap();
        assert!(!not_created.exists());
        assert_eq!(link, created.join("monkey"));
    }

    #[test]
    fn path_value_returns_none_when_dir_already_present() {
        let dir = std::path::Path::new(r"C:\Users\me\AppData\Local\Programs\monkey-cli");
        let current = r"C:\Windows\system32;C:\Users\me\AppData\Local\Programs\monkey-cli;C:\bin";
        assert_eq!(compute_path_value_with_dir(current, dir), None);
    }

    #[test]
    fn path_value_appends_with_separator() {
        let dir = std::path::Path::new("/usr/local/bin");
        assert_eq!(
            compute_path_value_with_dir("/usr/bin", dir),
            Some("/usr/bin;/usr/local/bin".to_string())
        );
    }

    #[test]
    fn path_value_handles_empty_current() {
        let dir = std::path::Path::new("/usr/local/bin");
        assert_eq!(
            compute_path_value_with_dir("", dir),
            Some("/usr/local/bin".to_string())
        );
    }

    #[test]
    fn path_value_handles_trailing_separator() {
        let dir = std::path::Path::new("/usr/local/bin");
        assert_eq!(
            compute_path_value_with_dir("/usr/bin;", dir),
            Some("/usr/bin;/usr/local/bin".to_string())
        );
    }
}
