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

use crate::profiles::ProfileScopedPaths;
use crate::run_scope::{RunScope, Unattributed};
use crate::AppState;

/// Profile-scoped (K23). The default profile keeps this exact service name, so
/// every credential stored before profiles existed still resolves; any other
/// profile's secrets live under `<service>.profile.<id>`, which is a different
/// keychain item that this profile's code never names.
static KEYCHAIN_SERVICE: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::profiles::keychain_service("com.littlemonkey.app"));

/// Valid values for the app's effort scale — the five levels Anthropic's
/// native `output_config.effort` field accepts. Other providers with a
/// reasoning knob get a clamped subset on the wire (see
/// `clamped_reasoning_effort`); providers without one get nothing.
const VALID_EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Maps the app's five-level effort scale onto the three-level
/// `reasoning_effort` scale OpenAI-compatible reasoning surfaces accept:
/// `xhigh`/`max` clamp down to `high`, everything else passes through.
pub fn clamped_reasoning_effort(effort: &str) -> &str {
    match effort {
        "xhigh" | "max" => "high",
        other => other,
    }
}

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
    /// What the provider itself says about image input, when it says anything
    /// at all. `None` for the providers whose `/models` returns an id and
    /// nothing else (OpenAI, Gemini's OpenAI-compatible shim, most custom base
    /// URLs) — the frontend falls back to `visionModels.ts`'s name heuristic
    /// there, which is a guess and is wrong every time a new family ships.
    /// Skipped rather than serialized as `null` so the TS mirror can be
    /// optional: every `{ id }` literal in the existing tests stays valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    /// The model's context window, when the provider publishes one. Drives
    /// `usageStore`'s `contextLimit`, which is what `contextTrimmer.ts`'s
    /// `shouldTrim` needs before it will compact anything — a cloud model
    /// without this never auto-trims. Same skip/optional treatment as
    /// `vision`, and likewise for `tool_calling`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    /// Whether the provider says this model accepts tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calling: Option<bool>,
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
    /// OpenRouter: `architecture.input_modalities: ["text", "image"]`.
    #[serde(default)]
    architecture: Option<RawArchitecture>,
    /// Anthropic: `capabilities.image_input.supported`.
    #[serde(default)]
    capabilities: Option<RawCapabilities>,
    /// OpenRouter's context window.
    #[serde(default)]
    context_length: Option<u64>,
    /// Anthropic's, under a different name.
    #[serde(default)]
    max_input_tokens: Option<u64>,
    /// OpenRouter: `supported_parameters: ["tools", "tool_choice", ...]`.
    #[serde(default)]
    supported_parameters: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct RawArchitecture {
    #[serde(default)]
    input_modalities: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct RawCapabilities {
    #[serde(default)]
    image_input: Option<RawSupported>,
}

#[derive(Deserialize)]
struct RawSupported {
    #[serde(default)]
    supported: Option<bool>,
}

impl RawModelEntry {
    /// The provider's own vision answer, or `None` if it didn't give one.
    /// An entry that carries the field but says "no image input" is a real
    /// `Some(false)` — it overrides the name heuristic just as `Some(true)`
    /// does, since the provider knows better than a regex either way.
    fn vision(&self) -> Option<bool> {
        if let Some(supported) = self
            .capabilities
            .as_ref()
            .and_then(|c| c.image_input.as_ref())
            .and_then(|image| image.supported)
        {
            return Some(supported);
        }
        self.architecture
            .as_ref()
            .and_then(|a| a.input_modalities.as_ref())
            .map(|modalities| modalities.iter().any(|m| m == "image"))
    }

    /// The context window under whichever name the provider gave it.
    fn context_length(&self) -> Option<u64> {
        self.context_length.or(self.max_input_tokens)
    }

    /// Only OpenRouter answers this one — Anthropic's `capabilities` tree has
    /// no tool-use entry, so its models stay unknown rather than guessed at.
    fn tool_calling(&self) -> Option<bool> {
        self.supported_parameters
            .as_ref()
            .map(|params| params.iter().any(|p| p == "tools"))
    }
}

/// Resolves (and creates, if missing) `<app_data_dir>/providers.json`'s path.
fn providers_file_path(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data directory: {e}"))?;
    if !base.exists() {
        std::fs::create_dir_all(&base).map_err(|e| {
            format!(
                "Failed to create app data directory {}: {e}",
                base.display()
            )
        })?;
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
    let parsed: ProvidersFile = serde_json::from_str(&raw)
        .map_err(|e| format!("Failed to parse {}: {e}", path.display()))?;
    Ok(parsed.custom)
}

fn write_custom_providers(app: &AppHandle, entries: &[CustomProviderEntry]) -> Result<(), String> {
    let path = providers_file_path(app)?;
    let file = ProvidersFile {
        custom: entries.to_vec(),
    };
    let serialized = serde_json::to_string_pretty(&file)
        .map_err(|e| format!("Failed to serialize provider config: {e}"))?;
    std::fs::write(&path, serialized)
        .map_err(|e| format!("Failed to write {}: {e}", path.display()))
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

/// The custom provider list read straight off disk, for callers with no
/// `AppHandle` to resolve the app data dir through.
///
/// A missing or unreadable `providers.json` is an empty list rather than an
/// error: the presets are still resolvable without it, and a caller that then
/// fails to find its provider says so with a better message than "no file".
pub fn configured_custom_providers() -> Vec<CustomProviderEntry> {
    crate::app_paths::data_dir()
        .and_then(|dir| std::fs::read_to_string(dir.join("providers.json")).ok())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|value| value.get("custom").cloned())
        .and_then(|value| serde_json::from_value::<Vec<CustomProviderEntry>>(value).ok())
        .unwrap_or_default()
}

fn find_base_url(app: &AppHandle, id: &str) -> Result<String, String> {
    let custom = read_custom_providers(app)?;
    resolve_base_url(id, &custom)
}

/// Secret-free stable reference written into durable run snapshots. The
/// actual credential remains in the OS keychain and is still loaded only for
/// the lifetime of one request.
pub fn credential_ref_id(id: &str) -> String {
    format!("keychain:{}:{id}", *KEYCHAIN_SERVICE)
}

pub fn configured_endpoint(app: &AppHandle, id: &str) -> Result<String, String> {
    find_base_url(app, id).map(|value| value.trim_end_matches('/').to_string())
}

pub fn has_key(id: &str) -> bool {
    keyring::Entry::new(&KEYCHAIN_SERVICE, id)
        .and_then(|e| e.get_password())
        .is_ok()
}

pub fn read_key(provider_id: &str) -> Result<String, String> {
    let entry = keyring::Entry::new(&KEYCHAIN_SERVICE, provider_id)
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
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
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
    let entry = keyring::Entry::new(&KEYCHAIN_SERVICE, provider_id)
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
pub fn add_anthropic_headers(
    request: reqwest::RequestBuilder,
    provider_id: &str,
    api_key: &str,
) -> reqwest::RequestBuilder {
    if provider_id == "anthropic" {
        request
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
    } else {
        request
    }
}

/// GETs `${base_url}/models` and parses the OpenAI-style `data[].id` list,
/// plus whatever that entry says about image input (see
/// [`RawModelEntry::vision`]).
/// `pub` so `server.rs`'s `GET /v1/models` (phase 3) can list cloud provider
/// models the same keychain-authed way `providers_list_models` already does
/// — no behavior change.
pub async fn fetch_models(
    base_url: &str,
    provider_id: &str,
    api_key: &str,
) -> Result<Vec<ProviderModelInfo>, String> {
    // `egress::hardened()` rather than a default client because `base_url` is
    // user-configurable and [`add_anthropic_headers`] attaches `x-api-key`,
    // which reqwest does NOT strip across a cross-host redirect (it strips only
    // `Authorization`, `Cookie`, `Proxy-Authorization`, `WWW-Authenticate`) —
    // so a `302` from here could hand the key to a host the response chose.
    let client = crate::egress::hardened()
        .build()
        .map_err(|e| format!("Failed to build the provider HTTP client: {e}"))?;
    let request = add_anthropic_headers(
        client
            .get(format!("{base_url}/models"))
            .bearer_auth(api_key),
        provider_id,
        api_key,
    );

    let response = crate::egress::send(request)
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
        .filter_map(|entry| {
            let vision = entry.vision();
            let context_length = entry.context_length();
            let tool_calling = entry.tool_calling();
            entry.id.map(|id| ProviderModelInfo {
                id,
                vision,
                context_length,
                tool_calling,
            })
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}

/// Returns the built-in provider presets for native API/CLI consumers.
/// Desktop IPC uses [`providers_list_configured`], which already includes
/// these entries and their live keychain status, so exposing a second Tauri
/// command would only create two overlapping frontend contracts.
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
pub fn providers_add_custom(
    app: AppHandle,
    label: String,
    base_url: String,
) -> Result<ProviderConfig, String> {
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
pub async fn providers_set_key(
    app: AppHandle,
    id: String,
    api_key: String,
) -> Result<Vec<ProviderModelInfo>, String> {
    let api_key = api_key.trim().to_string();
    if api_key.is_empty() {
        return Err("API key is required".to_string());
    }

    let base_url = find_base_url(&app, &id)?;
    let models = fetch_models(&base_url, &id, &api_key).await?;

    let entry = keyring::Entry::new(&KEYCHAIN_SERVICE, &id)
        .map_err(|e| format!("Failed to access keychain: {e}"))?;
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
pub async fn providers_list_models(
    app: AppHandle,
    id: String,
) -> Result<Vec<ProviderModelInfo>, String> {
    let base_url = find_base_url(&app, &id)?;
    let api_key = read_key(&id)?;
    fetch_models(&base_url, &id, &api_key).await
}

/// Model Retirement and Compatibility Warnings (ROADMAP.md Phase 8, item 14):
/// flags any of `model_ids` (typically the provider's own already-fetched
/// model list — see `providers_list_models`/`ProviderCard.tsx`) that this
/// app's local, conservative retired-model registry recognizes, each with a
/// migration hint. See `model_retirement.rs`'s module doc for the honest
/// maintenance story (a versioned local list, not a live-verified source —
/// there is no upstream API this app can call to ask "is this retired?").
/// Pure and read-only: never touches the keychain or network.
#[tauri::command]
pub fn providers_check_model_retirements(
    provider_id: String,
    model_ids: Vec<String>,
) -> Vec<crate::model_retirement::CloudModelRetirementWarning> {
    crate::model_retirement::check_cloud_models_batch(&provider_id, &model_ids)
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
        Self {
            leftover: Vec::new(),
        }
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
    run_id: Option<String>,
) -> Result<(), String> {
    if let Some(ref e) = effort {
        if !VALID_EFFORT_LEVELS.contains(&e.as_str()) {
            return Err(format!("Unknown effort level '{e}'"));
        }
    }

    let frozen_endpoint = match run_id.as_deref() {
        Some(run_id) => Some(crate::run_commands::provider_endpoint_for_run(
            &app,
            state.inner(),
            run_id,
            &provider_id,
            &model,
        )?),
        None => None,
    };
    let cancel = Arc::new(Notify::new());
    state
        .stream_cancels
        .lock()
        .unwrap()
        .insert(request_id.clone(), cancel.clone());
    // The scope covers the whole stream, so every refusal raised anywhere inside it
    // is attributable without `run_stream_chat` — or the SSRF predicates several
    // frames below it — taking a run id they have no other use for. Both arms are
    // real here: a ledgered run carries its id, and an ordinary chat is not a run
    // and says so, rather than arriving at the sink as an unexplained blank.
    let scope = match run_id.as_deref() {
        Some(run_id) => RunScope::run(run_id),
        None => RunScope::Unattributed(Unattributed::UserAction),
    };
    // `scoped_with_egress` rather than `scoped`: it attaches the run's process row
    // when there is one, so the bytes this stream moves reach
    // `agent_processes.bytes_egressed` instead of an unattributed tally. See its
    // doc for what that column counts and how often it is written.
    let result = crate::run_commands::scoped_with_egress(
        &app,
        state.inner(),
        scope,
        run_stream_chat(
            &app,
            &request_id,
            &provider_id,
            &model,
            messages,
            tools,
            effort,
            cancel,
            frozen_endpoint,
        ),
    )
    .await;

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
/// `effort` is shaped per provider. Anthropic gets its native
/// `output_config.effort` field verbatim (low/medium/high/xhigh/max — see
/// platform.claude.com's effort docs): it's not part of the OpenAI
/// chat/completions schema, but Anthropic's compat layer forwards
/// unrecognized top-level JSON keys straight through to the underlying
/// Messages API request, the same way it documents doing for `thinking`.
/// OpenAI and Gemini's compat surface take the OpenAI-schema
/// `reasoning_effort` field, and OpenRouter normalizes the same scale under
/// `reasoning.effort` — all three only know low/medium/high, so the two
/// Anthropic-only top levels clamp down to `high`. Custom/unknown providers
/// get nothing: OpenAI-compatible servers commonly hard-reject
/// `reasoning_effort` on non-reasoning models, so an unknowable endpoint
/// must never receive a speculative field.
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
        "stream": true,
        "model": model,
    });

    // `tool_choice` without at least one tool is rejected by a number of
    // OpenAI-compatible/custom endpoints. No-tools runs (notably Compare)
    // omit both fields rather than sending an empty catalog plus "auto".
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        body["tool_choice"] = json!("auto");
    }

    // OpenAI-compatible streaming APIs only include the terminal usage
    // chunk when explicitly requested. Anthropic/Gemini compatibility
    // endpoints reject or ignore this OpenAI-only extension, matching the
    // CLI's request-shaping rule.
    if provider_id != "anthropic" && provider_id != "gemini" {
        body["stream_options"] = json!({ "include_usage": true });
    }

    if let Some(effort) = effort {
        match provider_id {
            "anthropic" => body["output_config"] = json!({ "effort": effort }),
            "openai" | "gemini" => {
                body["reasoning_effort"] = json!(clamped_reasoning_effort(effort));
            }
            "openrouter" => {
                body["reasoning"] = json!({ "effort": clamped_reasoning_effort(effort) });
            }
            _ => {}
        }
    }

    let request = client
        .post(format!("{base_url}/chat/completions"))
        .bearer_auth(api_key)
        .json(&body);
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
    frozen_endpoint: Option<String>,
) -> Result<(), String> {
    let base_url = match frozen_endpoint {
        Some(endpoint) => endpoint,
        None => configured_endpoint(app, provider_id)?,
    };
    let api_key = read_key(provider_id)?;

    // Same `x-api-key`-across-a-redirect reasoning as [`fetch_models`]. Note
    // that `egress::hardened()` sets a *read* timeout and never a total one:
    // this request is `"stream": true` and its body is consumed chunk by chunk
    // for as long as the model generates, so a total deadline would truncate a
    // long answer rather than catch a dead peer.
    let client = crate::egress::hardened()
        .build()
        .map_err(|e| format!("Failed to build the provider HTTP client: {e}"))?;
    let request = build_chat_request(
        &client,
        &base_url,
        provider_id,
        &api_key,
        model,
        &messages,
        &tools,
        effort.as_deref(),
    );

    // Stop button: race connection establishment itself, not just the body
    // stream below. Without this, a slow/hung provider (a loaded free-tier
    // model queuing before it sends headers) makes cancellation inert until
    // `send()` resolves on its own — the run would sit in `cancelling`
    // forever with no way to actually stop it.
    let response = tokio::select! {
        _ = cancel.notified() => {
            let _ = app.emit(
                "provider://chat-done",
                json!({ "request_id": request_id, "cancelled": true }),
            );
            return Ok(());
        }
        result = crate::egress::send(request) => {
            result.map_err(|e| format!("Failed to reach {base_url}: {e}"))?
        }
    };

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

    /// The two providers that answer the vision question themselves, in their
    /// own shapes, plus the OpenAI shape that answers nothing — a `None` there
    /// is what keeps the name heuristic in play for them.
    #[test]
    fn model_entries_report_vision_when_the_provider_says_so() {
        let parsed: RawModelsResponse = serde_json::from_str(
            r#"{"data": [
                {"id": "anthropic/claude-opus-5",
                 "architecture": {"input_modalities": ["text", "image"]}},
                {"id": "openai/gpt-oss-120b",
                 "architecture": {"input_modalities": ["text"]}},
                {"id": "claude-opus-5",
                 "capabilities": {"image_input": {"supported": true}}},
                {"id": "claude-instant-1.2",
                 "capabilities": {"image_input": {"supported": false}}},
                {"id": "gpt-4o", "object": "model", "owned_by": "openai"}
            ]}"#,
        )
        .expect("the fixture is a valid /models response");
        let vision: Vec<Option<bool>> = parsed.data.iter().map(RawModelEntry::vision).collect();
        assert_eq!(
            vision,
            vec![Some(true), Some(false), Some(true), Some(false), None]
        );
    }

    /// Context window and tool support, under each provider's own field name.
    /// The OpenAI-shaped entry answers neither, which is what leaves those
    /// capabilities "unknown" in the UI rather than guessed at.
    #[test]
    fn model_entries_report_context_length_and_tool_calling() {
        let parsed: RawModelsResponse = serde_json::from_str(
            r#"{"data": [
                {"id": "anthropic/claude-opus-5", "context_length": 1000000,
                 "supported_parameters": ["max_tokens", "tools", "tool_choice"]},
                {"id": "vendor/no-tools", "context_length": 8192,
                 "supported_parameters": ["max_tokens"]},
                {"id": "claude-opus-5", "max_input_tokens": 1000000},
                {"id": "gpt-4o", "object": "model", "owned_by": "openai"}
            ]}"#,
        )
        .expect("the fixture is a valid /models response");
        let read: Vec<(Option<u64>, Option<bool>)> = parsed
            .data
            .iter()
            .map(|entry| (entry.context_length(), entry.tool_calling()))
            .collect();
        assert_eq!(
            read,
            vec![
                (Some(1_000_000), Some(true)),
                (Some(8192), Some(false)),
                (Some(1_000_000), None),
                (None, None),
            ]
        );
    }

    #[test]
    fn validate_base_url_accepts_http_and_https() {
        assert_eq!(
            validate_base_url("https://api.openai.com/v1").unwrap(),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            validate_base_url("http://localhost:8080/v1/").unwrap(),
            "http://localhost:8080/v1"
        );
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
        assert_eq!(
            provider_env_var_name("openrouter"),
            "LITTLE_MONKEY_API_KEY_OPENROUTER"
        );
        assert_eq!(
            provider_env_var_name("anthropic"),
            "LITTLE_MONKEY_API_KEY_ANTHROPIC"
        );
    }

    #[test]
    fn provider_env_var_name_replaces_non_alphanumeric_chars() {
        // A custom provider id could contain a hyphen — standard env-var
        // naming replaces it with an underscore rather than dropping it
        // (dropping could collide two distinct provider ids onto one var).
        assert_eq!(
            provider_env_var_name("my-custom-provider"),
            "LITTLE_MONKEY_API_KEY_MY_CUSTOM_PROVIDER"
        );
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

    fn request_body(provider_id: &str, effort: Option<&str>) -> serde_json::Value {
        let client = reqwest::Client::new();
        let request = build_chat_request(
            &client,
            "https://example.com/v1",
            provider_id,
            "key",
            "some-model",
            &[],
            &[],
            effort,
        )
        .build()
        .unwrap();
        serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap()
    }

    #[test]
    fn build_chat_request_sends_anthropic_effort_verbatim_at_all_five_levels() {
        for level in VALID_EFFORT_LEVELS {
            let body = request_body("anthropic", Some(level));
            assert_eq!(body["output_config"]["effort"], *level);
            assert!(body.get("reasoning_effort").is_none());
            assert!(body.get("reasoning").is_none());
            assert!(body.get("stream_options").is_none());
        }
    }

    #[test]
    fn build_chat_request_clamps_effort_to_reasoning_effort_for_openai_and_gemini() {
        for provider in ["openai", "gemini"] {
            for (level, wire) in [
                ("low", "low"),
                ("medium", "medium"),
                ("high", "high"),
                ("xhigh", "high"),
                ("max", "high"),
            ] {
                let body = request_body(provider, Some(level));
                assert_eq!(body["reasoning_effort"], wire, "{provider} {level}");
                assert!(body.get("output_config").is_none());
                assert!(body.get("reasoning").is_none());
            }
        }
        // The OpenAI-only stream_options extension is orthogonal to effort.
        assert_eq!(
            request_body("openai", Some("max"))["stream_options"]["include_usage"],
            true
        );
        assert!(request_body("gemini", Some("max"))
            .get("stream_options")
            .is_none());
    }

    #[test]
    fn build_chat_request_nests_clamped_effort_under_reasoning_for_openrouter() {
        assert_eq!(
            request_body("openrouter", Some("medium"))["reasoning"]["effort"],
            "medium"
        );
        let clamped = request_body("openrouter", Some("max"));
        assert_eq!(clamped["reasoning"]["effort"], "high");
        assert!(clamped.get("reasoning_effort").is_none());
        assert!(clamped.get("output_config").is_none());
    }

    #[test]
    fn build_chat_request_omits_effort_entirely_for_custom_providers() {
        let body = request_body("my-custom-provider", Some("max"));
        assert!(body.get("output_config").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn build_chat_request_omits_every_effort_field_when_effort_is_none() {
        for provider in ["anthropic", "openai", "gemini", "openrouter", "custom-x"] {
            let body = request_body(provider, None);
            assert!(body.get("output_config").is_none(), "{provider}");
            assert!(body.get("reasoning_effort").is_none(), "{provider}");
            assert!(body.get("reasoning").is_none(), "{provider}");
        }
    }

    #[test]
    fn build_chat_request_offers_tools_with_auto_tool_choice_only_when_tools_exist() {
        let body = request_body("openai", None);
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());

        let client = reqwest::Client::new();
        let tool_req = build_chat_request(
            &client,
            "https://api.openai.com/v1",
            "openai",
            "key",
            "gpt-4o",
            &[],
            &[json!({"type": "function", "function": {"name": "read_file"}})],
            None,
        )
        .build()
        .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(tool_req.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
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
