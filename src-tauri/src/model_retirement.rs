//! Model Retirement and Compatibility Warnings (ROADMAP.md Phase 8, item 14).
//!
//! Tauri-free, unit-testable core logic for two independent signals that
//! both feed the same "clear migration path before a run starts" contract:
//!
//! 1. **Retired/deprecated cloud provider models** (`RETIRED_CLOUD_MODELS` +
//!    [`check_cloud_model`] / [`check_cloud_models_batch`]). There is no live
//!    API this app can call in this sandbox to ask "is model X retired?" for
//!    OpenAI/Anthropic/etc — `providers.rs`'s `fetch_models` only returns
//!    whatever a provider's `/models` endpoint currently advertises, which
//!    silently omits retired ids rather than flagging them. So this is
//!    exactly what it looks like: a maintained, versioned, local static list
//!    of model identifiers publicly documented as retired/deprecated by
//!    their provider, each with a migration hint. It is deliberately
//!    conservative — only ids we have solid public-knowledge confidence are
//!    actually shut down, never a guess — and structured as a plain Rust
//!    slice so a future update is a one-line diff. Mirrors the "Runtime
//!    Component Update Channels" module's local-registry pattern for the
//!    same "can't verify a live upstream source in this sandbox" situation.
//!
//! 2. **Outdated local Runtime Hub models** ([`check_local_model_staleness`]).
//!    `m3_runtime_hub.rs` has no per-model "driver support" classification to
//!    flag an unsupported model family, so per the roadmap item's own scope
//!    note this narrows to a real, honest signal the hub already has: an
//!    installed model whose catalog has a newer revision available *and*
//!    which hasn't been updated in a long time. `M3RuntimeHub::search_catalog`
//!    (the same mechanism the "Find updates" button already uses) supplies
//!    the "newer revision" half; this module only does the pure comparison.
//!
//! **Old agent model defaults (roadmap item 14's third bullet):** a
//! repository-wide search (`DEFAULT_MODEL`/`default_model`/`defaultModel`/
//! `fallback_model`/`preferred_model` across `src/` and `src-tauri/src/`, plus
//! a literal-model-id sweep) found no hardcoded per-feature default cloud
//! model id in shipped code as of this module's introduction — every chat
//! surface (main chat, Side Tasks, Migration Agent, Crew, PM Copilot,
//! Compare, Debate, Deep Research, triage draft generation, and friends)
//! resolves the model to use from the user's own active selection
//! (`resolveTarget()` / `getActiveChatTarget()` in `src/lib/agentLoop.ts` and
//! `src/store/modelStore.ts`), never a baked-in fallback id. The only literal
//! model-id strings found were test fixtures (`server.rs`'s `route_model`
//! tests, `*.test.ts` illustrative values), which are not shipped defaults.
//! If a future change ever introduces a hardcoded default, running it through
//! [`check_cloud_model`] is exactly how to catch it before it ships.

use serde::{Deserialize, Serialize};

/// One entry in the static retired-cloud-model registry. Kept as plain
/// `&'static str` fields (not `Serialize`) — this is source data, never sent
/// to the frontend directly; callers get an owned [`CloudModelRetirementWarning`]
/// instead.
struct RetiredCloudModelEntry {
    provider_id: &'static str,
    model_id: &'static str,
    reason: &'static str,
    /// Case-insensitive substring matched against a provider's *currently
    /// fetched* model list to find a concrete, definitely-still-available
    /// migration target — e.g. `"gpt-4o"` matches `"gpt-4o"`, `"gpt-4o-mini"`,
    /// `"gpt-4o-2024-08-06"`. Never a guessed exact id: providers rename and
    /// re-date their "current" models constantly, so the only way to suggest
    /// a real, currently-selectable model is to search the live list the
    /// frontend already has, not to hardcode one that could itself go stale.
    replacement_family_query: &'static str,
    /// Human-readable fallback shown when no live model matches
    /// `replacement_family_query` (e.g. the account has no models cached
    /// yet), and alongside a concrete match for context.
    replacement_note: &'static str,
}

/// Known-retired/deprecated cloud model identifiers. Exact-match only
/// (never a fuzzy pattern) — a false "retired" claim about a model that is
/// actually still current is worse than missing a real retirement, so this
/// only ever matches an id character-for-character (case-insensitively).
///
/// Every entry here is a model publicly documented by its provider as
/// retired (the API rejects it, not merely "not recommended for new
/// projects"). Update this list as providers retire more models — that is
/// the entire maintenance story; there is no live verification step.
const RETIRED_CLOUD_MODELS: &[RetiredCloudModelEntry] = &[
    // --- OpenAI: legacy GPT-3-era completions models, retired January 2024. ---
    RetiredCloudModelEntry {
        provider_id: "openai",
        model_id: "text-davinci-003",
        reason: "OpenAI retired the legacy GPT-3 completions models in January 2024.",
        replacement_family_query: "gpt-4o",
        replacement_note: "a current GPT-4o family chat model (e.g. gpt-4o or gpt-4o-mini)",
    },
    RetiredCloudModelEntry {
        provider_id: "openai",
        model_id: "text-davinci-002",
        reason: "OpenAI retired the legacy GPT-3 completions models in January 2024.",
        replacement_family_query: "gpt-4o",
        replacement_note: "a current GPT-4o family chat model (e.g. gpt-4o or gpt-4o-mini)",
    },
    RetiredCloudModelEntry {
        provider_id: "openai",
        model_id: "text-davinci-001",
        reason: "OpenAI retired the legacy GPT-3 completions models in January 2024.",
        replacement_family_query: "gpt-4o",
        replacement_note: "a current GPT-4o family chat model (e.g. gpt-4o or gpt-4o-mini)",
    },
    RetiredCloudModelEntry {
        provider_id: "openai",
        model_id: "text-curie-001",
        reason: "OpenAI retired the legacy GPT-3 completions models in January 2024.",
        replacement_family_query: "gpt-4o",
        replacement_note: "a current GPT-4o family chat model (e.g. gpt-4o or gpt-4o-mini)",
    },
    RetiredCloudModelEntry {
        provider_id: "openai",
        model_id: "text-babbage-001",
        reason: "OpenAI retired the legacy GPT-3 completions models in January 2024.",
        replacement_family_query: "gpt-4o",
        replacement_note: "a current GPT-4o family chat model (e.g. gpt-4o or gpt-4o-mini)",
    },
    RetiredCloudModelEntry {
        provider_id: "openai",
        model_id: "text-ada-001",
        reason: "OpenAI retired the legacy GPT-3 completions models in January 2024.",
        replacement_family_query: "gpt-4o",
        replacement_note: "a current GPT-4o family chat model (e.g. gpt-4o or gpt-4o-mini)",
    },
    // --- OpenAI: Codex completion models, retired March 2023. ---
    RetiredCloudModelEntry {
        provider_id: "openai",
        model_id: "code-davinci-002",
        reason: "OpenAI retired the Codex completion models in March 2023.",
        replacement_family_query: "gpt-4o",
        replacement_note: "a current GPT-4o family chat model — tool/code-calling is built in",
    },
    RetiredCloudModelEntry {
        provider_id: "openai",
        model_id: "code-cushman-001",
        reason: "OpenAI retired the Codex completion models in March 2023.",
        replacement_family_query: "gpt-4o",
        replacement_note: "a current GPT-4o family chat model — tool/code-calling is built in",
    },
    // --- OpenAI: dated GPT-3.5/GPT-4 snapshot retirements. ---
    RetiredCloudModelEntry {
        provider_id: "openai",
        model_id: "gpt-3.5-turbo-0301",
        reason: "OpenAI retired this dated GPT-3.5 Turbo snapshot.",
        replacement_family_query: "gpt-3.5-turbo",
        replacement_note: "the current gpt-3.5-turbo alias, or a GPT-4o family model for better quality",
    },
    RetiredCloudModelEntry {
        provider_id: "openai",
        model_id: "gpt-4-vision-preview",
        reason: "OpenAI folded vision support into the main GPT-4o line and retired the separate vision-preview snapshot.",
        replacement_family_query: "gpt-4o",
        replacement_note: "a current GPT-4o family model — vision is built in, no separate vision-preview variant needed",
    },
    // --- Anthropic: Claude 1 / Claude Instant 1 line, deprecated per Anthropic's model deprecation policy. ---
    RetiredCloudModelEntry {
        provider_id: "anthropic",
        model_id: "claude-1",
        reason: "Anthropic retired the Claude 1 model line.",
        replacement_family_query: "claude-3",
        replacement_note: "a current Claude 3.x+ model, such as a Sonnet or Haiku variant",
    },
    RetiredCloudModelEntry {
        provider_id: "anthropic",
        model_id: "claude-1.0",
        reason: "Anthropic retired the Claude 1 model line.",
        replacement_family_query: "claude-3",
        replacement_note: "a current Claude 3.x+ model, such as a Sonnet or Haiku variant",
    },
    RetiredCloudModelEntry {
        provider_id: "anthropic",
        model_id: "claude-1.2",
        reason: "Anthropic retired the Claude 1 model line.",
        replacement_family_query: "claude-3",
        replacement_note: "a current Claude 3.x+ model, such as a Sonnet or Haiku variant",
    },
    RetiredCloudModelEntry {
        provider_id: "anthropic",
        model_id: "claude-1.3",
        reason: "Anthropic retired the Claude 1 model line.",
        replacement_family_query: "claude-3",
        replacement_note: "a current Claude 3.x+ model, such as a Sonnet or Haiku variant",
    },
    RetiredCloudModelEntry {
        provider_id: "anthropic",
        model_id: "claude-instant-1",
        reason: "Anthropic retired the Claude Instant model line.",
        replacement_family_query: "claude-3-5-haiku",
        replacement_note: "a current, fast/low-cost Claude Haiku model",
    },
    RetiredCloudModelEntry {
        provider_id: "anthropic",
        model_id: "claude-instant-1.1",
        reason: "Anthropic retired the Claude Instant model line.",
        replacement_family_query: "claude-3-5-haiku",
        replacement_note: "a current, fast/low-cost Claude Haiku model",
    },
    RetiredCloudModelEntry {
        provider_id: "anthropic",
        model_id: "claude-instant-1.2",
        reason: "Anthropic retired the Claude Instant model line.",
        replacement_family_query: "claude-3-5-haiku",
        replacement_note: "a current, fast/low-cost Claude Haiku model",
    },
    // --- Anthropic: Claude 2 line, deprecated per Anthropic's model deprecation policy. ---
    RetiredCloudModelEntry {
        provider_id: "anthropic",
        model_id: "claude-2.0",
        reason: "Anthropic retired the Claude 2 model line.",
        replacement_family_query: "claude-3",
        replacement_note: "a current Claude 3.x+ Sonnet or Opus model",
    },
    RetiredCloudModelEntry {
        provider_id: "anthropic",
        model_id: "claude-2.1",
        reason: "Anthropic retired the Claude 2 model line.",
        replacement_family_query: "claude-3",
        replacement_note: "a current Claude 3.x+ Sonnet or Opus model",
    },
];

/// A concrete, owned warning for one retired cloud model — safe to
/// `Serialize` straight back to the frontend.
// Deliberately NOT `#[serde(rename_all = "camelCase")]`: this type crosses
// the wire through `providers_check_model_retirements`
// (`src-tauri/src/providers.rs`), whose sibling types (`ProviderConfig`,
// `ProviderModelInfo`) serialize with plain snake_case field names — unlike
// the M3 Runtime Hub's camelCase convention that `LocalModelStalenessWarning`
// below follows instead, since that one crosses through `m3_commands.rs`.
// The frontend's `CloudModelRetirementWarning` TS interface
// (`src/store/modelStore.ts`) matches this snake_case shape exactly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudModelRetirementWarning {
    pub provider_id: String,
    pub model_id: String,
    pub reason: String,
    /// A specific, currently-available replacement model id found in the
    /// caller-supplied live list — never a guessed/hardcoded id. `None` when
    /// no live match was found; use `replacement_note` in that case.
    pub suggested_replacement_model_id: Option<String>,
    pub replacement_note: String,
}

fn find_entry(provider_id: &str, model_id: &str) -> Option<&'static RetiredCloudModelEntry> {
    RETIRED_CLOUD_MODELS
        .iter()
        .find(|entry| entry.provider_id == provider_id && entry.model_id.eq_ignore_ascii_case(model_id))
}

fn is_known_retired(provider_id: &str, model_id: &str) -> bool {
    find_entry(provider_id, model_id).is_some()
}

/// Finds the best concrete replacement for a retired model from
/// `available_model_ids` — the provider's own already-fetched live model
/// list. Never recommends another model this registry itself lists as
/// retired. Deterministic: prefers the shortest matching id (favoring a
/// bare family alias like `gpt-4o` over a longer dated snapshot like
/// `gpt-4o-2024-08-06`), then lexical order.
fn suggest_replacement_from_available(
    provider_id: &str,
    family_query: &str,
    available_model_ids: &[String],
) -> Option<String> {
    let needle = family_query.to_ascii_lowercase();
    let mut candidates: Vec<&String> = available_model_ids
        .iter()
        .filter(|id| id.to_ascii_lowercase().contains(&needle))
        .filter(|id| !is_known_retired(provider_id, id))
        .collect();
    candidates.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    candidates.into_iter().next().cloned()
}

/// Checks a single model id against the retired-cloud-model registry.
/// `available_model_ids` should be the provider's own currently fetched
/// model list (used only to find a concrete replacement — see
/// [`suggest_replacement_from_available`]); pass an empty slice if unknown,
/// which still returns a warning with `suggested_replacement_model_id: None`.
pub fn check_cloud_model(
    provider_id: &str,
    model_id: &str,
    available_model_ids: &[String],
) -> Option<CloudModelRetirementWarning> {
    let entry = find_entry(provider_id, model_id)?;
    Some(CloudModelRetirementWarning {
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
        reason: entry.reason.to_string(),
        suggested_replacement_model_id: suggest_replacement_from_available(
            provider_id,
            entry.replacement_family_query,
            available_model_ids,
        ),
        replacement_note: entry.replacement_note.to_string(),
    })
}

/// Checks every id in `model_ids` (typically a provider's entire fetched
/// model list) against the registry in one pass, returning a warning only
/// for the ones that are actually retired. `model_ids` doubles as the
/// "available" pool for replacement suggestions, since it already is the
/// full live list for this provider.
pub fn check_cloud_models_batch(
    provider_id: &str,
    model_ids: &[String],
) -> Vec<CloudModelRetirementWarning> {
    model_ids
        .iter()
        .filter_map(|model_id| check_cloud_model(provider_id, model_id, model_ids))
        .collect()
}

/// An installed local Runtime Hub model flagged as outdated: a newer catalog
/// revision exists for it, and it has gone unrefreshed for a long time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalModelStalenessWarning {
    pub asset_id: String,
    pub installed_revision: String,
    pub latest_revision: String,
    pub installed_at_ms: u64,
    pub age_ms: u64,
    /// Display name of the newer catalog entry — the concrete migration
    /// target ("update to this") for this warning.
    pub suggested_replacement_display_name: String,
}

/// How long an installed model may go without a refresh before a newer
/// catalog revision turns into an actionable staleness warning, rather than
/// just informational "an update exists" noise. 180 days: long enough that a
/// user who updates periodically never sees it, short enough to catch a
/// model that has been sitting untouched for one or more app-lifetimes.
pub const STALE_LOCAL_MODEL_THRESHOLD_MS: u64 = 180 * 24 * 60 * 60 * 1000;

/// Pure comparison: flags an installed model as outdated only when *both*
/// hold — a different (newer) catalog revision is available, and the
/// installed version has gone unrefreshed for at least
/// [`STALE_LOCAL_MODEL_THRESHOLD_MS`]. A different revision alone is not
/// enough (a model updated yesterday isn't "outdated" just because a source
/// republished it again today), and old-but-current is not enough either
/// (nothing to migrate to).
pub fn check_local_model_staleness(
    asset_id: &str,
    installed_revision: &str,
    installed_at_ms: u64,
    latest_catalog_revision: &str,
    latest_catalog_display_name: &str,
    now_ms: u64,
) -> Option<LocalModelStalenessWarning> {
    if installed_revision == latest_catalog_revision {
        return None;
    }
    let age_ms = now_ms.saturating_sub(installed_at_ms);
    if age_ms < STALE_LOCAL_MODEL_THRESHOLD_MS {
        return None;
    }
    Some(LocalModelStalenessWarning {
        asset_id: asset_id.to_string(),
        installed_revision: installed_revision.to_string(),
        latest_revision: latest_catalog_revision.to_string(),
        installed_at_ms,
        age_ms,
        suggested_replacement_display_name: latest_catalog_display_name.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_entries_are_unique_and_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for entry in RETIRED_CLOUD_MODELS {
            assert!(
                seen.insert((entry.provider_id, entry.model_id.to_ascii_lowercase())),
                "duplicate registry entry for {}/{}",
                entry.provider_id,
                entry.model_id
            );
            assert!(!entry.reason.is_empty());
            assert!(!entry.replacement_family_query.is_empty());
            assert!(!entry.replacement_note.is_empty());
        }
    }

    #[test]
    fn check_cloud_model_flags_known_retired_ids() {
        let warning = check_cloud_model("openai", "text-davinci-003", &[]).expect("should be retired");
        assert_eq!(warning.provider_id, "openai");
        assert_eq!(warning.model_id, "text-davinci-003");
        assert!(warning.reason.contains("retired"));
        assert!(warning.suggested_replacement_model_id.is_none());
    }

    #[test]
    fn check_cloud_model_is_case_insensitive_but_exact_otherwise() {
        assert!(check_cloud_model("openai", "TEXT-DAVINCI-003", &[]).is_some());
        // A still-current-looking id must never false-positive, even though
        // it shares a prefix with a retired one.
        assert!(check_cloud_model("openai", "text-davinci-003-vNext", &[]).is_none());
        assert!(check_cloud_model("openai", "gpt-4o", &[]).is_none());
        assert!(check_cloud_model("anthropic", "claude-3-5-sonnet-20241022", &[]).is_none());
    }

    #[test]
    fn check_cloud_model_never_flags_the_wrong_provider() {
        // "claude-1" is only retired under the anthropic provider id — a
        // custom/self-hosted provider that happens to expose a model with
        // the same string must never be flagged.
        assert!(check_cloud_model("some-custom-provider", "claude-1", &[]).is_none());
    }

    #[test]
    fn suggests_a_concrete_replacement_from_the_live_list() {
        let available = vec![
            "gpt-4o-2024-08-06".to_string(),
            "gpt-4o".to_string(),
            "gpt-4o-mini".to_string(),
            "gpt-3.5-turbo".to_string(),
        ];
        let warning = check_cloud_model("openai", "text-davinci-003", &available).unwrap();
        // Shortest matching "gpt-4o" family id wins — the bare alias, not a
        // longer dated snapshot.
        assert_eq!(warning.suggested_replacement_model_id.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn never_suggests_a_replacement_that_is_itself_retired() {
        // A pathological live list that (somehow) still contains another
        // retired id sharing the family substring must never be suggested.
        let available = vec!["claude-1".to_string(), "claude-3-5-sonnet-20241022".to_string()];
        let warning = check_cloud_model("anthropic", "claude-1.0", &available).unwrap();
        assert_eq!(
            warning.suggested_replacement_model_id.as_deref(),
            Some("claude-3-5-sonnet-20241022")
        );
    }

    #[test]
    fn falls_back_to_the_replacement_note_when_no_live_match_exists() {
        let available = vec!["some-other-model".to_string()];
        let warning = check_cloud_model("openai", "code-cushman-001", &available).unwrap();
        assert!(warning.suggested_replacement_model_id.is_none());
        assert!(!warning.replacement_note.is_empty());
    }

    #[test]
    fn batch_check_only_returns_the_retired_subset() {
        let ids = vec![
            "gpt-4o".to_string(),
            "text-davinci-003".to_string(),
            "gpt-4o-mini".to_string(),
            "code-davinci-002".to_string(),
        ];
        let warnings = check_cloud_models_batch("openai", &ids);
        let flagged: Vec<&str> = warnings.iter().map(|w| w.model_id.as_str()).collect();
        assert_eq!(flagged.len(), 2);
        assert!(flagged.contains(&"text-davinci-003"));
        assert!(flagged.contains(&"code-davinci-002"));
    }

    #[test]
    fn local_staleness_requires_both_a_different_revision_and_enough_age() {
        let now_ms: u64 = 1_000_000_000_000;
        let old_enough = now_ms - STALE_LOCAL_MODEL_THRESHOLD_MS - 1;
        let not_old_enough = now_ms - STALE_LOCAL_MODEL_THRESHOLD_MS + 1;

        // Different revision, old enough -> flagged.
        assert!(check_local_model_staleness("asset-1", "rev-1", old_enough, "rev-2", "Newer Model", now_ms).is_some());

        // Different revision, but recently installed -> not flagged yet.
        assert!(check_local_model_staleness("asset-1", "rev-1", not_old_enough, "rev-2", "Newer Model", now_ms).is_none());

        // Same revision, however old -> nothing to migrate to.
        assert!(check_local_model_staleness("asset-1", "rev-1", old_enough, "rev-1", "Same Model", now_ms).is_none());
    }

    #[test]
    fn local_staleness_warning_carries_a_concrete_migration_target() {
        let now_ms: u64 = 1_000_000_000_000;
        let installed_at_ms = now_ms - STALE_LOCAL_MODEL_THRESHOLD_MS - 1;
        let warning = check_local_model_staleness(
            "ollama:llama3:8b",
            "rev-old",
            installed_at_ms,
            "rev-new",
            "Llama 3.1 8B Instruct (Q4_K_M)",
            now_ms,
        )
        .expect("should be stale");
        assert_eq!(warning.asset_id, "ollama:llama3:8b");
        assert_eq!(warning.installed_revision, "rev-old");
        assert_eq!(warning.latest_revision, "rev-new");
        assert_eq!(warning.suggested_replacement_display_name, "Llama 3.1 8B Instruct (Q4_K_M)");
        assert!(warning.age_ms >= STALE_LOCAL_MODEL_THRESHOLD_MS);
    }
}
