//! Minimal stdio MCP server used *only* by `mcp.rs`'s unit tests, to exercise
//! the real `TokioChildProcess` connect → `list_tools` → `call_tool` path
//! end-to-end against an actual child process (not a mock), the way a real
//! third-party MCP server would be spoken to. Never shipped as a
//! user-facing feature — it has no purpose outside `cargo test`.
//!
//! Exposes three tools:
//! - `echo`: returns its `text` argument unchanged, so tests can assert the
//!   round trip through JSON-RPC actually happened.
//! - `boom`: always fails, so tests can assert `is_error`/error propagation.
//! - `wait_for_cancel`: waits until it observes a real MCP-protocol
//!   `notifications/cancelled` for its own in-flight request (or 30s elapse),
//!   used only by `mcp.rs`'s cancellation-propagation test to prove a
//!   client-side Stop/timeout on a slow tool call actually reaches the
//!   server, instead of merely abandoning the client's own wait for it.

use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct EchoRequest {
    text: String,
}

#[derive(Debug, Clone)]
struct TestServer {
    tool_router: ToolRouter<Self>,
}

impl TestServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router(router = tool_router)]
impl TestServer {
    /// Echoes `text` back unchanged.
    #[tool(name = "echo", description = "Echo back the given text.")]
    async fn echo(&self, params: Parameters<EchoRequest>) -> String {
        params.0.text
    }

    /// Always returns a tool-level error (`is_error: true` in the result,
    /// per the MCP spec's recommended way for a tool's own execution to
    /// fail) — deliberately *not* a protocol-level JSON-RPC error, which is
    /// reserved for things like an unknown tool name.
    #[tool(name = "boom", description = "Always returns a tool error.")]
    async fn boom(&self) -> CallToolResult {
        CallToolResult::error(vec![ContentBlock::text("intentional test failure")])
    }

    /// Waits until this request's `ct` (extracted automatically by `rmcp`'s
    /// `#[tool]` macro — see `FromContextPart<_> for CancellationToken`) is
    /// cancelled, i.e. until the client sends a real MCP-protocol
    /// `notifications/cancelled` referencing this call's request id, or 30
    /// seconds pass. If cancelled, writes a marker to the path named by the
    /// `MCP_TEST_CANCEL_MARKER` env var (set by the test that spawns this
    /// server), so the test can observe — from OUTSIDE this process — that
    /// the cancellation notification genuinely reached the server, not just
    /// that the client stopped waiting for a response.
    #[tool(
        name = "wait_for_cancel",
        description = "Waits until cancelled (writing a marker file) or 30s elapse. For tests only."
    )]
    async fn wait_for_cancel(&self, ct: CancellationToken) -> String {
        tokio::select! {
            _ = ct.cancelled() => {
                if let Ok(path) = std::env::var("MCP_TEST_CANCEL_MARKER") {
                    let _ = std::fs::write(path, "cancelled");
                }
                "cancelled".to_string()
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => "timed_out".to_string(),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for TestServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Test MCP server for Little Monkey's unit tests.")
    }
}

#[tokio::main]
async fn main() {
    let transport = (tokio::io::stdin(), tokio::io::stdout());
    let service = TestServer::new()
        .serve(transport)
        .await
        .expect("failed to start test MCP server");
    let _ = service.waiting().await;
}
