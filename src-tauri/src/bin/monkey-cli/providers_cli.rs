//! Reads the same `providers.json` the GUI writes (custom OpenAI-compatible
//! endpoints added via Settings) without a `tauri::AppHandle` to resolve its
//! path through — computed directly via the same OS convention Tauri v2's
//! `app_data_dir()` uses: a per-platform base directory (what `dirs::data_dir()`
//! also returns) joined with the app's `identifier`.

use little_monkey_lib::providers::CustomProviderEntry;

fn providers_file_path() -> Option<std::path::PathBuf> {
    Some(little_monkey_lib::app_paths::data_dir()?.join("providers.json"))
}

/// Best-effort: an unreadable/missing/malformed file just means "no custom
/// providers" rather than a hard error — built-in presets (openai,
/// anthropic, gemini, openrouter) don't need this file at all.
pub fn load_custom_providers() -> Vec<CustomProviderEntry> {
    let Some(path) = providers_file_path() else {
        return Vec::new();
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    value
        .get("custom")
        .and_then(|c| serde_json::from_value::<Vec<CustomProviderEntry>>(c.clone()).ok())
        .unwrap_or_default()
}
