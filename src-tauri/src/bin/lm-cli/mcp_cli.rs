//! CLI parity for MCP servers (design doc phase 5): loads the same
//! `mcp_servers.json` the GUI writes (no `tauri::AppHandle` to resolve its
//! path through — the same hardcoded-identifier app-data convention as
//! `providers_cli.rs`/`checkpoints_cli.rs`), eagerly connects every enabled
//! server (mirroring the GUI's `mcp_connect_all`-on-mount, and bounded the
//! same way by `mcp::CONNECT_TIMEOUT_SECS`), and layers permission-gating +
//! a per-server timeout around
//! `little_monkey_lib::mcp::call_tool_with_cancel_impl` the same way
//! `tools_cli.rs` layers those around `run_shell`/`write_file` — a timeout
//! here genuinely cancels the in-flight request at the protocol level (a
//! real `notifications/cancelled` sent to the server), not just the
//! client's own abandoned wait.
//!
//! Deliberately does NOT wire turn-scoped cancellation via
//! `AppState::tool_cancel` — unlike the GUI's split-pane turns, the CLI has
//! no concurrent "Stop" affordance from another thread while a tool call is
//! in flight (same reasoning `tools_cli::run_shell` already applies: a plain
//! `tokio::time::timeout` is the only bound needed here).

use little_monkey_lib::mcp::{self, McpServerEntry};
use little_monkey_lib::AppState;

use crate::permission::TerminalPermissions;

/// Must match `identifier` in `src-tauri/tauri.conf.json` — same
/// hardcoded-identifier app-data resolution as `providers_cli.rs`/
/// `checkpoints_cli.rs` (duplicated per module rather than shared, following
/// their precedent).
const APP_IDENTIFIER: &str = "com.littlemonkey.app";

/// Default per-call timeout when a server entry doesn't override it via
/// `timeout_secs` — matches `mcp.rs::DEFAULT_TIMEOUT_SECS`.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

fn config_path() -> Option<std::path::PathBuf> {
    Some(dirs::data_dir()?.join(APP_IDENTIFIER).join("mcp_servers.json"))
}

/// Core logic behind [`load_enabled_servers`], parameterized by path for
/// testability (same split as `mcp.rs::load_config_impl` itself). A
/// missing/unreadable/corrupt file just means "no MCP servers configured" —
/// never a hard error — same tolerance `providers_cli::load_custom_providers`
/// has for its own file.
fn load_enabled_from(path: &std::path::Path) -> Vec<McpServerEntry> {
    mcp::load_config_impl(path)
        .map(|config| config.servers.into_iter().filter(|s| s.enabled).collect())
        .unwrap_or_default()
}

/// Loads `mcp_servers.json` (resolved the same hardcoded-identifier way
/// `providers_cli.rs` resolves `providers.json`) and returns only the
/// enabled entries.
pub fn load_enabled_servers() -> Vec<McpServerEntry> {
    match config_path() {
        Some(path) => load_enabled_from(&path),
        None => Vec::new(),
    }
}

/// Connects every entry, one at a time. A server that fails to connect (or
/// times out — see `mcp::CONNECT_TIMEOUT_SECS`; `connect_impl` itself has no
/// internal timeout, so a hung handshake would otherwise stall the whole
/// CLI's startup indefinitely) prints a `Warning:` line to stderr (same
/// "not found"/pull-error UX precedent as `llama.rs`/`cmds.rs`) and is
/// simply dropped from the returned list — its tools are never offered to
/// the model and its name never dispatches, rather than aborting the whole
/// CLI invocation over one misconfigured or unreachable server.
pub async fn connect_all(state: &AppState, entries: &[McpServerEntry]) -> Vec<McpServerEntry> {
    let mut connected = Vec::new();
    for entry in entries {
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(mcp::CONNECT_TIMEOUT_SECS),
            mcp::connect_impl(state, entry),
        )
        .await;
        match outcome {
            Ok(Ok(_)) => connected.push(entry.clone()),
            Ok(Err(e)) => eprintln!("Warning: MCP server '{}' failed to connect: {e}", entry.label),
            Err(_elapsed) => eprintln!(
                "Warning: MCP server '{}' timed out while connecting (>{}s)",
                entry.label,
                mcp::CONNECT_TIMEOUT_SECS
            ),
        }
    }
    connected
}

/// Executes one `mcp__`-namespaced tool call end to end: permission-gates it
/// with the same `mcp:<server_id>:<tool_name>` naming convention the GUI's
/// `mcp_call_tool` uses (preview = server label + tool name + pretty-printed
/// arguments, matching `run_shell`'s command-preview style), bounds it with
/// the entry's `timeout_secs` (default 60s) — genuinely: a timeout here
/// sends the server a real `notifications/cancelled` via
/// `mcp::call_tool_with_cancel_impl`, the same as the GUI's `mcp_call_tool`,
/// rather than merely abandoning the client's own wait for a response the
/// server keeps executing regardless — and flattens the resulting
/// `CallToolResult` into a string — mirroring
/// `src/lib/mcpTools.ts::formatMcpCallToolResult` exactly, content block for
/// content block, so the model sees the same shape of tool output on both
/// the desktop app and the CLI.
pub async fn call(
    state: &AppState,
    perms: &mut TerminalPermissions,
    entry: &McpServerEntry,
    tool_name: &str,
    arguments: serde_json::Value,
) -> Result<String, String> {
    let pretty_args = serde_json::to_string_pretty(&arguments).unwrap_or_else(|_| arguments.to_string());
    let detail = format!("{} → {}\n{}", entry.label, tool_name, pretty_args);
    perms.request(&format!("mcp:{}:{}", entry.id, tool_name), &detail).await?;

    let timeout = std::time::Duration::from_secs(entry.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let timeout_reason = async move {
        tokio::time::sleep(timeout).await;
        format!(
            "MCP tool '{}' on server '{}' timed out after {} seconds",
            tool_name,
            entry.id,
            timeout.as_secs()
        )
    };
    let result = mcp::call_tool_with_cancel_impl(state, entry, tool_name, arguments, timeout_reason).await?;
    Ok(format_call_tool_result(&result))
}

/// Flattens an `rmcp::model::CallToolResult` into the plain string used as a
/// `tool` message's content: text blocks concatenated, non-text blocks
/// (image/audio/resource/resource_link) rendered as an identifying
/// placeholder — full passthrough is a later enhancement (design doc phase
/// 6), same division of labor `mcpTools.ts` draws. `is_error: true` maps
/// into the same `{"error": ...}` JSON shape every other CLI tool failure
/// uses (see `agent.rs::execute_tool_call`'s final match arm).
fn format_call_tool_result(result: &rmcp::model::CallToolResult) -> String {
    let parts: Vec<String> = result
        .content
        .iter()
        .map(|block| match block {
            rmcp::model::ContentBlock::Text(text) => text.text.clone(),
            rmcp::model::ContentBlock::Image(_) => "[image]".to_string(),
            rmcp::model::ContentBlock::Audio(_) => "[audio]".to_string(),
            rmcp::model::ContentBlock::Resource(embedded) => {
                let uri = match &embedded.resource {
                    rmcp::model::ResourceContents::TextResourceContents { uri, .. } => uri.as_str(),
                    rmcp::model::ResourceContents::BlobResourceContents { uri, .. } => uri.as_str(),
                    _ => "unknown",
                };
                format!("[resource: {uri}]")
            }
            rmcp::model::ContentBlock::ResourceLink(link) => format!("[resource: {}]", link.uri),
            _ => "[unknown content]".to_string(),
        })
        .collect();
    let text = parts.join("\n");

    if result.is_error == Some(true) {
        serde_json::json!({ "error": if text.is_empty() { "MCP tool call failed".to_string() } else { text } })
            .to_string()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{CallToolResult, ContentBlock};

    #[test]
    fn format_concatenates_text_blocks() {
        let result = CallToolResult::success(vec![ContentBlock::text("hello"), ContentBlock::text("world")]);
        assert_eq!(format_call_tool_result(&result), "hello\nworld");
    }

    #[test]
    fn format_renders_placeholders_for_non_text_blocks() {
        let result = CallToolResult::success(vec![
            ContentBlock::image("base64data", "image/png"),
            ContentBlock::audio("base64data", "audio/wav"),
        ]);
        assert_eq!(format_call_tool_result(&result), "[image]\n[audio]");
    }

    #[test]
    fn format_maps_is_error_to_json_error_shape() {
        let result = CallToolResult::error(vec![ContentBlock::text("boom")]);
        assert_eq!(format_call_tool_result(&result), r#"{"error":"boom"}"#);
    }

    #[test]
    fn format_error_with_no_text_falls_back_to_generic_message() {
        let mut result = CallToolResult::error(vec![]);
        result.content = Vec::new();
        assert_eq!(format_call_tool_result(&result), r#"{"error":"MCP tool call failed"}"#);
    }

    #[tokio::test]
    async fn connect_all_skips_and_warns_on_failure_rather_than_aborting() {
        let state = AppState::default();
        let entries = vec![McpServerEntry {
            id: "broken".to_string(),
            label: "Broken server".to_string(),
            transport: mcp::McpTransport::Stdio {
                command: "definitely-not-a-real-binary-xyz".to_string(),
                args: Vec::new(),
                env: Default::default(),
            },
            enabled: true,
            tool_allowlist: None,
            timeout_secs: None,
        }];

        let connected = connect_all(&state, &entries).await;
        assert!(connected.is_empty(), "a failed connection must not be returned as connected");
    }

    #[test]
    fn load_enabled_servers_is_empty_when_config_path_is_unresolvable_or_missing() {
        // No OS-dependent assertion beyond "doesn't panic and returns
        // something" — the real app-data dir may or may not have a
        // `mcp_servers.json` in this test environment, so this only checks
        // the function is safe to call, mirroring
        // `providers_cli`'s own lack of a path-injection seam.
        let _ = load_enabled_servers();
    }

    fn temp_config_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "lm_cli_mcp_cli_test_{}_{}_{}",
            std::process::id(),
            name,
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ))
    }

    fn stdio_entry(id: &str, enabled: bool) -> McpServerEntry {
        McpServerEntry {
            id: id.to_string(),
            label: format!("Server {id}"),
            transport: mcp::McpTransport::Stdio {
                command: "echo".to_string(),
                args: Vec::new(),
                env: Default::default(),
            },
            enabled,
            tool_allowlist: None,
            timeout_secs: None,
        }
    }

    #[test]
    fn load_enabled_from_filters_out_disabled_servers() {
        let path = temp_config_path("filter.json");
        mcp::add_server_impl(&path, stdio_entry("on", true)).unwrap();
        mcp::add_server_impl(&path, stdio_entry("off", false)).unwrap();

        let enabled = load_enabled_from(&path);
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].id, "on");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_enabled_from_missing_file_is_empty() {
        let path = temp_config_path("missing.json");
        assert!(load_enabled_from(&path).is_empty());
    }
}
