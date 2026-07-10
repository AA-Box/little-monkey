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

use little_monkey_lib::mcp::{
    call_tool_impl, call_tool_with_cancel_impl, connect_impl, disconnect_all, disconnect_impl, McpServerEntry,
    McpTransport,
};
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

/// Regression test for the app-quit zombie-process bug: `disconnect_all`
/// (called from `lib.rs`'s `RunEvent::Exit` handler) must actually clear
/// every live connection, not just the one `disconnect_impl` targets.
#[tokio::test]
async fn disconnect_all_clears_every_connected_server() {
    let state = AppState::default();
    connect_impl(&state, &stdio_entry("d1", test_server_binary(), &[])).await.unwrap();
    connect_impl(&state, &stdio_entry("d2", test_server_binary(), &[])).await.unwrap();
    assert_eq!(state.mcp.lock().await.len(), 2);

    disconnect_all(&state).await;

    assert!(state.mcp.lock().await.is_empty());
}

/// `connect_impl` itself has no internal timeout (see its own doc comment) —
/// `mcp_connect`/lm-cli's `connect_all` are responsible for bounding it. This
/// proves the mechanism they use (wrapping the call in an external
/// `tokio::time::timeout`) actually works against a real child process that
/// spawns successfully but never speaks the MCP protocol at all (a stand-in
/// for a wrong-binary/hung-handshake server) — it doesn't itself deadlock,
/// and doesn't leave anything registered under the id once it gives up.
#[tokio::test]
async fn connect_impl_can_be_bounded_by_an_external_timeout_against_a_hung_server() {
    let state = AppState::default();
    let entry = stdio_entry("hangs-forever", "sleep", &["30"]);

    let result = tokio::time::timeout(std::time::Duration::from_millis(500), connect_impl(&state, &entry)).await;
    assert!(
        result.is_err(),
        "connect_impl unexpectedly completed against a server that never speaks MCP"
    );
    assert!(!state.mcp.lock().await.contains_key("hangs-forever"));
}

/// Regression test for the cancellation-doesn't-reach-the-server bug: a
/// client-side cancel (Stop button, or a per-server timeout) must actually
/// send the MCP server a real `notifications/cancelled`, not just abandon
/// the client's own wait while the server keeps working. Proven end to end
/// against a real child process: `wait_for_cancel` (see
/// `mcp_test_server.rs`) only writes its marker file if it observes an
/// actual protocol-level cancellation for its own request id.
#[tokio::test]
async fn call_tool_cancellation_sends_a_real_cancelled_notification_to_the_server() {
    let marker = std::env::temp_dir().join(format!(
        "lm_mcp_cancel_marker_{}_{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let _ = std::fs::remove_file(&marker);

    let state = AppState::default();
    let entry = McpServerEntry {
        id: "cancel-me".to_string(),
        label: "Cancel test server".to_string(),
        transport: McpTransport::Stdio {
            command: test_server_binary().to_string(),
            args: Vec::new(),
            env: BTreeMap::from([(
                "MCP_TEST_CANCEL_MARKER".to_string(),
                marker.to_string_lossy().to_string(),
            )]),
        },
        enabled: true,
        tool_allowlist: None,
        timeout_secs: None,
    };
    connect_impl(&state, &entry).await.unwrap();

    // A cancel signal that fires almost immediately — well before the
    // tool's own 30s fallback — simulating a Stop click or a short
    // per-server timeout while `wait_for_cancel` is in flight.
    let cancel = async {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        "test cancel".to_string()
    };

    let err = call_tool_with_cancel_impl(&state, &entry, "wait_for_cancel", serde_json::json!({}), cancel)
        .await
        .unwrap_err();
    assert_eq!(err, "test cancel");

    // Give the (separate) server process a moment to receive the
    // notification and write the marker before asserting on it.
    let mut seen = false;
    for _ in 0..50 {
        if marker.exists() {
            seen = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        seen,
        "server never observed a notifications/cancelled for the cancelled call"
    );
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "cancelled");

    let _ = std::fs::remove_file(&marker);
    disconnect_impl(&state, &entry.id).await;
}

/// `call_tool_impl` (the non-cancellable entry point CLI's `mcp_cli::call`
/// used before `call_tool_with_cancel_impl` existed, and this module's own
/// unit tests still use) must keep behaving exactly as before: it's just
/// `call_tool_with_cancel_impl` with a `cancel` that never resolves.
#[tokio::test]
async fn call_tool_impl_still_works_with_no_cancellation_wired_up() {
    let state = AppState::default();
    let entry = stdio_entry("plain-call", test_server_binary(), &[]);
    connect_impl(&state, &entry).await.unwrap();

    let result = call_tool_impl(&state, &entry, "echo", serde_json::json!({"text": "still works"}))
        .await
        .unwrap();
    let rmcp::model::ContentBlock::Text(text) = &result.content[0] else {
        panic!("expected a text content block, got {:?}", result.content[0]);
    };
    assert_eq!(text.text, "still works");

    disconnect_impl(&state, &entry.id).await;
}
