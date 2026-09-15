//! The end-to-end acceptance test: a provider message becomes an agent run,
//! and the agent's own reply comes back out through the provider.
//!
//! Everything between the two fixtures is production code, running the way the
//! resident daemon runs it:
//!
//! ```text
//! provider fixture
//!   → production adapter (poll)
//!   → durable channel event
//!   → production channel ingress
//!   → real DaemonChannelQueue::submit  (writes the immutable snapshot,
//!     registers the durable run through a real monkey-cli child)
//!   → real DaemonEngine tick → real task-run child process
//!   → real agent loop, offered the real send_message tool schema
//!   → model transport → the agent dispatches the tool call it got back
//!   → production channel_tool::send_message → durable outbox row
//!   → production drain_outbox_once → production adapter
//!   → provider fixture receives the outbound request
//! ```
//!
//! Two things are fixtures, and only two:
//!
//! - **The provider endpoints.** The adapter driving them is the one release
//!   builds ship.
//! - **The model's HTTP transport.** A recipe may name any OpenAI-compatible
//!   origin (`target.local_url`) — that is how this app already talks to
//!   llama.cpp, vLLM and LM Studio — so pointing one at a loopback server is
//!   using the product's own seam, not bypassing it. The fixture answers with
//!   a real tool call and nothing else; it cannot send a message, and the
//!   reply only exists because the production agent loop parsed that call and
//!   dispatched it. This is what keeps the test free of any model account.
//!
//! Nothing here fabricates an outbox row, an ingress turn or a job. The
//! assertions at the end are written so that a test which quietly stopped
//! crossing the agent boundary would fail rather than pass.
//!
//! # Why the body runs in a child process
//!
//! A real daemon child resolves its profile from the environment, so this test
//! needs an isolated `HOME` — and an environment variable is process-wide,
//! which in a test binary means every other test in the same process. So the
//! test re-executes itself once, with that environment, and the isolated work
//! happens in a process of its own. The parent is a launcher; the child is the
//! test.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::durable_run::CliRunEventSink;
use little_monkey_lib::channels::policy::{AccessPolicy, ChannelAccessPolicy, GroupActivation};
use little_monkey_lib::channels::routing::{ChannelRoute, RouteScope, RouteTarget};
use little_monkey_lib::channels::types::{ChannelHealth, ChannelKind};
use little_monkey_lib::run_protocol::RunEvent;

use super::adapters::telegram::TelegramAdapter;
use super::channel_adapter::{AdapterConfig, ChannelAdapter};
use super::channel_store::{ChannelAccountRecord, EventDirection};
use super::channel_worker::{drain_outbox_once, poll_account_once};
use super::engine::{DaemonEngine, RealProcessAdapter, SystemClock};
use super::ledger::SharedLedger;
use super::store::{DaemonConfig, DaemonPaths, DaemonStore, JobState};
use super::DaemonChannelQueue;

/// Set on the re-executed copy of this test binary, so it runs the body
/// instead of launching another child.
const CHILD_ENV: &str = "LM_CHANNEL_E2E_CHILD";
/// Where the isolated profile lives, handed to the child.
const ROOT_ENV: &str = "LM_CHANNEL_E2E_ROOT";
/// Printed by the child when it declines to run; surfaced by the launcher so
/// a skip is never silent.
pub(crate) const SKIPPED: &str = "channel agent end-to-end SKIPPED";

pub(crate) const ACCOUNT_ID: &str = "e2e-account";
pub(crate) const RECIPE: &str = "channel-e2e";
const BOT_TOKEN: &str = "e2e-bot-token";
/// What the fixture model asks `send_message` to say. Distinctive on purpose:
/// finding it on the provider's wire is the proof the whole chain ran.
const REPLY_TEXT: &str = "fixture reply from the agent loop";

// ---------------------------------------------------------------------------
// Finding the binary a daemon child must actually be
// ---------------------------------------------------------------------------

/// Where the launcher recorded the CLI binary it found, for the daemon code
/// running inside the child to launch.
const CLI_ENV: &str = "LM_CHANNEL_E2E_CLI";

/// The real `monkey-cli` this test's daemon children must be, or `None` when
/// nothing named one.
///
/// Called from [`super::monkey_executable`] under `#[cfg(test)]` only: a test
/// harness is not the CLI, so a daemon child launched from it would be a
/// second copy of the test suite. In a release build this function does not
/// exist and the daemon always launches itself.
pub(super) fn cli_beside_test_binary(_current: &Path) -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os(CLI_ENV)?);
    path.is_file().then_some(path)
}

/// Finds the `monkey-cli` cargo built for this profile.
///
/// The uplifted `target/<profile>/monkey-cli` cannot be trusted — a workspace
/// may keep its own file at that name — so the candidates are the artifacts in
/// `deps/` plus that path, newest first, and each is asked what it is: a test
/// harness answers `--list` with its test names, and the CLI refuses the flag.
/// One exec per candidate, once per test run.
fn locate_cli_binary() -> PathBuf {
    let current = std::env::current_exe().expect("test binary path");
    let deps = current.parent().expect("deps directory").to_path_buf();
    let profile = deps.parent().expect("profile directory").to_path_buf();
    let name = if cfg!(windows) {
        "monkey-cli.exe"
    } else {
        "monkey-cli"
    };

    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&deps)
        .expect("read deps")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path != &current
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with("monkey_cli-") && !name.contains('.')
                            || name.starts_with("monkey_cli-") && name.ends_with(".exe")
                    })
        })
        .filter_map(|path| Some((path.metadata().ok()?.modified().ok()?, path)))
        .collect();
    candidates.sort_by(|left, right| right.0.cmp(&left.0));
    let mut ordered: Vec<PathBuf> = candidates.into_iter().map(|(_, path)| path).collect();
    ordered.push(profile.join(name));

    for candidate in &ordered {
        if is_the_cli(candidate) {
            return candidate.clone();
        }
    }
    panic!(
        "no monkey-cli binary found beside the test harness in {}; \
         build it with `cargo build --bin monkey-cli`",
        deps.display()
    );
}

/// True when `candidate` is the CLI rather than a test harness: libtest
/// answers `--list` with its tests, clap rejects the flag outright.
fn is_the_cli(candidate: &Path) -> bool {
    if !candidate.is_file() {
        return false;
    }
    let Ok(output) = std::process::Command::new(candidate).arg("--list").output() else {
        return false;
    };
    !output.status.success() && !String::from_utf8_lossy(&output.stdout).contains(": test")
}

// ---------------------------------------------------------------------------
// A routing loopback HTTP fixture
// ---------------------------------------------------------------------------

/// Answers many requests, choosing the response by what the request asked for.
///
/// The existing `test_http::serve` plays a fixed script one connection at a
/// time, which cannot answer a daemon child whose request order this test does
/// not control. This one is a tiny router: `route` sees the request line and
/// body and returns the raw response to write.
pub(crate) struct HttpFixture {
    pub(crate) base: String,
    seen: Arc<Mutex<Vec<String>>>,
    calls: Arc<AtomicUsize>,
}

impl HttpFixture {
    pub(crate) fn spawn(
        route: impl Fn(&str, &str, usize) -> String + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let log = seen.clone();
        let counter = calls.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let log = log.clone();
                let counter = counter.clone();
                let route = &route;
                // Serve inline: these fixtures answer one request at a time
                // and a thread per connection buys nothing here.
                let Some((head, body)) = read_request(&mut stream) else {
                    continue;
                };
                let index = counter.fetch_add(1, Ordering::SeqCst);
                let response = route(&head, &body, index);
                log.lock().unwrap().push(format!("{head}\n{body}"));
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Ok(Self {
            base: format!("http://127.0.0.1:{port}"),
            seen,
            calls,
        })
    }

    /// Every request received so far, as "<request line + headers>\n<body>".
    pub(crate) fn requests(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    pub(crate) fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

/// Reads one HTTP request, honouring `Content-Length` and chunked bodies.
fn read_request(stream: &mut std::net::TcpStream) -> Option<(String, String)> {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .ok()?;
    let mut received = Vec::new();
    let mut scratch = [0u8; 8192];
    let mut header_end = None;
    let mut content_length = 0usize;
    let mut chunked = false;
    loop {
        match stream.read(&mut scratch) {
            Ok(0) => break,
            Ok(count) => {
                received.extend_from_slice(&scratch[..count]);
                if header_end.is_none() {
                    if let Some(index) = find(&received, b"\r\n\r\n") {
                        header_end = Some(index + 4);
                        let headers = String::from_utf8_lossy(&received[..index]).to_string();
                        content_length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        chunked = headers
                            .to_ascii_lowercase()
                            .contains("transfer-encoding: chunked");
                    }
                }
                if let Some(start) = header_end {
                    let complete = if content_length > 0 {
                        received.len() >= start + content_length
                    } else if chunked {
                        find(&received[start..], b"0\r\n\r\n").is_some()
                    } else {
                        true
                    };
                    if complete {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    let split = header_end?;
    Some((
        String::from_utf8_lossy(&received[..split]).to_string(),
        String::from_utf8_lossy(&received[split..]).to_string(),
    ))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

pub(crate) fn json_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

pub(crate) fn sse_response(frames: &[serde_json::Value]) -> String {
    let mut body = String::new();
    for frame in frames {
        body.push_str(&format!("data: {frame}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

// ---------------------------------------------------------------------------
// The provider fixtures
// ---------------------------------------------------------------------------

/// One provider, wired to a fixture, ready to play the same part.
///
/// The adapter is always the production one. Everything else here is what the
/// test needs to drive and read that provider's own protocol.
struct ProviderWorld {
    kind: ChannelKind,
    adapter: Arc<dyn ChannelAdapter>,
    /// Conversation the run must answer, as it appears on the wire.
    conversation_id: &'static str,
    /// Substring that identifies the outbound send request.
    send_marker: &'static str,
    /// The id the fixture returns for that send.
    provider_message_id: &'static str,
    requests: Box<dyn Fn() -> Vec<String> + Send + Sync>,
    /// Kept alive for the length of the test; a dropped socket fixture stops
    /// answering.
    _sockets: Option<super::channel_restart_tests::WsFixture>,
}

pub(crate) fn account_record(kind: ChannelKind, now: i64) -> ChannelAccountRecord {
    ChannelAccountRecord {
        account_id: ACCOUNT_ID.into(),
        kind,
        label: "End-to-end".into(),
        enabled: true,
        non_secret_config: serde_json::json!({}),
        credential_ref: Some(format!("test:{ACCOUNT_ID}")),
        access_policy: ChannelAccessPolicy {
            direct: AccessPolicy::Open,
            group: AccessPolicy::Open,
            group_activation: GroupActivation::Always,
        },
        health: ChannelHealth::connected(now, None),
        created_at_ms: now,
        updated_at_ms: now,
    }
}

/// Enough of the Bot API for one inbound message and one outbound reply.
fn telegram_world(now: i64) -> ProviderWorld {
    const CHAT: &str = "-4242";
    const MESSAGE_ID: &str = "918273";
    let fixture = HttpFixture::spawn(move |head, _body, _index| {
        if head.contains("/getMe") {
            return json_response(
                r#"{"ok":true,"result":{"id":4242,"is_bot":true,"username":"e2ebot"}}"#,
            );
        }
        if head.contains("/getUpdates") {
            // One update, then nothing: the adapter's own cursor keeps it from
            // being replayed, and the empty answers let a second poll return.
            return json_response(
                r#"{"ok":true,"result":[{"update_id":7001,"message":{"message_id":11,"date":1700000000,"chat":{"id":-4242,"type":"private"},"from":{"id":9001,"is_bot":false,"first_name":"Ada","username":"ada"},"text":"what is the shape of this pipeline?"}}]}"#,
            );
        }
        if head.contains("/sendMessage") {
            return json_response(&format!(
                r#"{{"ok":true,"result":{{"message_id":{MESSAGE_ID},"chat":{{"id":{CHAT},"type":"private"}}}}}}"#
            ));
        }
        json_response(r#"{"ok":true,"result":[]}"#)
    })
    .expect("bind the telegram fixture");

    let record = account_record(ChannelKind::Telegram, now);
    let adapter = Arc::new(
        TelegramAdapter::new(&AdapterConfig {
            account: &record,
            secret: BOT_TOKEN.into(),
        })
        .expect("telegram adapter")
        .with_base_url(&fixture.base),
    );
    let requests = fixture.seen.clone();
    ProviderWorld {
        kind: ChannelKind::Telegram,
        adapter,
        conversation_id: CHAT,
        send_marker: "/sendMessage",
        provider_message_id: MESSAGE_ID,
        requests: Box::new(move || requests.lock().unwrap().clone()),
        _sockets: None,
    }
}

/// REST gateway lookup, a live Gateway socket, and the create-message route.
fn discord_world(now: i64) -> ProviderWorld {
    const CHANNEL: &str = "chan-e2e";
    const MESSAGE_ID: &str = "msg-out-77";
    let sockets = super::channel_restart_tests::spawn_ws_fixture(vec![vec![serde_json::json!({
        "op": 10, "d": { "heartbeat_interval": 45_000 }
    })
    .to_string()]]);
    let gateway = sockets.url.clone();
    let fixture = HttpFixture::spawn(move |head, _body, _index| {
        if head.contains("/gateway/bot") {
            return json_response(&format!(
                r#"{{"url":"{gateway}","session_start_limit":{{"total":1000,"remaining":999,"reset_after":86400000,"max_concurrency":1}}}}"#
            ));
        }
        if head.contains("/messages") {
            return json_response(&format!(r#"{{"id":"{MESSAGE_ID}"}}"#));
        }
        if head.contains("/channels/") {
            return json_response(&format!(r#"{{"id":"{CHANNEL}","type":0}}"#));
        }
        json_response(r#"{"id":"bot-e2e","username":"monkey"}"#)
    })
    .expect("bind the discord fixture");

    // The inbound message, pushed once the adapter has identified.
    let received = sockets.received.clone();
    let inject = sockets.inject.clone();
    let url = sockets.url.clone();
    tokio::spawn(async move {
        super::channel_restart_tests::wait_for_frame(&received, 30, "IDENTIFY", |_, frame| {
            super::channel_restart_tests::frame_op(frame) == 2
        })
        .await;
        let _ = inject.send(
            serde_json::json!({
                "op": 0, "t": "READY", "s": 1,
                "d": {
                    "session_id": "sess-e2e",
                    "resume_gateway_url": url,
                    "user": { "id": "bot-e2e" },
                }
            })
            .to_string(),
        );
        let _ = inject.send(
            serde_json::json!({
                "op": 0, "t": "MESSAGE_CREATE", "s": 2,
                "d": {
                    "id": "msg-in-1", "channel_id": CHANNEL,
                    "content": "what is the shape of this pipeline?",
                    "author": { "id": "user-1", "username": "ada", "bot": false },
                }
            })
            .to_string(),
        );
    });

    let record = account_record(ChannelKind::Discord, now);
    let adapter = Arc::new(
        super::adapters::discord::DiscordAdapter::new(&AdapterConfig {
            account: &record,
            secret: BOT_TOKEN.into(),
        })
        .expect("discord adapter")
        .with_base_url(&fixture.base),
    );
    let requests = fixture.seen.clone();
    ProviderWorld {
        kind: ChannelKind::Discord,
        adapter,
        conversation_id: CHANNEL,
        send_marker: "/messages",
        provider_message_id: MESSAGE_ID,
        requests: Box::new(move || requests.lock().unwrap().clone()),
        _sockets: Some(sockets),
    }
}

/// `auth.test`, `apps.connections.open`, a Socket Mode socket, and
/// `chat.postMessage`.
fn slack_world(now: i64) -> ProviderWorld {
    const CHANNEL: &str = "C-E2E";
    const MESSAGE_TS: &str = "3100.007";
    let sockets = super::channel_restart_tests::spawn_ws_fixture(vec![vec![serde_json::json!({
        "type": "hello", "num_connections": 1
    })
    .to_string()]]);
    let socket_url = sockets.url.clone();
    let fixture = HttpFixture::spawn(move |head, _body, _index| {
        if head.contains("/auth.test") {
            return json_response(r#"{"ok":true,"user_id":"UBOT","bot_id":"B1"}"#);
        }
        if head.contains("/apps.connections.open") {
            return json_response(&format!(r#"{{"ok":true,"url":"{socket_url}"}}"#));
        }
        if head.contains("/chat.postMessage") {
            return json_response(&format!(r#"{{"ok":true,"ts":"{MESSAGE_TS}"}}"#));
        }
        json_response(r#"{"ok":true}"#)
    })
    .expect("bind the slack fixture");

    let inject = sockets.inject.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let _ = inject.send(
            serde_json::json!({
                "envelope_id": "env-e2e",
                "type": "events_api",
                "payload": { "event": {
                    "type": "message", "channel": CHANNEL, "channel_type": "channel",
                    "user": "U1", "text": "what is the shape of this pipeline?",
                    "ts": "3100.001",
                }}
            })
            .to_string(),
        );
    });

    let record = account_record(ChannelKind::Slack, now);
    let secret = serde_json::json!({
        "bot_token": "xoxb-e2e",
        "app_token": "xapp-e2e",
    })
    .to_string();
    let adapter = Arc::new(
        super::adapters::slack::SlackAdapter::new(&AdapterConfig {
            account: &record,
            secret,
        })
        .expect("slack adapter")
        .with_base_url(&fixture.base),
    );
    let requests = fixture.seen.clone();
    ProviderWorld {
        kind: ChannelKind::Slack,
        adapter,
        conversation_id: CHANNEL,
        send_marker: "/chat.postMessage",
        provider_message_id: MESSAGE_TS,
        requests: Box::new(move || requests.lock().unwrap().clone()),
        _sockets: Some(sockets),
    }
}

/// The deterministic model transport.
///
/// First turn: one `send_message` tool call, streamed the way an
/// OpenAI-compatible server streams one. Second turn (the model being shown
/// the tool result): a short final message, so the agent loop terminates.
///
/// It never sends anything itself. The only way its tool call becomes a
/// message is the production dispatcher acting on it.
fn model_fixture() -> HttpFixture {
    HttpFixture::spawn(move |head, body, _index| {
        if !head.contains("/chat/completions") {
            return json_response(r#"{"error":"unexpected model route"}"#);
        }
        // A body that already contains a tool result is the second turn.
        let answered = body.contains("\"role\":\"tool\"");
        if answered {
            return sse_response(&[
                serde_json::json!({
                    "choices": [{ "index": 0, "delta": { "content": "sent." } }]
                }),
                serde_json::json!({
                    "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
                }),
            ]);
        }
        let arguments = serde_json::json!({ "text": REPLY_TEXT }).to_string();
        sse_response(&[
            serde_json::json!({
                "choices": [{
                    "index": 0,
                    "delta": { "tool_calls": [{
                        "index": 0,
                        "id": "call_e2e_1",
                        "type": "function",
                        "function": { "name": "send_message", "arguments": arguments },
                    }] },
                }]
            }),
            serde_json::json!({
                "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }]
            }),
        ])
    })
    .expect("bind the model fixture")
}

/// The autonomous coordinator fixture uses the product's real model boundary
/// as well. The planner response deliberately asks for two worktree workers so
/// the test exercises the resident daemon, parallel scheduling, worktree
/// creation/application, integration, configured verification, and structured
/// review in one process-level run.
fn autonomous_model_fixture() -> HttpFixture {
    let planner = serde_json::json!({
        "plan": {
            "planId": "autonomous-e2e-plan",
            "strategy": "PARALLEL_DELEGATE",
            "revision": 1,
            "rationale": "Two independent repository workers feed one integration barrier.",
            "nodes": [
                {
                    "nodeId": "implement-frontend",
                    "taskClass": "implementation",
                    "objective": "Set a.txt to the initial frontend slice A1.",
                    "dependencies": [],
                    "mutationScope": ["a.txt"],
                    "isolation": "worktree",
                    "relevantFiles": ["a.txt"],
                    "capabilities": ["read", "mutate"],
                    "executionPlacement": {"kind": "worktree", "targetId": "local", "nodeId": "implement-frontend"},
                    "executionRequirements": {"needsWorkspaceWrite": true, "needsNetwork": false, "isolation": "worktree"}
                },
                {
                    "nodeId": "implement-backend",
                    "taskClass": "implementation",
                    "objective": "Set b.txt to the intentionally incomplete backend slice B1.",
                    "dependencies": [],
                    "mutationScope": ["b.txt"],
                    "isolation": "worktree",
                    "relevantFiles": ["b.txt"],
                    "capabilities": ["read", "mutate"],
                    "executionPlacement": {"kind": "worktree", "targetId": "local", "nodeId": "implement-backend"},
                    "executionRequirements": {"needsWorkspaceWrite": true, "needsNetwork": false, "isolation": "worktree"}
                },
                {
                    "nodeId": "integrate",
                    "taskClass": "integration",
                    "objective": "Integrate the worker results after scope inspection.",
                    "dependencies": ["implement-frontend", "implement-backend"],
                    "mutationScope": ["workspace"],
                    "isolation": "shared",
                    "relevantFiles": ["a.txt", "b.txt"],
                    "capabilities": ["read", "mutate"],
                    "executionRequirements": {"needsWorkspaceWrite": true, "needsNetwork": false, "isolation": "shared"}
                },
                {
                    "nodeId": "verify",
                    "taskClass": "verification",
                    "objective": "Run the configured verification command.",
                    "dependencies": ["integrate"],
                    "mutationScope": ["workspace"],
                    "isolation": "shared",
                    "relevantFiles": ["a.txt", "b.txt"],
                    "capabilities": ["read", "verify"]
                },
                {
                    "nodeId": "review",
                    "taskClass": "review",
                    "objective": "Return a structured review of the integrated repository.",
                    "dependencies": ["verify"],
                    "mutationScope": ["workspace"],
                    "isolation": "shared",
                    "relevantFiles": ["a.txt", "b.txt"],
                    "capabilities": ["read", "verify"]
                }
            ]
        },
        "acceptanceCriteria": [
            {"id": "verify-files", "description": "The configured verification command passes for both files.", "method": "verification_command", "blocking": true, "provenance": {"kind": "planner", "fragment": "configured verification"}},
            {"id": "review-files", "description": "The structured review passes against the actual diff.", "method": "review", "blocking": true, "provenance": {"kind": "planner", "fragment": "structured review"}},
            {"id": "scope-files", "description": "Workers stay inside their individual file scopes.", "method": "workspace_boundary", "blocking": true, "provenance": {"kind": "planner", "fragment": "file scopes"}}
        ],
        "planningContext": {"relevantFiles": ["a.txt", "b.txt"]},
        "summary": "parallel autonomous coordinator fixture"
    });
    let review = serde_json::json!({
        "verdict": "pass",
        "findings": [],
        "filesReviewed": ["a.txt", "b.txt"],
        "acceptanceCriteria": ["verify-files", "review-files", "scope-files"],
        "securityFindings": [],
        "testCoverageFindings": []
    });
    let sent_mutations = Arc::new(std::sync::Mutex::new(
        std::collections::HashSet::<String>::new(),
    ));
    HttpFixture::spawn(move |head, body, _index| {
        if !head.contains("/chat/completions") {
            return json_response(r#"{"error":"unexpected autonomous model route"}"#);
        }
        let prompt_text = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|request| request["messages"].as_array().cloned())
            .map(|messages| messages.iter().filter_map(|message| message["content"].as_str()).collect::<Vec<_>>().join("\n"))
            .unwrap_or_else(|| body.to_string());
        let implementation = prompt_text.contains("Universal AutonomousTask phase: implementation");
        let repair = prompt_text.contains("Diagnose and repair");
        let current_node_contract = prompt_text
            .rsplit("Frozen node contract (do not change it):")
            .next()
            .unwrap_or(&prompt_text);
        let mut path = if current_node_contract.contains("implement-backend")
            || current_node_contract.contains("Set b.txt")
            || (implementation && current_node_contract.contains("b.txt"))
        {
            "b.txt"
        } else {
            "a.txt"
        };
        if implementation {
            let mut sent = sent_mutations.lock().expect("mutation fixture lock");
            if repair && sent.contains("b.txt:repair") {
                path = "a.txt";
            }
            let key = format!("{path}:{}", if repair { "repair" } else { "initial" });
            if sent.insert(key.clone()) {
                let content = if repair { if path == "a.txt" { "A2\n" } else { "B2\n" } } else if path == "a.txt" { "A1\n" } else { "B1\n" };
                let arguments = serde_json::json!({ "path": path, "content": content }).to_string();
                return sse_response(&[
                    serde_json::json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": format!("call-{key}"), "type": "function", "function": {"name": "write_file", "arguments": arguments}}]}}]}),
                    serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
                ]);
            }
        }
        let content = if prompt_text.contains("phase: review") || prompt_text.contains("bounded 'review' phase") {
            review.to_string()
        } else if prompt_text.contains("phase: planner") {
            planner.to_string()
        } else {
            "autonomous phase completed".to_string()
        };
        sse_response(&[
            serde_json::json!({"choices": [{"index": 0, "delta": {"content": content}}]}),
            serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        ])
    })
    .expect("bind the autonomous model fixture")
}

fn autonomous_e2e_git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("start git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn run_autonomous_coordinator_end_to_end(root: &Path) {
    if !isolation_is_real(root) {
        println!(
            "{SKIPPED} on this platform: autonomous coordinator profile isolation is unavailable"
        );
        return;
    }
    let model = autonomous_model_fixture();
    let workspace = root.join("autonomous-workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    autonomous_e2e_git(&workspace, &["init", "-q"]);
    autonomous_e2e_git(&workspace, &["config", "user.email", "e2e@example.test"]);
    autonomous_e2e_git(&workspace, &["config", "user.name", "Autonomous E2E"]);
    std::fs::write(workspace.join("a.txt"), "A0\n").expect("a.txt");
    std::fs::write(workspace.join("b.txt"), "B0\n").expect("b.txt");
    autonomous_e2e_git(&workspace, &["add", "a.txt", "b.txt"]);
    autonomous_e2e_git(&workspace, &["commit", "-q", "-m", "fixture baseline"]);

    let roots = little_monkey_lib::app_paths::ensure_agent_config_roots().expect("config roots");
    let data_dir = little_monkey_lib::app_paths::data_dir().expect("data dir");
    std::fs::create_dir_all(&data_dir).expect("data directory");
    let workspace_key = workspace.canonicalize().expect("canonical workspace");
    let verify_config = serde_json::json!({
        workspace_key.to_string_lossy(): {
            "commands": [{
                "id": "autonomous-e2e-verify",
                "label": "Autonomous E2E verification",
                "command": "git diff --check -- && git grep -F -e \"A2\" -- a.txt && git grep -F -e \"B2\" -- b.txt",
                "kind": "custom",
                "enabled": true,
                "timeoutSecs": 30
            }]
        }
    });
    std::fs::write(
        data_dir.join("verify_configs.json"),
        serde_json::to_vec_pretty(&verify_config).expect("verify config JSON"),
    )
    .expect("verify config");
    assert_eq!(
        crate::verify_cli::enabled_commands_at(
            &data_dir.join("verify_configs.json"),
            &workspace_key
        )
        .len(),
        1,
        "the real CLI verification config was not visible to the coordinator"
    );
    let paths = DaemonPaths::under(&roots.legacy);
    paths.ensure().expect("daemon paths");
    let config = DaemonConfig::default();
    config.save(&paths).expect("daemon config");
    let cli = std::env::var(CLI_ENV).expect("real monkey-cli path");
    let target = format!("local-url:{}|autonomous-e2e", model.base);
    let started = std::process::Command::new(cli)
        .args([
            "task",
            "start",
            "Run the autonomous coordinator fixture",
            "--target",
            &target,
            "--workspace",
            workspace.to_str().expect("workspace UTF-8"),
            "--json",
        ])
        .current_dir(&workspace)
        .output()
        .expect("start autonomous task process");
    assert!(
        started.status.success(),
        "task start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let queued: serde_json::Value = serde_json::from_slice(&started.stdout).expect("queued JSON");
    let job_id = queued["job_id"]
        .as_str()
        .expect("queued job id")
        .to_string();
    let run_id = queued["run_id"]
        .as_str()
        .expect("queued run id")
        .to_string();

    let mut engine = DaemonEngine::new(
        DaemonStore::open(&paths).expect("engine store"),
        SharedLedger::open(&paths.ledger_db).expect("engine ledger"),
        paths.clone(),
        config,
        RealProcessAdapter::current().expect("process adapter"),
        SilentNotifier,
        SystemClock,
        "autonomous-e2e-daemon".to_string(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    let terminal = loop {
        engine.tick().expect("daemon tick");
        let state = DaemonStore::open(&paths)
            .expect("state read")
            .get_job(&job_id)
            .expect("job read")
            .expect("job")
            .state;
        if matches!(
            state,
            JobState::Succeeded | JobState::Failed | JobState::Cancelled
        ) {
            break state;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "autonomous task did not finish: {state:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    };
    let diagnostic_events = SharedLedger::open(&paths.ledger_db)
        .expect("diagnostic ledger")
        .run_ledger()
        .expect("diagnostic run ledger")
        .load_events(&run_id, 0, 1_000)
        .expect("diagnostic events");
    assert_eq!(
        terminal,
        JobState::Succeeded,
        "autonomous task failed; log: {}\nevents: {}",
        std::fs::read_to_string(paths.logs.join(format!("{job_id}.log"))).unwrap_or_default(),
        serde_json::to_string(&diagnostic_events).unwrap_or_default()
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).expect("a.txt result"),
        "A2\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("b.txt")).expect("b.txt result"),
        "B2\n"
    );
    let events = SharedLedger::open(&paths.ledger_db)
        .expect("ledger")
        .run_ledger()
        .expect("run ledger")
        .load_events(&run_id, 0, 1_000)
        .expect("run events");
    let rendered = serde_json::to_string(&events).expect("event JSON");
    assert!(
        rendered.contains("\"parallel\":true"),
        "parallel workers missing: {rendered}"
    );
    assert!(
        rendered.contains("\"isolation\":\"worktree\""),
        "worktree workers missing: {rendered}"
    );
    assert!(
        rendered.contains("\"verification_evidence\""),
        "verification evidence missing: {rendered}"
    );
    assert!(
        rendered.contains("\"authoritative\":true"),
        "authoritative evidence missing: {rendered}"
    );
    assert!(
        rendered.contains("\"review_evidence\""),
        "review evidence missing: {rendered}"
    );
    assert!(
        rendered.contains("\"verdict\":\"pass\""),
        "structured review missing: {rendered}"
    );
    assert!(
        rendered.contains("\"repair_of\""),
        "verification did not trigger bounded repair: {rendered}"
    );
    let worker_mutations = events
        .iter()
        .filter_map(|event| {
            if let RunEvent::TaskEvent {
                event_type,
                payload,
                ..
            } = &event.event
            {
                (event_type == "node_mutation"
                    && payload.get("parallel") != Some(&serde_json::Value::Bool(true)))
                .then_some(payload)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let parallel_mutations = events
        .iter()
        .filter_map(|event| {
            if let RunEvent::TaskEvent {
                event_type,
                payload,
                ..
            } = &event.event
            {
                (event_type == "node_mutation" && payload.get("integration_revision").is_some())
                    .then_some(payload)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert!(
        parallel_mutations.len() >= 2,
        "isolated worker mutation evidence missing: {rendered}"
    );
    assert!(
        parallel_mutations
            .iter()
            .all(|payload| payload["before_revision"].is_string()
                && payload["patch_digest"]
                    .as_str()
                    .is_some_and(|digest| !digest.is_empty())
                && payload["changed_files"].as_array().is_some()),
        "worker evidence is not revision-bound: {parallel_mutations:?}"
    );
    assert!(
        !worker_mutations.is_empty(),
        "mutation-bearing worker events missing: {rendered}"
    );
    assert!(
        model.count() >= 10,
        "coordinator did not run planner, workers, repair, integration, verify, and review"
    );
}

// ---------------------------------------------------------------------------
// The isolated profile
// ---------------------------------------------------------------------------

fn write_private(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create directory");
    }
    std::fs::write(path, contents).expect("write file");
}

/// The recipe the route runs: an OpenAI-compatible origin and nothing else.
/// `auto` is the mode a conversational route realistically uses; it does not
/// auto-approve `send_message`, which is the point — the approval below is the
/// operator's, made through the same durable path the phone app uses.
pub(crate) fn write_recipe(authored: &Path, workspace: &Path, model_base: &str) {
    let recipe = serde_json::json!({
        "version": 1,
        "name": RECIPE,
        "target": { "local_url": model_base, "model": "e2e-fixture" },
        "workspace": workspace.to_string_lossy(),
        "permission_mode": "auto",
        "prompt": "{{message}}",
        "params": { "message": null },
        "max_iterations": 4,
        "timeout_seconds": 180,
    });
    write_private(
        &authored.join("recipes").join(format!("{RECIPE}.json")),
        &serde_json::to_string_pretty(&recipe).expect("recipe json"),
    );
}

pub(crate) fn seed_channel(store: &mut DaemonStore, kind: ChannelKind, now: i64) {
    store
        .upsert_channel_account(&account_record(kind, now))
        .expect("account");
    store
        .insert_channel_route(&ChannelRoute {
            route_id: format!("route-{ACCOUNT_ID}"),
            scope: RouteScope::account(ACCOUNT_ID),
            target: RouteTarget::new(RECIPE),
            enabled: true,
            created_at_ms: now,
            updated_at_ms: now,
        })
        .expect("route");
    // A machine whose people were already told which model answers — the
    // steady state. The first-contact notice is exercised in `channel_commands`
    // and `channel_ingress`, not by every provider's path through here.
    super::channel_commands::suppress_first_run_notice(store).expect("told");
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

pub(crate) struct SilentNotifier;

impl super::engine::NotificationAdapter for SilentNotifier {
    fn notify(&self, _notification: &super::engine::DaemonNotification) -> Result<(), String> {
        Ok(())
    }
}

/// Decides any approval the run is waiting on, the way the phone app does:
/// read the pending request from the ledger, emit a `PermissionDecided` event
/// carrying the same operation digest. Returns how many it answered.
pub(crate) fn approve_pending(paths: &DaemonPaths, run_id: &str) -> usize {
    let Ok(shared) = SharedLedger::open(&paths.ledger_db) else {
        return 0;
    };
    let Ok(pending) = shared.pending_approvals(run_id) else {
        return 0;
    };
    let mut answered = 0;
    for approval in pending {
        let Ok(ledger) = shared.run_ledger() else {
            continue;
        };
        let Ok(recorder) = crate::durable_run::DurableRunRecorder::attach(
            ledger,
            run_id,
            "e2e-operator".to_string(),
            little_monkey_lib::run_protocol::ClientIdentity {
                client_id: "e2e-operator".to_string(),
                instance_id: "e2e-operator".to_string(),
                kind: little_monkey_lib::run_protocol::ClientKind::RemoteRunner,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        ) else {
            continue;
        };
        if recorder
            .emit(
                little_monkey_lib::run_protocol::RunEvent::PermissionDecided {
                    request_id: approval.request_id.clone(),
                    operation_sha256: approval.operation_sha256.clone(),
                    decision: little_monkey_lib::run_protocol::PermissionDecision::AllowOnce,
                    decided_by: recorder.client_identity(),
                },
            )
            .is_ok()
        {
            answered += 1;
        }
    }
    answered
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

/// A real inbound Telegram message becomes a real daemon run, the real agent
/// loop calls the real `send_message` tool, and the reply leaves through the
/// production adapter.
#[test]
fn a_telegram_message_becomes_an_agent_reply_end_to_end() {
    in_isolated_process(
        "channel_agent_e2e",
        "a_telegram_message_becomes_an_agent_reply_end_to_end",
        |root| {
            Box::pin(async move {
                let world = telegram_world(now_ms());
                run_end_to_end(&root, world).await;
            })
        },
    );
}

/// The same architecture, reached over the Discord Gateway: HELLO, IDENTIFY,
/// READY, a MESSAGE_CREATE dispatch, and a REST create-message on the way out.
#[test]
fn a_discord_message_becomes_an_agent_reply_end_to_end() {
    in_isolated_process(
        "channel_agent_e2e",
        "a_discord_message_becomes_an_agent_reply_end_to_end",
        |root| {
            Box::pin(async move {
                let world = discord_world(now_ms());
                run_end_to_end(&root, world).await;
            })
        },
    );
}

/// The same architecture, reached over Slack Socket Mode: `auth.test`,
/// `apps.connections.open`, an `events_api` envelope, and `chat.postMessage`
/// on the way out.
#[test]
fn a_slack_message_becomes_an_agent_reply_end_to_end() {
    in_isolated_process(
        "channel_agent_e2e",
        "a_slack_message_becomes_an_agent_reply_end_to_end",
        |root| {
            Box::pin(async move {
                let world = slack_world(now_ms());
                run_end_to_end(&root, world).await;
            })
        },
    );
}

/// The real CLI start path queues an autonomous coordinator and the resident
/// daemon executes the actual process, worktree, integration, verification,
/// and structured-review boundaries against a temporary Git repository.
#[test]
fn autonomous_coordinator_runs_through_the_resident_daemon_end_to_end() {
    in_isolated_process(
        "channel_agent_e2e",
        "autonomous_coordinator_runs_through_the_resident_daemon_end_to_end",
        |root| Box::pin(async move { run_autonomous_coordinator_end_to_end(&root).await }),
    );
}

/// Runs `body` in a process whose home is a fresh directory, relaunching this
/// test binary once to get there. See the module doc for why.
pub(crate) fn in_isolated_process(
    module: &'static str,
    name: &'static str,
    body: impl FnOnce(PathBuf) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>,
) {
    let Some(root) = child_root() else {
        // One at a time. Each of these runs a daemon, two child processes and
        // several loopback servers; three at once on a shared machine starves
        // the long-poll transports rather than testing them.
        static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
        let _guard = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        relaunch(module, name);
        return;
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(body(root));
}

/// Whether the isolated home actually took effect.
///
/// This test creates a channel account, a daemon queue and durable runs, and
/// a real daemon child resolves all of that from the platform's app-data
/// directory. That is only redirectable where the platform reads it out of
/// the environment. Where it does not — Windows resolves it through the Known
/// Folder API, which no environment variable reaches — running this would
/// write into the operator's real profile, so it does not run there.
pub(crate) fn isolation_is_real(root: &Path) -> bool {
    little_monkey_lib::app_paths::base_data_dir().is_some_and(|data| data.starts_with(root))
}

/// What one accepted turn produced once the daemon and the agent were done
/// with it.
pub(crate) struct AgentTurnProof {
    /// The provider's own id for the reply, read off the durable outbound
    /// event rather than off the send call.
    pub(crate) provider_message_id: String,
    /// The run's durable events, rendered, so a caller's failing assertion can
    /// quote what the agent really did.
    pub(crate) run_events_json: String,
}

/// The provider-independent middle of an acceptance run.
///
/// Given a turn that is already queued — however it arrived, from a protocol
/// fixture or from a real account — this runs the resident daemon over it,
/// proves the reply came from the agent rather than from the test, and drains
/// the outbox through the adapter the caller supplied.
///
/// Factored out because it is the half no provider gets to influence. The
/// fixture tests in this module and the live-account tests in
/// [`super::live_agent_e2e`] run exactly this code; what a provider supplies
/// is only how the message arrived and how the reply is observed on the far
/// side.
pub(crate) async fn execute_turn_through_the_daemon(
    paths: &DaemonPaths,
    config: &DaemonConfig,
    account_id: &str,
    job_id: &str,
    run_id: &str,
    adapter: &Arc<dyn ChannelAdapter>,
    model: &HttpFixture,
) -> AgentTurnProof {
    // ---- the daemon executes it. Production engine, production process
    // adapter: the child really is another monkey-cli running `task run`.
    let mut engine = DaemonEngine::new(
        DaemonStore::open(paths).expect("engine store"),
        SharedLedger::open(&paths.ledger_db).expect("engine ledger"),
        paths.clone(),
        config.clone(),
        RealProcessAdapter::current().expect("process adapter"),
        SilentNotifier,
        SystemClock,
        "e2e-daemon".to_string(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    let terminal = loop {
        engine.tick().expect("engine tick");
        // The operator's side of the external-mutation gate. A channel reply
        // leaves the machine, so the run asks; this stands in for the phone.
        approve_pending(paths, run_id);
        let state = DaemonStore::open(paths)
            .expect("state read")
            .get_job(job_id)
            .expect("job read")
            .expect("job")
            .state;
        if matches!(
            state,
            JobState::Succeeded | JobState::Failed | JobState::Cancelled
        ) {
            break state;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the daemon run never reached a terminal state (still {state:?}); \
             daemon log: {}",
            std::fs::read_to_string(paths.logs.join(format!("{job_id}.log"))).unwrap_or_default()
        );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    };
    assert_eq!(
        terminal,
        JobState::Succeeded,
        "the daemon run failed; log: {}",
        std::fs::read_to_string(paths.logs.join(format!("{job_id}.log"))).unwrap_or_default()
    );

    // ---- what the run actually did. The tool call has to have come from the
    // model and been dispatched by the agent, not been written here.
    let shared = SharedLedger::open(&paths.ledger_db).expect("ledger");
    let events = shared
        .run_ledger()
        .expect("run ledger")
        .load_events(run_id, 0, 500)
        .expect("run events");
    let rendered = serde_json::to_string(&events).expect("run events json");
    // The agent offered the tool. This is read off the model's own request:
    // whatever the fixture answers, the schema in front of it came from the
    // production tool builder deciding this run could reach somewhere.
    let asked = model.requests();
    assert!(!asked.is_empty(), "the agent never called the model");
    assert!(
        asked
            .iter()
            .any(|request| request.contains(r#""name":"send_message""#)),
        "send_message was not in the tool schema the agent offered: {asked:?}"
    );
    // The agent dispatched the call it got back, and the dispatch succeeded.
    // A test that wrote an outbox row by hand would leave this absent.
    let dispatched = events.iter().any(|event| {
        let json = serde_json::to_string(event).unwrap_or_default();
        json.contains("\"type\":\"tool_proposed\"") && json.contains("\"send_message\"")
    });
    assert!(
        dispatched,
        "no send_message tool call reached the dispatcher: {rendered}"
    );
    assert!(
        !rendered.contains(r#""outcome":"failed""#),
        "the tool call was dispatched and refused: {rendered}\n--- job log ---\n{}",
        std::fs::read_to_string(paths.logs.join(format!("{job_id}.log"))).unwrap_or_default()
    );
    assert!(
        model.count() >= 2,
        "the agent loop did not come back to the model with the tool result \
         ({} request(s)) — the tool call was not dispatched",
        model.count()
    );

    // ---- the outbox row the tool wrote, drained by the production worker
    // into the production adapter.
    let mut store = DaemonStore::open(paths).expect("store reopen");
    let mut adapters: BTreeMap<String, Arc<dyn ChannelAdapter>> = BTreeMap::new();
    adapters.insert(account_id.to_string(), adapter.clone());
    let drained = drain_outbox_once(&mut store, &adapters, now_ms())
        .await
        .expect("drain");
    assert_eq!(
        drained.sent, 1,
        "the agent's reply did not leave the outbox: {drained:?}\n--- run events ---\n{rendered}\n--- job log ---\n{}",
        std::fs::read_to_string(paths.logs.join(format!("{job_id}.log"))).unwrap_or_default()
    );

    let outbound = store
        .recent_channel_events(account_id, 50)
        .expect("events")
        .into_iter()
        .find(|event| event.direction == EventDirection::Outbound)
        .expect("a durable outbound event");
    AgentTurnProof {
        provider_message_id: outbound.provider_event_id,
        run_events_json: rendered,
    }
}

async fn run_end_to_end(root: &Path, world: ProviderWorld) {
    if !isolation_is_real(root) {
        println!(
            "{SKIPPED} on this platform: the app-data directory is not resolved from the \
             environment, so the run could not be kept out of the real profile"
        );
        return;
    }
    let model = model_fixture();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");

    let roots = little_monkey_lib::app_paths::ensure_agent_config_roots().expect("config roots");
    write_recipe(&roots.authored, &workspace, &model.base);
    let paths = DaemonPaths::under(&roots.legacy);
    paths.ensure().expect("daemon paths");
    let config = DaemonConfig::default();
    config.save(&paths).expect("daemon config");

    let now = now_ms();
    let mut store = DaemonStore::open(&paths).expect("daemon store");
    seed_channel(&mut store, world.kind, now);

    // ---- inbound: production adapter → durable event → production ingress
    // → the real queue, which writes the immutable snapshot and registers the
    // durable run through an actual monkey-cli child.
    let adapter = world.adapter.clone();
    let queue = DaemonChannelQueue::new(paths.clone());
    let report = poll_account_once(&mut store, &queue, ACCOUNT_ID, adapter.as_ref(), now)
        .await
        .expect("poll");
    assert_eq!(report.accepted, 1, "the inbound message was not accepted");

    let event = store
        .recent_channel_events(ACCOUNT_ID, 10)
        .expect("events")
        .into_iter()
        .find(|event| event.direction == EventDirection::Inbound)
        .expect("a durable inbound event");
    assert!(
        event.ingress_id.is_some(),
        "an accepted event with no durable turn behind it"
    );
    let job_id = event.job_id.clone().expect("a real job id");
    let job = store
        .get_job(&job_id)
        .expect("job read")
        .expect("the daemon queue has the job");
    let run_id = job.run_id.clone().expect("a durable run id");
    assert_eq!(job.state, JobState::Queued, "the job was not queued");
    // The authority the child will read back out of the durable turn. Checked
    // here so a run that later refuses to send names the reason.
    assert_eq!(
        store.ingress_reply_grant_for_job(&job_id).expect("grant"),
        Some(true),
        "the frozen route did not grant this turn a reply"
    );
    assert!(
        store
            .channel_origin_for_job(&job_id)
            .expect("origin read")
            .is_some(),
        "the job has no channel origin for send_message to answer"
    );
    // The authority the run's own process derives, derived here in the same
    // environment it will run in. If this is not a reply grant the agent is
    // never offered `send_message` at all, and the failure further down would
    // say only that nothing was sent.
    std::env::set_var(super::channel_tool::JOB_ID_ENV, &job_id);
    let authority = super::channel_tool::send_authority(false, None);
    assert!(
        authority.reply,
        "the run's process cannot resolve its own reply grant: {authority:?} \
         (daemon paths {:?} vs {:?})",
        DaemonPaths::resolve().map(|resolved| resolved.root),
        paths.root
    );

    // ---- the provider-independent middle: the daemon runs it, the agent
    // answers, and the reply leaves through the production adapter. The
    // live-account acceptance tests reach this same function.
    let proof = execute_turn_through_the_daemon(
        &paths, &config, ACCOUNT_ID, &job_id, &run_id, &adapter, &model,
    )
    .await;
    let store = DaemonStore::open(&paths).expect("store reopen");

    // ---- and what the provider received.
    let sent = (world.requests)()
        .into_iter()
        .find(|request| request.contains(world.send_marker))
        .unwrap_or_else(|| {
            panic!(
                "the provider never received a {} request; it saw {:?}\n--- run events ---\n{}",
                world.send_marker,
                (world.requests)(),
                proof.run_events_json
            )
        });
    assert!(
        sent.contains(REPLY_TEXT),
        "the provider got a reply the model never asked for: {sent}"
    );
    assert!(
        sent.contains(world.conversation_id),
        "the reply did not go back to the conversation it came from: {sent}"
    );
    // One of everything: one message in, one turn, one run, one reply out.
    let all = store.recent_channel_events(ACCOUNT_ID, 50).expect("events");
    let inbound: Vec<_> = all
        .iter()
        .filter(|event| event.direction == EventDirection::Inbound)
        .collect();
    let outbound: Vec<_> = all
        .iter()
        .filter(|event| event.direction == EventDirection::Outbound)
        .collect();
    assert_eq!(inbound.len(), 1, "more than one inbound event: {inbound:?}");
    assert_eq!(
        outbound.len(),
        1,
        "the agent's one reply became {} outbound events",
        outbound.len()
    );
    assert_eq!(
        outbound[0].provider_event_id, world.provider_message_id,
        "the provider's own message id was not captured"
    );
    assert_eq!(
        (world.requests)()
            .iter()
            .filter(|request| request.contains(world.send_marker))
            .count(),
        1,
        "the reply reached the provider more than once"
    );
}

// ---------------------------------------------------------------------------
// Relaunching this test with an isolated home
// ---------------------------------------------------------------------------

fn child_root() -> Option<PathBuf> {
    std::env::var_os(CHILD_ENV)?;
    Some(PathBuf::from(std::env::var_os(ROOT_ENV)?))
}

/// Runs `name` again in a child copy of this test binary whose `HOME` (and
/// authored-config override) point at a fresh directory, so the isolated
/// profile is real for every process in the tree and invisible to every other
/// test in this one.
fn relaunch(module: &str, name: &str) {
    // Named per test as well as per process: three of these can start in the
    // same millisecond, and a shared root would have one delete another's.
    let root = std::env::temp_dir().join(format!(
        "lm-channel-e2e-{}-{name}-{}",
        std::process::id(),
        now_ms()
    ));
    std::fs::create_dir_all(&root).expect("isolated root");
    restrict(&root);
    let home = root.join("agent-home");
    std::fs::create_dir_all(&home).expect("agent home");
    restrict(&home);

    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .arg("--exact")
        .arg(format!("daemon::{module}::{name}"))
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(CHILD_ENV, "1")
        .env(ROOT_ENV, &root)
        // Resolved here, once, and inherited by every daemon child below.
        .env(CLI_ENV, locate_cli_binary())
        // Everything the app-data directory can be resolved from. The child
        // checks that it worked before touching anything — see
        // `isolation_is_real`.
        .env("HOME", &root)
        .env("XDG_DATA_HOME", root.join("data"))
        .env(little_monkey_lib::app_paths::AGENT_HOME_ENV, &home)
        .env("RUST_BACKTRACE", "1")
        .output()
        .expect("relaunch the end-to-end test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        output.status.success(),
        "the isolated end-to-end run failed\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        stdout.contains("1 passed"),
        "the isolated end-to-end run did not report a passing test\n{stdout}\n{stderr}"
    );
    // A platform that could not be isolated says so here rather than quietly
    // reporting a pass that proved nothing.
    if let Some(reason) = stdout.lines().find(|line| line.contains(SKIPPED)) {
        eprintln!("{name}: {reason}");
    }
}

#[cfg(unix)]
fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).expect("restrict");
}

#[cfg(not(unix))]
fn restrict(_path: &Path) {}
