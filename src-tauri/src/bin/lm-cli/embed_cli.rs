//! CLI-side lifecycle for the embeddings-only `llama-server` instance (RAG
//! design doc slice 4, CLI parity for the `llama` embedding backend).
//!
//! Before this module existed, nothing in `lm-cli` could ever start this
//! process at all: `stacks::embed_via_llama` (`stacks.rs`) just POSTs to
//! `http://127.0.0.1:{EMBED_PORT}/v1/embeddings` with no fallback, so any
//! `llama`-backend stack (the curated/default backend — see
//! `KnowledgePanel.tsx`) failed outright from a terminal unless the desktop
//! app happened to already have its embeddings server running. The desktop
//! app's equivalent (`llama::embed_server_start`) keeps the spawned
//! `std::process::Child` inside its own long-running `AppState`, so a later
//! Tauri command can find and kill it — `lm-cli` has no such thing: every
//! invocation is its own short-lived process, so there is nothing for a
//! later `lm-cli` invocation to hold onto in memory. Lifecycle here is
//! therefore pid-file based instead: `start` spawns `llama-server`, waits for
//! it to become healthy (reusing `llama::find_llama_server_binary`/
//! `llama::embed_server_args` so the actual binary/flags never drift from the
//! desktop app's), verifies `/v1/embeddings` actually works (mirroring
//! `llama::embed_server_start`'s own verification step), writes its pid to
//! `<stacks-dir>/embed-server.pid`, and returns while deliberately leaving
//! the process running in the background (a plain `std::process::Child` is
//! NOT killed when its handle is dropped, unlike `tokio::process::Child` with
//! `kill_on_drop` — so simply never calling `.kill()`/`.wait()` here is
//! enough to detach it). `status`/`stop` read that pid file back and use
//! `kill -0`/`kill <pid>` (POSIX-only, matching this project's macOS/Linux-
//! only support — see `find_llama_server_binary`'s Homebrew-only fallback
//! paths) to check liveness / terminate it.

use std::path::PathBuf;
use std::time::Duration;

use little_monkey_lib::llama;

fn pid_file_path() -> Option<PathBuf> {
    crate::stacks_cli::base_dir().map(|d| d.join("embed-server.pid"))
}

fn read_pid() -> Option<u32> {
    let path = pid_file_path()?;
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn write_pid(pid: u32) -> Result<(), String> {
    let path = pid_file_path().ok_or("Could not resolve the app data directory")?;
    std::fs::write(&path, pid.to_string()).map_err(|e| format!("Failed to write {}: {e}", path.display()))
}

fn clear_pid_file() {
    if let Some(path) = pid_file_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// `kill -0 <pid>` sends no signal — it just checks whether a process with
/// that pid exists and is ours to signal — so this never disturbs the
/// process it's checking.
fn process_is_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// `lm-cli stacks embed-server start --model-path <path>` — spawns the
/// embeddings-only `llama-server` instance against `model_path` (the same
/// argument shape as the desktop app's manual "Start" button in the
/// Knowledge panel: this module doesn't attempt to resolve a stack's
/// `model_id_or_tag` to a model file automatically, matching that the GUI
/// doesn't either), waits for it to become healthy and verified, then
/// returns, leaving the process running in the background for subsequent
/// `lm-cli stacks reindex`/`search_docs` calls to use.
pub async fn start(model_path: String) -> Result<(), String> {
    if let Some(pid) = read_pid() {
        if process_is_alive(pid) {
            println!("Embedding server already running (pid {pid}, port {}).", llama::EMBED_PORT);
            return Ok(());
        }
        // Stale pid file (process died without `stop` being called) — clear
        // it before starting a fresh one.
        clear_pid_file();
    }

    let binary = llama::find_llama_server_binary()?;
    let args = llama::embed_server_args(&model_path);

    let child = std::process::Command::new(&binary)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn llama-server: {e}"))?;
    let pid = child.id();

    let client = reqwest::Client::new();
    let health_url = format!("http://127.0.0.1:{}/health", llama::EMBED_PORT);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        if let Ok(resp) = client.get(&health_url).timeout(Duration::from_secs(2)).send().await {
            if resp.status().is_success() {
                ready = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if !ready {
        let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
        return Err("Timed out waiting for the embedding server to become healthy after 60s".to_string());
    }

    let verify = client
        .post(format!("http://127.0.0.1:{}/v1/embeddings", llama::EMBED_PORT))
        .json(&serde_json::json!({ "model": model_path, "input": ["ready check"] }))
        .timeout(Duration::from_secs(10))
        .send()
        .await;
    if !matches!(verify, Ok(resp) if resp.status().is_success()) {
        let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
        return Err(
            "The embedding server process started but a test /v1/embeddings request failed — this build of \
             llama-server may not support --pooling mean, or may need a newer version."
                .to_string(),
        );
    }

    write_pid(pid)?;
    println!("Embedding server ready on port {} (pid {pid}).", llama::EMBED_PORT);
    // `child` is dropped here without `.kill()`/`.wait()` — deliberately: a
    // `std::process::Child` is not killed on drop, so the process keeps
    // running in the background after this CLI invocation exits.
    Ok(())
}

/// `lm-cli stacks embed-server stop` — kills the process recorded in the pid
/// file, if it's still alive, and clears the pid file either way.
pub fn stop() -> Result<(), String> {
    match read_pid() {
        Some(pid) if process_is_alive(pid) => {
            std::process::Command::new("kill")
                .arg(pid.to_string())
                .status()
                .map_err(|e| format!("Failed to stop process {pid}: {e}"))?;
            println!("Stopped embedding server (pid {pid}).");
        }
        Some(_) => println!("No embedding server is running (stale pid file cleared)."),
        None => println!("No embedding server is running."),
    }
    clear_pid_file();
    Ok(())
}

/// `lm-cli stacks embed-server status` — prints whether the pid file's
/// process is actually still alive (a stale pid file left behind by a
/// process that died without `stop` being called reports "not running").
pub fn status() -> Result<(), String> {
    match read_pid() {
        Some(pid) if process_is_alive(pid) => println!("Running (pid {pid}, port {}).", llama::EMBED_PORT),
        _ => println!("Not running."),
    }
    Ok(())
}
