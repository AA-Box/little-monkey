//! Two operator-owned Slack apps/bots -> real Slack Socket Mode/Web API ->
//! installed resident daemon -> provider-derived approval -> production agent
//! -> Slack -> independent provider-side exact-text proof.
//!
//! Little Monkey receives only the target app's xoxb bot token + xapp Socket
//! Mode token, through `channels set-token`. This harness never constructs
//! `SlackAdapter` and never opens Socket Mode itself. A second Slack bot is the
//! external sender/observer; its credential is never configured in Little
//! Monkey. The external bot first proves the group allow-list rejects an
//! unknown real Slack sender, that provider-derived sender is approved through
//! the normal CLI, the installed daemon is restarted/reconnected, and only
//! then does the external bot send the marker that drives the real agent.

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROFILE_ENV: &str = "LITTLE_MONKEY_PROFILE";
const REQUIRE_ENV: &str = "LITTLE_MONKEY_REQUIRE_SLACK_INSTALLED_SERVICE_E2E";
const BOT_TOKEN_ENV: &str = "SLACK_E2E_BOT_TOKEN";
const APP_TOKEN_ENV: &str = "SLACK_E2E_APP_TOKEN";
const EXTERNAL_BOT_TOKEN_ENV: &str = "SLACK_E2E_EXTERNAL_BOT_TOKEN";
const CHANNEL_ENV: &str = "SLACK_E2E_CHANNEL_ID";
const API_BASE: &str = "https://slack.com/api";
const RECIPE: &str = "slack-installed-service-e2e";
const REPLY_PREFIX: &str = "little-monkey slack installed-service reply";
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);
const SERVICE_WAIT: Duration = Duration::from_secs(150);
const PROVIDER_WAIT: Duration = Duration::from_secs(180);
const AGENT_WAIT: Duration = Duration::from_secs(240);

fn unique() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
}
fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
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
    let output = require_cli(None, &["profiles", "create", &format!("Slack installed-service E2E {}", unique()), "--json"])?;
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|e| format!("profile JSON: {e}"))?;
    v.get("id").and_then(serde_json::Value::as_str).map(str::to_string).ok_or_else(|| format!("profile JSON had no id: {v}"))
}
fn add_account(profile: &str) -> Result<String, String> {
    let label = format!("slack-e2e-{}", unique());
    let output = require_cli(Some(profile), &["channels", "add", "slack", &label, "--json"])?;
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|e| format!("account JSON: {e}"))?;
    v.get("account_id").and_then(serde_json::Value::as_str).map(str::to_string).ok_or_else(|| format!("account JSON had no account_id: {v}"))
}

struct ModelFixture { base: String, seen: Arc<Mutex<Vec<String>>> }
impl ModelFixture {
    fn spawn(marker: String) -> Result<Self, String> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("bind model fixture: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = seen.clone();
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
                    let arguments = serde_json::json!({"text": format!("{REPLY_PREFIX} {marker}")}).to_string();
                    sse_response(&[
                        serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_slack_installed_1","type":"function","function":{"name":"send_message","arguments":arguments}}]}}]}),
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
    let recipe = serde_json::json!({"version":1,"name":RECIPE,"target":{"local_url":model_base,"model":"slack-e2e-fixture"},"workspace":workspace.to_string_lossy(),"permission_mode":"bypass","prompt":"{{message}}","params":{"message":null},"max_iterations":4,"timeout_seconds":180});
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
#[derive(Debug)]
struct InboundProof { provider_event_id: String, conversation_id: String, sender_id: String, ingress_id: Option<String>, job_id: Option<String>, ignore_reason: Option<String> }
fn find_inbound(db: &Path, account: &str, marker: &str, disposition: &str, expected_sender: Option<&str>) -> Result<Option<InboundProof>, String> {
    if !db.is_file() { return Ok(None); }
    let c = read_only(db)?; let like = format!("%{marker}%");
    let mut s = c.prepare("SELECT provider_event_id, conversation_id, sender_id, ingress_id, job_id, ignore_reason FROM channel_events WHERE account_id=?1 AND direction='inbound' AND disposition=?2 AND envelope_json LIKE ?3 ORDER BY received_at_ms DESC").map_err(|e| e.to_string())?;
    let mut rows = s.query((account, disposition, like)).map_err(|e| e.to_string())?;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let sender: Option<String> = row.get(2).map_err(|e| e.to_string())?; let Some(sender_id) = sender else { continue };
        if expected_sender.is_some_and(|expected| sender_id.as_str() != expected) { continue; }
        return Ok(Some(InboundProof { provider_event_id: row.get(0).map_err(|e| e.to_string())?, conversation_id: row.get(1).map_err(|e| e.to_string())?, sender_id, ingress_id: row.get(3).map_err(|e| e.to_string())?, job_id: row.get(4).map_err(|e| e.to_string())?, ignore_reason: row.get(5).map_err(|e| e.to_string())? }));
    }
    Ok(None)
}
fn find_outbound(db: &Path, account: &str, conversation: &str, expected: &str) -> Result<Option<String>, String> {
    if !db.is_file() { return Ok(None); }
    let c = read_only(db)?; let like = format!("%{expected}%");
    c.query_row("SELECT provider_event_id FROM channel_events WHERE account_id=?1 AND direction='outbound' AND conversation_id=?2 AND envelope_json LIKE ?3 AND provider_event_id NOT LIKE 'local:%' ORDER BY received_at_ms DESC LIMIT 1", (account, conversation, like), |row| row.get(0)).optional().map_err(|e| e.to_string())
}
fn wait_for_service_pid(profile: &str) -> Result<u64, String> {
    let deadline = Instant::now() + SERVICE_WAIT; let mut last = String::new();
    while Instant::now() < deadline {
        if let Ok(output) = run_cli(Some(profile), &["daemon", "status", "--json"]) {
            if output.status.success() { if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                last = v.to_string(); let running = v.get("service_running").and_then(serde_json::Value::as_bool).unwrap_or(false); let fresh = v.get("heartbeat_fresh").and_then(serde_json::Value::as_bool).unwrap_or(false); let pid = v.get("pid").and_then(serde_json::Value::as_u64).unwrap_or(0);
                if running && fresh && pid != u64::from(std::process::id()) { return Ok(pid); }
            }}
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!("installed daemon never reported a fresh resident pid; last status {last}"))
}
fn wait_for_connected(profile: &str, account: &str, since_ms: u64) -> Result<(), String> {
    let deadline = Instant::now() + SERVICE_WAIT; let mut last = String::new();
    while Instant::now() < deadline {
        let output = require_cli(Some(profile), &["channels", "list", "--json"])?;
        let v: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
        if let Some(a) = v.get("accounts").and_then(serde_json::Value::as_array).and_then(|xs| xs.iter().find(|x| x.get("account_id").and_then(serde_json::Value::as_str)==Some(account))) {
            let health = a.get("health").and_then(serde_json::Value::as_str).unwrap_or("unknown"); let probe = a.get("last_probe_at_ms").and_then(serde_json::Value::as_u64).unwrap_or(0); last = format!("{health}@{probe}");
            if probe >= since_ms && health == "connected" { return Ok(()); }
            if probe >= since_ms && health == "error" { return Err("resident daemon rejected Slack credentials or Socket Mode".to_string()); }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!("Slack never reached fresh connected health; last {last}"))
}
async fn slack_call(token: &str, method: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
    let client = little_monkey_lib::egress::hardened().build().map_err(|e| e.to_string())?;
    let response = little_monkey_lib::egress::send(client.post(format!("{API_BASE}/{method}")).bearer_auth(token).json(&body)).await.map_err(|e| format!("Slack {method}: {e}"))?;
    let value: serde_json::Value = response.json().await.map_err(|e| format!("Slack {method} JSON: {e}"))?;
    if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) { return Err(format!("Slack {method} refused: {}", value.get("error").and_then(serde_json::Value::as_str).unwrap_or("unknown_error"))); }
    Ok(value)
}
async fn slack_post(token: &str, channel: &str, text: &str) -> Result<String, String> {
    let v = slack_call(token, "chat.postMessage", serde_json::json!({"channel":channel,"text":text})).await?;
    v.get("ts").and_then(serde_json::Value::as_str).map(str::to_string).ok_or_else(|| "Slack chat.postMessage returned no ts".to_string())
}
async fn slack_external_identity(token: &str) -> Result<(String, String), String> {
    let v = slack_call(token, "auth.test", serde_json::json!({})).await?;
    Ok((v.get("user_id").and_then(serde_json::Value::as_str).unwrap_or_default().to_string(), v.get("bot_id").and_then(serde_json::Value::as_str).unwrap_or_default().to_string()))
}
async fn observe_slack(token: &str, channel: &str, ts: &str) -> Result<serde_json::Value, String> {
    let client = little_monkey_lib::egress::hardened().build().map_err(|e| e.to_string())?;
    let response = little_monkey_lib::egress::send(client.get(format!("{API_BASE}/conversations.history")).bearer_auth(token).query(&[("channel",channel),("latest",ts),("oldest",ts),("inclusive","true"),("limit","1")])).await.map_err(|e| e.to_string())?;
    let v: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
    if v.get("ok").and_then(serde_json::Value::as_bool) != Some(true) { return Err(format!("Slack observer refused: {}", v.get("error").and_then(serde_json::Value::as_str).unwrap_or("unknown_error"))); }
    v.get("messages").and_then(serde_json::Value::as_array).and_then(|xs| xs.iter().find(|m| m.get("ts").and_then(serde_json::Value::as_str)==Some(ts))).cloned().ok_or_else(|| format!("Slack history did not contain exact ts {ts}"))
}
fn cleanup(profile: Option<&str>, account: Option<&str>) {
    if let Some(profile) = profile { let _=run_cli(Some(profile), &["daemon","stop"]); let _=run_cli(Some(profile), &["daemon","uninstall"]); if let Some(a)=account { let _=run_cli(Some(profile), &["channels","remove",a]); } let _=run_cli(None, &["profiles","delete",profile,"--yes"]); }
}
struct LiveConfig { bot_token:String, app_token:String, external_token:String, channel:String }
impl LiveConfig {
    fn from_env() -> Result<Self,String> {
        fn required(name:&str)->Result<String,String>{ std::env::var(name).ok().filter(|v|!v.trim().is_empty()).ok_or_else(||format!("{name} is required")) }
        Ok(Self { bot_token:required(BOT_TOKEN_ENV)?, app_token:required(APP_TOKEN_ENV)?, external_token:required(EXTERNAL_BOT_TOKEN_ENV)?, channel:required(CHANNEL_ENV)? })
    }
}
async fn run_case(config:&LiveConfig)->Result<(),String>{
    if std::env::var(REQUIRE_ENV).as_deref()!=Ok("1"){return Err(format!("refusing live Slack/install unless {REQUIRE_ENV}=1"));}
    let stamp=unique(); let denied_marker=format!("lm-slack-denied-{stamp:x}"); let marker=format!("lm-slack-run-{stamp:x}"); let expected=format!("{REPLY_PREFIX} {marker}");
    let model=ModelFixture::spawn(marker.clone())?; let mut profile=None::<String>; let mut account_id=None::<String>;
    let result:Result<(),String>=async{
        let created=create_profile()?; profile=Some(created.clone()); let workspace=std::env::temp_dir().join(format!("lm-slack-e2e-{stamp:x}")); std::fs::create_dir_all(&workspace).map_err(|e|e.to_string())?; write_recipe(&created,&workspace,&model.base)?;
        let account=add_account(&created)?; account_id=Some(account.clone());
        let secret=serde_json::json!({"bot_token":config.bot_token,"app_token":config.app_token}).to_string();
        require_cli_stdin(&created,&["channels","set-token",&account],&format!("{secret}\n"))?;
        require_cli(Some(&created), &["channels","policy",&account,"--direct","allow_list","--group","allow_list","--activation","always"])?;
        require_cli(Some(&created), &["channels","add-route",RECIPE,"--account",&account,"--session-scope","conversation","--json"])?;
        require_cli(Some(&created), &["channels","enable",&account])?;
        let installed_at=now_ms(); require_cli(Some(&created), &["daemon","install"])?; let first_pid=wait_for_service_pid(&created)?; wait_for_connected(&created,&account,installed_at)?;
        let db=profile_state_db(&created)?;
        let (_external_user, external_bot)=slack_external_identity(&config.external_token).await?;
        let denied_ts=slack_post(&config.external_token,&config.channel,&denied_marker).await?;
        let deadline=Instant::now()+PROVIDER_WAIT; let denied=loop{ if let Some(v)=find_inbound(&db,&account,&denied_marker,"ignored",None)?{break v;} if Instant::now()>=deadline{return Err("external Slack bot never reached the durable deny gate".to_string());} tokio::time::sleep(Duration::from_millis(400)).await; };
        if denied.conversation_id!=config.channel{return Err(format!("Slack normalized unexpected channel {}",denied.conversation_id));}
        if denied.ingress_id.is_some()||denied.job_id.is_some(){return Err("unapproved Slack bot unexpectedly reached ingress/job".to_string());}
        if denied.ignore_reason.as_deref()!=Some("sender_not_allowed"){return Err(format!("unapproved Slack bot ignored for wrong reason {:?}",denied.ignore_reason));}
        if !model.requests().is_empty(){return Err("unapproved Slack bot reached model".to_string());}
        if !denied.provider_event_id.contains(&denied_ts){ eprintln!("Slack event id {} is provider-derived but differs from sent ts {} (valid when Slack supplies client_msg_id)",denied.provider_event_id,denied_ts); }
        require_cli(Some(&created), &["channels","approve",&account,&denied.sender_id])?;
        require_cli(Some(&created), &["daemon","stop"])?; let restarted_at=now_ms(); require_cli(Some(&created), &["daemon","start"])?; let second_pid=wait_for_service_pid(&created)?; if second_pid==first_pid{return Err(format!("Slack daemon restart reused pid {first_pid}"));} wait_for_connected(&created,&account,restarted_at)?;
        let sent_ts=slack_post(&config.external_token,&config.channel,&marker).await?;
        let deadline=Instant::now()+PROVIDER_WAIT; let accepted=loop{ if let Some(v)=find_inbound(&db,&account,&marker,"accepted",Some(&denied.sender_id))?{if v.ingress_id.is_some()&&v.job_id.is_some(){break v;}} if Instant::now()>=deadline{return Err("approved Slack sender never produced durable ingress/job".to_string());} tokio::time::sleep(Duration::from_millis(400)).await; };
        if accepted.conversation_id!=config.channel{return Err("accepted Slack event changed channels".to_string());}
        eprintln!("Slack provider event {} (sent {sent_ts}) became ingress {:?}/job {:?} after installed-service restart {} -> {}",accepted.provider_event_id,accepted.ingress_id,accepted.job_id,first_pid,second_pid);
        let deadline=Instant::now()+AGENT_WAIT; loop{let requests=model.requests(); if requests.iter().any(|r|r.contains(&marker))&&requests.iter().any(|r|r.contains("send_message"))&&requests.len()>=2{break;} if Instant::now()>=deadline{return Err(format!("Slack agent/tool loop incomplete ({} model calls)",requests.len()));} tokio::time::sleep(Duration::from_millis(250)).await;}
        let deadline=Instant::now()+PROVIDER_WAIT; let provider_ts=loop{if let Some(v)=find_outbound(&db,&account,&config.channel,&expected)?{break v;} if Instant::now()>=deadline{return Err("Slack generated reply never became provider-named durable outbound".to_string());} tokio::time::sleep(Duration::from_millis(400)).await;};
        if provider_ts.starts_with("local:")||provider_ts.trim().is_empty(){return Err(format!("Slack reply lacks real provider ts: {provider_ts:?}"));}
        let observed=observe_slack(&config.external_token,&config.channel,&provider_ts).await?; let text=observed.get("text").and_then(serde_json::Value::as_str).unwrap_or_default(); if text!=expected{return Err(format!("Slack provider observation mismatch: expected {expected:?}, got {text:?}"));}
        let observed_bot=observed.get("bot_id").and_then(serde_json::Value::as_str).unwrap_or_default(); if !external_bot.is_empty()&&observed_bot==external_bot{return Err("observer found its own bot message instead of Little Monkey reply".to_string());}
        eprintln!("Independent Slack bot read exact generated reply at ts {provider_ts}."); Ok(())
    }.await;
    if result.is_err(){if let Some(p)=profile.as_deref(){if let Ok(o)=run_cli(Some(p), &["daemon","status","--json"]){eprintln!("--- daemon ---\n{}",output_text(&o));} if let Some(a)=account_id.as_deref(){if let Ok(o)=run_cli(Some(p), &["channels","events",a,"--limit","80","--json"]){eprintln!("--- events ---\n{}",output_text(&o));}}}}
    cleanup(profile.as_deref(),account_id.as_deref()); result
}
#[tokio::main(flavor="multi_thread")]
async fn main(){let config=match LiveConfig::from_env(){Ok(v)=>v,Err(e)=>{eprintln!("Slack installed-service E2E configuration error: {e}");std::process::exit(2)}};if let Err(e)=run_case(&config).await{eprintln!("Slack installed-service E2E failed: {e}");std::process::exit(1)}}
