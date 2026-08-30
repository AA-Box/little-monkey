//! Real Signal -> production signal-cli helper -> installed resident daemon
//! -> agent -> real Signal reply acceptance.
//!
//! This is intentionally not an adapter smoke test. Little Monkey is configured
//! only through its production CLI, then an actual installed resident daemon
//! owns the production Signal adapter and its signal-cli child. A separately
//! registered Signal identity, stored in a separate signal-cli data directory,
//! sends the marker and must receive the generated reply from the real service.
//!
//! The deterministic model is the only fixture. It is reached through an
//! ordinary recipe `target.local_url`; it cannot send a channel message itself.
//!
//! This acceptance cannot be made safe for ordinary hosted CI: registering a
//! Signal identity requires a real account/phone or linked device. Run it on a
//! machine that already has two disposable registered identities:
//!
//! ```text
//! LITTLE_MONKEY_REQUIRE_SIGNAL_INSTALLED_SERVICE_E2E=1 \
//! SIGNAL_E2E_HELPER=/usr/local/bin/signal-cli \
//! SIGNAL_E2E_BOT_ACCOUNT=+15550000000 \
//! SIGNAL_E2E_EXTERNAL_ACCOUNT=+15550000001 \
//! SIGNAL_E2E_EXTERNAL_DATA_DIR=/private/tmp/signal-cli-external \
//! cargo run --manifest-path src-tauri/Cargo.toml \
//!   --example signal_installed_service_e2e
//! ```
//!
//! The bot identity must be registered in signal-cli's normal data directory
//! for this user, because that is the exact helper environment the installed
//! daemon inherits in production. The external identity must use a different
//! data directory so the observer never shares the daemon-owned account store.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROFILE_ENV: &str = "LITTLE_MONKEY_PROFILE";
const REQUIRE_ENV: &str = "LITTLE_MONKEY_REQUIRE_SIGNAL_INSTALLED_SERVICE_E2E";
const RECIPE: &str = "signal-installed-service-e2e";
const REPLY_PREFIX: &str = "little-monkey signal installed-service reply";
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);
const SERVICE_WAIT: Duration = Duration::from_secs(180);
const REPLY_WAIT: Duration = Duration::from_secs(300);

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

fn run_cli(profile: Option<&str>, args: &[&str]) -> Result<Output, String> {
    let binary = cli();
    if !binary.is_file() {
        return Err(format!(
            "prebuilt monkey-cli is missing at {}",
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
    match profile {
        Some(profile) => {
            command.env(PROFILE_ENV, profile);
        }
        None => {
            command.env_remove(PROFILE_ENV);
        }
    }
    let child = command
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
    let name = format!("Signal installed-service E2E {}", unique());
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
        .ok_or_else(|| format!("profile JSON had no id: {payload}"))
}

fn add_account(profile: &str, helper: &Path, bot_account: &str) -> Result<String, String> {
    let label = format!("signal-e2e-{}", unique());
    let config = serde_json::json!({
        "helper_path": helper.to_string_lossy(),
        "account": bot_account,
    })
    .to_string();
    let output = require_cli(
        Some(profile),
        &[
            "channels", "add", "signal", &label, "--config", &config, "--json",
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
                                    "id": "call_signal_installed_1",
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
    loop {
        match stream.read(&mut scratch) {
            Ok(0) => break,
            Ok(count) => {
                received.extend_from_slice(&scratch[..count]);
                if header_end.is_none() {
                    if let Some(index) = find(&received, b"\r\n\r\n") {
                        header_end = Some(index + 4);
                        let headers = String::from_utf8_lossy(&received[..index]);
                        content_length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                    }
                }
                if let Some(start) = header_end {
                    if content_length == 0 || received.len() >= start + content_length {
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
        "target": { "local_url": model_base, "model": "signal-e2e-fixture" },
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
            return Err("resident daemon could not build/connect the Signal account".to_string());
        }
        if fresh && last_health == "connected" {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "resident service never reported connected Signal health of its own within {}s\nlast account health: {last_health} (last probe {last_probe:?}, waiting for one at or after {since_ms})",
        SERVICE_WAIT.as_secs()
    ))
}

fn run_signal(
    helper: &Path,
    external_data_dir: &Path,
    external_account: &str,
    args: &[&str],
) -> Result<Output, String> {
    let mut command = Command::new(helper);
    command
        .arg("--config")
        .arg(external_data_dir)
        .args(["-a", external_account, "--output=json"])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command
        .spawn()
        .map_err(|error| format!("failed to start external signal-cli: {error}"))?;
    bounded_output(child, "external signal-cli")
}

fn assert_external_account(
    helper: &Path,
    external_data_dir: &Path,
    external_account: &str,
) -> Result<(), String> {
    if !external_data_dir.is_dir() {
        return Err(format!(
            "external Signal data directory does not exist: {}",
            external_data_dir.display()
        ));
    }
    let mut command = Command::new(helper);
    command
        .arg("--config")
        .arg(external_data_dir)
        .args(["--output=json", "listAccounts"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = bounded_output(
        command
            .spawn()
            .map_err(|error| format!("start signal-cli listAccounts: {error}"))?,
        "signal-cli listAccounts",
    )?;
    if !output.status.success() {
        return Err(format!(
            "could not verify external Signal registration: {}",
            output_text(&output)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.contains(external_account) {
        return Err(format!(
            "external account {external_account} is not registered in {}: {stdout}",
            external_data_dir.display()
        ));
    }
    Ok(())
}

fn drain_external_pending(
    helper: &Path,
    external_data_dir: &Path,
    external_account: &str,
) -> Result<(), String> {
    let output = run_signal(
        helper,
        external_data_dir,
        external_account,
        &[
            "receive",
            "--timeout",
            "1",
            "--max-messages",
            "100",
            "--ignore-attachments",
        ],
    )?;
    if !output.status.success() {
        return Err(format!(
            "external Signal pre-drain failed: {}",
            output_text(&output)
        ));
    }
    Ok(())
}

fn send_external_marker(
    helper: &Path,
    external_data_dir: &Path,
    external_account: &str,
    bot_account: &str,
    marker: &str,
) -> Result<(), String> {
    let output = run_signal(
        helper,
        external_data_dir,
        external_account,
        &["send", "-m", marker, bot_account],
    )?;
    if !output.status.success() {
        return Err(format!(
            "external Signal send failed: {}",
            output_text(&output)
        ));
    }
    Ok(())
}

fn received_expected_reply(value: &serde_json::Value, bot_account: &str, expected: &str) -> bool {
    let envelope = value.get("envelope").unwrap_or(value);
    let source = envelope
        .get("sourceNumber")
        .or_else(|| envelope.get("source"))
        .and_then(serde_json::Value::as_str);
    let text = envelope
        .get("dataMessage")
        .and_then(|message| message.get("message"))
        .and_then(serde_json::Value::as_str);
    source == Some(bot_account) && text == Some(expected)
}

fn wait_for_external_reply(
    helper: &Path,
    external_data_dir: &Path,
    external_account: &str,
    bot_account: &str,
    marker: &str,
) -> Result<(), String> {
    let expected = format!("{REPLY_PREFIX} {marker}");
    let deadline = Instant::now() + REPLY_WAIT;
    let mut observed = Vec::new();
    while Instant::now() < deadline {
        let output = run_signal(
            helper,
            external_data_dir,
            external_account,
            &[
                "receive",
                "--timeout",
                "10",
                "--max-messages",
                "25",
                "--ignore-attachments",
            ],
        )?;
        if !output.status.success() {
            return Err(format!(
                "external Signal receive failed: {}",
                output_text(&output)
            ));
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if received_expected_reply(&value, bot_account, &expected) {
                eprintln!("external Signal identity observed exact generated reply");
                return Ok(());
            }
            observed.push(value);
            if observed.len() > 20 {
                observed.remove(0);
            }
        }
    }
    Err(format!(
        "external Signal identity did not observe {expected:?} from {bot_account} within {}s; last messages: {observed:?}",
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
    if inbound != 1 || outbound != 1 {
        return Err(format!(
            "expected exactly one accepted Signal inbound and one outbound event, got inbound={inbound}, outbound={outbound}: {payload}"
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
    // "the provider never delivered the message", and the two have nothing in
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

fn run_case() -> Result<(), String> {
    if std::env::var(REQUIRE_ENV).as_deref() != Ok("1") {
        return Err(format!(
            "refusing to install a real OS service or send Signal messages unless {REQUIRE_ENV}=1"
        ));
    }

    let helper = PathBuf::from(
        std::env::var("SIGNAL_E2E_HELPER")
            .map_err(|_| "SIGNAL_E2E_HELPER is required".to_string())?,
    );
    let bot_account = std::env::var("SIGNAL_E2E_BOT_ACCOUNT")
        .map_err(|_| "SIGNAL_E2E_BOT_ACCOUNT is required".to_string())?;
    let external_account = std::env::var("SIGNAL_E2E_EXTERNAL_ACCOUNT")
        .map_err(|_| "SIGNAL_E2E_EXTERNAL_ACCOUNT is required".to_string())?;
    let external_data_dir = PathBuf::from(
        std::env::var("SIGNAL_E2E_EXTERNAL_DATA_DIR")
            .map_err(|_| "SIGNAL_E2E_EXTERNAL_DATA_DIR is required".to_string())?,
    );
    if !helper.is_file() {
        return Err(format!(
            "signal-cli helper does not exist: {}",
            helper.display()
        ));
    }
    if bot_account == external_account {
        return Err("Signal E2E requires two independent registered identities".to_string());
    }
    assert_external_account(&helper, &external_data_dir, &external_account)?;

    let stamp = unique();
    let marker = format!("lm-signal-installed-{stamp:x}");
    let model = ModelFixture::spawn(marker.clone())?;
    let mut profile: Option<String> = None;
    let mut account_id: Option<String> = None;

    let result: Result<(), String> = (|| {
        let created = create_profile()?;
        profile = Some(created.clone());
        let workspace = std::env::temp_dir().join(format!("lm-signal-e2e-workspace-{stamp:x}"));
        std::fs::create_dir_all(&workspace)
            .map_err(|error| format!("create workspace {}: {error}", workspace.display()))?;
        write_recipe(&created, &workspace, &model.base)?;

        let account = add_account(&created, &helper, &bot_account)?;
        account_id = Some(account.clone());
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
        require_cli(Some(&created), &["daemon", "install"])?;
        let waiting_since_ms = now_ms();
        let deadline = Instant::now() + SERVICE_WAIT;
        let first_pid = wait_for_service_pid(&created, deadline)?;
        wait_for_account_connected(&created, &account, waiting_since_ms, deadline)?;
        require_cli(Some(&created), &["daemon", "stop"])?;
        require_cli(Some(&created), &["daemon", "start"])?;
        let waiting_since_ms = now_ms();
        let deadline = Instant::now() + SERVICE_WAIT;
        let restarted_pid = wait_for_service_pid(&created, deadline)?;
        wait_for_account_connected(&created, &account, waiting_since_ms, deadline)?;
        if first_pid == restarted_pid {
            return Err(format!(
                "daemon restart reused pid {first_pid}; installed-service process boundary was not independently demonstrated"
            ));
        }
        eprintln!(
            "installed Signal daemon restored real helper/account (pid {first_pid} -> {restarted_pid})"
        );

        drain_external_pending(&helper, &external_data_dir, &external_account)?;
        send_external_marker(
            &helper,
            &external_data_dir,
            &external_account,
            &bot_account,
            &marker,
        )?;
        eprintln!("independent Signal identity sent marker through the real provider");
        wait_for_external_reply(
            &helper,
            &external_data_dir,
            &external_account,
            &bot_account,
            &marker,
        )?;

        let requests = model.requests();
        if !requests.iter().any(|request| request.contains(&marker)) {
            return Err(format!(
                "installed agent never sent the Signal marker to the model: {requests:?}"
            ));
        }
        if !requests
            .iter()
            .any(|request| request.contains(r#""name":"send_message""#))
        {
            return Err("send_message was never offered to the installed Signal agent".to_string());
        }
        if requests.len() < 2 {
            return Err(format!(
                "agent never returned the send_message tool result to the model: {} request(s)",
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
    result
}

fn main() {
    if let Err(error) = run_case() {
        eprintln!("Signal installed-service E2E failed: {error}");
        std::process::exit(1);
    }
}
