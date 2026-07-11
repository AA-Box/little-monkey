//! The agentic tool-calling loop — a Rust port of `src/lib/agentLoop.ts`'s
//! `runAgentTurn`: send the conversation (plus tool defs) to the model,
//! stream its reply to stdout, and whenever it asks for tool calls, execute
//! them through `tools_cli.rs` (permission-gated the same way the GUI's
//! Tauri commands are, just via terminal prompts instead of a modal), feed
//! the results back as `tool` messages, and repeat. Ends as soon as a turn
//! produces a plain answer with no tool calls, or after MAX_ITERATIONS round
//! trips as a safety cap against a runaway/looping model.

use little_monkey_lib::checkpoints;
use little_monkey_lib::mcp::McpServerEntry;
use little_monkey_lib::web;
use little_monkey_lib::AppState;

use crate::checkpoints_cli;
use crate::chat::{self, Target};
use crate::mcp_cli;
use crate::permission::TerminalPermissions;
use crate::tools_cli;
use crate::tools_def::{self, McpToolRegistry};
use crate::web_cli;

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
    checkpoint_id: Option<&str>,
    mcp_entries: &[McpServerEntry],
    mcp_registry: &McpToolRegistry,
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

    // `mcp__<serverId>__<toolName>`-named calls dispatch to the connected
    // MCP server via `mcp_cli::call` instead of the `tool_<name>` switch
    // below — resolved through the registry `tools_def::merged_tool_definitions`
    // built for this turn, not by re-parsing `name` (see that module's doc
    // comment for why a naive `__`-split isn't reliable), same division of
    // labor as `agentLoop.ts`'s `invokeMcpTool`/`resolveMcpToolName`.
    if let Some((server_id, tool_name)) = mcp_registry.0.get(name) {
        return match mcp_entries.iter().find(|e| &e.id == server_id) {
            Some(entry) => match mcp_cli::call(state, perms, entry, tool_name, args).await {
                Ok(content) => content,
                Err(err) => serde_json::json!({ "error": err }).to_string(),
            },
            None => serde_json::json!({
                "error": format!("MCP server '{server_id}' is not connected")
            })
            .to_string(),
        };
    }

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
                checkpoint_id,
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
                checkpoint_id,
            )
            .await
            .map(serde_json::Value::String)
        }
        "run_shell" => {
            tools_cli::run_shell(
                state,
                perms,
                args["command"].as_str().unwrap_or_default(),
                args["cwd"].as_str(),
                checkpoint_id,
            )
            .await
        }
        "remember" => tools_cli::remember(state, perms, args["text"].as_str().unwrap_or_default())
            .await
            .and_then(|fact| serde_json::to_value(fact).map_err(|e| e.to_string())),
        // Both web tools always prompt outside `--mode bypass` — same
        // `TerminalPermissions::request` choke point every other
        // permission-gated tool goes through — and then call
        // `little_monkey_lib::web::{fetch_impl,search_impl}` directly (the
        // AppHandle-free lib fns the desktop app's `tool_web_fetch`/
        // `tool_web_search` commands also call), rather than duplicating the
        // fetch/search pipeline a third time. `web_cli::load_settings()`
        // reads the identical `web_settings.json` the GUI's Settings > Web
        // tab writes, resolved via the same hardcoded-identifier convention
        // `providers_cli.rs` uses for `providers.json`.
        "web_fetch" => {
            let url = args["url"].as_str().unwrap_or_default().to_string();
            match perms.request("web_fetch", &url).await {
                Ok(()) => {
                    let settings = web_cli::load_settings();
                    let max_chars = args["max_chars"].as_u64().map(|v| v as usize);
                    let start_index = args["start_index"].as_u64().map(|v| v as usize);
                    web::fetch_impl(&settings, url, max_chars, start_index)
                        .await
                        .and_then(|result| serde_json::to_value(result).map_err(|e| e.to_string()))
                }
                Err(e) => Err(e),
            }
        }
        "web_search" => {
            let query = args["query"].as_str().unwrap_or_default().to_string();
            match perms.request("web_search", &query).await {
                Ok(()) => {
                    let settings = web_cli::load_settings();
                    // Only resolved when actually needed, same as
                    // `tool_web_search`'s own dispatch — a missing key just
                    // means `search_impl`'s Brave branch surfaces its own
                    // actionable error rather than short-circuiting here.
                    let brave_key = if settings.search_provider == web::SearchProvider::Brave {
                        web::read_brave_key().ok()
                    } else {
                        None
                    };
                    let count = args["count"].as_u64().map(|v| v as usize);
                    web::search_impl(&settings, brave_key, query, count)
                        .await
                        .and_then(|results| serde_json::to_value(results).map_err(|e| e.to_string()))
                }
                Err(e) => Err(e),
            }
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

/// True when a native target's model reports the "vision" capability via
/// `/api/show`. A failed lookup counts as no vision — the prompt then passes
/// through untouched, and the chat call right after surfaces any daemon
/// problem itself.
async fn supports_vision(client: &reqwest::Client, target: &Target) -> bool {
    let Target::Local { model, native_ollama: true, .. } = target else {
        return false;
    };
    let model = model.clone().unwrap_or_default();
    crate::ollama_api::show(client, &model)
        .await
        .map(|resp| resp.capabilities.iter().any(|c| c == "vision"))
        .unwrap_or(false)
}

/// Builds the user message, attaching any images referenced by path in the
/// prompt only when the target can actually use them — native Ollama models
/// that report the "vision" capability (a sibling base64 `images` array), or
/// OpenAI-compat targets with `--attach-images` passed (multi-part content
/// with data: URLs). Otherwise the prompt passes through verbatim: nothing
/// is stripped from the text and no file bytes are uploaded.
async fn build_user_message(
    client: &reqwest::Client,
    target: &Target,
    options: &chat::ChatOptions,
    user_text: &str,
) -> Result<serde_json::Value, String> {
    let plain = serde_json::json!({ "role": "user", "content": user_text });
    if !target.is_native() && !options.attach_images {
        return Ok(plain);
    }
    let (clean, images) = chat::extract_image_paths(user_text);
    if images.is_empty() {
        return Ok(plain);
    }
    if target.is_native() && !supports_vision(client, target).await {
        return Ok(plain);
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
///
/// Opens a per-turn checkpoint (see `checkpoints_cli.rs`) before the
/// tool-calling loop and always closes it afterward — success or error alike
/// — the same finally-equivalent shape `agentLoop.ts`'s `runTurnGuarded` uses,
/// just without a session/timeline (CLI history is in-memory only).
/// `mcp_entries` is the (possibly empty) list of MCP servers `mcp_cli::connect_all`
/// connected for this process — `&[]` when `--no-mcp` was passed or nothing
/// is configured/enabled, in which case the merged tool list is just the
/// built-ins, same as before MCP existed.
pub async fn run_turn(
    client: &reqwest::Client,
    target: &Target,
    state: &AppState,
    perms: &mut TerminalPermissions,
    history: &mut Vec<serde_json::Value>,
    options: &chat::ChatOptions,
    user_text: &str,
    mcp_entries: &[McpServerEntry],
) -> Result<(), String> {
    if let Some(system) = &options.system {
        apply_system_prompt(history, system);
    }
    history.push(build_user_message(client, target, options, user_text).await?);

    // `None` (no app-data dir resolvable, or it couldn't be created) just
    // means this turn runs without a checkpoint — same tolerance
    // `record_original`/`record_shell` already have for a missing id.
    let anchor_index = history.len() - 1;
    let label: String = user_text.chars().take(120).collect();
    let checkpoint_id = checkpoints_cli::base_dir().and_then(|base| {
        checkpoints::begin_impl(state, &base, checkpoints_cli::CLI_SESSION_ID.to_string(), anchor_index, label, None)
            .ok()
    });

    let result =
        run_tool_loop(client, target, state, perms, history, options, checkpoint_id.as_deref(), mcp_entries).await;

    if let Some(id) = &checkpoint_id {
        let _ = checkpoints::end_impl(state, id);
    }

    result
}

/// The tool-calling loop itself, factored out of `run_turn` so the
/// checkpoint's `end_impl` above can run unconditionally regardless of how
/// this returns (a model error via `?`, the safety cap, or a plain answer).
async fn run_tool_loop(
    client: &reqwest::Client,
    target: &Target,
    state: &AppState,
    perms: &mut TerminalPermissions,
    history: &mut Vec<serde_json::Value>,
    options: &chat::ChatOptions,
    checkpoint_id: Option<&str>,
    mcp_entries: &[McpServerEntry],
) -> Result<(), String> {
    // Built once per turn (mirroring `agentLoop.ts`'s two `attemptStream`
    // call sites recomputing `mcpToolDefs()` per turn, not per streaming
    // attempt within it): the connected server set doesn't change mid-turn,
    // so there's no need to re-read `state.mcp` on every iteration below.
    let (tools, mcp_registry) = tools_def::merged_tool_definitions(state, mcp_entries).await;
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
            let content =
                execute_tool_call(state, perms, &call.name, &call.arguments, checkpoint_id, mcp_entries, &mcp_registry)
                    .await;
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
