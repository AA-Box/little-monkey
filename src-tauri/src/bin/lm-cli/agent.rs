//! The agentic tool-calling loop — a Rust port of `src/lib/agentLoop.ts`'s
//! `runAgentTurn`: send the conversation (plus tool defs) to the model,
//! stream its reply to stdout, and whenever it asks for tool calls, execute
//! them through `tools_cli.rs` (permission-gated the same way the GUI's
//! Tauri commands are, just via terminal prompts instead of a modal), feed
//! the results back as `tool` messages, and repeat. Ends as soon as a turn
//! produces a plain answer with no tool calls, or after MAX_ITERATIONS round
//! trips as a safety cap against a runaway/looping model.

use std::io::Write;

use little_monkey_lib::checkpoints;
use little_monkey_lib::mcp::McpServerEntry;
use little_monkey_lib::verify::{self, VerifyResult};
use little_monkey_lib::web;
use little_monkey_lib::workspace;
use little_monkey_lib::AppState;

use crate::checkpoints_cli;
use crate::chat::{self, Target};
use crate::mcp_cli;
use crate::permission::{self, PermissionMode, TerminalPermissions};
use crate::stacks_cli;
use crate::tools_cli;
use crate::tools_def::{self, McpToolRegistry};
use crate::verify_cli;
use crate::web_cli;

const MAX_ITERATIONS: usize = 25;

/// Prefix identifying a synthetic `[Verify]` notice pushed into `history` —
/// a Rust port of `agentLoop.ts`'s `VERIFY_NOTE_PREFIX`/`formatVerifyNotice`.
/// Unlike the desktop app there's no `MessageList` to render these as a
/// pretty collapsible row; the raw `[Verify]{...json...}` text becomes part
/// of the model's own context on the next round trip exactly like the GUI
/// (the CLI's `println!`s right next to it are the human-readable side of
/// the same event, not what's stored in `history`).
const VERIFY_NOTE_PREFIX: &str = "[Verify]";

/// Cap on how many times a failed verification round feeds a fix instruction
/// back to the model within a single turn — mirrors the desktop app's
/// `verifyMaxRounds` setting (default 1, clamp 0-3; see `settingsStore.ts`).
/// Not exposed as its own CLI flag (only on/off via `--verify`/`--no-verify`):
/// there is no persisted CLI settings store for a numeric override to live
/// in, so this just takes the GUI's own default.
const DEFAULT_VERIFY_MAX_ROUNDS: u32 = 1;

/// Each verify notice's `output` field is capped at this many chars — a
/// second, wire/context-facing cap on top of `verify.rs`'s own ~20k-char cap
/// on each of stdout/stderr individually. Mirrors `agentLoop.ts`'s
/// `VERIFY_NOTICE_OUTPUT_CAP`.
const VERIFY_NOTICE_OUTPUT_CAP: usize = 8000;

/// The tool-message content returned for a `present_plan` call — a Rust port
/// of `turnEngine.ts`'s `PRESENT_PLAN_RESULT`. Deliberately a fixed literal,
/// not anything derived from the model's own arguments: it only needs to end
/// the model's turn cleanly, since the plan itself was already printed to
/// the terminal (and the approve/keep-planning decision already made) by
/// `present_plan` below before this is returned.
const PRESENT_PLAN_RESULT: &str =
    r#"{"status":"plan_presented","note":"Wait for the user to approve before doing anything else."}"#;

/// The first failed command from a [`run_verification_phase`] pass — enough
/// detail to build the feed-back-to-the-model fix instruction. Mirrors
/// `agentLoop.ts`'s `VerifyFailure`.
struct VerifyFailure {
    label: String,
    code: Option<i32>,
    output: String,
}

/// Combines a verify command's stdout/stderr into the single `output` string
/// a notice carries, tail-capping the combination (a failure's most useful
/// detail is almost always printed last) at [`VERIFY_NOTICE_OUTPUT_CAP`]
/// chars. Splits on a UTF-8 char boundary so it never panics on a truncation
/// point that lands mid-codepoint. A Rust port of `agentLoop.ts`'s
/// `buildVerifyOutput`.
fn build_verify_output(result: &VerifyResult) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if result.timed_out {
        parts.push("Command timed out.");
    }
    let stdout = result.stdout.trim();
    if !stdout.is_empty() {
        parts.push(stdout);
    }
    let stderr = result.stderr.trim();
    if !stderr.is_empty() {
        parts.push(stderr);
    }
    let combined = parts.join("\n\n");
    if combined.len() <= VERIFY_NOTICE_OUTPUT_CAP {
        return combined;
    }
    let mut start = combined.len() - VERIFY_NOTICE_OUTPUT_CAP;
    while start < combined.len() && !combined.is_char_boundary(start) {
        start += 1;
    }
    format!("… (truncated)\n{}", &combined[start..])
}

/// Whether `resultContent` (a `write_file`/`edit_file` tool result string)
/// represents success rather than the `{"error": ...}` shape a failed tool
/// call produces — a Rust port of `agentLoop.ts`'s
/// `isSuccessfulMutationResult`, used only to decide whether to add the
/// call's path to `mutated_files`.
fn is_successful_mutation_result(result_content: &str) -> bool {
    match serde_json::from_str::<serde_json::Value>(result_content) {
        Ok(serde_json::Value::Object(map)) => !map.contains_key("error"),
        Ok(_) => true,
        // Not JSON at all — the plain "Wrote N bytes to …"/"Edited …" success string.
        Err(_) => true,
    }
}

/// Extracts the `path` argument from a `write_file`/`edit_file` tool call's
/// raw arguments JSON — used only to populate `mutated_files`. A Rust port
/// of `agentLoop.ts`'s `toolCallPathArg`; never panics on malformed
/// arguments, just degrades to "no path known".
fn tool_call_path_arg(raw_arguments: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(raw_arguments).ok()?;
    parsed.get("path")?.as_str().map(str::to_string)
}

/// Runs every ENABLED verification command configured for the current
/// workspace (see `verify_cli.rs`), in order, appending one `[Verify]`
/// notice per command to `history` (so it flows to the model on the next
/// round trip exactly like every other synthetic notice) and printing a
/// human-readable pass/fail line for the terminal. Returns the first failed
/// command, if any, so [`run_tool_loop`] can decide whether to spend a
/// feed-back round — a Rust port of `agentLoop.ts`'s `runVerificationPhase`,
/// minus the `sessionId`/`runningVerifyLabel` "running…" UI bookkeeping the
/// CLI has no timeline to show (the `println!` right before each command
/// serves the same "don't look hung" purpose here). No-ops (returning
/// `None`) when the workspace root can't be resolved or nothing is enabled.
async fn run_verification_phase(state: &AppState, history: &mut Vec<serde_json::Value>) -> Option<VerifyFailure> {
    let root = workspace::primary_root_canon(state).ok()?;
    let commands = verify_cli::enabled_commands(&root);
    if commands.is_empty() {
        return None;
    }

    let mut first_failure: Option<VerifyFailure> = None;
    for cmd in &commands {
        println!("\n[verify] running \"{}\"…", cmd.label);
        let result = verify::run_command_impl(state, &root, cmd, None).await;
        let ok = !result.timed_out && result.code == Some(0);
        let output = build_verify_output(&result);
        println!(
            "[verify] {} — {} ({} ms)",
            result.label,
            if ok { "PASS" } else { "FAIL" },
            result.duration_ms
        );
        if !ok && !output.is_empty() {
            println!("{output}");
        }

        let notice = serde_json::json!({
            "label": result.label,
            "kind": result.kind,
            "ok": ok,
            "code": result.code,
            "output": output,
            "durationMs": result.duration_ms,
        });
        history.push(serde_json::json!({
            "role": "system",
            "content": format!("{VERIFY_NOTE_PREFIX}{notice}"),
        }));

        if !ok && first_failure.is_none() {
            first_failure = Some(VerifyFailure { label: result.label.clone(), code: result.code, output });
        }
    }
    first_failure
}

fn preview(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}… ({} more chars)", s.chars().count() - max)
    }
}

/// Prints the model's proposed plan to the terminal and prompts to approve
/// switching from Plan Mode to Act mode — the terminal-side counterpart of
/// the desktop app's `PlanCard` "Approve & start acting" button (see
/// `src/components/Chat/PlanCard.tsx` and `agentLoop.ts`'s `PLAN_NOTE_PREFIX`
/// notice/`lastActMode`). There is no persisted transcript notice or
/// `lastActMode` setting here — the CLI's `history` is in-memory only and has
/// no settings store to remember a preferred act mode in — so an approval
/// always switches to `PermissionMode::AcceptEdits`, the same mode the
/// desktop app's `lastActMode` itself defaults to before a user ever manually
/// picks a different one. Anything other than y/yes leaves the mode at
/// `Plan`, exactly like the GUI's "Keep planning" button.
async fn present_plan(perms: &mut TerminalPermissions, args: &serde_json::Value) {
    let title = args["title"].as_str().unwrap_or("(untitled plan)");
    let plan = args["plan"].as_str().unwrap_or_default();
    let open_questions: Vec<&str> = args["open_questions"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    println!("\n=== Plan: {title} ===\n{plan}");
    if !open_questions.is_empty() {
        println!("\nOpen questions:");
        for question in &open_questions {
            println!("  - {question}");
        }
    }

    print!("\nApprove plan and switch to act mode? [y/N]: ");
    std::io::stdout().flush().ok();
    let answer = permission::read_line_blocking().await.trim().to_lowercase();

    if answer == "y" || answer == "yes" {
        perms.set_mode(PermissionMode::AcceptEdits);
        println!("Switched to acceptEdits mode — mutating tools will now run without a plan-mode block.");
    } else {
        println!("Still in Plan Mode.");
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
    attached_stacks: &[String],
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

    // `present_plan` is a frontend/terminal-only tool (see `tools_def.rs`'s
    // `present_plan_tool_def` doc comment): it never dispatches to any
    // `tool_<name>` command, checked BEFORE the mcp__/tool_<name> dispatch
    // below just like `turnEngine.ts`'s `executeToolCall` checks it before
    // its own `invoke` dispatch. Guarded on the CURRENT mode (not just
    // whether it happened to be offered this turn) so a model that
    // hallucinates the name outside Plan Mode can't flip it — the same
    // "only offered while mode==='plan'" boundary the GUI enforces via
    // `isToolCallAllowed`, just checked here at dispatch time instead of a
    // separate offered-tools allowlist.
    if name == "present_plan" {
        return if perms.mode() != PermissionMode::Plan {
            serde_json::json!({ "error": "present_plan is only available in Plan Mode." }).to_string()
        } else {
            present_plan(perms, &args).await;
            PRESENT_PLAN_RESULT.to_string()
        };
    }

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
        // Read-only, so — like `read_file`/`grep`/`list_dir` above — never
        // goes through `perms.request`: unaffected by permission mode,
        // including Plan Mode's hard block (see `stacks.rs::tool_search_docs`'s
        // own doc comment for the full reasoning, which applies verbatim
        // here). `stacks_cli::search_docs` resolves the model's `stack` name
        // argument through the same `stacks::resolve_search_stack_ids` the
        // desktop app's Tauri command uses, over the same `stacks::query_impl`
        // ranking, so this and the GUI's `search_docs` produce identically
        // shaped results for the same stack/query.
        "search_docs" => {
            let query = args["query"].as_str().unwrap_or_default().to_string();
            let stack = args["stack"].as_str().map(str::to_string);
            let max_results = args["max_results"].as_u64().map(|v| v as u32);
            stacks_cli::search_docs(state, query, stack, max_results, attached_stacks)
                .await
                .and_then(|results| serde_json::to_value(results).map_err(|e| e.to_string()))
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
    attached_stacks: &[String],
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

    let result = run_tool_loop(
        client,
        target,
        state,
        perms,
        history,
        options,
        checkpoint_id.as_deref(),
        mcp_entries,
        attached_stacks,
    )
    .await;

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
    attached_stacks: &[String],
) -> Result<(), String> {
    // Built once per turn (mirroring `agentLoop.ts`'s two `attemptStream`
    // call sites recomputing `mcpToolDefs()` per turn, not per streaming
    // attempt within it): the connected server set doesn't change mid-turn,
    // so there's no need to re-read `state.mcp` on every iteration below.
    let (tools, mcp_registry) = tools_def::merged_tool_definitions(state, mcp_entries).await;
    let mut tools_vec: Vec<serde_json::Value> = tools.as_array().cloned().unwrap_or_default();
    // `present_plan` is offered only in Plan Mode — read once per turn (like
    // `agentLoop.ts`'s own `toolsForTurn(mode)` snapshot), not re-checked on
    // every iteration below, so an approval mid-turn simply leaves it listed
    // (but dispatch-blocked, see `execute_tool_call`) for the rest of this
    // turn rather than disappearing out from under an in-flight model reply.
    if perms.mode() == PermissionMode::Plan {
        tools_vec.push(tools_def::present_plan_tool_def());
    }
    // `search_docs` is offered only when at least one `--stack` was given —
    // mirrors the desktop app's `buildTools(attachedStackNames)`, which only
    // offers the tool when a stack is actually attached to the session (see
    // `tools_def::search_docs_tool_def`'s doc comment).
    if !attached_stacks.is_empty() {
        tools_vec.push(tools_def::search_docs_tool_def(attached_stacks));
    }
    let native = target.is_native();

    // Absolute (or workspace-relative, as given by the model) paths this
    // turn's `write_file`/`edit_file` calls have successfully mutated so far,
    // across every tool-calling round trip below — read by
    // `run_verification_phase` at the loop's natural exit to decide whether
    // there's anything worth verifying. Mirrors `agentLoop.ts`'s
    // `mutatedFiles`.
    let mut mutated_files: std::collections::HashSet<String> = std::collections::HashSet::new();

    // How many verification feed-back rounds have been consumed so far this
    // turn — bounded by `DEFAULT_VERIFY_MAX_ROUNDS`, mirroring
    // `agentLoop.ts`'s `verifyRound`/`settings.verifyMaxRounds`.
    let mut verify_round: u32 = 0;

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

            // The model gave a plain answer with no further tool requests —
            // this turn's natural exit point. Run the workspace's configured
            // verification commands (if `--verify` is on and any files were
            // mutated) before returning, exactly like `agentLoop.ts`'s
            // `runAgentTurnBody` does at its own `toolCalls.length === 0`
            // exit.
            if options.verify && !mutated_files.is_empty() {
                if let Some(failure) = run_verification_phase(state, history).await {
                    if verify_round < DEFAULT_VERIFY_MAX_ROUNDS {
                        verify_round += 1;
                        // Cleared so only edits made in response to *this*
                        // failure trigger the next verification pass.
                        mutated_files.clear();
                        let code_display = failure.code.map(|c| c.to_string()).unwrap_or_else(|| "timeout".to_string());
                        let message = format!(
                            "{VERIFY_NOTE_PREFIX} The verification command \"{}\" failed (exit {code_display}). Fix the reported problems, then stop.\n{}",
                            failure.label, failure.output
                        );
                        println!("\n{message}");
                        history.push(serde_json::json!({ "role": "system", "content": message }));
                        continue;
                    }
                }
            }

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
            let content = execute_tool_call(
                state,
                perms,
                &call.name,
                &call.arguments,
                checkpoint_id,
                mcp_entries,
                &mcp_registry,
                attached_stacks,
            )
            .await;
            println!("[tool result] {}", preview(&content, 300));

            // Track this turn's file mutations for `run_verification_phase`
            // at the loop's eventual exit — only for calls that actually
            // succeeded (the "Wrote…"/"Edited…" string shape, not
            // `{"error": ...}`). Mirrors `agentLoop.ts`'s equivalent check
            // right after `executeToolCall`.
            if (call.name == "write_file" || call.name == "edit_file") && is_successful_mutation_result(&content) {
                if let Some(path) = tool_call_path_arg(&call.arguments) {
                    mutated_files.insert(path);
                }
            }

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

#[cfg(test)]
mod tests {
    use super::*;

    fn result(stdout: &str, stderr: &str, timed_out: bool) -> VerifyResult {
        VerifyResult {
            command_id: "c1".to_string(),
            label: "lint".to_string(),
            kind: "lint".to_string(),
            code: Some(1),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            duration_ms: 42,
            timed_out,
        }
    }

    /// `present_plan` must refuse to run (and, crucially, never touch stdin)
    /// outside Plan Mode — otherwise a model that hallucinates the tool name
    /// in, say, `"auto"` mode could pop an unexpected "switch to act mode?"
    /// prompt. Every other mode used here is asserted, not just `"manual"`,
    /// so a future mode addition can't silently widen the guard by accident.
    #[tokio::test]
    async fn present_plan_is_rejected_outside_plan_mode() {
        let state = AppState::default();
        let registry = McpToolRegistry(std::collections::HashMap::new());
        let args = r#"{"title":"t","plan":"p"}"#;

        for mode in [
            PermissionMode::Manual,
            PermissionMode::AcceptEdits,
            PermissionMode::Smart,
            PermissionMode::Auto,
            PermissionMode::Bypass,
        ] {
            let mut perms = TerminalPermissions::new(mode);
            let content =
                execute_tool_call(&state, &mut perms, "present_plan", args, None, &[], &registry, &[]).await;
            let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
            assert!(parsed["error"].as_str().unwrap().contains("Plan Mode"), "mode {mode:?} should reject present_plan");
            // The guard must reject BEFORE flipping anything — mode stays
            // exactly what it was.
            assert_eq!(perms.mode(), mode);
        }
    }

    #[test]
    fn is_successful_mutation_result_accepts_plain_success_strings() {
        assert!(is_successful_mutation_result("Wrote 12 bytes to foo.txt"));
        assert!(is_successful_mutation_result("Edited foo.txt"));
    }

    #[test]
    fn is_successful_mutation_result_rejects_error_json() {
        assert!(!is_successful_mutation_result(r#"{"error":"not a file"}"#));
    }

    #[test]
    fn is_successful_mutation_result_accepts_non_error_json() {
        // Not the actual write_file/edit_file shape, but structurally
        // confirms only the presence of an "error" key matters.
        assert!(is_successful_mutation_result(r#"{"ok":true}"#));
    }

    #[test]
    fn tool_call_path_arg_extracts_the_path_field() {
        assert_eq!(tool_call_path_arg(r#"{"path":"src/lib.rs","content":"x"}"#), Some("src/lib.rs".to_string()));
    }

    #[test]
    fn tool_call_path_arg_is_none_for_malformed_or_missing_path() {
        assert_eq!(tool_call_path_arg("not json"), None);
        assert_eq!(tool_call_path_arg(r#"{"content":"x"}"#), None);
    }

    #[test]
    fn build_verify_output_combines_stdout_and_stderr() {
        let output = build_verify_output(&result("out line", "err line", false));
        assert!(output.contains("out line"));
        assert!(output.contains("err line"));
    }

    #[test]
    fn build_verify_output_notes_timeout() {
        let output = build_verify_output(&result("", "", true));
        assert_eq!(output, "Command timed out.");
    }

    #[test]
    fn build_verify_output_caps_and_keeps_the_tail() {
        let long_stdout = "y".repeat(VERIFY_NOTICE_OUTPUT_CAP + 500);
        let output = build_verify_output(&result(&long_stdout, "", false));
        assert!(output.starts_with("… (truncated)"));
        assert!(output.ends_with('y'));
        assert!(output.len() < long_stdout.len());
    }
}
