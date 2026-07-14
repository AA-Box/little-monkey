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
