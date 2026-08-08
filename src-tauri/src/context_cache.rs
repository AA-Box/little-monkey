//! Context window and KV-cache observability for loaded models (Phase 8,
//! "Context and KV Cache Control Center").
//!
//! This module is intentionally Tauri-free and side-effect-light so its
//! logic is unit-testable with fixtures instead of a live long-context
//! runtime session:
//!
//! - [`resolve_configured_context`] derives the context size that will be
//!   (or was) requested for a load from the runtime's own persisted
//!   settings/capability schema — never a guess, always traceable to a
//!   concrete setting key and value.
//! - [`fetch_llama_cpp_live_context_state`] best-effort queries a managed
//!   `llama-server` process's `/props` and `/slots` HTTP endpoints (reusing
//!   [`crate::runtime_adapter`]'s existing [`HttpTransport`] abstraction) for
//!   whatever it actually reports. Any endpoint that is absent, disabled, or
//!   returns an unexpected shape degrades to `None` fields rather than a
//!   fabricated value — [`parse_llama_props_body`] and
//!   [`parse_llama_slots_body`] are pure and fixture-tested.
//! - [`classify_context_failure`] turns a runtime error/response signal into
//!   one of the five acceptance-criterion categories (prompt too long, cache
//!   exhausted/context shift, memory pressure, runtime limitation, model
//!   metadata limit) with a plain-language explanation, or `None` when there
//!   is no real signal — it never forces a classification onto an unrelated
//!   failure.
//! - [`resolve_effective_context`] exposes a safe user control for the
//!   effective context size of a load: it starts from the caller's requested
//!   value and clamps it using the [`crate::runtime_adapter::LocalOffloadPlanner`]
//!   output the caller already computed (memory-aware), the model's own
//!   metadata max context when known, and the runtime's configured
//!   setting bounds — it never bypasses those bounds, only tightens them
//!   further and explains why.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::runtime_adapter::{
    AdvancedSettingCapability, EndpointOrigin, HttpMethod, HttpRequest, HttpTransport, SettingValue,
};

/// Where a configured-context figure came from, so the UI never presents an
/// estimate as if it were a guaranteed runtime fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextLimitSource {
    /// An explicit, persisted setting value will be (or was) sent to the
    /// runtime at load time.
    RuntimeConfigured,
    /// No setting was persisted; the runtime's own advertised default will
    /// apply.
    RuntimeDefault,
    /// This runtime has no configurable context-window setting at all (for
    /// example, this app's MLX runtime today has no context-length control).
    Unavailable,
}

/// The context size that will be (or was) requested for a runtime load,
/// resolved purely from that runtime's own capability schema and any
/// persisted configuration — never fabricated.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfiguredContext {
    pub tokens: Option<u32>,
    pub source: ContextLimitSource,
    /// The advanced-setting key this value came from (e.g. `"context_size"`
    /// for llama.cpp, `"num_ctx"` for Ollama), for transparency in the UI.
    pub setting_key: Option<String>,
}

/// Scans `settings_schema` for the first of `key_candidates` present, then
/// prefers a persisted value for that key over the schema's own default.
/// Returns [`ContextLimitSource::Unavailable`] when the runtime advertises no
/// matching setting at all (e.g. MLX, which has no context-length control).
pub fn resolve_configured_context(
    settings_schema: &[AdvancedSettingCapability],
    persisted: Option<&BTreeMap<String, SettingValue>>,
    key_candidates: &[&str],
) -> ConfiguredContext {
    for key in key_candidates {
        let Some(capability) = settings_schema.iter().find(|entry| entry.key == *key) else {
            continue;
        };
        if let Some(SettingValue::Integer { value }) = persisted.and_then(|map| map.get(*key)) {
            if let Ok(tokens) = u32::try_from(*value) {
                return ConfiguredContext {
                    tokens: Some(tokens),
                    source: ContextLimitSource::RuntimeConfigured,
                    setting_key: Some((*key).to_string()),
                };
            }
        }
        if let SettingValue::Integer { value } = &capability.default_value {
            if let Ok(tokens) = u32::try_from(*value) {
                return ConfiguredContext {
                    tokens: Some(tokens),
                    source: ContextLimitSource::RuntimeDefault,
                    setting_key: Some((*key).to_string()),
                };
            }
        }
    }
    ConfiguredContext {
        tokens: None,
        source: ContextLimitSource::Unavailable,
        setting_key: None,
    }
}

/// Tokens of headroom left in the context window, or `None` when either
/// figure is unknown (never guessed).
pub fn context_headroom(configured_tokens: Option<u32>, tokens_in_use: Option<u32>) -> Option<u32> {
    match (configured_tokens, tokens_in_use) {
        (Some(configured), Some(used)) => Some(configured.saturating_sub(used)),
        _ => None,
    }
}

// ---------------------------------------------------------------------
// llama.cpp live query (best-effort, tolerant, never fabricated)
// ---------------------------------------------------------------------

/// A single `llama-server` `/slots` entry, tolerantly parsed. Every field is
/// `None` when the response didn't contain it — this app does not require
/// `--slots` to be enabled and treats it as absent rather than an error.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LlamaCppSlotState {
    pub id: Option<u64>,
    pub context_tokens: Option<u32>,
    pub tokens_in_use: Option<u32>,
    /// `true` when this slot's prompt cache was truncated/rolled (llama.cpp's
    /// "context shift"), `None` when the runtime's response didn't include a
    /// truncation flag at all.
    pub context_shifted: Option<bool>,
}

/// Best-effort live state read directly from a managed `llama-server`
/// process. Populated opportunistically from whichever of `/props` and
/// `/slots` responded with a recognizable shape; absent data is left `None`/
/// empty rather than guessed.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LlamaCppLiveContextState {
    /// The context size llama-server itself reports it was started with
    /// (from `/props`'s `default_generation_settings.n_ctx`), independent of
    /// what this app believes it asked for.
    pub reported_context_tokens: Option<u32>,
    pub total_slots: Option<u32>,
    pub slots: Vec<LlamaCppSlotState>,
    /// Which endpoints actually answered with a 2xx and a recognizable body,
    /// for transparency about what "live" means here.
    pub endpoints_reachable: Vec<String>,
}

fn as_u32(value: Option<&Value>) -> Option<u32> {
    value.and_then(Value::as_u64).and_then(|value| u32::try_from(value).ok())
}

/// Parses a `llama-server` `GET /props` response body. Returns
/// `(reported_context_tokens, total_slots)`; either (or both) may be `None`
/// if the body is malformed or simply doesn't contain that field, which is
/// treated as "unavailable", not an error.
pub fn parse_llama_props_body(body: &[u8]) -> (Option<u32>, Option<u32>) {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return (None, None);
    };
    let generation_settings = value.get("default_generation_settings");
    let n_ctx = generation_settings
        .and_then(|settings| as_u32(settings.get("n_ctx")))
        .or_else(|| as_u32(value.get("n_ctx")));
    let total_slots = as_u32(value.get("total_slots"));
    (n_ctx, total_slots)
}

/// Parses a `llama-server` `GET /slots` response body (a JSON array). Returns
/// `None` only when the body isn't a JSON array at all (e.g. the endpoint is
/// disabled and returned an error page) — individual slot fields still
/// degrade to `None` independently when absent.
pub fn parse_llama_slots_body(body: &[u8]) -> Option<Vec<LlamaCppSlotState>> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let entries = value.as_array()?;
    Some(
        entries
            .iter()
            .map(|entry| {
                let params = entry.get("params");
                let context_tokens =
                    as_u32(entry.get("n_ctx")).or_else(|| params.and_then(|params| as_u32(params.get("n_ctx"))));
                let tokens_in_use = as_u32(entry.get("n_past"))
                    .or_else(|| as_u32(entry.get("tokens_evaluated")))
                    .or_else(|| as_u32(entry.get("n_ctx_used")));
                let context_shifted = entry
                    .get("truncated")
                    .and_then(Value::as_bool)
                    .or_else(|| entry.get("context_shifted").and_then(Value::as_bool));
                LlamaCppSlotState {
                    id: entry.get("id").and_then(Value::as_u64),
                    context_tokens,
                    tokens_in_use,
                    context_shifted,
                }
            })
            .collect(),
    )
}

const LLAMA_CPP_RESPONSE_TIMEOUT_MS: u64 = 5_000;
const LLAMA_CPP_MAX_RESPONSE_BYTES: usize = 512 * 1024;

/// Best-effort live query of a managed `llama-server`'s `/props` and
/// `/slots` endpoints. Never returns an error: any transport failure, non-2xx
/// status, or unrecognized body simply leaves the corresponding fields
/// `None`/empty, since a `/slots` endpoint that does not answer is an expected,
/// not exceptional, outcome — a server started with `--no-slots`, or one this app
/// does not manage. (The doc here used to say `--slots` was off by default; it is
/// *on* by default in the `llama.cpp` build
/// [`crate::managed_runtime::MANAGED_LLAMA_VERSION`] pins, which was checked
/// against that binary's own `--help` rather than assumed.)
pub async fn fetch_llama_cpp_live_context_state(
    endpoint: &EndpointOrigin,
    transport: &dyn HttpTransport,
    cancellation: &CancellationToken,
) -> LlamaCppLiveContextState {
    let mut state = LlamaCppLiveContextState::default();

    if let Ok(url) = endpoint.url("/props") {
        let request = HttpRequest {
            method: HttpMethod::Get,
            url,
            content_type: None,
            body: None,
            timeout_ms: LLAMA_CPP_RESPONSE_TIMEOUT_MS,
            max_response_bytes: LLAMA_CPP_MAX_RESPONSE_BYTES,
        };
        if let Ok(response) = transport.execute(request, cancellation).await {
            if (200..300).contains(&response.status) {
                let (n_ctx, total_slots) = parse_llama_props_body(&response.body);
                if n_ctx.is_some() || total_slots.is_some() {
                    state.endpoints_reachable.push("/props".to_string());
                }
                state.reported_context_tokens = n_ctx;
                state.total_slots = total_slots;
            }
        }
    }

    if let Ok(url) = endpoint.url("/slots") {
        let request = HttpRequest {
            method: HttpMethod::Get,
            url,
            content_type: None,
            body: None,
            timeout_ms: LLAMA_CPP_RESPONSE_TIMEOUT_MS,
            max_response_bytes: LLAMA_CPP_MAX_RESPONSE_BYTES,
        };
        if let Ok(response) = transport.execute(request, cancellation).await {
            if (200..300).contains(&response.status) {
                if let Some(slots) = parse_llama_slots_body(&response.body) {
                    state.endpoints_reachable.push("/slots".to_string());
                    state.slots = slots;
                }
            }
        }
    }

    state
}

// ---------------------------------------------------------------------
// Long-context failure classification (the hard acceptance requirement)
// ---------------------------------------------------------------------

/// The five failure categories required by the Phase 8 acceptance criterion:
/// a long-context failure must explain whether the limit was the prompt, the
/// cache, memory, the runtime, or the model's own metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextFailureClass {
    PromptTooLong,
    CacheExhaustedContextShift,
    MemoryPressure,
    RuntimeLimitation,
    ModelMetadataLimit,
}

impl ContextFailureClass {
    pub fn slug(self) -> &'static str {
        match self {
            Self::PromptTooLong => "prompt_too_long",
            Self::CacheExhaustedContextShift => "cache_exhausted_context_shift",
            Self::MemoryPressure => "memory_pressure",
            Self::RuntimeLimitation => "runtime_limitation",
            Self::ModelMetadataLimit => "model_metadata_limit",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextFailureClassification {
    pub class: ContextFailureClass,
    pub explanation: String,
    pub evidence: Vec<String>,
}

/// Every field is optional: callers supply whatever they actually know (a
/// raw error string, an HTTP status, live context-cache numbers, offload
/// planner memory figures, model metadata) and get back the best
/// classification the available evidence supports, or `None` when nothing
/// points to a context/cache-related cause at all.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextFailureInput {
    pub error_text: Option<String>,
    pub http_status: Option<u16>,
    pub configured_context_tokens: Option<u32>,
    pub requested_context_tokens: Option<u32>,
    pub model_metadata_max_context_tokens: Option<u32>,
    pub prompt_tokens: Option<u64>,
    /// `Some(true)` when the caller already knows (e.g. from a llama.cpp
    /// `/slots` `truncated` flag) that the context was shifted/rolled for
    /// this generation.
    pub context_shift_signal: Option<bool>,
    pub available_ram_bytes: Option<u64>,
    pub required_ram_bytes: Option<u64>,
    pub available_vram_bytes: Option<u64>,
    pub required_vram_bytes: Option<u64>,
    /// `Some(false)` when the caller already knows this runtime/backend
    /// cannot honor the requested context configuration at all.
    pub runtime_supports_context_control: Option<bool>,
}

fn contains_any(haystack: &str, needles: &'static [&'static str]) -> Option<&'static str> {
    needles.iter().find(|needle| haystack.contains(**needle)).copied()
}

const CONTEXT_SHIFT_PHRASES: &[&str] = &[
    "context shift",
    "shifted the context",
    "shifting the context",
    "context has been truncated",
    "dropping the oldest",
    "dropping earliest",
    "kv cache is full",
    "cache slot reused",
];

const PROMPT_TOO_LONG_PHRASES: &[&str] = &[
    "prompt is too long",
    "input is too long",
    "exceeds the available context",
    "exceeds context",
    "context length exceeded",
    "context length is exceeded",
    "please reduce the length",
    "prompt exceeds context",
    "too many tokens in the prompt",
    "try increasing it",
];

const MEMORY_PRESSURE_PHRASES: &[&str] = &[
    "out of memory",
    "failed to allocate",
    "cudamalloc",
    "cuda out of memory",
    "insufficient memory",
    "insufficient vram",
    "not enough memory",
    "cannot allocate memory",
    "ggml_backend_alloc",
    "ggml_gallocr",
];

const RUNTIME_LIMITATION_PHRASES: &[&str] = &[
    "unsupported parameter",
    "not supported",
    "unknown parameter",
    "unrecognized argument",
    "unrecognized option",
    "does not support",
    "invalid option",
    "feature not available",
];

/// Classifies a plausibly context/cache-related failure or degradation into
/// one of [`ContextFailureClass`]'s five categories, or returns `None` when
/// the available evidence gives no reason to believe context/cache/memory
/// was the cause (so callers should leave an unrelated error message
/// untouched rather than force a misleading label onto it).
pub fn classify_context_failure(input: &ContextFailureInput) -> Option<ContextFailureClassification> {
    let lower = input.error_text.as_deref().map(str::to_lowercase).unwrap_or_default();

    // 1. Cache exhausted / context shift: the conversation outgrew the
    //    window and the runtime rolled it, with or without a hard error.
    if input.context_shift_signal == Some(true) {
        let mut evidence = vec!["the runtime reported a context-shift/truncation flag".to_string()];
        if let Some(phrase) = contains_any(&lower, CONTEXT_SHIFT_PHRASES) {
            evidence.push(format!("error text matched \"{phrase}\""));
        }
        return Some(ContextFailureClassification {
            class: ContextFailureClass::CacheExhaustedContextShift,
            explanation: context_shift_explanation(input.configured_context_tokens),
            evidence,
        });
    }
    if let Some(phrase) = contains_any(&lower, CONTEXT_SHIFT_PHRASES) {
        return Some(ContextFailureClassification {
            class: ContextFailureClass::CacheExhaustedContextShift,
            explanation: context_shift_explanation(input.configured_context_tokens),
            evidence: vec![format!("error text matched \"{phrase}\"")],
        });
    }

    // 2. Prompt too long: the prompt alone is at or beyond the configured
    //    window, before any generation could even begin.
    let prompt_exceeds_numeric = matches!(
        (input.prompt_tokens, input.configured_context_tokens),
        (Some(prompt), Some(configured)) if prompt >= u64::from(configured)
    );
    if prompt_exceeds_numeric {
        return Some(ContextFailureClassification {
            class: ContextFailureClass::PromptTooLong,
            explanation: prompt_too_long_explanation(input.prompt_tokens, input.configured_context_tokens),
            evidence: vec![format!(
                "prompt is {} tokens, at or beyond the configured {} token context window",
                input.prompt_tokens.unwrap_or_default(),
                input.configured_context_tokens.unwrap_or_default()
            )],
        });
    }
    if let Some(phrase) = contains_any(&lower, PROMPT_TOO_LONG_PHRASES) {
        return Some(ContextFailureClassification {
            class: ContextFailureClass::PromptTooLong,
            explanation: prompt_too_long_explanation(input.prompt_tokens, input.configured_context_tokens),
            evidence: vec![format!("error text matched \"{phrase}\"")],
        });
    }

    // 3. Memory pressure: not enough host RAM/VRAM for the configured
    //    context alongside the model weights.
    let memory_insufficient = matches!(
        (input.available_ram_bytes, input.required_ram_bytes),
        (Some(available), Some(required)) if available < required
    ) || matches!(
        (input.available_vram_bytes, input.required_vram_bytes),
        (Some(available), Some(required)) if available < required
    );
    if memory_insufficient {
        return Some(ContextFailureClassification {
            class: ContextFailureClass::MemoryPressure,
            explanation: memory_pressure_explanation(
                input.available_ram_bytes,
                input.required_ram_bytes,
                input.available_vram_bytes,
                input.required_vram_bytes,
            ),
            evidence: vec!["available memory is below what the configured context requires".to_string()],
        });
    }
    if let Some(phrase) = contains_any(&lower, MEMORY_PRESSURE_PHRASES) {
        return Some(ContextFailureClassification {
            class: ContextFailureClass::MemoryPressure,
            explanation: memory_pressure_explanation(
                input.available_ram_bytes,
                input.required_ram_bytes,
                input.available_vram_bytes,
                input.required_vram_bytes,
            ),
            evidence: vec![format!("error text matched \"{phrase}\"")],
        });
    }

    // 4. Model metadata limit: the model's own advertised max context is
    //    smaller than what was requested.
    if let (Some(requested), Some(model_max)) =
        (input.requested_context_tokens, input.model_metadata_max_context_tokens)
    {
        if requested > model_max {
            return Some(ContextFailureClassification {
                class: ContextFailureClass::ModelMetadataLimit,
                explanation: format!(
                    "The model's own metadata reports a maximum context of {model_max} tokens, smaller than the requested {requested} tokens. Choose a model with a larger native context or reduce the requested context size."
                ),
                evidence: vec![format!(
                    "requested {requested} tokens exceeds the model's advertised maximum of {model_max} tokens"
                )],
            });
        }
    }

    // 5. Runtime limitation: the backend simply cannot honor the requested
    //    context configuration at all.
    if input.runtime_supports_context_control == Some(false) {
        return Some(ContextFailureClassification {
            class: ContextFailureClass::RuntimeLimitation,
            explanation: "This runtime does not expose a configurable context window at all, so this limit comes from the runtime itself, not from your settings. Switch to a runtime that supports adjusting the context size if you need a larger window.".to_string(),
            evidence: vec!["the runtime was reported as not supporting context-size control".to_string()],
        });
    }
    if let Some(phrase) = contains_any(&lower, RUNTIME_LIMITATION_PHRASES) {
        return Some(ContextFailureClassification {
            class: ContextFailureClass::RuntimeLimitation,
            explanation: "This runtime/backend rejected the requested context configuration as unsupported. Lower the requested context size or switch to a runtime that supports it.".to_string(),
            evidence: vec![format!("error text matched \"{phrase}\"")],
        });
    }

    None
}

fn context_shift_explanation(configured_context_tokens: Option<u32>) -> String {
    match configured_context_tokens {
        Some(tokens) => format!(
            "The conversation grew past the configured {tokens}-token context window, so the runtime rolled/shifted its context (dropping earlier turns) to keep generating. Recent replies may no longer reflect the earliest parts of the conversation. Increase the context size or start a new conversation to avoid this."
        ),
        None => "The conversation grew past the context window, so the runtime rolled/shifted its context (dropping earlier turns) to keep generating. Recent replies may no longer reflect the earliest parts of the conversation. Increase the context size or start a new conversation to avoid this.".to_string(),
    }
}

fn prompt_too_long_explanation(prompt_tokens: Option<u64>, configured_context_tokens: Option<u32>) -> String {
    match (prompt_tokens, configured_context_tokens) {
        (Some(prompt), Some(configured)) => format!(
            "The prompt itself is about {prompt} tokens, at or beyond the {configured}-token configured context window. Shorten the prompt or increase the context size for this model."
        ),
        _ => "The prompt itself appears to exceed the model's context window before any reply could be generated. Shorten the prompt or increase the context size for this model.".to_string(),
    }
}

fn memory_pressure_explanation(
    available_ram_bytes: Option<u64>,
    required_ram_bytes: Option<u64>,
    available_vram_bytes: Option<u64>,
    required_vram_bytes: Option<u64>,
) -> String {
    let ram = match (available_ram_bytes, required_ram_bytes) {
        (Some(available), Some(required)) => {
            format!(" RAM: needed ~{required} bytes, available ~{available} bytes.")
        }
        _ => String::new(),
    };
    let vram = match (available_vram_bytes, required_vram_bytes) {
        (Some(available), Some(required)) => {
            format!(" VRAM: needed ~{required} bytes, available ~{available} bytes.")
        }
        _ => String::new(),
    };
    format!(
        "The host does not have enough memory to hold the configured context window alongside the model weights.{ram}{vram} Lower the context size or free memory by unloading other models."
    )
}

// ---------------------------------------------------------------------
// Safe user controls: effective context size
// ---------------------------------------------------------------------

/// Input for [`resolve_effective_context`]. `offload_plan_context_tokens`
/// must already come from a real [`crate::runtime_adapter::LocalOffloadPlanner::plan`]
/// call — this function only tightens that bound further, it never
/// recomputes or bypasses the planner's own memory accounting.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveContextInput {
    pub requested_tokens: u32,
    pub offload_plan_context_tokens: u32,
    pub model_metadata_max_context_tokens: Option<u32>,
    pub runtime_setting_min_tokens: Option<u32>,
    pub runtime_setting_max_tokens: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveContextResolution {
    pub effective_tokens: u32,
    pub capped_by: Vec<String>,
    pub rationale: Vec<String>,
}

/// Resolves the effective context size for a load: starts at the requested
/// value and clamps it down to whichever of the offload plan's
/// memory-aware bound, the model's own metadata max, and the runtime's
/// configured setting bounds is smallest — explaining every reduction.
pub fn resolve_effective_context(input: &EffectiveContextInput) -> EffectiveContextResolution {
    let mut tokens = input.requested_tokens.max(1);
    let mut capped_by = Vec::new();
    let mut rationale = Vec::new();

    if input.offload_plan_context_tokens < tokens {
        tokens = input.offload_plan_context_tokens;
        capped_by.push("offload_plan".to_string());
        rationale.push(format!(
            "Reduced to {tokens} tokens by the adaptive offload plan, which accounts for available RAM/VRAM alongside the model weights."
        ));
    }
    if let Some(model_max) = input.model_metadata_max_context_tokens {
        if model_max < tokens {
            tokens = model_max;
            capped_by.push("model_metadata".to_string());
            rationale.push(format!(
                "Reduced to {tokens} tokens because the model's own metadata does not support a larger context window."
            ));
        }
    }
    if let Some(max) = input.runtime_setting_max_tokens {
        if max < tokens {
            tokens = max;
            capped_by.push("runtime_setting_max".to_string());
            rationale.push(format!("Reduced to {tokens} tokens, the runtime's configured maximum."));
        }
    }
    if let Some(min) = input.runtime_setting_min_tokens {
        if min > tokens {
            tokens = min;
            capped_by.push("runtime_setting_min".to_string());
            rationale.push(format!("Raised to {tokens} tokens, the runtime's configured minimum."));
        }
    }
    if capped_by.is_empty() {
        rationale.push(format!("Using the requested {tokens} tokens; no known bound reduced it further."));
    }

    EffectiveContextResolution {
        effective_tokens: tokens,
        capped_by,
        rationale,
    }
}

// ---------------------------------------------------------------------
// Combined per-model view (what the UI actually renders)
// ---------------------------------------------------------------------

/// The runtime kind a [`ContextCacheView`] describes, kept local to this
/// module (rather than importing `runtime_adapter::RuntimeKind`, which does
/// not cover MLX) so this module stays a self-contained leaf.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextRuntimeKind {
    Ollama,
    LlamaCpp,
    Mlx,
}

/// Everything the Runtime Hub's "Context & cache" panel needs for one
/// runtime: the configured context size, whatever live state the runtime
/// actually reports, and honest notes about what could not be observed.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextCacheView {
    pub runtime_id: String,
    pub runtime_kind: ContextRuntimeKind,
    pub configured: ConfiguredContext,
    pub reported_context_tokens: Option<u32>,
    pub context_tokens_in_use: Option<u32>,
    pub context_headroom_tokens: Option<u32>,
    pub context_shift_detected: Option<bool>,
    pub total_slots: Option<u32>,
    /// Whether two of this app's processes on one resident model can reuse each
    /// other's prompt prefix (roadmap K11).
    pub prefix_sharing: PrefixSharing,
    pub notes: Vec<String>,
    pub sampled_at_ms: u64,
}

/// Whether a runtime lets two processes on one resident model reuse each
/// other's cached prompt prefix, read-only.
///
/// A tagged union rather than a `bool` with prose beside it, for the reason
/// `RenderedMeasurement` and `ChainVerification` are: a caller cannot render
/// "supported" without also having the mechanism that makes it true, and cannot
/// render "unsupported" without the reason. Both arms carry `&'static str`
/// because both are settled facts about a pinned runtime, not per-sample
/// observations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum PrefixSharing {
    Supported { mechanism: &'static str },
    Unsupported { reason: &'static str },
}

/// What each runtime actually does about prefix sharing.
///
/// # llama.cpp does this already, and the app's job is not to break it
///
/// Verified against the build [`crate::managed_runtime::MANAGED_LLAMA_VERSION`]
/// pins, by running it: with no `--parallel` argument — which is what
/// `runtime_adapter::llama_args` passes — b9637 resolves `n_parallel` to `auto`
/// and reports `total_slots = 4` with a unified KV cache, and it routes each
/// request to the slot whose cached prefix matches best ("selected slot by LCP
/// similarity" in its own log), while `--cache-idle-slots` saves an idle slot's
/// KV into a server-wide RAM cache. Two different conversations sharing a
/// 454-token prefix were measured at 451 of 456 prompt tokens reused on the
/// second, on a slot the first had warmed.
///
/// So the sharing is the runtime's, it is read-only (no KV is copied between
/// sequences — the *request* is routed to where the prefix already is), and this
/// app gets it by **not** doing two things: pinning `id_slot`, and disabling
/// `cache_prompt`. `openai_request_body` does neither, and a test asserts that,
/// because either one would silently cost the whole feature with nothing failing.
///
/// A first probe of this pinned `id_slot` and saw zero reuse, which looked like
/// "the runtime cannot share across slots". It was the pin itself defeating the
/// router — worth recording, since that is exactly the mistake the guard exists
/// to prevent.
pub fn prefix_sharing(kind: ContextRuntimeKind) -> PrefixSharing {
    match kind {
        ContextRuntimeKind::LlamaCpp => PrefixSharing::Supported {
            mechanism: "llama-server routes each request to the slot whose cached prompt prefix matches it best, and saves an idle slot's cache into a server-wide pool, so a second conversation that starts with the same prefix reuses it instead of re-evaluating it. Nothing is copied between sequences and no request can read another's tokens.",
        },
        // Not "we did not implement it" — Ollama's HTTP API exposes neither slots
        // nor prompt-cache state, so there is nothing here to observe or steer.
        // Whatever its server does internally, this app cannot claim it.
        ContextRuntimeKind::Ollama => PrefixSharing::Unsupported {
            reason: "Ollama's API exposes no slot or prompt-cache surface, so this app cannot observe or influence whether two of its processes reuse a prompt prefix, and will not claim reuse it cannot see.",
        },
        ContextRuntimeKind::Mlx => PrefixSharing::Unsupported {
            reason: "The MLX runtime in this build keeps no prompt cache between requests, so every request evaluates its whole prompt and there is no prefix to share.",
        },
    }
}

/// Setting-key candidates to try, in priority order, per runtime kind — the
/// only place this module hardcodes the two adapters' existing setting
/// names (`"context_size"` for llama.cpp, `"num_ctx"` for Ollama).
pub fn configured_context_key_candidates(kind: ContextRuntimeKind) -> &'static [&'static str] {
    match kind {
        ContextRuntimeKind::LlamaCpp => &["context_size"],
        ContextRuntimeKind::Ollama => &["num_ctx"],
        ContextRuntimeKind::Mlx => &[],
    }
}

/// Builds the honest, non-fabricated per-runtime notes describing what live
/// data is and is not available, given what was actually observed.
pub fn context_cache_notes(kind: ContextRuntimeKind, live: Option<&LlamaCppLiveContextState>) -> Vec<String> {
    match kind {
        ContextRuntimeKind::Mlx => vec![
            "The MLX runtime does not expose a configurable context window or live KV-cache state today; only generation token counts are available per request.".to_string(),
        ],
        ContextRuntimeKind::Ollama => vec![
            "Ollama's API does not report live context/KV-cache occupancy for a running model; only the context size this app requested at load time is known.".to_string(),
        ],
        ContextRuntimeKind::LlamaCpp => {
            let mut notes = Vec::new();
            match live {
                Some(state) if state.endpoints_reachable.iter().any(|entry| entry == "/props") => {
                    notes.push("Live context size confirmed by llama-server's /props endpoint.".to_string());
                }
                _ => notes.push(
                    "llama-server's /props endpoint did not respond; showing only the context size this app requested at load time.".to_string(),
                ),
            }
            match live {
                Some(state) if state.endpoints_reachable.iter().any(|entry| entry == "/slots") => {
                    notes.push("Live per-slot token usage and context-shift state read from llama-server's /slots endpoint.".to_string());
                }
                _ => notes.push(
                    "llama-server's /slots endpoint did not answer (it is on by default in the version this app manages, so the likely causes are --no-slots or a server that is not reachable), leaving live token-in-use and context-shift state unknown, not zero.".to_string(),
                ),
            }
            notes
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_adapter::{EndpointPolicy, RuntimeFuture, SettingValueSchema};
    use std::sync::Mutex;

    fn capability(key: &str, default: i64, min: i64, max: i64) -> AdvancedSettingCapability {
        AdvancedSettingCapability {
            key: key.to_string(),
            label: key.to_string(),
            description: String::new(),
            schema: SettingValueSchema::Integer { min, max, step: 1 },
            default_value: SettingValue::Integer { value: default },
            restart_required: false,
            supported: true,
            unsupported_reason: None,
        }
    }

    // -- resolve_configured_context ---------------------------------

    #[test]
    fn resolve_configured_context_prefers_persisted_value_over_default() {
        let schema = vec![capability("context_size", 4_096, 128, 1_048_576)];
        let mut persisted = BTreeMap::new();
        persisted.insert("context_size".to_string(), SettingValue::Integer { value: 16_384 });
        let resolved = resolve_configured_context(&schema, Some(&persisted), &["context_size"]);
        assert_eq!(resolved.tokens, Some(16_384));
        assert_eq!(resolved.source, ContextLimitSource::RuntimeConfigured);
        assert_eq!(resolved.setting_key.as_deref(), Some("context_size"));
    }

    #[test]
    fn resolve_configured_context_falls_back_to_schema_default() {
        let schema = vec![capability("num_ctx", 4_096, 128, 1_048_576)];
        let resolved = resolve_configured_context(&schema, None, &["num_ctx"]);
        assert_eq!(resolved.tokens, Some(4_096));
        assert_eq!(resolved.source, ContextLimitSource::RuntimeDefault);
    }

    #[test]
    fn resolve_configured_context_unavailable_when_no_matching_setting() {
        let schema: Vec<AdvancedSettingCapability> = vec![];
        let resolved = resolve_configured_context(&schema, None, &[]);
        assert_eq!(resolved.tokens, None);
        assert_eq!(resolved.source, ContextLimitSource::Unavailable);
        assert_eq!(resolved.setting_key, None);
    }

    #[test]
    fn context_headroom_is_none_unless_both_figures_are_known() {
        assert_eq!(context_headroom(Some(8_192), Some(2_000)), Some(6_192));
        assert_eq!(context_headroom(None, Some(2_000)), None);
        assert_eq!(context_headroom(Some(8_192), None), None);
    }

    // -- llama.cpp response parsing -----------------------------------

    #[test]
    fn parse_llama_props_body_reads_nested_context_size() {
        let body = br#"{"default_generation_settings":{"n_ctx":8192},"total_slots":2}"#;
        assert_eq!(parse_llama_props_body(body), (Some(8_192), Some(2)));
    }

    #[test]
    fn parse_llama_props_body_handles_malformed_json_as_unavailable() {
        assert_eq!(parse_llama_props_body(b"not json"), (None, None));
    }

    #[test]
    fn parse_llama_props_body_handles_missing_fields_as_unavailable() {
        assert_eq!(parse_llama_props_body(b"{}"), (None, None));
    }

    #[test]
    fn parse_llama_slots_body_reads_truncated_flag() {
        let body = br#"[{"id":0,"n_ctx":8192,"n_past":512,"truncated":true}]"#;
        let slots = parse_llama_slots_body(body).expect("valid slots array");
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].id, Some(0));
        assert_eq!(slots[0].context_tokens, Some(8_192));
        assert_eq!(slots[0].tokens_in_use, Some(512));
        assert_eq!(slots[0].context_shifted, Some(true));
    }

    #[test]
    fn parse_llama_slots_body_reads_nested_params_context_size() {
        let body = br#"[{"id":1,"params":{"n_ctx":4096},"truncated":false}]"#;
        let slots = parse_llama_slots_body(body).expect("valid slots array");
        assert_eq!(slots[0].context_tokens, Some(4_096));
        assert_eq!(slots[0].context_shifted, Some(false));
    }

    #[test]
    fn parse_llama_slots_body_none_when_not_disabled_endpoint_body() {
        // A disabled /slots endpoint commonly returns a JSON error object,
        // not an array — this must be treated as "unavailable", not parsed
        // as zero slots.
        let body = br#"{"error":"slots endpoint is disabled"}"#;
        assert_eq!(parse_llama_slots_body(body), None);
    }

    // -- fetch_llama_cpp_live_context_state (mock transport) -----------

    #[derive(Default)]
    struct FixtureTransport {
        by_path: Mutex<BTreeMap<String, (u16, Vec<u8>)>>,
    }

    impl FixtureTransport {
        fn respond(&self, path: &str, status: u16, body: &[u8]) {
            self.by_path
                .lock()
                .expect("lock fixture transport")
                .insert(path.to_string(), (status, body.to_vec()));
        }
    }

    impl HttpTransport for FixtureTransport {
        fn execute<'a>(
            &'a self,
            request: HttpRequest,
            _cancellation: &'a CancellationToken,
        ) -> RuntimeFuture<'a, crate::runtime_adapter::HttpResponse> {
            let path = request.url.rsplit_once('/').map(|(_, tail)| format!("/{tail}"));
            let found = path.and_then(|path| self.by_path.lock().expect("lock fixture transport").get(&path).cloned());
            Box::pin(async move {
                match found {
                    Some((status, body)) => Ok(crate::runtime_adapter::HttpResponse { status, body }),
                    None => Ok(crate::runtime_adapter::HttpResponse { status: 404, body: b"not found".to_vec() }),
                }
            })
        }
    }

    #[tokio::test]
    async fn fetch_llama_cpp_live_context_state_merges_both_endpoints() {
        let transport = FixtureTransport::default();
        transport.respond(
            "/props",
            200,
            br#"{"default_generation_settings":{"n_ctx":8192},"total_slots":1}"#,
        );
        transport.respond("/slots", 200, br#"[{"id":0,"n_ctx":8192,"n_past":100,"truncated":false}]"#);
        let endpoint = EndpointOrigin::parse("http://127.0.0.1:8090", EndpointPolicy::LoopbackOnly).expect("endpoint");
        let cancellation = CancellationToken::new();
        let state = fetch_llama_cpp_live_context_state(&endpoint, &transport, &cancellation).await;
        assert_eq!(state.reported_context_tokens, Some(8_192));
        assert_eq!(state.total_slots, Some(1));
        assert_eq!(state.slots.len(), 1);
        assert_eq!(state.slots[0].tokens_in_use, Some(100));
        assert!(state.endpoints_reachable.contains(&"/props".to_string()));
        assert!(state.endpoints_reachable.contains(&"/slots".to_string()));
    }

    #[tokio::test]
    async fn fetch_llama_cpp_live_context_state_degrades_when_slots_disabled() {
        let transport = FixtureTransport::default();
        transport.respond(
            "/props",
            200,
            br#"{"default_generation_settings":{"n_ctx":4096},"total_slots":1}"#,
        );
        transport.respond("/slots", 501, b"Not Implemented");
        let endpoint = EndpointOrigin::parse("http://127.0.0.1:8090", EndpointPolicy::LoopbackOnly).expect("endpoint");
        let cancellation = CancellationToken::new();
        let state = fetch_llama_cpp_live_context_state(&endpoint, &transport, &cancellation).await;
        assert_eq!(state.reported_context_tokens, Some(4_096));
        assert!(state.slots.is_empty());
        assert!(!state.endpoints_reachable.contains(&"/slots".to_string()));
    }

    #[tokio::test]
    async fn fetch_llama_cpp_live_context_state_all_unavailable_when_unreachable() {
        let transport = FixtureTransport::default();
        // No fixtures registered at all: every request 404s.
        let endpoint = EndpointOrigin::parse("http://127.0.0.1:8090", EndpointPolicy::LoopbackOnly).expect("endpoint");
        let cancellation = CancellationToken::new();
        let state = fetch_llama_cpp_live_context_state(&endpoint, &transport, &cancellation).await;
        assert_eq!(state, LlamaCppLiveContextState::default());
    }

    // -- classify_context_failure --------------------------------------

    #[test]
    fn classifies_prompt_too_long_from_known_llama_server_message() {
        let input = ContextFailureInput {
            error_text: Some(
                "the request exceeds the available context size, try increasing it".to_string(),
            ),
            http_status: Some(400),
            ..Default::default()
        };
        let classification = classify_context_failure(&input).expect("should classify");
        assert_eq!(classification.class, ContextFailureClass::PromptTooLong);
    }

    #[test]
    fn classifies_prompt_too_long_from_numeric_comparison_without_error_text() {
        let input = ContextFailureInput {
            prompt_tokens: Some(9_000),
            configured_context_tokens: Some(8_192),
            ..Default::default()
        };
        let classification = classify_context_failure(&input).expect("should classify");
        assert_eq!(classification.class, ContextFailureClass::PromptTooLong);
        assert!(classification.explanation.contains("9000") || classification.explanation.contains("9,000"));
    }

    #[test]
    fn classifies_cache_exhausted_context_shift_from_signal() {
        let input = ContextFailureInput {
            context_shift_signal: Some(true),
            configured_context_tokens: Some(8_192),
            ..Default::default()
        };
        let classification = classify_context_failure(&input).expect("should classify");
        assert_eq!(classification.class, ContextFailureClass::CacheExhaustedContextShift);
        assert!(classification.explanation.contains("8192"));
    }

    #[test]
    fn classifies_memory_pressure_from_error_text() {
        let input = ContextFailureInput {
            error_text: Some("failed to allocate compute buffer, CUDA out of memory".to_string()),
            ..Default::default()
        };
        let classification = classify_context_failure(&input).expect("should classify");
        assert_eq!(classification.class, ContextFailureClass::MemoryPressure);
    }

    #[test]
    fn classifies_memory_pressure_from_numeric_comparison() {
        let input = ContextFailureInput {
            available_ram_bytes: Some(2_000_000_000),
            required_ram_bytes: Some(8_000_000_000),
            ..Default::default()
        };
        let classification = classify_context_failure(&input).expect("should classify");
        assert_eq!(classification.class, ContextFailureClass::MemoryPressure);
    }

    #[test]
    fn classifies_model_metadata_limit_when_requested_exceeds_model_max() {
        let input = ContextFailureInput {
            requested_context_tokens: Some(32_000),
            model_metadata_max_context_tokens: Some(8_192),
            ..Default::default()
        };
        let classification = classify_context_failure(&input).expect("should classify");
        assert_eq!(classification.class, ContextFailureClass::ModelMetadataLimit);
    }

    #[test]
    fn classifies_runtime_limitation_when_runtime_cannot_support_context_control() {
        let input = ContextFailureInput {
            runtime_supports_context_control: Some(false),
            requested_context_tokens: Some(8_192),
            ..Default::default()
        };
        let classification = classify_context_failure(&input).expect("should classify");
        assert_eq!(classification.class, ContextFailureClass::RuntimeLimitation);
    }

    #[test]
    fn classifies_runtime_limitation_from_error_text() {
        let input = ContextFailureInput {
            error_text: Some("unrecognized argument: --ctx-size".to_string()),
            ..Default::default()
        };
        let classification = classify_context_failure(&input).expect("should classify");
        assert_eq!(classification.class, ContextFailureClass::RuntimeLimitation);
    }

    #[test]
    fn returns_none_for_unrelated_errors() {
        let input = ContextFailureInput {
            error_text: Some("connection refused".to_string()),
            ..Default::default()
        };
        assert!(classify_context_failure(&input).is_none());
    }

    #[test]
    fn context_shift_takes_priority_over_prompt_too_long() {
        // A conversation that both grew past the window (shift) and has a
        // long individual prompt should still be reported as a shift, since
        // that is the more specific/actionable signal.
        let input = ContextFailureInput {
            context_shift_signal: Some(true),
            prompt_tokens: Some(9_000),
            configured_context_tokens: Some(8_192),
            ..Default::default()
        };
        let classification = classify_context_failure(&input).expect("should classify");
        assert_eq!(classification.class, ContextFailureClass::CacheExhaustedContextShift);
    }

    // -- resolve_effective_context --------------------------------------

    #[test]
    fn resolve_effective_context_uses_request_when_nothing_caps_it() {
        let input = EffectiveContextInput {
            requested_tokens: 8_192,
            offload_plan_context_tokens: 16_384,
            model_metadata_max_context_tokens: None,
            runtime_setting_min_tokens: None,
            runtime_setting_max_tokens: None,
        };
        let resolution = resolve_effective_context(&input);
        assert_eq!(resolution.effective_tokens, 8_192);
        assert!(resolution.capped_by.is_empty());
    }

    #[test]
    fn resolve_effective_context_clamps_to_offload_plan() {
        let input = EffectiveContextInput {
            requested_tokens: 32_000,
            offload_plan_context_tokens: 8_192,
            model_metadata_max_context_tokens: None,
            runtime_setting_min_tokens: None,
            runtime_setting_max_tokens: None,
        };
        let resolution = resolve_effective_context(&input);
        assert_eq!(resolution.effective_tokens, 8_192);
        assert_eq!(resolution.capped_by, vec!["offload_plan".to_string()]);
    }

    #[test]
    fn resolve_effective_context_clamps_to_model_metadata_after_offload_plan() {
        let input = EffectiveContextInput {
            requested_tokens: 32_000,
            offload_plan_context_tokens: 16_384,
            model_metadata_max_context_tokens: Some(4_096),
            runtime_setting_min_tokens: None,
            runtime_setting_max_tokens: None,
        };
        let resolution = resolve_effective_context(&input);
        assert_eq!(resolution.effective_tokens, 4_096);
        assert_eq!(
            resolution.capped_by,
            vec!["offload_plan".to_string(), "model_metadata".to_string()]
        );
    }

    #[test]
    fn resolve_effective_context_applies_runtime_setting_bounds() {
        let input = EffectiveContextInput {
            requested_tokens: 64,
            offload_plan_context_tokens: 16_384,
            model_metadata_max_context_tokens: None,
            runtime_setting_min_tokens: Some(128),
            runtime_setting_max_tokens: Some(1_048_576),
        };
        let resolution = resolve_effective_context(&input);
        assert_eq!(resolution.effective_tokens, 128);
        assert_eq!(resolution.capped_by, vec!["runtime_setting_min".to_string()]);
    }

    // -- context_cache_notes ---------------------------------------------

    #[test]
    fn mlx_notes_are_honest_about_missing_support() {
        let notes = context_cache_notes(ContextRuntimeKind::Mlx, None);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("does not expose"));
    }

    /// Every runtime resolves to one arm or the other, and neither arm can be
    /// rendered without the prose that justifies it — an `unsupported` with no
    /// reason is the shape this union exists to make unconstructible.
    #[test]
    fn every_runtime_states_its_prefix_sharing_with_a_reason() {
        for kind in [
            ContextRuntimeKind::LlamaCpp,
            ContextRuntimeKind::Ollama,
            ContextRuntimeKind::Mlx,
        ] {
            match prefix_sharing(kind) {
                PrefixSharing::Supported { mechanism } => assert!(
                    !mechanism.trim().is_empty(),
                    "{kind:?} claims support without naming the mechanism"
                ),
                PrefixSharing::Unsupported { reason } => {
                    assert!(
                        !reason.trim().is_empty(),
                        "{kind:?} refuses without a reason"
                    )
                }
            }
        }
        // The one runtime this app can actually observe is the one that supports
        // it; the other two are honest refusals, not unimplemented work.
        assert!(matches!(
            prefix_sharing(ContextRuntimeKind::LlamaCpp),
            PrefixSharing::Supported { .. }
        ));
        assert!(matches!(
            prefix_sharing(ContextRuntimeKind::Ollama),
            PrefixSharing::Unsupported { .. }
        ));
        assert!(matches!(
            prefix_sharing(ContextRuntimeKind::Mlx),
            PrefixSharing::Unsupported { .. }
        ));
    }

    /// The wire shape the UI switches on. A bare boolean would let a caller show
    /// "supported" with no mechanism beside it.
    #[test]
    fn prefix_sharing_serializes_as_a_tagged_union() {
        let json =
            serde_json::to_value(prefix_sharing(ContextRuntimeKind::Mlx)).expect("serialize");
        assert_eq!(json["state"], "unsupported");
        assert!(json["reason"]
            .as_str()
            .expect("a reason")
            .contains("no prompt cache"));
        let json =
            serde_json::to_value(prefix_sharing(ContextRuntimeKind::LlamaCpp)).expect("serialize");
        assert_eq!(json["state"], "supported");
        assert!(
            json.get("reason").is_none(),
            "the supported arm carries no reason field"
        );
    }

    #[test]
    fn llama_cpp_notes_reflect_which_endpoints_answered() {
        let mut live = LlamaCppLiveContextState::default();
        live.endpoints_reachable.push("/props".to_string());
        let notes = context_cache_notes(ContextRuntimeKind::LlamaCpp, Some(&live));
        assert!(notes[0].contains("confirmed"));
        assert!(notes[1].contains("did not answer"));
        assert!(
            notes[1].contains("unknown, not zero"),
            "the unavailable branch must still refuse to read as a zero"
        );
    }
}
