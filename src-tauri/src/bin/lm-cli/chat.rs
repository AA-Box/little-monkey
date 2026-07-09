//! Streams one chat turn against a model target. Ollama targets (`--ollama`,
//! `run`) speak the daemon's native `/api/chat` NDJSON protocol — which
//! carries options, thinking, format constraints, and keep_alive — while
//! `--local-url` servers and cloud providers use OpenAI-compatible
//! chat-completions SSE (reusing `little_monkey_lib::providers`' keychain-backed
//! key storage, the same as the GUI's Rust-side proxy).

use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use futures_util::StreamExt;
use little_monkey_lib::providers::Utf8ChunkAccumulator;

use crate::ollama_api::{self, ChatEvent, ChatMetrics, NativeChatReq};
use crate::sse::{SseParser, StreamEvent, ToolCallEvent};

pub enum Target {
    Local { base_url: String, model: Option<String>, native_ollama: bool },
    Provider { provider_id: String, model: String },
}

impl Target {
    /// True when this target speaks the native Ollama `/api/chat` protocol
    /// (`--ollama` and `run`) rather than OpenAI chat-completions.
    pub fn is_native(&self) -> bool {
        matches!(self, Target::Local { native_ollama: true, .. })
    }
}

/// Per-session generation options collected from the chat flags. `None` /
/// empty fields are omitted from requests entirely.
#[derive(Debug, Clone, Default)]
pub struct ChatOptions {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub seed: Option<i64>,
    pub stop: Vec<String>,
    pub num_ctx: Option<i64>,
    pub num_predict: Option<i64>,
    pub system: Option<String>,
    /// `Value::String("json")` or an inline JSON schema object.
    pub format: Option<serde_json::Value>,
    /// `Value::Bool` or a `"low"/"medium"/"high"` string.
    pub think: Option<serde_json::Value>,
    pub hide_thinking: bool,
    pub keep_alive: Option<String>,
    pub verbose: bool,
}

/// Parses a `--format` value: `json`, an inline JSON schema, or `@path` to a
/// schema file.
pub fn parse_format_flag(raw: &str) -> Result<serde_json::Value, String> {
    if raw == "json" {
        return Ok(serde_json::Value::String("json".to_string()));
    }
    let text = if let Some(path) = raw.strip_prefix('@') {
        std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read --format schema file '{path}': {e}"))?
    } else {
        raw.to_string()
    };
    serde_json::from_str(&text)
        .map_err(|e| format!("Invalid --format value (expected 'json', an inline JSON schema, or @file): {e}"))
}

/// Parses a `--think` value: the bare flag means `true`, otherwise an
/// explicit true/false or an effort level.
pub fn parse_think_flag(raw: &str) -> Result<serde_json::Value, String> {
    match raw {
        "true" => Ok(serde_json::Value::Bool(true)),
        "false" => Ok(serde_json::Value::Bool(false)),
        "low" | "medium" | "high" => Ok(serde_json::Value::String(raw.to_string())),
        other => Err(format!(
            "Invalid --think value '{other}' (expected true, false, low, medium, or high)"
        )),
    }
}

/// Ollama accepts `keep_alive` as a bare number of seconds or a Go duration
/// string ("5m", "0", "-1"): numeric values are sent as numbers, anything
/// else verbatim.
fn keep_alive_value(raw: &str) -> serde_json::Value {
    match raw.parse::<i64>() {
        Ok(n) => serde_json::Value::from(n),
        Err(_) => serde_json::Value::String(raw.to_string()),
    }
}

/// Splits image attachments out of a prompt: any whitespace-separated token
/// (after stripping quotes) with a png/jpg/jpeg/webp extension that exists
/// on disk becomes an attachment. The prompt is only rewritten (whitespace
/// collapsed) when something actually matched.
pub fn extract_image_paths(prompt: &str) -> (String, Vec<PathBuf>) {
    let mut images = Vec::new();
    let mut kept: Vec<&str> = Vec::new();
    for token in prompt.split_whitespace() {
        let candidate = token.trim_matches(|c| c == '"' || c == '\'');
        if is_image_path(candidate) {
            images.push(PathBuf::from(candidate));
        } else {
            kept.push(token);
        }
    }
    if images.is_empty() {
        (prompt.to_string(), images)
    } else {
        (kept.join(" "), images)
    }
}

fn is_image_path(candidate: &str) -> bool {
    let path = Path::new(candidate);
    let ext_matches = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| matches!(e.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "webp"))
        .unwrap_or(false);
    ext_matches && path.is_file()
}

/// Reads and base64-encodes an image, returning its data and MIME type
/// (native messages want the bare base64; OpenAI content parts a data: URL).
pub fn encode_image(path: &Path) -> Result<(String, &'static str), String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("Failed to read image '{}': {e}", path.display()))?;
    let mime = match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref()
    {
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        _ => "image/jpeg",
    };
    Ok((base64::engine::general_purpose::STANDARD.encode(bytes), mime))
}

pub struct TurnUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

pub struct TurnResult {
    pub content: String,
    pub tool_calls: Vec<ToolCallEvent>,
    pub usage: Option<TurnUsage>,
    /// Timing metrics from the native path's final `done` line (`None` on
    /// OpenAI-compat paths).
    pub metrics: Option<ChatMetrics>,
    /// Wall-clock seconds the turn took, for `--verbose`'s tok/s.
    pub elapsed_secs: f64,
}

/// Streams one turn, printing assistant text to stdout as it arrives (so the
/// CLI "feels" like it's typing, same as the GUI's streamed bubble) and
/// collecting any requested tool calls to return once the stream ends.
pub async fn stream_turn(
    client: &reqwest::Client,
    target: &Target,
    messages: &[serde_json::Value],
    tools: &[serde_json::Value],
    options: &ChatOptions,
) -> Result<TurnResult, String> {
    let started = std::time::Instant::now();
    if let Target::Local { model, native_ollama: true, .. } = target {
        let model = model.clone().unwrap_or_default();
        return stream_turn_native(client, &model, messages, tools, options, started).await;
    }

    let request = match target {
        Target::Local { base_url, model, .. } => {
            let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));
            let mut body = serde_json::json!({
                "messages": messages,
                "tools": tools,
                "tool_choice": "auto",
                "stream": true,
                "stream_options": { "include_usage": true },
                "model": model.clone().unwrap_or_else(|| "local".to_string()),
            });
            apply_openai_options(&mut body, options);
            client.post(url).json(&body)
        }
        Target::Provider { provider_id, model } => {
            // Same request `little_monkey_lib::providers::build_chat_request` builds,
            // inlined so the chat options (and the usage fix below) can be
            // attached without widening the GUI-shared helper.
            let custom = crate::providers_cli::load_custom_providers();
            let base_url = little_monkey_lib::providers::resolve_base_url(provider_id, &custom)?;
            let api_key = little_monkey_lib::providers::read_key(provider_id)?;
            let mut body = serde_json::json!({
                "messages": messages,
                "tools": tools,
                "tool_choice": "auto",
                "stream": true,
                "model": model,
            });
            // stream_options is OpenAI-schema-specific; anthropic/gemini's
            // compat layers aren't verified to tolerate it, so skip them.
            if !matches!(provider_id.as_str(), "anthropic" | "gemini") {
                body["stream_options"] = serde_json::json!({ "include_usage": true });
            }
            apply_openai_options(&mut body, options);
            let mut request =
                client.post(format!("{base_url}/chat/completions")).bearer_auth(&api_key).json(&body);
            if provider_id == "anthropic" {
                request = request
                    .header("x-api-key", &api_key)
                    .header("anthropic-version", "2023-06-01");
            }
            request
        }
    };

    let response = request.send().await.map_err(|e| format!("Request failed: {e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        return Err(format!(
            "Request failed ({status}){}",
            if detail.is_empty() { String::new() } else { format!(": {detail}") }
        ));
    }

    let mut stream = response.bytes_stream();
    let mut acc = Utf8ChunkAccumulator::new();
    let mut parser = SseParser::new();
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    let mut usage = None;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Stream error: {e}"))?;
        let text = acc.push(&chunk);
        if !text.is_empty() {
            for event in parser.feed(&text) {
                apply_event(event, &mut content, &mut tool_calls, &mut usage);
            }
        }
    }
    if let Some(tail) = acc.finish() {
        for event in parser.feed(&tail) {
            apply_event(event, &mut content, &mut tool_calls, &mut usage);
        }
    }
    for event in parser.flush() {
        apply_event(event, &mut content, &mut tool_calls, &mut usage);
    }

    Ok(TurnResult { content, tool_calls, usage, metrics: None, elapsed_secs: started.elapsed().as_secs_f64() })
}

/// The native `/api/chat` turn: an NDJSON stream with options/think/format/
/// keep_alive attached. Tool calls arrive complete (arguments as a JSON
/// object) and are adapted into the same `ToolCallEvent` shape the OpenAI
/// SSE path produces — a synthesized `call_N` id and the arguments
/// re-serialized to a string — so `agent.rs` consumes both paths uniformly.
async fn stream_turn_native(
    client: &reqwest::Client,
    model: &str,
    messages: &[serde_json::Value],
    tools: &[serde_json::Value],
    options: &ChatOptions,
    started: std::time::Instant,
) -> Result<TurnResult, String> {
    let mut opts = serde_json::Map::new();
    if let Some(temperature) = options.temperature {
        opts.insert("temperature".to_string(), serde_json::json!(temperature));
    }
    if let Some(top_p) = options.top_p {
        opts.insert("top_p".to_string(), serde_json::json!(top_p));
    }
    if let Some(seed) = options.seed {
        opts.insert("seed".to_string(), serde_json::json!(seed));
    }
    if !options.stop.is_empty() {
        opts.insert("stop".to_string(), serde_json::json!(options.stop));
    }
    if let Some(num_ctx) = options.num_ctx {
        opts.insert("num_ctx".to_string(), serde_json::json!(num_ctx));
    }
    if let Some(num_predict) = options.num_predict {
        opts.insert("num_predict".to_string(), serde_json::json!(num_predict));
    }

    let req = NativeChatReq {
        model: model.to_string(),
        messages: messages.to_vec(),
        tools: if tools.is_empty() { None } else { Some(serde_json::Value::Array(tools.to_vec())) },
        stream: true,
        options: if opts.is_empty() { None } else { Some(opts) },
        keep_alive: options.keep_alive.as_deref().map(keep_alive_value),
        think: options.think.clone(),
        format: options.format.clone(),
    };

    let mut content = String::new();
    let mut tool_calls: Vec<ToolCallEvent> = Vec::new();
    let mut metrics: Option<ChatMetrics> = None;
    let mut thinking_open = false;

    ollama_api::chat_stream(client, &req, |event| match event {
        ChatEvent::Thinking(text) => {
            if !options.hide_thinking {
                if !thinking_open {
                    println!("Thinking...");
                    thinking_open = true;
                }
                print!("{text}");
                std::io::stdout().flush().ok();
            }
        }
        ChatEvent::Content(text) => {
            if thinking_open {
                print!("\n...done thinking.\n\n");
                thinking_open = false;
            }
            print!("{text}");
            std::io::stdout().flush().ok();
            content.push_str(&text);
        }
        ChatEvent::ToolCall(call) => {
            let id = format!("call_{}", tool_calls.len() + 1);
            tool_calls.push(ToolCallEvent { id, name: call.name, arguments: call.arguments.to_string() });
        }
        ChatEvent::Done(m) => metrics = Some(m),
    })
    .await?;
    if thinking_open {
        print!("\n...done thinking.\n\n");
        std::io::stdout().flush().ok();
    }

    let usage = metrics.as_ref().map(|m| {
        let prompt_tokens = m.prompt_eval_count.unwrap_or(0);
        let completion_tokens = m.eval_count.unwrap_or(0);
        TurnUsage { prompt_tokens, completion_tokens, total_tokens: prompt_tokens + completion_tokens }
    });
    Ok(TurnResult { content, tool_calls, usage, metrics, elapsed_secs: started.elapsed().as_secs_f64() })
}

/// Adds the OpenAI-schema equivalents of the set chat options to a
/// chat-completions body. num_ctx/keep_alive/think have no equivalent there
/// (the caller warns once when they're set on a non-native target).
fn apply_openai_options(body: &mut serde_json::Value, options: &ChatOptions) {
    if let Some(temperature) = options.temperature {
        body["temperature"] = serde_json::json!(temperature);
    }
    if let Some(top_p) = options.top_p {
        body["top_p"] = serde_json::json!(top_p);
    }
    if let Some(seed) = options.seed {
        body["seed"] = serde_json::json!(seed);
    }
    if !options.stop.is_empty() {
        body["stop"] = serde_json::json!(options.stop);
    }
    if let Some(num_predict) = options.num_predict {
        body["max_tokens"] = serde_json::json!(num_predict);
    }
    match &options.format {
        Some(serde_json::Value::String(s)) if s == "json" => {
            body["response_format"] = serde_json::json!({ "type": "json_object" });
        }
        Some(schema) => {
            body["response_format"] = serde_json::json!({
                "type": "json_schema",
                "json_schema": { "name": "response", "schema": schema },
            });
        }
        None => {}
    }
}

/// Prints `ollama run --verbose`'s post-response metrics block to stderr.
pub fn print_verbose_metrics(metrics: &ChatMetrics) {
    let rate = |count: Option<u64>, ns: Option<u64>| {
        let secs = ns.unwrap_or(0) as f64 / 1e9;
        if secs > 0.0 {
            format!("{:.2} tokens/s", count.unwrap_or(0) as f64 / secs)
        } else {
            "-".to_string()
        }
    };
    eprintln!();
    eprintln!("total duration:       {}", fmt_duration_ns(metrics.total_duration.unwrap_or(0)));
    eprintln!("load duration:        {}", fmt_duration_ns(metrics.load_duration.unwrap_or(0)));
    eprintln!("prompt eval count:    {} token(s)", metrics.prompt_eval_count.unwrap_or(0));
    eprintln!("prompt eval duration: {}", fmt_duration_ns(metrics.prompt_eval_duration.unwrap_or(0)));
    eprintln!("prompt eval rate:     {}", rate(metrics.prompt_eval_count, metrics.prompt_eval_duration));
    eprintln!("eval count:           {} token(s)", metrics.eval_count.unwrap_or(0));
    eprintln!("eval duration:        {}", fmt_duration_ns(metrics.eval_duration.unwrap_or(0)));
    eprintln!("eval rate:            {}", rate(metrics.eval_count, metrics.eval_duration));
}

/// Humanizes a nanosecond duration, roughly like Go's `Duration.String()`.
fn fmt_duration_ns(ns: u64) -> String {
    if ns < 1_000 {
        format!("{ns}ns")
    } else if ns < 1_000_000 {
        format!("{:.3}µs", ns as f64 / 1e3)
    } else if ns < 1_000_000_000 {
        format!("{:.3}ms", ns as f64 / 1e6)
    } else if ns < 60_000_000_000 {
        format!("{:.3}s", ns as f64 / 1e9)
    } else {
        let secs = ns as f64 / 1e9;
        format!("{}m{:.3}s", (secs / 60.0) as u64, secs % 60.0)
    }
}

fn apply_event(
    event: StreamEvent,
    content: &mut String,
    tool_calls: &mut Vec<ToolCallEvent>,
    usage: &mut Option<TurnUsage>,
) {
    match event {
        StreamEvent::Delta(text) => {
            print!("{text}");
            std::io::stdout().flush().ok();
            content.push_str(&text);
        }
        StreamEvent::ToolCall(call) => tool_calls.push(call),
        StreamEvent::Usage { prompt_tokens, completion_tokens, total_tokens } => {
            *usage = Some(TurnUsage { prompt_tokens, completion_tokens, total_tokens });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_format_flag_forms() {
        assert_eq!(parse_format_flag("json").unwrap(), serde_json::json!("json"));

        let schema = parse_format_flag(r#"{"type":"object","properties":{"a":{"type":"string"}}}"#).unwrap();
        assert_eq!(schema["type"], "object");

        let dir = std::env::temp_dir().join("lm_cli_chat_test_format");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("schema.json");
        std::fs::write(&file, r#"{"type":"object"}"#).unwrap();
        let from_file = parse_format_flag(&format!("@{}", file.display())).unwrap();
        assert_eq!(from_file["type"], "object");

        assert!(parse_format_flag("not json").is_err());
        assert!(parse_format_flag("@/definitely/not/a/file.json").is_err());
    }

    #[test]
    fn parse_think_flag_levels_and_bools() {
        assert_eq!(parse_think_flag("true").unwrap(), serde_json::json!(true));
        assert_eq!(parse_think_flag("false").unwrap(), serde_json::json!(false));
        assert_eq!(parse_think_flag("high").unwrap(), serde_json::json!("high"));
        assert!(parse_think_flag("max").is_err());
    }

    #[test]
    fn keep_alive_numbers_stay_numbers() {
        assert_eq!(keep_alive_value("0"), serde_json::json!(0));
        assert_eq!(keep_alive_value("-1"), serde_json::json!(-1));
        assert_eq!(keep_alive_value("5m"), serde_json::json!("5m"));
    }

    #[test]
    fn extract_image_paths_finds_existing_images_only() {
        let dir = std::env::temp_dir().join("lm_cli_chat_test_images");
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("cat.png");
        std::fs::write(&img, [0x89u8, b'P', b'N', b'G']).unwrap();

        let prompt = format!("describe \"{}\" please", img.display());
        let (clean, images) = extract_image_paths(&prompt);
        assert_eq!(images, vec![img.clone()]);
        assert_eq!(clean, "describe please");

        // Non-existent path and non-image extension are left alone.
        let (clean, images) = extract_image_paths("look at /no/such/file.png and notes.txt");
        assert!(images.is_empty());
        assert_eq!(clean, "look at /no/such/file.png and notes.txt");

        // No matches: prompt returned verbatim, whitespace intact.
        let (clean, images) = extract_image_paths("two  spaces\nand a newline");
        assert!(images.is_empty());
        assert_eq!(clean, "two  spaces\nand a newline");
    }

    #[test]
    fn apply_openai_options_maps_fields() {
        let options = ChatOptions {
            temperature: Some(0.0),
            top_p: Some(0.9),
            seed: Some(7),
            stop: vec!["END".to_string()],
            num_predict: Some(128),
            format: Some(serde_json::json!("json")),
            ..Default::default()
        };
        let mut body = serde_json::json!({ "stream": true });
        apply_openai_options(&mut body, &options);
        assert_eq!(body["temperature"], 0.0);
        assert_eq!(body["top_p"], 0.9);
        assert_eq!(body["seed"], 7);
        assert_eq!(body["stop"], serde_json::json!(["END"]));
        assert_eq!(body["max_tokens"], 128);
        assert_eq!(body["response_format"], serde_json::json!({ "type": "json_object" }));

        // A schema becomes response_format json_schema; nothing set adds nothing.
        let schema_options = ChatOptions {
            format: Some(serde_json::json!({ "type": "object" })),
            ..Default::default()
        };
        let mut body = serde_json::json!({ "stream": true });
        apply_openai_options(&mut body, &schema_options);
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["schema"]["type"], "object");

        let mut body = serde_json::json!({ "stream": true });
        apply_openai_options(&mut body, &ChatOptions::default());
        assert_eq!(body, serde_json::json!({ "stream": true }));
    }

    #[test]
    fn fmt_duration_ns_buckets() {
        assert_eq!(fmt_duration_ns(500), "500ns");
        assert_eq!(fmt_duration_ns(1_500), "1.500µs");
        assert_eq!(fmt_duration_ns(2_746_875), "2.747ms");
        assert_eq!(fmt_duration_ns(8_583_802_625), "8.584s");
        assert_eq!(fmt_duration_ns(90_000_000_000), "1m30.000s");
    }
}
