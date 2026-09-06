//! The operator's own Home Assistant -> native credential store -> installed
//! resident daemon -> agent -> the same Home Assistant, as an acceptance.
//!
//! The provider is a real instance whoever runs this names — in CI, one the job
//! starts on loopback; on a dispatch or a local run, an operator's own house.
//! Little Monkey is configured only through the production CLI; its access
//! token is written with `channels set-token`, so the installed daemon must
//! recover it through `KeyringChannelSecrets` rather than from anything this
//! harness holds. The independent client is the instance's own REST API: it
//! fires the subscribed event with a unique marker and then watches the
//! instance's states for the notification the agent's reply produces.
//!
//! The sole deterministic component is the model HTTP origin, reached through
//! an ordinary recipe `target.local_url`. It cannot create channel events,
//! write the outbox or call a notify service.
//!
//! The accompanying workflow compiles this on every pull request and runs the
//! live acceptance there too, against an official Home Assistant container the
//! job starts on `127.0.0.1:8123` and onboards through Home Assistant's own
//! APIs. A dispatch or a local run points the same four environment variables
//! at an operator's own instance instead.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROFILE_ENV: &str = "LITTLE_MONKEY_PROFILE";
const REQUIRE_ENV: &str = "LITTLE_MONKEY_REQUIRE_HOME_ASSISTANT_INSTALLED_SERVICE_E2E";
const RECIPE: &str = "home-assistant-installed-service-e2e";
const REPLY_PREFIX: &str = "little-monkey home assistant installed-service reply";
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);
const SERVICE_WAIT: Duration = Duration::from_secs(120);
const REPLY_WAIT: Duration = Duration::from_secs(240);

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

fn run_cli_with_stdin(
    profile: Option<&str>,
    args: &[&str],
    stdin: Option<&str>,
) -> Result<Output, String> {
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
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    if let Some(profile) = profile {
        command.env(PROFILE_ENV, profile);
    } else {
        command.env_remove(PROFILE_ENV);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to start monkey-cli {args:?}: {error}"))?;
    if let Some(value) = stdin {
        child
            .stdin
            .take()
            .ok_or_else(|| "monkey-cli child had no stdin".to_string())?
            .write_all(value.as_bytes())
            .map_err(|error| format!("write monkey-cli stdin: {error}"))?;
    }
    bounded_output(child, &format!("monkey-cli {args:?}"))
}

fn run_cli(profile: Option<&str>, args: &[&str]) -> Result<Output, String> {
    run_cli_with_stdin(profile, args, None)
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

fn require_cli_stdin(profile: &str, args: &[&str], stdin: &str) -> Result<Output, String> {
    let output = run_cli_with_stdin(Some(profile), args, Some(stdin))?;
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
    let name = format!("Home Assistant installed-service E2E {}", unique());
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

fn add_account(
    profile: &str,
    base_url: &str,
    event_type: &str,
    notify_service: &str,
) -> Result<String, String> {
    let label = format!("home-assistant-e2e-{}", unique());
    let config = serde_json::json!({
        "base_url": base_url,
        "event_type": event_type,
        "notify_service": notify_service,
    })
    .to_string();
    let output = require_cli(
        Some(profile),
        &[
            "channels",
            "add",
            "home_assistant",
            &label,
            "--config",
            &config,
            "--json",
        ],
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
        .ok_or_else(|| format!("account JSON had no account_id: {payload}"))
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
                                    "id": "call_ha_installed_1",
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

fn write_recipe(profile: &str, workspace: &Path, model_base: &str) -> Result<(), String> {
    let previous = std::env::var_os(PROFILE_ENV);
    std::env::set_var(PROFILE_ENV, profile);
    let roots = little_monkey_lib::app_paths::ensure_agent_config_roots()
        .map_err(|error| format!("resolve profile config roots: {error}"));
    match previous {
        Some(value) => std::env::set_var(PROFILE_ENV, value),
        None => std::env::remove_var(PROFILE_ENV),
    }
    let roots = roots?;
    let recipes = roots.authored.join("recipes");
    std::fs::create_dir_all(&recipes)
        .map_err(|error| format!("create recipe directory {}: {error}", recipes.display()))?;
    let recipe = serde_json::json!({
        "version": 1,
        "name": RECIPE,
        "target": { "local_url": model_base, "model": "home-assistant-e2e-fixture" },
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// The pid of the installed resident service, once it is running and its
/// heartbeat is fresh.
///
/// Split from the account check below rather than answered together, so the
/// number this returns is read from `daemon status` and from nothing else. A
/// pid that came back through a function which had also parsed an account row
/// is a pid the reader — and code scanning — cannot tell apart from the row
/// that carries the credential.
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
/// `since_ms` is what makes this an answer rather than an echo. Account health
/// is a stored row, and the daemon that wrote it may be a process that has
/// since been stopped — so after a restart the row still says `connected` for
/// as long as it takes the new process to reach the provider and write its own
/// verdict. A harness that posts the moment it reads that row is posting into a
/// window where nothing is listening, and a socket provider does not replay
/// what it delivered to nobody. Requiring `last_probe_at_ms` to be at or after
/// the moment this wait began is what makes `connected` mean the running
/// process said so.
///
/// Returns nothing on success and a fixed label on failure: the account row is
/// the one payload here that can carry credential-bearing fields, so no part of
/// it travels back out to a caller that prints.
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
                "resident daemon could not build/connect the Home Assistant account".to_string(),
            );
        }
        if fresh && last_health == "connected" {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "resident service never reported connected Home Assistant health of its own within {}s\nlast account health: {last_health} (last probe {last_probe:?}, waiting for one at or after {since_ms})",
        SERVICE_WAIT.as_secs()
    ))
}

/// Fire the subscribed event on the operator's own instance, exactly as their
/// automation would. This is the independent sender: it goes in over the REST
/// API and comes back to Little Monkey over the WebSocket subscription, so
/// nothing in this harness can hand the daemon an event directly.
async fn fire_event_as_external(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    event_type: &str,
    marker: &str,
) -> Result<(), String> {
    let response = client
        .post(format!("{base_url}/api/events/{event_type}"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "text": marker,
            "conversation_id": "installed-service-e2e",
            "user": "installed-service-e2e",
        }))
        .send()
        .await
        .map_err(|error| format!("external Home Assistant event fire failed: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "Home Assistant rejected the external event ({status})"
        ));
    }
    Ok(())
}

/// Watch the instance's own states for the reply.
///
/// The workflow points the account at a `command_line` notify service whose
/// message a `command_line` sensor republishes as its own state, because that
/// is what makes the round trip readable over this REST API.
/// `notify.persistent_notification` cannot serve here: since 2023.6 a
/// persistent notification is a WebSocket-only object that never becomes a
/// state, so `GET /api/states` can never carry one. Any state whose JSON
/// carries the expected reply counts, so an operator who points the account at
/// a different service that also surfaces as an entity still gets a meaningful
/// run.
async fn wait_for_reply(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    marker: &str,
) -> Result<String, String> {
    let expected = format!("{REPLY_PREFIX} {marker}");
    let deadline = Instant::now() + REPLY_WAIT;
    while Instant::now() < deadline {
        let response = client
            .get(format!("{base_url}/api/states"))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|error| format!("read Home Assistant states: {error}"))?;
        if response.status().is_success() {
            let payload: serde_json::Value = response
                .json()
                .await
                .map_err(|error| format!("Home Assistant states JSON: {error}"))?;
            if let Some(states) = payload.as_array() {
                for state in states {
                    if state.to_string().contains(&expected) {
                        return Ok(state.to_string());
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(format!(
        "the operator's Home Assistant never surfaced {expected:?} within {}s",
        REPLY_WAIT.as_secs()
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
    let inbound = events
        .iter()
        .filter(|event| {
            event.get("direction").and_then(serde_json::Value::as_str) == Some("inbound")
                && event
                    .get("ingress_id")
                    .is_some_and(|value| !value.is_null())
                && event.get("job_id").is_some_and(|value| !value.is_null())
        })
        .count();
    let outbound = events
        .iter()
        .filter(|event| {
            event.get("direction").and_then(serde_json::Value::as_str) == Some("outbound")
        })
        .count();
    // Two outbound rows, not one: the daemon greets a person's first message
    // with a one-time notice naming the model, and every harness runs a fresh
    // profile, so its sender is always a first contact. The count stays exact
    // on purpose — a third row would be a reply sent twice, which is the bug
    // this assertion exists to catch.
    if inbound != 1 || outbound != 2 {
        return Err(format!(
            "expected one accepted inbound and two outbound events (the first-contact notice and the reply), got inbound={inbound}, outbound={outbound}: {payload}"
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
    // The question every failure here starts with: did anything arrive at all?
    // Without this, "no reply within the timeout" cannot be told apart from
    // "the subscription never delivered the event", and the two have nothing in
    // common to fix.
    if let Some(account_id) = account_id {
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

async fn run_case(
    base_url: &str,
    token: &str,
    event_type: &str,
    notify_service: &str,
) -> Result<(), String> {
    if std::env::var(REQUIRE_ENV).as_deref() != Ok("1") {
        return Err(format!(
            "refusing to install a real OS service and drive a real Home Assistant instance unless {REQUIRE_ENV}=1"
        ));
    }
    if cfg!(target_os = "macos") {
        return Err("this acceptance currently targets Linux service semantics; macOS service acceptance remains tracked separately".to_string());
    }

    let stamp = unique();
    let marker = format!("lm-home-assistant-installed-{stamp:x}");
    let model = ModelFixture::spawn(marker.clone())?;
    let mut profile: Option<String> = None;
    let mut account_id: Option<String> = None;

    let result: Result<(), String> = async {
        let created = create_profile()?;
        profile = Some(created.clone());
        let workspace = std::env::temp_dir().join(format!("lm-ha-e2e-workspace-{stamp:x}"));
        std::fs::create_dir_all(&workspace)
            .map_err(|error| format!("create workspace {}: {error}", workspace.display()))?;
        write_recipe(&created, &workspace, &model.base)?;

        let account = add_account(&created, base_url, event_type, notify_service)?;
        account_id = Some(account.clone());
        require_cli_stdin(
            &created,
            &["channels", "set-token", &account],
            &format!("{token}\n"),
        )?;
        require_cli(
            Some(&created),
            &[
                "channels",
                "policy",
                &account,
                "--direct",
                "open",
                "--group",
                "open",
                "--activation",
                "always",
            ],
        )?;
        require_cli(
            Some(&created),
            &["channels", "add-route", RECIPE, "--account", &account, "--json"],
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

        // Prove the subscription is owned by a real resident lifecycle, not
        // merely by the process that configured it: stop/start and require the
        // service to authenticate and subscribe again before the event is
        // fired. A socket provider does not replay what it delivered to nobody.
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
            "installed Home Assistant daemon ready (initial pid {first_pid}, after restart {restarted_pid})"
        );

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|error| format!("build external Home Assistant client: {error}"))?;
        fire_event_as_external(&client, base_url, token, event_type, &marker).await?;
        eprintln!("fired {event_type} on the operator's instance with marker {marker}");
        let observed = wait_for_reply(&client, base_url, token, &marker).await?;
        eprintln!("the operator's Home Assistant surfaced the reply: {observed}");

        let requests = model.requests();
        if !requests.iter().any(|request| request.contains(&marker)) {
            return Err(format!(
                "the installed agent never sent the real Home Assistant event to the model: {requests:?}"
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
                "agent never returned to the model with the tool result: {} request(s)",
                requests.len()
            ));
        }
        assert_durable_events(&created, &account)
    }
    .await;

    if result.is_err() {
        if let Some(profile) = profile.as_deref() {
            dump_diagnostics(profile, account_id.as_deref());
        }
    }
    cleanup(profile.as_deref(), account_id.as_deref());
    result
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let base_url = std::env::var("HA_E2E_BASE_URL").unwrap_or_default();
    let token = std::env::var("HA_E2E_TOKEN").unwrap_or_default();
    let event_type =
        std::env::var("HA_E2E_EVENT_TYPE").unwrap_or_else(|_| "little_monkey_message".to_string());
    // No default notify service. `persistent_notification` used to be it, but a
    // persistent notification never becomes a state, so that default silently
    // produced a run that could only ever time out. Naming the service is the
    // caller's job.
    let notify_service = std::env::var("HA_E2E_NOTIFY_SERVICE").unwrap_or_default();
    if base_url.is_empty() || token.is_empty() || notify_service.is_empty() {
        eprintln!("HA_E2E_BASE_URL, HA_E2E_TOKEN and HA_E2E_NOTIFY_SERVICE are required");
        std::process::exit(2);
    }
    if let Err(error) = run_case(&base_url, &token, &event_type, &notify_service).await {
        eprintln!("Home Assistant installed-service E2E failed: {error}");
        std::process::exit(1);
    }
}
