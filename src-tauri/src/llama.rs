//! Lifecycle management for managed `llama-server` (llama.cpp) processes.
//!
//! Little Monkey does not bundle or auto-download the `llama-server` binary — it must
//! already be installed on the host (e.g. via `brew install llama.cpp`). This
//! module locates that binary, spawns it against a chosen GGUF model, polls
//! its `/health` endpoint until it is ready to serve requests, and exposes
//! Tauri commands so the frontend can start/stop it and read its status.
//!
//! Two independent instances share the same spawn/health-poll/kill core
//! ([`spawn_and_wait_healthy`]): the chat instance (`AppState::llama`, port
//! [`CHAT_PORT`], started via [`llama_start`]) and the embeddings-only
//! instance (`AppState::embed_llama`, port [`EMBED_PORT`], started via
//! [`embed_server_start`] with `--embeddings --pooling mean`) used by
//! `stacks.rs`'s local-embedding backend. They are deliberately separate
//! `LlamaState`s (not one process serving both roles) so a stack reindex
//! never has to fight the chat model for the same server slot, and so
//! stopping/restarting one never interrupts the other.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;
use tauri::{AppHandle, Emitter, State};

use crate::AppState;

/// Port the managed chat `llama-server` instance listens on.
pub(crate) const CHAT_PORT: u16 = 8090;
/// Port the managed embeddings-only `llama-server` instance listens on —
/// see `stacks.rs`'s `embed_via_llama`, which talks to this port directly.
/// `pub` (not `pub(crate)`) so `monkey-cli`'s `embed_cli.rs` (RAG design doc slice
/// 4, CLI parity for the llama embedding backend) can poll/target the same
/// port from outside this crate.
pub const EMBED_PORT: u16 = 8091;
/// Context size (and `-ub` ubatch size) the embeddings instance is started
/// with. Not user-configurable in slice 1 — 2048 tokens comfortably covers
/// `KnowledgeStack::chunk_chars` (1600 chars is well under 2048 tokens). `pub`
/// so `monkey-cli`'s `embed_cli::start` can build the exact same args via
/// [`embed_server_args`].
pub const EMBED_CTX: u32 = 2048;

/// Builds the embeddings-only `llama-server` process's argument list for
/// `model_path` — factored out of [`embed_server_start`] so `monkey-cli`'s
/// `embed_cli::start` (RAG design doc slice 4 CLI parity: see that module's
/// doc comment for why the CLI needs its own process lifecycle rather than
/// reusing `embed_server_start` directly) launches the exact same flags
/// rather than a second, potentially-drifting copy of them.
pub fn embed_server_args(model_path: &str) -> Vec<String> {
    vec![
        "-m".into(),
        model_path.to_string(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        EMBED_PORT.to_string(),
        "-c".into(),
        EMBED_CTX.to_string(),
        "-ub".into(),
        EMBED_CTX.to_string(),
        "--embeddings".into(),
        "--pooling".into(),
        "mean".into(),
    ]
}

/// In-memory state for a managed `llama-server` child process.
pub struct LlamaState {
    pub process: Option<std::process::Child>,
    pub port: u16,
    pub model_path: Option<String>,
    pub status: String,
    /// Whether the currently-running (or most recently started) process was
    /// launched with `--embeddings`. The local API server (`server.rs`,
    /// phase 3) reads this to decide whether `POST /v1/embeddings` can
    /// actually route to llama-server — routing there without this flag set
    /// would just surface llama-server's own "embeddings not enabled" error,
    /// so `server.rs` returns a clearer `501` up front instead.
    pub embeddings_enabled: bool,
}

impl Default for LlamaState {
    fn default() -> Self {
        LlamaState {
            process: None,
            port: CHAT_PORT,
            model_path: None,
            status: "stopped".to_string(),
            embeddings_enabled: false,
        }
    }
}

impl LlamaState {
    /// Constructs the initial state for the embeddings-only instance —
    /// identical to `Default::default()` except for the port, so
    /// `AppState`'s own `Default` impl (see `lib.rs`) can give
    /// `embed_llama` a distinct starting port from the chat instance's.
    /// `pub` so `monkey-cli`'s `embed_cli` module can build its own throwaway
    /// `LlamaState` for the one-off spawn it performs per CLI invocation.
    pub fn for_embeddings() -> Self {
        LlamaState {
            port: EMBED_PORT,
            ..Self::default()
        }
    }
}

/// Locate the `llama-server` binary: first on PATH (via `which`), then in the
/// common Homebrew install locations. `pub` (not module-private) so
/// `monkey-cli`'s `embed_cli::start` (RAG design doc slice 4 CLI parity) can
/// resolve the same binary without re-implementing this search.
pub fn find_llama_server_binary() -> Result<String, String> {
    if let Ok(output) = Command::new("which").arg("llama-server").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() && Path::new(&path).exists() {
                return Ok(path);
            }
        }
    }

    for candidate in [
        "/opt/homebrew/bin/llama-server",
        "/usr/local/bin/llama-server",
    ] {
        if Path::new(candidate).exists() {
            return Ok(candidate.to_string());
        }
    }

    Err(
        "Could not find the `llama-server` binary on your PATH or in common install locations.\n\n\
         Install llama.cpp to get it:\n\n  brew install llama.cpp\n\n\
         (On Linux, install via your package manager or build llama.cpp from source: \
         https://github.com/ggerganov/llama.cpp). Once installed, make sure `llama-server` \
         is on your PATH and try again."
            .to_string(),
    )
}

/// Emit a status event (`llama://status` or `embed://status`) to all windows
/// with the current status snapshot.
fn emit_status(
    app: &AppHandle,
    event_name: &str,
    status: &str,
    port: u16,
    model_path: &Option<String>,
) {
    let _ = app.emit(
        event_name,
        json!({
            "status": status,
            "port": port,
            "model_path": model_path,
        }),
    );
}

/// Shared spawn + health-poll body for a managed `llama-server` instance —
/// used by both [`llama_start`] (chat instance) and [`embed_server_start`]
/// (embeddings instance). Kills any previous process already held in
/// `state`, spawns `binary` with `args`, then polls `GET /health` on `port`
/// for up to 60s, updating `state`'s status and emitting `event_name` status
/// events along the way. On success `state.status == "ready"`; on failure or
/// timeout the process is killed and `state.status == "error"`.
///
/// Does NOT perform any embeddings-specific verification — `/health` only
/// proves the process is alive, not that `/v1/embeddings` actually works
/// (see the RAG design doc's risk note on llama.cpp regressions there).
/// [`embed_server_start`] does that extra check itself after this returns.
async fn spawn_and_wait_healthy(
    app: &AppHandle,
    state: &std::sync::Mutex<LlamaState>,
    event_name: &str,
    binary: &str,
    args: &[String],
    port: u16,
    model_path: &str,
) -> Result<(), String> {
    {
        let mut guard = state.lock().map_err(|e| e.to_string())?;
        if let Some(mut child) = guard.process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        guard.status = "starting".to_string();
        guard.model_path = Some(model_path.to_string());
    }
    emit_status(
        app,
        event_name,
        "starting",
        port,
        &Some(model_path.to_string()),
    );

    let mut command = Command::new(binary);
    command.args(args);
    let spawn_result = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    let child = match spawn_result {
        Ok(child) => child,
        Err(e) => {
            let mut guard = state.lock().map_err(|e| e.to_string())?;
            guard.status = "error".to_string();
            drop(guard);
            emit_status(
                app,
                event_name,
                "error",
                port,
                &Some(model_path.to_string()),
            );
            return Err(format!("Failed to spawn llama-server: {e}"));
        }
    };

    {
        let mut guard = state.lock().map_err(|e| e.to_string())?;
        guard.process = Some(child);
    }

    // Poll the health endpoint until it responds successfully, the process
    // exits early, or we hit the 60s timeout.
    let client = reqwest::Client::new();
    let health_url = format!("http://127.0.0.1:{port}/health");
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut ready = false;
    let mut failure: Option<String> = None;

    while Instant::now() < deadline {
        {
            let mut guard = state.lock().map_err(|e| e.to_string())?;
            if let Some(child) = guard.process.as_mut() {
                match child.try_wait() {
                    Ok(Some(exit_status)) => {
                        failure = Some(format!(
                            "llama-server exited unexpectedly before becoming ready (status: {exit_status})"
                        ));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        failure = Some(format!("Failed to check llama-server process status: {e}"));
                    }
                }
            }
        }

        if failure.is_some() {
            break;
        }

        if let Ok(resp) = client
            .get(&health_url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
        {
            if resp.status().is_success() {
                ready = true;
                break;
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    if ready {
        let mut guard = state.lock().map_err(|e| e.to_string())?;
        guard.status = "ready".to_string();
        drop(guard);
        emit_status(
            app,
            event_name,
            "ready",
            port,
            &Some(model_path.to_string()),
        );
        Ok(())
    } else {
        let error_message = failure.unwrap_or_else(|| {
            "Timed out waiting for llama-server to become healthy after 60s".to_string()
        });

        {
            let mut guard = state.lock().map_err(|e| e.to_string())?;
            if let Some(mut child) = guard.process.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            guard.status = "error".to_string();
        }

        emit_status(
            app,
            event_name,
            "error",
            port,
            &Some(model_path.to_string()),
        );
        Err(error_message)
    }
}

/// Start the chat `llama-server` process for the given model, waiting for it
/// to report healthy (or fail/time out).
#[tauri::command]
pub async fn llama_start(
    app: AppHandle,
    state: State<'_, AppState>,
    model_path: String,
    ctx_size: u32,
    gpu_layers: i32,
    embeddings: bool,
) -> Result<(), String> {
    let binary = find_llama_server_binary()?;
    let port = state.llama.lock().map_err(|e| e.to_string())?.port;

    {
        let mut guard = state.llama.lock().map_err(|e| e.to_string())?;
        guard.embeddings_enabled = embeddings;
    }

    let mut args: Vec<String> = vec![
        "-m".into(),
        model_path.clone(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string(),
        "-c".into(),
        ctx_size.to_string(),
        "-ngl".into(),
        gpu_layers.to_string(),
        "--jinja".into(),
    ];
    if embeddings {
        args.push("--embeddings".into());
    }

    spawn_and_wait_healthy(
        &app,
        &state.llama,
        "llama://status",
        &binary,
        &args,
        port,
        &model_path,
    )
    .await
}

/// Kill the managed chat `llama-server` process, if any, and mark it stopped.
#[tauri::command]
pub async fn llama_stop(state: State<'_, AppState>) -> Result<(), String> {
    let mut llama = state.llama.lock().map_err(|e| e.to_string())?;
    if let Some(mut child) = llama.process.take() {
        child
            .kill()
            .map_err(|e| format!("Failed to kill llama-server process: {e}"))?;
        let _ = child.wait();
    }
    llama.status = "stopped".to_string();
    llama.embeddings_enabled = false;
    Ok(())
}

/// Return the current status snapshot: `{status, port, model_path, embeddings_enabled}`.
#[tauri::command]
pub fn llama_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let llama = state.llama.lock().map_err(|e| e.to_string())?;
    Ok(json!({
        "status": llama.status,
        "port": llama.port,
        "model_path": llama.model_path,
        "embeddings_enabled": llama.embeddings_enabled,
    }))
}

/// Start the embeddings-only `llama-server` instance (port [`EMBED_PORT`])
/// against `model_path`, for `stacks.rs`'s managed-llama embedding backend.
/// Launched with `--embeddings --pooling mean` — per current llama.cpp
/// server docs, the OpenAI-compatible `/v1/embeddings` endpoint requires a
/// pooling mode other than `none` (verified against
/// `tools/server/README.md` at implementation time). After the shared
/// spawn/health-poll succeeds, performs one real `POST /v1/embeddings` call
/// to verify the endpoint actually works before declaring the instance
/// ready — a `/health` 200 alone doesn't prove that (see
/// `spawn_and_wait_healthy`'s doc comment).
#[tauri::command]
pub async fn embed_server_start(
    app: AppHandle,
    state: State<'_, AppState>,
    model_path: String,
) -> Result<(), String> {
    let binary = find_llama_server_binary()?;
    let args = embed_server_args(&model_path);

    spawn_and_wait_healthy(
        &app,
        &state.embed_llama,
        "embed://status",
        &binary,
        &args,
        EMBED_PORT,
        &model_path,
    )
    .await?;

    let client = reqwest::Client::new();
    let verify = client
        .post(format!("http://127.0.0.1:{EMBED_PORT}/v1/embeddings"))
        .json(&json!({ "model": model_path, "input": ["ready check"] }))
        .timeout(Duration::from_secs(10))
        .send()
        .await;

    let verified = matches!(verify, Ok(resp) if resp.status().is_success());
    if !verified {
        let mut guard = state.embed_llama.lock().map_err(|e| e.to_string())?;
        if let Some(mut child) = guard.process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        guard.status = "error".to_string();
        drop(guard);
        emit_status(
            &app,
            "embed://status",
            "error",
            EMBED_PORT,
            &Some(model_path),
        );
        return Err(
            "The embedding server process started but a test /v1/embeddings request failed — this build of \
             llama-server may not support --pooling mean, or may need a newer version."
                .to_string(),
        );
    }

    Ok(())
}

/// Kill the managed embeddings-only `llama-server` process, if any, and mark
/// it stopped.
#[tauri::command]
pub async fn embed_server_stop(state: State<'_, AppState>) -> Result<(), String> {
    let mut embed = state.embed_llama.lock().map_err(|e| e.to_string())?;
    if let Some(mut child) = embed.process.take() {
        child
            .kill()
            .map_err(|e| format!("Failed to kill embedding server process: {e}"))?;
        let _ = child.wait();
    }
    embed.status = "stopped".to_string();
    Ok(())
}

/// Return the embeddings instance's current status snapshot: `{status, port, model_path}`.
#[tauri::command]
pub fn embed_server_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let embed = state.embed_llama.lock().map_err(|e| e.to_string())?;
    Ok(json!({
        "status": embed.status,
        "port": embed.port,
        "model_path": embed.model_path,
    }))
}

/// Kills both managed `llama-server` child processes (chat + embeddings), if
/// running, and marks each stopped — used by `lib.rs`'s `RunEvent::Exit`
/// handler so quitting the app never orphans either one (see that handler's
/// doc comment for why `RunEvent::Exit` is the only chance to do this at
/// all). Synchronous and best-effort, mirroring `llama_stop`'s/
/// `embed_server_stop`'s bodies exactly, but callable without an
/// `AppHandle`/async runtime: `RunEvent::Exit` fires synchronously right
/// before the process exits, and a plain `std::process::Child::kill` needs
/// neither.
pub fn stop_all_blocking(state: &AppState) {
    if let Ok(mut guard) = state.llama.lock() {
        if let Some(mut child) = guard.process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        guard.status = "stopped".to_string();
    }
    if let Ok(mut guard) = state.embed_llama.lock() {
        if let Some(mut child) = guard.process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        guard.status = "stopped".to_string();
    }
}
