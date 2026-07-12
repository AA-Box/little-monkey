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

/// Prefix of the message pushed to history (and printed) when the
/// tool-calling loop hits its iteration cap without a final answer — the
/// loop still returns `Ok(())` in that case (an ordinary, if incomplete,
/// chat turn), so `task.rs`'s `task run` checks the last history entry
/// against this prefix to tell "hit the cap" apart from "answered normally"
/// for its own exit-code discipline (design doc slice 1: exit 3).
pub(crate) const ITERATION_CAP_MESSAGE_PREFIX: &str = "Stopped after reaching the safety limit of";

/// Cap on how many model/tool round trips a `task`-delegated subagent (see
/// [`run_subagent_turn`]) may take before it's forced to stop and report
/// whatever it has — a Rust port of `subagent.ts`'s `MAX_SUBAGENT_ITERATIONS`
/// (docs/roadmap/p3-subagents.md). Smaller than [`MAX_ITERATIONS`] because a
/// subagent's job is one scoped, disjoint task, not an open-ended session.
const MAX_SUBAGENT_ITERATIONS: usize = 15;

/// Cap on a subagent's final report returned as the `task` tool's result —
/// mirrors the design doc's "Context/report bloat" risk (and
/// `subagent.ts`'s own cap): one chatty child's report shouldn't be able to
/// blow up the parent turn's own context on its own.
const SUBAGENT_REPORT_CHAR_CAP: usize = 8000;

/// Truncates `report` to [`SUBAGENT_REPORT_CHAR_CAP`] chars, keeping the head
/// (a subagent's report usually leads with its conclusion) and appending a
/// marker noting how much was cut, rather than silently dropping the tail.
fn truncate_report(report: &str) -> String {
    if report.chars().count() <= SUBAGENT_REPORT_CHAR_CAP {
        return report.to_string();
    }
    let truncated: String = report.chars().take(SUBAGENT_REPORT_CHAR_CAP).collect();
    format!("{truncated}\n… (truncated, {} more chars)", report.chars().count() - SUBAGENT_REPORT_CHAR_CAP)
}

/// The `task` tool's allowed profile on the CLI — always `"explore"` (see
/// [`execute_tool_call`]'s `"task"` arm doc comment for why `"code"` is
/// rejected here rather than supported).
const CLI_SUBAGENT_PROFILE: &str = "explore";

/// The exact tool names offered to a `task` subagent's own tool-calling loop
/// on the CLI — the read-only subset of [`tool_definitions`]'s base list.
/// Deliberately an ALLOWLIST filtered out of the same list every top-level
/// turn uses (not a denylist over some larger set), so there is no way for a
/// gated or mutating tool — or `task` itself — to end up in it by omission.
/// `tool_definitions()` never includes `"task"` (it's a per-turn-only
/// addition the desktop app's `toolsForSettings` makes; here it's handled
/// directly in [`execute_tool_call`], never added to any list at all), so
/// this filter structurally cannot let it through either.
///
/// Note this is one tool short of the desktop app's `explore` profile
/// (`read_file`/`list_dir`/`glob`/`grep`): `tool_definitions()` has no `glob`
/// tool at all yet (a pre-existing CLI/GUI parity gap, not introduced here),
/// so the CLI subagent's explore set is `read_file`/`list_dir`/`grep`.
const EXPLORE_TOOL_NAMES: [&str; 3] = ["read_file", "list_dir", "grep"];

/// Builds the subagent's own per-turn tool list: [`tool_definitions`]'s base
/// array filtered down to [`EXPLORE_TOOL_NAMES`]. This is the ONLY tool list
/// ever handed to a subagent's model calls in [`run_subagent_turn`] — the
/// depth-1 cap on delegation (a subagent can never spawn another subagent)
/// falls directly out of `"task"` never appearing in this allowlist, with no
/// separate runtime recursion guard needed. See `tests::` below for the
/// construction proof.
fn explore_tool_definitions() -> Vec<serde_json::Value> {
    tools_def::tool_definitions()
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|def| {
            let name = def["function"]["name"].as_str().unwrap_or_default();
            EXPLORE_TOOL_NAMES.contains(&name)
        })
        .collect()
}

/// Executes one tool call from inside a `task` subagent's own loop —
/// deliberately a SEPARATE, much smaller dispatcher from the top-level
/// [`execute_tool_call`] rather than a recursive call into it: it implements
/// only the three [`EXPLORE_TOOL_NAMES`] arms, so `write_file`/`edit_file`/
/// `run_shell`/`task` (and anything else) fall straight to the `other` arm's
/// "Unknown tool" error no matter what a model hallucinates into the
/// `function.name` field — the shell/write path, and further delegation, are
/// categorically unreachable here by construction, not by a mode/permission
/// check that could have an exception carved into it later. None of
/// `EXPLORE_TOOL_NAMES`'s tools are permission-gated (see `tools_cli.rs`'s
/// `read_file`/`list_dir`/`grep`, which take no `TerminalPermissions` at
/// all), so this needs no `perms` parameter and never prompts.
async fn execute_subagent_tool_call(state: &AppState, name: &str, raw_arguments: &str) -> String {
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
        other => Err(format!(
            "Unknown tool \"{other}\" — this subagent only has read-only explore tools (read_file, list_dir, grep) and cannot spawn further subagents."
        )),
    };

    match result {
        Ok(serde_json::Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(err) => serde_json::json!({ "error": err }).to_string(),
    }
}

/// Runs one `task`-delegated subagent to completion and returns its final
/// report as the string to use for the parent's `task` tool-call result —
/// mirrors `subagent.ts`'s `runSubagentTask`, minus the GUI's live
/// `subagentStore` status (there is no timeline here; the `println!`s below
/// are the terminal's equivalent "don't look hung" signal) and minus any
/// checkpoint/turn_id threading (explore-only tools never mutate, so there is
/// nothing to checkpoint or cancel-scope).
///
/// Seeds a brand-new, local message history — never touching the parent
/// turn's `history` — with a subagent system prompt plus `prompt` as the user
/// message, then loops model→tools→model up to [`MAX_SUBAGENT_ITERATIONS`]
/// times using [`explore_tool_definitions`] and [`execute_subagent_tool_call`]
/// exclusively. Never propagates an error out to the caller: a model-call
/// failure or the iteration cap becomes a `{"error": ...}` string result
/// instead, exactly like every other tool result — so a subagent that stalls
/// or the provider that errors out still yields a well-formed tool message
/// for the parent's history (the transcript-validity invariant: every
/// `task` tool_call must get a matching tool result).
async fn run_subagent_turn(
    client: &reqwest::Client,
    target: &Target,
    options: &chat::ChatOptions,
    state: &AppState,
    description: &str,
    prompt: &str,
) -> String {
    let system = format!(
        "You are a subagent completing one scoped task: \"{description}\". You have read-only tools only \
         (read_file, list_dir, grep) — you cannot write, edit, run shell commands, or delegate to a further \
         subagent. Investigate, then reply with a final report; your reply is returned to the coordinating \
         agent, not shown directly to the user. Do not ask questions — if blocked, report what you found and \
         why you stopped."
    );
    let mut history = vec![
        serde_json::json!({ "role": "system", "content": system }),
        serde_json::json!({ "role": "user", "content": prompt }),
    ];
    let tools_vec = explore_tool_definitions();
    let native = target.is_native();

    for _ in 0..MAX_SUBAGENT_ITERATIONS {
        let result = match chat::stream_turn(client, target, history.as_slice(), &tools_vec, options).await {
            Ok(r) => r,
            Err(e) => return serde_json::json!({ "error": e }).to_string(),
        };
        println!();

        let mut assistant_message = serde_json::json!({ "role": "assistant", "content": result.content });

        if result.tool_calls.is_empty() {
            return truncate_report(&result.content);
        }

        assistant_message["tool_calls"] = serde_json::json!(result
            .tool_calls
            .iter()
            .map(|c| if native {
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
            println!("\n[subagent tool] {}({})", call.name, call.arguments);
            let content = execute_subagent_tool_call(state, &call.name, &call.arguments).await;
            println!("[subagent tool result] {}", preview(&content, 300));

            history.push(if native {
                serde_json::json!({ "role": "tool", "tool_name": call.name, "content": content })
            } else {
                serde_json::json!({ "role": "tool", "tool_call_id": call.id, "content": content })
            });
        }
    }

    serde_json::json!({
        "error": format!("Subagent \"{description}\" exceeded {MAX_SUBAGENT_ITERATIONS} tool-calling iterations without a final answer.")
    })
    .to_string()
}

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
    client: &reqwest::Client,
    target: &Target,
    options: &chat::ChatOptions,
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

    // `task` delegates a scoped subtask to a subagent with its own isolated
    // tool-calling loop (see `run_subagent_turn`/`explore_tool_definitions`/
    // `execute_subagent_tool_call` above) — a Rust port of `turnEngine.ts`'s
    // `executeToolCall` `"task"` branch, checked before the `mcp__`/built-in
    // dispatch below exactly like `present_plan` is. CLI parity is
    // deliberately EXPLORE-ONLY (slice 5 of docs/roadmap/p3-subagents.md):
    // the desktop app's `code` profile relies on injecting the *parent's*
    // checkpoint_id into the child's mutating tool calls so subagent writes
    // land in the parent turn's revertable checkpoint manifest, but the CLI
    // has no checkpoints at all (`checkpoints_cli`'s begin/end pair exists
    // only for the top-level turn in `run_turn`) — there is nothing safe to
    // revert a subagent's shell command or file write into here, so `"code"`
    // is rejected outright rather than silently downgraded or run unguarded.
    // Depth is capped at 1 by construction: `run_subagent_turn` only ever
    // offers `explore_tool_definitions()` to the child model and only ever
    // dispatches through `execute_subagent_tool_call`, which has no `"task"`
    // arm at all — there is no code path from inside a subagent back into
    // this function, so a subagent spawning another subagent is
    // structurally unreachable, not merely guarded by a runtime counter.
    if name == "task" {
        // Re-checked here, at dispatch time — same defense-in-depth posture
        // as `present_plan`'s `perms.mode()` re-check just above: `task` is
        // only appended to `tools_vec` when `--subagents` was given (see
        // `run_tool_loop`), but `tools_vec` only shapes what's *offered* to
        // the model, not what actually gets dispatched here. Without this
        // check, a model that hallucinates a `task` call outside
        // `--subagents` (the exact "weak local model may misuse the task
        // tool" risk this flag ships opt-in specifically to guard against —
        // see docs/roadmap/p3-subagents.md) would still have it executed,
        // spinning up a full extra model-calling loop the operator never
        // opted into.
        if !options.subagents {
            return serde_json::json!({
                "error": "The task tool is not enabled for this session — pass --subagents to allow subagent delegation."
            })
            .to_string();
        }
        let description = args["description"].as_str().unwrap_or("(untitled subtask)").to_string();
        let prompt = args["prompt"].as_str().unwrap_or_default().to_string();
        let profile = args["profile"].as_str().unwrap_or(CLI_SUBAGENT_PROFILE);
        if profile != CLI_SUBAGENT_PROFILE {
            return serde_json::json!({
                "error": format!(
                    "monkey-cli only supports the 'explore' subagent profile (got '{profile}') — there are no \
                     checkpoints here to safely land a 'code'-profile subagent's mutations into; see \
                     docs/roadmap/p3-subagents.md slice 5."
                )
            })
            .to_string();
        }
        println!("\n[subagent: {description}] starting…");
        let report = run_subagent_turn(client, target, options, state, &description, &prompt).await;
        println!("[subagent: {description}] done.");
        return report;
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
) -> Result<Vec<String>, String> {
    run_turn_with_max_iterations(client, target, state, perms, history, options, user_text, mcp_entries, attached_stacks, None)
        .await
}

/// Same as [`run_turn`], but lets a caller cap the tool-calling loop below
/// (or, in principle, above) the default [`MAX_ITERATIONS`] — `monkey-cli
/// task run` uses this to honor a recipe's own `max_iterations` field
/// (design doc slice 1). `None` behaves exactly like plain `run_turn`.
/// Returns the checkpoint's recorded files-changed list (empty when no
/// checkpoint was recorded, e.g. the app-data dir couldn't be resolved) —
/// `task run --json`'s `files_changed` field (existing callers that only
/// check `if let Err(e) = ...` are unaffected by this Ok payload).
#[allow(clippy::too_many_arguments)]
pub async fn run_turn_with_max_iterations(
    client: &reqwest::Client,
    target: &Target,
    state: &AppState,
    perms: &mut TerminalPermissions,
    history: &mut Vec<serde_json::Value>,
    options: &chat::ChatOptions,
    user_text: &str,
    mcp_entries: &[McpServerEntry],
    attached_stacks: &[String],
    max_iterations_override: Option<usize>,
) -> Result<Vec<String>, String> {
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
        max_iterations_override,
    )
    .await;

    let files_changed = checkpoint_id.as_deref().and_then(|id| checkpoints::end_impl(state, id).ok()).map(|summary| summary.files).unwrap_or_default();

    result.map(|()| files_changed)
}

/// The tool-calling loop itself, factored out of `run_turn` so the
/// checkpoint's `end_impl` above can run unconditionally regardless of how
/// this returns (a model error via `?`, the safety cap, or a plain answer).
#[allow(clippy::too_many_arguments)]
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
    max_iterations_override: Option<usize>,
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
    // `task` is offered only when `--subagents` was given — mirrors the
    // desktop app's `subagentsEnabled` toggle (default off) and the
    // `present_plan`/`search_docs` per-turn-conditional pattern just above.
    // See `execute_tool_call`'s `"task"` arm for the dispatch and depth cap.
    if options.subagents {
        tools_vec.push(tools_def::task_tool_def());
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

    let max_iterations = max_iterations_override.unwrap_or(MAX_ITERATIONS);
    for _ in 0..max_iterations {
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
                client,
                target,
                options,
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

    let message =
        format!("{ITERATION_CAP_MESSAGE_PREFIX} {max_iterations} tool-calling iterations without a final answer.");
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
    /// A `Target`/`ChatOptions`/`reqwest::Client` fixture for `execute_tool_call`
    /// tests below that never actually need to reach the network (every case
    /// here is rejected, or dispatched, before any model call would happen).
    fn dummy_target_and_options() -> (reqwest::Client, Target, chat::ChatOptions) {
        let target = Target::Local { base_url: "http://127.0.0.1:0".to_string(), model: None, native_ollama: false };
        (reqwest::Client::new(), target, chat::ChatOptions::default())
    }

    #[tokio::test]
    async fn present_plan_is_rejected_outside_plan_mode() {
        let state = AppState::default();
        let registry = McpToolRegistry(std::collections::HashMap::new());
        let args = r#"{"title":"t","plan":"p"}"#;
        let (client, target, options) = dummy_target_and_options();

        for mode in [
            PermissionMode::Manual,
            PermissionMode::AcceptEdits,
            PermissionMode::Smart,
            PermissionMode::Auto,
            PermissionMode::Bypass,
        ] {
            let mut perms = TerminalPermissions::new(mode);
            let content = execute_tool_call(
                &client, &target, &options, &state, &mut perms, "present_plan", args, None, &[], &registry, &[],
            )
            .await;
            let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
            assert!(parsed["error"].as_str().unwrap().contains("Plan Mode"), "mode {mode:?} should reject present_plan");
            // The guard must reject BEFORE flipping anything — mode stays
            // exactly what it was.
            assert_eq!(perms.mode(), mode);
        }
    }

    /// A `task` call requesting the `"code"` profile must be rejected outright
    /// (not silently downgraded to `"explore"`) — the CLI has no checkpoints
    /// to safely land a mutating subagent's writes into (design doc slice 5).
    /// Asserted across every permission mode, including `bypass`: this is a
    /// profile-support rejection, not a permission decision, so no mode
    /// should ever let it through.
    #[tokio::test]
    async fn task_rejects_code_profile_in_every_mode() {
        let state = AppState::default();
        let registry = McpToolRegistry(std::collections::HashMap::new());
        let args = r#"{"description":"d","prompt":"p","profile":"code"}"#;
        let (client, target, mut options) = dummy_target_and_options();
        // This test exercises the profile-rejection branch specifically, so
        // `--subagents` must be on — otherwise the new `options.subagents`
        // gate (see `task_is_rejected_without_subagents_enabled` below)
        // would reject the call first, for an unrelated reason.
        options.subagents = true;

        for mode in [PermissionMode::Manual, PermissionMode::Bypass, PermissionMode::Auto] {
            let mut perms = TerminalPermissions::new(mode);
            let content =
                execute_tool_call(&client, &target, &options, &state, &mut perms, "task", args, None, &[], &registry, &[])
                    .await;
            let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
            assert!(
                parsed["error"].as_str().unwrap().contains("explore"),
                "mode {mode:?} should reject the 'code' subagent profile"
            );
        }
    }

    /// `task` must be rejected at DISPATCH time when `--subagents` was never
    /// given — mirrors `present_plan_is_rejected_outside_plan_mode`'s
    /// re-check-at-dispatch posture: `tools_vec` only controls what's
    /// *offered* to the model (see `run_tool_loop`'s `if options.subagents`
    /// gate), so a model that hallucinates a `task` call anyway (the exact
    /// "weak local model may misuse the task tool" risk `--subagents` ships
    /// opt-in to guard against) must not have it dispatched to
    /// `run_subagent_turn`. `dummy_target_and_options()`'s `ChatOptions::
    /// default()` already has `subagents: false`, so this exercises the
    /// default, no-flag case explicitly.
    #[tokio::test]
    async fn task_is_rejected_without_subagents_enabled() {
        let state = AppState::default();
        let registry = McpToolRegistry(std::collections::HashMap::new());
        let args = r#"{"description":"d","prompt":"p","profile":"explore"}"#;
        let (client, target, options) = dummy_target_and_options();
        assert!(!options.subagents, "this test relies on the default being off");

        let mut perms = TerminalPermissions::new(PermissionMode::Bypass);
        let content =
            execute_tool_call(&client, &target, &options, &state, &mut perms, "task", args, None, &[], &registry, &[])
                .await;

        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(
            parsed["error"].as_str().unwrap().contains("--subagents"),
            "expected a subagents-disabled error, got: {content}"
        );
    }

    /// The depth-1 cap on subagent delegation: `explore_tool_definitions()`
    /// (the ONLY tool list ever handed to a `task` subagent's own model
    /// calls) must never include `"task"` itself — verified by construction,
    /// not merely by a runtime counter, per the design doc's invariant that a
    /// subagent can never spawn another subagent.
    #[test]
    fn explore_tool_definitions_never_includes_task_or_mutating_tools() {
        let defs = explore_tool_definitions();
        let names: Vec<&str> = defs.iter().map(|d| d["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["read_file", "list_dir", "grep"]);
        assert!(!names.contains(&"task"));
        assert!(!names.contains(&"write_file"));
        assert!(!names.contains(&"edit_file"));
        assert!(!names.contains(&"run_shell"));
    }

    /// The shell/write path (and further delegation) must be categorically
    /// unreachable from inside a subagent's own tool dispatch — not merely
    /// absent from the tool list offered to the model, since a model can
    /// hallucinate a function name it was never offered. Exercises every
    /// dangerous name directly against `execute_subagent_tool_call`, which
    /// takes no `TerminalPermissions` at all (nothing here should ever need
    /// to prompt), confirming each falls to the "Unknown tool" arm rather
    /// than being dispatched.
    #[tokio::test]
    async fn subagent_dispatch_cannot_reach_write_shell_or_task() {
        let state = AppState::default();
        for (name, args) in [
            ("write_file", r#"{"path":"x","content":"y"}"#),
            ("edit_file", r#"{"path":"x","old_string":"a","new_string":"b"}"#),
            ("run_shell", r#"{"command":"echo hi"}"#),
            ("task", r#"{"description":"d","prompt":"p","profile":"explore"}"#),
            ("remember", r#"{"text":"x"}"#),
        ] {
            let content = execute_subagent_tool_call(&state, name, args).await;
            let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
            assert!(
                parsed["error"].as_str().unwrap().contains("Unknown tool"),
                "{name} should be unreachable from a subagent's own dispatch"
            );
        }
    }

    /// A subagent's final plain-text answer (no further tool calls) becomes
    /// the `task` tool's result verbatim when under the report cap — no
    /// wrapping, no truncation marker.
    #[test]
    fn truncate_report_passes_short_reports_through_unchanged() {
        assert_eq!(truncate_report("short report"), "short report");
    }

    /// A report over `SUBAGENT_REPORT_CHAR_CAP` chars is truncated with a
    /// marker noting how much was cut, rather than silently dropped or
    /// returned in full (the "context/report bloat" risk the design doc
    /// calls out) — mirrors `build_verify_output_caps_and_keeps_the_tail`'s
    /// shape above.
    #[test]
    fn truncate_report_caps_long_reports_with_a_marker() {
        let long = "z".repeat(SUBAGENT_REPORT_CHAR_CAP + 200);
        let truncated = truncate_report(&long);
        assert!(truncated.len() < long.len());
        assert!(truncated.contains("truncated"));
        assert!(truncated.starts_with('z'));
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
