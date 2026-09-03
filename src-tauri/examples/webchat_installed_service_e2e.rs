//! A real browser exchange -> installed resident daemon -> agent -> a real
//! reply read back off the page.
//!
//! This is deliberately a black-box harness, and it is the one of the three
//! newest channels that needs **nothing of the operator's**: no account, no
//! network, no hardware. Little Monkey is configured only through `monkey-cli`,
//! the real user service is installed, and the visitor is an ordinary HTTPS
//! client pinned to the daemon's own self-signed loopback certificate — which
//! `openssl` mints here exactly as `daemon/peer_live.rs` mints one.
//!
//! The only deterministic component is the OpenAI-compatible model origin. It
//! is the same `target.local_url` seam a real recipe uses; it cannot write the
//! outbox and it cannot answer an HTTP request on the chat page. The reply
//! exists only if the installed daemon queued a real run and the production
//! agent dispatched `send_message`.
//!
//! What it proves, in order: an unknown visitor's first message is answered
//! with a **pairing code** and nothing runs; the operator approving that sender
//! through the CLI is what changes the answer; and the second message becomes a
//! durable turn whose agent-produced reply the same visitor reads back — and
//! which a *second* visitor of the same account cannot.
//!
//! What it does not prove is a browser on another machine reaching a
//! non-loopback bind with a certificate it trusts. That needs a certificate
//! authority and a network the operator owns, and stays theirs to verify.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROFILE_ENV: &str = "LITTLE_MONKEY_PROFILE";
const REQUIRE_ENV: &str = "LITTLE_MONKEY_REQUIRE_WEBCHAT_INSTALLED_SERVICE_E2E";
const RECIPE: &str = "webchat-installed-service-e2e";
const REPLY_PREFIX: &str = "little-monkey webchat installed-service reply";
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);
const SERVICE_WAIT: Duration = Duration::from_secs(120);
const PAGE_WAIT: Duration = Duration::from_secs(240);
const VISITOR_HEADER: &str = "x-webchat-visitor";

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"))
}

fn cli() -> PathBuf {
    target_dir().join("debug").join(if cfg!(windows) {
        "monkey-cli.exe"
    } else {
        "monkey-cli"
    })
}

fn unique() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn output_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn bounded_output(mut child: std::process::Child, label: &str) -> Result<Output, String> {
    let deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{label} was killed after {}s without finishing",
                    CHILD_TIMEOUT.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => return Err(format!("failed waiting for {label}: {error}")),
        }
    }
    child
        .wait_with_output()
        .map_err(|error| format!("failed to collect {label} output: {error}"))
}

fn run_cli(profile: Option<&str>, args: &[&str]) -> Result<Output, String> {
    let binary = cli();
    if !binary.is_file() {
        return Err(format!(
            "prebuilt monkey-cli is missing at {}; run `cargo build --bin monkey-cli` first",
            binary.display()
        ));
    }
    if std::fs::metadata(&binary)
        .map_err(|error| format!("could not stat {}: {error}", binary.display()))?
        .len()
        == 0
    {
        return Err(format!(
            "{} is the zero-byte Tauri bootstrap placeholder",
            binary.display()
        ));
    }

    let mut command = Command::new(binary);
    command.args(args);
    if let Some(profile) = profile {
        command.env(PROFILE_ENV, profile);
    } else {
        command.env_remove(PROFILE_ENV);
    }
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start monkey-cli {args:?}: {error}"))?;
    bounded_output(child, &format!("monkey-cli {args:?}"))
}

fn require_cli(profile: Option<&str>, args: &[&str]) -> Result<Output, String> {
    let output = run_cli(profile, args)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(format!(
            "monkey-cli {args:?} failed with {}\n{}",
            output.status,
            output_text(&output)
        ))
    }
}

fn create_profile() -> Result<String, String> {
    let name = format!("WebChat installed-service E2E {}", unique());
    let output = require_cli(None, &["profiles", "create", &name, "--json"])?;
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        format!(
            "profile JSON was invalid: {error}\n{}",
            output_text(&output)
        )
    })?;
    payload
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "profile JSON had no id: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
}

fn add_webchat_account(profile: &str) -> Result<String, String> {
    let label = format!("webchat-e2e-{}", unique());
    let output = require_cli(
        Some(profile),
        &["channels", "add", "webchat", &label, "--json"],
    )?;
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        format!(
            "account JSON was invalid: {error}\n{}",
            output_text(&output)
        )
    })?;
    payload
        .get("account_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "account JSON had no account_id: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
}

/// A self-signed loopback certificate, minted by the `openssl` CLI exactly as
/// `daemon/peer_live.rs` mints one. `None` means openssl is absent, which is a
/// skip rather than a failure.
fn self_signed_certificate(directory: &Path) -> Option<(PathBuf, PathBuf)> {
    let certificate = directory.join("cert.pem");
    let key = directory.join("key.pem");
    let output = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=127.0.0.1",
            "-addext",
            "subjectAltName=IP:127.0.0.1",
            // Deliberately not a CA: rustls refuses a certificate marked as one
            // when it is presented as the end entity, which is exactly what a
            // self-signed host certificate is.
            "-addext",
            "basicConstraints=critical,CA:FALSE",
            "-keyout",
        ])
        .arg(&key)
        .arg("-out")
        .arg(&certificate)
        .output()
        .ok()?;
    if !output.status.success() {
        eprintln!(
            "openssl could not mint a test certificate: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    Some((certificate, key))
}

fn free_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|error| format!("reserve a port: {error}"))?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|error| format!("read the reserved port: {error}"))
}

struct ModelFixture {
    base: String,
    seen: Arc<Mutex<Vec<String>>>,
}

impl ModelFixture {
    fn spawn(marker: String) -> Result<Self, String> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .map_err(|error| format!("bind model fixture: {error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| format!("model fixture address: {error}"))?
            .port();
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = seen.clone();
        std::thread::spawn(move || {
            for incoming in listener.incoming() {
                let Ok(mut stream) = incoming else { break };
                let Some((head, body)) = read_http_request(&mut stream) else {
                    continue;
                };
                log.lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(format!("{head}\n{body}"));

                let response = if !head.contains("/chat/completions") {
                    json_response(r#"{"error":"unexpected model route"}"#)
                } else if body.contains("\"role\":\"tool\"") {
                    sse_response(&[
                        serde_json::json!({
                            "choices": [{ "index": 0, "delta": { "content": "sent" } }]
                        }),
                        serde_json::json!({
                            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
                        }),
                    ])
                } else {
                    let arguments = serde_json::json!({
                        "text": format!("{REPLY_PREFIX} {marker}")
                    })
                    .to_string();
                    sse_response(&[
                        serde_json::json!({
                            "choices": [{
                                "index": 0,
                                "delta": { "tool_calls": [{
                                    "index": 0,
                                    "id": "call_webchat_installed_1",
                                    "type": "function",
                                    "function": {
                                        "name": "send_message",
                                        "arguments": arguments
                                    }
                                }] }
                            }]
                        }),
                        serde_json::json!({
                            "choices": [{
                                "index": 0,
                                "delta": {},
                                "finish_reason": "tool_calls"
                            }]
                        }),
                    ])
                };
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Ok(Self {
            base: format!("http://127.0.0.1:{port}"),
            seen,
        })
    }

    fn requests(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

fn read_http_request(stream: &mut TcpStream) -> Option<(String, String)> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
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

fn json_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn sse_response(frames: &[serde_json::Value]) -> String {
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

fn with_profile_roots(
    profile: &str,
) -> Result<little_monkey_lib::app_paths::AgentConfigRoots, String> {
    let previous = std::env::var_os(PROFILE_ENV);
    std::env::set_var(PROFILE_ENV, profile);
    let result = little_monkey_lib::app_paths::ensure_agent_config_roots()
        .map_err(|error| format!("resolve profile config roots: {error}"));
    match previous {
        Some(value) => std::env::set_var(PROFILE_ENV, value),
        None => std::env::remove_var(PROFILE_ENV),
    }
    result
}

fn write_recipe(profile: &str, workspace: &Path, model_base: &str) -> Result<(), String> {
    let roots = with_profile_roots(profile)?;
    let recipes = roots.authored.join("recipes");
    std::fs::create_dir_all(&recipes)
        .map_err(|error| format!("create recipe directory {}: {error}", recipes.display()))?;
    let recipe = serde_json::json!({
        "version": 1,
        "name": RECIPE,
        "target": { "local_url": model_base, "model": "webchat-e2e-fixture" },
        "workspace": workspace.to_string_lossy(),
        "permission_mode": "auto",
        "prompt": "{{message}}",
        "params": { "message": null },
        "max_iterations": 4,
        "timeout_seconds": 180,
    });
    let path = recipes.join(format!("{RECIPE}.json"));
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&recipe).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("write recipe {}: {error}", path.display()))
}

/// The pid of the installed resident service, once it is running and its
/// heartbeat is fresh.
fn wait_for_service_pid(profile: &str, deadline: Instant) -> Result<u64, String> {
    let mut last_status = String::new();
    while Instant::now() < deadline {
        if let Ok(status_output) = run_cli(Some(profile), &["daemon", "status", "--json"]) {
            if status_output.status.success() {
                if let Ok(status) =
                    serde_json::from_slice::<serde_json::Value>(&status_output.stdout)
                {
                    last_status = status.to_string();
                    let running = status
                        .get("service_running")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let heartbeat = status
                        .get("heartbeat_fresh")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let pid = status
                        .get("pid")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or_default();
                    if running && heartbeat && pid != u64::from(std::process::id()) {
                        return Ok(pid);
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "installed service never reported a live pid of its own within {}s\nlast daemon status: {last_status}",
        SERVICE_WAIT.as_secs()
    ))
}

/// Whether the installed daemon has this account connected **now**.
///
/// `since_ms` is what makes this an answer rather than an echo: account health
/// is a stored row, and after a restart it still says `connected` until the new
/// process writes its own verdict. For a served surface that verdict is "there
/// is a remote host enabled to serve the page on", written by the running
/// service's own probe — which is exactly the thing this harness needs to be
/// true before a visitor knocks.
fn wait_for_account_connected(
    profile: &str,
    account_id: &str,
    since_ms: u64,
    deadline: Instant,
) -> Result<(), String> {
    let mut last_health = "unknown".to_string();
    let mut last_probe: Option<u64> = None;
    while Instant::now() < deadline {
        let listed = require_cli(Some(profile), &["channels", "list", "--json"])?;
        let payload: serde_json::Value = serde_json::from_slice(&listed.stdout)
            .map_err(|error| format!("channels list JSON was invalid: {error}"))?;
        let account = payload
            .get("accounts")
            .and_then(serde_json::Value::as_array)
            .and_then(|accounts| {
                accounts.iter().find(|account| {
                    account
                        .get("account_id")
                        .and_then(serde_json::Value::as_str)
                        == Some(account_id)
                })
            })
            .ok_or_else(|| format!("account {account_id} disappeared"))?;
        last_health = account
            .get("health")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        last_probe = account
            .get("last_probe_at_ms")
            .and_then(serde_json::Value::as_u64);
        let fresh = last_probe.is_some_and(|probed| probed >= since_ms);
        if fresh && last_health == "error" {
            return Err(
                "resident daemon reports no listener to serve the chat page on".to_string(),
            );
        }
        if fresh && last_health == "connected" {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "resident service never reported connected web chat health of its own within {}s\nlast account health: {last_health} (last probe {last_probe:?}, waiting for one at or after {since_ms})",
        SERVICE_WAIT.as_secs()
    ))
}

/// An HTTPS client pinned to the daemon's own certificate and nothing else —
/// the same pin a real browser would be asked to make, and the reason a
/// substituted certificate fails the handshake rather than being noticed later.
fn pinned_client(certificate: &Path) -> Result<reqwest::Client, String> {
    let pem = std::fs::read(certificate)
        .map_err(|error| format!("read {}: {error}", certificate.display()))?;
    reqwest::Client::builder()
        .tls_certs_only([reqwest::tls::Certificate::from_pem(&pem)
            .map_err(|error| format!("parse the test certificate: {error}"))?])
        .build()
        .map_err(|error| format!("build the pinned visitor client: {error}"))
}

/// One browser: a visitor identifier the daemon minted, and the two calls the
/// page makes with it.
struct Visitor {
    client: reqwest::Client,
    base: String,
    account_id: String,
    visitor_id: String,
}

impl Visitor {
    async fn open(client: &reqwest::Client, base: &str, account_id: &str) -> Result<Self, String> {
        // The page itself first, exactly as a browser would load it.
        let page = client
            .get(format!("{base}/webchat/{account_id}"))
            .send()
            .await
            .map_err(|error| format!("GET the chat page: {error}"))?;
        if !page.status().is_success() {
            return Err(format!("the chat page answered {}", page.status()));
        }
        let policy = page
            .headers()
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if !policy.contains("default-src 'none'") || !policy.contains("frame-ancestors 'none'") {
            return Err(format!(
                "the chat page was served without its policy: {policy:?}"
            ));
        }
        let html = page
            .text()
            .await
            .map_err(|error| format!("read the chat page: {error}"))?;
        if !html.contains("/webchat/ui/webchat.js") {
            return Err("the chat page did not reference its own script".to_string());
        }

        let session = client
            .post(format!("{base}/webchat/{account_id}/session"))
            .send()
            .await
            .map_err(|error| format!("POST a session: {error}"))?;
        if !session.status().is_success() {
            return Err(format!("the session route answered {}", session.status()));
        }
        let payload: serde_json::Value = session
            .json()
            .await
            .map_err(|error| format!("session JSON: {error}"))?;
        let visitor_id = payload
            .get("visitor_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("the session route minted no identifier: {payload}"))?
            .to_string();
        if visitor_id.len() != 43 {
            return Err(format!(
                "a minted identifier has 43 characters: {visitor_id:?}"
            ));
        }
        Ok(Self {
            client: client.clone(),
            base: base.to_string(),
            account_id: account_id.to_string(),
            visitor_id,
        })
    }

    async fn say(&self, text: &str) -> Result<(), String> {
        let response = self
            .client
            .post(format!(
                "{}/webchat/{}/messages",
                self.base, self.account_id
            ))
            .json(&serde_json::json!({ "visitor_id": self.visitor_id, "text": text }))
            .send()
            .await
            .map_err(|error| format!("POST a message: {error}"))?;
        if response.status().as_u16() != 202 {
            return Err(format!(
                "the message route answered {} rather than 202",
                response.status()
            ));
        }
        Ok(())
    }

    async fn transcript(&self) -> Result<Vec<serde_json::Value>, String> {
        let response = self
            .client
            .get(format!(
                "{}/webchat/{}/messages",
                self.base, self.account_id
            ))
            .header(VISITOR_HEADER, &self.visitor_id)
            .send()
            .await
            .map_err(|error| format!("GET the transcript: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "the transcript route answered {}",
                response.status()
            ));
        }
        let payload: serde_json::Value = response
            .json()
            .await
            .map_err(|error| format!("transcript JSON: {error}"))?;
        Ok(payload
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Poll until an outbound message contains `needle`, exactly as the page's
    /// own loop does.
    async fn wait_for_reply(&self, needle: &str) -> Result<String, String> {
        let deadline = Instant::now() + PAGE_WAIT;
        let mut last = String::new();
        while Instant::now() < deadline {
            let messages = self.transcript().await?;
            last = serde_json::Value::Array(messages.clone()).to_string();
            for message in &messages {
                if message.get("outbound") == Some(&serde_json::Value::Bool(true)) {
                    let text = message
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    if text.contains(needle) {
                        return Ok(text.to_string());
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
        Err(format!(
            "the page never showed {needle:?} within {}s\nlast transcript: {last}",
            PAGE_WAIT.as_secs()
        ))
    }
}

/// The sender id waiting for approval on this account, once the pairing
/// challenge has been recorded. This is the hashed visitor, which is the only
/// form of it the durable store ever holds.
fn wait_for_pending_sender(
    profile: &str,
    account_id: &str,
    deadline: Instant,
) -> Result<String, String> {
    let mut last = String::new();
    while Instant::now() < deadline {
        let output = require_cli(
            Some(profile),
            &["channels", "senders", account_id, "--json"],
        )?;
        let payload: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("senders JSON was invalid: {error}"))?;
        last = payload.to_string();
        if let Some(sender) = payload
            .get("pending")
            .and_then(serde_json::Value::as_array)
            .and_then(|pending| pending.first())
            .and_then(|sender| sender.get("sender_id"))
            .and_then(serde_json::Value::as_str)
        {
            if !sender.starts_with("web-") {
                return Err(format!(
                    "a web chat visitor reaches the store hashed, not raw: {sender:?}"
                ));
            }
            return Ok(sender.to_string());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "no visitor was ever waiting for approval\nlast senders: {last}"
    ))
}

fn assert_durable_events(profile: &str, account_id: &str) -> Result<(), String> {
    let output = require_cli(
        Some(profile),
        &["channels", "events", account_id, "--limit", "50", "--json"],
    )?;
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("channel events JSON was invalid: {error}"))?;
    let events = payload
        .get("events")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "channel events JSON had no events array".to_string())?;

    let ran: Vec<&serde_json::Value> = events
        .iter()
        .filter(|event| {
            event.get("direction").and_then(serde_json::Value::as_str) == Some("inbound")
                && event
                    .get("ingress_id")
                    .is_some_and(|value| !value.is_null())
                && event.get("job_id").is_some_and(|value| !value.is_null())
        })
        .collect();
    let inbound = events
        .iter()
        .filter(|event| {
            event.get("direction").and_then(serde_json::Value::as_str) == Some("inbound")
        })
        .count();
    let outbound = events
        .iter()
        .filter(|event| {
            event.get("direction").and_then(serde_json::Value::as_str) == Some("outbound")
        })
        .count();

    // Two inbound messages arrived; exactly one of them earned a run. The
    // first was answered with a pairing code and must NOT have run — that is
    // the whole reason an unknown visitor is not an authorized one.
    if inbound != 2 {
        return Err(format!(
            "expected two durable inbound events (the challenged one and the approved one), got {inbound}: {payload}"
        ));
    }
    if ran.len() != 1 {
        return Err(format!(
            "expected exactly one inbound event to own a run, got {}: {payload}",
            ran.len()
        ));
    }
    // Three outbound rows: the pairing code, the one-time first-contact notice
    // naming the model, and the agent's reply. Exact on purpose — a fourth
    // would be a reply sent twice.
    if outbound != 3 {
        return Err(format!(
            "expected exactly three durable outbound events (the pairing code, the first-contact notice and the reply), got {outbound}: {payload}"
        ));
    }
    Ok(())
}

fn dump_diagnostics(profile: &str, account_id: Option<&str>) {
    if let Ok(output) = run_cli(Some(profile), &["daemon", "status", "--json"]) {
        eprintln!("--- daemon status ---\n{}", output_text(&output));
    }
    if let Ok(output) = run_cli(Some(profile), &["channels", "list", "--json"]) {
        eprintln!("--- channels ---\n{}", output_text(&output));
    }
    if let Some(account_id) = account_id {
        if let Ok(output) = run_cli(
            Some(profile),
            &["channels", "senders", account_id, "--json"],
        ) {
            eprintln!("--- senders ---\n{}", output_text(&output));
        }
        if let Ok(output) = run_cli(
            Some(profile),
            &["channels", "events", account_id, "--limit", "20", "--json"],
        ) {
            eprintln!("--- channel events ---\n{}", output_text(&output));
        }
    }
}

fn cleanup(profile: Option<&str>, account_id: Option<&str>) {
    if let Some(profile) = profile {
        let _ = run_cli(Some(profile), &["daemon", "stop"]);
        let _ = run_cli(Some(profile), &["daemon", "uninstall"]);
        if let Some(account_id) = account_id {
            let _ = run_cli(Some(profile), &["channels", "remove", account_id]);
        }
        let _ = run_cli(None, &["profiles", "delete", profile, "--yes"]);
    }
}

fn run_case() -> Result<(), String> {
    if std::env::var(REQUIRE_ENV).as_deref() != Ok("1") {
        return Err(format!(
            "refusing to install a real OS service unless {REQUIRE_ENV}=1"
        ));
    }
    if cfg!(target_os = "macos") {
        return Err("this acceptance currently targets Linux/Windows service semantics; macOS service acceptance remains tracked separately".to_string());
    }

    let stamp = unique();
    let first_marker = format!("lm-webchat-first-{stamp:x}");
    let marker = format!("lm-webchat-installed-{stamp:x}");
    let model = ModelFixture::spawn(marker.clone())?;

    let scratch = std::env::temp_dir().join(format!("lm-webchat-e2e-{stamp:x}"));
    std::fs::create_dir_all(&scratch)
        .map_err(|error| format!("create scratch {}: {error}", scratch.display()))?;
    let Some((certificate, key)) = self_signed_certificate(&scratch) else {
        eprintln!("skipping: the openssl CLI is required to mint a loopback certificate");
        return Ok(());
    };
    let port = free_port()?;
    let base = format!("https://127.0.0.1:{port}");

    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| format!("build a tokio runtime: {error}"))?;

    let mut profile: Option<String> = None;
    let mut account_id: Option<String> = None;
    let result = (|| -> Result<(), String> {
        let created = create_profile()?;
        profile = Some(created.clone());

        let workspace = scratch.join("workspace");
        std::fs::create_dir_all(&workspace)
            .map_err(|error| format!("create workspace {}: {error}", workspace.display()))?;
        write_recipe(&created, &workspace, &model.base)?;

        require_cli(
            Some(&created),
            &[
                "daemon",
                "remote",
                "host-configure",
                "--listen",
                &format!("127.0.0.1:{port}"),
                "--advertise-url",
                &base,
                "--tls-certificate",
                &certificate.to_string_lossy(),
                "--tls-private-key",
                &key.to_string_lossy(),
            ],
        )?;

        let account = add_webchat_account(&created)?;
        account_id = Some(account.clone());
        // Pairing, not open: the point of this harness is that an unknown
        // visitor is challenged and an approved one is answered.
        require_cli(
            Some(&created),
            &[
                "channels",
                "policy",
                &account,
                "--direct",
                "pairing",
                "--group",
                "pairing",
                "--activation",
                "always",
            ],
        )?;
        require_cli(
            Some(&created),
            &[
                "channels",
                "add-route",
                RECIPE,
                "--account",
                &account,
                "--json",
            ],
        )?;
        require_cli(Some(&created), &["channels", "enable", &account])?;

        let installed = require_cli(Some(&created), &["daemon", "install"])?;
        if !String::from_utf8_lossy(&installed.stdout).contains("Installed") {
            return Err(format!(
                "daemon install returned unexpected output: {}",
                String::from_utf8_lossy(&installed.stdout)
            ));
        }
        let waiting_since_ms = now_ms();
        let deadline = Instant::now() + SERVICE_WAIT;
        let first_pid = wait_for_service_pid(&created, deadline)?;
        wait_for_account_connected(&created, &account, waiting_since_ms, deadline)?;

        // Prove the listener belongs to a real resident lifecycle rather than
        // to the process that configured it.
        require_cli(Some(&created), &["daemon", "stop"])?;
        require_cli(Some(&created), &["daemon", "start"])?;
        let waiting_since_ms = now_ms();
        let deadline = Instant::now() + SERVICE_WAIT;
        let restarted_pid = wait_for_service_pid(&created, deadline)?;
        wait_for_account_connected(&created, &account, waiting_since_ms, deadline)?;
        if restarted_pid == u64::from(std::process::id()) {
            return Err("restarted daemon pid is the acceptance harness pid".to_string());
        }
        eprintln!(
            "installed web chat daemon ready (initial pid {first_pid}, after restart {restarted_pid})"
        );

        let client = pinned_client(&certificate)?;
        let visitor = runtime.block_on(Visitor::open(&client, &base, &account))?;

        // 1. An unknown visitor is answered with a pairing code, and nothing
        //    runs.
        runtime.block_on(visitor.say(&first_marker))?;
        let challenge = runtime.block_on(visitor.wait_for_reply("code"))?;
        eprintln!("the page showed the pairing challenge: {challenge}");
        if model
            .requests()
            .iter()
            .any(|request| request.contains(&first_marker))
        {
            return Err(
                "an unapproved visitor's message reached the model; pairing decided nothing"
                    .to_string(),
            );
        }

        // 2. The operator approves that sender through the CLI, exactly as on
        //    any other provider.
        let sender = wait_for_pending_sender(&created, &account, Instant::now() + SERVICE_WAIT)?;
        require_cli(Some(&created), &["channels", "approve", &account, &sender])?;

        // 3. Now the same visitor gets a real agent reply.
        runtime.block_on(visitor.say(&marker))?;
        let observed = runtime.block_on(visitor.wait_for_reply(REPLY_PREFIX))?;
        eprintln!("the page showed the agent reply: {observed}");
        if !observed.contains(&marker) {
            return Err(format!("the reply did not carry the marker: {observed}"));
        }

        // 4. A second visitor of the same account reads none of it.
        let stranger = runtime.block_on(Visitor::open(&client, &base, &account))?;
        let theirs = runtime.block_on(stranger.transcript())?;
        if !theirs.is_empty() {
            return Err(format!(
                "a second visitor read somebody else's conversation: {theirs:?}"
            ));
        }

        let requests = model.requests();
        if !requests.iter().any(|request| request.contains(&marker)) {
            return Err(format!(
                "the installed agent never sent the real page message to the model: {requests:?}"
            ));
        }
        if !requests
            .iter()
            .any(|request| request.contains(r#""name":"send_message""#))
        {
            return Err("send_message was never offered to the installed agent".to_string());
        }
        if requests.len() < 2 {
            return Err(format!(
                "agent never returned to the model with the send_message result: {} request(s)",
                requests.len()
            ));
        }

        assert_durable_events(&created, &account)
    })();

    if result.is_err() {
        if let Some(profile) = profile.as_deref() {
            dump_diagnostics(profile, account_id.as_deref());
        }
    }
    cleanup(profile.as_deref(), account_id.as_deref());
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

fn main() {
    if let Err(error) = run_case() {
        eprintln!("WebChat installed-service E2E failed: {error}");
        std::process::exit(1);
    }
}
