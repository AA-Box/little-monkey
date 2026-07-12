//! Multi-provider AI chat: cloud API keys (OpenAI, Anthropic, Google Gemini,
//! OpenRouter, and arbitrary custom OpenAI-compatible endpoints).
//!
//! Every one of those now exposes an OpenAI-compatible `chat/completions` +
//! `models` surface reachable with a plain bearer token, so this module is a
//! single generic adapter rather than N per-vendor ones — the wire format
//! Little Monkey already speaks to llama-server/Ollama (see `llamaClient.ts`) passes
//! straight through unchanged.
//!
//! API keys are billable secrets: they live in the OS keychain only (never
//! written to `providers.json`, never sent back to the frontend), and chat
//! requests are proxied through this Rust layer rather than fetched directly
//! from the WebView — the frontend only ever sees streamed response chunks
//! over Tauri events, mirroring `ollama_pull_model`'s progress-event
//! pattern. `has_key` is always a live keychain probe, never a persisted
//! flag, so it can't drift from reality.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Notify;

use crate::AppState;

const KEYCHAIN_SERVICE: &str = "com.littlemonkey.app";

/// Valid values for Anthropic's `output_config.effort` field — the same set
/// the native Messages API accepts. Ignored for every other provider (their
/// OpenAI-compatible surfaces have no equivalent knob).
const VALID_EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// A single built-in provider preset.
struct ProviderPresetDef {
    id: &'static str,
    label: &'static str,
    base_url: &'static str,
}

/// Built-in OpenAI-compatible provider presets. The frontend carries zero
/// hardcoded provider URLs — see `providers_list_presets`.
const PROVIDER_PRESETS: &[ProviderPresetDef] = &[
    ProviderPresetDef {
        id: "openai",
        label: "OpenAI",
        base_url: "https://api.openai.com/v1",
    },
    ProviderPresetDef {
        id: "anthropic",
        label: "Anthropic",
        base_url: "https://api.anthropic.com/v1",
    },
    ProviderPresetDef {
        id: "gemini",
        label: "Google Gemini",
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
    },
    ProviderPresetDef {
        id: "openrouter",
        label: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
    },
];

#[derive(Debug, Clone, Serialize)]
pub struct ProviderPreset {
    pub id: String,
    pub label: String,
    pub base_url: String,
}

/// A configured provider (preset or custom) as shown in Settings, with a
/// live `has_key` probe — never the key itself.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderConfig {
    pub id: String,
    pub label: String,
    pub base_url: String,
    pub is_custom: bool,
    pub has_key: bool,
}

/// A user-added OpenAI-compatible endpoint (Groq, Mistral, self-hosted,
/// etc.), persisted in `providers.json`. Contains no secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomProviderEntry {
    pub id: String,
    pub label: String,
    pub base_url: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProvidersFile {
    #[serde(default)]
    custom: Vec<CustomProviderEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderModelInfo {
    pub id: String,
}

/// Minimal shape of an OpenAI-style `GET /models` response — same defensive,
/// lenient style as `ollama.rs::RawTagEntry`.
#[derive(Deserialize, Default)]
struct RawModelsResponse {
    #[serde(default)]
    data: Vec<RawModelEntry>,
}

#[derive(Deserialize)]
struct RawModelEntry {
    #[serde(default)]
    id: Option<String>,
}

/// Resolves (and creates, if missing) `<app_data_dir>/providers.json`'s path.
fn providers_file_path(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data directory: {e}"))?;
    if !base.exists() {
        std::fs::create_dir_all(&base)
            .map_err(|e| format!("Failed to create app data directory {}: {e}", base.display()))?;
    }
    Ok(base.join("providers.json"))
}

/// `pub` so `server.rs`'s local API server (phase 3) can build its
/// provider-id routing catalog (presets + custom, via
/// [`providers_list_presets`] + this) without duplicating the
/// `providers.json` parsing logic — same file, same shape, no behavior
/// change.
pub fn read_custom_providers(app: &AppHandle) -> Result<Vec<CustomProviderEntry>, String> {
    let path = providers_file_path(app)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    let parsed: ProvidersFile =
        serde_json::from_str(&raw).map_err(|e| format!("Failed to parse {}: {e}", path.display()))?;
    Ok(parsed.custom)
}

fn write_custom_providers(app: &AppHandle, entries: &[CustomProviderEntry]) -> Result<(), String> {
    let path = providers_file_path(app)?;
    let file = ProvidersFile {
        custom: entries.to_vec(),
    };
    let serialized =
        serde_json::to_string_pretty(&file).map_err(|e| format!("Failed to serialize provider config: {e}"))?;
    std::fs::write(&path, serialized).map_err(|e| format!("Failed to write {}: {e}", path.display()))
}

/// Pure preset-then-custom lookup, parameterized by an already-loaded
/// `custom` list so it needs no `AppHandle` — reused by [`find_base_url`]
/// (which loads `custom` from `providers.json` via the GUI's app data dir)
/// and by the CLI (which loads the same file directly, having no WebView/
/// AppHandle of its own to resolve that path through).
pub fn resolve_base_url(id: &str, custom: &[CustomProviderEntry]) -> Result<String, String> {
    if let Some(preset) = PROVIDER_PRESETS.iter().find(|p| p.id == id) {
        return Ok(preset.base_url.to_string());
    }
    custom
        .iter()
        .find(|c| c.id == id)
        .map(|c| c.base_url.clone())
        .ok_or_else(|| format!("Unknown provider '{id}'"))
}

fn find_base_url(app: &AppHandle, id: &str) -> Result<String, String> {
    let custom = read_custom_providers(app)?;
    resolve_base_url(id, &custom)
}

pub fn has_key(id: &str) -> bool {
    keyring::Entry::new(KEYCHAIN_SERVICE, id)
        .and_then(|e| e.get_password())
        .is_ok()
}

pub fn read_key(provider_id: &str) -> Result<String, String> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, provider_id)
        .map_err(|e| format!("Failed to access keychain: {e}"))?;
    entry.get_password().map_err(|e| match e {
        keyring::Error::NoEntry => {
            format!("No API key saved for '{provider_id}' — add one in Settings.")
        }
        other => format!("Failed to read saved key: {other}"),
    })
}

/// Converts a provider id into its env-var name for [`read_key_with_env`]:
/// `openrouter` -> `LITTLE_MONKEY_API_KEY_OPENROUTER`. Non-alphanumeric
/// characters (a custom provider id could contain `-`) become `_`, matching
/// standard env-var naming.
fn provider_env_var_name(provider_id: &str) -> String {
    let upper: String = provider_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect();
    format!("LITTLE_MONKEY_API_KEY_{upper}")
}

/// `read_key`, but tried only after two env-var fallbacks that don't exist
/// for the keychain-only path: `LITTLE_MONKEY_API_KEY_<PROVIDER_ID_UPPER>`
/// first, then the generic `LITTLE_MONKEY_API_KEY` — for `monkey-cli task
/// run` in CI, where there is no OS keychain to read from at all (design doc
/// slice 1). Scoped to reading only (never persisted anywhere), and the GUI
/// never calls this — `read_key` itself, and its keychain-only behavior when
/// neither env var is set, are both completely unchanged.
pub fn read_key_with_env(provider_id: &str) -> Result<String, String> {
    if let Ok(key) = std::env::var(provider_env_var_name(provider_id)) {
        if !key.is_empty() {
            return Ok(key);
        }
    }
    if let Ok(key) = std::env::var("LITTLE_MONKEY_API_KEY") {
        if !key.is_empty() {
            return Ok(key);
        }
    }
    read_key(provider_id)
}

fn remove_key_impl(provider_id: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, provider_id)
        .map_err(|e| format!("Failed to access keychain: {e}"))?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("Failed to remove saved key: {e}")),
    }
}

/// Rejects anything that isn't a plain `http(s)://...` base URL and
/// normalizes away a trailing slash, so `${base_url}/models` never ends up
/// with a double slash.
fn validate_base_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err(format!(
            "Invalid base URL '{raw}': must start with http:// or https://"
        ));
    }
    if trimmed.len() <= "https://".len() {
        return Err(format!("Invalid base URL '{raw}'"));
    }
    Ok(trimmed.to_string())
}

/// Lowercases `label` and keeps only alphanumerics, collapsing everything
/// else into single dashes, e.g. "Groq Cloud!!" -> "groq-cloud".
fn slugify(label: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for ch in label.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "provider".to_string()
    } else {
        slug
    }
}

/// `slugify(label)`, disambiguated against `existing` ids with a numeric
/// suffix (`-2`, `-3`, ...) so two custom providers can never collide.
fn unique_slug(label: &str, existing: &HashSet<String>) -> String {
    let base = slugify(label);
    if !existing.contains(&base) {
        return base;
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if !existing.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("HashSet::contains cannot fail for every n");
}

/// Anthropic's native API additionally expects `x-api-key` +
/// `anthropic-version` alongside (or instead of) a bearer token — a no-op
/// for every other provider, whose OpenAI-compatible surfaces just ignore
/// unrecognized headers. Factored out of [`fetch_models`]/[`build_chat_request`]
/// (both applied the same two headers inline) so `server.rs`'s local API
/// server (phase 3) can apply the identical quirk to an arbitrary
/// already-OpenAI-shaped body it forwards verbatim, without duplicating the
/// header names/version string in a third place. `pub` for that reuse; no
/// behavior change to either existing call site.
pub fn add_anthropic_headers(request: reqwest::RequestBuilder, provider_id: &str, api_key: &str) -> reqwest::RequestBuilder {
    if provider_id == "anthropic" {
        request
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
    } else {
        request
    }
}

/// GETs `${base_url}/models` and parses the OpenAI-style `data[].id` list.
/// `pub` so `server.rs`'s `GET /v1/models` (phase 3) can list cloud provider
/// models the same keychain-authed way `providers_list_models` already does
/// — no behavior change.
pub async fn fetch_models(
    base_url: &str,
    provider_id: &str,
    api_key: &str,
) -> Result<Vec<ProviderModelInfo>, String> {
    let client = reqwest::Client::new();
    let request = add_anthropic_headers(client.get(format!("{base_url}/models")).bearer_auth(api_key), provider_id, api_key);

    let response = request
        .send()
        .await
        .map_err(|e| format!("Failed to reach {base_url}: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        let detail = response.text().await.unwrap_or_default();
        return Err(format!(
            "Couldn't list models ({status}){}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        ));
    }

    let parsed: RawModelsResponse = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse model list from {base_url}: {e}"))?;

    let mut models: Vec<ProviderModelInfo> = parsed
        .data
        .into_iter()
        .filter_map(|entry| entry.id.map(|id| ProviderModelInfo { id }))
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}

/// Returns the built-in provider presets, so the frontend carries zero
/// hardcoded provider URLs of its own.
#[tauri::command]
pub fn providers_list_presets() -> Vec<ProviderPreset> {
    PROVIDER_PRESETS
        .iter()
        .map(|p| ProviderPreset {
            id: p.id.to_string(),
            label: p.label.to_string(),
            base_url: p.base_url.to_string(),
        })
        .collect()
}

/// Every configured provider (built-in presets + custom), each annotated
/// with a live `has_key` keychain probe.
#[tauri::command]
pub fn providers_list_configured(app: AppHandle) -> Result<Vec<ProviderConfig>, String> {
    let custom = read_custom_providers(&app)?;

    let mut out: Vec<ProviderConfig> = PROVIDER_PRESETS
        .iter()
        .map(|p| ProviderConfig {
            id: p.id.to_string(),
            label: p.label.to_string(),
            base_url: p.base_url.to_string(),
            is_custom: false,
            has_key: has_key(p.id),
        })
        .collect();

    for c in custom {
        out.push(ProviderConfig {
            has_key: has_key(&c.id),
            id: c.id,
            label: c.label,
            base_url: c.base_url,
            is_custom: true,
        });
    }

    Ok(out)
}

/// Registers a new custom OpenAI-compatible provider (Groq, Mistral,
/// self-hosted, etc.) with no key yet — call `providers_set_key` next.
#[tauri::command]
pub fn providers_add_custom(app: AppHandle, label: String, base_url: String) -> Result<ProviderConfig, String> {
    let label = label.trim().to_string();
    if label.is_empty() {
        return Err("Label is required".to_string());
    }
    let base_url = validate_base_url(&base_url)?;

    let mut custom = read_custom_providers(&app)?;
    let existing_ids: HashSet<String> = PROVIDER_PRESETS
        .iter()
        .map(|p| p.id.to_string())
        .chain(custom.iter().map(|c| c.id.clone()))
        .collect();
    let id = unique_slug(&label, &existing_ids);

    custom.push(CustomProviderEntry {
        id: id.clone(),
        label: label.clone(),
        base_url: base_url.clone(),
    });
    write_custom_providers(&app, &custom)?;

    Ok(ProviderConfig {
        id,
        label,
        base_url,
        is_custom: true,
        has_key: false,
    })
}

/// Removes a custom provider's metadata and any saved key for it. Presets
/// can never be removed this way — only their key (`providers_remove_key`).
#[tauri::command]
pub fn providers_remove_custom(app: AppHandle, id: String) -> Result<(), String> {
    let mut custom = read_custom_providers(&app)?;
    let before = custom.len();
    custom.retain(|c| c.id != id);
    if custom.len() == before {
        return Err(format!("Unknown custom provider '{id}'"));
    }
    write_custom_providers(&app, &custom)?;
    remove_key_impl(&id)
}

/// Validates `api_key` by fetching the provider's model list *before*
/// touching the keychain — a bad key is never persisted — then saves it and
/// returns the model list immediately so the caller doesn't need a second
/// round trip.
#[tauri::command]
pub async fn providers_set_key(app: AppHandle, id: String, api_key: String) -> Result<Vec<ProviderModelInfo>, String> {
    let api_key = api_key.trim().to_string();
    if api_key.is_empty() {
        return Err("API key is required".to_string());
    }

    let base_url = find_base_url(&app, &id)?;
    let models = fetch_models(&base_url, &id, &api_key).await?;

    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, &id).map_err(|e| format!("Failed to access keychain: {e}"))?;
    entry
        .set_password(&api_key)
        .map_err(|e| format!("Failed to save key to keychain: {e}"))?;

    Ok(models)
}

/// Removes a provider's saved key (preset or custom) from the keychain.
#[tauri::command]
pub fn providers_remove_key(id: String) -> Result<(), String> {
    remove_key_impl(&id)
}

/// Manual "refresh models" — re-fetches using the already-saved key.
#[tauri::command]
pub async fn providers_list_models(app: AppHandle, id: String) -> Result<Vec<ProviderModelInfo>, String> {
    let base_url = find_base_url(&app, &id)?;
    let api_key = read_key(&id)?;
    fetch_models(&base_url, &id, &api_key).await
}

/// Buffers raw bytes across chunk boundaries and only ever hands out valid
/// UTF-8 text, so a multi-byte character split across two network chunks
/// (common with emoji/non-ASCII model output) never breaks `String`
/// construction — the same guarantee `TextDecoder({stream:true})` gives the
/// frontend's own chunk handling for local llama-server/Ollama chat.
pub struct Utf8ChunkAccumulator {
    leftover: Vec<u8>,
}

impl Utf8ChunkAccumulator {
    pub fn new() -> Self {
        Self { leftover: Vec::new() }
    }

    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.leftover.extend_from_slice(bytes);
        match std::str::from_utf8(&self.leftover) {
            Ok(s) => {
                let out = s.to_string();
                self.leftover.clear();
                out
            }
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                let out = std::str::from_utf8(&self.leftover[..valid_up_to])
                    .expect("valid_up_to guarantees a valid UTF-8 prefix")
                    .to_string();
                self.leftover = self.leftover[valid_up_to..].to_vec();
                out
            }
        }
    }

    /// Called once the stream ends: flushes any still-buffered tail bytes
    /// (lossily — a stream that ends mid-character is already truncated).
    pub fn finish(&mut self) -> Option<String> {
        if self.leftover.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(&std::mem::take(&mut self.leftover)).into_owned())
        }
    }
}

/// The secure chat proxy: resolves the provider's base URL + keychain key,
/// POSTs the same OpenAI-shaped body Little Monkey already sends to llama-server/
/// Ollama, and re-emits each decoded chunk of the SSE response as
/// `provider://chat-chunk` events keyed by `request_id` — the frontend's
/// existing wire-format parsing (see `llamaClient.ts`) handles the rest.
/// The API key is read into memory only for this request's lifetime and is
/// never sent back to the frontend.
#[tauri::command]
pub async fn providers_stream_chat(
    app: AppHandle,
    state: State<'_, AppState>,
    request_id: String,
    provider_id: String,
    model: String,
    messages: Vec<serde_json::Value>,
    tools: Vec<serde_json::Value>,
    effort: Option<String>,
) -> Result<(), String> {
    if let Some(ref e) = effort {
        if !VALID_EFFORT_LEVELS.contains(&e.as_str()) {
            return Err(format!("Unknown effort level '{e}'"));
        }
    }

    let cancel = Arc::new(Notify::new());
    state
        .stream_cancels
        .lock()
        .unwrap()
        .insert(request_id.clone(), cancel.clone());

    let result = run_stream_chat(&app, &request_id, &provider_id, &model, messages, tools, effort, cancel).await;

    state.stream_cancels.lock().unwrap().remove(&request_id);

    if let Err(ref message) = result {
        let _ = app.emit(
            "provider://chat-error",
            json!({ "request_id": request_id, "message": message }),
        );
    }
    result
}

/// Cancels an in-flight `providers_stream_chat` request. A no-op (not an
/// error) if `request_id` has already finished or never existed — the Stop
/// button races the stream's own completion by design.
#[tauri::command]
pub fn providers_cancel_chat(app: AppHandle, request_id: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    if let Some(cancel) = state.stream_cancels.lock().unwrap().get(&request_id) {
        cancel.notify_one();
    }
    Ok(())
}

/// Builds the `POST {base_url}/chat/completions` request for `provider_id`,
/// including Anthropic's extra native-API headers alongside the bearer
/// token (harmless no-ops for every other provider). Shared by the GUI's
/// streaming proxy (below) and the CLI, which talks to the same providers
/// directly — no WebView/keychain-proxy split needed in a terminal.
///
/// `effort` maps to Anthropic's native `output_config.effort` field
/// (low/medium/high/xhigh/max — see platform.claude.com's effort docs). It's
/// not part of the OpenAI chat/completions schema (OpenAI's own
/// `reasoning_effort` is a different field the Anthropic compat layer
/// ignores), but the compat layer forwards unrecognized top-level JSON keys
/// straight through to the underlying Messages API request, the same way it
/// documents doing for `thinking`. Only sent for `provider_id == "anthropic"`
/// — every other provider either has no such knob or a differently-shaped
/// one, and sending it there would be a meaningless extra field at best.
pub fn build_chat_request(
    client: &reqwest::Client,
    base_url: &str,
    provider_id: &str,
    api_key: &str,
    model: &str,
    messages: &[serde_json::Value],
    tools: &[serde_json::Value],
    effort: Option<&str>,
) -> reqwest::RequestBuilder {
    let mut body = json!({
        "messages": messages,
        "tools": tools,
        "tool_choice": "auto",
        "stream": true,
        "model": model,
    });

    if provider_id == "anthropic" {
        if let Some(effort) = effort {
            body["output_config"] = json!({ "effort": effort });
        }
    }

    let request = client.post(format!("{base_url}/chat/completions")).bearer_auth(api_key).json(&body);
    add_anthropic_headers(request, provider_id, api_key)
}

async fn run_stream_chat(
    app: &AppHandle,
    request_id: &str,
    provider_id: &str,
    model: &str,
    messages: Vec<serde_json::Value>,
    tools: Vec<serde_json::Value>,
    effort: Option<String>,
    cancel: Arc<Notify>,
) -> Result<(), String> {
    let base_url = find_base_url(app, provider_id)?;
    let api_key = read_key(provider_id)?;

    let client = reqwest::Client::new();
    let request =
        build_chat_request(&client, &base_url, provider_id, &api_key, model, &messages, &tools, effort.as_deref());

    let response = request
        .send()
        .await
        .map_err(|e| format!("Failed to reach {base_url}: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        return Err(format!(
            "{provider_id} request failed ({status}){}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        ));
    }

    let mut stream = response.bytes_stream();
    let mut acc = Utf8ChunkAccumulator::new();

    loop {
        tokio::select! {
            _ = cancel.notified() => {
                // Stop button: end the stream early without flushing any
                // still-accumulating tool call — the frontend's SSE parser
                // would otherwise synthesize a bogus call from a partial
                // fragment. `cancelled: true` tells it to skip that flush.
                let _ = app.emit(
                    "provider://chat-done",
                    json!({ "request_id": request_id, "cancelled": true }),
                );
                return Ok(());
            }
            chunk_result = stream.next() => {
                match chunk_result {
                    Some(Ok(chunk)) => {
                        let text = acc.push(&chunk);
                        if !text.is_empty() {
                            let _ = app.emit(
                                "provider://chat-chunk",
                                json!({ "request_id": request_id, "chunk": text }),
                            );
                        }
                    }
                    Some(Err(e)) => return Err(format!("Stream error from {provider_id}: {e}")),
                    None => break,
                }
            }
        }
    }

    if let Some(text) = acc.finish() {
        let _ = app.emit(
            "provider://chat-chunk",
            json!({ "request_id": request_id, "chunk": text }),
        );
    }

    let _ = app.emit("provider://chat-done", json!({ "request_id": request_id }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_have_unique_ids_and_valid_urls() {
        let mut ids = HashSet::new();
        for preset in PROVIDER_PRESETS {
            assert!(ids.insert(preset.id), "duplicate preset id '{}'", preset.id);
            assert!(validate_base_url(preset.base_url).is_ok());
        }
    }

    #[test]
    fn validate_base_url_accepts_http_and_https() {
        assert_eq!(validate_base_url("https://api.openai.com/v1").unwrap(), "https://api.openai.com/v1");
        assert_eq!(validate_base_url("http://localhost:8080/v1/").unwrap(), "http://localhost:8080/v1");
    }

    #[test]
    fn validate_base_url_rejects_non_http_schemes() {
        assert!(validate_base_url("ftp://example.com").is_err());
        assert!(validate_base_url("file:///etc/passwd").is_err());
        assert!(validate_base_url("api.openai.com/v1").is_err());
        assert!(validate_base_url("").is_err());
        assert!(validate_base_url("https://").is_err());
    }

    #[test]
    fn provider_env_var_name_upcases_and_prefixes() {
        assert_eq!(provider_env_var_name("openrouter"), "LITTLE_MONKEY_API_KEY_OPENROUTER");
        assert_eq!(provider_env_var_name("anthropic"), "LITTLE_MONKEY_API_KEY_ANTHROPIC");
    }

    #[test]
    fn provider_env_var_name_replaces_non_alphanumeric_chars() {
        // A custom provider id could contain a hyphen — standard env-var
        // naming replaces it with an underscore rather than dropping it
        // (dropping could collide two distinct provider ids onto one var).
        assert_eq!(provider_env_var_name("my-custom-provider"), "LITTLE_MONKEY_API_KEY_MY_CUSTOM_PROVIDER");
    }

    #[test]
    fn slugify_normalizes_labels() {
        assert_eq!(slugify("Groq Cloud!!"), "groq-cloud");
        assert_eq!(slugify("  Self Hosted vLLM  "), "self-hosted-vllm");
        assert_eq!(slugify("***"), "provider");
    }

    #[test]
    fn unique_slug_disambiguates_collisions() {
        let mut existing = HashSet::new();
        existing.insert("groq".to_string());
        existing.insert("groq-2".to_string());
        assert_eq!(unique_slug("Groq", &existing), "groq-3");
        assert_eq!(unique_slug("Mistral", &existing), "mistral");
    }

    #[test]
    fn build_chat_request_adds_output_config_effort_for_anthropic_only() {
        let client = reqwest::Client::new();

        let anthropic_req = build_chat_request(
            &client,
            "https://api.anthropic.com/v1",
            "anthropic",
            "key",
            "claude-opus-4-8",
            &[],
            &[],
            Some("xhigh"),
        )
        .build()
        .unwrap();
        let body: serde_json::Value = serde_json::from_slice(anthropic_req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["output_config"]["effort"], "xhigh");

        let openai_req = build_chat_request(
            &client,
            "https://api.openai.com/v1",
            "openai",
            "key",
            "gpt-4o",
            &[],
            &[],
            Some("xhigh"),
        )
        .build()
        .unwrap();
        let body: serde_json::Value = serde_json::from_slice(openai_req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn utf8_chunk_accumulator_reassembles_split_multibyte_chars() {
        let bytes = "café 🎉".as_bytes();
        let mut acc = Utf8ChunkAccumulator::new();
        let mut out = String::new();
        // Feed one byte at a time, deliberately splitting multi-byte
        // sequences mid-character.
        for b in bytes {
            out.push_str(&acc.push(&[*b]));
        }
        if let Some(tail) = acc.finish() {
            out.push_str(&tail);
        }
        assert_eq!(out, "café 🎉");
    }
}
