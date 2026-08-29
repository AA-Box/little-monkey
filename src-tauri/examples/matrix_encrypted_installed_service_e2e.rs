//! Official Synapse + encrypted Matrix room -> native credential store ->
//! installed resident daemon -> agent -> encrypted Matrix reply acceptance.
//!
//! The external side is itself `matrix-sdk` 0.18, the same maintained SDK the
//! production adapter uses. It logs in as a second real Matrix device, sends a
//! marker into a real `m.room.encryption` room, proves Synapse stores that event
//! as `m.room.encrypted`, and only accepts the reply when the SDK delivers a
//! decrypted text event with `EncryptionInfo` present.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use matrix_sdk::config::SyncSettings;
use matrix_sdk::deserialized_responses::EncryptionInfo;
use matrix_sdk::ruma::events::room::message::{
    MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
};
use matrix_sdk::ruma::events::AnyMessageLikeEventContent;
use matrix_sdk::ruma::RoomId;
use matrix_sdk::{Client, Room, RoomState};

const PROFILE_ENV: &str = "LITTLE_MONKEY_PROFILE";
const REQUIRE_ENV: &str = "LITTLE_MONKEY_REQUIRE_MATRIX_INSTALLED_SERVICE_E2E";
const RECIPE: &str = "matrix-encrypted-installed-service-e2e";
const REPLY_PREFIX: &str = "little-monkey matrix installed-service reply";
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

fn run_cli_with_stdin(
    profile: Option<&str>,
    args: &[&str],
    stdin: Option<&str>,
) -> Result<Output, String> {
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
    let name = format!("Matrix encrypted installed-service E2E {}", unique());
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

fn add_account(
    profile: &str,
    homeserver: &str,
    user_id: &str,
    device_id: &str,
) -> Result<String, String> {
    let label = format!("matrix-e2e-{}", unique());
    let config = serde_json::json!({
        "homeserver_url": homeserver,
        "user_id": user_id,
        "device_id": device_id,
    })
    .to_string();
    let output = require_cli(
        Some(profile),
        &[
            "channels", "add", "matrix", &label, "--config", &config, "--json",
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
                                    "id": "call_matrix_installed_1",
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
        "target": { "local_url": model_base, "model": "matrix-e2e-fixture" },
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
            return Err("resident daemon could not build/connect the Matrix account".to_string());
        }
        if fresh && last_health == "connected" {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "resident service never reported connected Matrix health of its own within {}s\nlast account health: {last_health} (last probe {last_probe:?}, waiting for one at or after {since_ms})",
        SERVICE_WAIT.as_secs()
    ))
}

async fn raw_event_is_encrypted(
    homeserver: &str,
    access_token: &str,
    room_id: &str,
    event_id: &str,
) -> Result<(), String> {
    let response = reqwest::Client::new()
        .get(format!(
            "{homeserver}/_matrix/client/v3/rooms/{}/event/{}",
            urlencoding(room_id),
            urlencoding(event_id)
        ))
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|error| format!("fetch raw Matrix event: {error}"))?;
    let status = response.status();
    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("raw Matrix event JSON: {error}"))?;
    if !status.is_success() {
        return Err(format!(
            "Synapse refused raw event read ({status}): {value}"
        ));
    }
    if value.get("type").and_then(serde_json::Value::as_str) != Some("m.room.encrypted") {
        return Err(format!(
            "event {event_id} was not encrypted on the server: {value}"
        ));
    }
    Ok(())
}

fn urlencoding(value: &str) -> String {
    // Matrix ids need path-segment escaping. `url` is already a direct
    // dependency; use its serializer rather than adding a percent crate just
    // for this acceptance.
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    form.append_key_only(value);
    form.finish().replace('+', "%20")
}

async fn external_encrypted_round_trip(
    homeserver: &str,
    username: &str,
    password: &str,
    bootstrap_token: &str,
    room_id_text: &str,
    marker: &str,
) -> Result<(), String> {
    let homeserver_url = url::Url::parse(homeserver)
        .map_err(|error| format!("parse Matrix homeserver URL: {error}"))?;
    let client = Client::new(homeserver_url)
        .await
        .map_err(|error| format!("build external Matrix SDK client: {error}"))?;
    client
        .matrix_auth()
        .login_username(username, password)
        .initial_device_display_name("Little Monkey Matrix E2E observer")
        .await
        .map_err(|error| format!("external Matrix SDK login: {error}"))?;

    let initial = client
        .sync_once(SyncSettings::default())
        .await
        .map_err(|error| format!("initial external Matrix sync: {error}"))?;
    let room_id = RoomId::parse(room_id_text)
        .map_err(|error| format!("invalid Matrix E2E room id: {error}"))?;
    let room = client
        .get_room(&room_id)
        .ok_or_else(|| format!("encrypted room {room_id_text} is absent after initial sync"))?;
    if room.state() != RoomState::Joined {
        return Err(format!(
            "external Matrix SDK is not joined to {room_id_text}"
        ));
    }

    let expected = format!("{REPLY_PREFIX} {marker}");
    let expected_for_handler = expected.clone();
    let room_for_handler = room_id.clone();
    let (reply_tx, mut reply_rx) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();
    client.add_event_handler(
        move |event: OriginalSyncRoomMessageEvent,
              room: Room,
              encryption_info: Option<EncryptionInfo>| {
            let tx = reply_tx.clone();
            let expected = expected_for_handler.clone();
            let expected_room = room_for_handler.clone();
            async move {
                // `&*` rather than `as_ref()`: an `OwnedRoomId` is both
                // `AsRef<RoomId>` and `AsRef<str>`, so the target of the
                // comparison is ambiguous and the example does not compile.
                if room.room_id() != &*expected_room {
                    return;
                }
                let MessageType::Text(text) = event.content.msgtype else {
                    return;
                };
                if text.body.contains(&expected) {
                    let _ = tx.send((event.event_id.to_string(), encryption_info.is_some()));
                }
            }
        },
    );

    // The room has m.room.encryption state. `matrix-sdk` therefore performs
    // the Olm/Megolm key sharing and sends m.room.encrypted; the raw event
    // assertion below independently proves no plaintext message was stored.
    let sent = room
        .send(AnyMessageLikeEventContent::RoomMessage(
            RoomMessageEventContent::text_plain(marker),
        ))
        .await
        .map_err(|error| format!("external encrypted Matrix send: {error}"))?;
    let inbound_event_id = sent.response.event_id.to_string();
    raw_event_is_encrypted(homeserver, bootstrap_token, room_id_text, &inbound_event_id).await?;

    let deadline = Instant::now() + REPLY_WAIT;
    let mut token = initial.next_batch;
    loop {
        if Instant::now() >= deadline {
            return Err(format!(
                "external Matrix SDK did not decrypt {expected:?} within {}s",
                REPLY_WAIT.as_secs()
            ));
        }
        let response = client
            .sync_once(
                SyncSettings::default()
                    .token(token)
                    .timeout(Duration::from_secs(20)),
            )
            .await
            .map_err(|error| format!("external Matrix sync while waiting for reply: {error}"))?;
        token = response.next_batch;
        while let Ok((event_id, encrypted)) = reply_rx.try_recv() {
            if !encrypted {
                return Err(format!(
                    "Matrix SDK delivered agent reply {event_id} without EncryptionInfo"
                ));
            }
            raw_event_is_encrypted(homeserver, bootstrap_token, room_id_text, &event_id).await?;
            eprintln!("external Matrix device decrypted encrypted agent reply {event_id}");
            return Ok(());
        }
    }
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
            "expected one accepted inbound and one outbound event, got inbound={inbound}, outbound={outbound}: {payload}"
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

async fn run_case() -> Result<(), String> {
    if std::env::var(REQUIRE_ENV).as_deref() != Ok("1") {
        return Err(format!(
            "refusing to install a real OS service and mutate Matrix unless {REQUIRE_ENV}=1"
        ));
    }
    let homeserver = std::env::var("MATRIX_E2E_HOMESERVER")
        .unwrap_or_else(|_| "http://localhost:8008".to_string());
    let bot_user_id = std::env::var("MATRIX_E2E_BOT_USER_ID")
        .map_err(|_| "MATRIX_E2E_BOT_USER_ID is required".to_string())?;
    let bot_device_id = std::env::var("MATRIX_E2E_BOT_DEVICE_ID")
        .map_err(|_| "MATRIX_E2E_BOT_DEVICE_ID is required".to_string())?;
    let bot_token = std::env::var("MATRIX_E2E_BOT_TOKEN")
        .map_err(|_| "MATRIX_E2E_BOT_TOKEN is required".to_string())?;
    let external_user = std::env::var("MATRIX_E2E_EXTERNAL_USER")
        .map_err(|_| "MATRIX_E2E_EXTERNAL_USER is required".to_string())?;
    let external_password = std::env::var("MATRIX_E2E_EXTERNAL_PASSWORD")
        .map_err(|_| "MATRIX_E2E_EXTERNAL_PASSWORD is required".to_string())?;
    let external_token = std::env::var("MATRIX_E2E_EXTERNAL_TOKEN")
        .map_err(|_| "MATRIX_E2E_EXTERNAL_TOKEN is required".to_string())?;
    let room_id = std::env::var("MATRIX_E2E_ROOM_ID")
        .map_err(|_| "MATRIX_E2E_ROOM_ID is required".to_string())?;

    let stamp = unique();
    let marker = format!("lm-matrix-installed-{stamp:x}");
    let model = ModelFixture::spawn(marker.clone())?;
    let mut profile: Option<String> = None;
    let mut account_id: Option<String> = None;

    let result: Result<(), String> = async {
        let created = create_profile()?;
        profile = Some(created.clone());
        let workspace = std::env::temp_dir().join(format!("lm-matrix-e2e-workspace-{stamp:x}"));
        std::fs::create_dir_all(&workspace)
            .map_err(|error| format!("create workspace {}: {error}", workspace.display()))?;
        write_recipe(&created, &workspace, &model.base)?;

        let account = add_account(&created, &homeserver, &bot_user_id, &bot_device_id)?;
        account_id = Some(account.clone());
        require_cli_stdin(
            &created,
            &["channels", "set-token", &account],
            &format!("{bot_token}\n"),
        )?;
        require_cli(
            Some(&created),
            &["channels", "policy", &account, "--direct", "open", "--group", "open", "--activation", "always"],
        )?;
        require_cli(
            Some(&created),
            &["channels", "add-route", RECIPE, "--account", &account, "--json"],
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
        eprintln!(
            "installed Matrix daemon ready (initial pid {first_pid}, after restart {restarted_pid})"
        );

        external_encrypted_round_trip(
            &homeserver,
            &external_user,
            &external_password,
            &external_token,
            &room_id,
            &marker,
        )
        .await?;

        let requests = model.requests();
        if !requests.iter().any(|request| request.contains(&marker)) {
            return Err(format!(
                "the installed agent never sent the decrypted Matrix marker to the model: {requests:?}"
            ));
        }
        if !requests.iter().any(|request| request.contains(r#""name":"send_message""#)) {
            return Err("send_message was never offered to the installed agent".to_string());
        }
        if requests.len() < 2 {
            return Err(format!(
                "agent never returned to the model with the send_message result: {} request(s)",
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
    if let Err(error) = run_case().await {
        eprintln!("Matrix encrypted installed-service E2E failed: {error}");
        std::process::exit(1);
    }
}
