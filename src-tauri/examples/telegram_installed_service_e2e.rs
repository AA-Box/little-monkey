//! Arbitrary operator-owned Telegram bot -> installed resident daemon -> real
//! pairing/approval -> production agent -> Telegram -> provider-side text proof.
//!
//! This is deliberately black-box with respect to Little Monkey's Telegram
//! adapter. The harness configures the account only through `monkey-cli`, puts
//! the operator's BotFather token through `channels set-token`, installs and
//! restarts the real OS user service, and waits for real Telegram traffic. It
//! never constructs `TelegramAdapter`, never calls `getUpdates`, and never
//! calls `sendMessage`.
//!
//! A previously unknown Telegram user is discovered by the production pairing
//! gate. The harness approves that provider-derived sender id through the same
//! CLI path the desktop uses, then asks the same user to send the execution
//! marker. The only direct Bot API call made by this process is *after* the
//! production daemon has sent its reply: `forwardMessage` is used as an
//! independent provider-side read because Telegram bots have no history API.

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROFILE_ENV: &str = "LITTLE_MONKEY_PROFILE";
const REQUIRE_ENV: &str = "LITTLE_MONKEY_REQUIRE_TELEGRAM_INSTALLED_SERVICE_E2E";
const TOKEN_ENV: &str = "TELEGRAM_E2E_BOT_TOKEN";
const RECIPE: &str = "telegram-installed-service-e2e";
const REPLY_PREFIX: &str = "little-monkey telegram installed-service reply";
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);
const SERVICE_WAIT: Duration = Duration::from_secs(120);
const HUMAN_WAIT: Duration = Duration::from_secs(600);
const AGENT_WAIT: Duration = Duration::from_secs(240);
const PROVIDER_WAIT: Duration = Duration::from_secs(180);

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

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"))
}

fn cli() -> PathBuf {
    target_dir()
        .join("debug")
        .join(if cfg!(windows) { "monkey-cli.exe" } else { "monkey-cli" })
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
    let name = format!("Telegram installed-service E2E {}", unique());
    let output = require_cli(None, &["profiles", "create", &name, "--json"])?;
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("profile JSON was invalid: {error}\n{}", output_text(&output)))?;
    payload
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("profile JSON had no id: {payload}"))
}

fn add_account(profile: &str) -> Result<String, String> {
    let label = format!("telegram-e2e-{}", unique());
    let output = require_cli(
        Some(profile),
        &["channels", "add", "telegram", &label, "--json"],
    )?;
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("account JSON was invalid: {error}\n{}", output_text(&output)))?;
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
                                    "id": "call_telegram_installed_1",
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
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok()?;
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
    haystack.windows(needle.len()).position(|window| window == needle)
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
        "target": { "local_url": model_base, "model": "telegram-e2e-fixture" },
        "workspace": workspace.to_string_lossy(),
        "permission_mode": "bypass",
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

fn profile_state_db(profile: &str) -> Result<PathBuf, String> {
    let previous = std::env::var_os(PROFILE_ENV);
    std::env::set_var(PROFILE_ENV, profile);
    let data = little_monkey_lib::app_paths::data_dir()
        .ok_or_else(|| "could not resolve active profile data directory".to_string());
    match previous {
        Some(value) => std::env::set_var(PROFILE_ENV, value),
        None => std::env::remove_var(PROFILE_ENV),
    }
    Ok(data?.join("daemon").join("daemon-v1.sqlite3"))
}

fn read_only(path: &Path) -> Result<Connection, String> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("open daemon state {} read-only: {error}", path.display()))
}

#[derive(Debug)]
struct ProviderInbound {
    provider_event_id: String,
    conversation_id: String,
    sender_id: String,
    ingress_id: Option<String>,
    job_id: Option<String>,
}

fn find_inbound(
    db: &Path,
    account_id: &str,
    marker: &str,
    disposition: &str,
    expected_sender: Option<&str>,
) -> Result<Option<ProviderInbound>, String> {
    if !db.is_file() {
        return Ok(None);
    }
    let connection = read_only(db)?;
    let like = format!("%{marker}%");
    let mut statement = connection
        .prepare(
            "SELECT provider_event_id, conversation_id, sender_id, ingress_id, job_id
             FROM channel_events
             WHERE account_id = ?1
               AND direction = 'inbound'
               AND disposition = ?2
               AND envelope_json LIKE ?3
             ORDER BY received_at_ms DESC",
        )
        .map_err(|error| format!("prepare Telegram inbound lookup: {error}"))?;
    let mut rows = statement
        .query((account_id, disposition, like))
        .map_err(|error| format!("query Telegram inbound lookup: {error}"))?;
    while let Some(row) = rows
        .next()
        .map_err(|error| format!("read Telegram inbound lookup: {error}"))?
    {
        let sender_id: Option<String> = row
            .get(2)
            .map_err(|error| format!("read Telegram sender id: {error}"))?;
        let Some(sender_id) = sender_id else { continue };
        if expected_sender.is_some_and(|expected| expected != sender_id) {
            continue;
        }
        return Ok(Some(ProviderInbound {
            provider_event_id: row
                .get(0)
                .map_err(|error| format!("read Telegram provider event id: {error}"))?,
            conversation_id: row
                .get(1)
                .map_err(|error| format!("read Telegram conversation id: {error}"))?,
            sender_id,
            ingress_id: row
                .get(3)
                .map_err(|error| format!("read Telegram ingress id: {error}"))?,
            job_id: row
                .get(4)
                .map_err(|error| format!("read Telegram job id: {error}"))?,
        }));
    }
    Ok(None)
}

fn find_outbound(
    db: &Path,
    account_id: &str,
    conversation_id: &str,
    expected_reply: &str,
) -> Result<Option<String>, String> {
    if !db.is_file() {
        return Ok(None);
    }
    let connection = read_only(db)?;
    let like = format!("%{expected_reply}%");
    connection
        .query_row(
            "SELECT provider_event_id
             FROM channel_events
             WHERE account_id = ?1
               AND direction = 'outbound'
               AND conversation_id = ?2
               AND envelope_json LIKE ?3
               AND provider_event_id NOT LIKE 'local:%'
             ORDER BY received_at_ms DESC LIMIT 1",
            (account_id, conversation_id, like),
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| format!("read exact durable Telegram outbound: {error}"))
}

fn wait_for_service_pid(profile: &str, deadline: Instant) -> Result<u64, String> {
    let mut last = String::new();
    while Instant::now() < deadline {
        if let Ok(output) = run_cli(Some(profile), &["daemon", "status", "--json"]) {
            if output.status.success() {
                if let Ok(status) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                    last = status.to_string();
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
        "installed service never reported a fresh resident pid within {}s; last status: {last}",
        SERVICE_WAIT.as_secs()
    ))
}

fn wait_for_account_connected(
    profile: &str,
    account_id: &str,
    since_ms: u64,
    deadline: Instant,
) -> Result<(), String> {
    let mut last_health = "unknown".to_string();
    let mut last_probe = None;
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
            .ok_or_else(|| format!("Telegram account {account_id} disappeared"))?;
        last_health = account
            .get("health")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        last_probe = account
            .get("last_probe_at_ms")
            .and_then(serde_json::Value::as_u64);
        let fresh = last_probe.is_some_and(|value| value >= since_ms);
        if fresh && last_health == "error" {
            return Err("resident daemon rejected or could not reach the operator-supplied Telegram bot".to_string());
        }
        if fresh && last_health == "connected" {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "resident daemon never reported fresh connected Telegram health within {}s; last health {last_health}, probe {last_probe:?}",
        SERVICE_WAIT.as_secs()
    ))
}

fn wait_for_challenge(
    db: &Path,
    account_id: &str,
    marker: &str,
) -> Result<ProviderInbound, String> {
    let deadline = Instant::now() + HUMAN_WAIT;
    while Instant::now() < deadline {
        if let Some(row) = find_inbound(db, account_id, marker, "challenged", None)? {
            return Ok(row);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "no real Telegram sender produced the pairing challenge marker within {}s",
        HUMAN_WAIT.as_secs()
    ))
}

fn wait_for_accepted(
    db: &Path,
    account_id: &str,
    marker: &str,
    sender_id: &str,
) -> Result<ProviderInbound, String> {
    let deadline = Instant::now() + HUMAN_WAIT;
    while Instant::now() < deadline {
        if let Some(row) = find_inbound(db, account_id, marker, "accepted", Some(sender_id))? {
            if row.ingress_id.is_some() && row.job_id.is_some() {
                return Ok(row);
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "approved Telegram sender never produced an accepted durable ingress/job within {}s",
        HUMAN_WAIT.as_secs()
    ))
}

fn pending_contains(profile: &str, account_id: &str, sender_id: &str) -> Result<bool, String> {
    let output = require_cli(
        Some(profile),
        &["channels", "senders", account_id, "--json"],
    )?;
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("channels senders JSON was invalid: {error}"))?;
    Ok(payload
        .get("pending")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|pending| {
            pending.iter().any(|sender| {
                sender
                    .get("sender_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(sender_id)
            })
        }))
}

async fn observe_telegram(
    token: &str,
    chat_id: &str,
    message_id: &str,
) -> Result<String, String> {
    let message_id = message_id
        .parse::<i64>()
        .map_err(|_| format!("Telegram outbound id {message_id:?} is not numeric"))?;
    let client = little_monkey_lib::egress::hardened()
        .build()
        .map_err(|_| "could not build hardened Telegram observer client".to_string())?;
    let request = client
        .post(format!("https://api.telegram.org/bot{token}/forwardMessage"))
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "from_chat_id": chat_id,
            "message_id": message_id,
        }));
    let response = little_monkey_lib::egress::send(request)
        .await
        .map_err(|_| "Telegram forwardMessage observer request failed".to_string())?;
    let status = response.status();
    let payload: serde_json::Value = response
        .json()
        .await
        .map_err(|_| "Telegram forwardMessage returned invalid JSON".to_string())?;
    if !status.is_success() || payload.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(format!(
            "Telegram did not confirm the generated reply through forwardMessage (HTTP {status})"
        ));
    }
    let text = payload
        .get("result")
        .and_then(|result| result.get("text"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    if let Some(copy_id) = payload
        .get("result")
        .and_then(|result| result.get("message_id"))
        .and_then(serde_json::Value::as_i64)
    {
        let cleanup = client
            .post(format!("https://api.telegram.org/bot{token}/deleteMessage"))
            .json(&serde_json::json!({ "chat_id": chat_id, "message_id": copy_id }));
        let _ = little_monkey_lib::egress::send(cleanup).await;
    }
    Ok(text)
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
            &["channels", "events", account_id, "--limit", "80", "--json"],
        ) {
            eprintln!("--- channel events ---\n{}", output_text(&output));
        }
        if let Ok(output) = run_cli(
            Some(profile),
            &["channels", "senders", account_id, "--json"],
        ) {
            eprintln!("--- pending senders ---\n{}", output_text(&output));
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

struct LiveConfig {
    token: String,
}

impl LiveConfig {
    fn from_env() -> Result<Self, String> {
        let token = std::env::var(TOKEN_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("{TOKEN_ENV} is required and must be a token for a bot you own"))?;
        Ok(Self { token })
    }
}

async fn run_case(config: &LiveConfig) -> Result<(), String> {
    if std::env::var(REQUIRE_ENV).as_deref() != Ok("1") {
        return Err(format!(
            "refusing to install a real OS service and use a live Telegram bot unless {REQUIRE_ENV}=1"
        ));
    }

    let stamp = unique();
    let pairing_marker = format!("lm-telegram-pair-{stamp:x}");
    let execution_marker = format!("lm-telegram-run-{stamp:x}");
    let expected_reply = format!("{REPLY_PREFIX} {execution_marker}");
    let model = ModelFixture::spawn(execution_marker.clone())?;
    let mut profile: Option<String> = None;
    let mut account_id: Option<String> = None;

    let result: Result<(), String> = async {
        let created = create_profile()?;
        profile = Some(created.clone());
        let workspace = std::env::temp_dir().join(format!("lm-telegram-e2e-{stamp:x}"));
        std::fs::create_dir_all(&workspace)
            .map_err(|error| format!("create workspace {}: {error}", workspace.display()))?;
        write_recipe(&created, &workspace, &model.base)?;

        let account = add_account(&created)?;
        account_id = Some(account.clone());
        require_cli_stdin(
            &created,
            &["channels", "set-token", &account],
            &format!("{}\n", config.token),
        )?;
        require_cli(
            Some(&created),
            &[
                "channels", "policy", &account,
                "--direct", "pairing",
                "--group", "pairing",
                "--activation", "mention_only",
            ],
        )?;
        require_cli(
            Some(&created),
            &[
                "channels", "add-route", RECIPE,
                "--account", &account,
                "--session-scope", "conversation",
                "--json",
            ],
        )?;
        require_cli(Some(&created), &["channels", "enable", &account])?;

        let installed_at = now_ms();
        require_cli(Some(&created), &["daemon", "install"])?;
        let first_pid = wait_for_service_pid(&created, Instant::now() + SERVICE_WAIT)?;
        wait_for_account_connected(
            &created,
            &account,
            installed_at,
            Instant::now() + SERVICE_WAIT,
        )?;

        require_cli(Some(&created), &["daemon", "stop"])?;
        let restarted_at = now_ms();
        require_cli(Some(&created), &["daemon", "start"])?;
        let restarted_pid = wait_for_service_pid(&created, Instant::now() + SERVICE_WAIT)?;
        if restarted_pid == first_pid {
            return Err(format!(
                "daemon restart did not produce a new resident process (pid {first_pid})"
            ));
        }
        wait_for_account_connected(
            &created,
            &account,
            restarted_at,
            Instant::now() + SERVICE_WAIT,
        )?;

        let db = profile_state_db(&created)?;
        eprintln!(
            "\nThe installed daemon is connected to the BotFather bot you supplied.\n\nFrom a real Telegram user that has NOT been approved in this fresh profile, send this exact text to your bot:\n\n    {pairing_marker}\n\nThe harness will discover that provider user id from the real challenged delivery and approve it through Little Monkey.\n"
        );
        let challenge = wait_for_challenge(&db, &account, &pairing_marker)?;
        if challenge.ingress_id.is_some() || challenge.job_id.is_some() {
            return Err(format!(
                "pairing-gated Telegram event {} unexpectedly became a runnable ingress/job",
                challenge.provider_event_id
            ));
        }
        if !model.requests().is_empty() {
            return Err("pairing challenge reached the model before the sender was approved".to_string());
        }
        if !pending_contains(&created, &account, &challenge.sender_id)? {
            return Err(format!(
                "challenged Telegram sender {} was not exposed through the normal pending-sender UI/CLI path",
                challenge.sender_id
            ));
        }
        require_cli(
            Some(&created),
            &["channels", "approve", &account, &challenge.sender_id],
        )?;
        eprintln!(
            "Approved provider-discovered Telegram sender {} for this installation.\n\nFrom that SAME Telegram user, now send this exact execution text:\n\n    {execution_marker}\n",
            challenge.sender_id
        );

        let accepted = wait_for_accepted(
            &db,
            &account,
            &execution_marker,
            &challenge.sender_id,
        )?;
        if accepted.conversation_id != challenge.conversation_id {
            return Err(format!(
                "the approved sender changed Telegram conversations between pairing ({}) and execution ({})",
                challenge.conversation_id, accepted.conversation_id
            ));
        }
        let ingress_id = accepted
            .ingress_id
            .as_deref()
            .ok_or_else(|| "accepted Telegram event had no ingress id".to_string())?;
        let job_id = accepted
            .job_id
            .as_deref()
            .ok_or_else(|| "accepted Telegram event had no job id".to_string())?;
        eprintln!(
            "Real Telegram event {} became ingress {} / job {} in installed daemon pid {}",
            accepted.provider_event_id, ingress_id, job_id, restarted_pid
        );

        let agent_deadline = Instant::now() + AGENT_WAIT;
        loop {
            let requests = model.requests();
            let saw_marker = requests.iter().any(|request| request.contains(&execution_marker));
            let saw_tool = requests.iter().any(|request| request.contains("send_message"));
            if saw_marker && saw_tool && requests.len() >= 2 {
                break;
            }
            if Instant::now() >= agent_deadline {
                return Err(format!(
                    "installed Telegram turn did not complete the real model/tool loop within {}s ({} model requests)",
                    AGENT_WAIT.as_secs(),
                    requests.len()
                ));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        let provider_deadline = Instant::now() + PROVIDER_WAIT;
        let provider_message_id = loop {
            if let Some(id) = find_outbound(
                &db,
                &account,
                &accepted.conversation_id,
                &expected_reply,
            )? {
                break id;
            }
            if Instant::now() >= provider_deadline {
                return Err(format!(
                    "generated Telegram reply never became a provider-named durable outbound event within {}s",
                    PROVIDER_WAIT.as_secs()
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        if provider_message_id.starts_with("local:") || provider_message_id.trim().is_empty() {
            return Err(format!(
                "Telegram reply has no real provider message id: {provider_message_id:?}"
            ));
        }

        let observed = observe_telegram(
            &config.token,
            &accepted.conversation_id,
            &provider_message_id,
        )
        .await?;
        if observed != expected_reply {
            return Err(format!(
                "Telegram provider-side observation did not match the generated reply; expected {expected_reply:?}, got {observed:?}"
            ));
        }
        eprintln!(
            "Telegram independently returned the exact generated reply through forwardMessage (provider message id {provider_message_id})."
        );
        Ok(())
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
    let config = match LiveConfig::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("Telegram installed-service E2E configuration error: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = run_case(&config).await {
        eprintln!("Telegram installed-service E2E failed: {error}");
        std::process::exit(1);
    }
}
