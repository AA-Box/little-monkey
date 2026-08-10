//! Chat Template and Renderer Compatibility Lab (ROADMAP Phase 8 item 8).
//!
//! Little Monkey does not implement its own Jinja-style chat template
//! engine: Ollama and llama.cpp (started with `--jinja`, see `llama.rs`)
//! apply a model's *own* chat template server-side, and the M3 runtime hub
//! only ever has to compose/parse the OpenAI-compatible wire protocol that
//! sits on top of that (see `m3_production.rs`'s `openai_request_body` /
//! `parse_openai_response` / `ingest_sse_line`, and `m3_runtime_hub.rs`'s
//! `canonical_message_to_mlx` for the MLX driver). This module is the
//! validation layer that checks that wire-level renderer/parser against a
//! small library of realistic fixtures — tool calls, system prompts, stop
//! reasons, structured output, image blocks, and reasoning ("thinking")
//! turns — grouped by a coarse "template family" derived from a catalog
//! model's declared `template` string (see `M3CatalogModel::template`).
//!
//! This is deliberately **not** a general chat-template engine or a Jinja
//! interpreter, and it does not fix the renderer it inspects — it only
//! reports, honestly, whether each fixture currently passes. Two of the six
//! fixtures (`Vision`, `Thinking`) fail unconditionally today because the
//! canonical message model (`CanonicalContent` in `compatibility_hub.rs`)
//! has no image or reasoning-content variant yet, and the OpenAI-compatible
//! stream parser never reads a `reasoning_content` delta field — see each
//! fixture's doc comment for specifics. That is a real, current limitation
//! of the renderer being validated, not a bug in the lab; per the acceptance
//! criterion ("a model cannot be advertised as chat/tool/vision-ready until
//! renderer tests pass"), `vision` is therefore never gated as ready by this
//! lab until image-content support lands (tracked separately under Phase 8
//! item 12, Multimodal Projector and Vision Model Manager).
//!
//! Template family detection is intentionally coarse: five buckets
//! (`Chatml`, `Llama3`, `Mistral`, `Gemma`, `Generic`) keyed by a substring
//! match on the catalog's `template` field, not a model-by-model or
//! tokenizer-level identification of every chat template in existence.

use crate::compatibility_hub::{
    CanonicalContent, CanonicalInferenceRequest, CanonicalMessage, CanonicalRole,
    CanonicalStreamEvent, CanonicalToolDefinition, CompatibilityProtocol,
    COMPATIBILITY_SCHEMA_VERSION,
};
use crate::m3_production::{
    ingest_sse_line, openai_request_body, parse_openai_response, OpenAiStreamState,
};
use crate::m3_runtime_hub::{M3CanonicalStreamSink, M3ModelCapabilities};
// The MLX leg of the tool-call fixture only exists where the MLX driver does.
#[cfg(target_os = "macos")]
use crate::m3_runtime_hub::canonical_message_to_mlx;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Coarse chat-template family, derived from a catalog model's declared
/// `template` string. Unknown/absent templates fall back to `Generic`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemplateFamily {
    Chatml,
    Llama3,
    Mistral,
    Gemma,
    Generic,
}

impl TemplateFamily {
    /// Keyword-matches a catalog/Modelfile `template` string into one of the
    /// five known families. This is a deliberately small, fixed set — see
    /// the module doc comment for why finer-grained detection is out of
    /// scope here.
    pub fn detect(template: Option<&str>) -> Self {
        let Some(template) = template else {
            return Self::Generic;
        };
        let lower = template.to_lowercase();
        if lower.contains("gemma") {
            Self::Gemma
        } else if lower.contains("llama-3") || lower.contains("llama3") {
            Self::Llama3
        } else if lower.contains("mistral") {
            Self::Mistral
        } else if lower.contains("chatml") || lower.contains("qwen") {
            Self::Chatml
        } else {
            Self::Generic
        }
    }

    /// Whether this family's own published chat template defines a distinct
    /// `system`-role turn. Gemma's official chat template (Gemma 2/3
    /// `tokenizer_config.json`) has no `system` branch — the documented
    /// community convention is to fold system content into the leading user
    /// turn instead. Every other known family here accepts a literal
    /// `system` turn.
    fn supports_system_role(self) -> bool {
        !matches!(self, Self::Gemma)
    }
}

/// One fixture area from the roadmap wording ("tool rendering, image
/// blocks, thinking modes, system prompts, and stop tokens"), plus
/// structured output (already a distinct `M3ModelCapabilities` flag).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityArea {
    ToolCalling,
    SystemPrompt,
    StopToken,
    StructuredOutput,
    Vision,
    Thinking,
}

/// A single fixture's outcome: whether the renderer/parser round-tripped
/// the fixture correctly, and a human-readable explanation either way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatTemplateLabResult {
    pub area: CapabilityArea,
    pub passed: bool,
    pub detail: String,
}

impl ChatTemplateLabResult {
    fn pass(area: CapabilityArea, detail: impl Into<String>) -> Self {
        Self {
            area,
            passed: true,
            detail: detail.into(),
        }
    }

    fn fail(area: CapabilityArea, detail: impl Into<String>) -> Self {
        Self {
            area,
            passed: false,
            detail: detail.into(),
        }
    }
}

/// The full compatibility lab result for one template family.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatTemplateLabReport {
    pub template_family: TemplateFamily,
    pub results: Vec<ChatTemplateLabResult>,
}

impl ChatTemplateLabReport {
    /// Whether the given area's fixture passed for this family. `false` for
    /// an area with no matching result (defensive; every area currently
    /// always has exactly one result).
    pub fn passed(&self, area: CapabilityArea) -> bool {
        self.results
            .iter()
            .any(|result| result.area == area && result.passed)
    }
}

/// Runs every fixture against the real renderer/parser code for one
/// template family. Pure and deterministic — no network, no clock
/// dependency beyond what `parse_openai_response`/`ingest_sse_line`
/// themselves already use for a response's `created` timestamp fallback.
pub fn run_chat_template_lab(family: TemplateFamily) -> ChatTemplateLabReport {
    ChatTemplateLabReport {
        template_family: family,
        results: vec![
            fixture_tool_calling(family),
            fixture_system_prompt(family),
            fixture_stop_token(family),
            fixture_structured_output(family),
            fixture_vision(family),
            fixture_thinking(family),
        ],
    }
}

/// Intersects a catalog/installed model's *declared* capabilities with what
/// this lab actually verified for its template family: a capability can
/// only stay `true` if it was already declared true AND its fixture(s)
/// passed. This never upgrades a capability the catalog didn't already
/// claim — it only ever tightens, matching the acceptance criterion that a
/// model "cannot be advertised as chat/tool/vision-ready until renderer
/// tests pass". `embeddings` has no chat-template fixture (it isn't a chat
/// rendering concern) and passes through unchanged.
pub fn gate_capabilities(
    declared: &M3ModelCapabilities,
    report: &ChatTemplateLabReport,
) -> M3ModelCapabilities {
    M3ModelCapabilities {
        chat: declared.chat
            && report.passed(CapabilityArea::SystemPrompt)
            && report.passed(CapabilityArea::StopToken),
        embeddings: declared.embeddings,
        tool_calling: declared.tool_calling && report.passed(CapabilityArea::ToolCalling),
        vision: declared.vision && report.passed(CapabilityArea::Vision),
        structured_output: declared.structured_output
            && report.passed(CapabilityArea::StructuredOutput),
    }
}

/// Collects every canonical stream event a fixture's synthetic response
/// produces, for later reassembly/assertion. Not `#[cfg(test)]`: the lab
/// runs for real from the `m3_chat_template_lab_report` command, not only
/// under `cargo test`.
#[derive(Default)]
struct RecordingSink(Vec<CanonicalStreamEvent>);

impl M3CanonicalStreamSink for RecordingSink {
    fn emit(&mut self, event: CanonicalStreamEvent) -> Result<(), String> {
        self.0.push(event);
        Ok(())
    }
}

const FIXTURE_MODEL: &str = "chat-template-lab-fixture";

fn fixture_request(
    messages: Vec<CanonicalMessage>,
    tools: Vec<CanonicalToolDefinition>,
    response_schema: Option<Value>,
) -> CanonicalInferenceRequest {
    CanonicalInferenceRequest {
        schema_version: COMPATIBILITY_SCHEMA_VERSION,
        protocol: CompatibilityProtocol::OpenAiChatCompletions,
        request_id: "chat-template-lab".to_string(),
        model: FIXTURE_MODEL.to_string(),
        messages,
        tools,
        max_output_tokens: 256,
        temperature: Some(0.2),
        stream: false,
        response_schema,
        metadata: Value::Null,
    }
}

fn sse_line(value: &Value) -> String {
    format!("data: {value}")
}

/// Tool-call round trip: an assistant turn that calls a tool, followed by a
/// tool-role turn carrying that call's result. Validates both real render
/// paths the M3 hub can send tool traffic through:
///  1. The OpenAI-compatible wire body (`openai_request_body`) used by the
///     Ollama/llama.cpp driver, in both directions (compose the request,
///     then parse a synthetic streamed tool-call response back).
///  2. The MLX driver's flattened text representation
///     (`canonical_message_to_mlx`), which has no native `tool_calls` wire
///     field and instead embeds the same JSON shape inside `MlxMessage.text`.
fn fixture_tool_calling(_family: TemplateFamily) -> ChatTemplateLabResult {
    let area = CapabilityArea::ToolCalling;
    let tool_input = json!({"path": "src/lib.rs", "limit": 40});

    // --- 1a. Compose direction: OpenAI-compatible wire body. ---
    let request = fixture_request(
        vec![
            CanonicalMessage {
                role: CanonicalRole::User,
                content: vec![CanonicalContent::Text {
                    text: "Read src/lib.rs".to_string(),
                }],
            },
            CanonicalMessage {
                role: CanonicalRole::Assistant,
                content: vec![CanonicalContent::ToolUse {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    input: tool_input.clone(),
                }],
            },
            CanonicalMessage {
                role: CanonicalRole::Tool,
                content: vec![CanonicalContent::ToolResult {
                    tool_use_id: "call_1".to_string(),
                    content: "fn main() {}".to_string(),
                    is_error: false,
                }],
            },
        ],
        vec![CanonicalToolDefinition {
            name: "read_file".to_string(),
            description: "Reads a file from the workspace".to_string(),
            input_schema: json!({"type": "object"}),
            strict: false,
        }],
        None,
    );
    let wire = match openai_request_body(&request, false) {
        Ok(body) => body,
        Err(error) => {
            return ChatTemplateLabResult::fail(
                area,
                format!("request composition failed: {error}"),
            )
        }
    };
    let messages = wire["messages"].as_array().cloned().unwrap_or_default();
    if messages.len() != 3 {
        return ChatTemplateLabResult::fail(
            area,
            format!("expected 3 composed wire messages, got {}", messages.len()),
        );
    }
    let assistant_call = &messages[1]["tool_calls"][0];
    if assistant_call["id"] != "call_1" || assistant_call["function"]["name"] != "read_file" {
        return ChatTemplateLabResult::fail(
            area,
            "composed assistant message did not carry the expected tool_calls id/name".to_string(),
        );
    }
    let composed_arguments: Option<Value> = assistant_call["function"]["arguments"]
        .as_str()
        .and_then(|raw| serde_json::from_str(raw).ok());
    if composed_arguments.as_ref() != Some(&tool_input) {
        return ChatTemplateLabResult::fail(
            area,
            "composed tool_calls[0].function.arguments did not round-trip the tool input JSON"
                .to_string(),
        );
    }
    let tool_result_message = &messages[2];
    if tool_result_message["role"] != "tool"
        || tool_result_message["tool_call_id"] != "call_1"
        || tool_result_message["content"] != "fn main() {}"
    {
        return ChatTemplateLabResult::fail(
            area,
            "composed tool-role message did not carry the expected tool_call_id/content"
                .to_string(),
        );
    }

    // --- 1b. Parse direction: synthetic streamed tool-call response. ---
    // Declares the same `read_file` tool as the compose-direction `request`
    // above: the streamed/non-streaming parse checks below simulate the
    // model calling that exact tool, and the compatibility layer's
    // tool-call hardening (Phase 8, "Tool-Call and Structured-Output Parser
    // Hardening") rejects any tool call naming a tool the request never
    // offered — so this fixture request has to offer it, same as a real
    // caller would.
    let stream_request = fixture_request(
        vec![CanonicalMessage {
            role: CanonicalRole::User,
            content: vec![CanonicalContent::Text {
                text: "go".to_string(),
            }],
        }],
        vec![CanonicalToolDefinition {
            name: "read_file".to_string(),
            description: "Reads a file from the workspace".to_string(),
            input_schema: json!({"type": "object"}),
            strict: false,
        }],
        None,
    );
    let mut sink = RecordingSink::default();
    let mut state = OpenAiStreamState::default();
    let delta_chunk = json!({
        "id": "resp-tool-call",
        "created": 1,
        "model": FIXTURE_MODEL,
        "choices": [{
            "index": 0,
            "delta": {"tool_calls": [{
                "index": 0,
                "id": "call_9",
                "function": {"name": "read_file", "arguments": tool_input.to_string()},
            }]},
            "finish_reason": Value::Null,
        }],
    });
    let finish_chunk = json!({
        "id": "resp-tool-call",
        "created": 1,
        "model": FIXTURE_MODEL,
        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
    });
    for chunk in [&delta_chunk, &finish_chunk] {
        if let Err(error) =
            ingest_sse_line(&sse_line(chunk), &stream_request, &mut sink, &mut state)
        {
            return ChatTemplateLabResult::fail(
                area,
                format!("streamed tool-call ingestion failed: {error}"),
            );
        }
    }
    if let Err(error) = state.finish(&stream_request, &mut sink) {
        return ChatTemplateLabResult::fail(
            area,
            format!("streamed tool-call did not finish cleanly: {error}"),
        );
    }
    let reconstructed_name = sink.0.iter().find_map(|event| match event {
        CanonicalStreamEvent::ToolCallStart { call_id, name, .. } if call_id == "call_9" => {
            Some(name.clone())
        }
        _ => None,
    });
    let reconstructed_arguments: String = sink
        .0
        .iter()
        .filter_map(|event| match event {
            CanonicalStreamEvent::ToolCallArgumentsDelta {
                call_id,
                json_delta,
                ..
            } if call_id == "call_9" => Some(json_delta.as_str()),
            _ => None,
        })
        .collect();
    let reconstructed_input: Option<Value> = serde_json::from_str(&reconstructed_arguments).ok();
    if reconstructed_name.as_deref() != Some("read_file")
        || reconstructed_input.as_ref() != Some(&tool_input)
    {
        return ChatTemplateLabResult::fail(
            area,
            "streamed tool-call reconstruction did not match the original name/arguments"
                .to_string(),
        );
    }

    // --- 1c. Non-streaming parse direction, same shape. ---
    let complete_body = json!({
        "id": "resp-tool-call-complete",
        "created": 1,
        "model": FIXTURE_MODEL,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": Value::Null,
                "tool_calls": [{"id": "call_9", "type": "function", "function": {"name": "read_file", "arguments": tool_input.to_string()}}],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": {"prompt_tokens": 4, "completion_tokens": 2},
    });
    let parsed = match parse_openai_response(&complete_body, &stream_request) {
        Ok(response) => response,
        Err(error) => {
            return ChatTemplateLabResult::fail(
                area,
                format!("non-streaming tool-call parse failed: {error}"),
            )
        }
    };
    let non_stream_ok = parsed.content.iter().any(|content| {
        matches!(
            content,
            CanonicalContent::ToolUse { name, input, .. } if name == "read_file" && input == &tool_input
        )
    });
    if !non_stream_ok {
        return ChatTemplateLabResult::fail(
            area,
            "non-streaming response parse did not reconstruct the expected ToolUse content"
                .to_string(),
        );
    }

    // --- 1d. MLX driver's flattened text representation. Only the macOS build
    // carries an MLX driver, so this leg — and the claim the pass message makes
    // about it — exists only there. ---
    #[cfg(target_os = "macos")]
    {
        let mlx_assistant = match canonical_message_to_mlx(&CanonicalMessage {
            role: CanonicalRole::Assistant,
            content: vec![CanonicalContent::ToolUse {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
                input: tool_input.clone(),
            }],
        }) {
            Ok(message) => message,
            Err(error) => {
                return ChatTemplateLabResult::fail(
                    area,
                    format!("MLX assistant flattening failed: {error}"),
                )
            }
        };
        let mlx_tool_result = match canonical_message_to_mlx(&CanonicalMessage {
            role: CanonicalRole::Tool,
            content: vec![CanonicalContent::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: "fn main() {}".to_string(),
                is_error: false,
            }],
        }) {
            Ok(message) => message,
            Err(error) => {
                return ChatTemplateLabResult::fail(
                    area,
                    format!("MLX tool-result flattening failed: {error}"),
                )
            }
        };
        let mlx_assistant_json: Option<Value> = serde_json::from_str(&mlx_assistant.text).ok();
        let mlx_ok = mlx_assistant_json
            .as_ref()
            .and_then(|value| value.get("input"))
            == Some(&tool_input)
            && mlx_assistant_json
                .as_ref()
                .and_then(|value| value.get("name"))
                == Some(&json!("read_file"))
            && mlx_tool_result.text.contains("fn main() {}");
        if !mlx_ok {
            return ChatTemplateLabResult::fail(
                area,
                "MLX driver's flattened tool_use/tool_result text did not round-trip the same call"
                    .to_string(),
            );
        }
    }

    #[cfg(target_os = "macos")]
    let detail = "tool_calls round-trip through the OpenAI-compatible wire format (compose, streamed parse, and \
         non-streaming parse) and through the MLX driver's flattened text representation, all matching the \
         original tool name/arguments/result";
    #[cfg(not(target_os = "macos"))]
    let detail = "tool_calls round-trip through the OpenAI-compatible wire format (compose, streamed parse, and \
         non-streaming parse), matching the original tool name/arguments/result";
    ChatTemplateLabResult::pass(area, detail)
}

/// System-prompt-present conversation. Whether Little Monkey's renderer
/// should even send a literal `role: "system"` message depends on whether
/// the target family's own chat template defines one — see
/// `TemplateFamily::supports_system_role`. The renderer (`openai_messages`,
/// via `openai_request_body`) currently always emits a system-role message
/// unconditionally, so this fixture legitimately fails for Gemma today.
fn fixture_system_prompt(family: TemplateFamily) -> ChatTemplateLabResult {
    let area = CapabilityArea::SystemPrompt;
    let system_text = "You are a careful, concise assistant.";
    let request = fixture_request(
        vec![
            CanonicalMessage {
                role: CanonicalRole::System,
                content: vec![CanonicalContent::Text {
                    text: system_text.to_string(),
                }],
            },
            CanonicalMessage {
                role: CanonicalRole::User,
                content: vec![CanonicalContent::Text {
                    text: "Hi".to_string(),
                }],
            },
        ],
        Vec::new(),
        None,
    );
    let wire = match openai_request_body(&request, false) {
        Ok(body) => body,
        Err(error) => {
            return ChatTemplateLabResult::fail(
                area,
                format!("request composition failed: {error}"),
            )
        }
    };
    let messages = wire["messages"].as_array().cloned().unwrap_or_default();
    let sends_distinct_system_turn = messages
        .first()
        .is_some_and(|message| message["role"] == "system" && message["content"] == system_text);

    if family.supports_system_role() {
        if sends_distinct_system_turn {
            ChatTemplateLabResult::pass(
                area,
                format!(
                    "{family:?}'s published chat template accepts a distinct system-role turn, and the \
                     renderer emits exactly that"
                ),
            )
        } else {
            ChatTemplateLabResult::fail(
                area,
                "renderer did not emit the expected leading system-role message".to_string(),
            )
        }
    } else if sends_distinct_system_turn {
        ChatTemplateLabResult::fail(
            area,
            format!(
                "{family:?}'s published chat template has no distinct system-role turn (system content is \
                 conventionally folded into the leading user turn instead); the renderer still emits a bare \
                 role:\"system\" message this template family does not define, so system-prompt handling is \
                 not renderer-verified for this family yet"
            ),
        )
    } else {
        ChatTemplateLabResult::pass(
            area,
            format!(
                "renderer correctly avoided a bare system-role turn for {family:?}, whose template has none"
            ),
        )
    }
}

/// Stop-token/stop-sequence scenario: a normal `"stop"` finish reason and a
/// max-output-tokens `"length"` finish reason, checked through both the
/// non-streaming and streaming parse paths. Family-agnostic: which literal
/// stop token/string ends generation is entirely up to the runtime's own
/// template-driven sampling (delegated to Ollama/llama.cpp, not
/// re-implemented here) — Little Monkey's renderer only needs to surface
/// whatever `finish_reason` string comes back unchanged, which it does for
/// every known family today.
fn fixture_stop_token(_family: TemplateFamily) -> ChatTemplateLabResult {
    let area = CapabilityArea::StopToken;
    for (raw_reason, expected) in [("stop", "stop"), ("length", "length")] {
        let request = fixture_request(
            vec![CanonicalMessage {
                role: CanonicalRole::User,
                content: vec![CanonicalContent::Text {
                    text: "hi".to_string(),
                }],
            }],
            Vec::new(),
            None,
        );

        let complete_body = json!({
            "id": "resp-stop",
            "created": 1,
            "model": FIXTURE_MODEL,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "done"}, "finish_reason": raw_reason}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1},
        });
        let response = match parse_openai_response(&complete_body, &request) {
            Ok(response) => response,
            Err(error) => {
                return ChatTemplateLabResult::fail(
                    area,
                    format!("non-streaming finish_reason {raw_reason:?} failed to parse: {error}"),
                )
            }
        };
        if response.finish_reason != expected {
            return ChatTemplateLabResult::fail(
                area,
                format!(
                    "expected non-streaming finish_reason {expected:?}, got {:?}",
                    response.finish_reason
                ),
            );
        }

        let mut sink = RecordingSink::default();
        let mut state = OpenAiStreamState::default();
        let chunk = json!({
            "id": "resp-stop-stream",
            "created": 1,
            "model": FIXTURE_MODEL,
            "choices": [{"index": 0, "delta": {}, "finish_reason": raw_reason}],
        });
        if let Err(error) = ingest_sse_line(&sse_line(&chunk), &request, &mut sink, &mut state) {
            return ChatTemplateLabResult::fail(
                area,
                format!("streamed finish_reason {raw_reason:?} ingestion failed: {error}"),
            );
        }
        if let Err(error) = state.finish(&request, &mut sink) {
            return ChatTemplateLabResult::fail(
                area,
                format!("streamed finish_reason {raw_reason:?} did not finish cleanly: {error}"),
            );
        }
        let completed = sink.0.iter().find_map(|event| match event {
            CanonicalStreamEvent::ResponseCompleted { finish_reason, .. } => {
                Some(finish_reason.clone())
            }
            _ => None,
        });
        if completed.as_deref() != Some(expected) {
            return ChatTemplateLabResult::fail(
                area,
                format!("expected streamed finish_reason {expected:?}, got {completed:?}"),
            );
        }
    }
    ChatTemplateLabResult::pass(
        area,
        "\"stop\" and max-token \"length\" finish reasons round-trip through both the streaming and \
         non-streaming OpenAI-compatible parsers unchanged",
    )
}

/// Structured-output (JSON schema) round trip: the requested schema reaches
/// the wire body's `response_format.json_schema.schema`, and a JSON-text
/// response parses back into an exact `CanonicalContent::Text` match.
fn fixture_structured_output(_family: TemplateFamily) -> ChatTemplateLabResult {
    let area = CapabilityArea::StructuredOutput;
    let schema = json!({"type": "object", "properties": {"answer": {"type": "string"}}, "required": ["answer"]});
    let request = fixture_request(
        vec![CanonicalMessage {
            role: CanonicalRole::User,
            content: vec![CanonicalContent::Text {
                text: "What is 2+2?".to_string(),
            }],
        }],
        Vec::new(),
        Some(schema.clone()),
    );
    let wire = match openai_request_body(&request, false) {
        Ok(body) => body,
        Err(error) => {
            return ChatTemplateLabResult::fail(
                area,
                format!("request composition failed: {error}"),
            )
        }
    };
    if wire["response_format"]["type"] != "json_schema"
        || wire["response_format"]["json_schema"]["schema"] != schema
    {
        return ChatTemplateLabResult::fail(
            area,
            "composed response_format.json_schema.schema did not match the requested schema"
                .to_string(),
        );
    }

    let payload = json!({"answer": "4"});
    let complete_body = json!({
        "id": "resp-structured",
        "created": 1,
        "model": FIXTURE_MODEL,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": payload.to_string()}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2},
    });
    let parsed = match parse_openai_response(&complete_body, &request) {
        Ok(response) => response,
        Err(error) => {
            return ChatTemplateLabResult::fail(
                area,
                format!("structured response parse failed: {error}"),
            )
        }
    };
    let round_tripped = parsed.content.iter().find_map(|content| match content {
        CanonicalContent::Text { text } => serde_json::from_str::<Value>(text).ok(),
        _ => None,
    });
    if round_tripped.as_ref() != Some(&payload) {
        return ChatTemplateLabResult::fail(
            area,
            "structured JSON response text did not round-trip to the expected value".to_string(),
        );
    }
    ChatTemplateLabResult::pass(
        area,
        "the requested JSON schema reaches response_format.json_schema.schema unchanged, and a JSON-text \
         response parses back to the exact expected value",
    )
}

/// Image block in a user message. `CanonicalContent` (`compatibility_hub.rs`)
/// has no image/document variant: the OpenAI/Anthropic-compatible pass-
/// through API server explicitly rejects any non-text/tool content block
/// ("images/documents are not advertised by this compatibility subset"),
/// `openai_request_body` only ever iterates Text/ToolUse/ToolResult, and
/// `MlxMessage` (the MLX driver's wire type) is a bare `{role, text}` pair.
/// There is today no path by which an image attached in the main chat UI
/// (`ChatContentPart`/`toMessageContent` in `src/lib/agentLoop.ts` and
/// `llamaClient.ts`, which talk to a runtime directly, bypassing this
/// canonical model entirely) could reach the M3 hub's inference engine
/// intact. This fixture therefore fails unconditionally, for every family,
/// until image-block support is added to the canonical message model —
/// deliberately out of scope here (see Phase 8 item 12).
fn fixture_vision(_family: TemplateFamily) -> ChatTemplateLabResult {
    ChatTemplateLabResult::fail(
        CapabilityArea::Vision,
        "CanonicalContent has no image/document content variant yet, so the M3 hub's OpenAI-compatible \
         request builder and the MLX driver's flattened-text messages cannot carry image bytes to any \
         runtime today. Vision is never renderer-verified until image-block transport is added to the \
         canonical message model (tracked separately, Phase 8 item 12) — this fixture intentionally always \
         fails so `vision` is never advertised as ready in the meantime.",
    )
}

/// Reasoning ("thinking") turn: a model emits a distinguishable reasoning
/// block before its final answer. Several local OpenAI-compatible servers
/// (e.g. reasoning-tuned models served behind vLLM/newer Ollama builds)
/// stream this as a sibling `delta.reasoning_content` field alongside the
/// normal `delta.content`. `OpenAiStreamState::ingest` (`m3_production.rs`)
/// only ever reads `delta.content` and `delta.tool_calls` — a
/// `reasoning_content` delta is silently discarded today, not corrupted-but-
/// hidden: it simply never becomes a `CanonicalStreamEvent`. This fixture
/// proves that gap with a synthetic stream carrying both fields and
/// asserting the reasoning text is NOT recoverable from the emitted events.
/// `M3ModelCapabilities` has no "thinking" flag to gate, so this result is
/// informational (surfaced in the report) rather than gating; it exists
/// because the roadmap wording explicitly calls out thinking-mode testing.
fn fixture_thinking(_family: TemplateFamily) -> ChatTemplateLabResult {
    let area = CapabilityArea::Thinking;
    let request = fixture_request(
        vec![CanonicalMessage {
            role: CanonicalRole::User,
            content: vec![CanonicalContent::Text {
                text: "What's 19 * 23?".to_string(),
            }],
        }],
        Vec::new(),
        None,
    );
    let mut sink = RecordingSink::default();
    let mut state = OpenAiStreamState::default();
    let reasoning_text = "19 * 23 = 19*20 + 19*3 = 380 + 57 = 437";
    let chunk = json!({
        "id": "resp-thinking",
        "created": 1,
        "model": FIXTURE_MODEL,
        "choices": [{
            "index": 0,
            "delta": {"reasoning_content": reasoning_text, "content": "437"},
            "finish_reason": Value::Null,
        }],
    });
    let finish_chunk = json!({
        "id": "resp-thinking",
        "created": 1,
        "model": FIXTURE_MODEL,
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
    });
    for line in [&chunk, &finish_chunk] {
        if let Err(error) = ingest_sse_line(&sse_line(line), &request, &mut sink, &mut state) {
            return ChatTemplateLabResult::fail(
                area,
                format!("reasoning-turn stream ingestion failed: {error}"),
            );
        }
    }
    if let Err(error) = state.finish(&request, &mut sink) {
        return ChatTemplateLabResult::fail(
            area,
            format!("reasoning-turn stream did not finish cleanly: {error}"),
        );
    }
    let reasoning_recovered = sink.0.iter().any(|event| match event {
        CanonicalStreamEvent::TextDelta { text, .. } => text.contains(reasoning_text),
        _ => false,
    });
    let final_answer_recovered = sink.0.iter().any(|event| match event {
        CanonicalStreamEvent::TextDelta { text, .. } => text.contains("437"),
        _ => false,
    });
    if reasoning_recovered {
        ChatTemplateLabResult::pass(
            area,
            "reasoning_content delta text was recovered as a distinguishable event stream alongside the \
             final answer",
        )
    } else if final_answer_recovered {
        ChatTemplateLabResult::fail(
            area,
            "the OpenAI-compatible stream parser only reads delta.content and delta.tool_calls; a sibling \
             delta.reasoning_content field (used by several reasoning-capable local servers) is silently \
             discarded today, so thinking-mode content never reaches the app",
        )
    } else {
        ChatTemplateLabResult::fail(
            area,
            "neither the reasoning content nor the final answer text was recoverable from the stream".to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_known_template_families() {
        assert_eq!(
            TemplateFamily::detect(Some("chatml")),
            TemplateFamily::Chatml
        );
        assert_eq!(
            TemplateFamily::detect(Some("Qwen2.5-ChatML")),
            TemplateFamily::Chatml
        );
        assert_eq!(
            TemplateFamily::detect(Some("llama-3")),
            TemplateFamily::Llama3
        );
        assert_eq!(
            TemplateFamily::detect(Some("Llama3-Instruct")),
            TemplateFamily::Llama3
        );
        assert_eq!(
            TemplateFamily::detect(Some("mistral-instruct")),
            TemplateFamily::Mistral
        );
        assert_eq!(
            TemplateFamily::detect(Some("gemma-2")),
            TemplateFamily::Gemma
        );
        assert_eq!(
            TemplateFamily::detect(Some("something-unknown")),
            TemplateFamily::Generic
        );
        assert_eq!(TemplateFamily::detect(None), TemplateFamily::Generic);
    }

    #[test]
    fn tool_calling_fixture_passes_for_every_family() {
        for family in [
            TemplateFamily::Chatml,
            TemplateFamily::Llama3,
            TemplateFamily::Mistral,
            TemplateFamily::Gemma,
            TemplateFamily::Generic,
        ] {
            let result = fixture_tool_calling(family);
            assert!(result.passed, "{family:?}: {}", result.detail);
        }
    }

    #[test]
    fn system_prompt_fixture_fails_only_for_gemma() {
        for family in [
            TemplateFamily::Chatml,
            TemplateFamily::Llama3,
            TemplateFamily::Mistral,
            TemplateFamily::Generic,
        ] {
            let result = fixture_system_prompt(family);
            assert!(
                result.passed,
                "{family:?} should accept a system-role turn: {}",
                result.detail
            );
        }
        let gemma = fixture_system_prompt(TemplateFamily::Gemma);
        assert!(
            !gemma.passed,
            "gemma should flag the unhandled system-role quirk"
        );
        assert!(gemma.detail.contains("system"));
    }

    #[test]
    fn stop_token_fixture_passes_for_every_family() {
        for family in [
            TemplateFamily::Chatml,
            TemplateFamily::Llama3,
            TemplateFamily::Mistral,
            TemplateFamily::Gemma,
            TemplateFamily::Generic,
        ] {
            let result = fixture_stop_token(family);
            assert!(result.passed, "{family:?}: {}", result.detail);
        }
    }

    #[test]
    fn structured_output_fixture_passes() {
        let result = fixture_structured_output(TemplateFamily::Generic);
        assert!(result.passed, "{}", result.detail);
    }

    #[test]
    fn vision_fixture_always_fails_today() {
        // Regression trap: if this ever starts passing, it means image
        // content support was added to CanonicalContent — update this test
        // (and the gating story) deliberately rather than let it silently
        // start advertising `vision` as ready.
        for family in [
            TemplateFamily::Chatml,
            TemplateFamily::Gemma,
            TemplateFamily::Generic,
        ] {
            assert!(!fixture_vision(family).passed);
        }
    }

    #[test]
    fn thinking_fixture_documents_the_dropped_reasoning_content_gap() {
        let result = fixture_thinking(TemplateFamily::Generic);
        assert!(!result.passed);
        assert!(result.detail.contains("reasoning_content"));
    }

    #[test]
    fn run_chat_template_lab_covers_every_area_exactly_once() {
        let report = run_chat_template_lab(TemplateFamily::Chatml);
        assert_eq!(report.template_family, TemplateFamily::Chatml);
        let areas: Vec<_> = report.results.iter().map(|result| result.area).collect();
        assert_eq!(
            areas,
            vec![
                CapabilityArea::ToolCalling,
                CapabilityArea::SystemPrompt,
                CapabilityArea::StopToken,
                CapabilityArea::StructuredOutput,
                CapabilityArea::Vision,
                CapabilityArea::Thinking,
            ]
        );
    }

    fn all_true_capabilities() -> M3ModelCapabilities {
        M3ModelCapabilities {
            chat: true,
            embeddings: true,
            tool_calling: true,
            vision: true,
            structured_output: true,
        }
    }

    #[test]
    fn gate_capabilities_tightens_but_never_upgrades() {
        let declared = all_true_capabilities();
        let generic_report = run_chat_template_lab(TemplateFamily::Generic);
        let gated = gate_capabilities(&declared, &generic_report);
        assert!(
            gated.chat,
            "generic family's system prompt/stop-token fixtures both pass"
        );
        assert!(gated.tool_calling);
        assert!(gated.structured_output);
        assert!(
            gated.embeddings,
            "embeddings has no fixture and passes through unchanged"
        );
        assert!(!gated.vision, "vision fixture always fails today");

        let gemma_report = run_chat_template_lab(TemplateFamily::Gemma);
        let gemma_gated = gate_capabilities(&declared, &gemma_report);
        assert!(
            !gemma_gated.chat,
            "gemma's system-prompt fixture fails, so chat must not be advertised ready"
        );
        assert!(
            gemma_gated.tool_calling,
            "tool-calling fixture is unaffected by the system-role quirk"
        );

        let mut nothing_declared = all_true_capabilities();
        nothing_declared.tool_calling = false;
        let still_gated = gate_capabilities(&nothing_declared, &generic_report);
        assert!(
            !still_gated.tool_calling,
            "gating must never turn an undeclared capability on, even if its fixture passes"
        );
    }
}
