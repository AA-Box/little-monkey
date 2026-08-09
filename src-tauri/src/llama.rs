//! Lifecycle management for managed `llama-server` (llama.cpp) processes.
//!
//! Release builds bundle a pinned, checksum-verified `llama-server` runtime
//! and materialize it inside Little Monkey's app-data directory. Developer
//! builds still fall back to a host installation when no staged resource is
//! present. This module locates that binary, spawns it against a chosen GGUF
//! model, polls its `/health` endpoint until it is ready to serve requests,
//! and exposes Tauri commands so the frontend can start/stop it and read its
//! status.
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

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, State};

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
/// Layers the embeddings server offloads. Embedding models are a few hundred
/// megabytes, so all of them fit; llama.cpp clamps an overshoot to the layers
/// the model has, and a build with no GPU backend ignores the flag.
pub const EMBED_GPU_LAYERS: u32 = 999;
/// Upper bound for the startup identity response. A local process on the
/// fixed port is untrusted until it proves the exact alias we passed to the
/// child, so never buffer an arbitrarily large `/v1/models` body.
const MAX_MODELS_RESPONSE_BYTES: usize = 64 * 1024;

/// Generates a per-process identity that cannot be predicted from the model
/// path. Fixed ports may still have an orphan or unrelated server listening;
/// only the child launched for this exact attempt can know this nonce alias.
pub fn fresh_server_alias() -> String {
    format!("little-monkey-{}", uuid::Uuid::new_v4().simple())
}

fn chat_server_args(
    model_path: &str,
    port: u16,
    ctx_size: u32,
    gpu_layers: i32,
    embeddings: bool,
    alias: &str,
) -> Vec<String> {
    let mut args = vec![
        "-m".into(),
        model_path.to_string(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string(),
        "-c".into(),
        ctx_size.to_string(),
        "-ngl".into(),
        gpu_layers.to_string(),
        "--jinja".into(),
        "--alias".into(),
        alias.to_string(),
    ];
    if embeddings {
        args.push("--embeddings".into());
    }
    args
}

/// Floor/ceiling for the auto-detected chat context size: the floor matches
/// what every "Start" click used to hardcode, so a model whose GGUF metadata
/// is missing or unreadable is never worse off than before; the ceiling
/// stops a model with a huge (128K+) trained context from ballooning the
/// KV-cache RAM footprint by default when nobody asked for that.
const AUTO_CTX_SIZE_FLOOR: u32 = 4_096;
const AUTO_CTX_SIZE_CEILING: u32 = 32_768;

/// Picks the context size to launch the chat `llama-server` with. An
/// explicit `requested` value (e.g. a future manual override) always wins;
/// otherwise this reads the model's own `<arch>.context_length` GGUF
/// metadata (see `quantization::sniff_gguf_header`) and clamps it to
/// `[AUTO_CTX_SIZE_FLOOR, AUTO_CTX_SIZE_CEILING]`, falling back to the floor
/// when that metadata can't be read at all. Automatic, per-model, and never
/// requires the user to pick a number themselves.
fn resolve_ctx_size(requested: Option<u32>, model_path: &Path) -> u32 {
    if let Some(value) = requested {
        return value;
    }
    crate::quantization::sniff_gguf_file(model_path)
        .ok()
        .and_then(|header| header.context_length)
        .map(|trained| u32::try_from(trained).unwrap_or(u32::MAX))
        .map(|trained| trained.clamp(AUTO_CTX_SIZE_FLOOR, AUTO_CTX_SIZE_CEILING))
        .unwrap_or(AUTO_CTX_SIZE_FLOOR)
}

/// Builds the embeddings-only `llama-server` process's argument list for
/// `model_path` — factored out of [`embed_server_start`] so `monkey-cli`'s
/// `embed_cli::start` (RAG design doc slice 4 CLI parity: see that module's
/// doc comment for why the CLI needs its own process lifecycle rather than
/// reusing `embed_server_start` directly) launches the exact same flags
/// rather than a second, potentially-drifting copy of them.
pub fn embed_server_args(model_path: &str, alias: &str) -> Vec<String> {
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
        // Chat passes `-ngl` and embeddings did not, so on a Metal or CUDA
        // build the same binary ran the chat model on the GPU and the
        // embedding model on the CPU. Embedding models are small enough that
        // "all layers" fits wherever the flag does anything, and indexing a
        // corpus is the one place throughput is the whole experience.
        "-ngl".into(),
        EMBED_GPU_LAYERS.to_string(),
        "--embeddings".into(),
        "--pooling".into(),
        "mean".into(),
        "--alias".into(),
        alias.to_string(),
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

/// Locate `llama-server`: first the verified app-owned runtime shared by the
/// desktop and CLI, then PATH/common Homebrew locations as a developer
/// fallback. `pub` (not module-private) so
/// `monkey-cli`'s `embed_cli::start` (RAG design doc slice 4 CLI parity) can
/// resolve the same binary without re-implementing this search.
pub fn find_llama_server_binary() -> Result<String, String> {
    // K22: nothing native starts while the startup self-integrity check says a
    // component is tampered with — including the PATH fallback below, which
    // would otherwise be a way past a refused managed tree.
    crate::self_integrity::ensure_loadable()?;
    if let Some(app_data_dir) = crate::app_paths::data_dir() {
        let _ = crate::managed_runtime::materialize_bundled_runtime(None, &app_data_dir);
        if let Some(path) = crate::managed_runtime::find_managed_llama_server(Some(&app_data_dir)) {
            return Ok(path.to_string_lossy().into_owned());
        }
    }

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
        "Little Monkey's managed llama.cpp runtime is missing or failed verification. \
         Reinstall Little Monkey to restore the bundled runtime. Developers running from source \
         can run `pnpm stage:runtime` or put `llama-server` on PATH."
            .to_string(),
    )
}

/// Desktop-specific resolver that can use Tauri's authoritative resource
/// directory even when the current executable layout is non-standard (for
/// example an installer-managed Windows resource directory). Successful
/// materialization makes the same runtime available to the standalone CLI.
fn find_llama_server_binary_for_app(app: &AppHandle) -> Result<String, String> {
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Failed to resolve app data directory: {error}"))?;
    let resource_dir = app.path().resource_dir().ok();
    // A bundle that fails verification must not be the end of the search: an
    // already-published app-data tree, the `LITTLE_MONKEY_LLAMA_RUNTIME`
    // override and a system llama.cpp are all still valid answers, and a single
    // bad bundle used to mask every one of them (leaving no workaround at all).
    let bundle_error = match crate::managed_runtime::materialize_bundled_runtime(
        resource_dir.as_deref(),
        &app_data_dir,
    ) {
        Ok(Some(path)) => return Ok(path.to_string_lossy().into_owned()),
        Ok(None) => None,
        Err(error) => Some(error),
    };
    find_llama_server_binary().map_err(|fallback_error| match bundle_error {
        Some(bundle_error) => format!(
            "Little Monkey's bundled llama.cpp runtime failed verification: {bundle_error}. \
             {fallback_error}"
        ),
        None => fallback_error,
    })
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

/// Returns whether a bounded OpenAI-compatible models payload contains the
/// exact alias passed to the child. Substrings and malformed payloads never
/// establish process identity.
fn models_payload_reports_alias(bytes: &[u8], expected_alias: &str) -> bool {
    let payload: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(payload) => payload,
        Err(_) => return false,
    };
    payload["data"].as_array().is_some_and(|models| {
        models
            .iter()
            .any(|model| model["id"].as_str() == Some(expected_alias))
    })
}

/// Checks the fixed-port service's bounded `/v1/models` response for the
/// exact startup alias. This is public so the standalone embedding CLI can
/// apply the same identity boundary as the desktop process manager.
pub async fn server_reports_alias(
    client: &reqwest::Client,
    port: u16,
    expected_alias: &str,
) -> bool {
    let models_url = format!("http://127.0.0.1:{port}/v1/models");
    let mut response = match crate::egress::send(
        client.get(models_url).timeout(Duration::from_secs(2)),
    )
    .await
    {
        Ok(response) if response.status().is_success() => response,
        _ => return false,
    };
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MODELS_RESPONSE_BYTES as u64)
    {
        return false;
    }

    let mut bytes = Vec::new();
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(_) => return false,
        };
        if bytes.len().saturating_add(chunk.len()) > MAX_MODELS_RESPONSE_BYTES {
            return false;
        }
        bytes.extend_from_slice(&chunk);
    }
    models_payload_reports_alias(&bytes, expected_alias)
}

/// Reports a startup failure when the child exited or disappeared from the
/// managed state. This check deliberately runs both before probing the port
/// and again after HTTP identity succeeds: a different service can answer on
/// a fixed port while our child is still in the process of failing its bind.
fn spawned_child_failure(state: &std::sync::Mutex<LlamaState>) -> Result<Option<String>, String> {
    let mut guard = state.lock().map_err(|error| error.to_string())?;
    let Some(child) = guard.process.as_mut() else {
        return Ok(Some(
            "Managed llama-server child disappeared during startup".to_string(),
        ));
    };
    match child.try_wait() {
        Ok(Some(exit_status)) => Ok(Some(format!(
            "llama-server exited unexpectedly before becoming ready (status: {exit_status})"
        ))),
        Ok(None) => Ok(None),
        Err(error) => Ok(Some(format!(
            "Failed to check llama-server process status: {error}"
        ))),
    }
}

/// Shared spawn + health-poll body for a managed `llama-server` instance —
/// used by both [`llama_start`] (chat instance) and [`embed_server_start`]
/// (embeddings instance). Kills any previous process already held in `state`,
/// spawns `binary` with `args`, then polls `GET /health` and bounded
/// `GET /v1/models` identity on `port` for up to 60s. Readiness requires the
/// exact alias passed to the spawned child and a final child-liveness recheck;
/// this prevents an unrelated service on the fixed port from being accepted.
/// On success `state.status == "ready"`; on failure or timeout the process is
/// killed and `state.status == "error"`.
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
    expected_alias: &str,
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
        if let Some(error) = spawned_child_failure(state)? {
            failure = Some(error);
            break;
        }

        if let Ok(resp) = crate::egress::send(
            client.get(&health_url).timeout(Duration::from_secs(2)),
        )
        .await
        {
            if resp.status().is_success()
                && server_reports_alias(&client, port, expected_alias).await
            {
                // Identity alone is not enough: a pre-existing service could
                // answer while our just-spawned process is about to lose the
                // port bind. Prove the child still exists after the response.
                if let Some(error) = spawned_child_failure(state)? {
                    failure = Some(error);
                } else {
                    ready = true;
                }
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
/// to report healthy (or fail/time out). `ctx_size` is optional — omit it
/// (as every normal "Start" click does) to auto-size the context window from
/// the model's own GGUF metadata via [`resolve_ctx_size`] instead of one
/// fixed number for every model. Returns the context size it actually
/// launched with, so the caller can reflect the real limit (not a guess) in
/// its own UI.
#[tauri::command]
pub async fn llama_start(
    app: AppHandle,
    state: State<'_, AppState>,
    model_path: String,
    ctx_size: Option<u32>,
    gpu_layers: i32,
    embeddings: bool,
) -> Result<u32, String> {
    let verification_path = PathBuf::from(&model_path);
    tokio::task::spawn_blocking(move || {
        crate::model_sources::verify_managed_model_for_runtime(&verification_path)
    })
    .await
    .map_err(|error| format!("Managed model verification task failed: {error}"))??;

    let binary = find_llama_server_binary_for_app(&app)?;
    let port = state.llama.lock().map_err(|e| e.to_string())?.port;

    {
        let mut guard = state.llama.lock().map_err(|e| e.to_string())?;
        guard.embeddings_enabled = embeddings;
    }

    let resolved_ctx_size = resolve_ctx_size(ctx_size, Path::new(&model_path));
    let startup_alias = fresh_server_alias();
    let args = chat_server_args(
        &model_path,
        port,
        resolved_ctx_size,
        gpu_layers,
        embeddings,
        &startup_alias,
    );

    spawn_and_wait_healthy(
        &app,
        &state.llama,
        "llama://status",
        &binary,
        &args,
        port,
        &model_path,
        &startup_alias,
    )
    .await?;

    Ok(resolved_ctx_size)
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
    let verification_path = PathBuf::from(&model_path);
    tokio::task::spawn_blocking(move || {
        crate::model_sources::verify_managed_model_for_runtime(&verification_path)
    })
    .await
    .map_err(|error| format!("Managed model verification task failed: {error}"))??;

    let binary = find_llama_server_binary_for_app(&app)?;
    let startup_alias = fresh_server_alias();
    let args = embed_server_args(&model_path, &startup_alias);

    spawn_and_wait_healthy(
        &app,
        &state.embed_llama,
        "embed://status",
        &binary,
        &args,
        EMBED_PORT,
        &model_path,
        &startup_alias,
    )
    .await?;

    let client = reqwest::Client::new();
    let verify = crate::egress::send(
        client
            .post(format!("http://127.0.0.1:{EMBED_PORT}/v1/embeddings"))
            .json(&json!({ "model": startup_alias, "input": ["ready check"] }))
            .timeout(Duration::from_secs(10)),
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_identity_requires_the_exact_model_alias() {
        let payload = br#"{
            "object": "list",
            "data": [
                {"id": "/models/verified.gguf", "object": "model"}
            ]
        }"#;
        assert!(models_payload_reports_alias(
            payload,
            "/models/verified.gguf"
        ));
        assert!(!models_payload_reports_alias(payload, "/models/verified"));
        assert!(!models_payload_reports_alias(payload, "verified.gguf"));
        assert!(!models_payload_reports_alias(
            b"not-json",
            "/models/verified.gguf"
        ));
        assert!(!models_payload_reports_alias(
            br#"{"data":{"id":"/models/verified.gguf"}}"#,
            "/models/verified.gguf"
        ));
    }

    #[test]
    fn desktop_server_args_bind_loopback_and_set_identity_alias() {
        assert_eq!(
            chat_server_args(
                "/models/chat.gguf",
                8090,
                4096,
                99,
                true,
                "little-monkey-chat-nonce",
            ),
            [
                "-m",
                "/models/chat.gguf",
                "--host",
                "127.0.0.1",
                "--port",
                "8090",
                "-c",
                "4096",
                "-ngl",
                "99",
                "--jinja",
                "--alias",
                "little-monkey-chat-nonce",
                "--embeddings",
            ]
        );
        assert_eq!(
            embed_server_args("/models/embed.gguf", "little-monkey-embed-nonce"),
            [
                "-m",
                "/models/embed.gguf",
                "--host",
                "127.0.0.1",
                "--port",
                "8091",
                "-c",
                "2048",
                "-ub",
                "2048",
                // Embeddings offload like chat does. The CLI builds its args
                // from this same function, so both get the GPU or neither does.
                "-ngl",
                "999",
                "--embeddings",
                "--pooling",
                "mean",
                "--alias",
                "little-monkey-embed-nonce",
            ]
        );
    }

    #[test]
    fn startup_alias_is_unpredictable_and_path_independent() {
        let first = fresh_server_alias();
        let second = fresh_server_alias();
        assert!(first.starts_with("little-monkey-"));
        assert!(second.starts_with("little-monkey-"));
        assert_ne!(first, second);
        assert!(!first.contains('/') && !first.contains('\\'));
    }

    #[test]
    fn resolve_ctx_size_prefers_an_explicit_value_over_auto_detection() {
        assert_eq!(
            resolve_ctx_size(Some(8_192), Path::new("/does/not/exist.gguf")),
            8_192
        );
    }

    #[test]
    fn resolve_ctx_size_falls_back_to_the_floor_when_the_model_cant_be_read() {
        assert_eq!(
            resolve_ctx_size(None, Path::new("/does/not/exist.gguf")),
            AUTO_CTX_SIZE_FLOOR
        );
    }

    /// Writes a minimal, real GGUF v3 file with only `general.architecture`
    /// and `<architecture>.context_length` metadata — just enough for
    /// `quantization::sniff_gguf_file` to parse, mirroring
    /// `quantization::tests::build_minimal_gguf_full` without depending on
    /// that module's private test helpers.
    fn write_minimal_gguf_with_context_length(path: &std::path::Path, architecture: &str, context_length: u32) {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(b"GGUF");
        buffer.extend_from_slice(&3_u32.to_le_bytes()); // version
        buffer.extend_from_slice(&0_u64.to_le_bytes()); // tensor_count
        buffer.extend_from_slice(&2_u64.to_le_bytes()); // metadata_kv_count

        let write_string = |buffer: &mut Vec<u8>, value: &str| {
            buffer.extend_from_slice(&(value.len() as u64).to_le_bytes());
            buffer.extend_from_slice(value.as_bytes());
        };

        write_string(&mut buffer, "general.architecture");
        buffer.extend_from_slice(&8_u32.to_le_bytes()); // GGUF_TYPE_STRING
        write_string(&mut buffer, architecture);

        write_string(&mut buffer, &format!("{architecture}.context_length"));
        buffer.extend_from_slice(&4_u32.to_le_bytes()); // GGUF_TYPE_UINT32
        buffer.extend_from_slice(&context_length.to_le_bytes());

        std::fs::write(path, buffer).expect("write fixture GGUF");
    }

    #[test]
    fn resolve_ctx_size_auto_detects_from_the_models_trained_context() {
        let dir = std::env::temp_dir().join(format!("llama-rs-ctx-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("model.gguf");
        write_minimal_gguf_with_context_length(&path, "qwen2", 8_192);

        assert_eq!(resolve_ctx_size(None, &path), 8_192);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_ctx_size_clamps_a_huge_trained_context_to_the_ceiling() {
        let dir = std::env::temp_dir().join(format!("llama-rs-ctx-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("model.gguf");
        write_minimal_gguf_with_context_length(&path, "llama", 1_048_576);

        assert_eq!(resolve_ctx_size(None, &path), AUTO_CTX_SIZE_CEILING);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_ctx_size_raises_a_tiny_trained_context_to_the_floor() {
        let dir = std::env::temp_dir().join(format!("llama-rs-ctx-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("model.gguf");
        write_minimal_gguf_with_context_length(&path, "gpt2", 1_024);

        assert_eq!(resolve_ctx_size(None, &path), AUTO_CTX_SIZE_FLOOR);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
