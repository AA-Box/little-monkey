//! CLI-side lifecycle for the embeddings-only `llama-server` instance (RAG
//! design doc slice 4, CLI parity for the `llama` embedding backend).
//!
//! Before this module existed, nothing in `monkey-cli` could ever start this
//! process at all: `embed_via_llama` (now `knowledge_core.rs`, re-exported by
//! `stacks`) just POSTs to
//! `http://127.0.0.1:{EMBED_PORT}/v1/embeddings` with no fallback, so any
//! `llama`-backend stack (the curated/default backend — see
//! `KnowledgePanel.tsx`) failed outright from a terminal unless the desktop
//! app happened to already have its embeddings server running. The desktop
//! app's equivalent (`llama::embed_server_start`) keeps the spawned
//! `std::process::Child` inside its own long-running `AppState`, so a later
//! Tauri command can find and kill it — `monkey-cli` has no such thing: every
//! invocation is its own short-lived process, so there is nothing for a
//! later `monkey-cli` invocation to hold onto in memory. Lifecycle here
//! therefore uses a durable ownership record: `start` spawns `llama-server`,
//! waits for it to become healthy (reusing `llama::find_llama_server_binary`/
//! `llama::embed_server_args` so the actual binary/flags never drift from the
//! desktop app's), verifies `/v1/embeddings` actually works (mirroring
//! `llama::embed_server_start`'s own verification step), and atomically writes
//! the pid plus immutable process identity and a per-launch nonce alias to
//! `<stacks-dir>/embed-server.pid`. It then returns while deliberately leaving
//! the process running in the background (a plain `std::process::Child` is
//! NOT killed when its handle is dropped, unlike `tokio::process::Child` with
//! `kill_on_drop`). `status`/`stop` only trust the record when that exact
//! identity still belongs to the recorded pid, so a recycled pid is never
//! treated as Little Monkey's process.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use little_monkey_lib::{egress, llama};
use serde::{Deserialize, Serialize};

const PROCESS_RECORD_SCHEMA_VERSION: u32 = 1;
const MAX_PROCESS_RECORD_BYTES: u64 = 32 * 1024;
const MAX_PROCESS_IDENTITY_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EmbedProcessRecord {
    schema_version: u32,
    pid: u32,
    process_identity: String,
    alias: String,
    model_path: String,
    binary_path: String,
}

fn pid_file_path() -> Option<PathBuf> {
    crate::stacks_cli::base_dir().map(|d| d.join("embed-server.pid"))
}

fn validate_process_record(record: EmbedProcessRecord) -> Result<EmbedProcessRecord, String> {
    if record.schema_version != PROCESS_RECORD_SCHEMA_VERSION
        || record.pid == 0
        || record.process_identity.is_empty()
        || record.process_identity.len() > MAX_PROCESS_IDENTITY_BYTES
        || record.alias.len() < 32
        || !record.alias.starts_with("little-monkey-")
        || record.model_path.is_empty()
        || record.binary_path.is_empty()
        || !record.process_identity.contains(&record.alias)
        || !record.process_identity.contains(&record.model_path)
        || !record.process_identity.contains(&record.binary_path)
    {
        return Err("Embedding server ownership record is invalid".to_string());
    }
    Ok(record)
}

fn parse_process_record(bytes: &[u8]) -> Result<EmbedProcessRecord, String> {
    if bytes.len() as u64 > MAX_PROCESS_RECORD_BYTES {
        return Err("Embedding server ownership record is too large".to_string());
    }
    let record = serde_json::from_slice(bytes)
        .map_err(|error| format!("Embedding server ownership record is invalid: {error}"))?;
    validate_process_record(record)
}

fn read_process_record() -> Result<Option<EmbedProcessRecord>, String> {
    let Some(path) = pid_file_path() else {
        return Ok(None);
    };
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Failed to inspect embedding server ownership record {}: {error}",
                path.display()
            ))
        }
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_PROCESS_RECORD_BYTES {
        return Err(format!(
            "Embedding server ownership record {} is not a bounded regular file",
            path.display()
        ));
    }
    let bytes = std::fs::read(&path).map_err(|error| {
        format!(
            "Failed to read embedding server ownership record {}: {error}",
            path.display()
        )
    })?;
    parse_process_record(&bytes).map(Some)
}

fn write_process_record(record: &EmbedProcessRecord) -> Result<(), String> {
    validate_process_record(record.clone())?;
    let path = pid_file_path().ok_or("Could not resolve the app data directory")?;
    let parent = path
        .parent()
        .ok_or("Embedding server ownership record has no parent directory")?;
    std::fs::create_dir_all(parent).map_err(|error| {
        format!(
            "Failed to create embedding server state directory {}: {error}",
            parent.display()
        )
    })?;
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|error| format!("Failed to serialize embedding server ownership: {error}"))?;
    if bytes.len() as u64 > MAX_PROCESS_RECORD_BYTES {
        return Err("Embedding server ownership record is too large".to_string());
    }
    let temporary = parent.join(format!(
        ".embed-server-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let write_result = (|| -> Result<(), String> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(|error| {
            format!(
                "Failed to stage embedding server ownership {}: {error}",
                temporary.display()
            )
        })?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| {
                format!(
                    "Failed to write embedding server ownership {}: {error}",
                    temporary.display()
                )
            })?;
        std::fs::rename(&temporary, &path).map_err(|error| {
            format!(
                "Failed to publish embedding server ownership {}: {error}",
                path.display()
            )
        })
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    write_result
}

fn clear_process_record() {
    if let Some(path) = pid_file_path() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(unix)]
fn process_identity(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args([
            "-ww",
            "-p",
            &pid.to_string(),
            "-o",
            "lstart=",
            "-o",
            "command=",
        ])
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.len() > MAX_PROCESS_IDENTITY_BYTES {
        return None;
    }
    let identity = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!identity.is_empty()).then_some(identity)
}

#[cfg(windows)]
fn process_identity(pid: u32) -> Option<String> {
    let script = format!(
        "$p=Get-CimInstance Win32_Process -Filter \"ProcessId = {pid}\"; \
         if ($null -ne $p) {{ Write-Output ($p.CreationDate.ToUniversalTime().Ticks.ToString() + '|' + $p.ExecutablePath + '|' + $p.CommandLine) }}"
    );
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.len() > MAX_PROCESS_IDENTITY_BYTES {
        return None;
    }
    let identity = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!identity.is_empty()).then_some(identity)
}

#[cfg(not(any(unix, windows)))]
fn process_identity(_pid: u32) -> Option<String> {
    None
}

fn process_record_matches_identity(
    record: &EmbedProcessRecord,
    current_identity: Option<&str>,
) -> bool {
    current_identity == Some(record.process_identity.as_str())
}

fn process_record_matches(record: &EmbedProcessRecord) -> bool {
    let current_identity = process_identity(record.pid);
    process_record_matches_identity(record, current_identity.as_deref())
}

#[cfg(unix)]
fn terminate_process(pid: u32) -> Result<(), String> {
    let status = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .map_err(|error| format!("Failed to stop process {pid}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "Failed to stop process {pid}: kill exited {status}"
        ))
    }
}

#[cfg(windows)]
fn terminate_process(pid: u32) -> Result<(), String> {
    let status = std::process::Command::new("taskkill.exe")
        .args(["/PID", &pid.to_string(), "/T"])
        .status()
        .map_err(|error| format!("Failed to stop process {pid}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "Failed to stop process {pid}: taskkill exited {status}"
        ))
    }
}

#[cfg(not(any(unix, windows)))]
fn terminate_process(pid: u32) -> Result<(), String> {
    Err(format!(
        "Stopping embedding process {pid} is unsupported on this platform"
    ))
}

fn stop_owned_process(record: &EmbedProcessRecord) -> Result<bool, String> {
    if !process_record_matches(record) {
        return Ok(false);
    }
    terminate_process(record.pid)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if !process_record_matches(record) {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!(
        "Embedding server process {} did not stop within 5 seconds",
        record.pid
    ))
}

/// `monkey-cli stacks embed-server start --model-path <path>` — spawns the
/// embeddings-only `llama-server` instance against `model_path` (the same
/// argument shape as the desktop app's manual "Start" button in the
/// Knowledge panel: this module doesn't attempt to resolve a stack's
/// `model_id_or_tag` to a model file automatically, matching that the GUI
/// doesn't either), waits for it to become healthy and verified, then
/// returns, leaving the process running in the background for subsequent
/// `monkey-cli stacks reindex`/`search_docs` calls to use.
pub async fn start(model_path: String) -> Result<(), String> {
    let verification_path = PathBuf::from(&model_path);
    tokio::task::spawn_blocking(move || {
        little_monkey_lib::model_sources::verify_managed_model_for_runtime(&verification_path)
    })
    .await
    .map_err(|error| format!("Managed model verification task failed: {error}"))??;

    let canonical_model = std::fs::canonicalize(&model_path)
        .map_err(|error| format!("Failed to resolve embedding model {model_path}: {error}"))?;
    let model_path = canonical_model.to_string_lossy().into_owned();
    let client = reqwest::Client::new();

    match read_process_record() {
        Ok(Some(record)) if process_record_matches(&record) => {
            let same_model = record.model_path == model_path;
            let ready =
                llama::server_reports_alias(&client, llama::EMBED_PORT, &record.alias).await;
            if same_model && ready {
                println!(
                    "Embedding server already running (pid {}, port {}).",
                    record.pid,
                    llama::EMBED_PORT
                );
                return Ok(());
            }
            // This is the exact process originally spawned by Little Monkey,
            // proven by its immutable start identity and nonce-bearing command
            // line. Restart it when the requested model changed or it is no
            // longer serving its recorded alias.
            stop_owned_process(&record)?;
            clear_process_record();
        }
        Ok(Some(_)) => {
            // PID was reused or the original process exited. Never signal the
            // process that owns it now.
            clear_process_record();
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("Ignoring stale embedding server state: {error}");
            clear_process_record();
        }
    }

    let binary = std::fs::canonicalize(llama::find_llama_server_binary()?)
        .map_err(|error| format!("Failed to resolve llama-server binary: {error}"))?;
    let binary_path = binary.to_string_lossy().into_owned();
    let startup_alias = llama::fresh_server_alias();
    let args = llama::embed_server_args(&model_path, &startup_alias);

    let mut child = std::process::Command::new(&binary)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn llama-server: {e}"))?;
    let pid = child.id();

    let health_url = format!("http://127.0.0.1:{}/health", llama::EMBED_PORT);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "Embedding llama-server exited before becoming ready ({status})"
                ))
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "Failed to inspect embedding llama-server process: {error}"
                ));
            }
        }

        if let Ok(resp) =
            egress::send(client.get(&health_url).timeout(Duration::from_secs(2))).await
        {
            if resp.status().is_success()
                && llama::server_reports_alias(&client, llama::EMBED_PORT, &startup_alias).await
            {
                // A different process can answer on this fixed port while
                // our child is still failing its bind. Recheck the child only
                // after the exact bounded alias response succeeds.
                match child.try_wait() {
                    Ok(None) => ready = true,
                    Ok(Some(status)) => {
                        return Err(format!(
                            "Embedding llama-server exited before becoming ready ({status})"
                        ))
                    }
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!(
                            "Failed to inspect embedding llama-server process: {error}"
                        ));
                    }
                }
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if !ready {
        let _ = child.kill();
        let _ = child.wait();
        return Err(
            "Timed out waiting for the embedding server to become healthy after 60s".to_string(),
        );
    }

    let verify = egress::send(
        client
            .post(format!(
                "http://127.0.0.1:{}/v1/embeddings",
                llama::EMBED_PORT
            ))
            .json(&serde_json::json!({ "model": &startup_alias, "input": ["ready check"] }))
            .timeout(Duration::from_secs(10)),
    )
    .await;
    if !matches!(verify, Ok(resp) if resp.status().is_success()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(
            "The embedding server process started but a test /v1/embeddings request failed — this build of \
             llama-server may not support --pooling mean, or may need a newer version."
                .to_string(),
        );
    }

    let Some(process_identity) = process_identity(pid) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "Could not capture a durable ownership identity for embedding server process {pid}"
        ));
    };
    let record = EmbedProcessRecord {
        schema_version: PROCESS_RECORD_SCHEMA_VERSION,
        pid,
        process_identity,
        alias: startup_alias,
        model_path,
        binary_path,
    };
    if let Err(error) = write_process_record(&record) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    println!(
        "Embedding server ready on port {} (pid {pid}).",
        llama::EMBED_PORT
    );
    // `child` is dropped here without `.kill()`/`.wait()` — deliberately: a
    // `std::process::Child` is not killed on drop, so the process keeps
    // running in the background after this CLI invocation exits.
    Ok(())
}

/// `monkey-cli stacks embed-server stop` — terminates the process only when
/// the durable record still proves that pid belongs to the exact server
/// Little Monkey launched, and clears stale state without signalling it.
pub fn stop() -> Result<(), String> {
    match read_process_record() {
        Ok(Some(record)) if process_record_matches(&record) => {
            stop_owned_process(&record)?;
            println!("Stopped embedding server (pid {}).", record.pid);
        }
        Ok(Some(_)) => {
            println!("No embedding server is running (stale ownership record cleared).")
        }
        Ok(None) => println!("No embedding server is running."),
        Err(error) => {
            println!("No embedding server is running ({error}; stale ownership record cleared).")
        }
    }
    clear_process_record();
    Ok(())
}

/// `monkey-cli stacks embed-server status` — reports running only while the
/// recorded immutable process identity still belongs to the recorded pid.
pub fn status() -> Result<(), String> {
    match read_process_record() {
        Ok(Some(record)) if process_record_matches(&record) => {
            println!(
                "Running (pid {}, port {}, model {}).",
                record.pid,
                llama::EMBED_PORT,
                record.model_path
            )
        }
        _ => println!("Not running."),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process_record() -> EmbedProcessRecord {
        let alias = "little-monkey-0123456789abcdef0123456789abcdef";
        let model_path = "/models/embed.gguf";
        let binary_path = "/runtime/llama-server";
        EmbedProcessRecord {
            schema_version: PROCESS_RECORD_SCHEMA_VERSION,
            pid: 4242,
            process_identity: format!(
                "Wed Jul 30 10:00:00 2026 {binary_path} -m {model_path} --alias {alias}"
            ),
            alias: alias.to_string(),
            model_path: model_path.to_string(),
            binary_path: binary_path.to_string(),
        }
    }

    #[test]
    fn process_record_round_trips_and_rejects_legacy_bare_pids() {
        let record = process_record();
        let encoded = serde_json::to_vec(&record).unwrap();
        assert_eq!(parse_process_record(&encoded).unwrap(), record);
        assert!(parse_process_record(b"4242").is_err());
    }

    #[test]
    fn process_record_requires_nonce_model_and_binary_in_identity() {
        let record = process_record();
        assert!(validate_process_record(record.clone()).is_ok());

        let mut wrong_binary = record;
        wrong_binary.binary_path = "/other/llama-server".to_string();
        assert!(validate_process_record(wrong_binary).is_err());
    }

    #[test]
    fn recycled_pid_identity_never_matches_the_ownership_record() {
        let record = process_record();
        assert!(process_record_matches_identity(
            &record,
            Some(record.process_identity.as_str())
        ));
        assert!(!process_record_matches_identity(
            &record,
            Some("Wed Jul 30 10:00:01 2026 /usr/bin/unrelated")
        ));
        assert!(!process_record_matches_identity(&record, None));
    }
}
