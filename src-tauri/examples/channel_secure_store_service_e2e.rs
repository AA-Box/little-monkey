//! Native secure-store -> real resident-service acceptance harness.
//!
//! CI prebuilds only the production `monkey-cli` binary plus this harness. The
//! harness then proves the boundary that rendered service definitions cannot:
//!
//! independent writer process -> OS credential store -> installed OS user
//! service -> daemon process -> KeyringChannelSecrets -> Telegram poll loop.
//!
//! The successful Telegram transport/agent/reply half is covered separately by
//! the existing `daemon::channel_agent_e2e` acceptance with deterministic
//! Telegram/model fixtures. Keeping the two tests compositional avoids adding a
//! production-only endpoint override to the resident daemon.

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// No single child in this harness has a legitimate reason to run for minutes:
/// every one of them either edits local state or waits on a bounded poll. A
/// stuck child (a macOS keychain prompt with nobody to answer it, a service
/// that never terminates) would otherwise wedge the whole CI job silently, so
/// wait for it here and fail with the command that stopped.
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);

const REQUIRE_ENV: &str = "LITTLE_MONKEY_REQUIRE_CHANNEL_SECURE_STORE_SERVICE_E2E";
const PROFILE_ENV: &str = "LITTLE_MONKEY_PROFILE";
const EXPECTED_SHA_ENV: &str = "LM_CHANNEL_SECURE_STORE_EXPECTED_SHA256";

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

fn nonce() -> u128 {
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

fn run_cli(profile: Option<&str>, args: &[&str]) -> Result<Output, String> {
    let binary = cli();
    if !binary.is_file() {
        return Err(format!(
            "prebuilt monkey-cli is missing at {}; run `cargo build --bin monkey-cli` first",
            binary.display()
        ));
    }
    let size = std::fs::metadata(&binary)
        .map_err(|error| format!("could not stat prebuilt monkey-cli at {}: {error}", binary.display()))?
        .len();
    if size == 0 {
        return Err(format!(
            "prebuilt monkey-cli at {} is the zero-byte Tauri bootstrap placeholder, not the real executable",
            binary.display()
        ));
    }
    let mut command = Command::new(binary);
    command.args(args);
    if let Some(profile) = profile {
        command.env(PROFILE_ENV, profile);
    }
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start monkey-cli {args:?}: {error}"))?;
    bounded_output(child, &format!("monkey-cli {args:?}"))
}

/// Wait for a child, killing it once it is clearly stuck.
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
    let name = format!("Channel secure-store E2E {}", nonce());
    let output = require_cli(None, &["profiles", "create", &name, "--json"])?;
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("profile JSON was invalid: {error}\n{}", output_text(&output)))?;
    value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("profile JSON had no id: {}", String::from_utf8_lossy(&output.stdout)))
}

fn add_telegram_account(profile: &str) -> Result<String, String> {
    let label = format!("native-secure-store-e2e-{}", nonce());
    let output = require_cli(
        Some(profile),
        &["channels", "add", "telegram", &label, "--json"],
    )?;
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("account JSON was invalid: {error}\n{}", output_text(&output)))?;
    value
        .get("account_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("account JSON had no account_id: {}", String::from_utf8_lossy(&output.stdout)))
}

fn exact_digest(secret: &str) -> String {
    Sha256::digest(secret.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn run_boundary_child(
    mode: &str,
    profile: &str,
    account_id: &str,
    stdin: Option<&str>,
    expected_sha256: Option<&str>,
) -> Result<(), String> {
    let mut command = Command::new(std::env::current_exe().map_err(|error| error.to_string())?);
    command
        .arg(mode)
        .arg(account_id)
        .env(PROFILE_ENV, profile)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(expected) = expected_sha256 {
        command.env(EXPECTED_SHA_ENV, expected);
    }
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to start native {mode} child: {error}"))?;
    if let Some(value) = stdin {
        child
            .stdin
            .take()
            .ok_or_else(|| format!("native {mode} child had no stdin"))?
            .write_all(value.as_bytes())
            .map_err(|error| format!("failed to write native {mode} stdin: {error}"))?;
    }
    let output = bounded_output(child, &format!("native {mode} child"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "native {mode} child failed with {}\n{}",
            output.status,
            output_text(&output)
        ))
    }
}

fn writer(account_id: &str) -> Result<(), String> {
    let mut secret = String::new();
    std::io::stdin()
        .read_to_string(&mut secret)
        .map_err(|error| format!("read writer secret from stdin: {error}"))?;
    if secret.is_empty() || secret.len() > 8192 {
        return Err("writer received an invalid credential length".to_string());
    }
    let reference = little_monkey_lib::channels::credential_ref(account_id);
    keyring::Entry::new(&little_monkey_lib::channels::KEYCHAIN_SERVICE, &reference)
        .map_err(|error| format!("open native credential entry: {error}"))?
        .set_password(&secret)
        .map_err(|error| format!("write native credential: {error}"))
}

fn reader(account_id: &str) -> Result<(), String> {
    let expected = std::env::var(EXPECTED_SHA_ENV)
        .map_err(|_| "reader did not receive expected digest".to_string())?;
    let reference = little_monkey_lib::channels::credential_ref(account_id);
    let secret = keyring::Entry::new(&little_monkey_lib::channels::KEYCHAIN_SERVICE, &reference)
        .map_err(|error| format!("open native credential entry: {error}"))?
        .get_password()
        .map_err(|error| format!("read native credential: {error}"))?;
    let actual = exact_digest(&secret);
    if actual != expected {
        return Err(format!(
            "native credential digest mismatch: expected {expected}, got {actual}"
        ));
    }
    Ok(())
}

fn wait_for_resident_credential_use(
    profile: &str,
    account_id: &str,
    secret: &str,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut last_status = String::new();
    let mut last_health = "unknown";

    while Instant::now() < deadline {
        if let Ok(output) = run_cli(Some(profile), &["daemon", "status", "--json"]) {
            if output.status.success() {
                if let Ok(status) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                    last_status = status.to_string();
                    let service_running = status
                        .get("service_running")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let heartbeat_fresh = status
                        .get("heartbeat_fresh")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let pid = status
                        .get("pid")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or_default();

                    if service_running && heartbeat_fresh && pid != u64::from(std::process::id()) {
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
                            .ok_or_else(|| format!("account {account_id} disappeared from channels list"))?;
                        // Only a fixed label crosses back out of the account
                        // payload: the harness reports why it is still waiting
                        // without ever writing credential-bearing JSON to a log.
                        last_health = match account.get("health").and_then(serde_json::Value::as_str)
                        {
                            Some("connected") => "connected",
                            Some("degraded") => "degraded",
                            Some("error") => "error",
                            Some("disconnected") => "disconnected",
                            Some(_) | None => "unknown",
                        };

                        if last_health == "error" {
                            return Err(
                                "resident service started but failed to build the channel account"
                                    .to_string(),
                            );
                        }
                        if matches!(last_health, "degraded" | "connected") {
                            if account.to_string().contains(secret) {
                                return Err("Telegram credential leaked into channel status".to_string());
                            }
                            return Ok(());
                        }
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(750));
    }

    Err(format!(
        "installed resident service never proved it consumed the desktop-written credential within 120s\nlast daemon status: {last_status}\nlast account health: {last_health}"
    ))
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

fn run_real_service_case(case: &str) -> Result<(), String> {
    if std::env::var(REQUIRE_ENV).as_deref() != Ok("1") {
        return Err(format!(
            "refusing to install a real OS service unless {REQUIRE_ENV}=1"
        ));
    }

    let mut profile: Option<String> = None;
    let mut account_id: Option<String> = None;
    let mut redact = String::new();
    let result = (|| -> Result<(), String> {
        let created = create_profile()?;
        profile = Some(created.clone());
        let account = add_telegram_account(&created)?;
        account_id = Some(account.clone());

        // Unique, syntactically token-like, deliberately non-live credential.
        let secret = format!("999999999:{case}-{}-{}", std::process::id(), nonce());
        redact = secret.clone();

        // Process A: desktop boundary. Secret travels only via stdin.
        run_boundary_child("writer", &created, &account, Some(&secret), None)?;

        // Mirrors the production mutation after channels_set_credential writes
        // the native keychain item.
        require_cli(Some(&created), &["channels", "mark-credential", &account])?;

        // Process B: exact byte persistence; only a digest crosses into it.
        let digest = exact_digest(&secret);
        run_boundary_child("reader", &created, &account, None, Some(&digest))?;

        require_cli(Some(&created), &["channels", "enable", &account])?;

        // Production lifecycle: launchctl, systemctl --user, or Scheduled Task.
        let installed = require_cli(Some(&created), &["daemon", "install"])?;
        let install_text = String::from_utf8_lossy(&installed.stdout);
        if !install_text.contains("Installed") {
            return Err(format!("daemon install returned unexpected output: {install_text}"));
        }

        wait_for_resident_credential_use(&created, &account, &secret)
    })();

    if result.is_err() {
        if let Some(profile) = profile.as_deref() {
            dump_daemon_logs(profile, &redact);
            if let Some(account_id) = account_id.as_deref() {
                probe_credential_through_cli(profile, account_id);
            }
        }
    }
    cleanup(profile.as_deref(), account_id.as_deref());
    result
}

/// Read the same credential through the CLI, which is the daemon's own binary.
///
/// An account stuck at `disconnected` while the heartbeat stays fresh means the
/// daemon's read never returned rather than failing, and the usual reason a
/// Keychain read never returns is a prompt no CI runner can answer. This splits
/// the two possibilities without the service in the way: a timeout here is the
/// binary blocking on the native store, while an ordinary error means the read
/// works and the daemon stopped somewhere else.
fn probe_credential_through_cli(profile: &str, account_id: &str) {
    match run_cli(Some(profile), &["channels", "probe", account_id, "--json"]) {
        Ok(output) => eprintln!(
            "--- CLI credential probe exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout).trim()
        ),
        Err(error) => eprintln!("--- CLI credential probe: {error}"),
    }
    #[cfg(target_os = "macos")]
    match run_program("/usr/bin/security", &["list-keychains", "-d", "user"]) {
        Ok(output) => eprintln!(
            "--- keychain search list: {}",
            String::from_utf8_lossy(&output.stdout).trim()
        ),
        Err(error) => eprintln!("--- keychain search list unavailable: {error}"),
    }
}

#[cfg(target_os = "macos")]
fn run_program(program: &str, args: &[&str]) -> Result<Output, String> {
    let child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start {program}: {error}"))?;
    bounded_output(child, program)
}

/// Print what the installed service itself recorded, before cleanup deletes the
/// profile that holds it.
///
/// A daemon that never reports a channel is indistinguishable from one that
/// reported a failure until you read its own stderr: an unreadable credential
/// marks the account `error`, so an account still sitting at `disconnected`
/// means the read never returned at all.
fn dump_daemon_logs(profile: &str, secret: &str) {
    let Some(base) = little_monkey_lib::app_paths::base_data_dir() else {
        eprintln!("--- no base data directory to read daemon logs from");
        return;
    };
    let logs = little_monkey_lib::profiles::profile_root(&base, profile)
        .join("daemon")
        .join("logs");
    let entries = match std::fs::read_dir(&logs) {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("--- no daemon logs at {}: {error}", logs.display());
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match std::fs::read_to_string(&path) {
            // The harness's own credential is the one secret known to be in
            // scope here, and CI logs are public in a fork build.
            Ok(text) => {
                let lines: Vec<&str> = text.lines().collect();
                let tail = lines.len().saturating_sub(40);
                eprintln!("--- {} (last {} lines)", path.display(), lines.len() - tail);
                for line in &lines[tail..] {
                    eprintln!("{}", line.replace(secret, "<redacted>"));
                }
            }
            Err(error) => eprintln!("--- {} unreadable: {error}", path.display()),
        }
    }
}

fn real_main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().ok_or_else(|| {
        "usage: channel_secure_store_service_e2e <desktop|writer|reader> [account_id]"
            .to_string()
    })?;

    match mode.as_str() {
        "writer" => writer(&args.next().ok_or_else(|| "writer requires account id".to_string())?),
        "reader" => reader(&args.next().ok_or_else(|| "reader requires account id".to_string())?),
        "desktop" => run_real_service_case("desktop"),
        other => Err(format!("unknown mode {other}")),
    }
}

fn main() {
    if let Err(error) = real_main() {
        eprintln!("channel secure-store service E2E failed: {error}");
        std::process::exit(1);
    }
}
