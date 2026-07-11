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
        }
    ])
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
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
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
    let mut defs: Vec<serde_json::Value> = tool_definitions().as_array().cloned().unwrap_or_default();
    let mut registry = HashMap::new();
    let mut used: HashSet<String> = HashSet::new();

    let guard = state.mcp.lock().await;
    for entry in mcp_entries {
        let Some(connection) = guard.get(&entry.id) else { continue };
        let tools: Vec<&mcp::CachedMcpTool> = match &entry.tool_allowlist {
            Some(allow) => connection.tools.iter().filter(|t| allow.iter().any(|a| a == &t.name)).collect(),
            None => connection.tools.iter().collect(),
        };

        for tool in tools {
            let base = format!("mcp__{}__{}", sanitize_segment(&entry.id), sanitize_segment(&tool.name));
            let name = unique_name(base, &mut used);
            registry.insert(name.clone(), (entry.id.clone(), tool.name.clone()));

            let description = format!("[MCP: {}] {}", entry.label, tool.description.clone().unwrap_or_default());
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
            transport: mcp::McpTransport::Stdio { command: "echo".to_string(), args: Vec::new(), env: Default::default() },
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
        assert_eq!(unique_name("mcp__a__b".to_string(), &mut used), "mcp__a__b_2");
        assert_eq!(unique_name("mcp__a__b".to_string(), &mut used), "mcp__a__b_3");
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
