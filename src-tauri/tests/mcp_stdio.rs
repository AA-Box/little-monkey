//! Integration tests for `mcp.rs`'s connect/call path against a *real* stdio
//! MCP server (the trivial `mcp_test_server` bin — see
//! `src/bin/mcp_test_server.rs`), not a mock.
//!
//! These live here (rather than in `mcp.rs`'s own `#[cfg(test)] mod tests`,
//! where the rest of that module's tests live) because they need
//! `env!("CARGO_BIN_EXE_mcp_test_server")` to find the compiled test
//! server's path, and Cargo only defines `CARGO_BIN_EXE_<name>` for
//! integration tests (files under `tests/`) — it is *not* set for `--lib`
//! unit tests, so this same code fails to compile if moved into `mcp.rs`.

use std::collections::BTreeMap;

use little_monkey_lib::mcp::{call_tool_impl, connect_impl, disconnect_impl, McpServerEntry, McpTransport};
use little_monkey_lib::AppState;

fn stdio_entry(id: &str, command: &str, args: &[&str]) -> McpServerEntry {
    McpServerEntry {
        id: id.to_string(),
        label: format!("Test server {id}"),
        transport: McpTransport::Stdio {
            command: command.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            env: BTreeMap::new(),
        },
        enabled: true,
        tool_allowlist: None,
        timeout_secs: None,
    }
}

/// Path to the trivial `mcp_test_server` binary built alongside this crate
/// purely for these tests.
fn test_server_binary() -> &'static str {
    env!("CARGO_BIN_EXE_mcp_test_server")
}

#[tokio::test]
async fn connect_lists_tools_and_instructions_from_a_real_stdio_server() {
    let state = AppState::default();
    let entry = stdio_entry("echoer", test_server_binary(), &[]);

    let (tools, instructions) = connect_impl(&state, &entry).await.unwrap();

    assert!(tools.iter().any(|t| t.name == "echo"));
    assert!(tools.iter().any(|t| t.name == "boom"));
    assert_eq!(
        instructions.as_deref(),
        Some("Test MCP server for Little Monkey's unit tests.")
    );

    disconnect_impl(&state, &entry.id).await;
}

#[tokio::test]
async fn call_tool_round_trips_arguments_through_a_real_server() {
    let state = AppState::default();
    let entry = stdio_entry("echoer2", test_server_binary(), &[]);
    connect_impl(&state, &entry).await.unwrap();

    let result = call_tool_impl(
        &state,
        &entry,
        "echo",
        serde_json::json!({"text": "hello from a test"}),
    )
    .await
    .unwrap();

    let rmcp::model::ContentBlock::Text(text) = &result.content[0] else {
        panic!("expected a text content block, got {:?}", result.content[0]);
    };
    assert_eq!(text.text, "hello from a test");
    assert_ne!(result.is_error, Some(true));

    disconnect_impl(&state, &entry.id).await;
}

#[tokio::test]
async fn call_tool_surfaces_server_side_errors() {
    let state = AppState::default();
    let entry = stdio_entry("echoer3", test_server_binary(), &[]);
    connect_impl(&state, &entry).await.unwrap();

    let result = call_tool_impl(&state, &entry, "boom", serde_json::json!({}))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));

    disconnect_impl(&state, &entry.id).await;
}

#[tokio::test]
async fn call_tool_rejects_unknown_tool_names() {
    let state = AppState::default();
    let entry = stdio_entry("echoer4", test_server_binary(), &[]);
    connect_impl(&state, &entry).await.unwrap();

    let err = call_tool_impl(&state, &entry, "not_a_real_tool", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.contains("Unknown tool"), "unexpected error: {err}");

    disconnect_impl(&state, &entry.id).await;
}

#[tokio::test]
async fn call_tool_enforces_the_allowlist() {
    let state = AppState::default();
    let mut entry = stdio_entry("echoer5", test_server_binary(), &[]);
    connect_impl(&state, &entry).await.unwrap();
    entry.tool_allowlist = Some(vec!["boom".to_string()]); // echo not allowed

    let err = call_tool_impl(&state, &entry, "echo", serde_json::json!({"text": "x"}))
        .await
        .unwrap_err();
    assert!(err.contains("not in the allowlist"), "unexpected error: {err}");

    disconnect_impl(&state, &entry.id).await;
}

#[tokio::test]
async fn reconnect_replaces_the_previous_connection() {
    let state = AppState::default();
    let entry = stdio_entry("reconnect-me", test_server_binary(), &[]);

    connect_impl(&state, &entry).await.unwrap();
    // Connecting again for the same id must not error or leak the old
    // connection — it should be gracefully closed and replaced.
    connect_impl(&state, &entry).await.unwrap();

    assert_eq!(state.mcp.lock().await.len(), 1);

    disconnect_impl(&state, &entry.id).await;
}
