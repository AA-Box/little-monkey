//! Rust port of `src/lib/llamaClient.ts`'s `SseEventParser`. The GUI parses
//! the OpenAI-compatible chat-completions SSE stream in the WebView (TS);
//! the CLI has no WebView, so this is the one place on the Rust side that
//! needs the same line-buffering + streamed-tool-call-accumulation logic.
//! Kept behaviorally identical to the TS version — see that file for the
//! semantics (line buffering across chunk boundaries, tool-call fragments
//! keyed by index until `finish_reason` shows up, `usage` as a sibling of
//! `choices` on the final chunk).

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ToolCallEvent {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Delta(String),
    ToolCall(ToolCallEvent),
    Usage {
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
    },
}

#[derive(Default)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
pub struct SseParser {
    line_buffer: String,
    pending: HashMap<u64, PendingToolCall>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed newly-arrived (already-decoded) text; returns any complete
    /// events it produces.
    pub fn feed(&mut self, text: &str) -> Vec<StreamEvent> {
        self.line_buffer.push_str(text);
        let mut lines: Vec<String> = self.line_buffer.split('\n').map(String::from).collect();
        // The last entry may be an incomplete line — keep it in the buffer.
        self.line_buffer = lines.pop().unwrap_or_default();

        let mut events = Vec::new();
        for line in lines {
            self.handle_line(&line, &mut events);
        }
        events
    }

    /// Call once the underlying stream has ended: processes any trailing
    /// partial line and flushes any tool call still accumulating.
    pub fn flush(&mut self) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if !self.line_buffer.trim().is_empty() {
            let line = std::mem::take(&mut self.line_buffer);
            self.handle_line(&line, &mut events);
        }
        if !self.pending.is_empty() {
            self.flush_pending(&mut events);
        }
        events
    }

    fn flush_pending(&mut self, events: &mut Vec<StreamEvent>) {
        let mut indices: Vec<u64> = self.pending.keys().copied().collect();
        indices.sort_unstable();
        for index in indices {
            if let Some(call) = self.pending.remove(&index) {
                let id = if call.id.is_empty() {
                    format!("call_{index}")
                } else {
                    call.id
                };
                events.push(StreamEvent::ToolCall(ToolCallEvent {
                    id,
                    name: call.name,
                    arguments: call.arguments,
                }));
            }
        }
    }

    fn handle_line(&mut self, raw_line: &str, events: &mut Vec<StreamEvent>) {
        let line = raw_line.trim();
        let Some(data) = line.strip_prefix("data:") else {
            return;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return;
        }

        let payload: serde_json::Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return, // malformed/partial chunk — skip rather than crash the loop
        };

        if let Some(usage) = payload.get("usage") {
            events.push(StreamEvent::Usage {
                prompt_tokens: usage
                    .get("prompt_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                completion_tokens: usage
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                total_tokens: usage
                    .get("total_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            });
        }

        let Some(choice) = payload.get("choices").and_then(|c| c.get(0)) else {
            return;
        };
        let delta = choice.get("delta");

        if let Some(content) = delta
            .and_then(|d| d.get("content"))
            .and_then(|c| c.as_str())
        {
            if !content.is_empty() {
                events.push(StreamEvent::Delta(content.to_string()));
            }
        }

        if let Some(tool_calls) = delta
            .and_then(|d| d.get("tool_calls"))
            .and_then(|t| t.as_array())
        {
            for fragment in tool_calls {
                let index = fragment.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                let entry = self.pending.entry(index).or_default();
                if let Some(id) = fragment.get("id").and_then(|i| i.as_str()) {
                    entry.id = id.to_string();
                }
                if let Some(name) = fragment
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                {
                    entry.name = name.to_string();
                }
                if let Some(args) = fragment
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                {
                    entry.arguments.push_str(args);
                }
            }
        }

        let finish_reason_present = choice
            .get("finish_reason")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        if finish_reason_present && !self.pending.is_empty() {
            self.flush_pending(events);
        }
    }
}

/// Keys a text-emitted tool-call object may carry and still be recognized as
/// one. Anything else in the object means it is not a call the model meant to
/// make — see [`recover_text_tool_calls`].
const TEXT_TOOL_CALL_KEYS: [&str; 6] = ["name", "arguments", "parameters", "id", "type", "index"];

/// Byte spans of the top-level `{…}` objects in `text`, brace-matched with
/// string/escape awareness so a `}` inside a JSON string value cannot end a
/// span early.
fn top_level_object_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    start = index;
                }
                depth += 1;
            }
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    spans.push((start, index + character.len_utf8()));
                }
            }
            _ => {}
        }
    }
    spans
}

/// Parses one candidate span as a call for a tool that was actually offered.
/// Deliberately strict: a name the model was not given, missing arguments, or
/// any unexpected key disqualifies it.
fn parse_text_tool_call(candidate: &str, offered: &[String]) -> Option<(String, String)> {
    let parsed: serde_json::Value = serde_json::from_str(candidate).ok()?;
    let object = parsed.as_object()?;
    if object.keys().any(|key| !TEXT_TOOL_CALL_KEYS.contains(&key.as_str())) {
        return None;
    }
    let name = object.get("name")?.as_str()?;
    if !offered.iter().any(|offered_name| offered_name == name) {
        return None;
    }
    let arguments = object.get("arguments").or_else(|| object.get("parameters"))?;
    let arguments = match arguments {
        serde_json::Value::String(raw) => raw.clone(),
        serde_json::Value::Object(_) => serde_json::to_string(arguments).ok()?,
        _ => return None,
    };
    Some((name.to_string(), arguments))
}

/// Removes the fences and Hermes tags the extracted JSON was wrapped in, now
/// that they would otherwise be left behind empty.
fn tidy_recovered_content(text: &str) -> String {
    let without_tags = text.replace("<tool_call>", "").replace("</tool_call>", "");
    let empty_fence = regex::Regex::new(r"```[a-zA-Z]*\s*```").expect("static regex");
    let blank_runs = regex::Regex::new(r"\n{3,}").expect("static regex");
    let stripped = empty_fence.replace_all(&without_tags, "");
    blank_runs.replace_all(&stripped, "\n\n").trim().to_string()
}

/// Recovers the tool calls a model wrote as prose instead of emitting on the
/// wire, and returns the content with that JSON removed.
///
/// Rust port of `recoverTextToolCalls` in `src/lib/llamaClient.ts`, and it
/// exists for the same reason `SseParser` above does: the GUI's in-process
/// turn loop does this in the WebView, but a packaged desktop hands every
/// chat turn to this daemon (see `agentLoop.ts`'s `runDaemonAgentTurn`), so a
/// fix that lived only in TS would never run for the app's own chat.
///
/// Small local models do this routinely: the system prompt names the tools,
/// so the model knows they exist, but it answers with a ```json fenced
/// `{"name": …, "arguments": {…}}` block (or a bare `<tool_call>` one whose
/// tags the server's chat template parser did not recognise) rather than a
/// real `tool_calls` delta. Qwen2.5-7B on llama.cpp — the model this app
/// ships — does it several turns into a session. Nothing executes, and the
/// user is shown wire JSON and left to run the command themselves.
///
/// A recovered call is executed through the same path as a native one, with
/// the same permission gate, so this changes what runs only in that it makes
/// the model's stated intent actually happen. The match is kept strict to
/// keep an answer that merely *documents* a call from becoming one: only when
/// the turn produced no real tool call, only for a tool that was offered this
/// turn, and only for an object carrying nothing but the tool-call keys.
/// Identical repeated blocks (models often restate the same call twice in one
/// message) collapse to a single call.
pub fn recover_text_tool_calls(
    content: &str,
    tools: &[serde_json::Value],
) -> (String, Vec<ToolCallEvent>) {
    if tools.is_empty() || !content.contains('{') {
        return (content.to_string(), Vec::new());
    }
    let offered: Vec<String> = tools
        .iter()
        .filter_map(|tool| {
            tool.get("function")
                .and_then(|function| function.get("name"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect();
    if offered.is_empty() {
        return (content.to_string(), Vec::new());
    }

    let mut calls: Vec<ToolCallEvent> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut kept = String::new();
    let mut cursor = 0usize;

    for (start, end) in top_level_object_spans(content) {
        let Some((name, arguments)) = parse_text_tool_call(&content[start..end], &offered) else {
            continue;
        };
        let key = format!("{name} {arguments}");
        if !seen.contains(&key) {
            seen.push(key);
            calls.push(ToolCallEvent {
                id: format!("call_text_{}", calls.len()),
                name,
                arguments,
            });
        }
        kept.push_str(&content[cursor..start]);
        cursor = end;
    }

    if calls.is_empty() {
        return (content.to_string(), Vec::new());
    }
    kept.push_str(&content[cursor..]);
    (tidy_recovered_content(&kept), calls)
}

#[cfg(test)]
mod recovery_tests {
    use super::{recover_text_tool_calls, ToolCallEvent};

    fn tools(names: &[&str]) -> Vec<serde_json::Value> {
        names
            .iter()
            .map(|name| serde_json::json!({"type": "function", "function": {"name": name}}))
            .collect()
    }

    fn shapes(calls: &[ToolCallEvent]) -> Vec<(String, String)> {
        calls
            .iter()
            .map(|call| (call.name.clone(), call.arguments.clone()))
            .collect()
    }

    #[test]
    fn recovers_a_fenced_call_and_strips_it_from_the_answer() {
        let content = "Let me check the log.\n\n```json\n{\n  \"name\": \"run_shell\",\n  \"arguments\": {\n    \"command\": \"cat ~/Library/Logs/bf6.log\"\n  }\n}\n```\n";
        let (answer, calls) = recover_text_tool_calls(content, &tools(&["run_shell"]));
        assert_eq!(
            shapes(&calls),
            vec![(
                "run_shell".to_string(),
                "{\"command\":\"cat ~/Library/Logs/bf6.log\"}".to_string()
            )]
        );
        assert_eq!(answer, "Let me check the log.");
    }

    #[test]
    fn recovers_every_call_in_a_multi_step_answer_and_collapses_repeats() {
        // The real shape a 7B model produces: a numbered plan, one fence per
        // step, and the same call restated at the end.
        let content = "### Step 1\n```json\n{\"name\": \"write_file\", \"arguments\": {\"path\": \"a.env\", \"content\": \"X=1\"}}\n```\n### Step 2\n```json\n{\"name\": \"run_shell\", \"arguments\": {\"command\": \"./run\"}}\n```\nStarting now.\n```json\n{\"name\": \"run_shell\", \"arguments\": {\"command\": \"./run\"}}\n```";
        let (_, calls) = recover_text_tool_calls(content, &tools(&["run_shell", "write_file"]));
        assert_eq!(
            shapes(&calls),
            vec![
                // Keys come back sorted: this crate's `serde_json` has no
                // `preserve_order`, so re-serializing the arguments object
                // orders them. A JSON object is unordered and every tool reads
                // its arguments by name, so the call is identical.
                (
                    "write_file".to_string(),
                    "{\"content\":\"X=1\",\"path\":\"a.env\"}".to_string()
                ),
                ("run_shell".to_string(), "{\"command\":\"./run\"}".to_string()),
            ]
        );
    }

    #[test]
    fn recovers_a_hermes_tag_the_server_template_did_not_parse() {
        let content =
            "<tool_call>{\"name\": \"run_shell\", \"arguments\": {\"command\": \"echo \\\"}\\\"\"}}</tool_call>";
        let (answer, calls) = recover_text_tool_calls(content, &tools(&["run_shell"]));
        // The `}` inside the string value must not end the object early.
        assert_eq!(
            shapes(&calls),
            vec![(
                "run_shell".to_string(),
                "{\"command\":\"echo \\\"}\\\"\"}".to_string()
            )]
        );
        assert_eq!(answer, "");
    }

    #[test]
    fn leaves_json_that_is_not_a_call_for_an_offered_tool_alone() {
        for content in [
            // A tool this turn was not offered.
            "{\"name\": \"run_shell\", \"arguments\": {\"command\": \"ls\"}}",
            // An object that merely documents a call.
            "{\"name\": \"write_file\", \"arguments\": {\"path\": \"a\"}, \"note\": \"example\"}",
            // No arguments at all.
            "{\"name\": \"write_file\"}",
            // Ordinary JSON that happens to have a `name`.
            "{\"name\": \"little-monkey\", \"version\": \"1.7.1\"}",
        ] {
            let (answer, calls) = recover_text_tool_calls(content, &tools(&["write_file"]));
            assert!(calls.is_empty(), "recovered from: {content}");
            assert_eq!(answer, content);
        }
    }

    #[test]
    fn recovers_nothing_when_no_tools_were_offered() {
        let content = "{\"name\": \"run_shell\", \"arguments\": {\"command\": \"ls\"}}";
        let (answer, calls) = recover_text_tool_calls(content, &[]);
        assert!(calls.is_empty());
        assert_eq!(answer, content);
    }

    #[test]
    fn keeps_multibyte_prose_intact_around_a_recovered_call() {
        // Byte-indexed spans over a UTF-8 answer: a naive slice would panic or
        // cut a character in half.
        let content = "Kör det här — nu:\n```json\n{\"name\": \"run_shell\", \"arguments\": {\"command\": \"ls\"}}\n```\nKlart ✅";
        let (answer, calls) = recover_text_tool_calls(content, &tools(&["run_shell"]));
        assert_eq!(calls.len(), 1);
        assert_eq!(answer, "Kör det här — nu:\n\nKlart ✅");
    }
}
