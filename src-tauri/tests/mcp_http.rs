//! Integration tests for `mcp.rs`'s connect/call path against a *real*
//! streamable-HTTP MCP server — not a mock of `rmcp` itself, but a
//! hand-rolled raw-TCP HTTP/1.1 responder that speaks the wire protocol
//! using `rmcp`'s own JSON-RPC model types (`ClientJsonRpcMessage`,
//! `ServerJsonRpcMessage`, `ServerResult`, ...) for (de)serialization, so the
//! bytes on the wire are exactly what a real MCP HTTP server would
//! exchange. This exercises the phase-4 `StreamableHttpClientTransport`
//! path end-to-end, mirroring how `tests/mcp_stdio.rs` exercises the
//! `TokioChildProcess` path against a real (if trivial) child process.
//!
//! Deliberately stateless (never returns an `Mcp-Session-Id` response
//! header): `StreamableHttpClientTransportConfig::allow_stateless` defaults
//! to `true`, so the client is fine with that, and it means this fake
//! server never needs to track sessions or serve a standalone SSE GET
//! stream — every JSON-RPC message is just one POST request/response pair
//! over its own short-lived connection (the server always answers with
//! `Connection: close`, so the client opens a fresh TCP connection per
//! request instead of trying to reuse one).
//!
//! Bearer-token attachment itself (`mcp_set_http_token`/`read_http_token`)
//! is NOT exercised here — that would mean writing to the real OS keychain
//! from an automated test, which nothing in this codebase does (see
//! `providers.rs`, which never unit-tests its own keychain read/write path
//! either). Instead, the "no token configured" default path is asserted:
//! the fake server records whether it ever saw an `Authorization` header,
//! and the test asserts it did not, since `read_http_token` finds nothing
//! in the keychain for a server id that was never given one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rmcp::model::{
    CallToolResult, ClientJsonRpcMessage, ClientRequest, ContentBlock, JsonRpcMessage,
    ListToolsResult, ServerCapabilities, ServerInfo, ServerJsonRpcMessage, ServerResult, Tool,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use little_monkey_lib::mcp::{
    call_tool_impl, connect_impl, disconnect_impl, McpServerEntry, McpTransport,
};
use little_monkey_lib::AppState;

const TEST_INSTRUCTIONS: &str = "Test HTTP MCP server instructions.";

fn http_entry(id: &str, url: String) -> McpServerEntry {
    McpServerEntry {
        id: id.to_string(),
        label: format!("Test HTTP server {id}"),
        transport: McpTransport::Http { url },
        enabled: true,
        tool_allowlist: None,
        timeout_secs: None,
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Reads one HTTP/1.1 request off `stream` (headers + exactly
/// `Content-Length` body bytes) and returns `(header_text, body)`. Good
/// enough for what reqwest's streamable-HTTP client actually sends — not a
/// general-purpose HTTP parser.
async fn read_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];

    let header_end = loop {
        let n = stream
            .read(&mut chunk)
            .await
            .expect("read failed while waiting for headers");
        assert!(n > 0, "connection closed before headers completed");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
    };
    let header_text = String::from_utf8_lossy(&buf[..header_end]).to_string();

    let content_length: usize = header_text
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);

    let body_start = header_end + 4;
    while buf.len() < body_start + content_length {
        let n = stream
            .read(&mut chunk)
            .await
            .expect("read failed while waiting for body");
        assert!(n > 0, "connection closed before body completed");
        buf.extend_from_slice(&chunk[..n]);
    }

    (
        header_text,
        buf[body_start..body_start + content_length].to_vec(),
    )
}

async fn write_json_response(stream: &mut TcpStream, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .expect("write headers failed");
    stream.write_all(body).await.expect("write body failed");
    stream.flush().await.expect("flush failed");
}

async fn write_accepted_response(stream: &mut TcpStream) {
    let head = "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    stream
        .write_all(head.as_bytes())
        .await
        .expect("write headers failed");
    stream.flush().await.expect("flush failed");
}

/// Handles exactly one JSON-RPC message over one accepted TCP connection,
/// responding using `rmcp`'s own model types so the reply is byte-for-byte
/// what a real server would send.
async fn handle_one_request(
    mut stream: TcpStream,
    tools: Vec<Tool>,
    saw_auth_header: Arc<AtomicBool>,
) {
    let (headers, body) = read_request(&mut stream).await;
    if headers
        .lines()
        .any(|line| line.to_ascii_lowercase().starts_with("authorization:"))
    {
        saw_auth_header.store(true, Ordering::SeqCst);
    }

    match serde_json::from_slice::<ClientJsonRpcMessage>(&body) {
        Ok(JsonRpcMessage::Request(req)) => {
            let id = req.id.clone();
            let result = match req.request {
                ClientRequest::InitializeRequest(_) => ServerResult::from(
                    ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
                        .with_instructions(TEST_INSTRUCTIONS),
                ),
                ClientRequest::ListToolsRequest(_) => {
                    ServerResult::from(ListToolsResult::with_all_items(tools))
                }
                ClientRequest::CallToolRequest(call) => {
                    if call.params.name == "boom_http" {
                        ServerResult::from(CallToolResult::error(vec![ContentBlock::text(
                            "intentional http test failure",
                        )]))
                    } else {
                        let text = call
                            .params
                            .arguments
                            .as_ref()
                            .and_then(|args| args.get("text"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        ServerResult::from(CallToolResult::success(vec![ContentBlock::text(text)]))
                    }
                }
                _ => ServerResult::empty(()),
            };
            let message = ServerJsonRpcMessage::response(result, id);
            let payload = serde_json::to_vec(&message).expect("serialize response failed");
            write_json_response(&mut stream, &payload).await;
        }
        Ok(JsonRpcMessage::Notification(_)) => {
            // e.g. `notifications/initialized` — no response body expected.
            write_accepted_response(&mut stream).await;
        }
        other => panic!(
            "unexpected client message: {other:?} (raw body: {:?})",
            String::from_utf8_lossy(&body)
        ),
    }
}

/// Spawns a background task serving one connection-per-request forever
/// (until aborted) on an OS-assigned localhost port, and returns its URL,
/// the join handle, and a flag that becomes `true` the first time any
/// request carries an `Authorization` header.
async fn spawn_fake_http_mcp_server(
    tools: Vec<Tool>,
) -> (String, tokio::task::JoinHandle<()>, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind failed");
    let addr = listener.local_addr().expect("local_addr failed");
    let saw_auth_header = Arc::new(AtomicBool::new(false));
    let saw_auth_header_for_server = saw_auth_header.clone();

    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let tools = tools.clone();
            let saw_auth_header = saw_auth_header_for_server.clone();
            tokio::spawn(handle_one_request(stream, tools, saw_auth_header));
        }
    });

    (format!("http://{addr}/mcp"), handle, saw_auth_header)
}

#[tokio::test]
async fn connect_lists_tools_and_instructions_from_a_real_http_server() {
    let tools = vec![Tool::new(
        "echo_http",
        "Echo back the given text over HTTP.",
        serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
            .as_object()
            .unwrap()
            .clone(),
    )];
    let (url, server, saw_auth_header) = spawn_fake_http_mcp_server(tools).await;
    let state = AppState::default();
    let entry = http_entry("http-echoer", url);

    let (tools, instructions) = connect_impl(&state, &entry).await.unwrap();

    assert!(tools.iter().any(|t| t.name == "echo_http"));
    assert_eq!(instructions.as_deref(), Some(TEST_INSTRUCTIONS));
    // No token was ever saved for this server id, so `connect_impl` must not
    // have attached an Authorization header.
    assert!(!saw_auth_header.load(Ordering::SeqCst));

    disconnect_impl(&state, &entry.id).await;
    server.abort();
}

#[tokio::test]
async fn call_tool_round_trips_arguments_through_a_real_http_server() {
    let tools = vec![
        Tool::new(
            "echo_http",
            "Echo",
            serde_json::json!({"type": "object"})
                .as_object()
                .unwrap()
                .clone(),
        ),
        Tool::new(
            "boom_http",
            "Always fails",
            serde_json::json!({"type": "object"})
                .as_object()
                .unwrap()
                .clone(),
        ),
    ];
    let (url, server, _saw_auth_header) = spawn_fake_http_mcp_server(tools).await;
    let state = AppState::default();
    let entry = http_entry("http-echoer2", url);

    connect_impl(&state, &entry).await.unwrap();

    let ok = call_tool_impl(
        &state,
        &entry,
        "echo_http",
        serde_json::json!({ "text": "hello over http" }),
    )
    .await
    .unwrap();
    assert_eq!(
        ok.content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str()),
        Some("hello over http")
    );
    assert_ne!(ok.is_error, Some(true));

    let err = call_tool_impl(&state, &entry, "boom_http", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(err.is_error, Some(true));

    disconnect_impl(&state, &entry.id).await;
    server.abort();
}

#[tokio::test]
async fn connect_rejects_an_empty_http_url() {
    let state = AppState::default();
    let entry = http_entry("http-no-url", "   ".to_string());

    let err = connect_impl(&state, &entry).await.unwrap_err();
    assert!(err.contains("no URL configured"), "unexpected error: {err}");
}
