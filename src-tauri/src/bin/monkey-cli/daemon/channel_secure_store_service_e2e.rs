//! Native secure-store -> real resident-service acceptance.
//!
//! This test exists for the boundary that unit tests and rendered service
//! manifests cannot prove:
//!
//! desktop-like process -> OS credential store -> installed OS user service
//! -> daemon process -> KeyringChannelSecrets -> Telegram adapter/poll loop.
//!
//! The writer child deliberately performs the same native keyring operation as
//! `daemon_commands::channels_set_credential`: the shared profile-scoped
//! service name plus `channel:<account_id>`, with the secret never placed in an
//! argument or environment variable. A second independent child verifies the
//! exact bytes by digest. The parent then enables that brand-new account and
//! installs the real launchd/systemd/Scheduled-Task service. The account may
//! only leave `Disconnected` for `Degraded`/`Connected` after the daemon has
//! read the credential and constructed/runs the Telegram adapter; a secure-store
//! failure is recorded as `Error` by `channel_worker::reconcile_workers` and is
//! therefore a hard failure here.
//!
//! This is a `monkey-cli` binary test rather than a Cargo integration-test
//! target on purpose. Cargo integration tests build every binary target so it
//! can expose `CARGO_BIN_EXE_*`; that would make an unrelated application bin a
//! prerequisite for this acceptance. CI explicitly builds the real monkey-cli
//! executable first and this test invokes exactly that artifact.
//!
//! The fake token is intentional. This test is about the native process/service
//! boundary, not a live Telegram account. The existing
//! `daemon::channel_agent_e2e::a_telegram_message_becomes_an_agent_reply_end_to_end`
//! test supplies a deterministic Telegram fixture and proves the rest of the
//! production path (poll -> ingress -> agent run -> send_message -> outbox ->
//! Telegram reply). CI runs that acceptance beside this one.

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const REQUIRE_ENV: &str = "LITTLE_MONKEY_REQUIRE_CHANNEL_SECURE_STORE_SERVICE_E2E";
const CHILD_MODE_ENV: &str = "LM_CHANNEL_SECURE_STORE_CHILD_MODE";
const CHILD_ACCOUNT_ENV: &str = "LM_CHANNEL_SECURE_STORE_ACCOUNT";
const CHILD_EXPECTED_SHA_ENV: &str = "LM_CHANNEL_SECURE_STORE_EXPECTED_SHA256";
const PROFILE_ENV: &str = "LITTLE_MONKEY_PROFILE";
const WRITER_TEST: &str =
    "daemon::channel_secure_store_service_e2e::native_secure_store_writer_child";
const READER_TEST: &str =
    "daemon::channel_secure_store_service_e2e::native_secure_store_reader_child";

fn cli() -> PathBuf {
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
    let executable = if cfg!(windows) {
        "monkey-cli.exe"
    } else {
        "monkey-cli"
    };
    target_dir.join("debug").join(executable)
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
            "prebuilt monkey-cli is missing at {}; CI must run `cargo build --bin monkey-cli` before this test",
            binary.display()
        ));
    }
    let mut command = Command::new(binary);
    command.args(args);
    if let Some(profile) = profile {
        command.env(PROFILE_ENV, profile);
    }
    command
        .output()
        .map_err(|error| format!("failed to start monkey-cli {args:?}: {error}"))
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
        .ok_or_else(|| {
            format!(
                "profile JSON had no id: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
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
        .ok_or_else(|| {
            format!(
                "account JSON had no account_id: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
}

fn run_boundary_child(
    mode: &str,
    profile: &str,
    account_id: &str,
    stdin: Option<&str>,
    expected_sha256: Option<&str>,
) -> Result<(), String> {
    let test_name = match mode {
        "writer" => WRITER_TEST,
        "reader" => READER_TEST,
        other => return Err(format!("unknown boundary-child mode {other}")),
    };
    let mut command = Command::new(std::env::current_exe().map_err(|error| error.to_string())?);
    command
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(CHILD_MODE_ENV, mode)
        .env(CHILD_ACCOUNT_ENV, account_id)
        .env(PROFILE_ENV, profile)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(expected) = expected_sha256 {
        command.env(CHILD_EXPECTED_SHA_ENV, expected);
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
    let output = child
        .wait_with_output()
        .map_err(|error| format!("failed waiting for native {mode} child: {error}"))?;
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

fn exact_digest(secret: &str) -> String {
    Sha256::digest(secret.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn wait_for_resident_credential_use(
    profile: &str,
    account_id: &str,
    secret: &str,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut last_status = String::new();
    let mut last_account = String::new();

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
                    if service_running
                        && heartbeat_fresh
                        && pid != u64::from(std::process::id())
                    {
                        let listed =
                            require_cli(Some(profile), &["channels", "list", "--json"])?;
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
                            .ok_or_else(|| {
                                format!("account {account_id} disappeared from channels list")
                            })?;
                        last_account = account.to_string();
                        let health = account
                            .get("health")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("unknown");
                        if health == "error" {
                            return Err(format!(
                                "the real resident service started but could not build the account; this is the exact secure-store/service failure this test guards against: {last_account}"
                            ));
                        }
                        if matches!(health, "degraded" | "connected") {
                            let rendered = account.to_string();
                            if rendered.contains(secret) {
                                return Err(
                                    "the Telegram credential leaked into channel health/status"
                                        .to_string(),
                                );
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
        "the installed resident service never proved it could consume the desktop-written credential within 120s\nlast daemon status: {last_status}\nlast account: {last_account}"
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
        eprintln!(
            "channel secure-store native service E2E skipped; set {REQUIRE_ENV}=1 to require it"
        );
        return Ok(());
    }

    let mut profile: Option<String> = None;
    let mut account_id: Option<String> = None;
    let result = (|| -> Result<(), String> {
        let created = create_profile()?;
        profile = Some(created.clone());
        let account = add_telegram_account(&created)?;
        account_id = Some(account.clone());

        // Syntactically token-like but intentionally not a live credential.
        // It is unique per run, so a successful read of this brand-new account
        // cannot be satisfied by a stale keychain item from another test.
        let secret = format!(
            "999999999:{}{}{}",
            case,
            std::process::id(),
            nonce()
        );

        // Process A: the desktop boundary. Secret travels only on stdin.
        run_boundary_child("writer", &created, &account, Some(&secret), None)?;

        // The desktop performs this metadata mutation after the native keychain
        // write. Keeping it separate here exactly mirrors the current production
        // ordering and catches a credential that exists but is not referenced by
        // the durable channel row.
        require_cli(
            Some(&created),
            &["channels", "mark-credential", &account],
        )?;

        // Process B: prove the exact bytes survive the native store before the
        // service is involved. Only a SHA-256 digest crosses into this child.
        let digest = exact_digest(&secret);
        run_boundary_child("reader", &created, &account, None, Some(&digest))?;

        require_cli(Some(&created), &["channels", "enable", &account])?;

        // This is the production OS lifecycle, not a spawned `daemon serve`:
        // launchctl bootstrap/kickstart on macOS, systemctl --user on Linux, and
        // schtasks with InteractiveToken on Windows. `daemon install` itself
        // waits for a fresh heartbeat from that resident process.
        let installed = require_cli(Some(&created), &["daemon", "install"])?;
        let install_text = String::from_utf8_lossy(&installed.stdout);
        if !install_text.contains("Installed") {
            return Err(format!(
                "daemon install returned unexpected output: {install_text}"
            ));
        }

        wait_for_resident_credential_use(&created, &account, &secret)?;
        Ok(())
    })();

    cleanup(profile.as_deref(), account_id.as_deref());
    result
}

/// Child process standing in for the desktop Tauri process. It intentionally
/// spells the production `channels_set_credential` keyring write the same way:
/// profile-scoped service + `channel:<id>` account + native `set_password`.
#[test]
fn native_secure_store_writer_child() {
    if std::env::var(CHILD_MODE_ENV).as_deref() != Ok("writer") {
        return;
    }
    let account_id = std::env::var(CHILD_ACCOUNT_ENV).expect("writer account id");
    let mut secret = String::new();
    std::io::stdin()
        .read_to_string(&mut secret)
        .expect("read writer secret from stdin");
    assert!(!secret.is_empty(), "writer received an empty credential");
    assert!(
        secret.len() <= 8192,
        "writer credential exceeded desktop bound"
    );
    let reference = little_monkey_lib::channels::credential_ref(&account_id);
    keyring::Entry::new(&little_monkey_lib::channels::KEYCHAIN_SERVICE, &reference)
        .expect("open native credential entry")
        .set_password(&secret)
        .expect("write native credential");
}

/// Independent process that verifies exact credential bytes without printing
/// them. This is deliberately separate from the resident-service assertion:
/// together they prove both exact native persistence and real service access.
#[test]
fn native_secure_store_reader_child() {
    if std::env::var(CHILD_MODE_ENV).as_deref() != Ok("reader") {
        return;
    }
    let account_id = std::env::var(CHILD_ACCOUNT_ENV).expect("reader account id");
    let expected = std::env::var(CHILD_EXPECTED_SHA_ENV).expect("expected digest");
    let reference = little_monkey_lib::channels::credential_ref(&account_id);
    let secret = keyring::Entry::new(&little_monkey_lib::channels::KEYCHAIN_SERVICE, &reference)
        .expect("open native credential entry")
        .get_password()
        .expect("read native credential");
    assert_eq!(
        exact_digest(&secret),
        expected,
        "native credential digest mismatch"
    );
}

#[test]
fn real_os_service_reads_desktop_written_credential() {
    if let Err(error) = run_real_service_case("desktop-") {
        panic!("{error}");
    }
}

/// Linux's persistent keyring configuration intentionally supports a user
/// service with no Secret Service provider. CI runs this only after removing
/// `org.freedesktop.secrets` from the user bus, so the native keyutils side of
/// the combined keyring backend is exercised across the same real systemd
/// service boundary.
#[cfg(target_os = "linux")]
#[test]
fn real_systemd_service_reads_headless_native_credential() {
    if let Err(error) = run_real_service_case("headless-") {
        panic!("{error}");
    }
}
