//! Global Command Palette (ROADMAP.md, Phase 1): a Raycast-style command
//! surface reachable from anywhere via one OS-level global shortcut. This
//! module owns only the shortcut's persisted configuration and the "bring
//! the palette to the front" action — see `lib.rs::run`, which registers
//! this shortcut alongside the desktop companion's own overlay shortcut on
//! the *same* `tauri_plugin_global_shortcut::Builder` (a Tauri app manages
//! exactly one instance of each plugin, so both shortcuts share one
//! registration and one dispatching handler).
//!
//! Deliberately, this module does NOT own a second execution path for
//! summarizing, rewriting, translating, asking the model, starting a
//! workflow, searching knowledge, creating a task, or approving a pending
//! action — the frontend (`src/components/Palette/CommandPalette.tsx`)
//! dispatches every one of those through the exact same Tauri commands chat
//! and the rest of the app already use (`runAgentTurn`, `runRecipeNow`,
//! `knowledge_v2_query`, `permission_respond`, `recipes_save`, ...), so the
//! palette inherits the same permission/approval gating and run-ledger
//! evidence those already have, rather than a parallel implementation that
//! could silently drift out of sync with it.
//!
//! Unlike the companion overlay (a separate always-on-top webview window),
//! the palette renders *inside* the main window as an ordinary overlay
//! component. That's what lets it reuse the companion's own capture-grant
//! commands (`m7_capture_text`/`m7_capture_file`/`m7_capture_screen`) for
//! its "clipboard / selected file / screenshot" context capture without any
//! Rust changes: those commands are ungated by window label, and the grant
//! commands they depend on (`m7_capture_grant`/`m7_capture_revoke`) already
//! allow the `"main"` window (see `m7_companion::ensure_companion_control_window`).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};
use tauri_plugin_global_shortcut::GlobalShortcutExt;
use uuid::Uuid;

const CONFIG_SCHEMA_VERSION: u32 = 1;
const CONFIG_FILE: &str = "command-palette-config-v1.json";
/// Chosen to avoid colliding with the OS ("Spotlight" et al. typically claim
/// Cmd+Space), the companion overlay's own default (Cmd+Shift+Space), and
/// every in-window accelerator already registered in `shortcuts.ts`.
pub const DEFAULT_SHORTCUT: &str = "CommandOrControl+Shift+K";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PaletteConfig {
    pub schema_version: u32,
    pub shortcut: String,
}

impl Default for PaletteConfig {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            shortcut: DEFAULT_SHORTCUT.to_string(),
        }
    }
}

pub struct CommandPaletteState {
    root: PathBuf,
    config: Mutex<PaletteConfig>,
}

impl CommandPaletteState {
    pub fn production(app_data_dir: &Path) -> Result<Self, String> {
        let root = app_data_dir.join("command-palette-v1");
        ensure_private_directory(&root)?;
        let config = load_json::<PaletteConfig>(&root.join(CONFIG_FILE))?.unwrap_or_default();
        validate_config(&config)?;
        Ok(Self {
            root,
            config: Mutex::new(config),
        })
    }

    fn config(&self) -> Result<PaletteConfig, String> {
        Ok(lock(&self.config, "command palette config")?.clone())
    }

    pub fn shortcut(&self) -> Result<String, String> {
        Ok(self.config()?.shortcut)
    }

    fn save_config(&self, config: PaletteConfig) -> Result<PaletteConfig, String> {
        validate_config(&config)?;
        atomic_write_json(&self.root.join(CONFIG_FILE), &config)?;
        *lock(&self.config, "command palette config")? = config.clone();
        Ok(config)
    }
}

fn lock<'a, T>(mutex: &'a Mutex<T>, label: &str) -> Result<MutexGuard<'a, T>, String> {
    mutex
        .lock()
        .map_err(|_| format!("{label} lock is poisoned"))
}

fn ensure_main_window(window: &tauri::Window) -> Result<(), String> {
    if window.label() == "main" {
        Ok(())
    } else {
        Err("The command palette shortcut can only be configured from the main window".to_string())
    }
}

fn validate_config(config: &PaletteConfig) -> Result<(), String> {
    if config.schema_version != CONFIG_SCHEMA_VERSION
        || config.shortcut.trim().is_empty()
        || config.shortcut.len() > 128
    {
        return Err("Command palette configuration is invalid".to_string());
    }
    config
        .shortcut
        .parse::<tauri_plugin_global_shortcut::Shortcut>()
        .map_err(|error| format!("Command palette shortcut is invalid: {error}"))?;
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("Could not create {}: {error}", path.display()))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect {}: {error}", path.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(format!("{} is not a real directory", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not secure {}: {error}", path.display()))?;
    }
    Ok(())
}

fn load_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| format!("Could not decode {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Could not read {}: {error}", path.display())),
    }
}

fn atomic_write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    let temp = path.with_extension(format!("tmp-{}", Uuid::new_v4().simple()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .map_err(|error| format!("Could not stage {}: {error}", path.display()))?;
    file.write_all(&bytes).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    fs::rename(&temp, path)
        .map_err(|error| format!("Could not publish {}: {error}", path.display()))
}

/// Brings the main window to the front (creating no new window — the
/// palette renders inside it) and tells the frontend to open the palette
/// overlay. Called both from the OS-level global-shortcut handler (works
/// even when Little Monkey isn't the focused app — see `lib.rs::run`) and
/// from the `palette_show` command below (e.g. a future in-app trigger).
pub fn show_palette(app: &tauri::AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or("The main window is not available")?;
    if window.is_minimized().unwrap_or(false) {
        let _ = window.unminimize();
    }
    window.show().map_err(|error| error.to_string())?;
    window.set_focus().map_err(|error| error.to_string())?;
    app.emit_to("main", "palette://open", ())
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn palette_show(app: tauri::AppHandle) -> Result<(), String> {
    show_palette(&app)
}

#[tauri::command]
pub fn palette_config_get(
    state: tauri::State<'_, CommandPaletteState>,
) -> Result<PaletteConfig, String> {
    state.config()
}

#[tauri::command]
pub fn palette_config_save(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, CommandPaletteState>,
    config: PaletteConfig,
) -> Result<PaletteConfig, String> {
    ensure_main_window(&window)?;
    validate_config(&config)?;
    let previous_config = state.config()?;
    let previous = previous_config.shortcut.clone();
    if previous == config.shortcut {
        return state.save_config(config);
    }

    let next = config.shortcut.clone();
    app.global_shortcut()
        .register(next.as_str())
        .map_err(|error| format!("Could not register command palette shortcut: {error}"))?;
    if let Err(error) = state.save_config(config) {
        let _ = app.global_shortcut().unregister(next.as_str());
        return Err(error);
    }
    if let Err(error) = app.global_shortcut().unregister(previous.as_str()) {
        let _ = app.global_shortcut().unregister(next.as_str());
        let _ = state.save_config(previous_config);
        return Err(format!(
            "Could not release the previous shortcut; restored the previous command palette configuration: {error}"
        ));
    }
    Ok(state.config()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempRoot(PathBuf);
    impl TempRoot {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("little-monkey-palette-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn defaults_to_a_valid_parseable_shortcut() {
        let config = PaletteConfig::default();
        assert_eq!(config.shortcut, DEFAULT_SHORTCUT);
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn rejects_an_unparseable_shortcut() {
        let config = PaletteConfig {
            schema_version: CONFIG_SCHEMA_VERSION,
            shortcut: "definitely not a shortcut".to_string(),
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn rejects_an_empty_shortcut() {
        let config = PaletteConfig {
            schema_version: CONFIG_SCHEMA_VERSION,
            shortcut: String::new(),
        };
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn config_roundtrips_atomically_through_disk() {
        let root = TempRoot::new();
        let state = CommandPaletteState::production(&root.0).unwrap();
        assert_eq!(state.config().unwrap(), PaletteConfig::default());

        let updated = PaletteConfig {
            schema_version: CONFIG_SCHEMA_VERSION,
            shortcut: "CommandOrControl+Shift+P".to_string(),
        };
        // `save_config` alone (not `palette_config_save`, which also swaps
        // the live OS registration) is enough to exercise persistence/reload.
        state.save_config(updated.clone()).unwrap();
        assert_eq!(state.config().unwrap(), updated);

        // Reloading from the same directory picks up the persisted value.
        let reloaded = CommandPaletteState::production(&root.0).unwrap();
        assert_eq!(reloaded.config().unwrap(), updated);
    }

    #[test]
    fn save_config_rejects_invalid_configuration_without_persisting_it() {
        let root = TempRoot::new();
        let state = CommandPaletteState::production(&root.0).unwrap();
        let invalid = PaletteConfig {
            schema_version: CONFIG_SCHEMA_VERSION,
            shortcut: "nope".to_string(),
        };
        assert!(state.save_config(invalid).is_err());
        assert_eq!(state.config().unwrap(), PaletteConfig::default());
    }
}
