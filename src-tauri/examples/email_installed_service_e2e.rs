//! Real mailbox -> installed resident daemon -> agent -> real mailbox acceptance.
//!
//! This is deliberately a black-box harness. It configures Little Monkey only
//! through `monkey-cli` (the mailbox password goes in through
//! `channels set-token` on stdin, never as an argument), installs the real user
//! service, sends one message from a *second* mailbox over a real SMTP relay,
//! and proves that message becomes a durable turn and an agent-produced reply
//! the second mailbox receives — threaded, with `In-Reply-To` naming the
//! marker's own `Message-ID`.
//!
//! The only deterministic component is the OpenAI-compatible model origin. It
//! is the same `target.local_url` seam a real recipe uses; it cannot write the
//! outbox or send mail. The reply exists only if the installed daemon queues a
//! real run and the production agent dispatches `send_message`.
//!
//! This harness needs two real mailboxes that can send to each other. On every
//! pull request the accompanying workflow supplies them from a real Postfix and
//! Dovecot server it starts in a container on the runner, reached over implicit
//! TLS on 993 and 465 with a certificate minted from a certificate authority
//! the job creates; `EMAIL_E2E_CA_FILE` names that authority, and both this
//! harness's own mail client and the account it configures trust it *in
//! addition to* the public web anchors. `workflow_dispatch` is the additional
//! way to run the same acceptance against mailboxes an operator owns, where no
//! extra authority is named and the public anchors are all there is. See
//! `docs/email-installed-service-e2e.md`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use futures_util::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_rustls::TlsConnector;

const PROFILE_ENV: &str = "LITTLE_MONKEY_PROFILE";
const REQUIRE_ENV: &str = "LITTLE_MONKEY_REQUIRE_EMAIL_INSTALLED_SERVICE_E2E";
const RECIPE: &str = "email-installed-service-e2e";
const REPLY_PREFIX: &str = "little-monkey email installed-service reply";
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);
const SERVICE_WAIT: Duration = Duration::from_secs(180);
/// The adapter paces itself to one IMAP session about every thirty seconds and
/// a relay may sit on a message for a while, so the reply window is generous.
const MAIL_WAIT: Duration = Duration::from_secs(600);

/// One mailbox, as the harness needs it: enough to log in and to send.
#[derive(Clone)]
struct Mailbox {
    imap_host: String,
    imap_port: u16,
    smtp_host: String,
    smtp_port: u16,
    username: String,
    password: String,
    address: String,
}

impl Mailbox {
    /// Read one mailbox out of the environment. `prefix` is `EMAIL_E2E` for the
    /// account under test and `EMAIL_E2E_PEER` for the independent sender.
    fn from_env(prefix: &str, address_var: &str) -> Result<Self, String> {
        let var = |suffix: &str| -> Result<String, String> {
            let name = format!("{prefix}_{suffix}");
            std::env::var(&name)
                .map_err(|_| format!("{name} must name the mailbox for this acceptance"))
        };
        let port = |suffix: &str, default: u16| -> Result<u16, String> {
            match std::env::var(format!("{prefix}_{suffix}")) {
                Ok(value) if !value.trim().is_empty() => value
                    .trim()
                    .parse()
                    .map_err(|error| format!("{prefix}_{suffix} is not a port: {error}")),
                _ => Ok(default),
            }
        };
        Ok(Self {
            imap_host: var("IMAP_HOST")?,
            imap_port: port("IMAP_PORT", 993)?,
            smtp_host: var("SMTP_HOST")?,
            smtp_port: port("SMTP_PORT", 465)?,
            username: var("USERNAME")?,
            password: var("PASSWORD")?,
            address: std::env::var(address_var)
                .map_err(|_| format!("{address_var} must name the mailbox address"))?,
        })
    }
}

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

fn run_cli_stdin(
    profile: Option<&str>,
    args: &[&str],
    stdin_bytes: Option<&[u8]>,
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
    command.args(args);
    if let Some(profile) = profile {
        command.env(PROFILE_ENV, profile);
    } else {
        command.env_remove(PROFILE_ENV);
    }
    let mut child = command
        .stdin(if stdin_bytes.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start monkey-cli {args:?}: {error}"))?;
    if let Some(bytes) = stdin_bytes {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "monkey-cli stdin was not piped".to_string())?;
        stdin
            .write_all(bytes)
            .map_err(|error| format!("failed to write the credential to stdin: {error}"))?;
    }
    bounded_output(child, &format!("monkey-cli {args:?}"))
}

fn run_cli(profile: Option<&str>, args: &[&str]) -> Result<Output, String> {
    run_cli_stdin(profile, args, None)
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
    let name = format!("Email installed-service E2E {}", unique());
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

fn add_email_account(profile: &str, mailbox: &Mailbox) -> Result<String, String> {
    let label = format!("email-e2e-{}", unique());
    let mut config = serde_json::json!({
        "imap_host": mailbox.imap_host,
        "imap_port": mailbox.imap_port,
        "smtp_host": mailbox.smtp_host,
        "smtp_port": mailbox.smtp_port,
        "username": mailbox.username,
        "from_address": mailbox.address,
        "mailbox": "INBOX",
    });
    // Omitted entirely when unset, so a dispatch run against an operator's own
    // provider is configured exactly as it is today.
    if let Ok(ca_file) = std::env::var("EMAIL_E2E_CA_FILE") {
        if !ca_file.trim().is_empty() {
            config["tls_ca_file"] = serde_json::json!(ca_file.trim());
        }
    }
    let config = config.to_string();
    let output = require_cli(
        Some(profile),
        &[
            "channels", "add", "email", &label, "--config", &config, "--json",
        ],
    )?;
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        format!(
            "account JSON was invalid: {error}\n{}",
            output_text(&output)
        )
    })?;
    let account_id = payload
        .get("account_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "account JSON had no account_id: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })?;

    // The credential goes in the way an operator's does: as a JSON bundle on
    // stdin, so it never appears in a process listing.
    let bundle = serde_json::json!({ "imap_password": mailbox.password }).to_string();
    let stored = run_cli_stdin(
        Some(profile),
        &["channels", "set-token", &account_id],
        Some(bundle.as_bytes()),
    )?;
    if !stored.status.success() {
        // Deliberately not `output_text`: the failure path of a credential
        // write is the one place an echoed secret would leak into CI logs.
        return Err(format!(
            "channels set-token failed with {} for the email account",
            stored.status
        ));
    }
    Ok(account_id)
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
                                    "id": "call_email_installed_1",
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
        "target": { "local_url": model_base, "model": "email-e2e-fixture" },
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
/// is a stored row, and the process that wrote it may already have been
/// stopped. Requiring `last_probe_at_ms` to be at or after the moment this wait
/// began is what makes `connected` mean the running process said so.
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
            return Err("resident daemon could not reach the mailbox over IMAP".to_string());
        }
        if fresh && last_health == "connected" {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "resident service never reported connected mailbox health of its own within {}s\nlast account health: {last_health} (last probe {last_probe:?}, waiting for one at or after {since_ms})",
        SERVICE_WAIT.as_secs()
    ))
}

// ---------------------------------------------------------------------------
// The independent mail client
// ---------------------------------------------------------------------------

/// The independent mail client's own trust. `EMAIL_E2E_CA_FILE` is honoured
/// exactly as the product's `tls_ca_file` is — added to the public anchors,
/// never instead of them — because this client talks to the same server the
/// account under test does. A silent fallback to the public set here would
/// fail the run six hundred seconds later with the wrong story, so a named
/// file that cannot be used panics with its path.
fn tls_config() -> Arc<rustls::ClientConfig> {
    use rustls::pki_types::pem::PemObject;

    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if let Ok(path) = std::env::var("EMAIL_E2E_CA_FILE") {
        let path = path.trim().to_string();
        if !path.is_empty() {
            let certificates = rustls::pki_types::CertificateDer::pem_file_iter(&path)
                .unwrap_or_else(|error| panic!("EMAIL_E2E_CA_FILE {path} could not be read: {error}"));
            let mut added = 0usize;
            for certificate in certificates {
                let certificate = certificate
                    .unwrap_or_else(|error| panic!("EMAIL_E2E_CA_FILE {path} is not PEM: {error}"));
                roots
                    .add(certificate)
                    .unwrap_or_else(|error| panic!("EMAIL_E2E_CA_FILE {path}: {error}"));
                added += 1;
            }
            assert!(added > 0, "EMAIL_E2E_CA_FILE {path} contains no certificate");
        }
    }
    // Both `ring` and `aws-lc-rs` are compiled in, so rustls refuses to pick
    // one and `ClientConfig::builder()` would panic without this.
    let _ = rustls::crypto::ring::default_provider().install_default();
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

async fn tls_stream(
    host: &str,
    port: u16,
) -> Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>, String> {
    let tcp = tokio::net::TcpStream::connect((host, port))
        .await
        .map_err(|error| format!("TCP connect to {host}:{port}: {error}"))?;
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|error| format!("invalid TLS server name {host}: {error}"))?;
    TlsConnector::from(tls_config())
        .connect(name, tcp)
        .await
        .map_err(|error| format!("TLS handshake with {host}:{port}: {error}"))
}

async fn read_smtp_reply<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<(u16, String), String> {
    let mut collected = String::new();
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(|error| format!("SMTP read: {error}"))?;
        if read == 0 {
            return Err("the SMTP server closed the connection".to_string());
        }
        let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
        if !collected.is_empty() {
            collected.push(' ');
        }
        collected.push_str(&trimmed);
        let bytes = trimmed.as_bytes();
        if bytes.len() < 4 || bytes[3] != b'-' {
            let code = trimmed
                .get(..3)
                .and_then(|code| code.parse().ok())
                .unwrap_or(0);
            return Ok((code, collected));
        }
    }
}

async fn expect_smtp<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    stage: &str,
) -> Result<String, String> {
    let (code, line) = read_smtp_reply(reader).await?;
    if (200..400).contains(&code) {
        Ok(line)
    } else {
        Err(format!("SMTP {stage} answered {code}"))
    }
}

async fn write_smtp<W: AsyncWrite + Unpin>(writer: &mut W, line: &str) -> Result<(), String> {
    writer
        .write_all(line.as_bytes())
        .await
        .map_err(|error| format!("SMTP write: {error}"))?;
    writer
        .write_all(b"\r\n")
        .await
        .map_err(|error| format!("SMTP write: {error}"))?;
    writer
        .flush()
        .await
        .map_err(|error| format!("SMTP flush: {error}"))
}

/// Send one message from the independent mailbox, and return its `Message-ID`.
///
/// A second, self-contained SMTP client on purpose: the acceptance must be able
/// to send *to* Little Monkey from code that is not Little Monkey's own send
/// path, or it would only ever be testing that path against itself.
async fn send_marker_mail(peer: &Mailbox, to: &str, marker: &str) -> Result<String, String> {
    let message_id = format!("{marker}.peer@{}", domain_of(&peer.address));
    let body = format!(
        "From: <{from}>\r\nTo: <{to}>\r\nSubject: {marker}\r\nMessage-ID: <{message_id}>\r\n\
         MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{marker}\r\n",
        from = peer.address,
    );

    let stream = tls_stream(&peer.smtp_host, peer.smtp_port).await?;
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    expect_smtp(&mut reader, "greeting").await?;
    write_smtp(&mut writer, &format!("EHLO {}", domain_of(&peer.address))).await?;
    let greeting = expect_smtp(&mut reader, "EHLO").await?;
    if !greeting.to_ascii_uppercase().contains("AUTH") {
        return Err("the peer SMTP relay offered no AUTH".to_string());
    }
    let credential = BASE64.encode(format!("\0{}\0{}", peer.username, peer.password).as_bytes());
    write_smtp(&mut writer, &format!("AUTH PLAIN {credential}")).await?;
    expect_smtp(&mut reader, "AUTH").await?;
    write_smtp(&mut writer, &format!("MAIL FROM:<{}>", peer.address)).await?;
    expect_smtp(&mut reader, "MAIL FROM").await?;
    write_smtp(&mut writer, &format!("RCPT TO:<{to}>")).await?;
    expect_smtp(&mut reader, "RCPT TO").await?;
    write_smtp(&mut writer, "DATA").await?;
    expect_smtp(&mut reader, "DATA").await?;
    writer
        .write_all(body.trim_end().as_bytes())
        .await
        .map_err(|error| format!("SMTP body: {error}"))?;
    writer
        .write_all(b"\r\n.\r\n")
        .await
        .map_err(|error| format!("SMTP terminator: {error}"))?;
    writer
        .flush()
        .await
        .map_err(|error| format!("SMTP flush: {error}"))?;
    expect_smtp(&mut reader, "message body").await?;
    let _ = write_smtp(&mut writer, "QUIT").await;
    Ok(message_id)
}

fn domain_of(address: &str) -> &str {
    address
        .rsplit_once('@')
        .map(|(_, domain)| domain)
        .unwrap_or("localhost")
}

/// Poll the independent mailbox until the agent's reply lands, then return the
/// `(subject-carrying body, In-Reply-To)` pair the assertion needs.
async fn await_reply(peer: &Mailbox, expected: &str, deadline: Instant) -> Result<String, String> {
    let mut last_error = "no message matched yet".to_string();
    while Instant::now() < deadline {
        match scan_mailbox_once(peer, expected).await {
            Ok(Some(in_reply_to)) => return Ok(in_reply_to),
            Ok(None) => last_error = "the reply has not arrived yet".to_string(),
            Err(error) => last_error = error,
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    Err(format!(
        "the independent mailbox never received {expected:?} within {}s: {last_error}",
        MAIL_WAIT.as_secs()
    ))
}

async fn scan_mailbox_once(peer: &Mailbox, expected: &str) -> Result<Option<String>, String> {
    let stream = tls_stream(&peer.imap_host, peer.imap_port).await?;
    let mut client = async_imap::Client::new(stream);
    client
        .read_response()
        .await
        .map_err(|error| format!("peer IMAP greeting: {error}"))?
        .ok_or_else(|| "peer IMAP closed before greeting".to_string())?;
    let mut session = client
        .login(&peer.username, &peer.password)
        .await
        .map_err(|(error, _)| format!("peer IMAP login: {error}"))?;
    let mailbox = session
        .select("INBOX")
        .await
        .map_err(|error| format!("peer IMAP SELECT INBOX: {error}"))?;
    let next = mailbox.uid_next.unwrap_or(1);
    // The last few messages are enough: the reply is the newest thing in a
    // mailbox this harness is the only writer of.
    let first = next.saturating_sub(20).max(1);
    let mut found = None;
    {
        let mut fetches = session
            .uid_fetch(format!("{first}:*"), "(UID BODY.PEEK[])")
            .await
            .map_err(|error| format!("peer IMAP UID FETCH: {error}"))?;
        while let Some(fetch) = fetches.next().await {
            let fetch = fetch.map_err(|error| format!("peer IMAP fetch stream: {error}"))?;
            let Some(raw) = fetch.body() else { continue };
            let Some(message) = mail_parser::MessageParser::default().parse(raw) else {
                continue;
            };
            let body = message.body_text(0).unwrap_or_default();
            if !body.contains(expected) {
                continue;
            }
            found = Some(
                message
                    .in_reply_to()
                    .as_text_list()
                    .and_then(|ids| ids.last().cloned())
                    .map(|id| id.trim().trim_matches(['<', '>']).to_string())
                    .unwrap_or_default(),
            );
            break;
        }
    }
    let _ = session.logout().await;
    Ok(found)
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
                && event
                    .get("ingress_id")
                    .is_some_and(|value| !value.is_null())
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
    // Two outbound rows, not one: the daemon greets a person's first message
    // with a one-time notice naming the model, and every harness runs a fresh
    // profile, so its sender is always a first contact. The count stays exact
    // on purpose — a third row would be a reply sent twice.
    if outbound != 2 {
        return Err(format!(
            "expected exactly two durable outbound events (the first-contact notice and the reply), got {outbound}: {payload}"
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
            "refusing to install a real OS service and send real mail unless {REQUIRE_ENV}=1"
        ));
    }
    if cfg!(target_os = "macos") {
        return Err("this acceptance currently targets Linux/Windows service semantics; macOS service acceptance remains tracked separately".to_string());
    }

    let account_mailbox = Mailbox::from_env("EMAIL_E2E", "EMAIL_E2E_FROM")?;
    let peer_mailbox = Mailbox::from_env("EMAIL_E2E_PEER", "EMAIL_E2E_PEER_ADDRESS")?;
    if account_mailbox
        .address
        .eq_ignore_ascii_case(&peer_mailbox.address)
    {
        return Err(
            "the independent sender must be a different mailbox, or the account would only be \
             talking to itself"
                .to_string(),
        );
    }

    let stamp = unique();
    let marker = format!("lm-email-installed-{stamp:x}");
    let model = ModelFixture::spawn(marker.clone())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("build the mail runtime: {error}"))?;

    let mut profile: Option<String> = None;
    let mut account_id: Option<String> = None;
    let result = (|| -> Result<(), String> {
        let created = create_profile()?;
        profile = Some(created.clone());

        let workspace = std::env::temp_dir().join(format!("lm-email-e2e-workspace-{stamp:x}"));
        std::fs::create_dir_all(&workspace)
            .map_err(|error| format!("create workspace {}: {error}", workspace.display()))?;
        write_recipe(&created, &workspace, &model.base)?;

        let account = add_email_account(&created, &account_mailbox)?;
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

        // Prove the mailbox session is owned by a real resident lifecycle, not
        // merely by the process that configured it.
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
            "installed email daemon ready (initial pid {first_pid}, after restart {restarted_pid})"
        );

        let sent_id = runtime.block_on(send_marker_mail(
            &peer_mailbox,
            &account_mailbox.address,
            &marker,
        ))?;
        eprintln!("independent mailbox sent {sent_id}");

        let expected = format!("{REPLY_PREFIX} {marker}");
        let in_reply_to = runtime.block_on(await_reply(
            &peer_mailbox,
            &expected,
            Instant::now() + MAIL_WAIT,
        ))?;
        eprintln!("independent mailbox observed the reply, In-Reply-To: {in_reply_to}");
        // Threading is the half of this channel a body match cannot prove: a
        // reply that does not name what it answers lands as a new conversation
        // in every mail client there is.
        if in_reply_to != sent_id {
            return Err(format!(
                "the reply's In-Reply-To was {in_reply_to:?}, not the marker's own {sent_id:?}"
            ));
        }

        let requests = model.requests();
        if !requests.iter().any(|request| request.contains(&marker)) {
            return Err(format!(
                "the installed agent never sent the real mail to the model: {requests:?}"
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
    result
}

fn main() {
    if let Err(error) = run_case() {
        eprintln!("Email installed-service E2E failed: {error}");
        std::process::exit(1);
    }
}
