//! OpenAI-style tool (function) definitions handed to the model, mirroring
//! `src/lib/tools.ts` 1:1 (minus the multi-root "label/" prefix wording —
//! the CLI supports a single `--workspace` root, so there's nothing to
//! disambiguate).

use std::collections::{HashMap, HashSet};

use little_monkey_lib::mcp::{self, McpServerEntry};
use little_monkey_lib::AppState;

/// The tool schemas themselves live in `little_monkey_lib::agent_tools`
/// because they are the source of truth the published K19 contract's `tools`
/// section is generated from, and that generator runs inside the library.
/// Re-exported here so every call site in this binary is unchanged.
pub use little_monkey_lib::agent_tools::{
    present_plan_tool_def, search_docs_tool_def, task_tool_def, tool_definitions,
};

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
