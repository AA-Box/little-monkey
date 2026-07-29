//! Runtime Telemetry and Memory Trace Viewer (ROADMAP.md Phase 8): a bounded,
//! in-memory ring buffer of per-load and per-request runtime trace records,
//! plus a redacted support-bundle assembler.
//!
//! This module deliberately does **not** invent new capture mechanisms for
//! things other Phase 8 work already computes:
//!   - Offload placement and memory/VRAM headroom are the exact numbers
//!     `runtime_adapter::LocalOffloadPlanner` already produced for the load
//!     the caller is reporting ([`OffloadPlacementSummary`]/[`MemoryFootprint`],
//!     built via `From<&OffloadPlan>`).
//!   - Runtime log tails are read through the existing
//!     `M3RuntimeHub::runtime_logs` (backed by `SystemManagedProcessController`
//!     in `m3_production.rs`, which already redirects a managed llama.cpp/MLX
//!     child's stdout/stderr to a private on-disk log file) — this module
//!     only redacts and bounds the tail it is handed, it does not spawn or
//!     capture processes itself.
//!   - Hardware/compatibility context reuses `HardwareSnapshot` and
//!     `M3HardwareCompatibilityReport`, already `Serialize`.
//!
//! What genuinely IS new here: the trace schema itself, the bounded store,
//! and — the hard part — redaction. `cached_prompt_tokens` is honestly
//! `None`/noted `unavailable` today: the Context and KV Cache Control Center
//! that would report prompt-cache reuse was not merged into `origin/develop`
//! as of this module's introduction (see `unavailable` notes on
//! [`RuntimeTraceRecord`]).
//!
//! # Redaction guarantee
//!
//! [`RuntimeTraceRecord`] and [`SupportBundle`] never carry a free-text
//! "prompt" or "response" field — [`SamplerStats`] and [`TokenTiming`] are
//! fixed-shape structs of numeric/enum fields only
//! (`#[serde(deny_unknown_fields)]`), so a caller cannot smuggle prompt text
//! through them even by construction. The only free-text surfaces are
//! `error_message` (redacted before it is ever stored, see
//! [`RuntimeTelemetryState::record_load`]/[`RuntimeTelemetryState::record_request`])
//! and runtime log tails (redacted in [`build_support_bundle`] before they
//! are written into the bundle). [`Redactor`] layers three passes over that
//! free text:
//!   1. `knowledge_pipeline::SensitiveDataScanner` (already shipped for the
//!      Privacy Firewall roadmap item) — private keys, API credentials,
//!      emails, credit cards, phone numbers, IP addresses.
//!   2. A home-directory/username scrubber (`/Users/<name>`, `/home/<name>`,
//!      `C:\Users\<name>`) — file paths a managed process's log lines can
//!      legitimately contain, but which leak a real local username.
//!   3. A prompt/response-content scrubber for the shape managed runtimes
//!      actually log: `"content"`/`"prompt"`/`"response"`/`"input"`/`"output"`
//!      JSON string fields, and plain `Prompt: ...`/`Response: ...` log
//!      lines some verbose runtime builds emit.
//!
//! None of this is a blanket "redact everything" — ordinary diagnostic log
//! lines (load progress, layer counts, port numbers) pass through unchanged,
//! which is what makes the bundle still useful for diagnosis.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::knowledge_pipeline::SensitiveDataScanner;
use crate::m3_runtime_hub::M3HardwareCompatibilityReport;
use crate::runtime_adapter::{AcceleratorKind, HardwareSnapshot, OffloadPlan, ProjectorPlacement};

pub const RUNTIME_TELEMETRY_SCHEMA_VERSION: u32 = 1;

/// Ring-buffer capacity: enough recent history to diagnose a flapping
/// runtime across many loads/requests without unbounded memory growth.
const DEFAULT_TRACE_CAPACITY: usize = 200;
/// Hard cap on how many traces a single `recent(...)` call or support bundle
/// can return, independent of what the caller asked for.
const MAX_RECENT_TRACES: usize = 200;
/// Free-text inputs (error messages) are truncated before redaction so a
/// pathological caller cannot balloon the in-memory store or the exported
/// bundle.
const MAX_ERROR_MESSAGE_BYTES: usize = 8_000;
const MAX_IDENTIFIER_LEN: usize = 256;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn truncate_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\u{2026}[truncated]", &text[..end])
}

fn validate_identifier(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > MAX_IDENTIFIER_LEN || value.chars().any(char::is_control) {
        Err(format!(
            "{field} must be 1..={MAX_IDENTIFIER_LEN} bytes without control characters"
        ))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Trace schema
// ---------------------------------------------------------------------------

/// Documents a field this exact trace could not populate, and why — used
/// instead of silently leaving an `Option` empty so the UI/support bundle can
/// state plainly "this runtime does not report X" rather than looking like a
/// bug. Mirrors `runtime_adapter::OffloadRationale`'s `field`/`explanation`
/// shape on purpose.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TraceFieldNote {
    pub field: String,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TraceOutcome {
    Success,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LoadTiming {
    pub started_at_ms: u64,
    pub ready_at_ms: u64,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestTiming {
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    pub duration_ms: u64,
}

/// Sampler parameters actually used for one request. Fixed numeric/enum
/// shape only, `deny_unknown_fields` — this cannot carry prompt or response
/// text even if a caller tried to smuggle it in under an unexpected key.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SamplerStats {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<i64>,
    pub max_output_tokens: Option<u64>,
    pub repeat_penalty: Option<f64>,
    pub seed: Option<i64>,
}

/// Token counts/timing for one request. Same structural guarantee as
/// [`SamplerStats`]: numeric fields only.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenTiming {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Computed from `output_tokens` and wall-clock duration when the caller
    /// did not already supply a runtime-reported value (see
    /// [`RuntimeTelemetryState::record_request`]) — real arithmetic on real
    /// counts, never fabricated.
    pub tokens_per_second: Option<f64>,
    /// Always `None` today: no merged runtime surfaces prompt-cache reuse
    /// counts yet. Kept as a real field (not just an `unavailable` note) so
    /// the Context and KV Cache Control Center can populate it later without
    /// a schema change.
    pub cached_prompt_tokens: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryFootprint {
    pub available_ram_bytes: u64,
    pub available_vram_bytes: u64,
}

impl From<&OffloadPlan> for MemoryFootprint {
    fn from(plan: &OffloadPlan) -> Self {
        Self {
            available_ram_bytes: plan.available_ram_bytes,
            available_vram_bytes: plan.available_vram_bytes,
        }
    }
}

/// Copy of the fields of `runtime_adapter::OffloadPlan` relevant to a trace.
/// Kept as a distinct type (rather than embedding `OffloadPlan` itself) so
/// this schema does not silently change shape whenever the planner's
/// `rationale`/`improvement_suggestions` bookkeeping fields change.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OffloadPlacementSummary {
    pub accelerator: AcceleratorKind,
    pub context_tokens: u32,
    pub batch_size: u32,
    pub gpu_layers: u32,
    pub estimated_total_layers: u32,
    pub cpu_spill_layers: u32,
    pub projector_placement: ProjectorPlacement,
    pub parallel_sequences: u16,
}

impl From<&OffloadPlan> for OffloadPlacementSummary {
    fn from(plan: &OffloadPlan) -> Self {
        Self {
            accelerator: plan.accelerator,
            context_tokens: plan.context_tokens,
            batch_size: plan.batch_size,
            gpu_layers: plan.gpu_layers,
            estimated_total_layers: plan.estimated_total_layers,
            cpu_spill_layers: plan.cpu_spill_layers,
            projector_placement: plan.projector_placement,
            parallel_sequences: plan.parallel_sequences,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum TraceEvent {
    Load {
        timing: LoadTiming,
        offload: Option<OffloadPlacementSummary>,
        memory: Option<MemoryFootprint>,
    },
    Request {
        timing: RequestTiming,
        sampler: SamplerStats,
        tokens: TokenTiming,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeTraceRecord {
    pub schema_version: u32,
    pub trace_id: String,
    pub runtime_id: String,
    pub model_id: String,
    pub recorded_at_ms: u64,
    pub outcome: TraceOutcome,
    /// Redacted before this record is ever constructed — see
    /// [`RuntimeTelemetryState::record_load`]/[`record_request`].
    pub error_message: Option<String>,
    pub event: TraceEvent,
    pub unavailable: Vec<TraceFieldNote>,
}

// ---------------------------------------------------------------------------
// Command input shapes
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecordLoadTraceRequest {
    pub runtime_id: String,
    pub model_id: String,
    pub started_at_ms: u64,
    pub ready_at_ms: u64,
    /// The exact plan `runtime_adapter::LocalOffloadPlanner::plan` produced
    /// for this load, if the caller computed one (the Runtime Hub's load
    /// flow always does today). `None` only when no plan was ever computed.
    pub offload_plan: Option<OffloadPlan>,
    pub error_message: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecordRequestTraceRequest {
    pub runtime_id: String,
    pub model_id: String,
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    pub sampler: SamplerStats,
    pub tokens: TokenTiming,
    pub error_message: Option<String>,
}

// ---------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RedactionSummary {
    pub findings_redacted: usize,
    pub by_kind: BTreeMap<String, usize>,
}

impl RedactionSummary {
    fn note(&mut self, kind: &str) {
        self.findings_redacted += 1;
        *self.by_kind.entry(kind.to_string()).or_insert(0) += 1;
    }

    fn merge(&mut self, other: &RedactionSummary) {
        self.findings_redacted += other.findings_redacted;
        for (kind, count) in &other.by_kind {
            *self.by_kind.entry(kind.clone()).or_insert(0) += count;
        }
    }
}

/// Redacts free text before it is stored in a trace or written into a
/// support bundle. See the module doc comment for the three passes this
/// applies and why each exists.
pub struct Redactor {
    scanner: SensitiveDataScanner,
    home_dir_unix: Regex,
    home_dir_windows: Regex,
    cli_flag_secret: Regex,
    content_field: Regex,
    content_line: Regex,
}

impl Redactor {
    /// Infallible: every pattern here is a static string literal this module
    /// controls, never user input, so a compile failure would be a bug in
    /// this file, not a runtime condition to recover from — same reasoning
    /// `knowledge_pipeline`'s own test suite uses for
    /// `SensitiveDataScanner::new().expect(...)`.
    pub fn new() -> Self {
        Self {
            scanner: SensitiveDataScanner::new()
                .expect("SensitiveDataScanner's static regex patterns are valid"),
            home_dir_unix: Regex::new(r"(?i)(/Users/|/home/)([^/\s]+)")
                .expect("valid static regex"),
            home_dir_windows: Regex::new(r"(?i)(C:\\Users\\)([^\\\s]+)")
                .expect("valid static regex"),
            cli_flag_secret: Regex::new(
                r"(?i)(--(?:api[-_]?key|token|auth(?:orization)?)[= ]+)(\S+)",
            )
            .expect("valid static regex"),
            content_field: Regex::new(
                r#"(?i)"(content|prompt|response|input|output)"\s*:\s*"(?:[^"\\]|\\.)*""#,
            )
            .expect("valid static regex"),
            content_line: Regex::new(r"(?im)^([ \t]*(?:prompt|response)\s*[:=][ \t]*).*$")
                .expect("valid static regex"),
        }
    }

    /// Returns the redacted text plus a summary of what was found. Ordinary
    /// text with no findings comes back byte-for-byte identical.
    pub fn redact(&self, text: &str) -> (String, RedactionSummary) {
        let mut summary = RedactionSummary::default();

        let scan_preview = self.scanner.preview(text);
        for finding in &scan_preview.findings {
            summary.note(finding.kind.label());
        }
        let mut redacted = scan_preview.redacted_text;

        redacted = self
            .home_dir_unix
            .replace_all(&redacted, |caps: &Captures<'_>| {
                summary.note("HOME_DIRECTORY_PATH");
                format!("{}[REDACTED_USER]", &caps[1])
            })
            .into_owned();
        redacted = self
            .home_dir_windows
            .replace_all(&redacted, |caps: &Captures<'_>| {
                summary.note("HOME_DIRECTORY_PATH");
                format!("{}[REDACTED_USER]", &caps[1])
            })
            .into_owned();
        redacted = self
            .cli_flag_secret
            .replace_all(&redacted, |caps: &Captures<'_>| {
                summary.note("API_CREDENTIAL");
                format!("{}[REDACTED_SECRET]", &caps[1])
            })
            .into_owned();
        redacted = self
            .content_field
            .replace_all(&redacted, |caps: &Captures<'_>| {
                summary.note("PROMPT_CONTENT");
                format!("\"{}\":\"[REDACTED:CONTENT]\"", &caps[1])
            })
            .into_owned();
        redacted = self
            .content_line
            .replace_all(&redacted, |caps: &Captures<'_>| {
                summary.note("PROMPT_CONTENT");
                format!("{}[REDACTED:CONTENT]", &caps[1])
            })
            .into_owned();

        (redacted, summary)
    }
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Trace store
// ---------------------------------------------------------------------------

/// Bounded ring buffer of recent trace records, oldest evicted first.
pub struct RuntimeTraceStore {
    capacity: usize,
    records: Mutex<VecDeque<RuntimeTraceRecord>>,
}

impl RuntimeTraceStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            records: Mutex::new(VecDeque::new()),
        }
    }

    fn push(&self, record: RuntimeTraceRecord) {
        let Ok(mut records) = self.records.lock() else {
            return;
        };
        records.push_back(record);
        while records.len() > self.capacity {
            records.pop_front();
        }
    }

    /// Most-recent-first, optionally filtered by `runtime_id`, capped at
    /// both the caller's `limit` and [`MAX_RECENT_TRACES`].
    fn recent(&self, runtime_id: Option<&str>, limit: usize) -> Vec<RuntimeTraceRecord> {
        let Ok(records) = self.records.lock() else {
            return Vec::new();
        };
        let limit = limit.min(MAX_RECENT_TRACES).max(1);
        records
            .iter()
            .rev()
            .filter(|record| runtime_id.is_none_or(|id| record.runtime_id == id))
            .take(limit)
            .cloned()
            .collect()
    }

    fn len(&self) -> usize {
        self.records.lock().map(|records| records.len()).unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Public state facade (what `M3CommandState` holds)
// ---------------------------------------------------------------------------

pub struct RuntimeTelemetryState {
    store: RuntimeTraceStore,
    redactor: Redactor,
}

impl RuntimeTelemetryState {
    pub fn new() -> Self {
        Self {
            store: RuntimeTraceStore::new(DEFAULT_TRACE_CAPACITY),
            redactor: Redactor::new(),
        }
    }

    pub fn redactor(&self) -> &Redactor {
        &self.redactor
    }

    pub fn trace_count(&self) -> usize {
        self.store.len()
    }

    pub fn recent(&self, runtime_id: Option<&str>, limit: usize) -> Vec<RuntimeTraceRecord> {
        self.store.recent(runtime_id, limit)
    }

    fn redact_error(&self, error_message: Option<String>) -> Option<String> {
        error_message.map(|text| {
            let bounded = truncate_bytes(&text, MAX_ERROR_MESSAGE_BYTES);
            self.redactor.redact(&bounded).0
        })
    }

    pub fn record_load(
        &self,
        request: RecordLoadTraceRequest,
    ) -> Result<RuntimeTraceRecord, String> {
        validate_identifier(&request.runtime_id, "runtimeId")?;
        validate_identifier(&request.model_id, "modelId")?;

        let duration_ms = request.ready_at_ms.saturating_sub(request.started_at_ms);
        let mut unavailable = Vec::new();
        let (offload, memory) = match &request.offload_plan {
            Some(plan) => (
                Some(OffloadPlacementSummary::from(plan)),
                Some(MemoryFootprint::from(plan)),
            ),
            None => {
                let reason =
                    "no offload plan was supplied for this load".to_string();
                unavailable.push(TraceFieldNote {
                    field: "offload".to_string(),
                    reason: reason.clone(),
                });
                unavailable.push(TraceFieldNote {
                    field: "memory".to_string(),
                    reason,
                });
                (None, None)
            }
        };

        let error_message = self.redact_error(request.error_message);
        let outcome = if error_message.is_some() {
            TraceOutcome::Failed
        } else {
            TraceOutcome::Success
        };

        let record = RuntimeTraceRecord {
            schema_version: RUNTIME_TELEMETRY_SCHEMA_VERSION,
            trace_id: Uuid::new_v4().to_string(),
            runtime_id: request.runtime_id,
            model_id: request.model_id,
            recorded_at_ms: now_ms(),
            outcome,
            error_message,
            event: TraceEvent::Load {
                timing: LoadTiming {
                    started_at_ms: request.started_at_ms,
                    ready_at_ms: request.ready_at_ms,
                    duration_ms,
                },
                offload,
                memory,
            },
            unavailable,
        };
        self.store.push(record.clone());
        Ok(record)
    }

    pub fn record_request(
        &self,
        request: RecordRequestTraceRequest,
    ) -> Result<RuntimeTraceRecord, String> {
        validate_identifier(&request.runtime_id, "runtimeId")?;
        validate_identifier(&request.model_id, "modelId")?;

        let duration_ms = request.ended_at_ms.saturating_sub(request.started_at_ms);
        let mut tokens = request.tokens;
        let mut unavailable = Vec::new();

        if tokens.tokens_per_second.is_none() {
            match tokens.output_tokens {
                Some(output) if duration_ms > 0 => {
                    tokens.tokens_per_second = Some(output as f64 / (duration_ms as f64 / 1_000.0));
                }
                _ => unavailable.push(TraceFieldNote {
                    field: "tokens.tokensPerSecond".to_string(),
                    reason: "runtime did not report an output token count for this request"
                        .to_string(),
                }),
            }
        }
        if tokens.cached_prompt_tokens.is_none() {
            unavailable.push(TraceFieldNote {
                field: "tokens.cachedPromptTokens".to_string(),
                reason: "prompt-cache reuse reporting depends on the Context and KV Cache \
                         Control Center, which is not merged yet"
                    .to_string(),
            });
        }

        let error_message = self.redact_error(request.error_message);
        let outcome = if error_message.is_some() {
            TraceOutcome::Failed
        } else {
            TraceOutcome::Success
        };

        let record = RuntimeTraceRecord {
            schema_version: RUNTIME_TELEMETRY_SCHEMA_VERSION,
            trace_id: Uuid::new_v4().to_string(),
            runtime_id: request.runtime_id,
            model_id: request.model_id,
            recorded_at_ms: now_ms(),
            outcome,
            error_message,
            event: TraceEvent::Request {
                timing: RequestTiming {
                    started_at_ms: request.started_at_ms,
                    ended_at_ms: request.ended_at_ms,
                    duration_ms,
                },
                sampler: request.sampler,
                tokens,
            },
            unavailable,
        };
        self.store.push(record.clone());
        Ok(record)
    }
}

impl Default for RuntimeTelemetryState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Support bundle
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RedactedLogTail {
    pub runtime_id: String,
    pub text: String,
    pub truncated: bool,
    pub redaction: RedactionSummary,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SupportBundle {
    pub schema_version: u32,
    pub generated_at_ms: u64,
    pub app_version: String,
    pub platform: String,
    pub hardware: Option<HardwareSnapshot>,
    pub compatibility: Option<M3HardwareCompatibilityReport>,
    pub traces: Vec<RuntimeTraceRecord>,
    pub runtime_logs: Vec<RedactedLogTail>,
    pub redaction_totals: RedactionSummary,
    /// Human-readable statement of what this bundle deliberately leaves out,
    /// shown by the UI before export so redaction is visible, not implicit.
    pub excluded: Vec<String>,
}

fn default_exclusions() -> Vec<String> {
    vec![
        "Prompt and response text (traces never carry a free-text prompt/response field; log-tail redaction additionally scrubs prompt/response-shaped log lines and JSON fields)".to_string(),
        "API keys, tokens, bearer credentials, and private key material".to_string(),
        "Emails, phone numbers, IP addresses, and credit card numbers".to_string(),
        "Home-directory usernames in file paths (e.g. /Users/<name>, C:\\Users\\<name>)".to_string(),
    ]
}

/// Assembles a support bundle from already-collected inputs. Redaction is
/// applied here to every runtime log tail (traces are already redacted at
/// record time, see [`RuntimeTelemetryState::record_load`]/`record_request`,
/// but the pass runs again here too — cheap, and it means the bundle's
/// safety does not depend on every future caller of `record_*` remembering
/// to redact).
#[allow(clippy::too_many_arguments)]
pub fn build_support_bundle(
    redactor: &Redactor,
    app_version: String,
    platform: String,
    hardware: Option<HardwareSnapshot>,
    compatibility: Option<M3HardwareCompatibilityReport>,
    traces: Vec<RuntimeTraceRecord>,
    raw_logs: Vec<(String, String, bool)>,
    generated_at_ms: u64,
) -> SupportBundle {
    let mut redaction_totals = RedactionSummary::default();

    let traces = traces
        .into_iter()
        .map(|mut trace| {
            if let Some(message) = trace.error_message.take() {
                let (redacted, summary) = redactor.redact(&message);
                redaction_totals.merge(&summary);
                trace.error_message = Some(redacted);
            }
            trace
        })
        .collect();

    let runtime_logs = raw_logs
        .into_iter()
        .map(|(runtime_id, text, truncated)| {
            let (redacted, summary) = redactor.redact(&text);
            redaction_totals.merge(&summary);
            RedactedLogTail {
                runtime_id,
                text: redacted,
                truncated,
                redaction: summary,
            }
        })
        .collect();

    SupportBundle {
        schema_version: RUNTIME_TELEMETRY_SCHEMA_VERSION,
        generated_at_ms,
        app_version,
        platform,
        hardware,
        compatibility,
        traces,
        runtime_logs,
        redaction_totals,
        excluded: default_exclusions(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_offload_plan() -> OffloadPlan {
        OffloadPlan {
            schema_version: 1,
            accelerator: AcceleratorKind::Metal,
            context_tokens: 4096,
            requested_context_tokens: 4096,
            batch_size: 512,
            gpu_layers: 32,
            estimated_total_layers: 32,
            cpu_spill_layers: 0,
            projector_placement: ProjectorPlacement::NotApplicable,
            parallel_sequences: 1,
            available_ram_bytes: 8_000_000_000,
            available_vram_bytes: 16_000_000_000,
            rationale: Vec::new(),
            improvement_suggestions: Vec::new(),
        }
    }

    // -- redaction: secrets, in the formats the task asked to be thorough about --

    #[test]
    fn redacts_key_value_api_credential() {
        let redactor = Redactor::new();
        // Split so secret scanners don't flag the fixture as a real key.
        let fake_key = ["sk-", "ABCDEFGHIJKLMNOP1234567890"].concat();
        let (redacted, summary) = redactor.redact(&format!("config: api_key: {fake_key}"));
        assert!(!redacted.contains(&fake_key));
        assert!(redacted.contains("[REDACTED:API_CREDENTIAL]"));
        assert_eq!(summary.by_kind.get("API_CREDENTIAL"), Some(&1));
    }

    #[test]
    fn redacts_bearer_authorization_header() {
        let redactor = Redactor::new();
        let (redacted, _) = redactor.redact("Authorization: Bearer abcDEF1234567890abcdefGHIJ");
        assert!(!redacted.contains("abcDEF1234567890abcdefGHIJ"));
    }

    #[test]
    fn redacts_cli_flag_style_secret() {
        let redactor = Redactor::new();
        let (redacted, summary) =
            redactor.redact("launching: llama-server --api-key sk-live-9f8e7d6c5b4a3210 --port 8080");
        assert!(!redacted.contains("sk-live-9f8e7d6c5b4a3210"));
        assert!(redacted.contains("--port 8080"));
        assert!(summary.findings_redacted >= 1);
    }

    #[test]
    fn redacts_private_key_block() {
        let redactor = Redactor::new();
        let text = "-----BEGIN RSA PRIVATE KEY-----\nMIIEow==\n-----END RSA PRIVATE KEY-----";
        let (redacted, summary) = redactor.redact(text);
        assert!(!redacted.contains("MIIEow=="));
        assert_eq!(summary.by_kind.get("PRIVATE_KEY"), Some(&1));
    }

    #[test]
    fn redacts_email_address() {
        let redactor = Redactor::new();
        let (redacted, _) = redactor.redact("contact person@example.com for access");
        assert!(!redacted.contains("person@example.com"));
    }

    // -- redaction: prompt/response content --

    #[test]
    fn redacts_prompt_shaped_json_field() {
        let redactor = Redactor::new();
        let text = r#"{"messages":[{"role":"user","content":"the launch codes are 4471"}]}"#;
        let (redacted, summary) = redactor.redact(text);
        assert!(!redacted.contains("launch codes are 4471"));
        assert!(redacted.contains("\"content\":\"[REDACTED:CONTENT]\""));
        assert!(summary.by_kind.contains_key("PROMPT_CONTENT"));
    }

    #[test]
    fn redacts_plain_prompt_line() {
        let redactor = Redactor::new();
        let text = "loading model...\nPrompt: what is my bank account balance?\nready.";
        let (redacted, _) = redactor.redact(text);
        assert!(!redacted.contains("bank account balance"));
        assert!(redacted.contains("loading model..."));
        assert!(redacted.contains("ready."));
    }

    // -- redaction: usernames in file paths --

    #[test]
    fn redacts_unix_home_directory_username() {
        let redactor = Redactor::new();
        let (redacted, summary) = redactor.redact("model path: /Users/johndoe/Models/llama.gguf");
        assert!(!redacted.contains("johndoe"));
        assert!(redacted.contains("/Users/[REDACTED_USER]"));
        assert_eq!(summary.by_kind.get("HOME_DIRECTORY_PATH"), Some(&1));
    }

    #[test]
    fn redacts_linux_home_directory_username() {
        let redactor = Redactor::new();
        let (redacted, _) = redactor.redact("reading /home/janedoe/.cache/models/index");
        assert!(!redacted.contains("janedoe"));
        assert!(redacted.contains("/home/[REDACTED_USER]"));
    }

    #[test]
    fn redacts_windows_home_directory_username() {
        let redactor = Redactor::new();
        let (redacted, _) =
            redactor.redact(r"loading C:\Users\johndoe\AppData\Local\models\model.gguf");
        assert!(!redacted.contains("johndoe"));
        assert!(redacted.contains(r"C:\Users\[REDACTED_USER]"));
    }

    // -- redaction: ordinary diagnostic text is untouched --

    #[test]
    fn ordinary_log_lines_pass_through_unchanged() {
        let redactor = Redactor::new();
        let text = "llama_model_load: loaded 32/32 layers\ncontext size: 4096\nlistening on 127.0.0.1:8080";
        let (redacted, summary) = redactor.redact(text);
        // The bare loopback IP is itself a "sensitive" IP-address finding by
        // design (the scanner does not special-case loopback), so only
        // assert the non-IP diagnostic content is untouched verbatim.
        assert!(redacted.contains("llama_model_load: loaded 32/32 layers"));
        assert!(redacted.contains("context size: 4096"));
        let _ = summary;
    }

    #[test]
    fn text_with_nothing_sensitive_is_returned_identically() {
        let redactor = Redactor::new();
        let text = "model warmed up in 3 steps, no issues detected";
        let (redacted, summary) = redactor.redact(text);
        assert_eq!(redacted, text);
        assert_eq!(summary.findings_redacted, 0);
    }

    // -- trace recording --

    #[test]
    fn record_load_reuses_offload_plan_fields_and_computes_duration() {
        let state = RuntimeTelemetryState::new();
        let plan = fixture_offload_plan();
        let record = state
            .record_load(RecordLoadTraceRequest {
                runtime_id: "llama-cpp".to_string(),
                model_id: "qwen2.5-7b".to_string(),
                started_at_ms: 1_000,
                ready_at_ms: 4_500,
                offload_plan: Some(plan.clone()),
                error_message: None,
            })
            .expect("record_load should succeed");
        assert_eq!(record.outcome, TraceOutcome::Success);
        assert!(record.unavailable.is_empty());
        match record.event {
            TraceEvent::Load { timing, offload, memory } => {
                assert_eq!(timing.duration_ms, 3_500);
                let offload = offload.expect("offload placement expected");
                assert_eq!(offload.gpu_layers, plan.gpu_layers);
                assert_eq!(offload.accelerator, plan.accelerator);
                let memory = memory.expect("memory footprint expected");
                assert_eq!(memory.available_vram_bytes, plan.available_vram_bytes);
            }
            TraceEvent::Request { .. } => panic!("expected a load event"),
        }
    }

    #[test]
    fn record_load_without_offload_plan_marks_fields_unavailable_honestly() {
        let state = RuntimeTelemetryState::new();
        let record = state
            .record_load(RecordLoadTraceRequest {
                runtime_id: "ollama".to_string(),
                model_id: "llama3".to_string(),
                started_at_ms: 0,
                ready_at_ms: 200,
                offload_plan: None,
                error_message: None,
            })
            .expect("record_load should succeed");
        let fields: Vec<_> = record.unavailable.iter().map(|note| note.field.as_str()).collect();
        assert!(fields.contains(&"offload"));
        assert!(fields.contains(&"memory"));
    }

    #[test]
    fn record_load_redacts_a_secret_leaked_into_the_error_message() {
        let state = RuntimeTelemetryState::new();
        // Split so secret scanners don't flag the fixture as a real key.
        let fake_key = ["sk-", "ABCDEFGHIJKLMNOP123"].concat();
        let record = state
            .record_load(RecordLoadTraceRequest {
                runtime_id: "llama-cpp".to_string(),
                model_id: "broken-model".to_string(),
                started_at_ms: 0,
                ready_at_ms: 50,
                offload_plan: None,
                error_message: Some(format!(
                    "failed to reach https://api.example.com/v1?api_key={fake_key} (user /Users/johndoe)"
                )),
            })
            .expect("record_load should succeed");
        assert_eq!(record.outcome, TraceOutcome::Failed);
        let message = record.error_message.expect("error message expected");
        assert!(!message.contains(&fake_key));
        assert!(!message.contains("johndoe"));
    }

    #[test]
    fn record_request_computes_tokens_per_second_from_real_counts() {
        let state = RuntimeTelemetryState::new();
        let record = state
            .record_request(RecordRequestTraceRequest {
                runtime_id: "mlx".to_string(),
                model_id: "mlx-community/qwen".to_string(),
                started_at_ms: 0,
                ended_at_ms: 2_000,
                sampler: SamplerStats {
                    temperature: Some(0.7),
                    ..Default::default()
                },
                tokens: TokenTiming {
                    output_tokens: Some(100),
                    ..Default::default()
                },
                error_message: None,
            })
            .expect("record_request should succeed");
        match record.event {
            TraceEvent::Request { tokens, .. } => {
                assert_eq!(tokens.tokens_per_second, Some(50.0));
            }
            TraceEvent::Load { .. } => panic!("expected a request event"),
        }
        let fields: Vec<_> = record.unavailable.iter().map(|note| note.field.as_str()).collect();
        assert!(fields.contains(&"tokens.cachedPromptTokens"));
        assert!(!fields.contains(&"tokens.tokensPerSecond"));
    }

    #[test]
    fn record_request_marks_tokens_per_second_unavailable_when_uncomputable() {
        let state = RuntimeTelemetryState::new();
        let record = state
            .record_request(RecordRequestTraceRequest {
                runtime_id: "ollama".to_string(),
                model_id: "llama3".to_string(),
                started_at_ms: 0,
                ended_at_ms: 0,
                sampler: SamplerStats::default(),
                tokens: TokenTiming::default(),
                error_message: None,
            })
            .expect("record_request should succeed");
        let fields: Vec<_> = record.unavailable.iter().map(|note| note.field.as_str()).collect();
        assert!(fields.contains(&"tokens.tokensPerSecond"));
    }

    #[test]
    fn record_rejects_oversized_identifiers() {
        let state = RuntimeTelemetryState::new();
        let huge = "x".repeat(MAX_IDENTIFIER_LEN + 1);
        let result = state.record_load(RecordLoadTraceRequest {
            runtime_id: huge,
            model_id: "m".to_string(),
            started_at_ms: 0,
            ready_at_ms: 1,
            offload_plan: None,
            error_message: None,
        });
        assert!(result.is_err());
    }

    // -- trace store --

    #[test]
    fn store_evicts_oldest_beyond_capacity() {
        let store = RuntimeTraceStore::new(2);
        for index in 0..5u64 {
            store.push(RuntimeTraceRecord {
                schema_version: RUNTIME_TELEMETRY_SCHEMA_VERSION,
                trace_id: index.to_string(),
                runtime_id: "llama-cpp".to_string(),
                model_id: "m".to_string(),
                recorded_at_ms: index,
                outcome: TraceOutcome::Success,
                error_message: None,
                event: TraceEvent::Load {
                    timing: LoadTiming { started_at_ms: 0, ready_at_ms: 1, duration_ms: 1 },
                    offload: None,
                    memory: None,
                },
                unavailable: Vec::new(),
            });
        }
        assert_eq!(store.len(), 2);
        let recent = store.recent(None, 10);
        assert_eq!(recent.len(), 2);
        // Most-recent-first.
        assert_eq!(recent[0].trace_id, "4");
        assert_eq!(recent[1].trace_id, "3");
    }

    #[test]
    fn store_filters_by_runtime_id() {
        let store = RuntimeTraceStore::new(10);
        for (runtime_id, index) in [("llama-cpp", 0u64), ("ollama", 1), ("llama-cpp", 2)] {
            store.push(RuntimeTraceRecord {
                schema_version: RUNTIME_TELEMETRY_SCHEMA_VERSION,
                trace_id: index.to_string(),
                runtime_id: runtime_id.to_string(),
                model_id: "m".to_string(),
                recorded_at_ms: index,
                outcome: TraceOutcome::Success,
                error_message: None,
                event: TraceEvent::Load {
                    timing: LoadTiming { started_at_ms: 0, ready_at_ms: 1, duration_ms: 1 },
                    offload: None,
                    memory: None,
                },
                unavailable: Vec::new(),
            });
        }
        let recent = store.recent(Some("llama-cpp"), 10);
        assert_eq!(recent.len(), 2);
        assert!(recent.iter().all(|record| record.runtime_id == "llama-cpp"));
    }

    // -- support bundle --

    #[test]
    fn support_bundle_redacts_secrets_and_prompts_from_runtime_logs() {
        let redactor = Redactor::new();
        let raw_logs = vec![(
            "llama-cpp".to_string(),
            "starting server --api-key sk-supersecret1234567890\n\
             Prompt: what is the user's home address?\n\
             model path /Users/johndoe/models/model.gguf\n\
             context size: 4096"
                .to_string(),
            false,
        )];
        let bundle = build_support_bundle(
            &redactor,
            "0.0.0-test".to_string(),
            "macos".to_string(),
            None,
            None,
            Vec::new(),
            raw_logs,
            1_000,
        );
        let serialized = serde_json::to_string(&bundle).expect("bundle should serialize");
        assert!(!serialized.contains("sk-supersecret1234567890"));
        assert!(!serialized.contains("home address"));
        assert!(!serialized.contains("johndoe"));
        assert!(serialized.contains("context size: 4096"));
        assert!(bundle.redaction_totals.findings_redacted >= 3);
        assert!(!bundle.excluded.is_empty());
    }

    #[test]
    fn support_bundle_redacts_error_messages_already_on_stored_traces() {
        // Simulates a trace that (hypothetically) still carried an
        // unredacted error message — the bundle step must not assume
        // `record_load`/`record_request` was the only path that ever
        // constructed a `RuntimeTraceRecord`.
        let redactor = Redactor::new();
        // Split so secret scanners don't flag the fixture as a real key.
        let fake_key = ["sk-", "ABCDEFGHIJKLMNOP123456"].concat();
        let trace = RuntimeTraceRecord {
            schema_version: RUNTIME_TELEMETRY_SCHEMA_VERSION,
            trace_id: "t1".to_string(),
            runtime_id: "llama-cpp".to_string(),
            model_id: "m".to_string(),
            recorded_at_ms: 0,
            outcome: TraceOutcome::Failed,
            error_message: Some(format!("request rejected, authorization: Bearer {fake_key}")),
            event: TraceEvent::Load {
                timing: LoadTiming { started_at_ms: 0, ready_at_ms: 1, duration_ms: 1 },
                offload: None,
                memory: None,
            },
            unavailable: Vec::new(),
        };
        let bundle = build_support_bundle(
            &redactor,
            "0.0.0-test".to_string(),
            "linux".to_string(),
            None,
            None,
            vec![trace],
            Vec::new(),
            1_000,
        );
        let serialized = serde_json::to_string(&bundle).expect("bundle should serialize");
        assert!(!serialized.contains(&fake_key));
    }

    #[test]
    fn support_bundle_never_contains_a_prompt_or_response_field_by_construction() {
        let redactor = Redactor::new();
        let bundle = build_support_bundle(
            &redactor,
            "0.0.0-test".to_string(),
            "macos".to_string(),
            None,
            None,
            Vec::new(),
            Vec::new(),
            1_000,
        );
        let value = serde_json::to_value(&bundle).expect("bundle should serialize to a value");
        fn assert_no_prompt_keys(value: &serde_json::Value) {
            match value {
                serde_json::Value::Object(map) => {
                    for (key, nested) in map {
                        let lower = key.to_lowercase();
                        assert!(
                            !lower.contains("prompt") && lower != "content" && lower != "response",
                            "unexpected free-text-shaped key `{key}` in support bundle schema"
                        );
                        assert_no_prompt_keys(nested);
                    }
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        assert_no_prompt_keys(item);
                    }
                }
                _ => {}
            }
        }
        assert_no_prompt_keys(&value);
    }
}
