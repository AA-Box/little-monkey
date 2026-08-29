//! Independent iMessage identity -> Messages/chat.db -> production helper ->
//! installed resident daemon -> pairing/approval -> real agent -> helper
//! Automation send -> independent Messages-client observation.
//!
//! This is deliberately black-box with respect to the iMessage adapter and
//! helper protocol. The harness only configures Little Monkey through
//! `monkey-cli`, installs/restarts the real OS service, and observes durable
//! state. It never opens `~/Library/Messages/chat.db`, never constructs
//! `ImessageAdapter`, and never invokes `osascript` itself.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("iMessage installed-service E2E only runs on macOS.");
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
mod macos {
    use rusqlite::{Connection, OpenFlags, OptionalExtension};
    use std::io::{self, Read, Write};
    use std::net::TcpStream;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const PROFILE_ENV: &str = "LITTLE_MONKEY_PROFILE";
    const REQUIRE_ENV: &str = "LITTLE_MONKEY_REQUIRE_IMESSAGE_INSTALLED_SERVICE_E2E";
    const HELPER_ENV: &str = "IMESSAGE_E2E_HELPER_PATH";
    const HANDLE_ENV: &str = "IMESSAGE_E2E_HANDLE";
    const DESTINATION_ENV: &str = "IMESSAGE_E2E_DESTINATION";
    const RECIPE: &str = "imessage-installed-service-e2e";
    const REPLY_PREFIX: &str = "little-monkey imessage installed-service reply";
    const CHILD_TIMEOUT: Duration = Duration::from_secs(120);
    const SERVICE_WAIT: Duration = Duration::from_secs(180);
    const HUMAN_WAIT: Duration = Duration::from_secs(600);
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
        target_dir().join("debug").join("monkey-cli")
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
            return Err(format!("{} is the zero-byte Tauri sidecar placeholder", binary.display()));
        }
        let mut command = Command::new(binary);
        command
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(profile) = profile {
            command.env(PROFILE_ENV, profile);
        } else {
            command.env_remove(PROFILE_ENV);
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
        let name = format!("iMessage installed-service E2E {}", unique());
        let output = require_cli(None, &["profiles", "create", &name, "--json"])?;
        let payload: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("profile JSON was invalid: {error}"))?;
        payload
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("profile JSON had no id: {payload}"))
    }

    fn add_account(profile: &str, helper_path: &str, handle: &str) -> Result<String, String> {
        let label = format!("imessage-e2e-{}", unique());
        let config = serde_json::json!({
            "helper_path": helper_path,
            "handle": handle,
        })
        .to_string();
        let output = require_cli(
            Some(profile),
            &[
                "channels", "add", "imessage", &label, "--config", &config, "--json",
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
                                        "id": "call_imessage_installed_1",
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
            "target": { "local_url": model_base, "model": "imessage-e2e-fixture" },
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
    struct InboundProof {
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
    ) -> Result<Option<InboundProof>, String> {
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
            .map_err(|error| format!("prepare iMessage inbound lookup: {error}"))?;
        let mut rows = statement
            .query((account_id, disposition, like))
            .map_err(|error| format!("query iMessage inbound lookup: {error}"))?;
        while let Some(row) = rows
            .next()
            .map_err(|error| format!("read iMessage inbound lookup: {error}"))?
        {
            let sender_id: Option<String> = row
                .get(2)
                .map_err(|error| format!("read iMessage sender id: {error}"))?;
            let Some(sender_id) = sender_id else { continue };
            if expected_sender.is_some_and(|expected| expected != sender_id) {
                continue;
            }
            return Ok(Some(InboundProof {
                provider_event_id: row
                    .get(0)
                    .map_err(|error| format!("read Messages GUID: {error}"))?,
                conversation_id: row
                    .get(1)
                    .map_err(|error| format!("read iMessage conversation id: {error}"))?,
                sender_id,
                ingress_id: row
                    .get(3)
                    .map_err(|error| format!("read ingress id: {error}"))?,
                job_id: row.get(4).map_err(|error| format!("read job id: {error}"))?,
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
                   AND disposition = 'accepted'
                   AND envelope_json LIKE ?3
                 ORDER BY received_at_ms DESC LIMIT 1",
                (account_id, conversation_id, like),
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| format!("read exact durable iMessage outbound: {error}"))
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
        Err(format!("installed daemon never reported a fresh resident pid; last status: {last}"))
    }

    fn wait_for_account_connected(
        profile: &str,
        account_id: &str,
        since_ms: u64,
        deadline: Instant,
    ) -> Result<(), String> {
        let mut last = String::new();
        while Instant::now() < deadline {
            let output = require_cli(Some(profile), &["channels", "list", "--json"])?;
            let payload: serde_json::Value = serde_json::from_slice(&output.stdout)
                .map_err(|error| format!("channels list JSON was invalid: {error}"))?;
            if let Some(account) = payload
                .get("accounts")
                .and_then(serde_json::Value::as_array)
                .and_then(|accounts| {
                    accounts.iter().find(|row| {
                        row.get("account_id").and_then(serde_json::Value::as_str) == Some(account_id)
                    })
                })
            {
                last = account.to_string();
                let state = account
                    .get("health")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown");
                let probed = account
                    .get("last_probe_at_ms")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default();
                if probed >= since_ms && state == "connected" {
                    return Ok(());
                }
                if probed >= since_ms && matches!(state, "unsupported" | "error") {
                    return Err(format!(
                        "real helper probe did not report a usable Messages account: {last}"
                    ));
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Err(format!(
            "resident daemon never reported fresh connected iMessage health within {}s; last account: {last}",
            SERVICE_WAIT.as_secs()
        ))
    }

    fn wait_for_inbound(
        db: &Path,
        account_id: &str,
        marker: &str,
        disposition: &str,
        sender: Option<&str>,
    ) -> Result<InboundProof, String> {
        let deadline = Instant::now() + HUMAN_WAIT;
        while Instant::now() < deadline {
            if let Some(row) = find_inbound(db, account_id, marker, disposition, sender)? {
                return Ok(row);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Err(format!(
            "no real iMessage produced marker {marker:?} with disposition {disposition:?} within {}s",
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
                    sender.get("sender_id").and_then(serde_json::Value::as_str) == Some(sender_id)
                })
            }))
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
        helper_path: String,
        handle: String,
        destination: String,
    }

    impl LiveConfig {
        fn from_env() -> Result<Self, String> {
            fn required(name: &str) -> Result<String, String> {
                std::env::var(name)
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| format!("{name} is required"))
            }
            let helper_path = required(HELPER_ENV)?;
            if !Path::new(&helper_path).is_file() {
                return Err(format!("{HELPER_ENV} does not name an installed helper file"));
            }
            Ok(Self {
                helper_path,
                handle: required(HANDLE_ENV)?,
                destination: std::env::var(DESTINATION_ENV)
                    .unwrap_or_else(|_| "the independent Messages conversation".to_string()),
            })
        }
    }

    async fn run_case(config: &LiveConfig) -> Result<(), String> {
        if std::env::var(REQUIRE_ENV).as_deref() != Ok("1") {
            return Err(format!(
                "refusing to install a real macOS service and drive Messages unless {REQUIRE_ENV}=1"
            ));
        }

        let stamp = unique();
        let pairing_marker = format!("lm-imessage-pair-{stamp:x}");
        let marker = format!("lm-imessage-installed-{stamp:x}");
        let expected_reply = format!("{REPLY_PREFIX} {marker}");
        let model = ModelFixture::spawn(marker.clone())?;
        let mut profile: Option<String> = None;
        let mut account_id: Option<String> = None;

        let result: Result<(), String> = async {
            let created = create_profile()?;
            profile = Some(created.clone());
            let workspace = std::env::temp_dir().join(format!("lm-imessage-e2e-{stamp:x}"));
            std::fs::create_dir_all(&workspace)
                .map_err(|error| format!("create workspace {}: {error}", workspace.display()))?;
            write_recipe(&created, &workspace, &model.base)?;

            let account = add_account(&created, &config.helper_path, &config.handle)?;
            account_id = Some(account.clone());
            require_cli(
                Some(&created),
                &[
                    "channels", "policy", &account,
                    "--direct", "pairing",
                    "--group", "pairing",
                    "--activation", "always",
                ],
            )?;
            require_cli(
                Some(&created),
                &["channels", "add-route", RECIPE, "--account", &account, "--json"],
            )?;
            require_cli(Some(&created), &["channels", "enable", &account])?;

            let started_at = now_ms();
            require_cli(Some(&created), &["daemon", "install"])?;
            let first_pid = wait_for_service_pid(&created, Instant::now() + SERVICE_WAIT)?;
            wait_for_account_connected(
                &created,
                &account,
                started_at,
                Instant::now() + SERVICE_WAIT,
            )?;

            require_cli(Some(&created), &["daemon", "stop"])?;
            let restarted_at = now_ms();
            require_cli(Some(&created), &["daemon", "start"])?;
            let second_pid = wait_for_service_pid(&created, Instant::now() + SERVICE_WAIT)?;
            if first_pid == second_pid {
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
                "\nFrom an independent iMessage identity, send this exact pairing marker to {}:\n\n    {}\n",
                config.destination, pairing_marker
            );
            let challenged = wait_for_inbound(
                &db,
                &account,
                &pairing_marker,
                "challenged",
                None,
            )?;
            if challenged.sender_id.trim().is_empty() {
                return Err("Messages helper reported an empty sender id".to_string());
            }
            if !pending_contains(&created, &account, &challenged.sender_id)? {
                return Err(format!(
                    "iMessage sender {} was challenged but was not present in the production pairing queue",
                    challenged.sender_id
                ));
            }
            require_cli(
                Some(&created),
                &["channels", "approve", &account, &challenged.sender_id],
            )?;
            eprintln!(
                "approved provider-derived Messages sender {} from GUID {}",
                masked_sender(&challenged.sender_id), challenged.provider_event_id
            );

            eprintln!(
                "\nFrom that same independent iMessage identity, send this exact execution marker:\n\n    {}\n",
                marker
            );
            let inbound = wait_for_inbound(
                &db,
                &account,
                &marker,
                "accepted",
                Some(&challenged.sender_id),
            )?;
            if inbound.ingress_id.is_none() || inbound.job_id.is_none() {
                return Err(format!(
                    "accepted Messages GUID {} has no durable ingress/job",
                    inbound.provider_event_id
                ));
            }
            if inbound.conversation_id != challenged.conversation_id {
                return Err(format!(
                    "pairing and execution markers resolved to different conversations: {:?} vs {:?}",
                    challenged.conversation_id, inbound.conversation_id
                ));
            }

            let agent_deadline = Instant::now() + AGENT_WAIT;
            loop {
                let requests = model.requests();
                if requests.iter().any(|request| request.contains(&marker))
                    && requests
                        .iter()
                        .any(|request| request.contains(r#""name":"send_message""#))
                    && requests.len() >= 2
                {
                    break;
                }
                if Instant::now() >= agent_deadline {
                    return Err("installed daemon agent never completed the real send_message tool round trip".to_string());
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }

            let outbound_deadline = Instant::now() + AGENT_WAIT;
            let provider_event_id = loop {
                if let Some(id) = find_outbound(
                    &db,
                    &account,
                    &inbound.conversation_id,
                    &expected_reply,
                )? {
                    break id;
                }
                if Instant::now() >= outbound_deadline {
                    return Err(format!(
                        "helper-backed iMessage send never became a durable accepted outbound event: {expected_reply:?}"
                    ));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            };
            if !provider_event_id.starts_with("local:") {
                return Err(format!(
                    "iMessage unexpectedly invented a provider message id: {provider_event_id:?}"
                ));
            }

            eprintln!(
                "\nThe production helper reported the generated reply sent. In the independent Messages client, copy/paste the exact received reply below.\nExpected text:\n\n    {}\n",
                expected_reply
            );
            let mut observed = String::new();
            io::stdin()
                .read_line(&mut observed)
                .map_err(|error| format!("read external iMessage observation: {error}"))?;
            let observed = observed.trim_end_matches(&['\r', '\n'][..]);
            if observed != expected_reply {
                return Err(format!(
                    "independent iMessage observation did not match generated reply; expected {expected_reply:?}, got {observed:?}"
                ));
            }

            let requests = model.requests();
            if !requests.iter().any(|request| request.contains(&marker)) {
                return Err("model never received the real iMessage marker".to_string());
            }
            if !requests
                .iter()
                .any(|request| request.contains(r#""name":"send_message""#))
            {
                return Err("send_message was never offered to the installed agent".to_string());
            }
            if requests.len() < 2 {
                return Err("agent never returned to the model after sending the iMessage reply".to_string());
            }

            eprintln!(
                "literal iMessage acceptance passed: Messages GUID {} -> ingress {:?}/job {:?} -> helper send -> independent recipient observation",
                inbound.provider_event_id, inbound.ingress_id, inbound.job_id
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
    pub async fn entry() {
        let config = match LiveConfig::from_env() {
            Ok(config) => config,
            Err(error) => {
                eprintln!("iMessage installed-service E2E configuration error: {error}");
                std::process::exit(2);
            }
        };
        if let Err(error) = run_case(&config).await {
            eprintln!("iMessage installed-service E2E failed: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    macos::entry();
}
