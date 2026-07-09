//! The agentic tool-calling loop — a Rust port of `src/lib/agentLoop.ts`'s
//! `runAgentTurn`: send the conversation (plus tool defs) to the model,
//! stream its reply to stdout, and whenever it asks for tool calls, execute
//! them through `tools_cli.rs` (permission-gated the same way the GUI's
//! Tauri commands are, just via terminal prompts instead of a modal), feed
//! the results back as `tool` messages, and repeat. Ends as soon as a turn
//! produces a plain answer with no tool calls, or after MAX_ITERATIONS round
//! trips as a safety cap against a runaway/looping model.

use little_monkey_lib::AppState;

use crate::chat::{self, Target};
use crate::permission::TerminalPermissions;
use crate::tools_cli;
use crate::tools_def::tool_definitions;

const MAX_ITERATIONS: usize = 25;

fn preview(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}… ({} more chars)", s.chars().count() - max)
    }
}

/// Executes a single model-requested tool call, returning the string to use
/// as the resulting `tool` message's content. Never propagates an error as
/// a hard failure — the model sees it as a JSON `{"error": ...}` payload
/// instead, so it can react and retry rather than crashing the whole turn.
async fn execute_tool_call(
    state: &AppState,
    perms: &mut TerminalPermissions,
    name: &str,
    raw_arguments: &str,
) -> String {
    let args: serde_json::Value = if raw_arguments.trim().is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_str(raw_arguments) {
            Ok(v) => v,
            Err(e) => {
                return serde_json::json!({
                    "error": format!("Invalid tool call arguments JSON for \"{name}\": {e}")
                })
                .to_string()
            }
        }
    };

    let result: Result<serde_json::Value, String> = match name {
        "read_file" => tools_cli::read_file(state, args["path"].as_str().unwrap_or_default())
            .map(serde_json::Value::String),
        "list_dir" => tools_cli::list_dir(state, args["path"].as_str().unwrap_or_default())
            .map(serde_json::Value::Array),
        "grep" => tools_cli::grep(state, args["pattern"].as_str().unwrap_or_default(), args["path"].as_str())
            .map(serde_json::Value::Array),
        "write_file" => {
            tools_cli::write_file(
                state,
                perms,
                args["path"].as_str().unwrap_or_default(),
                args["content"].as_str().unwrap_or_default(),
            )
            .await
            .map(serde_json::Value::String)
        }
        "edit_file" => {
            tools_cli::edit_file(
                state,
                perms,
                args["path"].as_str().unwrap_or_default(),
                args["old_string"].as_str().unwrap_or_default(),
                args["new_string"].as_str().unwrap_or_default(),
            )
            .await
            .map(serde_json::Value::String)
        }
        "run_shell" => {
            tools_cli::run_shell(state, perms, args["command"].as_str().unwrap_or_default(), args["cwd"].as_str())
                .await
        }
        other => Err(format!("Unknown tool \"{other}\"")),
    };

    match result {
        Ok(serde_json::Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(err) => serde_json::json!({ "error": err }).to_string(),
    }
}

/// Inserts (or replaces) the leading system message when `--system` is set —
/// the same message shape works on both wire protocols.
fn apply_system_prompt(history: &mut Vec<serde_json::Value>, system: &str) {
    let message = serde_json::json!({ "role": "system", "content": system });
    match history.first() {
        Some(first) if first["role"] == "system" => history[0] = message,
        _ => history.insert(0, message),
    }
}

/// Builds the user message, attaching any images referenced by path in the
/// prompt using the target's wire format: native Ollama takes a sibling
/// base64 `images` array, OpenAI-compat multi-part content with data: URLs.
fn build_user_message(target: &Target, user_text: &str) -> Result<serde_json::Value, String> {
    let (clean, images) = chat::extract_image_paths(user_text);
    if images.is_empty() {
        return Ok(serde_json::json!({ "role": "user", "content": user_text }));
    }
    for path in &images {
        eprintln!("Added image '{}'", path.display());
    }
    if target.is_native() {
        let mut encoded = Vec::new();
        for path in &images {
            encoded.push(chat::encode_image(path)?.0);
        }
        Ok(serde_json::json!({ "role": "user", "content": clean, "images": encoded }))
    } else {
        let mut parts = vec![serde_json::json!({ "type": "text", "text": clean })];
        for path in &images {
            let (data, mime) = chat::encode_image(path)?;
            parts.push(serde_json::json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{mime};base64,{data}") },
            }));
        }
        Ok(serde_json::json!({ "role": "user", "content": parts }))
    }
}

/// Runs one full agentic turn for `user_text`: appends it to `history`, then
/// repeatedly calls the model with the full history and available tools,
/// printing its reply as it streams and executing any requested tool calls,
/// until it answers without requesting further tools or the safety cap hits.
pub async fn run_turn(
    client: &reqwest::Client,
    target: &Target,
    state: &AppState,
    perms: &mut TerminalPermissions,
    history: &mut Vec<serde_json::Value>,
    options: &chat::ChatOptions,
    user_text: &str,
) -> Result<(), String> {
    if let Some(system) = &options.system {
        apply_system_prompt(history, system);
    }
    history.push(build_user_message(target, user_text)?);

    let tools = tool_definitions();
    let tools_vec: Vec<serde_json::Value> = tools.as_array().cloned().unwrap_or_default();
    let native = target.is_native();

    for _ in 0..MAX_ITERATIONS {
        let result = chat::stream_turn(client, target, history.as_slice(), &tools_vec, options).await?;
        println!();
        if let (true, Some(metrics)) = (options.verbose, &result.metrics) {
            chat::print_verbose_metrics(metrics);
        } else if let Some(usage) = &result.usage {
            let rate = if options.verbose && result.elapsed_secs > 0.0 {
                format!(", {:.1} tok/s", usage.completion_tokens as f64 / result.elapsed_secs)
            } else {
                String::new()
            };
            eprintln!(
                "[tokens: {} prompt + {} completion = {} total{rate}]",
                usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
            );
        }

        let mut assistant_message = serde_json::json!({ "role": "assistant", "content": result.content });

        if result.tool_calls.is_empty() {
            history.push(assistant_message);
            return Ok(());
        }

        assistant_message["tool_calls"] = serde_json::json!(result
            .tool_calls
            .iter()
            .map(|c| if native {
                // Native shape: arguments as a JSON object, no id/type. The
                // string round-trips losslessly — it was serialized from the
                // daemon's object in the first place.
                let arguments: serde_json::Value =
                    serde_json::from_str(&c.arguments).unwrap_or_else(|_| serde_json::json!({}));
                serde_json::json!({ "function": { "name": c.name, "arguments": arguments } })
            } else {
                serde_json::json!({
                    "id": c.id,
                    "type": "function",
                    "function": { "name": c.name, "arguments": c.arguments },
                })
            })
            .collect::<Vec<_>>());
        history.push(assistant_message);

        for call in &result.tool_calls {
            println!("\n[tool] {}({})", call.name, call.arguments);
            let content = execute_tool_call(state, perms, &call.name, &call.arguments).await;
            println!("[tool result] {}", preview(&content, 300));
            history.push(if native {
                serde_json::json!({ "role": "tool", "tool_name": call.name, "content": content })
            } else {
                serde_json::json!({ "role": "tool", "tool_call_id": call.id, "content": content })
            });
        }
    }

    let message = format!(
        "Stopped after reaching the safety limit of {MAX_ITERATIONS} tool-calling iterations without a final answer."
    );
    history.push(serde_json::json!({ "role": "assistant", "content": message }));
    println!("\n{message}");

    Ok(())
}
