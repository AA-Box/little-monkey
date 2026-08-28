//! Real Google Chat user -> Google-signed app interaction -> installed resident
//! daemon -> production agent -> Chat API -> independent user API observation.
//!
//! Google does not expose a Chat app-configuration API for changing the HTTP
//! interaction endpoint in this harness. After the isolated account is created,
//! the operator points the test Chat app at the printed callback and confirms
//! that one setup step. The message itself must then originate from a real
//! Google Chat user, not from this process.

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROFILE_ENV: &str = "LITTLE_MONKEY_PROFILE";
const REQUIRE_ENV: &str = "LITTLE_MONKEY_REQUIRE_GOOGLE_CHAT_INSTALLED_SERVICE_E2E";
const RECIPE: &str = "google-chat-installed-service-e2e";
const REPLY_PREFIX: &str = "little-monkey google-chat installed-service reply";
const CHAT_API_BASE: &str = "https://chat.googleapis.com";
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);
const SERVICE_WAIT: Duration = Duration::from_secs(120);
const INBOUND_WAIT: Duration = Duration::from_secs(600);
const OUTBOUND_WAIT: Duration = Duration::from_secs(180);

fn unique() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
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
        return Err(format!("{} is the zero-byte Tauri bootstrap placeholder", binary.display()));
    }
    let mut command = Command::new(binary);
    command.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
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
    let name = format!("Google Chat installed-service E2E {}", unique());
    let output = require_cli(None, &["profiles", "create", &name, "--json"])?;
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("profile JSON was invalid: {error}"))?;
    payload
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("profile JSON had no id: {payload}"))
}

fn add_account(
    profile: &str,
    project_number: &str,
    bot_user_name: &str,
) -> Result<String, String> {
    let label = format!("google-chat-e2e-{}", unique());
    let config = serde_json::json!({
        "project_number": project_number,
        "bot_user_name": bot_user_name,
    })
    .to_string();
    let output = require_cli(
        Some(profile),
        &[
            "channels", "add", "google_chat", &label,
            "--config", &config, "--json",
        ],
    )?;
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("account JSON was invalid: {error}"))?;
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
                let Some((head, body)) = read_http_request(&mut stream) else { continue };
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
                                    "id": "call_gchat_installed_1",
                                    "type": "function",
                                    "function": { "name": "send_message", "arguments": arguments }
                                }] }
                            }]
                        }),
                        serde_json::json!({
                            "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }]
                        }),
                    ])
                };
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Ok(Self { base: format!("http://127.0.0.1:{port}"), seen })
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
                        chunked = headers.to_ascii_lowercase().contains("transfer-encoding: chunked");
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
                    if complete { break; }
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
    for frame in frames { body.push_str(&format!("data: {frame}\n\n")); }
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
    let path = recipes.join(format!("{RECIPE}.json"));
    let recipe = serde_json::json!({
        "version": 1,
        "name": RECIPE,
        "target": { "local_url": model_base, "model": "google-chat-e2e-fixture" },
        "workspace": workspace.to_string_lossy(),
        "permission_mode": "bypass",
        "prompt": "{{message}}",
        "params": { "message": null },
        "max_iterations": 4,
        "timeout_seconds": 180,
    });
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
struct InboundProof {
    provider_event_id: String,
    ingress_id: String,
    job_id: String,
}

fn find_inbound(
    db: &Path,
    account_id: &str,
    sender_name: &str,
    marker: &str,
) -> Result<Option<InboundProof>, String> {
    if !db.is_file() { return Ok(None); }
    let connection = read_only(db)?;
    let like = format!("%{marker}%");
    connection
        .query_row(
            "SELECT provider_event_id, ingress_id, job_id
             FROM channel_events
             WHERE account_id = ?1
               AND direction = 'inbound'
               AND sender_id = ?2
               AND envelope_json LIKE ?3
               AND ingress_id IS NOT NULL
               AND job_id IS NOT NULL
             ORDER BY received_at_ms DESC LIMIT 1",
            (account_id, sender_name, like),
            |row| {
                Ok(InboundProof {
                    provider_event_id: row.get(0)?,
                    ingress_id: row.get(1)?,
                    job_id: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(|error| format!("read exact durable Google Chat inbound: {error}"))
}

fn find_outbound(
    db: &Path,
    account_id: &str,
    space_name: &str,
    expected_reply: &str,
) -> Result<Option<String>, String> {
    if !db.is_file() { return Ok(None); }
    let connection = read_only(db)?;
    let like = format!("%{expected_reply}%");
    connection
        .query_row(
            "SELECT provider_event_id
             FROM channel_events
             WHERE account_id = ?1
               AND direction = 'outbound'
               AND conversation_id = ?2
               AND provider_event_id NOT LIKE 'local:%'
               AND envelope_json LIKE ?3
             ORDER BY received_at_ms DESC LIMIT 1",
            (account_id, space_name, like),
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| format!("read exact durable Google Chat outbound: {error}"))
}

fn wait_for_service(profile: &str) -> Result<u64, String> {
    let deadline = Instant::now() + SERVICE_WAIT;
    let mut last = String::new();
    while Instant::now() < deadline {
        if let Ok(output) = run_cli(Some(profile), &["daemon", "status", "--json"]) {
            if output.status.success() {
                if let Ok(status) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                    last = status.to_string();
                    let running = status.get("service_running").and_then(serde_json::Value::as_bool).unwrap_or(false);
                    let heartbeat = status.get("heartbeat_fresh").and_then(serde_json::Value::as_bool).unwrap_or(false);
                    let pid = status.get("pid").and_then(serde_json::Value::as_u64).unwrap_or_default();
                    if running && heartbeat && pid != u64::from(std::process::id()) { return Ok(pid); }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "installed daemon never reached a fresh heartbeat within {}s; last status: {last}",
        SERVICE_WAIT.as_secs()
    ))
}

async fn observe_chat_message(
    client: &reqwest::Client,
    user_access_token: &str,
    message_name: &str,
    expected_reply: &str,
) -> Result<(), String> {
    if !message_name.starts_with("spaces/") || !message_name.contains("/messages/") {
        return Err(format!("Google Chat did not return a message resource name: {message_name:?}"));
    }
    let response = client
        .get(format!("{CHAT_API_BASE}/v1/{message_name}"))
        .bearer_auth(user_access_token)
        .send()
        .await
        .map_err(|error| format!("independent Google Chat user could not get reply: {error}"))?;
    let status = response.status();
    let payload: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("Google Chat observer response was not JSON: {error}"))?;
    if !status.is_success() {
        return Err(format!("Google Chat observer GET failed ({status}): {payload}"));
    }
    let text = payload
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if text == expected_reply {
        Ok(())
    } else {
        Err(format!(
            "independent Google Chat user read {message_name} but its text did not match; expected {expected_reply:?}, got {text:?}"
        ))
    }
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
    project_number: String,
    bot_user_name: String,
    service_account_email: String,
    service_account_private_key: String,
    public_base: String,
    space_name: String,
    external_user_name: String,
    external_user_access_token: String,
    webhook_port: u16,
}

impl LiveConfig {
    fn from_env() -> Result<Self, String> {
        fn required(name: &str) -> Result<String, String> {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| format!("{name} is required"))
        }
        let public_base = required("GCHAT_E2E_PUBLIC_BASE")?
            .trim_end_matches('/')
            .to_string();
        if !public_base.starts_with("https://") {
            return Err("GCHAT_E2E_PUBLIC_BASE must be a public https:// origin".to_string());
        }
        let space_name = required("GCHAT_E2E_SPACE_NAME")?;
        if !space_name.starts_with("spaces/") {
            return Err("GCHAT_E2E_SPACE_NAME must be a spaces/<id> resource name".to_string());
        }
        let webhook_port = std::env::var("GCHAT_E2E_WEBHOOK_PORT")
            .unwrap_or_else(|_| "38445".to_string())
            .parse::<u16>()
            .map_err(|_| "GCHAT_E2E_WEBHOOK_PORT must be a non-zero u16".to_string())?;
        if webhook_port == 0 { return Err("GCHAT_E2E_WEBHOOK_PORT must be non-zero".to_string()); }
        Ok(Self {
            project_number: required("GCHAT_E2E_PROJECT_NUMBER")?,
            bot_user_name: required("GCHAT_E2E_BOT_USER_NAME")?,
            service_account_email: required("GCHAT_E2E_SERVICE_ACCOUNT_EMAIL")?,
            service_account_private_key: required("GCHAT_E2E_SERVICE_ACCOUNT_PRIVATE_KEY")?,
            public_base,
            space_name,
            external_user_name: required("GCHAT_E2E_EXTERNAL_USER_NAME")?,
            external_user_access_token: required("GCHAT_E2E_EXTERNAL_USER_ACCESS_TOKEN")?,
            webhook_port,
        })
    }
}

async fn run_case(config: &LiveConfig) -> Result<(), String> {
    if std::env::var(REQUIRE_ENV).as_deref() != Ok("1") {
        return Err(format!(
            "refusing to install a real OS service and use live Google Chat credentials unless {REQUIRE_ENV}=1"
        ));
    }
    let stamp = unique();
    let marker = format!("lm-google-chat-installed-{stamp:x}");
    let expected_reply = format!("{REPLY_PREFIX} {marker}");
    let model = ModelFixture::spawn(marker.clone())?;
    let observer = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("build Google Chat observer client: {error}"))?;
    let mut profile: Option<String> = None;
    let mut account_id: Option<String> = None;

    let result: Result<(), String> = async {
        let created = create_profile()?;
        profile = Some(created.clone());
        let workspace = std::env::temp_dir().join(format!("lm-gchat-e2e-workspace-{stamp:x}"));
        std::fs::create_dir_all(&workspace)
            .map_err(|error| format!("create workspace {}: {error}", workspace.display()))?;
        write_recipe(&created, &workspace, &model.base)?;

        let account = add_account(&created, &config.project_number, &config.bot_user_name)?;
        account_id = Some(account.clone());
        let credential = serde_json::json!({
            "client_email": config.service_account_email,
            "private_key": config.service_account_private_key,
        })
        .to_string();
        require_cli_stdin(
            &created,
            &["channels", "set-token", &account],
            &format!("{credential}\n"),
        )?;
        require_cli(
            Some(&created),
            &[
                "channels", "policy", &account,
                "--direct", "open",
                "--group", "open",
                "--activation", "always",
            ],
        )?;
        require_cli(
            Some(&created),
            &["channels", "add-route", RECIPE, "--account", &account, "--json"],
        )?;
        require_cli(
            Some(&created),
            &["channels", "set-public-url", &config.public_base],
        )?;
        require_cli(Some(&created), &["channels", "enable", &account])?;
        require_cli(Some(&created), &["channels", "probe", &account, "--json"])?;

        let port = config.webhook_port.to_string();
        require_cli(
            Some(&created),
            &["daemon", "install", "--webhook-port", &port],
        )?;
        let first_pid = wait_for_service(&created)?;
        require_cli(Some(&created), &["daemon", "stop"])?;
        require_cli(Some(&created), &["daemon", "start"])?;
        let restarted_pid = wait_for_service(&created)?;
        if first_pid == restarted_pid {
            return Err(format!("daemon restart did not produce a new resident process (pid {first_pid})"));
        }

        let callback_output = require_cli(
            Some(&created),
            &["channels", "callback-url", &account, "--json"],
        )?;
        let callback_payload: serde_json::Value = serde_json::from_slice(&callback_output.stdout)
            .map_err(|error| format!("callback-url JSON was invalid: {error}"))?;
        let callback_url = callback_payload
            .get("url")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("callback-url returned no public URL: {callback_payload}"))?
            .to_string();
        let expected_callback = format!("{}/v1/channels/{account}", config.public_base);
        if callback_url != expected_callback {
            return Err(format!("callback mismatch: CLI advertised {callback_url:?}, expected {expected_callback:?}"));
        }

        eprintln!(
            "\nGoogle Chat app callback for this isolated run:\n\n    {callback_url}\n\nSet the test Chat app's HTTP endpoint to that exact URL and keep Authentication Audience set to Project Number {}. Then press Enter here.\n",
            config.project_number
        );
        let mut ready = String::new();
        io::stdin()
            .read_line(&mut ready)
            .map_err(|error| format!("read callback-configuration confirmation: {error}"))?;

        eprintln!(
            "From the real Google Chat user {} send this exact text in {}:\n\n    {}\n",
            config.external_user_name, config.space_name, marker
        );

        let db = profile_state_db(&created)?;
        let inbound_deadline = Instant::now() + INBOUND_WAIT;
        let inbound = loop {
            if let Some(proof) = find_inbound(&db, &account, &config.external_user_name, &marker)? {
                break proof;
            }
            if Instant::now() >= inbound_deadline {
                return Err(format!(
                    "no Google-signed Chat message from {} containing the marker became a durable ingress/job within {}s",
                    config.external_user_name,
                    INBOUND_WAIT.as_secs()
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        eprintln!(
            "Google Chat inbound {} became ingress {} / job {}",
            inbound.provider_event_id, inbound.ingress_id, inbound.job_id
        );

        let model_deadline = Instant::now() + Duration::from_secs(180);
        while Instant::now() < model_deadline {
            let requests = model.requests();
            if requests.iter().any(|request| request.contains(&marker))
                && requests.iter().any(|request| request.contains(r#"\"name\":\"send_message\""#))
                && requests.len() >= 2
            { break; }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let requests = model.requests();
        if !requests.iter().any(|request| request.contains(&marker)) {
            return Err("installed agent never sent the real Google Chat marker to the model".to_string());
        }
        if !requests.iter().any(|request| request.contains(r#"\"name\":\"send_message\""#)) {
            return Err("send_message was never offered to the installed agent".to_string());
        }
        if requests.len() < 2 {
            return Err("agent never returned to the model after dispatching send_message".to_string());
        }

        let outbound_deadline = Instant::now() + OUTBOUND_WAIT;
        let message_name = loop {
            if let Some(name) = find_outbound(&db, &account, &config.space_name, &expected_reply)? {
                break name;
            }
            if Instant::now() >= outbound_deadline {
                return Err(format!("generated Google Chat reply never became a provider-named durable outbound event: {expected_reply:?}"));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        observe_chat_message(
            &observer,
            &config.external_user_access_token,
            &message_name,
            &expected_reply,
        )
        .await?;
        eprintln!("independent Google Chat user API observed exact generated reply {message_name}");
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
            eprintln!("Google Chat installed-service E2E configuration error: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = run_case(&config).await {
        eprintln!("Google Chat installed-service E2E failed: {error}");
        std::process::exit(1);
    }
}
