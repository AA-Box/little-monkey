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

/// Provider credential management.
///
/// The write lives in this binary rather than in the desktop app because
/// macOS scopes a keychain item to the executable that created it, and it is
/// this binary that reads the key back with nobody present: an agent the
/// daemon runs resolves its provider key through
/// `providers::read_key_with_env` (`chat::stream_turn`), where a
/// foreign-binary read's confirmation dialog has no one to answer it.
#[derive(clap::Subcommand, Debug)]
pub enum ProvidersCmd {
    /// Store a provider's API key, read from stdin so it never lands in a
    /// shell history or a process listing.
    SetKey {
        /// Provider id — a preset (openai, anthropic, gemini, openrouter) or a
        /// custom provider's id from `providers.json`.
        id: String,
    },
}

pub fn dispatch(action: &ProvidersCmd) -> Result<(), String> {
    match action {
        ProvidersCmd::SetKey { id } => set_key(id),
    }
}

fn set_key(id: &str) -> Result<(), String> {
    // Refuses an unknown id, and an extension provider (whose credentials live
    // on the extension's own secret slots), before anything is stored — a key
    // filed under a name nothing resolves is a key nothing ever reads.
    little_monkey_lib::providers::resolve_base_url(id, &load_custom_providers())?;

    let key = crate::channels_cli::read_secret_from_stdin()?;
    little_monkey_lib::providers::write_key(id, &key)?;
    println!("API key stored for {id}.");
    Ok(())
}
