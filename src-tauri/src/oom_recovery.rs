//! Fail-closed memory recovery for managed llama.cpp model *loads*.
//!
//! No inference replay, arbitrary-model substitution, eviction of other
//! workloads, or overriding a user-configured context/GPU-layer setting.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use crate::runtime_adapter::SettingValue;

pub const MAX_LOAD_ATTEMPTS: usize = 3;
pub const MAX_LOAD_HISTORY: usize = 64;

/// An OOM diagnosis requires evidence of an owned llama.cpp process exiting
/// before readiness and a recognized allocator message from its log. A timeout,
/// exit code 137, GPU busy report or standalone word "memory" is insufficient.
pub fn classify_managed_llama_oom(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    if !lower.contains("managed runtime controller failed during wait for managed llama.cpp readiness")
        || !lower.contains("process exited before readiness")
    {
        return false;
    }
    [
        "out of memory",
        "not enough memory",
        "failed to allocate memory",
        "failed to allocate buffer",
        "memory allocation failed",
        "cuda error out of memory",
        "cudamalloc failed",
        "hiperroroutofmemory",
        "ggml_backend_alloc_ctx_tensors: failed",
        "ggml_gallocr_reserve_n: failed",
    ].iter().any(|marker| lower.contains(marker))
}

/// Calculate one *strictly smaller* memory configuration. Only knobs absent
/// from persisted user config may be changed; there is no "reset to defaults".
/// A verified GGUF block count is required before changing GPU layer count.
pub fn next_lower_memory_settings(
    original: &BTreeMap<String, SettingValue>,
    current: &BTreeMap<String, SettingValue>,
    verified_gguf_blocks: Option<u64>,
    gpu_offload_available: bool,
) -> Option<BTreeMap<String, SettingValue>> {
    let mut next = current.clone();
    let current_context = match current.get("context_size") {
        Some(SettingValue::Integer { value }) => *value,
        _ => 4096,
    };
    if !original.contains_key("context_size") && current_context > 512 {
        let lower = (current_context / 2).max(512);
        if lower < current_context {
            next.insert("context_size".to_string(), SettingValue::Integer { value: lower });
        }
    }

    if !original.contains_key("gpu_layers") && gpu_offload_available {
        if let Some(blocks) = verified_gguf_blocks.filter(|count| *count > 0 && *count <= 999) {
            let current_layers = match current.get("gpu_layers") {
                Some(SettingValue::Integer { value }) if *value >= 0 => *value,
                _ => blocks as i64,
            };
            if current_layers > 0 {
                let lower = current_layers / 2;
                next.insert("gpu_layers".to_string(), SettingValue::Integer { value: lower });
            }
        }
    }
    (next != *current).then_some(next)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadAttemptOutcome {
    Success,
    OutOfMemory,
    OtherFailure,
    UnsafeToRetry,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadAttempt {
    pub ordinal: u8,
    pub context_tokens: i64,
    pub gpu_layers: i64,
    pub outcome: LoadAttemptOutcome,
}

impl LoadAttempt {
    pub fn from_settings(ordinal: usize, settings: &BTreeMap<String, SettingValue>, outcome: LoadAttemptOutcome) -> Self {
        let integer = |key: &str, fallback| match settings.get(key) {
            Some(SettingValue::Integer { value }) => *value,
            _ => fallback,
        };
        Self { ordinal: ordinal as u8, context_tokens: integer("context_size", 4096), gpu_layers: integer("gpu_layers", -1), outcome }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LoadRecoveryRecord {
    pub at_ms: u64,
    pub runtime_id: String,
    pub asset_id: String,
    pub recovered: bool,
    pub attempts: Vec<LoadAttempt>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(context: Option<i64>, gpu: Option<i64>) -> BTreeMap<String, SettingValue> {
        let mut settings = BTreeMap::new();
        if let Some(value) = context { settings.insert("context_size".into(), SettingValue::Integer { value }); }
        if let Some(value) = gpu { settings.insert("gpu_layers".into(), SettingValue::Integer { value }); }
        settings
    }

    #[test]
    fn only_owned_launch_with_allocator_evidence_triggers_recovery() {
        let prefix = "runtime: managed runtime controller failed during wait for managed llama.cpp readiness: process exited before readiness: exit status: 1\n";
        assert!(classify_managed_llama_oom(&format!("{prefix}ggml: CUDA out of memory")));
        assert!(classify_managed_llama_oom(&format!("{prefix}failed to allocate buffer")));
        for error in [
            "runtime: insufficient memory",
            "runtime: process exited before readiness: out of memory",
            "runtime: managed runtime controller failed during wait for managed llama.cpp readiness: timeout: out of memory",
            "runtime: managed runtime controller failed during wait for managed llama.cpp readiness: process exited before readiness: exit 137",
            "runtime: managed runtime controller failed during wait for managed llama.cpp readiness: process exited before readiness: model file missing",
        ] { assert!(!classify_managed_llama_oom(error), "{error}"); }
    }

    #[test]
    fn fallback_shrinks_only_implicit_controls_in_bounded_steps() {
        let original = settings(None, None);
        let initial = settings(None, None);
        let first = next_lower_memory_settings(&original, &initial, Some(32), true).unwrap();
        assert_eq!(first, settings(Some(2048), Some(16)));
        let second = next_lower_memory_settings(&original, &first, Some(32), true).unwrap();
        assert_eq!(second, settings(Some(1024), Some(8)));
    }

    #[test]
    fn explicit_settings_and_unverified_gpu_header_cannot_be_overridden() {
        let user = settings(Some(8192), Some(-1));
        assert_eq!(next_lower_memory_settings(&user, &user, Some(64), true), None);
        let initial = settings(None, None);
        assert_eq!(next_lower_memory_settings(&initial, &initial, None, true), Some(settings(Some(2048), None)));
        assert_eq!(next_lower_memory_settings(&initial, &initial, Some(64), false), Some(settings(Some(2048), None)));
    }

    #[test]
    fn one_knob_may_be_locked_while_the_other_is_reduced() {
        let original = settings(Some(2048), None);
        let next = next_lower_memory_settings(&original, &original, Some(24), true).unwrap();
        assert_eq!(next, settings(Some(2048), Some(12)));
        assert_eq!(next_lower_memory_settings(&settings(Some(512), Some(0)), &settings(Some(512), Some(0)), Some(24), true), None);
    }

    #[test]
    fn history_records_only_effective_numeric_controls_not_raw_logs() {
        let entry = LoadAttempt::from_settings(2, &settings(Some(1024), Some(8)), LoadAttemptOutcome::OutOfMemory);
        let serialized = serde_json::to_string(&entry).unwrap();
        assert!(serialized.contains("\"contextTokens\":1024"));
        assert!(serialized.contains("\"gpuLayers\":8"));
        assert!(!serialized.contains("log"));
    }
}
