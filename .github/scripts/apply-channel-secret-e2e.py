from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if old not in text:
        raise SystemExit(f"{label} anchor not found")
    return text.replace(old, new, 1)


daemon = Path("src-tauri/src/daemon_commands.rs")
text = daemon.read_text()
text = replace_once(
    text,
    "use std::process::Command;",
    "use std::process::{Command, Stdio};",
    "process import",
)

anchor = '''fn run_cli_with_secret(args: Vec<String>, secret: String) -> Result<String, String> {
    let output = Command::new(cli_path())
        .args(&args)
        .env("LM_EXTENSION_WEBHOOK_SECRET", secret)
        .output()
        .map_err(|error| format!("Failed to start bundled monkey-cli: {error}"))?;
    finish_cli_output(output)
}
'''
addition = anchor + '''
/// Feed a secret to a fixed bundled CLI command over stdin. This is the only
/// safe bridge for credentials that must be created by monkey-cli itself: no
/// argument, environment variable, log line, or shell ever contains the value.
fn run_cli_with_stdin_secret(args: Vec<String>, secret: String) -> Result<String, String> {
    use std::io::Write as _;

    let mut child = Command::new(cli_path())
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Failed to start bundled monkey-cli: {error}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Bundled monkey-cli stdin was unavailable".to_string())?;
    stdin
        .write_all(secret.as_bytes())
        .map_err(|error| format!("Failed to pass credential to bundled monkey-cli: {error}"))?;
    drop(stdin);
    let output = child
        .wait_with_output()
        .map_err(|error| format!("Failed waiting for bundled monkey-cli: {error}"))?;
    finish_cli_output(output)
}
'''
text = replace_once(text, anchor, addition, "run_cli_with_secret")

old = '''#[tauri::command]
pub async fn channels_set_credential(account_id: String, secret: String) -> Result<(), String> {
    let account_id = channel_id("account id", &account_id)?;
    if secret.is_empty() || secret.len() > 8192 {
        return Err("A messaging credential must contain 1-8192 bytes".to_string());
    }
    let reference = crate::channels::credential_ref(&account_id);
    keyring::Entry::new(&crate::channels::KEYCHAIN_SERVICE, &reference)
        .map_err(|error| format!("Failed to open the messaging keychain entry: {error}"))?
        .set_password(&secret)
        .map_err(|error| format!("Failed to save the messaging credential: {error}"))?;
    // The CLI owns the account row; this marks it as having a credential.
    command(vec![
        "channels".into(),
        "mark-credential".into(),
        account_id,
    ])
    .await
    .map(|_| ())
}
'''
new = '''#[tauri::command]
pub async fn channels_set_credential(account_id: String, secret: String) -> Result<(), String> {
    let account_id = channel_id("account id", &account_id)?;
    if secret.is_empty() || secret.len() > 8192 {
        return Err("A messaging credential must contain 1-8192 bytes".to_string());
    }

    // monkey-cli is also the resident daemon executable. Let that executable
    // create the OS credential so macOS Keychain ACL ownership, Windows
    // Credential Manager identity, and Linux keyring selection are identical
    // between the writer and the later daemon reader. The token crosses the
    // process boundary only on stdin and `set-token` also updates the account
    // row after the secure-store write succeeds.
    let args = vec!["channels".into(), "set-token".into(), account_id];
    tokio::task::spawn_blocking(move || run_cli_with_stdin_secret(args, secret))
        .await
        .map_err(|error| error.to_string())??;
    Ok(())
}
'''
text = replace_once(text, old, new, "channels_set_credential")
daemon.write_text(text)

channels = Path("src-tauri/src/bin/monkey-cli/channels_cli.rs")
text = channels.read_text()
import_anchor = "use little_monkey_lib::channels::types::{ChannelHealth, ChannelKind, HealthState};\n"
text = replace_once(
    text,
    import_anchor,
    import_anchor + "use sha2::{Digest, Sha256};\n",
    "channels import",
)

enum_anchor = '''    /// Store the account's credential, read from stdin so it never lands in a
    /// shell history or a process listing.
    SetToken { account_id: String },
'''
enum_new = enum_anchor + '''    /// Native secure-store acceptance helper. Hidden because it is an internal
    /// CI/diagnostic boundary check, not a way to retrieve credentials.
    #[command(hide = true)]
    CredentialReadCheck {
        account_id: String,
        expected_sha256: String,
    },
'''
text = replace_once(text, enum_anchor, enum_new, "ChannelsCmd SetToken")

dispatch_anchor = "        ChannelsCmd::SetToken { account_id } => set_token(account_id),\n"
dispatch_new = dispatch_anchor + '''        ChannelsCmd::CredentialReadCheck {
            account_id,
            expected_sha256,
        } => credential_read_check(account_id, expected_sha256),
'''
text = replace_once(text, dispatch_anchor, dispatch_new, "dispatch SetToken")

set_token_anchor = '''pub fn set_token(account_id: &str) -> Result<(), String> {
    let mut store = store()?;
'''
idx = text.find(set_token_anchor)
if idx < 0:
    raise SystemExit("set_token function not found")
insert_at = text.rfind("/// Store the account", 0, idx)
if insert_at < 0:
    raise SystemExit("set_token doc anchor not found")
helper = '''/// Read the credential exactly the way the daemon does, verify only a caller-
/// supplied digest, and prove the provider adapter can be constructed from the
/// resolved value. The secret is never printed. This command is deliberately
/// unavailable unless the native E2E gate opts in through an environment flag.
fn credential_read_check(account_id: &str, expected_sha256: &str) -> Result<(), String> {
    if std::env::var("LITTLE_MONKEY_CHANNEL_SECURE_STORE_E2E").as_deref() != Ok("1") {
        return Err("credential-read-check is reserved for the native secure-store acceptance test".to_string());
    }
    if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("expected SHA-256 must be 64 hexadecimal characters".to_string());
    }

    let store = store()?;
    let account = store
        .channel_account(account_id)?
        .ok_or_else(|| format!("No such account '{account_id}'"))?;
    let reference = account
        .credential_ref
        .as_deref()
        .ok_or_else(|| format!("Account '{account_id}' has no credential reference"))?;
    let secret = KeyringChannelSecrets.get(reference)?;
    let digest = Sha256::digest(secret.as_bytes());
    let actual = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err("The resident-process credential did not match the value written by the first process".to_string());
    }

    let adapter = crate::daemon::adapters::build_adapter(
        &AdapterConfig {
            account: &account,
            secret,
        },
        Some(&DaemonPaths::resolve()?),
    )?;
    if adapter.kind() != account.kind {
        return Err("The credential resolved but built the wrong provider adapter".to_string());
    }
    println!("native secure-store read and adapter construction succeeded");
    Ok(())
}

'''
text = text[:insert_at] + helper + text[insert_at:]
channels.write_text(text)

test = Path("src-tauri/tests/channel_secure_store_e2e.rs")
test.write_text(r'''//! Real OS secure-store acceptance for messaging credentials.
//!
//! Opt-in outside CI because it creates one short-lived native credential and
//! one short-lived channel account. CI requires it on every supported desktop
//! OS. The first monkey-cli process writes through production `set-token`; a
//! second process reads through `KeyringChannelSecrets` and constructs the
//! Telegram adapter. No provider network call or real token is required.

use sha2::{Digest, Sha256};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn cli() -> &'static str {
    env!("CARGO_BIN_EXE_monkey-cli")
}

fn run(args: &[&str]) -> Result<std::process::Output, String> {
    Command::new(cli())
        .args(args)
        .output()
        .map_err(|error| format!("failed to run monkey-cli: {error}"))
}

fn remove_account(account_id: &str) {
    let _ = Command::new(cli())
        .args(["channels", "remove", account_id])
        .output();
}

#[test]
fn desktop_writer_to_resident_reader_round_trips_native_credential() {
    if std::env::var("LITTLE_MONKEY_REQUIRE_CHANNEL_SECURE_STORE_E2E").as_deref() != Ok("1") {
        eprintln!("channel secure-store E2E skipped; set LITTLE_MONKEY_REQUIRE_CHANNEL_SECURE_STORE_E2E=1 to require it");
        return;
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let label = format!("secure-store-e2e-{}-{nonce}", std::process::id());
    let added = run(&["channels", "add", "telegram", &label, "--json"]).expect("add account");
    assert!(added.status.success(), "add failed: {}", String::from_utf8_lossy(&added.stderr));
    let value: serde_json::Value = serde_json::from_slice(&added.stdout).expect("account JSON");
    let account_id = value
        .get("account_id")
        .and_then(serde_json::Value::as_str)
        .expect("account_id")
        .to_string();

    let result = (|| -> Result<(), String> {
        let secret = format!("lm-native-e2e:{}:{nonce}:{}", std::env::consts::OS, std::process::id());
        let mut writer = Command::new(cli())
            .args(["channels", "set-token", &account_id])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("failed to spawn writer: {error}"))?;
        writer
            .stdin
            .take()
            .ok_or("writer stdin missing")?
            .write_all(secret.as_bytes())
            .map_err(|error| format!("failed to write token: {error}"))?;
        let written = writer
            .wait_with_output()
            .map_err(|error| format!("failed waiting for writer: {error}"))?;
        if !written.status.success() {
            return Err(format!("writer failed: {}", String::from_utf8_lossy(&written.stderr)));
        }

        let digest = Sha256::digest(secret.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let home = dirs::home_dir().ok_or("could not resolve user home")?;
        let mut reader = Command::new(cli());
        reader
            .args(["channels", "credential-read-check", &account_id, &digest])
            .env("LITTLE_MONKEY_CHANNEL_SECURE_STORE_E2E", "1")
            .env("HOME", &home)
            .current_dir(&home);
        #[cfg(target_os = "windows")]
        reader.env("USERPROFILE", &home);
        #[cfg(target_os = "linux")]
        {
            if let Some(address) = std::env::var_os("DBUS_SESSION_BUS_ADDRESS") {
                reader.env("DBUS_SESSION_BUS_ADDRESS", address);
            }
            if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
                reader.env("XDG_RUNTIME_DIR", runtime);
            }
        }
        let read = reader
            .output()
            .map_err(|error| format!("failed to spawn resident reader: {error}"))?;
        if !read.status.success() {
            return Err(format!("resident reader failed: {}", String::from_utf8_lossy(&read.stderr)));
        }
        let stdout = String::from_utf8_lossy(&read.stdout);
        if !stdout.contains("native secure-store read and adapter construction succeeded") {
            return Err(format!("resident reader returned unexpected output: {stdout}"));
        }
        Ok(())
    })();

    remove_account(&account_id);
    if let Err(error) = result {
        panic!("{error}");
    }
}
''')

ci = Path(".github/workflows/ci.yml")
text = ci.read_text()
env_anchor = "      LITTLE_MONKEY_REQUIRE_COW_TESTS: 1\n"
text = replace_once(
    text,
    env_anchor,
    env_anchor
    + "      # Real native secure-store boundary required on Linux, Windows, and macOS.\n"
    + "      LITTLE_MONKEY_REQUIRE_CHANNEL_SECURE_STORE_E2E: 1\n",
    "CI rust-tests env",
)
ci.write_text(text)
