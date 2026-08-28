//! Two operator-owned Discord bots -> real Discord Gateway/REST -> installed
//! resident daemon -> provider-derived approval -> production agent -> Discord
//! -> independent provider-side exact-text proof.
//!
//! The bot under test is configured only through `monkey-cli`: its token enters
//! through `channels set-token`, the real OS user service is installed and
//! restarted, and the production Discord adapter owns Gateway inbound and REST
//! outbound. This harness never constructs `DiscordAdapter`.
//!
//! A second operator-owned bot is the external actor. It is deliberately never
//! configured in Little Monkey. It posts into a dedicated Discord channel via
//! Discord's REST API, and later reads the production bot's generated reply back
//! from Discord. That makes both ends provider-observed without automating a
//! human Discord account (self-bots are not used).

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROFILE_ENV: &str = "LITTLE_MONKEY_PROFILE";
const REQUIRE_ENV: &str = "LITTLE_MONKEY_REQUIRE_DISCORD_INSTALLED_SERVICE_E2E";
const BOT_TOKEN_ENV: &str = "DISCORD_E2E_BOT_TOKEN";
const EXTERNAL_BOT_TOKEN_ENV: &str = "DISCORD_E2E_EXTERNAL_BOT_TOKEN";
const CHANNEL_ID_ENV: &str = "DISCORD_E2E_CHANNEL_ID";
const API_BASE: &str = "https://discord.com/api/v10";
const RECIPE: &str = "discord-installed-service-e2e";
const REPLY_PREFIX: &str = "little-monkey discord installed-service reply";
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);
const SERVICE_WAIT: Duration = Duration::from_secs(150);
const PROVIDER_WAIT: Duration = Duration::from_secs(180);
const AGENT_WAIT: Duration = Duration::from_secs(240);

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
    let name = format!("Discord installed-service E2E {}", unique());
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
    let label = format!("discord-e2e-{}", unique());
    let output = require_cli(
        Some(profile),
        &["channels", "add", "discord", &label, "--json"],
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
                                    "id": "call_discord_installed_1",
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
        "target": { "local_url": model_base, "model": "discord-e2e-fixture" },
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
    thread_id: Option<String>,
    sender_id: String,
    ingress_id: Option<String>,
    job_id: Option<String>,
}

impl ProviderInbound {
    fn target_channel(&self) -> &str {
        self.thread_id.as_deref().unwrap_or(&self.conversation_id)
    }
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
            "SELECT provider_event_id, conversation_id, thread_id, sender_id, ingress_id, job_id
             FROM channel_events
             WHERE account_id = ?1
               AND direction = 'inbound'
               AND disposition = ?2
               AND envelope_json LIKE ?3
             ORDER BY received_at_ms DESC",
        )
        .map_err(|error| format!("prepare Discord inbound lookup: {error}"))?;
    let mut rows = statement
        .query((account_id, disposition, like))
        .map_err(|error| format!("query Discord inbound lookup: {error}"))?;
    while let Some(row) = rows
        .next()
        .map_err(|error| format!("read Discord inbound lookup: {error}"))?
    {
        let sender_id: Option<String> = row
            .get(3)
            .map_err(|error| format!("read Discord sender id: {error}"))?;
        let Some(sender_id) = sender_id else { continue };
        if expected_sender.is_some_and(|expected| expected != sender_id) {
            continue;
        }
        return Ok(Some(ProviderInbound {
            provider_event_id: row
                .get(0)
                .map_err(|error| format!("read Discord provider event id: {error}"))?,
            conversation_id: row
                .get(1)
                .map_err(|error| format!("read Discord conversation id: {error}"))?,
            thread_id: row
                .get(2)
                .map_err(|error| format!("read Discord thread id: {error}"))?,
            sender_id,
            ingress_id: row
                .get(4)
                .map_err(|error| format!("read Discord ingress id: {error}"))?,
            job_id: row
                .get(5)
                .map_err(|error| format!("read Discord job id: {error}"))?,
        }));
    }
    Ok(None)
}

#[derive(Debug)]
struct ProviderOutbound {
    provider_message_id: String,
    thread_id: Option<String>,
}

fn find_outbound(
    db: &Path,
    account_id: &str,
    conversation_id: &str,
    expected_reply: &str,
) -> Result<Option<ProviderOutbound>, String> {
    if !db.is_file() {
        return Ok(None);
    }
    let connection = read_only(db)?;
    let like = format!("%{expected_reply}%");
    connection
        .query_row(
            "SELECT provider_event_id, thread_id
             FROM channel_events
             WHERE account_id = ?1
               AND direction = 'outbound'
               AND conversation_id = ?2
               AND envelope_json LIKE ?3
               AND provider_event_id NOT LIKE 'local:%'
             ORDER BY received_at_ms DESC LIMIT 1",
            (account_id, conversation_id, like),
            |row| {
                Ok(ProviderOutbound {
                    provider_message_id: row.get(0)?,
                    thread_id: row.get(1)?,
                })
            },
        )
        .optional()
        .map_err(|error| format!("read exact durable Discord outbound: {error}"))
}

fn count_events(
    db: &Path,
    account_id: &str,
    direction: &str,
    disposition: &str,
    marker: &str,
) -> Result<i64, String> {
    if !db.is_file() {
        return Ok(0);
    }
    let connection = read_only(db)?;
    let like = format!("%{marker}%");
    connection
        .query_row(
            "SELECT COUNT(*) FROM channel_events
             WHERE account_id = ?1 AND direction = ?2 AND disposition = ?3
               AND envelope_json LIKE ?4",
            (account_id, direction, disposition, like),
            |row| row.get(0),
        )
        .map_err(|error| format!("count Discord events: {error}"))
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
            .ok_or_else(|| format!("Discord account {account_id} disappeared"))?;
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
            return Err(
                "resident daemon rejected the Discord token or Gateway intents; enable the Message Content intent and verify the bot configuration"
                    .to_string(),
            );
        }
        if fresh && last_health == "connected" {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "resident daemon never reported a fresh connected Discord Gateway within {}s; last health {last_health}, probe {last_probe:?}",
        SERVICE_WAIT.as_secs()
    ))
}

fn wait_for_inbound(
    db: &Path,
    account_id: &str,
    marker: &str,
    disposition: &str,
    expected_sender: Option<&str>,
    deadline: Instant,
) -> Result<ProviderInbound, String> {
    while Instant::now() < deadline {
        if let Some(row) = find_inbound(db, account_id, marker, disposition, expected_sender)? {
            return Ok(row);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "no Discord {disposition} event containing {marker:?} arrived within {}s",
        PROVIDER_WAIT.as_secs()
    ))
}

#[derive(Debug, Clone)]
struct DiscordIdentity {
    id: String,
    username: String,
}

#[derive(Debug, Clone)]
struct DiscordMessage {
    id: String,
    channel_id: String,
    content: String,
    author_id: String,
}

fn discord_client() -> Result<reqwest::Client, String> {
    little_monkey_lib::egress::hardened()
        .build()
        .map_err(|_| "could not build hardened Discord observer client".to_string())
}

async fn discord_identity(token: &str) -> Result<DiscordIdentity, String> {
    let client = discord_client()?;
    let request = client
        .get(format!("{API_BASE}/users/@me"))
        .header("Authorization", format!("Bot {token}"))
        .header("User-Agent", "LittleMonkeyProviderE2E/1.0");
    let response = little_monkey_lib::egress::send(request)
        .await
        .map_err(|_| "Discord users/@me request failed".to_string())?;
    let status = response.status();
    let payload: serde_json::Value = response
        .json()
        .await
        .map_err(|_| "Discord users/@me returned invalid JSON".to_string())?;
    if !status.is_success() {
        return Err(format!(
            "Discord rejected one of the operator-supplied bot tokens (HTTP {status}): {}",
            payload
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("no provider message")
        ));
    }
    let id = payload
        .get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Discord users/@me response had no id".to_string())?;
    let username = payload
        .get("username")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("discord-bot");
    Ok(DiscordIdentity {
        id: id.to_string(),
        username: username.to_string(),
    })
}

fn parse_discord_message(payload: &serde_json::Value) -> Result<DiscordMessage, String> {
    let id = payload
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Discord message response had no id".to_string())?;
    let channel_id = payload
        .get("channel_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Discord message response had no channel_id".to_string())?;
    let content = payload
        .get("content")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let author_id = payload
        .get("author")
        .and_then(|author| author.get("id"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Discord message response had no author id".to_string())?;
    Ok(DiscordMessage {
        id: id.to_string(),
        channel_id: channel_id.to_string(),
        content: content.to_string(),
        author_id: author_id.to_string(),
    })
}

async fn discord_send(token: &str, channel_id: &str, content: &str) -> Result<DiscordMessage, String> {
    let client = discord_client()?;
    let request = client
        .post(format!("{API_BASE}/channels/{channel_id}/messages"))
        .header("Authorization", format!("Bot {token}"))
        .header("User-Agent", "LittleMonkeyProviderE2E/1.0")
        .json(&serde_json::json!({
            "content": content,
            "allowed_mentions": { "parse": ["users"] }
        }));
    let response = little_monkey_lib::egress::send(request)
        .await
        .map_err(|_| "Discord external sender request failed".to_string())?;
    let status = response.status();
    let payload: serde_json::Value = response
        .json()
        .await
        .map_err(|_| "Discord create-message returned invalid JSON".to_string())?;
    if !status.is_success() {
        return Err(format!(
            "Discord refused the external bot message in channel {channel_id} (HTTP {status}): {}",
            payload
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("no provider message")
        ));
    }
    parse_discord_message(&payload)
}

async fn discord_get_message(
    token: &str,
    channel_id: &str,
    message_id: &str,
) -> Result<DiscordMessage, String> {
    let client = discord_client()?;
    let request = client
        .get(format!(
            "{API_BASE}/channels/{channel_id}/messages/{message_id}"
        ))
        .header("Authorization", format!("Bot {token}"))
        .header("User-Agent", "LittleMonkeyProviderE2E/1.0");
    let response = little_monkey_lib::egress::send(request)
        .await
        .map_err(|_| "Discord independent read request failed".to_string())?;
    let status = response.status();
    let payload: serde_json::Value = response
        .json()
        .await
        .map_err(|_| "Discord get-message returned invalid JSON".to_string())?;
    if !status.is_success() {
        return Err(format!(
            "independent Discord bot could not read message {message_id} from channel {channel_id} (HTTP {status}): {}",
            payload
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("no provider message")
        ));
    }
    parse_discord_message(&payload)
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
    bot_token: String,
    external_bot_token: String,
    channel_id: String,
}

impl LiveConfig {
    fn from_env() -> Result<Self, String> {
        fn required(name: &str) -> Result<String, String> {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| format!("{name} is required"))
        }
        Ok(Self {
            bot_token: required(BOT_TOKEN_ENV)?,
            external_bot_token: required(EXTERNAL_BOT_TOKEN_ENV)?,
            channel_id: required(CHANNEL_ID_ENV)?,
        })
    }
}

async fn run_case(config: &LiveConfig) -> Result<(), String> {
    if std::env::var(REQUIRE_ENV).as_deref() != Ok("1") {
        return Err(format!(
            "refusing to install a real OS service and use real Discord accounts unless {REQUIRE_ENV}=1"
        ));
    }

    let tested = discord_identity(&config.bot_token).await?;
    let external = discord_identity(&config.external_bot_token).await?;
    if tested.id == external.id {
        return Err("the Discord bot under test and the external sender must be different bot identities".to_string());
    }

    let stamp = unique();
    let approval_marker = format!("lm-discord-denied-{stamp:x}");
    let execution_marker = format!("lm-discord-run-{stamp:x}");
    let expected_reply = format!("{REPLY_PREFIX} {execution_marker}");
    let model = ModelFixture::spawn(execution_marker.clone())?;
    let mut profile: Option<String> = None;
    let mut account_id: Option<String> = None;

    let result: Result<(), String> = async {
        let created = create_profile()?;
        profile = Some(created.clone());
        let workspace = std::env::temp_dir().join(format!("lm-discord-e2e-{stamp:x}"));
        std::fs::create_dir_all(&workspace)
            .map_err(|error| format!("create workspace {}: {error}", workspace.display()))?;
        write_recipe(&created, &workspace, &model.base)?;

        let account = add_account(&created)?;
        account_id = Some(account.clone());
        require_cli_stdin(
            &created,
            &["channels", "set-token", &account],
            &format!("{}\n", config.bot_token),
        )?;
        // Discord pairing is intentionally DM-only. For a guild-channel E2E
        // we prove the equivalent access boundary with the group allow-list:
        // the real provider identity is denied first, then explicitly approved.
        require_cli(
            Some(&created),
            &[
                "channels", "policy", &account,
                "--direct", "pairing",
                "--group", "allow_list",
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
        let denied_text = format!("<@{}> {}", tested.id, approval_marker);
        let denied_provider = discord_send(
            &config.external_bot_token,
            &config.channel_id,
            &denied_text,
        )
        .await?;
        if denied_provider.author_id != external.id || denied_provider.channel_id != config.channel_id {
            return Err("Discord did not attribute the pre-approval message to the independent external bot in the configured channel".to_string());
        }

        let denied = wait_for_inbound(
            &db,
            &account,
            &approval_marker,
            "ignored",
            None,
            Instant::now() + PROVIDER_WAIT,
        )?;
        if denied.provider_event_id != denied_provider.id {
            return Err(format!(
                "production Gateway event id {} did not match Discord's create-message id {}",
                denied.provider_event_id, denied_provider.id
            ));
        }
        if denied.sender_id != external.id {
            return Err(format!(
                "production Gateway normalized sender {} but Discord says the external bot is {}",
                denied.sender_id, external.id
            ));
        }
        if denied.target_channel() != config.channel_id {
            return Err(format!(
                "production Gateway normalized target {} but the external bot posted to {}",
                denied.target_channel(), config.channel_id
            ));
        }
        if denied.ingress_id.is_some() || denied.job_id.is_some() {
            return Err("unapproved Discord sender unexpectedly obtained an ingress/job".to_string());
        }
        if !model.requests().is_empty() {
            return Err("unapproved Discord sender reached the model before approval".to_string());
        }
        if count_events(&db, &account, "inbound", "ignored", &approval_marker)? != 1 {
            return Err("the unapproved Discord provider event was not durably deduplicated to exactly one ignored row".to_string());
        }

        require_cli(
            Some(&created),
            &["channels", "approve", &account, &denied.sender_id],
        )?;

        let execution_text = format!("<@{}> {}", tested.id, execution_marker);
        let execution_provider = discord_send(
            &config.external_bot_token,
            &config.channel_id,
            &execution_text,
        )
        .await?;
        let accepted = wait_for_inbound(
            &db,
            &account,
            &execution_marker,
            "accepted",
            Some(&external.id),
            Instant::now() + PROVIDER_WAIT,
        )?;
        if accepted.provider_event_id != execution_provider.id {
            return Err(format!(
                "accepted Gateway event id {} did not match Discord's execution message id {}",
                accepted.provider_event_id, execution_provider.id
            ));
        }
        if accepted.target_channel() != config.channel_id {
            return Err(format!(
                "accepted Discord target {} did not match configured channel {}",
                accepted.target_channel(), config.channel_id
            ));
        }
        let ingress_id = accepted
            .ingress_id
            .as_deref()
            .ok_or_else(|| "accepted Discord event had no ingress id".to_string())?;
        let job_id = accepted
            .job_id
            .as_deref()
            .ok_or_else(|| "accepted Discord event had no job id".to_string())?;
        eprintln!(
            "Real Discord message {} from independent bot {} became ingress {} / job {} in installed daemon pid {}",
            accepted.provider_event_id, external.username, ingress_id, job_id, restarted_pid
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
                    "installed Discord turn did not complete the real model/tool loop within {}s ({} model requests)",
                    AGENT_WAIT.as_secs(),
                    requests.len()
                ));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        let provider_deadline = Instant::now() + PROVIDER_WAIT;
        let outbound = loop {
            if let Some(outbound) = find_outbound(
                &db,
                &account,
                &accepted.conversation_id,
                &expected_reply,
            )? {
                break outbound;
            }
            if Instant::now() >= provider_deadline {
                return Err(format!(
                    "generated Discord reply never became a provider-named durable outbound event within {}s",
                    PROVIDER_WAIT.as_secs()
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        if outbound.provider_message_id.starts_with("local:")
            || outbound.provider_message_id.trim().is_empty()
        {
            return Err(format!(
                "Discord reply has no real provider message id: {:?}",
                outbound.provider_message_id
            ));
        }
        let outbound_channel = outbound
            .thread_id
            .as_deref()
            .unwrap_or(&accepted.conversation_id);
        if outbound_channel != config.channel_id {
            return Err(format!(
                "Discord outbound targeted {outbound_channel}, expected {}",
                config.channel_id
            ));
        }

        let observed = discord_get_message(
            &config.external_bot_token,
            outbound_channel,
            &outbound.provider_message_id,
        )
        .await?;
        if observed.author_id != tested.id {
            return Err(format!(
                "independent Discord observer says reply author is {}, expected tested bot {}",
                observed.author_id, tested.id
            ));
        }
        if observed.content != expected_reply {
            return Err(format!(
                "independent Discord observation did not match generated reply; expected {expected_reply:?}, got {:?}",
                observed.content
            ));
        }
        if observed.channel_id != config.channel_id {
            return Err(format!(
                "independent Discord observer read the reply from {}, expected {}",
                observed.channel_id, config.channel_id
            ));
        }
        if count_events(&db, &account, "inbound", "accepted", &execution_marker)? != 1 {
            return Err("the real Discord execution message did not deduplicate to exactly one accepted inbound row".to_string());
        }
        if count_events(&db, &account, "outbound", "accepted", &expected_reply)? != 1 {
            return Err("the generated Discord reply did not produce exactly one durable outbound row".to_string());
        }

        eprintln!(
            "Discord E2E passed: Gateway inbound from {} -> installed daemon -> real agent/tool/outbox -> REST send by {} -> independent {} exact-text read (message {}).",
            external.username,
            tested.username,
            external.username,
            outbound.provider_message_id
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
            eprintln!("Discord installed-service E2E configuration error: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = run_case(&config).await {
        eprintln!("Discord installed-service E2E failed: {error}");
        std::process::exit(1);
    }
}
