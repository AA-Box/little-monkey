//! Real handset -> real carrier-signed SMS webhook -> installed resident daemon
//! -> production SMS/telephony boundary -> real agent -> carrier -> handset.
//!
//! The telephony account owns the carrier credential. This harness never
//! constructs SmsAdapter or a carrier provider and never fabricates a webhook.
//! A real handset is both sender and observer. The final observation is proved
//! back through the carrier: after delivery is recorded, the handset sends the
//! exact received reply back unchanged while execution is disabled.

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROFILE_ENV: &str = "LITTLE_MONKEY_PROFILE";
const REQUIRE_ENV: &str = "LITTLE_MONKEY_REQUIRE_SMS_INSTALLED_SERVICE_E2E";
const RECIPE: &str = "sms-installed-service-e2e";
const REPLY_PREFIX: &str = "little-monkey sms installed-service reply";
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);
const SERVICE_WAIT: Duration = Duration::from_secs(150);
const PROVIDER_WAIT: Duration = Duration::from_secs(600);
const AGENT_WAIT: Duration = Duration::from_secs(240);
const DELIVERY_WAIT: Duration = Duration::from_secs(600);

fn unique() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
}
fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"))
}
fn cli() -> PathBuf {
    target_dir().join("debug").join(if cfg!(windows) { "monkey-cli.exe" } else { "monkey-cli" })
}
fn output_text(output: &Output) -> String {
    format!("stdout:\n{}\nstderr:\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
}
fn bounded_output(mut child: std::process::Child, label: &str) -> Result<Output, String> {
    let deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill(); let _ = child.wait();
                return Err(format!("{label} timed out after {}s", CHILD_TIMEOUT.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("wait for {label}: {e}")),
        }
    }
    child.wait_with_output().map_err(|e| format!("collect {label}: {e}"))
}
fn run_cli_with_stdin(profile: Option<&str>, args: &[&str], stdin: Option<&str>) -> Result<Output, String> {
    let binary = cli();
    if !binary.is_file() { return Err(format!("missing prebuilt monkey-cli at {}", binary.display())); }
    if std::fs::metadata(&binary).map_err(|e| e.to_string())?.len() == 0 {
        return Err(format!("{} is the zero-byte build placeholder", binary.display()));
    }
    let mut command = Command::new(binary);
    command.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin.is_some() { command.stdin(Stdio::piped()); }
    if let Some(profile) = profile { command.env(PROFILE_ENV, profile); } else { command.env_remove(PROFILE_ENV); }
    let mut child = command.spawn().map_err(|e| format!("start monkey-cli {args:?}: {e}"))?;
    if let Some(value) = stdin {
        child.stdin.take().ok_or_else(|| "monkey-cli child had no stdin".to_string())?
            .write_all(value.as_bytes()).map_err(|e| format!("write monkey-cli stdin: {e}"))?;
    }
    bounded_output(child, &format!("monkey-cli {args:?}"))
}
fn run_cli(profile: Option<&str>, args: &[&str]) -> Result<Output, String> { run_cli_with_stdin(profile, args, None) }
fn require_cli(profile: Option<&str>, args: &[&str]) -> Result<Output, String> {
    let output = run_cli(profile, args)?;
    if output.status.success() { Ok(output) } else { Err(format!("monkey-cli {args:?} failed with {}\n{}", output.status, output_text(&output))) }
}
fn require_cli_stdin(profile: &str, args: &[&str], stdin: &str) -> Result<Output, String> {
    let output = run_cli_with_stdin(Some(profile), args, Some(stdin))?;
    if output.status.success() { Ok(output) } else { Err(format!("monkey-cli {args:?} failed with {}\n{}", output.status, output_text(&output))) }
}
fn create_profile() -> Result<String, String> {
    let output = require_cli(None, &["profiles", "create", &format!("SMS installed-service E2E {}", unique()), "--json"])?;
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|e| format!("profile JSON: {e}"))?;
    v.get("id").and_then(serde_json::Value::as_str).map(str::to_string).ok_or_else(|| format!("profile JSON had no id: {v}"))
}

struct ModelFixture { base: String, seen: Arc<Mutex<Vec<String>>> }
impl ModelFixture {
    fn spawn(expected_reply: String) -> Result<Self, String> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("bind model fixture: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let seen: Arc<Mutex<Vec<String>>> = Arc::default(); let log = seen.clone();
        std::thread::spawn(move || {
            for incoming in listener.incoming() {
                let Ok(mut stream) = incoming else { break };
                let Some((head, body)) = read_http_request(&mut stream) else { continue };
                log.lock().unwrap_or_else(|e| e.into_inner()).push(format!("{head}\n{body}"));
                let response = if !head.contains("/chat/completions") {
                    json_response(r#"{"error":"unexpected model route"}"#)
                } else if body.contains("\"role\":\"tool\"") {
                    sse_response(&[
                        serde_json::json!({"choices":[{"index":0,"delta":{"content":"sent"}}]}),
                        serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})])
                } else {
                    let arguments = serde_json::json!({"text": expected_reply}).to_string();
                    sse_response(&[
                        serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_sms_installed_1","type":"function","function":{"name":"send_message","arguments":arguments}}]}}]}),
                        serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})])
                };
                let _ = stream.write_all(response.as_bytes()); let _ = stream.flush();
            }
        });
        Ok(Self { base: format!("http://127.0.0.1:{port}"), seen })
    }
    fn requests(&self) -> Vec<String> { self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone() }
}
fn read_http_request(stream: &mut TcpStream) -> Option<(String, String)> {
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok()?;
    let mut received = Vec::new(); let mut scratch = [0u8; 8192]; let mut header_end = None; let mut content_length = 0usize;
    loop {
        match stream.read(&mut scratch) {
            Ok(0) => break,
            Ok(n) => {
                received.extend_from_slice(&scratch[..n]);
                if header_end.is_none() {
                    if let Some(i) = find(&received, b"\r\n\r\n") {
                        header_end = Some(i + 4);
                        let headers = String::from_utf8_lossy(&received[..i]);
                        content_length = headers.lines().find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().ok()).flatten()
                        }).unwrap_or(0);
                    }
                }
                if let Some(start) = header_end { if received.len() >= start + content_length { break; } }
            }
            Err(_) => break,
        }
    }
    let split = header_end?;
    Some((String::from_utf8_lossy(&received[..split]).to_string(), String::from_utf8_lossy(&received[split..]).to_string()))
}
fn find(h: &[u8], n: &[u8]) -> Option<usize> { h.windows(n.len()).position(|w| w == n) }
fn json_response(body: &str) -> String { format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()) }
fn sse_response(frames: &[serde_json::Value]) -> String {
    let mut body = String::new(); for frame in frames { body.push_str(&format!("data: {frame}\n\n")); } body.push_str("data: [DONE]\n\n");
    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}
fn write_recipe(profile: &str, workspace: &Path, model_base: &str) -> Result<(), String> {
    let old = std::env::var_os(PROFILE_ENV); std::env::set_var(PROFILE_ENV, profile);
    let roots = little_monkey_lib::app_paths::ensure_agent_config_roots().map_err(|e| format!("resolve config roots: {e}"));
    match old { Some(v) => std::env::set_var(PROFILE_ENV, v), None => std::env::remove_var(PROFILE_ENV) }
    let recipes = roots?.authored.join("recipes"); std::fs::create_dir_all(&recipes).map_err(|e| e.to_string())?;
    let recipe = serde_json::json!({"version":1,"name":RECIPE,"target":{"local_url":model_base,"model":"sms-e2e-fixture"},"workspace":workspace.to_string_lossy(),"permission_mode":"auto","prompt":"{{message}}","params":{"message":null},"max_iterations":4,"timeout_seconds":180});
    std::fs::write(recipes.join(format!("{RECIPE}.json")), serde_json::to_vec_pretty(&recipe).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}
fn profile_state_db(profile: &str) -> Result<PathBuf, String> {
    let old = std::env::var_os(PROFILE_ENV); std::env::set_var(PROFILE_ENV, profile);
    let data = little_monkey_lib::app_paths::data_dir().ok_or_else(|| "could not resolve profile data directory".to_string());
    match old { Some(v) => std::env::set_var(PROFILE_ENV, v), None => std::env::remove_var(PROFILE_ENV) }
    Ok(data?.join("daemon").join("daemon-v1.sqlite3"))
}
fn read_only(path: &Path) -> Result<Connection, String> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e| format!("open {}: {e}", path.display()))
}
// A provider-derived sender is a phone number or account handle. The success
// path only has to confirm which one paired, so it prints the tail; the
// failure paths below keep the whole value, because there it is the diagnostic.
fn masked_sender(identity: &str) -> String {
    let chars: Vec<char> = identity.chars().collect();
    if chars.len() <= 4 {
        return "***".to_string();
    }
    format!("***{}", chars[chars.len() - 4..].iter().collect::<String>())
}

#[derive(Debug)]
struct InboundProof { provider_event_id: String, conversation_id: String, sender_id: String, disposition: String, ingress_id: Option<String>, job_id: Option<String> }
fn find_inbound(db: &Path, account: &str, marker: &str, expected_sender: Option<&str>) -> Result<Option<InboundProof>, String> {
    if !db.is_file() { return Ok(None); }
    let c = read_only(db)?; let like = format!("%{marker}%");
    let mut s = c.prepare("SELECT provider_event_id, conversation_id, sender_id, disposition, ingress_id, job_id FROM channel_events WHERE account_id=?1 AND direction='inbound' AND envelope_json LIKE ?2 ORDER BY received_at_ms DESC").map_err(|e| e.to_string())?;
    let mut rows = s.query((account, like)).map_err(|e| e.to_string())?;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let sender: Option<String> = row.get(2).map_err(|e| e.to_string())?; let Some(sender_id) = sender else { continue };
        if expected_sender.is_some_and(|expected| sender_id.as_str() != expected) { continue; }
        return Ok(Some(InboundProof { provider_event_id: row.get(0).map_err(|e| e.to_string())?, conversation_id: row.get(1).map_err(|e| e.to_string())?, sender_id, disposition: row.get(3).map_err(|e| e.to_string())?, ingress_id: row.get(4).map_err(|e| e.to_string())?, job_id: row.get(5).map_err(|e| e.to_string())? }));
    }
    Ok(None)
}
fn find_outbound(db: &Path, account: &str, conversation: &str, expected: &str) -> Result<Option<String>, String> {
    if !db.is_file() { return Ok(None); }
    let c = read_only(db)?; let like = format!("%{expected}%");
    c.query_row("SELECT provider_event_id FROM channel_events WHERE account_id=?1 AND direction='outbound' AND conversation_id=?2 AND envelope_json LIKE ?3 ORDER BY received_at_ms DESC LIMIT 1", (account, conversation, like), |row| row.get(0)).optional().map_err(|e| e.to_string())
}
fn wait_for_service_pid(profile: &str) -> Result<u64, String> {
    let deadline = Instant::now() + SERVICE_WAIT; let mut last = String::new();
    while Instant::now() < deadline {
        if let Ok(output) = run_cli(Some(profile), &["daemon", "status", "--json"]) {
            if output.status.success() { if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                last = v.to_string();
                let running = v.get("service_running").and_then(serde_json::Value::as_bool).unwrap_or(false);
                let heartbeat = v.get("heartbeat_fresh").and_then(serde_json::Value::as_bool).unwrap_or(false);
                let pid = v.get("pid").and_then(serde_json::Value::as_u64).unwrap_or_default();
                if running && heartbeat && pid != u64::from(std::process::id()) { return Ok(pid); }
            }}
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!("installed daemon never reached a fresh heartbeat; last status: {last}"))
}
fn wait_for_inbound(db: &Path, account: &str, marker: &str, expected_sender: Option<&str>) -> Result<InboundProof, String> {
    let deadline = Instant::now() + PROVIDER_WAIT;
    while Instant::now() < deadline {
        if let Some(proof) = find_inbound(db, account, marker, expected_sender)? { return Ok(proof); }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!("no real SMS inbound containing {marker:?} arrived within {}s", PROVIDER_WAIT.as_secs()))
}
fn wait_for_outbound(db: &Path, account: &str, conversation: &str, expected: &str) -> Result<String, String> {
    let deadline = Instant::now() + AGENT_WAIT;
    while Instant::now() < deadline {
        if let Some(id) = find_outbound(db, account, conversation, expected)? { return Ok(id); }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err("generated SMS reply never became a durable outbound provider event".to_string())
}
fn wait_for_delivery(profile: &str, account: &str, peer: &str, expected: &str) -> Result<(), String> {
    let deadline = Instant::now() + DELIVERY_WAIT; let mut last = String::new();
    while Instant::now() < deadline {
        if let Ok(output) = run_cli(Some(profile), &["telecom", "messages", account, "--limit", "50", "--json"]) {
            if output.status.success() {
                last = String::from_utf8_lossy(&output.stdout).to_string();
                if let Ok(rows) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                    if let Some(items) = rows.as_array() {
                        for row in items {
                            let matches = row.get("direction").and_then(serde_json::Value::as_str) == Some("outbound")
                                && row.get("peer_number").and_then(serde_json::Value::as_str) == Some(peer)
                                && row.get("text").and_then(serde_json::Value::as_str) == Some(expected);
                            if matches {
                                if row.get("delivery_state").and_then(serde_json::Value::as_str) == Some("delivered") { return Ok(()); }
                                if row.get("delivery_state").and_then(serde_json::Value::as_str) == Some("undelivered") {
                                    return Err(format!("carrier reported the generated SMS undelivered: {}", row.get("error").unwrap_or(&serde_json::Value::Null)));
                                }
                            }
                        }
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Err(format!("no delivered receipt for exact generated SMS within {}s; last telecom messages: {last}", DELIVERY_WAIT.as_secs()))
}
fn dump_diagnostics(profile: &str, account: Option<&str>) {
    for args in [vec!["daemon","status","--json"], vec!["channels","list","--json"], vec!["telecom","list","--json"]] {
        if let Ok(out) = run_cli(Some(profile), &args) { eprintln!("--- {args:?} ---\n{}", output_text(&out)); }
    }
    if let Some(account) = account {
        if let Ok(out) = run_cli(Some(profile), &["channels","events",account,"--limit","80","--json"]) { eprintln!("--- channel events ---\n{}", output_text(&out)); }
        if let Ok(out) = run_cli(Some(profile), &["telecom","messages",account,"--limit","80","--json"]) { eprintln!("--- telecom messages ---\n{}", output_text(&out)); }
    }
}
fn cleanup(profile: Option<&str>, account: Option<&str>) {
    if let Some(profile) = profile {
        let _ = run_cli(Some(profile), &["daemon","stop"]); let _ = run_cli(Some(profile), &["daemon","uninstall"]);
        if let Some(account) = account { let _ = run_cli(Some(profile), &["telecom","remove",account]); }
        let _ = run_cli(None, &["profiles","delete",profile,"--yes"]);
    }
}

struct LiveConfig {
    carrier: String,
    carrier_account_id: String,
    from_number: String,
    credential: String,
    public_base: String,
    webhook_public_key: Option<String>,
    webhook_port: u16,
}
impl LiveConfig {
    fn from_env() -> Result<Self, String> {
        fn req(name: &str) -> Result<String, String> { std::env::var(name).ok().filter(|v| !v.trim().is_empty()).ok_or_else(|| format!("{name} is required")) }
        let carrier = req("SMS_E2E_CARRIER")?.to_ascii_lowercase();
        if !matches!(carrier.as_str(), "twilio"|"telnyx"|"plivo") { return Err("SMS_E2E_CARRIER must be twilio, telnyx or plivo".into()); }
        let public_base = req("SMS_E2E_PUBLIC_BASE")?.trim_end_matches('/').to_string();
        if !public_base.starts_with("https://") { return Err("SMS_E2E_PUBLIC_BASE must be a public https:// origin".into()); }
        let webhook_public_key = std::env::var("SMS_E2E_WEBHOOK_PUBLIC_KEY").ok().filter(|v| !v.trim().is_empty());
        if carrier == "telnyx" && webhook_public_key.is_none() { return Err("SMS_E2E_WEBHOOK_PUBLIC_KEY is required for Telnyx".into()); }
        let webhook_port = std::env::var("SMS_E2E_WEBHOOK_PORT").unwrap_or_else(|_| "38447".into()).parse::<u16>().map_err(|_| "SMS_E2E_WEBHOOK_PORT must be a non-zero u16".to_string())?;
        if webhook_port == 0 { return Err("SMS_E2E_WEBHOOK_PORT must be non-zero".into()); }
        Ok(Self { carrier, carrier_account_id: req("SMS_E2E_CARRIER_ACCOUNT_ID")?, from_number: req("SMS_E2E_FROM_NUMBER")?, credential: req("SMS_E2E_CREDENTIAL")?, public_base, webhook_public_key, webhook_port })
    }
}

async fn run_case(config: &LiveConfig) -> Result<(), String> {
    if std::env::var(REQUIRE_ENV).as_deref() != Ok("1") { return Err(format!("refusing a billable real-carrier run unless {REQUIRE_ENV}=1")); }
    let stamp = unique();
    let pair_marker = format!("lm-sms-pair-{stamp:x}");
    let run_marker = format!("lm-sms-run-{stamp:x}");
    let hidden_nonce = format!("{:x}", unique());
    let expected_reply = format!("{REPLY_PREFIX} {hidden_nonce}");
    let model = ModelFixture::spawn(expected_reply.clone())?;
    let mut profile: Option<String> = None; let mut account: Option<String> = None;

    let result: Result<(), String> = async {
        let created = create_profile()?; profile = Some(created.clone());
        let workspace = std::env::temp_dir().join(format!("lm-sms-e2e-{stamp:x}")); std::fs::create_dir_all(&workspace).map_err(|e| e.to_string())?;
        write_recipe(&created, &workspace, &model.base)?;
        let label = format!("sms-e2e-{stamp:x}");
        let config_json = config.webhook_public_key.as_ref().map(|key| serde_json::json!({"webhook_public_key": key}).to_string()).unwrap_or_else(|| "{}".into());
        let out = require_cli(Some(&created), &["telecom","add",&config.carrier,&label,&config.carrier_account_id,&config.from_number,"--public-url",&config.public_base,"--config",&config_json,"--json"])?;
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("telecom add JSON: {e}"))?;
        let account_id = v.get("account_id").and_then(serde_json::Value::as_str).ok_or_else(|| format!("telecom add returned no account_id: {v}"))?.to_string(); account = Some(account_id.clone());
        require_cli_stdin(&created, &["telecom","set-token",&account_id], &format!("{}\n", config.credential))?;
        let probe = require_cli(Some(&created), &["telecom","probe",&account_id,"--json"])?;
        let pv: serde_json::Value = serde_json::from_slice(&probe.stdout).map_err(|e| format!("probe JSON: {e}"))?;
        if pv.get("state").and_then(serde_json::Value::as_str) != Some("connected") { return Err(format!("carrier probe did not prove connected: {pv}")); }
        require_cli(Some(&created), &["channels","policy",&account_id,"--direct","pairing","--group","disabled","--activation","disabled"])?;
        require_cli(Some(&created), &["channels","add-route",RECIPE,"--account",&account_id,"--json"])?;
        // One operator-visible switch owns the number. The internal SMS channel
        // must become enabled as a consequence of this command; the harness
        // deliberately does not call `channels enable` as a workaround.
        require_cli(Some(&created), &["telecom","enable",&account_id])?;
        let channels = require_cli(Some(&created), &["channels","list","--json"])?;
        let channel_rows: serde_json::Value = serde_json::from_slice(&channels.stdout).map_err(|e| format!("channels list JSON: {e}"))?;
        let shadow_enabled = channel_rows.as_array().and_then(|rows| rows.iter().find(|row| row.get("account_id").and_then(serde_json::Value::as_str) == Some(&account_id))).and_then(|row| row.get("enabled")).and_then(serde_json::Value::as_bool).unwrap_or(false);
        if !shadow_enabled { return Err("telecom enable did not enable the SMS shadow channel".into()); }
        let cb = require_cli(Some(&created), &["telecom","callback-url",&account_id,"--json"])?;
        let cv: serde_json::Value = serde_json::from_slice(&cb.stdout).map_err(|e| format!("callback JSON: {e}"))?;
        let callback = cv.get("callback_url").and_then(serde_json::Value::as_str).ok_or_else(|| format!("no callback_url: {cv}"))?;
        let expected_cb = format!("{}/v1/telecom/{account_id}", config.public_base);
        if callback != expected_cb { return Err(format!("callback mismatch: got {callback:?}, expected {expected_cb:?}")); }
        eprintln!("\nConfigure the operator-owned {} number in SMS_E2E_FROM_NUMBER so inbound SMS/MMS posts to:\n\n    {}\n\nThe provider must send its real signature. Ensure {} routes to localhost port {}.\n", config.carrier, callback, config.public_base, config.webhook_port);
        eprintln!("When that carrier configuration is active, send this exact SMS from an independent real handset:\n\n    {pair_marker}\n");

        let port = config.webhook_port.to_string(); require_cli(Some(&created), &["daemon","install","--webhook-port",&port])?;
        let first_pid = wait_for_service_pid(&created)?;
        let db = profile_state_db(&created)?;
        let first = wait_for_inbound(&db, &account_id, &pair_marker, None)?;
        if first.ingress_id.is_some() || first.job_id.is_some() { return Err(format!("unapproved SMS sender unexpectedly executed: {first:?}")); }
        if first.conversation_id != first.sender_id { return Err(format!("SMS peer identity drifted: {first:?}")); }
        require_cli(Some(&created), &["channels","approve",&account_id,&first.sender_id])?;

        require_cli(Some(&created), &["daemon","stop"])?; require_cli(Some(&created), &["daemon","start"])?;
        let second_pid = wait_for_service_pid(&created)?; if first_pid == second_pid { return Err(format!("daemon restart reused pid {first_pid}")); }
        eprintln!("Approved provider-derived SMS sender {} and restarted installed daemon {} -> {}.\nNow send this exact SMS from the SAME handset:\n\n    {run_marker}\n", masked_sender(&first.sender_id), first_pid, second_pid);
        let second = wait_for_inbound(&db, &account_id, &run_marker, Some(&first.sender_id))?;
        if second.disposition != "accepted" || second.ingress_id.is_none() || second.job_id.is_none() { return Err(format!("approved SMS did not become durable ingress/job: {second:?}")); }

        let deadline = Instant::now() + AGENT_WAIT;
        while Instant::now() < deadline {
            let reqs = model.requests();
            if reqs.iter().any(|r| r.contains(&run_marker)) && reqs.iter().any(|r| r.contains(r#""name":"send_message""#)) && reqs.len() >= 2 { break; }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let reqs = model.requests();
        if !reqs.iter().any(|r| r.contains(&run_marker)) { return Err("installed agent never sent the real SMS marker to the model".into()); }
        if !reqs.iter().any(|r| r.contains(r#""name":"send_message""#)) { return Err("send_message was never offered to the SMS agent".into()); }
        if reqs.len() < 2 { return Err("agent never returned to the model after send_message".into()); }
        let provider_id = wait_for_outbound(&db, &account_id, &second.conversation_id, &expected_reply)?;
        if provider_id.trim().is_empty() || provider_id.starts_with("local:") { return Err(format!("carrier did not name generated SMS: {provider_id:?}")); }
        wait_for_delivery(&created, &account_id, &second.sender_id, &expected_reply)?;

        require_cli(Some(&created), &["channels","policy",&account_id,"--direct","disabled"])?;
        eprintln!("\nCarrier delivery receipt matched provider message {provider_id}. On the independent handset, copy the Little Monkey SMS you just received and send that exact text back unchanged. Do not type a substitute; the reply contains a nonce that was never printed by this harness.\n");
        let confirmation = wait_for_inbound(&db, &account_id, &expected_reply, Some(&first.sender_id))?;
        if confirmation.job_id.is_some() || confirmation.ingress_id.is_some() { return Err(format!("confirmation SMS executed despite disabled direct policy: {confirmation:?}")); }
        eprintln!("Independent handset proved exact generated SMS through real carrier inbound {}", confirmation.provider_event_id);
        Ok(())
    }.await;

    if result.is_err() { if let Some(p) = profile.as_deref() { dump_diagnostics(p, account.as_deref()); } }
    cleanup(profile.as_deref(), account.as_deref()); result
}

#[tokio::main(flavor="multi_thread")]
async fn main() {
    let config = match LiveConfig::from_env() { Ok(v) => v, Err(e) => { eprintln!("SMS installed-service E2E configuration error: {e}"); std::process::exit(2); } };
    if let Err(e) = run_case(&config).await { eprintln!("SMS installed-service E2E failed: {e}"); std::process::exit(1); }
}
