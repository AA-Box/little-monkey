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
    /// Sign in with the official OpenRouter OAuth PKCE browser flow.
    /// Stores the resulting provider key in the CLI-owned OS keychain.
    Login {
        /// Currently supported: openrouter. Other subscriptions are not API keys.
        id: String,
    },
}

pub fn dispatch(action: &ProvidersCmd) -> Result<(), String> {
    match action {
        ProvidersCmd::SetKey { id } => set_key(id),
        ProvidersCmd::Login { id } => login(id),
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

fn login(id: &str) -> Result<(), String> {
    if id != "openrouter" {
        return Err("Only OpenRouter exposes a supported OAuth PKCE grant for model API keys. Use 'monkey providers set-key' for other providers.".into());
    }
    // The main CLI already runs inside Tokio. A separate OS thread owns this
    // interactive flow so we never block_on a nested runtime.
    std::thread::spawn(|| {
        tokio::runtime::Builder::new_current_thread().enable_all().build()
            .map_err(|e| format!("Failed to start authorization runtime: {e}"))?
            .block_on(little_monkey_lib::provider_oauth::login_openrouter_with(|url| {
                println!("Open this URL on the same machine to authorize OpenRouter: {url}");
                #[cfg(target_os = "macos")]
                let _ = std::process::Command::new("open").arg(&url).spawn();
                #[cfg(target_os = "linux")]
                let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
                Ok(())
            }))
    }).join().map_err(|_| "OpenRouter login thread panicked".to_string())??;
    println!("OpenRouter authorization succeeded; key saved for CLI and daemon.");
    Ok(())
}
