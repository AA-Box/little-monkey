//! Local Agent Integration Launcher (ROADMAP Phase 8, item 13).
//!
//! Generates safe, working local-provider configuration snippets for
//! **external** agent tools/editors (Continue.dev, aider, and generic
//! OpenAI-SDK-compatible CLIs) so a user can point that tool at Little
//! Monkey's own local API endpoint without hand-editing that tool's config
//! format themselves. This module also detects drift in a previously
//! generated (or user-pasted) config relative to the app's current state:
//! a model that is no longer installed, an endpoint/port that no longer
//! matches the running server, a missing now-required auth header, an
//! unsupported/oversized context length, and telemetry-sensitive defaults.
//!
//! Every generated route and auth shape here must match what
//! `m3_http_server.rs` actually serves: all inference/model routes live
//! under `/v1/*` (`GET /v1/models`, `POST /v1/chat/completions`, ...), and
//! authorization is the paired scoped-bearer-token scheme from
//! `compatibility_hub::LanServerPolicy` (see `m3_runtime_hub`'s
//! `begin_pairing`/`complete_pairing`). This module never invents an
//! endpoint/route this app does not serve (no Ollama-native `/api/*`
//! routes are generated, because the M3 HTTP server does not advertise
//! them) and never fabricates a placeholder secret pretending to be a
//! working API key — when a bearer token is required but not supplied it
//! is called out explicitly as a placeholder pointing back at the real LAN
//! pairing flow.
//!
//! Context-length resolution reuses the *existing* source of truth for a
//! runtime's effective context window — the `context_size`/`num_ctx`
//! `AdvancedSettingCapability` already exposed by `runtime_adapter.rs` and
//! persisted per-runtime via `M3RuntimeHub::runtime_config` — rather than
//! inventing a parallel concept. (The "Context and KV Cache Control
//! Center" Phase 8 item was still an open, unmerged PR at the time this
//! module was written; if/when it lands, `effective_context_tokens` below
//! should be reconciled with its resolver instead of this one.)
//!
//! Tauri-free and unit-testable: every function here takes plain data and
//! has no dependency on `tauri`. The `#[tauri::command]` glue that gathers
//! that plain data from the live `M3RuntimeHub` lives in `m3_commands.rs`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::compatibility_hub::{LanServerPolicy, TlsPolicy};
use crate::m3_runtime_hub::{M3InstalledModelView, M3RuntimeCapabilityView, M3RuntimeKind};
use crate::runtime_adapter::SettingValue;

const MAX_PASTED_CONFIG_BYTES: usize = 64 * 1024;
const AUTH_NOT_REQUIRED_MARKER: &str = "not-required-loopback-only";
const AUTH_PLACEHOLDER_MARKER: &str = "REPLACE_WITH_PAIRED_TOKEN";

/// External tool/editor config formats this launcher can generate. Each is a
/// real, documented config shape for a well-known tool that supports a
/// custom OpenAI-compatible endpoint — not a guess at an undocumented
/// schema. See the module doc for accuracy notes on each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTool {
    /// `.continue/config.yaml`, documented at
    /// <https://docs.continue.dev/customize/model-providers/top-level/openai>
    /// and <https://docs.continue.dev/reference> (`models[].provider: openai`,
    /// `apiBase`, `apiKey`, `models[].defaultCompletionOptions.contextLength`).
    /// Continue's telemetry opt-out ("Allow Anonymous Telemetry") is an
    /// IDE/editor setting, not a config.yaml field.
    ContinueDev,
    /// `.aider.conf.yml`, documented at
    /// <https://aider.chat/docs/llms/openai-compat.html> and
    /// <https://aider.chat/docs/config/options.html> (`openai-api-base`,
    /// `openai-api-key`, `model` — aider requires an `openai/` model-name
    /// prefix for custom OpenAI-compatible endpoints — and
    /// `analytics-disable` for aider's optional usage-analytics prompt).
    Aider,
    /// A generic `.env` pair recognized by the official OpenAI Python/Node
    /// SDKs and many tools built on them: `OPENAI_BASE_URL` +
    /// `OPENAI_API_KEY`. Some older tools instead read the legacy
    /// `OPENAI_API_BASE` name; this generator emits the current SDK name
    /// and documents the older one in a comment.
    OpenAiEnv,
}

fn tool_label(tool: AgentTool) -> &'static str {
    match tool {
        AgentTool::ContinueDev => "Continue.dev",
        AgentTool::Aider => "aider",
        AgentTool::OpenAiEnv => "This OpenAI-compatible environment file",
    }
}

/// The real, currently-effective local endpoint shape, derived from the
/// app's own persisted `LanServerPolicy` — never a placeholder host/port.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEndpointInfo {
    /// Includes the `/v1` prefix every advertised route in
    /// `m3_http_server.rs` actually lives under.
    pub base_url: String,
    pub loopback_only: bool,
    pub auth_required: bool,
    pub tls: bool,
}

/// Derives the endpoint an external tool should be pointed at from the
/// app's real, persisted LAN/API server policy.
pub fn resolve_endpoint(policy: &LanServerPolicy) -> AgentEndpointInfo {
    let scheme = if matches!(policy.tls, TlsPolicy::Certificate { .. }) {
        "https"
    } else {
        "http"
    };
    AgentEndpointInfo {
        base_url: format!("{scheme}://{}:{}/v1", policy.bind_address, policy.port),
        loopback_only: policy.is_loopback(),
        auth_required: policy.require_authentication,
        tls: matches!(policy.tls, TlsPolicy::Certificate { .. }),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentWarningKind {
    /// The config's context length is missing/unknown or exceeds what the
    /// runtime currently serves for the selected model.
    ContextLength,
    /// A tool-specific telemetry/analytics default worth calling out.
    Telemetry,
    /// The config needs a real bearer token the generator could not embed.
    Auth,
    /// A previously-generated config's auth material no longer matches the
    /// server's current authentication requirement.
    AuthDrift,
    /// A previously-generated/pasted config references a model that is no
    /// longer installed.
    ModelMissing,
    /// A previously-generated/pasted config's endpoint no longer matches
    /// the server's current bind address/port.
    EndpointDrift,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfigWarning {
    pub kind: AgentWarningKind,
    pub message: String,
}

/// Everything needed to render one tool's config, already resolved to real
/// values by the `#[tauri::command]` glue (real endpoint, a model id that
/// is actually installed, and the runtime's actual effective context size).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateAgentConfigRequest {
    pub tool: AgentTool,
    pub endpoint: AgentEndpointInfo,
    pub model_id: String,
    pub effective_context_tokens: Option<u64>,
    /// A real paired bearer token, if the caller supplied one (e.g. pasted
    /// from a just-completed pairing). `None` means either auth is not
    /// required (loopback, `require_authentication: false`) or the caller
    /// has not supplied one yet — `generate_config` tells the two apart via
    /// `endpoint.auth_required`.
    pub auth_token: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneratedAgentConfig {
    pub tool: AgentTool,
    pub filename: String,
    pub content: String,
    pub warnings: Vec<AgentConfigWarning>,
}

enum AuthMaterial<'a> {
    NotRequired,
    RealToken(&'a str),
    Missing,
}

fn auth_material<'a>(required: bool, token: Option<&'a str>) -> AuthMaterial<'a> {
    match token.map(str::trim) {
        Some(token) if !token.is_empty() => AuthMaterial::RealToken(token),
        _ if !required => AuthMaterial::NotRequired,
        _ => AuthMaterial::Missing,
    }
}

fn auth_value(auth: &AuthMaterial<'_>, tool: AgentTool, warnings: &mut Vec<AgentConfigWarning>) -> String {
    match auth {
        AuthMaterial::NotRequired => AUTH_NOT_REQUIRED_MARKER.to_string(),
        AuthMaterial::RealToken(token) => (*token).to_string(),
        AuthMaterial::Missing => {
            warnings.push(AgentConfigWarning {
                kind: AgentWarningKind::Auth,
                message: format!(
                    "{} requires an API key/token field, but no paired bearer token was supplied — the value below is a placeholder. Pair a client in Settings > Runtime Hub > LAN and paste the real token in before using this config.",
                    tool_label(tool)
                ),
            });
            AUTH_PLACEHOLDER_MARKER.to_string()
        }
    }
}

fn context_unknown_warning() -> AgentConfigWarning {
    AgentConfigWarning {
        kind: AgentWarningKind::ContextLength,
        message: "Little Monkey could not determine an effective context window for this model/runtime, so the generated config does not declare one — the tool's own default applies and may not match what this runtime can actually serve.".to_string(),
    }
}

fn continue_telemetry_note() -> AgentConfigWarning {
    AgentConfigWarning {
        kind: AgentWarningKind::Telemetry,
        message: "Continue's anonymous telemetry opt-out (\"Allow Anonymous Telemetry\") is an IDE/editor setting, not a config.yaml field — this generated file cannot disable it for you.".to_string(),
    }
}

fn aider_telemetry_note() -> AgentConfigWarning {
    AgentConfigWarning {
        kind: AgentWarningKind::Telemetry,
        message: "analytics-disable is set to true below so aider's optional usage-analytics opt-in prompt stays off by default; remove the line if you want to opt in.".to_string(),
    }
}

fn generate_continue(request: &GenerateAgentConfigRequest) -> GeneratedAgentConfig {
    let mut warnings = Vec::new();
    let auth = auth_material(request.endpoint.auth_required, request.auth_token.as_deref());
    let api_key = auth_value(&auth, AgentTool::ContinueDev, &mut warnings);

    let mut lines = vec![
        "name: Little Monkey Local".to_string(),
        "version: 1.0.0".to_string(),
        "schema: v1".to_string(),
        "models:".to_string(),
        format!("  - name: {}", request.model_id),
        "    provider: openai".to_string(),
        format!("    model: {}", request.model_id),
        format!("    apiBase: {}", request.endpoint.base_url),
        format!("    apiKey: {api_key}"),
        "    roles:".to_string(),
        "      - chat".to_string(),
        "      - edit".to_string(),
        "      - apply".to_string(),
    ];
    match request.effective_context_tokens {
        Some(tokens) => {
            lines.push("    defaultCompletionOptions:".to_string());
            lines.push(format!("      contextLength: {tokens}"));
        }
        None => warnings.push(context_unknown_warning()),
    }
    warnings.push(continue_telemetry_note());

    GeneratedAgentConfig {
        tool: AgentTool::ContinueDev,
        filename: ".continue/config.yaml".to_string(),
        content: lines.join("\n") + "\n",
        warnings,
    }
}

fn generate_aider(request: &GenerateAgentConfigRequest) -> GeneratedAgentConfig {
    let mut warnings = Vec::new();
    let auth = auth_material(request.endpoint.auth_required, request.auth_token.as_deref());
    let api_key = auth_value(&auth, AgentTool::Aider, &mut warnings);

    let lines = vec![
        format!("openai-api-base: {}", request.endpoint.base_url),
        format!("openai-api-key: {api_key}"),
        format!("model: openai/{}", request.model_id),
        "analytics-disable: true".to_string(),
    ];
    warnings.push(aider_telemetry_note());
    if request.effective_context_tokens.is_none() {
        warnings.push(context_unknown_warning());
    }

    GeneratedAgentConfig {
        tool: AgentTool::Aider,
        filename: ".aider.conf.yml".to_string(),
        content: lines.join("\n") + "\n",
        warnings,
    }
}

fn generate_openai_env(request: &GenerateAgentConfigRequest) -> GeneratedAgentConfig {
    let mut warnings = Vec::new();
    let auth = auth_material(request.endpoint.auth_required, request.auth_token.as_deref());
    let api_key = auth_value(&auth, AgentTool::OpenAiEnv, &mut warnings);

    let mut lines = vec![
        "# Generic OpenAI SDK-compatible environment variables.".to_string(),
        "# Recognized by the official OpenAI Python/Node SDKs and many tools built on".to_string(),
        "# them. Some older tools instead read OPENAI_API_BASE — check your tool's docs.".to_string(),
    ];
    if let Some(tokens) = request.effective_context_tokens {
        lines.push(format!(
            "# Effective context window for this model/runtime right now: {tokens} tokens."
        ));
    } else {
        warnings.push(context_unknown_warning());
    }
    lines.push(format!("OPENAI_BASE_URL={}", request.endpoint.base_url));
    lines.push(format!("OPENAI_API_KEY={api_key}"));
    lines.push(format!("# Model id to select in your tool: {}", request.model_id));

    GeneratedAgentConfig {
        tool: AgentTool::OpenAiEnv,
        filename: ".env".to_string(),
        content: lines.join("\n") + "\n",
        warnings,
    }
}

/// Renders `request` into a real, working config snippet for `request.tool`.
pub fn generate_config(request: &GenerateAgentConfigRequest) -> GeneratedAgentConfig {
    match request.tool {
        AgentTool::ContinueDev => generate_continue(request),
        AgentTool::Aider => generate_aider(request),
        AgentTool::OpenAiEnv => generate_openai_env(request),
    }
}

/// Which `AdvancedSettingCapability` key (already exposed by
/// `runtime_adapter.rs`) holds a runtime's context-window setting. `Mlx`
/// does not currently expose a configurable context window.
pub fn context_setting_key(kind: M3RuntimeKind) -> Option<&'static str> {
    match kind {
        M3RuntimeKind::LlamaCpp => Some("context_size"),
        M3RuntimeKind::Ollama => Some("num_ctx"),
        M3RuntimeKind::Mlx => None,
    }
}

/// Resolves the effective context window (in tokens) a runtime currently
/// serves: the user's saved override if one exists, else the runtime's
/// advertised default. Returns `None` when the runtime has no configurable
/// context-window setting or its capability view is unavailable.
pub fn effective_context_tokens(
    capability: Option<&M3RuntimeCapabilityView>,
    stored: Option<&BTreeMap<String, SettingValue>>,
    kind: M3RuntimeKind,
) -> Option<u64> {
    let key = context_setting_key(kind)?;
    if let Some(SettingValue::Integer { value }) = stored.and_then(|values| values.get(key)) {
        return u64::try_from(*value).ok();
    }
    let capability = capability?;
    capability
        .settings
        .iter()
        .find(|setting| setting.key == key)
        .and_then(|setting| match setting.default_value {
            SettingValue::Integer { value } => u64::try_from(value).ok(),
            _ => None,
        })
}

/// A reasonable default model to preselect: the first chat-capable
/// installed model, else the first installed model of any kind.
pub fn pick_default_model(installed: &[M3InstalledModelView]) -> Option<&M3InstalledModelView> {
    installed
        .iter()
        .find(|model| model.capabilities.chat)
        .or_else(|| installed.first())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentLauncherError {
    Invalid(String),
}

impl std::fmt::Display for AgentLauncherError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentLauncherError::Invalid(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for AgentLauncherError {}

/// Real, current state to check a previously-generated or user-pasted
/// config against.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriftCheckInput {
    pub tool: AgentTool,
    pub pasted_config: String,
    pub current_endpoint: Option<AgentEndpointInfo>,
    pub installed_model_ids: BTreeSet<String>,
    pub effective_context_by_model: BTreeMap<String, u64>,
    pub auth_currently_required: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfigDriftReport {
    pub tool: AgentTool,
    pub parsed_model_id: Option<String>,
    pub parsed_base_url: Option<String>,
    pub parsed_context_tokens: Option<u64>,
    pub findings: Vec<AgentConfigWarning>,
}

fn check_model_drift(
    parsed_model: Option<&str>,
    installed: &BTreeSet<String>,
    findings: &mut Vec<AgentConfigWarning>,
) {
    if let Some(model) = parsed_model {
        if !installed.contains(model) {
            findings.push(AgentConfigWarning {
                kind: AgentWarningKind::ModelMissing,
                message: format!(
                    "This config references model '{model}', which is not currently installed. Reinstall it or regenerate the config with a currently installed model."
                ),
            });
        }
    }
}

fn check_endpoint_drift(
    parsed_base: Option<&str>,
    current: Option<&AgentEndpointInfo>,
    findings: &mut Vec<AgentConfigWarning>,
) {
    match (parsed_base, current) {
        (Some(parsed), Some(current)) if !parsed.eq_ignore_ascii_case(&current.base_url) => {
            findings.push(AgentConfigWarning {
                kind: AgentWarningKind::EndpointDrift,
                message: format!(
                    "This config points at {parsed} but the current local API endpoint is {}. Regenerate the config or update the URL/port.",
                    current.base_url
                ),
            });
        }
        (Some(_), None) => findings.push(AgentConfigWarning {
            kind: AgentWarningKind::EndpointDrift,
            message: "No local API listener is currently configured, so the endpoint in this config cannot be verified.".to_string(),
        }),
        _ => {}
    }
}

fn check_auth_drift(
    raw_value: Option<&str>,
    currently_required: bool,
    findings: &mut Vec<AgentConfigWarning>,
) {
    let trimmed = raw_value.map(str::trim).filter(|value| !value.is_empty());
    match trimmed {
        Some(AUTH_PLACEHOLDER_MARKER) => findings.push(AgentConfigWarning {
            kind: AgentWarningKind::Auth,
            message: "This config still has the placeholder token (REPLACE_WITH_PAIRED_TOKEN) — replace it with your real paired bearer token from Settings > Runtime Hub > LAN.".to_string(),
        }),
        Some(AUTH_NOT_REQUIRED_MARKER) if currently_required => findings.push(AgentConfigWarning {
            kind: AgentWarningKind::AuthDrift,
            message: "This config was generated when the server did not require authentication, but authentication is now required — add your real paired bearer token.".to_string(),
        }),
        None if currently_required => findings.push(AgentConfigWarning {
            kind: AgentWarningKind::AuthDrift,
            message: "The server currently requires a paired bearer token, but this config has no API key/token set.".to_string(),
        }),
        _ => {}
    }
}

fn check_context_drift(
    parsed_model: Option<&str>,
    parsed_context: Option<u64>,
    effective_by_model: &BTreeMap<String, u64>,
    findings: &mut Vec<AgentConfigWarning>,
) {
    let (Some(model), Some(declared)) = (parsed_model, parsed_context) else {
        return;
    };
    if let Some(effective) = effective_by_model.get(model) {
        if declared > *effective {
            findings.push(AgentConfigWarning {
                kind: AgentWarningKind::ContextLength,
                message: format!(
                    "This config declares a context length of {declared} tokens for '{model}', but the runtime currently only serves {effective} tokens of context for that model. Lower the declared context length or increase the runtime's context size."
                ),
            });
        }
    }
}

#[derive(Deserialize, Default)]
struct ContinueYamlDoc {
    #[serde(default)]
    models: Vec<ContinueYamlModel>,
}

#[derive(Deserialize, Default, Clone)]
struct ContinueYamlModel {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default, rename = "apiBase")]
    api_base: Option<String>,
    #[serde(default, rename = "apiKey")]
    api_key: Option<String>,
    #[serde(default, rename = "defaultCompletionOptions")]
    default_completion_options: Option<ContinueYamlCompletionOptions>,
}

#[derive(Deserialize, Default, Clone)]
struct ContinueYamlCompletionOptions {
    #[serde(default, rename = "contextLength")]
    context_length: Option<u64>,
}

fn detect_drift_continue(input: &DriftCheckInput) -> Result<AgentConfigDriftReport, AgentLauncherError> {
    let doc: ContinueYamlDoc = serde_saphyr::from_str(&input.pasted_config).map_err(|error| {
        AgentLauncherError::Invalid(format!("Could not parse as a Continue.dev config.yaml: {error}"))
    })?;
    let selected = doc
        .models
        .iter()
        .find(|model| model.provider.as_deref() == Some("openai") && model.api_base.is_some())
        .or_else(|| doc.models.first());

    let parsed_model = selected.and_then(|model| model.model.clone());
    let parsed_base = selected.and_then(|model| model.api_base.clone());
    let parsed_context = selected
        .and_then(|model| model.default_completion_options.as_ref())
        .and_then(|options| options.context_length);
    let parsed_api_key = selected.and_then(|model| model.api_key.clone());

    let mut findings = Vec::new();
    if selected.is_none() {
        findings.push(AgentConfigWarning {
            kind: AgentWarningKind::ModelMissing,
            message: "No models[] entry was found in this config.yaml.".to_string(),
        });
    }
    check_model_drift(parsed_model.as_deref(), &input.installed_model_ids, &mut findings);
    check_endpoint_drift(parsed_base.as_deref(), input.current_endpoint.as_ref(), &mut findings);
    check_auth_drift(parsed_api_key.as_deref(), input.auth_currently_required, &mut findings);
    check_context_drift(
        parsed_model.as_deref(),
        parsed_context,
        &input.effective_context_by_model,
        &mut findings,
    );

    Ok(AgentConfigDriftReport {
        tool: AgentTool::ContinueDev,
        parsed_model_id: parsed_model,
        parsed_base_url: parsed_base,
        parsed_context_tokens: parsed_context,
        findings,
    })
}

#[derive(Deserialize, Default)]
struct AiderYamlDoc {
    #[serde(default, rename = "openai-api-base")]
    openai_api_base: Option<String>,
    #[serde(default, rename = "openai-api-key")]
    openai_api_key: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default, rename = "analytics-disable")]
    analytics_disable: Option<bool>,
}

fn detect_drift_aider(input: &DriftCheckInput) -> Result<AgentConfigDriftReport, AgentLauncherError> {
    let doc: AiderYamlDoc = serde_saphyr::from_str(&input.pasted_config)
        .map_err(|error| AgentLauncherError::Invalid(format!("Could not parse as an .aider.conf.yml: {error}")))?;
    let parsed_model = doc
        .model
        .as_deref()
        .map(|model| model.strip_prefix("openai/").unwrap_or(model).to_string());

    let mut findings = Vec::new();
    check_model_drift(parsed_model.as_deref(), &input.installed_model_ids, &mut findings);
    check_endpoint_drift(
        doc.openai_api_base.as_deref(),
        input.current_endpoint.as_ref(),
        &mut findings,
    );
    check_auth_drift(doc.openai_api_key.as_deref(), input.auth_currently_required, &mut findings);
    if doc.analytics_disable != Some(true) {
        findings.push(AgentConfigWarning {
            kind: AgentWarningKind::Telemetry,
            message: "analytics-disable is not set to true in this config — aider's optional usage-analytics prompt may still trigger.".to_string(),
        });
    }

    Ok(AgentConfigDriftReport {
        tool: AgentTool::Aider,
        parsed_model_id: parsed_model,
        parsed_base_url: doc.openai_api_base,
        parsed_context_tokens: None,
        findings,
    })
}

/// A deliberately small `.env`-style line scanner (`KEY=VALUE`, `#` comments,
/// optional matching quotes) — this format has no real YAML/JSON structure
/// to parse.
fn parse_env_like(content: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let mut value = value.trim();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = &value[1..value.len() - 1];
        }
        values.insert(key.to_string(), value.to_string());
    }
    values
}

fn detect_drift_openai_env(input: &DriftCheckInput) -> Result<AgentConfigDriftReport, AgentLauncherError> {
    let values = parse_env_like(&input.pasted_config);
    if values.is_empty() {
        return Err(AgentLauncherError::Invalid(
            "Could not find any KEY=VALUE lines in this .env-style config".to_string(),
        ));
    }
    let parsed_base = values
        .get("OPENAI_BASE_URL")
        .or_else(|| values.get("OPENAI_API_BASE"))
        .cloned();
    let parsed_key = values.get("OPENAI_API_KEY").cloned();

    let mut findings = Vec::new();
    check_endpoint_drift(parsed_base.as_deref(), input.current_endpoint.as_ref(), &mut findings);
    check_auth_drift(parsed_key.as_deref(), input.auth_currently_required, &mut findings);

    Ok(AgentConfigDriftReport {
        tool: AgentTool::OpenAiEnv,
        parsed_model_id: None,
        parsed_base_url: parsed_base,
        parsed_context_tokens: None,
        findings,
    })
}

/// Checks `input.pasted_config` (a previously-generated or user-pasted
/// file) for drift against the app's current real state: a model that is
/// no longer installed, an endpoint that no longer matches the running
/// server, a missing now-required auth header, an oversized context
/// length, or a telemetry-sensitive default that was left enabled.
pub fn detect_drift(input: &DriftCheckInput) -> Result<AgentConfigDriftReport, AgentLauncherError> {
    if input.pasted_config.trim().is_empty() {
        return Err(AgentLauncherError::Invalid("Paste a config to check for drift".to_string()));
    }
    if input.pasted_config.len() > MAX_PASTED_CONFIG_BYTES {
        return Err(AgentLauncherError::Invalid(format!(
            "Pasted config exceeds the {MAX_PASTED_CONFIG_BYTES}-byte limit"
        )));
    }
    match input.tool {
        AgentTool::ContinueDev => detect_drift_continue(input),
        AgentTool::Aider => detect_drift_aider(input),
        AgentTool::OpenAiEnv => detect_drift_openai_env(input),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::m3_runtime_hub::{M3ModelCapabilities, M3RuntimeDescriptor};
    use crate::runtime_adapter::{AdvancedSettingCapability, SettingValueSchema};

    fn endpoint(auth_required: bool) -> AgentEndpointInfo {
        AgentEndpointInfo {
            base_url: "http://127.0.0.1:1234/v1".to_string(),
            loopback_only: true,
            auth_required,
            tls: false,
        }
    }

    #[test]
    fn resolve_endpoint_uses_real_bind_address_and_port() {
        let policy = LanServerPolicy {
            bind_address: "127.0.0.1".to_string(),
            port: 4321,
            require_authentication: false,
            pairing_required: false,
            tls: TlsPolicy::Disabled,
            cors_allowlist: Vec::new(),
            allowed_backends: Default::default(),
            allowed_lan_mutations: Default::default(),
            allow_cloud_providers_over_lan: false,
            rate_limit: crate::compatibility_hub::RateLimitPolicy {
                window_ms: 60_000,
                max_requests: 60,
                max_input_bytes: 1_000,
            },
            pairing_ttl_ms: 60_000,
        };
        let resolved = resolve_endpoint(&policy);
        assert_eq!(resolved.base_url, "http://127.0.0.1:4321/v1");
        assert!(resolved.loopback_only);
        assert!(!resolved.auth_required);
        assert!(!resolved.tls);
    }

    #[test]
    fn continue_config_has_expected_shape_with_real_token() {
        let request = GenerateAgentConfigRequest {
            tool: AgentTool::ContinueDev,
            endpoint: endpoint(true),
            model_id: "qwen2.5-coder-7b".to_string(),
            effective_context_tokens: Some(8192),
            auth_token: Some("paired-secret-token".to_string()),
        };
        let generated = generate_config(&request);
        assert_eq!(generated.filename, ".continue/config.yaml");
        let expected = concat!(
            "name: Little Monkey Local\n",
            "version: 1.0.0\n",
            "schema: v1\n",
            "models:\n",
            "  - name: qwen2.5-coder-7b\n",
            "    provider: openai\n",
            "    model: qwen2.5-coder-7b\n",
            "    apiBase: http://127.0.0.1:1234/v1\n",
            "    apiKey: paired-secret-token\n",
            "    roles:\n",
            "      - chat\n",
            "      - edit\n",
            "      - apply\n",
            "    defaultCompletionOptions:\n",
            "      contextLength: 8192\n",
        );
        assert_eq!(generated.content, expected);
        assert!(generated.warnings.iter().any(|warning| warning.kind == AgentWarningKind::Telemetry));
        assert!(!generated.warnings.iter().any(|warning| warning.kind == AgentWarningKind::Auth));
    }

    #[test]
    fn continue_config_without_token_gets_placeholder_and_auth_warning() {
        let request = GenerateAgentConfigRequest {
            tool: AgentTool::ContinueDev,
            endpoint: endpoint(true),
            model_id: "qwen2.5-coder-7b".to_string(),
            effective_context_tokens: None,
            auth_token: None,
        };
        let generated = generate_config(&request);
        assert!(generated.content.contains("apiKey: REPLACE_WITH_PAIRED_TOKEN"));
        assert!(!generated.content.contains("defaultCompletionOptions"));
        assert!(generated.warnings.iter().any(|warning| warning.kind == AgentWarningKind::Auth));
        assert!(generated.warnings.iter().any(|warning| warning.kind == AgentWarningKind::ContextLength));
    }

    #[test]
    fn continue_config_without_auth_required_uses_not_required_marker() {
        let request = GenerateAgentConfigRequest {
            tool: AgentTool::ContinueDev,
            endpoint: endpoint(false),
            model_id: "phi-4-mini".to_string(),
            effective_context_tokens: Some(4096),
            auth_token: None,
        };
        let generated = generate_config(&request);
        assert!(generated.content.contains("apiKey: not-required-loopback-only"));
        assert!(!generated.warnings.iter().any(|warning| warning.kind == AgentWarningKind::Auth));
    }

    #[test]
    fn aider_config_has_expected_shape() {
        let request = GenerateAgentConfigRequest {
            tool: AgentTool::Aider,
            endpoint: endpoint(true),
            model_id: "qwen2.5-coder-7b".to_string(),
            effective_context_tokens: Some(8192),
            auth_token: Some("paired-secret-token".to_string()),
        };
        let generated = generate_config(&request);
        assert_eq!(generated.filename, ".aider.conf.yml");
        let expected = "openai-api-base: http://127.0.0.1:1234/v1\n\
openai-api-key: paired-secret-token\n\
model: openai/qwen2.5-coder-7b\n\
analytics-disable: true\n";
        assert_eq!(generated.content, expected);
        assert!(generated.warnings.iter().any(|warning| warning.kind == AgentWarningKind::Telemetry));
    }

    #[test]
    fn openai_env_config_has_expected_shape() {
        let request = GenerateAgentConfigRequest {
            tool: AgentTool::OpenAiEnv,
            endpoint: endpoint(true),
            model_id: "qwen2.5-coder-7b".to_string(),
            effective_context_tokens: Some(8192),
            auth_token: Some("paired-secret-token".to_string()),
        };
        let generated = generate_config(&request);
        assert_eq!(generated.filename, ".env");
        assert!(generated.content.contains("OPENAI_BASE_URL=http://127.0.0.1:1234/v1"));
        assert!(generated.content.contains("OPENAI_API_KEY=paired-secret-token"));
        assert!(generated.content.contains("# Model id to select in your tool: qwen2.5-coder-7b"));
        assert!(!generated.warnings.iter().any(|warning| warning.kind == AgentWarningKind::Telemetry));
    }

    #[test]
    fn effective_context_tokens_prefers_stored_override_over_default() {
        let capability = M3RuntimeCapabilityView {
            descriptor: M3RuntimeDescriptor {
                runtime_id: "llama-cpp".to_string(),
                kind: M3RuntimeKind::LlamaCpp,
                label: "llama.cpp".to_string(),
                managed: true,
                api_backend: api_backend_managed_local(),
            },
            can_load: true,
            can_unload: true,
            can_logs: true,
            can_metrics: true,
            can_infer: true,
            settings: vec![AdvancedSettingCapability {
                key: "context_size".to_string(),
                label: "Context size".to_string(),
                description: "llama-server context window.".to_string(),
                schema: SettingValueSchema::Integer { min: 128, max: 1_048_576, step: 1 },
                default_value: SettingValue::Integer { value: 4_096 },
                restart_required: true,
            }],
        };
        let mut stored = BTreeMap::new();
        stored.insert("context_size".to_string(), SettingValue::Integer { value: 32_768 });

        assert_eq!(
            effective_context_tokens(Some(&capability), Some(&stored), M3RuntimeKind::LlamaCpp),
            Some(32_768)
        );
        assert_eq!(
            effective_context_tokens(Some(&capability), None, M3RuntimeKind::LlamaCpp),
            Some(4_096)
        );
        assert_eq!(effective_context_tokens(None, None, M3RuntimeKind::Mlx), None);
    }

    #[test]
    fn pick_default_model_prefers_chat_capable() {
        fn model(model_id: &str, chat: bool) -> M3InstalledModelView {
            M3InstalledModelView {
                asset_id: format!("llama_cpp:{model_id}:default"),
                model_id: model_id.to_string(),
                display_name: model_id.to_string(),
                runtime: M3RuntimeKind::LlamaCpp,
                variant_id: "default".to_string(),
                capabilities: M3ModelCapabilities {
                    chat,
                    embeddings: false,
                    tool_calling: false,
                    vision: false,
                    structured_output: false,
                },
                estimated_ram_bytes: 0,
                estimated_vram_bytes: 0,
                required_accelerator: None,
                active_version_key: "v1".to_string(),
                versions: Vec::new(),
            }
        }
        let installed = vec![model("embedder", false), model("chat-model", true)];
        assert_eq!(pick_default_model(&installed).map(|m| m.model_id.as_str()), Some("chat-model"));
        assert_eq!(pick_default_model(&[]), None);
    }

    fn drift_input(tool: AgentTool, pasted_config: &str) -> DriftCheckInput {
        let mut installed_model_ids = BTreeSet::new();
        installed_model_ids.insert("qwen2.5-coder-7b".to_string());
        let mut effective_context_by_model = BTreeMap::new();
        effective_context_by_model.insert("qwen2.5-coder-7b".to_string(), 8192);
        DriftCheckInput {
            tool,
            pasted_config: pasted_config.to_string(),
            current_endpoint: Some(endpoint(true)),
            installed_model_ids,
            effective_context_by_model,
            auth_currently_required: true,
        }
    }

    #[test]
    fn detect_drift_continue_clean_config_has_no_findings() {
        let pasted = "name: Little Monkey Local\nversion: 1.0.0\nschema: v1\nmodels:\n  - name: qwen2.5-coder-7b\n    provider: openai\n    model: qwen2.5-coder-7b\n    apiBase: http://127.0.0.1:1234/v1\n    apiKey: real-token\n    defaultCompletionOptions:\n      contextLength: 4096\n";
        let report = detect_drift(&drift_input(AgentTool::ContinueDev, pasted)).expect("parses");
        assert_eq!(report.parsed_model_id.as_deref(), Some("qwen2.5-coder-7b"));
        assert_eq!(report.parsed_base_url.as_deref(), Some("http://127.0.0.1:1234/v1"));
        assert_eq!(report.parsed_context_tokens, Some(4096));
        assert!(report.findings.is_empty(), "unexpected findings: {:?}", report.findings);
    }

    #[test]
    fn detect_drift_continue_flags_missing_model_stale_endpoint_and_context() {
        let pasted = "models:\n  - name: retired-model\n    provider: openai\n    model: retired-model\n    apiBase: http://127.0.0.1:9999/v1\n    apiKey: REPLACE_WITH_PAIRED_TOKEN\n    defaultCompletionOptions:\n      contextLength: 999999\n";
        let mut input = drift_input(AgentTool::ContinueDev, pasted);
        input.effective_context_by_model.insert("retired-model".to_string(), 4096);
        let report = detect_drift(&input).expect("parses");
        let kinds: Vec<_> = report.findings.iter().map(|finding| finding.kind).collect();
        assert!(kinds.contains(&AgentWarningKind::ModelMissing));
        assert!(kinds.contains(&AgentWarningKind::EndpointDrift));
        assert!(kinds.contains(&AgentWarningKind::ContextLength));
        assert!(kinds.contains(&AgentWarningKind::Auth));
    }

    #[test]
    fn detect_drift_continue_flags_auth_drift_when_now_required() {
        let pasted = "models:\n  - name: qwen2.5-coder-7b\n    provider: openai\n    model: qwen2.5-coder-7b\n    apiBase: http://127.0.0.1:1234/v1\n    apiKey: not-required-loopback-only\n";
        let report = detect_drift(&drift_input(AgentTool::ContinueDev, pasted)).expect("parses");
        assert!(report.findings.iter().any(|finding| finding.kind == AgentWarningKind::AuthDrift));
    }

    #[test]
    fn detect_drift_continue_rejects_oversized_input() {
        let huge = "a".repeat(MAX_PASTED_CONFIG_BYTES + 1);
        let error = detect_drift(&drift_input(AgentTool::ContinueDev, &huge)).unwrap_err();
        assert!(error.to_string().contains("exceeds"));
    }

    #[test]
    fn detect_drift_continue_rejects_empty_input() {
        let error = detect_drift(&drift_input(AgentTool::ContinueDev, "   ")).unwrap_err();
        assert!(error.to_string().contains("Paste a config"));
    }

    #[test]
    fn detect_drift_aider_flags_missing_analytics_disable() {
        let pasted = "openai-api-base: http://127.0.0.1:1234/v1\nopenai-api-key: real-token\nmodel: openai/qwen2.5-coder-7b\n";
        let report = detect_drift(&drift_input(AgentTool::Aider, pasted)).expect("parses");
        assert_eq!(report.parsed_model_id.as_deref(), Some("qwen2.5-coder-7b"));
        assert!(report.findings.iter().any(|finding| finding.kind == AgentWarningKind::Telemetry));
    }

    #[test]
    fn detect_drift_aider_strips_openai_prefix_and_finds_no_drift() {
        let pasted = "openai-api-base: http://127.0.0.1:1234/v1\nopenai-api-key: real-token\nmodel: openai/qwen2.5-coder-7b\nanalytics-disable: true\n";
        let report = detect_drift(&drift_input(AgentTool::Aider, pasted)).expect("parses");
        assert!(!report.findings.iter().any(|finding| finding.kind == AgentWarningKind::ModelMissing));
        assert!(!report.findings.iter().any(|finding| finding.kind == AgentWarningKind::EndpointDrift));
    }

    #[test]
    fn detect_drift_openai_env_flags_stale_endpoint() {
        let pasted = "OPENAI_BASE_URL=http://127.0.0.1:9999/v1\nOPENAI_API_KEY=real-token\n";
        let report = detect_drift(&drift_input(AgentTool::OpenAiEnv, pasted)).expect("parses");
        assert!(report.findings.iter().any(|finding| finding.kind == AgentWarningKind::EndpointDrift));
    }

    #[test]
    fn detect_drift_openai_env_accepts_legacy_api_base_name() {
        let pasted = "OPENAI_API_BASE=http://127.0.0.1:1234/v1\nOPENAI_API_KEY=real-token\n";
        let report = detect_drift(&drift_input(AgentTool::OpenAiEnv, pasted)).expect("parses");
        assert_eq!(report.parsed_base_url.as_deref(), Some("http://127.0.0.1:1234/v1"));
        assert!(!report.findings.iter().any(|finding| finding.kind == AgentWarningKind::EndpointDrift));
    }

    #[test]
    fn detect_drift_openai_env_rejects_content_with_no_assignments() {
        let error = detect_drift(&drift_input(AgentTool::OpenAiEnv, "# just a comment\n")).unwrap_err();
        assert!(error.to_string().contains("KEY=VALUE"));
    }

    #[test]
    fn detect_drift_continue_rejects_invalid_yaml() {
        let error = detect_drift(&drift_input(AgentTool::ContinueDev, "not: [valid: yaml")).unwrap_err();
        assert!(error.to_string().contains("Could not parse"));
    }

    fn api_backend_managed_local() -> crate::compatibility_hub::ApiBackend {
        crate::compatibility_hub::ApiBackend::ManagedLocal
    }
}
