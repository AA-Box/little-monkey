//! Native Ollama daemon HTTP API client (`/api/*`) — model management
//! (tags/ps/show/pull/push/create/delete/copy) plus the native chat stream.
//! Unlike the GUI's `little_monkey_lib::ollama` (which shells out to the
//! `ollama` binary for pull/create), everything here speaks the daemon's
//! HTTP API directly, so monkey-cli only needs a reachable daemon. All endpoints
//! resolve their base URL through `host()`, honoring `OLLAMA_HOST`.
use futures_util::StreamExt;
use little_monkey_lib::providers::Utf8ChunkAccumulator;
use serde::{Deserialize, Serialize};

/// Default daemon address, matching Ollama's own default bind.
const DEFAULT_HOST: &str = "http://127.0.0.1:11434";

/// Resolve the daemon base URL from `OLLAMA_HOST` (accepts `host`,
/// `host:port`, or `http(s)://host[:port]`; scheme defaults to http, port to
/// 11434), falling back to `http://127.0.0.1:11434` when unset.
pub fn host() -> String {
    match std::env::var("OLLAMA_HOST") {
        Ok(raw) => parse_host(&raw),
        Err(_) => DEFAULT_HOST.to_string(),
    }
}

fn parse_host(raw: &str) -> String {
    let trimmed = raw.trim();
    let (scheme, rest) = if let Some(r) = trimmed.strip_prefix("https://") {
        ("https", r)
    } else if let Some(r) = trimmed.strip_prefix("http://") {
        ("http", r)
    } else {
        ("http", trimmed)
    };
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() {
        return DEFAULT_HOST.to_string();
    }
    // A bare (unbracketed) IPv6 address contains two or more colons; bracket
    // it before appending the default port so the URL stays unambiguous
    // (matching Ollama's own net.JoinHostPort handling). Specifying a port
    // with an IPv6 host requires the bracketed form.
    if !rest.starts_with('[') && rest.matches(':').count() >= 2 {
        return format!("{scheme}://[{rest}]:11434");
    }
    // Treat a trailing `:digits` as an explicit port; anything else gets the
    // default port appended.
    let has_port = match rest.rsplit_once(':') {
        Some((_, p)) => !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()),
        None => false,
    };
    if has_port {
        format!("{scheme}://{rest}")
    } else {
        format!("{scheme}://{rest}:11434")
    }
}

fn api(path: &str) -> String {
    format!("{}{path}", host())
}

#[derive(Debug, Deserialize)]
pub struct VersionResp {
    #[allow(dead_code)] // no CLI consumer yet; kept for API completeness
    pub version: String,
}

/// One entry in `GET /api/tags`' `models` array.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelTag {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub modified_at: String,
    #[serde(default)]
    pub digest: String,
}

#[derive(Debug, Deserialize)]
pub struct TagsResp {
    #[serde(default)]
    pub models: Vec<ModelTag>,
}

/// One entry in `GET /api/ps`' `models` array (a loaded/running model).
#[derive(Debug, Clone, Deserialize)]
pub struct PsModel {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub size_vram: u64,
    #[serde(default)]
    pub digest: String,
    #[serde(default)]
    pub expires_at: String,
}

#[derive(Debug, Deserialize)]
pub struct PsResp {
    #[serde(default)]
    pub models: Vec<PsModel>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ShowDetails {
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub parameter_size: String,
    #[serde(default)]
    pub quantization_level: String,
    #[allow(dead_code)] // parsed but not shown in the summary block
    #[serde(default)]
    pub format: String,
}

/// `POST /api/show` response — every section optional-with-default since the
/// daemon omits ones a model doesn't have.
#[derive(Debug, Default, Deserialize)]
pub struct ShowResp {
    #[serde(default)]
    pub modelfile: String,
    #[serde(default)]
    pub parameters: String,
    #[serde(default)]
    pub template: String,
    #[serde(default)]
    pub system: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub details: ShowDetails,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub model_info: serde_json::Value,
}

/// One NDJSON progress line from `/api/pull`, `/api/push`, or `/api/create`.
#[derive(Debug, Clone, Deserialize)]
pub struct ProgressLine {
    #[serde(default)]
    pub status: Option<String>,
    #[allow(dead_code)] // the layer digest already appears in `status`
    #[serde(default)]
    pub digest: Option<String>,
    #[serde(default)]
    pub total: Option<u64>,
    #[serde(default)]
    pub completed: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
}

/// A single `{role, content}` chat message, as used by `/api/create`'s
/// `messages` array (and Modelfile `MESSAGE` instructions).
#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// `POST /api/create` request body (Ollama 0.31 schema).
#[derive(Debug, Default, Serialize)]
pub struct CreateRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<std::collections::HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapters: Option<std::collections::HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<ChatMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantize: Option<String>,
    pub stream: bool,
}

/// `POST /api/chat` request body (native API, not the OpenAI-compat shim).
#[derive(Debug, Serialize)]
pub struct NativeChatReq {
    pub model: String,
    pub messages: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<serde_json::Value>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub think: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<serde_json::Value>,
}

/// A tool call from the native chat stream. Unlike the OpenAI SSE stream,
/// these arrive complete on a single line with `arguments` as a JSON object
/// (not string fragments) — no accumulation needed.
#[derive(Debug, Clone)]
pub struct NativeToolCall {
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Timing/token metrics from the final (`done: true`) chat line.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChatMetrics {
    #[allow(dead_code)] // read only in tests; --verbose doesn't print it
    #[serde(default)]
    pub done_reason: Option<String>,
    #[serde(default)]
    pub total_duration: Option<u64>,
    #[serde(default)]
    pub load_duration: Option<u64>,
    #[serde(default)]
    pub prompt_eval_count: Option<u64>,
    #[serde(default)]
    pub prompt_eval_duration: Option<u64>,
    #[serde(default)]
    pub eval_count: Option<u64>,
    #[serde(default)]
    pub eval_duration: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum ChatEvent {
    Content(String),
    Thinking(String),
    ToolCall(NativeToolCall),
    Done(ChatMetrics),
}

/// Maps a non-2xx response to an `Err`, surfacing the daemon's JSON
/// `{"error": ...}` message verbatim when present.
async fn check_status(response: reqwest::Response, what: &str) -> Result<reqwest::Response, String> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
        .unwrap_or(body);
    Err(if detail.is_empty() {
        format!("{what} failed (HTTP {status})")
    } else {
        format!("{what} failed (HTTP {status}): {detail}")
    })
}

async fn get_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    path: &str,
    what: &str,
) -> Result<T, String> {
    let response = client
        .get(api(path))
        .send()
        .await
        .map_err(|e| format!("Failed to reach Ollama: {e}"))?;
    let response = check_status(response, what).await?;
    response
        .json::<T>()
        .await
        .map_err(|e| format!("Failed to parse {what} response: {e}"))
}

#[allow(dead_code)] // no CLI consumer yet; kept for API completeness
pub async fn version(client: &reqwest::Client) -> Result<VersionResp, String> {
    get_json(client, "/api/version", "version").await
}

pub async fn tags(client: &reqwest::Client) -> Result<TagsResp, String> {
    get_json(client, "/api/tags", "list").await
}

pub async fn ps(client: &reqwest::Client) -> Result<PsResp, String> {
    get_json(client, "/api/ps", "ps").await
}

pub async fn show(client: &reqwest::Client, model: &str) -> Result<ShowResp, String> {
    let response = client
        .post(api("/api/show"))
        .json(&serde_json::json!({ "model": model }))
        .send()
        .await
        .map_err(|e| format!("Failed to reach Ollama: {e}"))?;
    let response = check_status(response, "show").await?;
    response
        .json()
        .await
        .map_err(|e| format!("Failed to parse show response: {e}"))
}

pub async fn delete(client: &reqwest::Client, model: &str) -> Result<(), String> {
    let response = client
        .delete(api("/api/delete"))
        .json(&serde_json::json!({ "model": model }))
        .send()
        .await
        .map_err(|e| format!("Failed to reach Ollama: {e}"))?;
    check_status(response, "delete").await.map(|_| ())
}

pub async fn copy(client: &reqwest::Client, source: &str, destination: &str) -> Result<(), String> {
    let response = client
        .post(api("/api/copy"))
        .json(&serde_json::json!({ "source": source, "destination": destination }))
        .send()
        .await
        .map_err(|e| format!("Failed to reach Ollama: {e}"))?;
    check_status(response, "copy").await.map(|_| ())
}

/// Unload a model from memory (= `ollama stop`): a chat request with an
/// empty message list and `keep_alive: 0`. The reply body is ignored.
pub async fn unload(client: &reqwest::Client, model: &str) -> Result<(), String> {
    let response = client
        .post(api("/api/chat"))
        .json(&serde_json::json!({
            "model": model,
            "messages": [],
            "keep_alive": 0,
            "stream": false,
        }))
        .send()
        .await
        .map_err(|e| format!("Failed to reach Ollama: {e}"))?;
    check_status(response, "stop").await.map(|_| ())
}

pub async fn pull(
    client: &reqwest::Client,
    model: &str,
    insecure: bool,
    on_progress: impl FnMut(ProgressLine),
) -> Result<(), String> {
    let request = client.post(api("/api/pull")).json(&serde_json::json!({
        "model": model,
        "insecure": insecure,
        "stream": true,
    }));
    stream_progress(request, "pull", on_progress).await
}

pub async fn push(
    client: &reqwest::Client,
    model: &str,
    insecure: bool,
    on_progress: impl FnMut(ProgressLine),
) -> Result<(), String> {
    let request = client.post(api("/api/push")).json(&serde_json::json!({
        "model": model,
        "insecure": insecure,
        "stream": true,
    }));
    stream_progress(request, "push", on_progress).await
}

pub async fn create(
    client: &reqwest::Client,
    req: &CreateRequest,
    on_progress: impl FnMut(ProgressLine),
) -> Result<(), String> {
    let request = client.post(api("/api/create")).json(req);
    stream_progress(request, "create", on_progress).await
}

/// Streams one native chat turn, invoking `on_event` for each content /
/// thinking delta, complete tool call, and the final metrics line. An
/// `{"error": ...}` line (or non-2xx response) becomes an `Err`.
pub async fn chat_stream(
    client: &reqwest::Client,
    req: &NativeChatReq,
    mut on_event: impl FnMut(ChatEvent),
) -> Result<(), String> {
    let response = client
        .post(api("/api/chat"))
        .json(req)
        .send()
        .await
        .map_err(|e| format!("Failed to reach Ollama: {e}"))?;
    let response = check_status(response, "chat").await?;
    stream_ndjson(response, |line| parse_chat_line(line, &mut on_event)).await
}

/// Shared NDJSON progress plumbing for pull/push/create: sends `request`,
/// then parses each response line into a `ProgressLine`. A line carrying an
/// `error` field aborts with that message.
async fn stream_progress(
    request: reqwest::RequestBuilder,
    what: &str,
    mut on_progress: impl FnMut(ProgressLine),
) -> Result<(), String> {
    let response = request
        .send()
        .await
        .map_err(|e| format!("Failed to reach Ollama: {e}"))?;
    let response = check_status(response, what).await?;
    stream_ndjson(response, |line| {
        let progress: ProgressLine = serde_json::from_str(line)
            .map_err(|e| format!("Failed to parse {what} progress line: {e}"))?;
        if let Some(error) = progress.error {
            return Err(error);
        }
        on_progress(progress);
        Ok(())
    })
    .await
}

/// Reads a response body as newline-delimited JSON, invoking `on_line` for
/// each complete non-empty line (handling lines split across chunks and
/// UTF-8 sequences split across chunk boundaries).
async fn stream_ndjson(
    response: reqwest::Response,
    mut on_line: impl FnMut(&str) -> Result<(), String>,
) -> Result<(), String> {
    let mut stream = response.bytes_stream();
    let mut acc = Utf8ChunkAccumulator::new();
    let mut buffer = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Stream error: {e}"))?;
        buffer.push_str(&acc.push(&chunk));
        while let Some(pos) = buffer.find('\n') {
            let line: String = buffer.drain(..=pos).collect();
            let line = line.trim();
            if !line.is_empty() {
                on_line(line)?;
            }
        }
    }
    if let Some(tail) = acc.finish() {
        buffer.push_str(&tail);
    }
    let tail = buffer.trim();
    if !tail.is_empty() {
        on_line(tail)?;
    }
    Ok(())
}

/// Parses one native chat NDJSON line into zero or more `ChatEvent`s.
fn parse_chat_line(line: &str, on_event: &mut impl FnMut(ChatEvent)) -> Result<(), String> {
    let payload: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("Failed to parse chat line: {e}"))?;

    if let Some(error) = payload.get("error").and_then(|e| e.as_str()) {
        return Err(error.to_string());
    }

    if let Some(message) = payload.get("message") {
        if let Some(content) = message.get("content").and_then(|c| c.as_str()) {
            if !content.is_empty() {
                on_event(ChatEvent::Content(content.to_string()));
            }
        }
        if let Some(thinking) = message.get("thinking").and_then(|t| t.as_str()) {
            if !thinking.is_empty() {
                on_event(ChatEvent::Thinking(thinking.to_string()));
            }
        }
        if let Some(calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
            for call in calls {
                let function = call.get("function");
                let name = function
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string();
                let arguments = function
                    .and_then(|f| f.get("arguments"))
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                on_event(ChatEvent::ToolCall(NativeToolCall { name, arguments }));
            }
        }
    }

    if payload.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
        let metrics: ChatMetrics = serde_json::from_value(payload).unwrap_or_default();
        on_event(ChatEvent::Done(metrics));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_host_handles_all_accepted_forms() {
        let cases = [
            ("", DEFAULT_HOST),
            ("   ", DEFAULT_HOST),
            ("localhost", "http://localhost:11434"),
            ("example.com:8080", "http://example.com:8080"),
            ("0.0.0.0", "http://0.0.0.0:11434"),
            ("http://10.0.0.5", "http://10.0.0.5:11434"),
            ("http://10.0.0.5:9999", "http://10.0.0.5:9999"),
            ("https://ollama.example.com", "https://ollama.example.com:11434"),
            ("https://ollama.example.com:443", "https://ollama.example.com:443"),
            ("http://localhost:11434/", "http://localhost:11434"),
            ("example.com:8080/", "http://example.com:8080"),
            ("http://", DEFAULT_HOST),
            // Bare IPv6 hosts get bracketed so the default port can attach.
            ("::1", "http://[::1]:11434"),
            ("http://::1", "http://[::1]:11434"),
            ("fe80::ab", "http://[fe80::ab]:11434"),
            ("https://2001:db8::1", "https://[2001:db8::1]:11434"),
            // Already-bracketed forms keep working, with or without a port.
            ("[::1]", "http://[::1]:11434"),
            ("[::1]:8080", "http://[::1]:8080"),
            ("http://[fe80::ab]:9999", "http://[fe80::ab]:9999"),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_host(input), expected, "input: {input:?}");
        }
    }

    #[test]
    fn progress_line_deserializes_ndjson_fixtures() {
        let manifest: ProgressLine = serde_json::from_str(r#"{"status":"pulling manifest"}"#).unwrap();
        assert_eq!(manifest.status.as_deref(), Some("pulling manifest"));
        assert!(manifest.digest.is_none() && manifest.error.is_none());

        let layer: ProgressLine = serde_json::from_str(
            r#"{"status":"pulling ab1c2d3e","digest":"sha256:ab1c2d3e","total":104857600,"completed":52428800}"#,
        )
        .unwrap();
        assert_eq!(layer.digest.as_deref(), Some("sha256:ab1c2d3e"));
        assert_eq!(layer.total, Some(104857600));
        assert_eq!(layer.completed, Some(52428800));

        let error: ProgressLine = serde_json::from_str(r#"{"error":"file does not exist"}"#).unwrap();
        assert_eq!(error.error.as_deref(), Some("file does not exist"));
    }

    fn collect_events(line: &str) -> Result<Vec<ChatEvent>, String> {
        let mut events = Vec::new();
        parse_chat_line(line, &mut |e| events.push(e))?;
        Ok(events)
    }

    #[test]
    fn chat_line_content_and_thinking_deltas() {
        let events = collect_events(
            r#"{"model":"m","message":{"role":"assistant","content":"Hello"},"done":false}"#,
        )
        .unwrap();
        assert!(matches!(&events[..], [ChatEvent::Content(c)] if c == "Hello"));

        let events = collect_events(
            r#"{"model":"m","message":{"role":"assistant","content":"","thinking":"hmm"},"done":false}"#,
        )
        .unwrap();
        assert!(matches!(&events[..], [ChatEvent::Thinking(t)] if t == "hmm"));
    }

    #[test]
    fn chat_line_tool_call_arrives_complete_with_object_arguments() {
        let events = collect_events(
            r#"{"model":"m","message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"read_file","arguments":{"path":"a.txt"}}}]},"done":false}"#,
        )
        .unwrap();
        match &events[..] {
            [ChatEvent::ToolCall(call)] => {
                assert_eq!(call.name, "read_file");
                assert_eq!(call.arguments["path"], "a.txt");
            }
            other => panic!("expected one tool call, got {other:?}"),
        }
    }

    #[test]
    fn chat_line_done_carries_metrics() {
        let events = collect_events(
            r#"{"model":"m","message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","total_duration":5000000000,"load_duration":1000,"prompt_eval_count":10,"prompt_eval_duration":2000,"eval_count":20,"eval_duration":4000000000}"#,
        )
        .unwrap();
        match &events[..] {
            [ChatEvent::Done(m)] => {
                assert_eq!(m.done_reason.as_deref(), Some("stop"));
                assert_eq!(m.total_duration, Some(5000000000));
                assert_eq!(m.prompt_eval_count, Some(10));
                assert_eq!(m.eval_count, Some(20));
                assert_eq!(m.eval_duration, Some(4000000000));
            }
            other => panic!("expected done metrics, got {other:?}"),
        }
    }

    #[test]
    fn chat_line_error_becomes_err() {
        assert_eq!(
            collect_events(r#"{"error":"model not found"}"#).unwrap_err(),
            "model not found"
        );
    }
}
