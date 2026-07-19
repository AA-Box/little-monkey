//! CLI parity for the web-tools settings (design doc phase 4): loads the
//! same `web_settings.json` the GUI's Settings > Web tab writes (via
//! `web::web_get_settings`/`web::web_set_settings`), without a
//! `tauri::AppHandle` to resolve its path through — same hardcoded-
//! identifier app-data convention as `providers_cli.rs`/`mcp_cli.rs`. The
//! Brave API key itself needs no such wrapper: `web::read_brave_key()` is
//! already an `AppHandle`-free keychain read, so `agent.rs` calls it
//! directly.

use little_monkey_lib::web::{self, WebSettings};

fn settings_file_path() -> Option<std::path::PathBuf> {
    Some(little_monkey_lib::app_paths::data_dir()?.join("web_settings.json"))
}

/// Loads `web_settings.json`, falling back to `WebSettings::default()` when
/// the app-data dir can't be resolved, the file is missing, or it's
/// unreadable/corrupt — same full tolerance
/// `providers_cli::load_custom_providers` has for its own file. Nothing
/// configured yet (the common case for a CLI-only user) is indistinguishable
/// from "settings file couldn't be read" here on purpose: either way the
/// tool call should proceed with sane defaults (keyless DuckDuckGo search,
/// the 20k-char fetch cap, no local-network access) rather than failing the
/// whole tool call over a settings-file problem.
pub fn load_settings() -> WebSettings {
    let Some(path) = settings_file_path() else {
        return WebSettings::default();
    };
    web::load_settings_impl(&path).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_settings_never_panics_and_falls_back_to_default() {
        // No path-injection seam here (mirrors `mcp_cli::load_enabled_servers`'s
        // own test) — this just checks the real app-data resolution path is
        // safe to call and always yields *something* usable.
        let _ = load_settings();
    }
}
