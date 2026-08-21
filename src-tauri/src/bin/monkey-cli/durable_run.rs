//! Tauri-free durable-run adapter for `monkey-cli task run`.
//!
//! The desktop, CLI, scheduler, and future daemon all write the same
//! [`RunSpec`]/[`RunEventEnvelope`] contract into the same [`RunLedger`].
//! This module owns the parts that must never be delegated to model output:
//! event ids, sequence numbers, timestamps, emitter identity, append order,
//! and terminal-state selection. Every append is committed synchronously so
//! a process crash can lose at most work that had not yet crossed an observed
//! callback boundary.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use little_monkey_lib::run_ledger::{RunLedger, StoredRun};
use little_monkey_lib::run_protocol::{
    ClientIdentity, OutputChannel, RedactedPayload, RedactionState, RunEvent, RunEventEnvelope,
    RunSpec, RunStatus, UsageSnapshot, RUN_PROTOCOL_SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_REDACTED_ARGUMENT_BYTES: usize = 240 * 1024;
const MAX_NORMALIZED_TEXT_BYTES: usize = 1024 * 1024;

/// A fallible, synchronous boundary. Tool execution must not continue after
/// an audit append fails: doing so would create effects with no durable
/// explanation of how they were authorized.
pub trait CliRunEventSink: Send + Sync {
    fn emit(&self, event: RunEvent) -> Result<(), String>;
    fn current_usage(&self) -> Result<UsageSnapshot, String>;
    fn client_identity(&self) -> ClientIdentity;
    fn run_id(&self) -> String;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmissionDisposition {
    Ready { resumed_before_start: bool },
    AlreadyTerminal(RunStatus),
    InterruptedReplayRefused,
}

struct RecorderState {
    ledger: RunLedger,
    next_sequence: u64,
    last_occurred_at_ms: u64,
    usage: UsageSnapshot,
}

/// Thread-safe because permission prompts and the agent loop share one sink.
/// SQLite still serializes the actual write transaction.
pub struct DurableRunRecorder {
    run_id: String,
    actor_id: String,
    emitter: ClientIdentity,
    state: Mutex<RecorderState>,
}

impl DurableRunRecorder {
    /// Attach a daemon/control-plane emitter to an already submitted run.
    /// This does not submit or replay anything; it only reconstructs the
    /// host-authoritative next sequence from the shared ledger. It is used
    /// for pause/resume/cancel/approval decisions while the supervised task
    /// child remains the execution engine.
    pub fn attach(
        ledger: RunLedger,
        run_id: &str,
        actor_id: String,
        emitter: ClientIdentity,
    ) -> Result<Arc<Self>, String> {
        emitter.validate().map_err(|error| error.to_string())?;
        let stored = ledger
            .load_run(run_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("Unknown durable run '{run_id}'"))?;
        let usage = latest_usage(&ledger, &stored)?;
        Ok(Arc::new(Self {
            run_id: run_id.to_string(),
            actor_id,
            emitter,
            state: Mutex::new(RecorderState {
                ledger,
                next_sequence: stored
                    .last_sequence
                    .checked_add(1)
                    .ok_or_else(|| "run event sequence overflow".to_string())?,
                last_occurred_at_ms: stored.updated_at_ms,
                usage,
            }),
        }))
    }

    /// Submit `spec` idempotently and decide whether execution may start.
    ///
    /// A retry may resume only before `Started` was durably written. Once a
    /// previous process crossed that boundary we cannot prove whether a tool
    /// effect happened between two callbacks, so replay is refused and the
    /// run terminates with an inspection-required failure instead.
    pub fn submit(
        mut ledger: RunLedger,
        spec: &RunSpec,
        actor_id: String,
    ) -> Result<(Arc<Self>, SubmissionDisposition), String> {
        let outcome = ledger.submit_run(spec).map_err(|error| error.to_string())?;
        let stored = outcome.run;
        let usage = latest_usage(&ledger, &stored)?;
        let recorder = Arc::new(Self {
            run_id: spec.run_id.clone(),
            actor_id,
            emitter: spec.submitted_by.clone(),
            state: Mutex::new(RecorderState {
                ledger,
                next_sequence: stored
                    .last_sequence
                    .checked_add(1)
                    .ok_or_else(|| "run event sequence overflow".to_string())?,
                last_occurred_at_ms: stored.updated_at_ms,
                usage,
            }),
        });

        if stored.status.is_terminal() {
            return Ok((
                recorder,
                SubmissionDisposition::AlreadyTerminal(stored.status),
            ));
        }

        if outcome.inserted || stored.last_sequence == 0 {
            recorder.emit(RunEvent::Queued {
                queue: Some("cli-task".to_string()),
            })?;
            return Ok((
                recorder,
                SubmissionDisposition::Ready {
                    resumed_before_start: !outcome.inserted,
                },
            ));
        }

        if is_only_queued(&recorder, &stored)? {
            return Ok((
                recorder,
                SubmissionDisposition::Ready {
                    resumed_before_start: true,
                },
            ));
        }

        recorder.emit(RunEvent::Failed {
            code: "interrupted_run".to_string(),
            message: "A previous process wrote Started or later events. Automatic replay is refused because the CLI cannot prove that no tool side effect occurred after the last durable callback.".to_string(),
            retryable: false,
        })?;
        Ok((recorder, SubmissionDisposition::InterruptedReplayRefused))
    }

    pub fn terminal_summary(&self) -> Result<Option<String>, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "durable run recorder lock was poisoned".to_string())?;
        let mut summary = None;
        visit_event_pages(&state.ledger, &self.run_id, |envelope| {
            if let Some(value) = match &envelope.event {
                RunEvent::Completed { summary, .. } => summary.as_ref(),
                RunEvent::Failed { message, .. } => Some(message),
                RunEvent::Cancelled { reason } => reason.as_ref(),
                RunEvent::NeedsReconciliation { reason, .. } => Some(reason),
                _ => None,
            } {
                summary = Some(value.clone());
            }
        })?;
        Ok(summary)
    }

    pub fn latest_checkpoint_id(&self) -> Result<Option<String>, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "durable run recorder lock was poisoned".to_string())?;
        let mut checkpoint_id = None;
        visit_event_pages(&state.ledger, &self.run_id, |envelope| {
            if let RunEvent::CheckpointLinked {
                checkpoint_id: value,
                ..
            } = &envelope.event
            {
                checkpoint_id = Some(value.clone());
            }
        })?;
        Ok(checkpoint_id)
    }

    fn append_locked(
        state: &mut RecorderState,
        run_id: &str,
        actor_id: &str,
        emitter: &ClientIdentity,
        event: RunEvent,
    ) -> Result<(), String> {
        // A daemon controller may append a permission/pause/cancel event
        // while the task child is waiting. Refresh before assigning sequence
        // so the child and controller remain one contiguous stream instead
        // of racing on a cached sequence number.
        let stored = state
            .ledger
            .load_run(run_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("Unknown durable run '{run_id}'"))?;
        let authoritative_next = stored
            .last_sequence
            .checked_add(1)
            .ok_or_else(|| "run event sequence overflow".to_string())?;
        if authoritative_next != state.next_sequence {
            state.next_sequence = authoritative_next;
            state.last_occurred_at_ms = state.last_occurred_at_ms.max(stored.updated_at_ms);
            state.usage = latest_usage(&state.ledger, &stored)?;
        }
        let now = unix_time_ms()?;
        let occurred_at_ms = now.max(state.last_occurred_at_ms.saturating_add(1));
        let sequence = state.next_sequence;
        let event_id = format!("evt-{}-{sequence}", &sha256_hex(run_id.as_bytes())[..24]);
        let envelope = RunEventEnvelope {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            event_id,
            run_id: run_id.to_string(),
            sequence,
            occurred_at_ms,
            actor_id: Some(actor_id.to_string()),
            emitter: emitter.clone(),
            event,
        };
        state
            .ledger
            .append_event(&envelope)
            .map_err(|error| error.to_string())?;
        state.next_sequence = sequence
            .checked_add(1)
            .ok_or_else(|| "run event sequence overflow".to_string())?;
        state.last_occurred_at_ms = occurred_at_ms;
        if let RunEvent::UsageRecorded { usage } | RunEvent::Completed { usage, .. } =
            &envelope.event
        {
            state.usage = usage.clone();
        }
        Ok(())
    }
}

impl CliRunEventSink for DurableRunRecorder {
    fn emit(&self, event: RunEvent) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "durable run recorder lock was poisoned".to_string())?;
        Self::append_locked(
            &mut state,
            &self.run_id,
            &self.actor_id,
            &self.emitter,
            event,
        )
    }

    fn current_usage(&self) -> Result<UsageSnapshot, String> {
        self.state
            .lock()
            .map(|state| state.usage.clone())
            .map_err(|_| "durable run recorder lock was poisoned".to_string())
    }

    fn client_identity(&self) -> ClientIdentity {
        self.emitter.clone()
    }

    fn run_id(&self) -> String {
        self.run_id.clone()
    }
}

fn latest_usage(ledger: &RunLedger, stored: &StoredRun) -> Result<UsageSnapshot, String> {
    let mut latest = None;
    visit_event_pages(ledger, &stored.spec.run_id, |envelope| {
        if let RunEvent::UsageRecorded { usage } | RunEvent::Completed { usage, .. } =
            &envelope.event
        {
            latest = Some(usage.clone());
        }
    })?;
    Ok(latest.unwrap_or_else(zero_usage))
}

fn visit_event_pages(
    ledger: &RunLedger,
    run_id: &str,
    mut visitor: impl FnMut(&RunEventEnvelope),
) -> Result<(), String> {
    let mut after_sequence = 0;
    loop {
        let page = ledger
            .load_events(run_id, after_sequence, 1_000)
            .map_err(|error| error.to_string())?;
        if page.is_empty() {
            return Ok(());
        }
        for envelope in &page {
            visitor(envelope);
        }
        after_sequence = page
            .last()
            .map(|event| event.sequence)
            .unwrap_or(after_sequence);
        if page.len() < 1_000 {
            return Ok(());
        }
    }
}

fn is_only_queued(recorder: &DurableRunRecorder, stored: &StoredRun) -> Result<bool, String> {
    if stored.last_sequence != 1 || stored.status != RunStatus::Queued {
        return Ok(false);
    }
    let state = recorder
        .state
        .lock()
        .map_err(|_| "durable run recorder lock was poisoned".to_string())?;
    let events = state
        .ledger
        .load_events(&recorder.run_id, 0, 2)
        .map_err(|error| error.to_string())?;
    Ok(matches!(
        events.as_slice(),
        [RunEventEnvelope {
            event: RunEvent::Queued { .. },
            ..
        }]
    ))
}

pub fn zero_usage() -> UsageSnapshot {
    UsageSnapshot {
        input_tokens: 0,
        output_tokens: 0,
        cached_input_tokens: 0,
        model_calls: 0,
        tool_calls: 0,
        cost_micros: None,
    }
}

pub fn unix_time_ms() -> Result<u64, String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("System clock is before the Unix epoch: {error}"))?
        .as_millis();
    u64::try_from(millis).map_err(|_| "System timestamp exceeds the run protocol".to_string())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Preserve a protocol-safe value for audit readability; otherwise use a
/// deterministic digest alias instead of dropping the event.
pub fn safe_protocol_id(prefix: &str, value: &str) -> String {
    if little_monkey_lib::run_protocol::validate_protocol_id("id", value).is_ok() {
        return value.to_string();
    }
    format!("{prefix}-{}", &sha256_hex(value.as_bytes())[..32])
}

fn canonical_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(canonical_json).collect())
        }
        serde_json::Value::Object(values) => {
            let sorted: BTreeMap<String, serde_json::Value> = values
                .iter()
                .map(|(key, value)| (key.clone(), canonical_json(value)))
                .collect();
            serde_json::to_value(sorted).expect("BTreeMap JSON serialization cannot fail")
        }
        scalar => scalar.clone(),
    }
}

fn canonical_argument_bytes(raw: &str) -> Vec<u8> {
    serde_json::from_str::<serde_json::Value>(raw)
        .map(|value| {
            serde_json::to_vec(&canonical_json(&value)).expect("JSON serialization cannot fail")
        })
        .unwrap_or_else(|_| raw.as_bytes().to_vec())
}

fn is_secret_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
    [
        "apikey",
        "token",
        "password",
        "secret",
        "authorization",
        "credential",
        "cookie",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn redact_json(tool: &str, value: &serde_json::Value) -> (serde_json::Value, bool) {
    match value {
        serde_json::Value::Array(values) => {
            let mut changed = false;
            let redacted = values
                .iter()
                .map(|value| {
                    let (value, child_changed) = redact_json(tool, value);
                    changed |= child_changed;
                    value
                })
                .collect();
            (serde_json::Value::Array(redacted), changed)
        }
        serde_json::Value::Object(values) => {
            let mut changed = false;
            let mut redacted = serde_json::Map::new();
            let mut keys: Vec<_> = values.keys().collect();
            keys.sort();
            for key in keys {
                let lower = key.to_ascii_lowercase();
                let sensitive_tool_input =
                    matches!(
                        (tool, lower.as_str()),
                        ("write_file", "content")
                            | ("edit_file", "old_string")
                            | ("edit_file", "new_string")
                            | ("run_shell", "command")
                            | ("remember", "text")
                            | ("task", "prompt")
                    ) || matches!(lower.as_str(), "headers" | "cookie" | "cookies");
                if is_secret_key(key) || sensitive_tool_input {
                    redacted.insert(
                        key.clone(),
                        serde_json::Value::String("[REDACTED]".to_string()),
                    );
                    changed = true;
                } else {
                    let (value, child_changed) = redact_json(tool, &values[key]);
                    changed |= child_changed;
                    redacted.insert(key.clone(), value);
                }
            }
            (serde_json::Value::Object(redacted), changed)
        }
        scalar => (scalar.clone(), false),
    }
}

/// Returns a bounded, explicitly-redacted event payload plus the digest of
/// the exact canonical arguments. The digest still binds approvals to the
/// original operation even when a secret-looking field is removed.
pub fn redacted_tool_arguments(tool: &str, raw: &str) -> (RedactedPayload, String) {
    let canonical = canonical_argument_bytes(raw);
    let digest = sha256_hex(&canonical);
    let parsed = serde_json::from_slice::<serde_json::Value>(&canonical)
        .unwrap_or_else(|_| serde_json::json!({ "unparsed": "[REDACTED NON-JSON ARGUMENTS]" }));
    let (mut value, changed) = if tool.starts_with("mcp__") {
        (
            serde_json::json!({
                "summary": "MCP arguments are omitted from the durable event",
                "sha256": digest.clone(),
            }),
            true,
        )
    } else {
        redact_json(tool, &parsed)
    };
    let mut applied = changed || serde_json::from_str::<serde_json::Value>(raw).is_err();
    if serde_json::to_vec(&value).map_or(true, |bytes| bytes.len() > MAX_REDACTED_ARGUMENT_BYTES) {
        value = serde_json::json!({
            "summary": "arguments omitted because the redacted payload exceeded the durable event limit",
            "sha256": digest.clone(),
        });
        applied = true;
    }
    (
        RedactedPayload {
            value,
            redaction: if applied {
                RedactionState::Applied
            } else {
                RedactionState::NotNeeded
            },
        },
        digest,
    )
}

/// Approval binding: length-delimited canonical tool name, arguments, and
/// the frozen workspace scope. Length prefixes prevent concatenation
/// ambiguity without depending on a JSON object's insertion order.
pub fn operation_sha256(tool: &str, raw_arguments: &str, scope: &str) -> String {
    let arguments = canonical_argument_bytes(raw_arguments);
    let mut binding = Vec::new();
    for part in [tool.as_bytes(), arguments.as_slice(), scope.as_bytes()] {
        binding.extend_from_slice(&(part.len() as u64).to_be_bytes());
        binding.extend_from_slice(part);
    }
    sha256_hex(&binding)
}

pub fn bounded_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let suffix = "\n… (truncated for durable event)";
    let mut end = max_bytes.saturating_sub(suffix.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &value[..end], suffix)
}

pub fn bounded_single_line(value: &str, max_bytes: usize) -> String {
    let sanitized: String = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    if sanitized.len() <= max_bytes {
        return sanitized;
    }
    let suffix = "...";
    let mut end = max_bytes.saturating_sub(suffix.len());
    while end > 0 && !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &sanitized[..end], suffix)
}

pub fn model_delta_chunks(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + 60 * 1024).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = text[start..]
                .char_indices()
                .nth(1)
                .map(|(offset, _)| start + offset)
                .unwrap_or(text.len());
        }
        chunks.push(text[start..end].to_string());
        start = end;
    }
    chunks
}

/// A deliberately id/timestamp-free semantic representation used to pin
/// desktop/CLI conformance even when one transport emits token-sized deltas
/// and the other observes the completed model response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedSemanticEvent {
    pub kind: String,
    pub detail: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticConformanceFixture {
    pub desktop: Vec<RunEventEnvelope>,
    pub cli: Vec<RunEventEnvelope>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticConformanceReport {
    pub matches: bool,
    pub first_difference: Option<usize>,
    pub desktop: Vec<NormalizedSemanticEvent>,
    pub cli: Vec<NormalizedSemanticEvent>,
}

impl SemanticConformanceFixture {
    pub fn compare(&self) -> SemanticConformanceReport {
        let desktop = normalize_semantic_stream(&self.desktop);
        let cli = normalize_semantic_stream(&self.cli);
        let max = desktop.len().max(cli.len());
        let first_difference = (0..max).find(|index| desktop.get(*index) != cli.get(*index));
        SemanticConformanceReport {
            matches: first_difference.is_none(),
            first_difference,
            desktop,
            cli,
        }
    }
}

fn semantic(kind: &str, detail: serde_json::Value) -> NormalizedSemanticEvent {
    NormalizedSemanticEvent {
        kind: kind.to_string(),
        detail,
    }
}

fn enum_json<T: Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

/// Normalize both clients' envelopes by dropping authority metadata and
/// opaque ids, coalescing streamed model chunks, and retaining the final
/// cumulative usage snapshot exactly once before the terminal outcome.
pub fn normalize_semantic_stream(events: &[RunEventEnvelope]) -> Vec<NormalizedSemanticEvent> {
    let mut normalized = Vec::new();
    let mut model_output = String::new();
    let mut model_channel: Option<OutputChannel> = None;
    let mut model_message_id: Option<String> = None;
    let mut final_usage: Option<UsageSnapshot> = None;
    let mut terminal: Option<NormalizedSemanticEvent> = None;

    let flush_model = |normalized: &mut Vec<NormalizedSemanticEvent>,
                       output: &mut String,
                       channel: &mut Option<OutputChannel>,
                       message_id: &mut Option<String>| {
        if !output.is_empty() {
            let text = if output.len() > MAX_NORMALIZED_TEXT_BYTES {
                bounded_text(output, MAX_NORMALIZED_TEXT_BYTES)
            } else {
                std::mem::take(output)
            };
            normalized.push(semantic(
                "model_output",
                serde_json::json!({ "channel": channel.as_ref(), "text": text }),
            ));
            output.clear();
        }
        *channel = None;
        *message_id = None;
    };

    for envelope in events {
        match &envelope.event {
            RunEvent::ModelDelta {
                message_id,
                channel,
                text,
            } => {
                if model_message_id
                    .as_deref()
                    .is_some_and(|current| current != message_id)
                    || model_channel
                        .as_ref()
                        .is_some_and(|current| current != channel)
                {
                    flush_model(
                        &mut normalized,
                        &mut model_output,
                        &mut model_channel,
                        &mut model_message_id,
                    );
                }
                model_message_id.get_or_insert_with(|| message_id.clone());
                model_channel.get_or_insert_with(|| channel.clone());
                model_output.push_str(text);
            }
            RunEvent::UsageRecorded { usage } => final_usage = Some(usage.clone()),
            event => {
                flush_model(
                    &mut normalized,
                    &mut model_output,
                    &mut model_channel,
                    &mut model_message_id,
                );
                match event {
                    RunEvent::Queued { .. } => normalized.push(semantic("queued", serde_json::json!({}))),
                    RunEvent::Started { .. } => normalized.push(semantic("started", serde_json::json!({}))),
                    // The policy and the sentence, not the chosen key: this
                    // normalization feeds a human-readable replay, and a target
                    // key is an identifier the reader would have to resolve.
                    RunEvent::RoutingDecided { policy_name, reason, .. } => normalized.push(semantic(
                        "routing_decided",
                        serde_json::json!({ "policy": policy_name, "reason": reason }),
                    )),
                    RunEvent::ToolProposed { tool_name, arguments, mutation, .. } => normalized.push(semantic(
                        "tool_proposed",
                        serde_json::json!({ "tool": tool_name, "arguments": arguments.value, "mutation": mutation }),
                    )),
                    RunEvent::PermissionRequested { tool_name, risk_level, .. } => normalized.push(semantic(
                        "permission_requested",
                        serde_json::json!({ "tool": tool_name, "risk": risk_level }),
                    )),
                    RunEvent::AwaitingApproval { .. } => {
                        normalized.push(semantic("awaiting_approval", serde_json::json!({})))
                    }
                    RunEvent::PermissionDecided { decision, .. } => normalized.push(semantic(
                        "permission_decided",
                        serde_json::json!({ "decision": decision }),
                    )),
                    RunEvent::ToolStarted { .. } => normalized.push(semantic("tool_started", serde_json::json!({}))),
                    // The exact version a replay reads back, so a run stays
                    // able to say which skill content it actually ran.
                    RunEvent::SkillInvoked { command, scope, sha256 } => normalized.push(semantic(
                        "skill_invoked",
                        serde_json::json!({ "command": command, "scope": scope, "sha256": sha256 }),
                    )),
                    RunEvent::ToolFinished { outcome, .. } => normalized.push(semantic(
                        "tool_finished",
                        serde_json::json!({ "outcome": outcome }),
                    )),
                    RunEvent::ArtifactAdded { kind, name, media_type, content_sha256, size_bytes, .. } => normalized.push(semantic(
                        "artifact_added",
                        serde_json::json!({ "kind": kind, "name": name, "mediaType": media_type, "sha256": content_sha256, "sizeBytes": size_bytes }),
                    )),
                    RunEvent::CheckpointLinked { kind, .. } => normalized.push(semantic(
                        "checkpoint_linked",
                        serde_json::json!({ "kind": kind }),
                    )),
                    RunEvent::VerificationFinished { name, passed, .. } => normalized.push(semantic(
                        "verification_finished",
                        serde_json::json!({ "name": name, "passed": passed }),
                    )),
                    RunEvent::CancellationRequested { .. } => {
                        normalized.push(semantic("cancellation_requested", serde_json::json!({})))
                    }
                    RunEvent::ExternalMutationPrepared { kind, .. } => normalized.push(semantic(
                        "external_mutation_prepared",
                        serde_json::json!({ "kind": kind }),
                    )),
                    RunEvent::ExternalMutationConfirmed { .. } => normalized.push(semantic(
                        "external_mutation_confirmed",
                        serde_json::json!({}),
                    )),
                    RunEvent::Paused { .. } => normalized.push(semantic("paused", serde_json::json!({}))),
                    RunEvent::Cancelling { .. } => normalized.push(semantic("cancelling", serde_json::json!({}))),
                    RunEvent::Completed { usage, .. } => {
                        final_usage = Some(usage.clone());
                        terminal = Some(semantic("terminal", serde_json::json!({ "status": "succeeded" })));
                    }
                    RunEvent::Failed { code, retryable, .. } => {
                        terminal = Some(semantic("terminal", serde_json::json!({ "status": "failed", "code": code, "retryable": retryable })));
                    }
                    RunEvent::Cancelled { .. } => {
                        terminal = Some(semantic("terminal", serde_json::json!({ "status": "cancelled" })));
                    }
                    RunEvent::NeedsReconciliation { .. } => {
                        terminal = Some(semantic("terminal", serde_json::json!({ "status": "needs_reconciliation" })));
                    }
                    // The node ids, because a replay of a migrated run read on
                    // either machine is otherwise silent about the fact that
                    // half of it happened somewhere else.
                    RunEvent::MigrationDeparted { target_node_id, .. } => normalized.push(semantic(
                        "migration_departed",
                        serde_json::json!({ "target_node_id": target_node_id }),
                    )),
                    RunEvent::MigrationArrived { origin_node_id, origin_last_sequence, .. } => normalized.push(semantic(
                        "migration_arrived",
                        serde_json::json!({ "origin_node_id": origin_node_id, "origin_last_sequence": origin_last_sequence }),
                    )),
                    RunEvent::TaskEvent { event_type, .. } => normalized.push(semantic(
                        "task_event",
                        serde_json::json!({ "event_type": event_type }),
                    )),
                    RunEvent::ModelDelta { .. } | RunEvent::UsageRecorded { .. } => unreachable!(),
                }
            }
        }
    }
    flush_model(
        &mut normalized,
        &mut model_output,
        &mut model_channel,
        &mut model_message_id,
    );
    if let Some(usage) = final_usage {
        normalized.push(semantic("usage", enum_json(&usage)));
    }
    if let Some(terminal) = terminal {
        normalized.push(terminal);
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::run_protocol::{
        CapabilityAssessment, CapabilityState, ClientKind, ModelCapabilitiesSnapshot,
        ModelTargetSnapshot, OutputChannel, PermissionMode, PermissionPolicySnapshot,
        RedactedPayload, RedactionState, RunBudgets, RunKind, ToolOutcome, ToolPolicyDecision,
    };

    struct TempDb {
        path: std::path::PathBuf,
    }

    impl TempDb {
        fn new(label: &str) -> Self {
            Self {
                path: std::env::temp_dir().join(format!(
                    "little-monkey-cli-durable-{label}-{}-{}.sqlite3",
                    std::process::id(),
                    uuid::Uuid::new_v4()
                )),
            }
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            for path in [
                self.path.clone(),
                std::path::PathBuf::from(format!("{}-wal", self.path.display())),
                std::path::PathBuf::from(format!("{}-shm", self.path.display())),
            ] {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    fn client(kind: ClientKind, instance: &str) -> ClientIdentity {
        ClientIdentity {
            client_id: match kind {
                ClientKind::Desktop => "little-monkey-desktop",
                _ => "monkey-cli",
            }
            .to_string(),
            instance_id: instance.to_string(),
            kind,
            version: "0.1.0-test".to_string(),
        }
    }

    fn capabilities() -> ModelCapabilitiesSnapshot {
        let capability = || CapabilityAssessment {
            state: CapabilityState::Unknown,
            evidence: "test fixture".to_string(),
        };
        ModelCapabilitiesSnapshot {
            tool_calling: capability(),
            vision: capability(),
            embeddings: capability(),
            structured_output: capability(),
            image_generation: capability(),
            audio: capability(),
            runtime_lifecycle: capability(),
            fim: capability(),
            code_completion: capability(),
            inline_edit: capability(),
            fim_metadata: None,
        }
    }

    fn spec(run_id: &str, key: &str) -> RunSpec {
        RunSpec {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            run_id: run_id.to_string(),
            idempotency_key: key.to_string(),
            created_at_ms: 1_000,
            kind: RunKind::Workflow,
            submitted_by: client(ClientKind::Cli, run_id),
            task: "test durable CLI task".to_string(),
            instructions: None,
            input_artifact_ids: Vec::new(),
            target: ModelTargetSnapshot::Ollama {
                target_id: "ollama-test".to_string(),
                label: "Ollama test".to_string(),
                base_url: "http://127.0.0.1:11434".to_string(),
                model: "qwen-test".to_string(),
                is_cloud: false,
                capabilities: capabilities(),
                estimated_memory_bytes: None,
            },
            workspace: None,
            permission_policy: PermissionPolicySnapshot {
                mode: PermissionMode::Auto,
                unattended: true,
                approval_timeout_ms: 60_000,
                default_tool_decision: ToolPolicyDecision::Prompt,
                tool_rules: Vec::new(),
                allow_network: true,
                allow_external_mutations: false,
                egress_allowlist: None,
                channel_send: None,
            },
            budgets: RunBudgets {
                wall_time_ms: 60_000,
                max_iterations: 10,
                max_model_calls: 100,
                max_tool_calls: 100,
                max_input_tokens: 1_000_000,
                max_output_tokens: 1_000_000,
                max_cost_micros: None,
                max_artifact_bytes: 1_000_000,
                max_event_count: 10_000,
            },
        }
    }

    fn envelope(
        emitter: ClientIdentity,
        event_id: &str,
        run_id: &str,
        sequence: u64,
        occurred_at_ms: u64,
        event: RunEvent,
    ) -> RunEventEnvelope {
        RunEventEnvelope {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            event_id: event_id.to_string(),
            run_id: run_id.to_string(),
            sequence,
            occurred_at_ms,
            actor_id: Some("actor-test".to_string()),
            emitter,
            event,
        }
    }

    #[test]
    fn recorder_commits_a_terminal_stream_and_idempotent_retry_does_not_reexecute() {
        let db = TempDb::new("terminal");
        let run_spec = spec("cli-run-terminal", "cli-task/terminal");
        let ledger = RunLedger::open(&db.path).unwrap();
        let (recorder, disposition) =
            DurableRunRecorder::submit(ledger, &run_spec, "recipe:test".to_string()).unwrap();
        assert_eq!(
            disposition,
            SubmissionDisposition::Ready {
                resumed_before_start: false
            }
        );
        recorder
            .emit(RunEvent::Started {
                engine_id: "monkey-cli-task".to_string(),
            })
            .unwrap();
        recorder
            .emit(RunEvent::Completed {
                summary: Some("done".to_string()),
                result_artifact_ids: Vec::new(),
                usage: zero_usage(),
            })
            .unwrap();
        drop(recorder);

        let ledger = RunLedger::open(&db.path).unwrap();
        let (_recorder, retry) =
            DurableRunRecorder::submit(ledger, &run_spec, "recipe:test".to_string()).unwrap();
        assert_eq!(
            retry,
            SubmissionDisposition::AlreadyTerminal(RunStatus::Succeeded)
        );
    }

    #[test]
    fn recorder_refuses_to_replay_a_run_that_crossed_started() {
        let db = TempDb::new("interrupted");
        let run_spec = spec("cli-run-interrupted", "cli-task/interrupted");
        let ledger = RunLedger::open(&db.path).unwrap();
        let (recorder, _) =
            DurableRunRecorder::submit(ledger, &run_spec, "recipe:test".to_string()).unwrap();
        recorder
            .emit(RunEvent::Started {
                engine_id: "monkey-cli-task".to_string(),
            })
            .unwrap();
        drop(recorder);

        let ledger = RunLedger::open(&db.path).unwrap();
        let (recorder, retry) =
            DurableRunRecorder::submit(ledger, &run_spec, "recipe:test".to_string()).unwrap();
        assert_eq!(retry, SubmissionDisposition::InterruptedReplayRefused);
        let stored = recorder
            .state
            .lock()
            .unwrap()
            .ledger
            .load_run(&run_spec.run_id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, RunStatus::Failed);
    }

    #[test]
    fn redacted_arguments_bind_the_original_but_do_not_store_secret_fields() {
        let raw = r#"{"path":"x","api_key":"super-secret","nested":{"password":"pw"}}"#;
        let (payload, digest) = redacted_tool_arguments("write_file", raw);
        assert_eq!(payload.redaction, RedactionState::Applied);
        assert_eq!(payload.value["api_key"], "[REDACTED]");
        assert_eq!(payload.value["nested"]["password"], "[REDACTED]");
        assert_eq!(digest.len(), 64);
        assert!(!serde_json::to_string(&payload)
            .unwrap()
            .contains("super-secret"));
    }

    #[test]
    fn mutation_content_and_shell_commands_are_not_persisted_in_arguments() {
        let (write, _) = redacted_tool_arguments(
            "write_file",
            r#"{"path":".env","content":"API_KEY=secret"}"#,
        );
        let (shell, _) = redacted_tool_arguments(
            "run_shell",
            r#"{"command":"curl -H 'Authorization: secret' example.test"}"#,
        );
        let serialized = format!(
            "{}{}",
            serde_json::to_string(&write).unwrap(),
            serde_json::to_string(&shell).unwrap()
        );
        assert!(!serialized.contains("API_KEY=secret"));
        assert!(!serialized.contains("Authorization: secret"));
    }

    #[test]
    fn operation_digest_is_canonical_and_scope_bound() {
        let a = operation_sha256("write_file", r#"{"b":2,"a":1}"#, "/workspace");
        let b = operation_sha256("write_file", r#"{"a":1,"b":2}"#, "/workspace");
        let other_scope = operation_sha256("write_file", r#"{"a":1,"b":2}"#, "/other");
        assert_eq!(a, b);
        assert_ne!(a, other_scope);
    }

    #[test]
    fn semantic_fixture_ignores_transport_authority_and_delta_chunking() {
        let args = RedactedPayload {
            value: serde_json::json!({ "path": "README.md" }),
            redaction: RedactionState::NotNeeded,
        };
        let digest = sha256_hex(br#"{"path":"README.md"}"#);
        let usage = UsageSnapshot {
            input_tokens: 10,
            output_tokens: 4,
            cached_input_tokens: 0,
            model_calls: 1,
            tool_calls: 1,
            cost_micros: None,
        };
        let desktop_client = client(ClientKind::Desktop, "window-main");
        let cli_client = client(ClientKind::Cli, "cli-instance");
        let desktop = vec![
            envelope(
                desktop_client.clone(),
                "desktop-1",
                "desktop-run",
                1,
                1_000,
                RunEvent::Queued { queue: None },
            ),
            envelope(
                desktop_client.clone(),
                "desktop-2",
                "desktop-run",
                2,
                1_001,
                RunEvent::Started {
                    engine_id: "desktop-engine".to_string(),
                },
            ),
            envelope(
                desktop_client.clone(),
                "desktop-3",
                "desktop-run",
                3,
                1_002,
                RunEvent::ModelDelta {
                    message_id: "desktop-message".to_string(),
                    channel: OutputChannel::Assistant,
                    text: "Hel".to_string(),
                },
            ),
            envelope(
                desktop_client.clone(),
                "desktop-4",
                "desktop-run",
                4,
                1_003,
                RunEvent::ModelDelta {
                    message_id: "desktop-message".to_string(),
                    channel: OutputChannel::Assistant,
                    text: "lo".to_string(),
                },
            ),
            envelope(
                desktop_client.clone(),
                "desktop-5",
                "desktop-run",
                5,
                1_004,
                RunEvent::ToolProposed {
                    tool_call_id: "desktop-call".to_string(),
                    tool_name: "read_file".to_string(),
                    arguments: args.clone(),
                    arguments_sha256: digest.clone(),
                    mutation: false,
                },
            ),
            envelope(
                desktop_client.clone(),
                "desktop-6",
                "desktop-run",
                6,
                1_005,
                RunEvent::ToolStarted {
                    tool_call_id: "desktop-call".to_string(),
                },
            ),
            envelope(
                desktop_client.clone(),
                "desktop-7",
                "desktop-run",
                7,
                1_006,
                RunEvent::ToolFinished {
                    tool_call_id: "desktop-call".to_string(),
                    outcome: ToolOutcome::Succeeded,
                    output_excerpt: None,
                    output_sha256: None,
                    duration_ms: 1,
                },
            ),
            envelope(
                desktop_client.clone(),
                "desktop-8",
                "desktop-run",
                8,
                1_007,
                RunEvent::UsageRecorded {
                    usage: usage.clone(),
                },
            ),
            envelope(
                desktop_client,
                "desktop-9",
                "desktop-run",
                9,
                1_008,
                RunEvent::Completed {
                    summary: None,
                    result_artifact_ids: Vec::new(),
                    usage: usage.clone(),
                },
            ),
        ];
        let cli = vec![
            envelope(
                cli_client.clone(),
                "cli-a",
                "cli-run",
                1,
                9_000,
                RunEvent::Queued {
                    queue: Some("cli-task".to_string()),
                },
            ),
            envelope(
                cli_client.clone(),
                "cli-b",
                "cli-run",
                2,
                9_001,
                RunEvent::Started {
                    engine_id: "monkey-cli-task".to_string(),
                },
            ),
            envelope(
                cli_client.clone(),
                "cli-c",
                "cli-run",
                3,
                9_002,
                RunEvent::ModelDelta {
                    message_id: "cli-answer-1".to_string(),
                    channel: OutputChannel::Assistant,
                    text: "Hello".to_string(),
                },
            ),
            envelope(
                cli_client.clone(),
                "cli-d",
                "cli-run",
                4,
                9_003,
                RunEvent::ToolProposed {
                    tool_call_id: "cli-call-99".to_string(),
                    tool_name: "read_file".to_string(),
                    arguments: args,
                    arguments_sha256: digest,
                    mutation: false,
                },
            ),
            envelope(
                cli_client.clone(),
                "cli-e",
                "cli-run",
                5,
                9_004,
                RunEvent::ToolStarted {
                    tool_call_id: "cli-call-99".to_string(),
                },
            ),
            envelope(
                cli_client.clone(),
                "cli-f",
                "cli-run",
                6,
                9_005,
                RunEvent::ToolFinished {
                    tool_call_id: "cli-call-99".to_string(),
                    outcome: ToolOutcome::Succeeded,
                    output_excerpt: None,
                    output_sha256: None,
                    duration_ms: 999,
                },
            ),
            envelope(
                cli_client.clone(),
                "cli-g",
                "cli-run",
                7,
                9_006,
                RunEvent::UsageRecorded {
                    usage: usage.clone(),
                },
            ),
            envelope(
                cli_client,
                "cli-h",
                "cli-run",
                8,
                9_007,
                RunEvent::Completed {
                    summary: Some("different UI summary".to_string()),
                    result_artifact_ids: Vec::new(),
                    usage,
                },
            ),
        ];
        let report = SemanticConformanceFixture { desktop, cli }.compare();
        assert!(report.matches, "{report:#?}");
        assert_eq!(report.first_difference, None);
    }

    #[test]
    fn semantic_fixture_reports_the_first_real_behavior_difference() {
        let emitter = client(ClientKind::Cli, "cli-instance");
        let passed = vec![envelope(
            emitter.clone(),
            "event-pass",
            "run-pass",
            1,
            1_000,
            RunEvent::VerificationFinished {
                verification_id: "verify-a".to_string(),
                name: "tests".to_string(),
                passed: true,
                summary: "passed".to_string(),
                artifact_ids: Vec::new(),
                duration_ms: 1,
            },
        )];
        let failed = vec![envelope(
            emitter,
            "event-fail",
            "run-fail",
            1,
            2_000,
            RunEvent::VerificationFinished {
                verification_id: "verify-b".to_string(),
                name: "tests".to_string(),
                passed: false,
                summary: "failed".to_string(),
                artifact_ids: Vec::new(),
                duration_ms: 99,
            },
        )];
        let report = SemanticConformanceFixture {
            desktop: passed,
            cli: failed,
        }
        .compare();
        assert!(!report.matches);
        assert_eq!(report.first_difference, Some(0));
    }

    #[test]
    fn checked_in_cross_client_fixture_is_valid_and_conformant() {
        let fixture: SemanticConformanceFixture =
            serde_json::from_str(include_str!("fixtures/durable_run_conformance.json")).unwrap();
        for event in fixture.desktop.iter().chain(&fixture.cli) {
            event.validate().unwrap();
        }
        assert!(fixture.compare().matches);
    }

    #[test]
    fn model_chunks_respect_the_event_size_and_utf8_boundaries() {
        let input = "🐒".repeat(40_000);
        let chunks = model_delta_chunks(&input);
        assert!(chunks.len() > 1);
        assert_eq!(chunks.concat(), input);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 60 * 1024));
    }

    #[test]
    fn database_path_can_be_a_normal_filesystem_path() {
        let path = std::path::Path::new("profile-v1.sqlite3");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("profile-v1.sqlite3")
        );
    }
}
