//! Real IRC -> installed resident daemon -> agent -> real IRC acceptance.
//!
//! This is deliberately a black-box harness. It configures Little Monkey only
//! through `monkey-cli`, installs the real user service, connects a second IRC
//! client over TLS, and proves that the message that client sends becomes a
//! durable turn and an agent-produced reply that the second client receives.
//!
//! The only deterministic component is the OpenAI-compatible model origin. It
//! is the same `target.local_url` seam a real recipe uses; it cannot write the
//! outbox or speak IRC. The reply exists only if the installed daemon queues a
//! real run and the production agent dispatches `send_message`.
//!
//! This harness intentionally needs a real, TLS IRC network chosen by the
//! operator. The accompanying workflow compiles it on every PR but only runs
//! the network acceptance on `workflow_dispatch`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROFILE_ENV: &str = "LITTLE_MONKEY_PROFILE";
const REQUIRE_ENV: &str = "LITTLE_MONKEY_REQUIRE_IRC_INSTALLED_SERVICE_E2E";
const RECIPE: &str = "irc-installed-service-e2e";
const REPLY_PREFIX: &str = "little-monkey irc installed-service reply";
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);
const SERVICE_WAIT: Duration = Duration::from_secs(120);
const IRC_WAIT: Duration = Duration::from_secs(240);

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
    let name = format!("IRC installed-service E2E {}", unique());
    let output = require_cli(None, &["profiles", "create", &name, "--json"])?;
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("profile JSON was invalid: {error}\n{}", output_text(&output)))?;
    payload
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("profile JSON had no id: {}", String::from_utf8_lossy(&output.stdout)))
}

fn add_irc_account(
    profile: &str,
    server: &str,
    port: u16,
    nick: &str,
    channel: &str,
) -> Result<String, String> {
    let label = format!("irc-e2e-{}", unique());
    let config = serde_json::json!({
        "server": server,
        "port": port,
        "nick": nick,
        "channels": [channel],
        "use_sasl": false,
    })
    .to_string();
    let output = require_cli(
        Some(profile),
        &["channels", "add", "irc", &label, "--config", &config, "--json"],
    )?;
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("account JSON was invalid: {error}\n{}", output_text(&output)))?;
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
                                    "id": "call_irc_installed_1",
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
        "target": { "local_url": model_base, "model": "irc-e2e-fixture" },
        "workspace": workspace.to_string_lossy(),
        // Bypass is a real production permission mode. It keeps this acceptance
        // unattended; the route's frozen reply grant still constrains
        // send_message to the conversation that caused this run.
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

fn wait_for_service_and_account(profile: &str, account_id: &str) -> Result<u64, String> {
    let deadline = Instant::now() + SERVICE_WAIT;
    let mut last_status = String::new();
    let mut last_health = "unknown".to_string();
    while Instant::now() < deadline {
        if let Ok(status_output) = run_cli(Some(profile), &["daemon", "status", "--json"]) {
            if status_output.status.success() {
                if let Ok(status) = serde_json::from_slice::<serde_json::Value>(&status_output.stdout)
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
                        let listed = require_cli(Some(profile), &["channels", "list", "--json"])?;
                        let payload: serde_json::Value = serde_json::from_slice(&listed.stdout)
                            .map_err(|error| format!("channels list JSON was invalid: {error}"))?;
                        let account = payload
                            .get("accounts")
                            .and_then(serde_json::Value::as_array)
                            .and_then(|accounts| {
                                accounts.iter().find(|account| {
                                    account.get("account_id").and_then(serde_json::Value::as_str)
                                        == Some(account_id)
                                })
                            })
                            .ok_or_else(|| format!("account {account_id} disappeared"))?;
                        last_health = account
                            .get("health")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("unknown")
                            .to_string();
                        if last_health == "error" {
                            return Err("resident daemon could not build/connect the IRC account"
                                .to_string());
                        }
                        if last_health == "connected" {
                            return Ok(pid);
                        }
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "resident service never reached connected IRC health within {}s\nlast daemon status: {last_status}\nlast account health: {last_health}",
        SERVICE_WAIT.as_secs()
    ))
}

fn write_irc_line<S: Write>(stream: &mut S, line: &str) -> Result<(), String> {
    stream
        .write_all(line.as_bytes())
        .and_then(|_| stream.write_all(b"\r\n"))
        .and_then(|_| stream.flush())
        .map_err(|error| format!("IRC write {line:?}: {error}"))
}

fn read_irc_line<S: Read>(stream: &mut S) -> std::io::Result<Option<String>> {
    let mut bytes = Vec::new();
    loop {
        let mut one = [0u8; 1];
        match stream.read(&mut one) {
            Ok(0) if bytes.is_empty() => return Ok(None),
            Ok(0) => break,
            Ok(_) if one[0] == b'\n' => break,
            Ok(_) if one[0] != b'\r' => bytes.push(one[0]),
            Ok(_) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(Some(String::from_utf8_lossy(&bytes).to_string()))
}

fn handle_ping<S: Write>(stream: &mut S, line: &str) -> Result<bool, String> {
    if let Some(payload) = line.strip_prefix("PING ") {
        write_irc_line(stream, &format!("PONG {payload}"))?;
        return Ok(true);
    }
    Ok(false)
}

fn real_external_irc_round_trip(
    server: &str,
    port: u16,
    external_nick: &str,
    bot_nick: &str,
    channel: &str,
    marker: &str,
) -> Result<String, String> {
    let tcp = TcpStream::connect((server, port))
        .map_err(|error| format!("external IRC TCP connect to {server}:{port}: {error}"))?;
    tcp.set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| format!("set IRC read timeout: {error}"))?;
    tcp.set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| format!("set IRC write timeout: {error}"))?;

    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let name = rustls::pki_types::ServerName::try_from(server.to_string())
        .map_err(|error| format!("invalid IRC TLS server name {server}: {error}"))?;
    let connection = rustls::ClientConnection::new(config, name)
        .map_err(|error| format!("build IRC TLS client: {error}"))?;
    let mut stream = rustls::StreamOwned::new(connection, tcp);

    write_irc_line(&mut stream, &format!("NICK {external_nick}"))?;
    write_irc_line(
        &mut stream,
        &format!("USER {external_nick} 0 * :Little Monkey IRC E2E"),
    )?;

    let registered_deadline = Instant::now() + Duration::from_secs(60);
    let mut registered = false;
    while Instant::now() < registered_deadline {
        match read_irc_line(&mut stream) {
            Ok(Some(line)) => {
                if handle_ping(&mut stream, &line)? {
                    continue;
                }
                if line.contains(" 001 ") {
                    registered = true;
                    break;
                }
                if line.contains(" 433 ") {
                    return Err(format!("external IRC nick {external_nick} is already in use"));
                }
            }
            Ok(None) => return Err("IRC server closed before registration".to_string()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(format!("IRC registration read: {error}")),
        }
    }
    if !registered {
        return Err("external IRC client did not receive 001 welcome".to_string());
    }

    write_irc_line(&mut stream, &format!("JOIN {channel}"))?;
    let join_deadline = Instant::now() + Duration::from_secs(60);
    let mut saw_bot = false;
    let mut names_done = false;
    while Instant::now() < join_deadline {
        match read_irc_line(&mut stream) {
            Ok(Some(line)) => {
                if handle_ping(&mut stream, &line)? {
                    continue;
                }
                if line.contains(" 353 ") && line.contains(channel) {
                    saw_bot |= line.split_whitespace().any(|word| {
                        word.trim_start_matches([':', '@', '+', '%', '~', '&']) == bot_nick
                    });
                }
                if line.contains(" 366 ") && line.contains(channel) {
                    names_done = true;
                    break;
                }
            }
            Ok(None) => return Err("IRC server closed while joining channel".to_string()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(format!("IRC join read: {error}")),
        }
    }
    if !names_done {
        return Err(format!("IRC channel {channel} never completed NAMES"));
    }
    if !saw_bot {
        return Err(format!(
            "installed Little Monkey nick {bot_nick} was not present in {channel} before the test message"
        ));
    }

    write_irc_line(&mut stream, &format!("PRIVMSG {channel} :{marker}"))?;

    let expected = format!("{REPLY_PREFIX} {marker}");
    let deadline = Instant::now() + IRC_WAIT;
    while Instant::now() < deadline {
        match read_irc_line(&mut stream) {
            Ok(Some(line)) => {
                if handle_ping(&mut stream, &line)? {
                    continue;
                }
                if line.contains(&format!("PRIVMSG {channel} :")) && line.contains(&expected) {
                    return Ok(line);
                }
            }
            Ok(None) => return Err("IRC server closed before the agent reply arrived".to_string()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(format!("IRC reply read: {error}")),
        }
    }
    Err(format!(
        "external IRC client did not observe {expected:?} within {}s",
        IRC_WAIT.as_secs()
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

    let accepted_inbound: Vec<&serde_json::Value> = events
        .iter()
        .filter(|event| {
            event.get("direction").and_then(serde_json::Value::as_str) == Some("inbound")
                && event.get("ingress_id").is_some_and(|value| !value.is_null())
                && event.get("job_id").is_some_and(|value| !value.is_null())
        })
        .collect();
    let outbound = events
        .iter()
        .filter(|event| {
            event.get("direction").and_then(serde_json::Value::as_str) == Some("outbound")
        })
        .count();

    if accepted_inbound.len() != 1 {
        return Err(format!(
            "expected exactly one durable accepted inbound event, got {}: {payload}",
            accepted_inbound.len()
        ));
    }
    if outbound != 1 {
        return Err(format!(
            "expected exactly one durable outbound event, got {outbound}: {payload}"
        ));
    }
    Ok(())
}

fn dump_diagnostics(profile: &str) {
    if let Ok(output) = run_cli(Some(profile), &["daemon", "status", "--json"]) {
        eprintln!("--- daemon status ---\n{}", output_text(&output));
    }
    if let Ok(output) = run_cli(Some(profile), &["channels", "list", "--json"]) {
        eprintln!("--- channels ---\n{}", output_text(&output));
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

fn run_case(server: &str, port: u16) -> Result<(), String> {
    if std::env::var(REQUIRE_ENV).as_deref() != Ok("1") {
        return Err(format!(
            "refusing to install a real OS service and contact a real IRC network unless {REQUIRE_ENV}=1"
        ));
    }
    if cfg!(target_os = "macos") {
        return Err("this acceptance currently targets Linux/Windows service semantics; macOS service acceptance remains tracked separately".to_string());
    }

    let stamp = unique();
    let suffix = format!("{:06x}", (stamp as u64) & 0x00ff_ffff);
    // Eight characters keeps the harness valid even on old networks with a
    // nine-character NICKLEN.
    let bot_nick = format!("lmB{suffix}");
    let external_nick = format!("lmU{suffix}");
    let channel = format!("#little-monkey-e2e-{stamp:x}");
    let marker = format!("lm-irc-installed-{stamp:x}");
    let model = ModelFixture::spawn(marker.clone())?;

    let mut profile: Option<String> = None;
    let mut account_id: Option<String> = None;
    let result = (|| -> Result<(), String> {
        let created = create_profile()?;
        profile = Some(created.clone());

        let workspace = std::env::temp_dir().join(format!("lm-irc-e2e-workspace-{stamp:x}"));
        std::fs::create_dir_all(&workspace)
            .map_err(|error| format!("create workspace {}: {error}", workspace.display()))?;
        write_recipe(&created, &workspace, &model.base)?;

        let account = add_irc_account(&created, server, port, &bot_nick, &channel)?;
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
        let first_pid = wait_for_service_and_account(&created, &account)?;

        // Prove the persistent socket is owned by a real resident lifecycle,
        // not merely by the process that configured it: stop/start and require
        // the service to reconnect before the external sender enters.
        require_cli(Some(&created), &["daemon", "stop"])?;
        require_cli(Some(&created), &["daemon", "start"])?;
        let restarted_pid = wait_for_service_and_account(&created, &account)?;
        if restarted_pid == u64::from(std::process::id()) {
            return Err("restarted daemon pid is the acceptance harness pid".to_string());
        }
        eprintln!(
            "installed IRC daemon ready (initial pid {first_pid}, after restart {restarted_pid})"
        );

        let observed = real_external_irc_round_trip(
            server,
            port,
            &external_nick,
            &bot_nick,
            &channel,
            &marker,
        )?;
        eprintln!("external IRC client observed: {observed}");

        let requests = model.requests();
        if !requests.iter().any(|request| request.contains(&marker)) {
            return Err(format!(
                "the installed agent never sent the real IRC message to the model: {requests:?}"
            ));
        }
        if !requests
            .iter()
            .any(|request| request.contains(r#"\"name\":\"send_message\""#))
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
            dump_diagnostics(profile);
        }
    }
    cleanup(profile.as_deref(), account_id.as_deref());
    result
}

fn real_main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let server = args
        .next()
        .ok_or_else(|| "usage: irc_installed_service_e2e <tls-server> [port]".to_string())?;
    let port = args
        .next()
        .map(|value| {
            value
                .parse::<u16>()
                .map_err(|error| format!("invalid IRC port {value:?}: {error}"))
        })
        .transpose()?
        .unwrap_or(6697);
    run_case(&server, port)
}

fn main() {
    if let Err(error) = real_main() {
        eprintln!("IRC installed-service E2E failed: {error}");
        std::process::exit(1);
    }
}
