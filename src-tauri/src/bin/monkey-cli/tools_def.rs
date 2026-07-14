//! OpenAI-style tool (function) definitions handed to the model, mirroring
//! `src/lib/tools.ts` 1:1 (minus the multi-root "label/" prefix wording —
//! the CLI supports a single `--workspace` root, so there's nothing to
//! disambiguate).

use std::collections::{HashMap, HashSet};

use little_monkey_lib::mcp::{self, McpServerEntry};
use little_monkey_lib::AppState;

pub fn tool_definitions() -> serde_json::Value {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read the full text contents of a file in the workspace. Path is resolved relative to the workspace root and must not escape it.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the file, relative to the workspace root." }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Create or overwrite a file in the workspace with the given content, creating parent directories as needed. Requires user permission.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the file, relative to the workspace root." },
                        "content": { "type": "string", "description": "The full new contents of the file." }
                    },
                    "required": ["path", "content"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "edit_file",
                "description": "Replace a single unique occurrence of old_string with new_string in an existing file. Fails if old_string is not found or is not unique. Requires user permission.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the file, relative to the workspace root." },
                        "old_string": { "type": "string", "description": "The exact, unique text to find in the file." },
                        "new_string": { "type": "string", "description": "The text to replace old_string with." }
                    },
                    "required": ["path", "old_string", "new_string"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "List the entries of a directory in the workspace, returning each entry's name, whether it is a directory, and its size in bytes.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the directory, relative to the workspace root." }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "glob",
                "description": "Find files by glob pattern, skipping VCS, dependency, and build directories. Results are workspace-relative, most-recently-modified first, and capped at 300.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Glob pattern such as **/*.rs or src/**/test_*.py." },
                        "path": { "type": "string", "description": "Optional workspace-relative directory to search. Defaults to the whole workspace." }
                    },
                    "required": ["pattern"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "grep",
                "description": "Search for a regular expression pattern across files in the workspace (skipping .git, node_modules, target, and dist), returning matching file, line number, and line text, capped at 200 matches.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Regular expression pattern to search for." },
                        "path": { "type": "string", "description": "Optional directory or file to scope the search to, relative to the workspace root. Defaults to the whole workspace." }
                    },
                    "required": ["pattern"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "run_shell",
                "description": "Run a shell command via `sh -c` in the workspace (or a subdirectory of it), with a 120 second timeout. Returns stdout, stderr, and exit code. Requires user permission.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The shell command to execute." },
                        "cwd": { "type": "string", "description": "Optional working directory, relative to the workspace root. Defaults to the workspace root." }
                    },
                    "required": ["command"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "remember",
                "description": "Save a short durable fact about this project or the user's preferences so future conversations remember it. Use for stated preferences, project conventions, and hard-won discoveries (build commands, gotchas). Requires user permission.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "The fact to remember, written as a short standalone statement (max 500 characters)." }
                    },
                    "required": ["text"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "web_fetch",
                "description": "Fetch a web page (or plain text/Markdown/JSON/XML document) by URL and return its content as Markdown, with the page title and final URL (after redirects). Long content is windowed to max_chars starting at start_index; the result reports total_chars and truncated so you can page through the rest with a later call. Requires user permission.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "The http(s) URL to fetch." },
                        "max_chars": { "type": "integer", "description": "Maximum characters of content to return in this call (default 20000)." },
                        "start_index": { "type": "integer", "description": "Character offset into the full content to start the returned window at (default 0). Use with total_chars/truncated from a previous call to page through a long page." }
                    },
                    "required": ["url"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Search the web (keyless DuckDuckGo by default) and return up to `count` ranked results, each with a title, url, and snippet. Follow up with web_fetch to read a result in full. Requires user permission.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "The search query." },
                        "count": { "type": "integer", "description": "Number of results to return, 1-10 (default 10)." }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }
            }
        }
    ])
}

/// The Plan Mode "present a plan" tool — a Rust port of `src/lib/tools.ts`'s
/// `PRESENT_PLAN_TOOL`. Deliberately excluded from [`tool_definitions`]'s
/// base array (and from [`merged_tool_definitions`]'s output): it is only
/// appended to the per-turn tool list by `agent.rs::run_tool_loop`, and only
/// while the active `PermissionMode` is `Plan` — the same offer-only-in-
/// plan-mode rule as the desktop app's `toolsForMode`. Like its TS
/// counterpart, this has NO Rust `tool_present_plan` command anywhere:
/// `agent.rs`'s `execute_tool_call` handles the name directly (printing the
/// plan to the terminal and prompting to switch to Act mode) rather than
/// dispatching into the `tool_<name>` Tauri-command world this binary
/// doesn't have. An intentional three-way-registry-drift exception —
/// `src/lib/tools.ts`'s doc comment calls out the identical exception for
/// the desktop app; a reader scanning this file's `tool_definitions()` for a
/// `present_plan` Rust command elsewhere and finding nothing should look at
/// `agent.rs` instead of assuming a missing command.
pub fn present_plan_tool_def() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "present_plan",
            "description": "Present a structured plan to the user for approval. Call this exactly once, after investigating with read-only tools, then stop and wait — do not call it more than once per turn, and do not attempt to make changes until the user approves.",
            "parameters": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "A short (few words) title summarizing the plan." },
                    "plan": { "type": "string", "description": "The full plan, written as Markdown, describing the changes you intend to make and why." },
                    "open_questions": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of questions to ask the user before proceeding, if anything is ambiguous."
                    }
                },
                "required": ["title", "plan"],
                "additionalProperties": false
            }
        }
    })
}

/// The agent's read-only knowledge-stack retrieval tool (RAG design doc
/// slice 4, `monkey-cli` parity) — a Rust port of `src/lib/tools.ts`'s
/// `search_docs` `ToolDef`. Like [`present_plan_tool_def`] above,
/// deliberately excluded from [`tool_definitions`]'s base array (and from
/// [`merged_tool_definitions`]'s output): `agent.rs::run_tool_loop` only
/// appends it when at least one `--stack <name>` was given on the command
/// line, mirroring the desktop app's `buildTools(attachedStackNames)` (the
/// tool is only offered once a stack is actually attached). `stack_names`
/// is embedded directly into the description — same "the model sees exactly
/// what's searchable" property the GUI's per-turn tool list has.
pub fn search_docs_tool_def(stack_names: &[String]) -> serde_json::Value {
    let names = stack_names.join(", ");
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "search_docs",
            "description": format!(
                "Search the attached knowledge stack(s) ({names}) for passages relevant to a query. Returns the top matching chunks, each with its source file path and a relevance score. Cite source paths when you use a result."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The natural-language search query." },
                    "stack": { "type": "string", "description": "Optional stack name to restrict the search to; defaults to searching every attached stack." },
                    "max_results": { "type": "integer", "description": "Maximum number of results to return (default 6)." }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }
    })
}

/// The `task` tool — delegates a scoped subtask to a subagent with its own
/// isolated tool-calling loop and explicit explore/code tool profiles — a Rust port of
/// `src/lib/tools.ts`'s `TASK_TOOL`. Like [`present_plan_tool_def`] and
/// [`search_docs_tool_def`], deliberately excluded from [`tool_definitions`]'s
/// base array (and from [`merged_tool_definitions`]'s output): `agent.rs`'s
/// `run_tool_loop` only appends it when `--subagents` was passed, mirroring
/// the desktop app's `subagentsEnabled` toggle (default off — a weak local
/// model that never had this turned on should never even see the schema).
/// `agent.rs::execute_tool_call` intercepts the name before the built-in
/// dispatch, exactly like `present_plan`; there is no `tool_task` Rust
/// command. Code-profile mutations reuse the parent turn checkpoint and the
/// same permission object; neither profile includes recursive delegation.
pub fn task_tool_def() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "task",
            "description": "Delegate a scoped subtask to a subagent with its own isolated tool-calling loop and restricted tool set. The subagent cannot see this conversation, so give it a fully self-contained prompt. Only its final report is returned to you — use this for broad exploration or an independent subtask you want kept out of your own context.",
            "parameters": {
                "type": "object",
                "properties": {
                    "description": { "type": "string", "description": "A short (3-6 word) label for this subtask, shown to the user." },
                    "prompt": { "type": "string", "description": "Full, self-contained instructions for the subagent — it has no access to this conversation, so include all necessary context (file paths, what to look for, what to report back)." },
                    "profile": {
                        "type": "string",
                        "enum": ["explore", "code"],
                        "description": "Tool access profile. 'explore' is read-only (read_file, list_dir, glob, grep). 'code' adds write_file, edit_file, and run_shell; mutations use the parent turn checkpoint and normal permission gate."
                    }
                },
                "required": ["description", "prompt", "profile"],
                "additionalProperties": false
            }
        }
    })
}

/// Composite `mcp__<serverId>__<toolName>` tool name mapped to the exact
/// server id and tool name it was built from, returned by
/// [`merged_tool_definitions`] alongside the tool defs themselves. Mirrors
/// `src/lib/mcpTools.ts`'s `registry` side table: the composite name isn't
/// reliably reversible by splitting on `__` (sanitization and
/// collision-suffixing can both eat the original substring), so
/// `agent.rs`'s dispatch branch looks the exact pair up here instead of
/// re-parsing the string.
pub struct McpToolRegistry(pub HashMap<String, (String, String)>);

/// Sanitizes one segment of the composite tool name to
/// `^[a-zA-Z0-9_-]+$` — mirrors `src/lib/mcpTools.ts::sanitizeSegment`
/// exactly (same replacement character, same non-empty fallback), so tool
/// naming is identical between the desktop app and the CLI given the same
/// server config.
fn sanitize_segment(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "_".to_string()
    } else {
        cleaned
    }
}

/// Returns `base` if unused, otherwise `base_2`, `base_3`, ... — the first
/// suffix not already in `used` — mirroring `src/lib/mcpTools.ts::uniqueName`.
fn unique_name(base: String, used: &mut HashSet<String>) -> String {
    if used.insert(base.clone()) {
        return base;
    }
    let mut suffix = 2;
    loop {
        let candidate = format!("{base}_{suffix}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        suffix += 1;
    }
}

/// Builds the merged tool defs (the built-ins above, plus every connected
/// server's cached, allowlist-filtered tools, namespaced
/// `mcp__<serverId>__<toolName>`) plus the registry `agent.rs`'s dispatch
/// branch uses to resolve a composite name back to `(server_id, tool_name)`
/// — mirrors `src/lib/mcpTools.ts::mcpToolDefs`/`resolveMcpToolName`
/// exactly, including the sanitize + numeric-suffix-on-collision behavior,
/// so tool naming is identical between the desktop app and the CLI given
/// the same server config.
///
/// `mcp_entries` is the list of servers `mcp_cli::connect_all` successfully
/// connected this run — every one of them is looked up in `state.mcp` (never
/// re-queried; just the in-memory cache `mcp::connect_impl` populated) to
/// read its cached tool list.
pub async fn merged_tool_definitions(
    state: &AppState,
    mcp_entries: &[McpServerEntry],
) -> (serde_json::Value, McpToolRegistry) {
    let mut defs: Vec<serde_json::Value> =
        tool_definitions().as_array().cloned().unwrap_or_default();
    let mut registry = HashMap::new();
    let mut used: HashSet<String> = HashSet::new();

    let guard = state.mcp.lock().await;
    for entry in mcp_entries {
        let Some(connection) = guard.get(&entry.id) else {
            continue;
        };
        let tools: Vec<&mcp::CachedMcpTool> = match &entry.tool_allowlist {
            Some(allow) => connection
                .tools
                .iter()
                .filter(|t| allow.iter().any(|a| a == &t.name))
                .collect(),
            None => connection.tools.iter().collect(),
        };

        for tool in tools {
            let base = format!(
                "mcp__{}__{}",
                sanitize_segment(&entry.id),
                sanitize_segment(&tool.name)
            );
            let name = unique_name(base, &mut used);
            registry.insert(name.clone(), (entry.id.clone(), tool.name.clone()));

            let description = format!(
                "[MCP: {}] {}",
                entry.label,
                tool.description.clone().unwrap_or_default()
            );
            defs.push(serde_json::json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description.trim(),
                    "parameters": tool.input_schema,
                }
            }));
        }
    }

    (serde_json::Value::Array(defs), McpToolRegistry(registry))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_with_tools(id: &str) -> McpServerEntry {
        McpServerEntry {
            id: id.to_string(),
            label: format!("Server {id}"),
            transport: mcp::McpTransport::Stdio {
                command: "echo".to_string(),
                args: Vec::new(),
                env: Default::default(),
            },
            enabled: true,
            tool_allowlist: None,
            timeout_secs: None,
        }
    }

    #[test]
    fn sanitize_segment_replaces_illegal_chars_and_never_empty() {
        assert_eq!(sanitize_segment("my-server_1"), "my-server_1");
        assert_eq!(sanitize_segment("weird server!"), "weird_server_");
        assert_eq!(sanitize_segment(""), "_");
    }

    #[test]
    fn unique_name_suffixes_on_collision() {
        let mut used = HashSet::new();
        assert_eq!(unique_name("mcp__a__b".to_string(), &mut used), "mcp__a__b");
        assert_eq!(
            unique_name("mcp__a__b".to_string(), &mut used),
            "mcp__a__b_2"
        );
        assert_eq!(
            unique_name("mcp__a__b".to_string(), &mut used),
            "mcp__a__b_3"
        );
    }

    #[test]
    fn present_plan_tool_def_is_a_well_formed_function_def_excluded_from_the_base_list() {
        let def = present_plan_tool_def();
        assert_eq!(def["type"], "function");
        assert_eq!(def["function"]["name"], "present_plan");
        let required = def["function"]["parameters"]["required"]
            .as_array()
            .unwrap();
        assert!(required.iter().any(|v| v == "title"));
        assert!(required.iter().any(|v| v == "plan"));

        // Not part of the base list (or merged_tool_definitions' output) —
        // only agent.rs::run_tool_loop appends it, and only in Plan Mode.
        assert!(!tool_definitions()
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "present_plan"));
    }

    #[test]
    fn search_docs_tool_def_embeds_stack_names_and_is_excluded_from_the_base_list() {
        let names = vec!["Docs".to_string(), "Notes".to_string()];
        let def = search_docs_tool_def(&names);
        assert_eq!(def["function"]["name"], "search_docs");
        assert!(def["function"]["description"]
            .as_str()
            .unwrap()
            .contains("Docs, Notes"));
        let required = def["function"]["parameters"]["required"]
            .as_array()
            .unwrap();
        assert!(required.iter().any(|v| v == "query"));

        // Only ever appended per-turn by `agent.rs::run_tool_loop` when
        // `--stack` was given — never part of the base list.
        assert!(!tool_definitions()
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "search_docs"));
    }

    #[test]
    fn task_tool_def_matches_desktop_profiles_and_is_excluded_from_the_base_list() {
        let def = task_tool_def();
        assert_eq!(def["function"]["name"], "task");
        let profile_enum = def["function"]["parameters"]["properties"]["profile"]["enum"]
            .as_array()
            .unwrap();
        assert_eq!(
            profile_enum,
            &vec![
                serde_json::Value::String("explore".to_string()),
                serde_json::Value::String("code".to_string()),
            ]
        );

        // Only ever appended per-turn by `agent.rs::run_tool_loop` when
        // `--subagents` was given — never part of the base list, and the
        // base list never contains "task" either (agent.rs's depth-1 cap
        // relies on that).
        assert!(!tool_definitions()
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "task"));
    }

    #[test]
    fn base_tool_contract_includes_desktop_glob_capability() {
        let definitions = tool_definitions();
        let names = definitions
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect::<HashSet<_>>();
        for expected in [
            "read_file",
            "write_file",
            "edit_file",
            "list_dir",
            "glob",
            "grep",
            "run_shell",
            "remember",
            "web_fetch",
            "web_search",
        ] {
            assert!(names.contains(expected), "missing CLI tool {expected}");
        }
    }

    #[tokio::test]
    async fn merged_tool_definitions_skips_entries_with_no_live_connection() {
        // No `mcp::connect_impl` was ever called for this entry, so
        // `state.mcp` has nothing cached for it — it must be silently
        // skipped (not an error), same as a disabled/never-connected server
        // in the GUI's `mcpToolDefs`.
        let state = AppState::default();
        let entries = vec![entry_with_tools("never-connected")];

        let (defs, registry) = merged_tool_definitions(&state, &entries).await;
        let builtin_count = tool_definitions().as_array().unwrap().len();
        assert_eq!(defs.as_array().unwrap().len(), builtin_count);
        assert!(registry.0.is_empty());
    }
}
