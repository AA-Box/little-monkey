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

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::profiles::ProfileScopedPaths;
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
const MAX_STDERR_TAIL: usize = 16 * 1024;

/// Generates a per-process identity that cannot be predicted from the model
/// path. Fixed ports may still have an orphan or unrelated server listening;
/// only the child launched for this exact attempt can know this nonce alias.
pub fn fresh_server_alias() -> String {
    format!("little-monkey-{}", uuid::Uuid::new_v4().simple())
}

fn chat_server_args(
    model_path: &str,
    projector_path: Option<&str>,
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
    if let Some(projector_path) = projector_path {
        args.splice(2..2, ["--mmproj".into(), projector_path.to_string()]);
    }
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
    stderr_tail: Option<Arc<Mutex<String>>>,
    pub port: u16,
    pub model_path: Option<String>,
    pub projector_path: Option<String>,
    pub vision_enabled: bool,
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
            stderr_tail: None,
            port: CHAT_PORT,
            model_path: None,
            projector_path: None,
            vision_enabled: false,
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
        .profile_data_dir()
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
    projector_path: &Option<String>,
    vision_enabled: bool,
) {
    let _ = app.emit(
        event_name,
        json!({
            "status": status,
            "port": port,
            "model_path": model_path,
            "projector_path": projector_path,
            "vision_enabled": vision_enabled,
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
    let mut response =
        match crate::egress::send(client.get(models_url).timeout(Duration::from_secs(2))).await {
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
fn stderr_detail(state: &Mutex<LlamaState>) -> Option<String> {
    let tail = state.lock().ok()?.stderr_tail.clone()?;
    let detail = tail.lock().ok()?.trim().to_string();
    (!detail.is_empty()).then_some(detail)
}

fn spawned_child_failure(state: &Mutex<LlamaState>) -> Result<Option<String>, String> {
    let outcome = {
        let mut guard = state.lock().map_err(|error| error.to_string())?;
        let Some(child) = guard.process.as_mut() else {
            return Ok(Some(
                "Managed llama-server child disappeared during startup".to_string(),
            ));
        };
        match child.try_wait() {
            Ok(Some(exit_status)) => Some(format!(
                "llama-server exited unexpectedly before becoming ready (status: {exit_status})"
            )),
            Ok(None) => None,
            Err(error) => Some(format!(
                "Failed to check llama-server process status: {error}"
            )),
        }
    };
    Ok(outcome.map(|message| match stderr_detail(state) {
        Some(detail) => format!("{message}:\n{detail}"),
        None => message,
    }))
}

fn drain_llama_stderr(stream: impl Read + Send + 'static, tail: Arc<Mutex<String>>) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let Ok(mut buffer) = tail.lock() else {
                        break;
                    };
                    buffer.push_str(&line);
                    if buffer.len() > MAX_STDERR_TAIL {
                        let (capped, _) = crate::output_cap::cap_tail(
                            std::mem::take(&mut *buffer),
                            MAX_STDERR_TAIL,
                        );
                        *buffer = capped;
                    }
                }
                Err(_) => break,
            }
        }
    });
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
    projector_path: Option<&str>,
    expected_alias: &str,
) -> Result<(), String> {
    {
        let mut guard = state.lock().map_err(|e| e.to_string())?;
        if let Some(mut child) = guard.process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        guard.status = "starting".to_string();
        guard.stderr_tail = None;
        guard.model_path = Some(model_path.to_string());
        guard.projector_path = projector_path.map(str::to_string);
        guard.vision_enabled = false;
    }
    emit_status(
        app,
        event_name,
        "starting",
        port,
        &Some(model_path.to_string()),
        &projector_path.map(str::to_string),
        false,
    );

    let mut command = Command::new(binary);
    command.args(args);
    let spawn_result = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match spawn_result {
        Ok(child) => child,
        Err(e) => {
            let mut guard = state.lock().map_err(|e| e.to_string())?;
            guard.status = "error".to_string();
            guard.projector_path = None;
            guard.vision_enabled = false;
            drop(guard);
            emit_status(
                app,
                event_name,
                "error",
                port,
                &Some(model_path.to_string()),
                &None,
                false,
            );
            return Err(format!("Failed to spawn llama-server: {e}"));
        }
    };

    {
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        if let Some(stderr) = child.stderr.take() {
            drain_llama_stderr(stderr, Arc::clone(&stderr_tail));
        }
        let mut guard = state.lock().map_err(|e| e.to_string())?;
        guard.process = Some(child);
        guard.stderr_tail = Some(stderr_tail);
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

        if let Ok(resp) =
            crate::egress::send(client.get(&health_url).timeout(Duration::from_secs(2))).await
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
        guard.vision_enabled = projector_path.is_some();
        drop(guard);
        emit_status(
            app,
            event_name,
            "ready",
            port,
            &Some(model_path.to_string()),
            &projector_path.map(str::to_string),
            projector_path.is_some(),
        );
        Ok(())
    } else {
        let headline = failure.unwrap_or_else(|| {
            "Timed out waiting for llama-server to become healthy after 60s".to_string()
        });

        {
            let mut guard = state.lock().map_err(|e| e.to_string())?;
            if let Some(mut child) = guard.process.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            guard.status = "error".to_string();
            guard.projector_path = None;
            guard.vision_enabled = false;
        }
        let error_message = match stderr_detail(state) {
            Some(detail) if !headline.contains(&detail) => format!("{headline}:\n{detail}"),
            _ => headline,
        };

        emit_status(
            app,
            event_name,
            "error",
            port,
            &Some(model_path.to_string()),
            &None,
            false,
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
/// Where the managed chat server's process id is recorded between runs.
///
/// Profile-scoped like everything else the app owns, so two profiles never reap
/// each other's server.
fn chat_server_pid_path(app: &AppHandle) -> Option<PathBuf> {
    app.profile_data_dir()
        .ok()
        .map(|dir| dir.join("llama-chat-server.pid"))
}

/// Writes down the server this start launched, as `pid port start_time`.
///
/// The port is in the record because the port is no longer fixed: a later start
/// that only knew to look at [`CHAT_PORT`] would call a fallback-port orphan's
/// record stale and drop it, stranding the process it named. The start time is
/// in the record because a pid alone is not an identity — after a reboot the
/// same number belongs to something else entirely, and the reap kills a process
/// group.
fn record_chat_server_pid(app: &AppHandle, pid: u32, port: u16) {
    if let Some(path) = chat_server_pid_path(app) {
        let record = match crate::process_tree::ProcessIdentity::of(pid) {
            Some(identity) => format!("{pid} {port} {}", identity.start_time),
            None => format!("{pid} {port}"),
        };
        let _ = std::fs::write(path, record);
    }
}

/// The pid, port and process start time in a recorded line.
///
/// Back-compatible with the bare `"4242"` an older build wrote: no port in the
/// record means the only port that build could have used, [`CHAT_PORT`], and no
/// start time means there is nothing to check the pid against. Anything else is
/// a partial write and names nobody.
fn parse_pid_record(recorded: &str) -> Option<(u32, u16, Option<u64>)> {
    let mut fields = recorded.split_whitespace();
    let pid = match fields.next()?.parse::<u32>() {
        Ok(pid) if pid > 1 => pid,
        _ => return None,
    };
    let port = match fields.next() {
        None => return Some((pid, CHAT_PORT, None)),
        Some(field) => match field.parse::<u16>() {
            Ok(port) if port > 0 => port,
            _ => return None,
        },
    };
    let start_time = match fields.next() {
        None => None,
        Some(field) => Some(field.parse::<u64>().ok()?),
    };
    Some((pid, port, start_time))
}

fn forget_chat_server_pid(app: &AppHandle) {
    if let Some(path) = chat_server_pid_path(app) {
        let _ = std::fs::remove_file(path);
    }
}

/// Kill a chat server this app left behind, before starting a new one.
///
/// `stop_all_blocking` kills the managed server on a clean quit, but a crash, a
/// force quit, or a development rebuild never runs it, and the child outlives
/// the app — still holding the fixed port with the model resident. The next
/// start then cannot bind, and the health probe correctly refuses the stranger
/// already answering there, so the local model is unusable until somebody finds
/// and kills a process by hand. Nothing in the UI says that is what happened;
/// the turn just reports that its target could not be frozen.
///
/// Adopting it instead is not an option: its alias was a nonce this process
/// never saw, and trusting whatever answers on a fixed loopback port is exactly
/// what `fresh_server_alias` exists to prevent.
///
/// Only ever kills a pid this app wrote down itself, and only while that pid is
/// still the process that was recorded *and* something is still holding the port
/// that record names — a recorded pid that has since been recycled onto an
/// unrelated process is left alone.
///
/// Returns the port it signalled a process on, so the caller can wait for that
/// port to actually come free.
async fn reap_recorded_chat_server(app: &AppHandle) -> Option<u16> {
    let path = chat_server_pid_path(app)?;
    let recorded = std::fs::read_to_string(&path).ok()?;
    // The port comes out of the record, not out of this start: the orphan may be
    // sitting on a fallback port this start would never think to look at.
    let parsed = parse_pid_record(&recorded);
    let occupied = match parsed {
        Some((_, recorded_port, _)) => port_occupied(recorded_port).await,
        None => false,
    };
    // An older build's record carries no start time, so it degrades to the port
    // check alone rather than refusing to reap anything it wrote.
    let still_the_recorded_process = match parsed {
        Some((pid, _, Some(start_time))) => crate::process_tree::ProcessIdentity::of(pid)
            .is_some_and(|identity| identity.start_time == start_time),
        _ => true,
    };
    match record_disposition(&recorded, occupied, still_the_recorded_process) {
        RecordDisposition::Kill(pid) => {
            if crate::os_signal::kill_process_group(pid).is_err() {
                // Left on disk deliberately: the signal did not land, so the
                // record still names the process holding the port and the next
                // start should get the same chance at it.
                return None;
            }
            let _ = std::fs::remove_file(&path);
            parsed.map(|(_, recorded_port, _)| recorded_port)
        }
        RecordDisposition::Forget => {
            let _ = std::fs::remove_file(&path);
            None
        }
        RecordDisposition::Keep => None,
    }
}

/// A port nothing is listening on, from the OS rather than from a guess: bind an
/// ephemeral listener, read the port it was given, and release it.
fn free_loopback_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("Could not find a free port for llama-server: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("Could not read the free port for llama-server: {error}"))?
        .port();
    drop(listener);
    Ok(port)
}

/// Whether a failed start is llama.cpp refusing to bind its HTTP socket, which
/// is the one failure another port can fix. Matched on llama.cpp's own wording
/// ("srv start: couldn't bind HTTP server socket, hostname: ..., port: ...")
/// because the child reports it on stderr and exits, with no distinct status.
fn failure_is_port_bind(error: &str) -> bool {
    error.contains("couldn't bind")
}

/// Whether anything holds `port`, established by connecting to it rather than
/// by asking it how it feels.
///
/// The conflict a start has to clear is a *bind* conflict, and llama.cpp binds
/// its port before the model is loaded — answering `/health` with 503
/// `{"status": "loading model"}` until it finishes. Reading a non-200 health
/// response as "nothing there" is what stranded orphans permanently: the kill
/// was skipped for a server that was very much holding the port, and every
/// later start died on
/// `couldn't bind HTTP server socket, hostname: 127.0.0.1, port: 8090`.
async fn port_occupied(port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)),
        )
        .await,
        Ok(Ok(_))
    )
}

/// What a start should do about the pid a previous one recorded.
#[derive(Debug, PartialEq, Eq)]
enum RecordDisposition {
    /// The recorded server is still holding the port: kill it, then drop the
    /// record it came from.
    Kill(u32),
    /// The record cannot name anything worth killing — either the port is free
    /// (so the number says nothing about whatever owns it now) or the file is
    /// not a pid at all.
    Forget,
    /// Something holds the port and the record still describes it, but this
    /// start could not act. Keep the record so the next one can.
    Keep,
}

/// Which recorded pid, if any, this start should kill — and whether the record
/// survives the decision.
///
/// Split out from the IO so the rule is testable. Three properties matter: a pid
/// is only killed while something still holds the port it was recorded on (a
/// recycled pid must never be killed), only while that pid is still the process
/// that was recorded, and the record is only forgotten once it can no longer
/// name the holder. Consuming the record unconditionally meant a single start
/// that declined to kill destroyed the only handle on the orphan, leaving the
/// port unusable until someone killed it by hand.
///
/// The identity check is what keeps the port check honest now that occupancy is
/// a bare TCP connect: a listener on a reused port says nothing about the pid,
/// and after a reboot that pid belongs to something else entirely — which the
/// kill would take out along with its whole process group.
fn record_disposition(
    recorded: &str,
    port_occupied: bool,
    still_the_recorded_process: bool,
) -> RecordDisposition {
    let Some((pid, _, _)) = parse_pid_record(recorded) else {
        return RecordDisposition::Forget;
    };
    if !port_occupied || !still_the_recorded_process {
        return RecordDisposition::Forget;
    }
    RecordDisposition::Kill(pid)
}

#[tauri::command]
pub async fn llama_start(
    app: AppHandle,
    state: State<'_, AppState>,
    model_path: String,
    projector_path: Option<String>,
    ctx_size: Option<u32>,
    gpu_layers: i32,
    embeddings: bool,
) -> Result<u32, String> {
    if embeddings && projector_path.is_some() {
        return Err("A multimodal projector cannot be used when the chat server is started in embedding mode".to_string());
    }
    let verification_path = PathBuf::from(&model_path);
    tokio::task::spawn_blocking(move || {
        crate::model_sources::verify_managed_model_for_runtime(&verification_path)
    })
    .await
    .map_err(|error| format!("Managed model verification task failed: {error}"))??;
    if let Some(projector_path) = projector_path.as_deref() {
        crate::models::verify_projector_for_runtime(&app, &model_path, projector_path)?;
    }

    let binary = find_llama_server_binary_for_app(&app)?;

    {
        // A server this app is still running holds the port until it is killed,
        // and `spawn_and_wait_healthy` only kills it once the port is already
        // chosen — so retire it here, or every restart would find the default
        // port busy and wander onto a fallback for no reason. `status` moves
        // with the process: leaving it "ready" would have `llama_status`,
        // `server.rs`'s routing and the model card all advertising a live server
        // for the whole reap-probe-and-sniff window below, at a port nothing is
        // listening on.
        let mut guard = state.llama.lock().map_err(|e| e.to_string())?;
        if let Some(mut child) = guard.process.take() {
            let _ = child.kill();
            // Reaps the child, so its listening socket is closed by the time
            // this returns — unlike the bare signal an orphan gets below.
            let _ = child.wait();
        }
        guard.status = "starting".to_string();
        guard.stderr_tail = None;
        guard.embeddings_enabled = embeddings;
    }

    if let Some(reaped_port) = reap_recorded_chat_server(&app).await {
        // `kill_process_group` returns as soon as the signal is queued, while
        // the orphan still holds its listening socket until the kernel has torn
        // down an address space with the whole model resident in it — tens of
        // milliseconds and up. Probing straight away would find the port busy
        // and send this start to a fallback for no reason, losing the default
        // port on exactly the crash-recovery path the reap exists for.
        for _ in 0..20 {
            if !port_occupied(reaped_port).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // Prefer the fixed port — a predictable 8090 is worth far more than a random
    // port every launch — and move only when something we are not allowed to
    // kill is still holding it (a foreign process, or one of ours whose pid
    // record was lost).
    // ponytail: nothing holds the port between this probe and llama-server's own
    // bind, so a racing process can take it in between. That race already
    // existed on the fixed port; the single retry further down is the whole
    // mitigation, and a bind lock would cost more than it buys.
    let mut port = CHAT_PORT;
    // The port the user was denied, for the error message: the one this start
    // wanted first, not whichever fallback failed last.
    let mut taken_port: Option<u16> = None;
    if port_occupied(port).await {
        taken_port = Some(port);
        port = free_loopback_port()?;
    }

    let resolved_ctx_size = resolve_ctx_size(ctx_size, Path::new(&model_path));
    let startup_alias = fresh_server_alias();

    // At most two attempts: the only start failure another port can fix is a
    // refused bind, and if a freshly allocated port is taken too, that is a
    // machine problem rather than a race worth looping over.
    let mut retried = false;
    loop {
        state.llama.lock().map_err(|e| e.to_string())?.port = port;
        let args = chat_server_args(
            &model_path,
            projector_path.as_deref(),
            port,
            resolved_ctx_size,
            gpu_layers,
            embeddings,
            &startup_alias,
        );

        match spawn_and_wait_healthy(
            &app,
            &state.llama,
            "llama://status",
            &binary,
            &args,
            port,
            &model_path,
            projector_path.as_deref(),
            &startup_alias,
        )
        .await
        {
            Ok(()) => break,
            Err(error) if failure_is_port_bind(&error) && !retried => {
                retried = true;
                taken_port.get_or_insert(port);
                port = free_loopback_port()?;
            }
            // The user used to get the whole llama.cpp log with a bind refusal
            // buried at the end of it. Say what happened first; keep the tail.
            // Deliberately not claiming *why* the second attempt failed: it may
            // have been the model, not the port.
            Err(error) => {
                return Err(match taken_port {
                    Some(taken) => format!(
                        "Port {taken} was already in use, so the app started llama-server on port \
                         {port} instead. That did not work either — llama-server \
                         reported:\n{error}"
                    ),
                    None => error,
                });
            }
        }
    }

    if let Some(pid) = state
        .llama
        .lock()
        .ok()
        .and_then(|guard| guard.process.as_ref().map(|child| child.id()))
    {
        record_chat_server_pid(&app, pid, port);
    }

    Ok(resolved_ctx_size)
}

/// Kill the managed chat `llama-server` process, if any, and mark it stopped.
#[tauri::command]
pub async fn llama_stop(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    forget_chat_server_pid(&app);
    let mut llama = state.llama.lock().map_err(|e| e.to_string())?;
    if let Some(mut child) = llama.process.take() {
        child
            .kill()
            .map_err(|e| format!("Failed to kill llama-server process: {e}"))?;
        let _ = child.wait();
    }
    llama.status = "stopped".to_string();
    llama.stderr_tail = None;
    llama.model_path = None;
    llama.projector_path = None;
    llama.vision_enabled = false;
    llama.embeddings_enabled = false;
    let _ = app.emit(
        "llama://status",
        json!({
            "status": "stopped",
            "port": llama.port,
            "model_path": null,
            "projector_path": null,
            "vision_enabled": false,
        }),
    );
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
        "projector_path": llama.projector_path,
        "vision_enabled": llama.vision_enabled,
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
        None,
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
        guard.stderr_tail = None;
        drop(guard);
        emit_status(
            &app,
            "embed://status",
            "error",
            EMBED_PORT,
            &Some(model_path),
            &None,
            false,
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
    embed.stderr_tail = None;
    embed.model_path = None;
    embed.projector_path = None;
    embed.vision_enabled = false;
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
        guard.stderr_tail = None;
        guard.model_path = None;
        guard.projector_path = None;
        guard.vision_enabled = false;
    }
    if let Ok(mut guard) = state.embed_llama.lock() {
        if let Some(mut child) = guard.process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        guard.status = "stopped".to_string();
        guard.stderr_tail = None;
        guard.model_path = None;
        guard.projector_path = None;
        guard.vision_enabled = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recorded_pid_is_reaped_only_while_the_port_is_still_held() {
        // The orphan case: a previous run's server is still holding the port.
        assert_eq!(
            record_disposition("4242 8090 99\n", true, true),
            RecordDisposition::Kill(4242)
        );
        // Nothing is listening, so the number says nothing about what owns that
        // pid now — a recycled pid must not be killed, and the record is spent.
        assert_eq!(
            record_disposition("4242 8090 99", false, true),
            RecordDisposition::Forget
        );
        // Something is listening, but the pid now belongs to a different process
        // than the one recorded — after a reboot an ephemeral fallback port and
        // a pid can both have been handed out again, to unrelated owners.
        assert_eq!(
            record_disposition("4242 8090 99", true, false),
            RecordDisposition::Forget
        );
        // Never init, and never nonsense left by a partial write.
        assert_eq!(
            record_disposition("1 8090 99", true, true),
            RecordDisposition::Forget
        );
        assert_eq!(
            record_disposition("", true, true),
            RecordDisposition::Forget
        );
        assert_eq!(
            record_disposition("not-a-pid", true, true),
            RecordDisposition::Forget
        );
    }

    #[test]
    fn a_pid_record_names_its_port_and_an_older_bare_record_means_the_default() {
        assert_eq!(
            parse_pid_record("4242 8092 1234\n"),
            Some((4242, 8092, Some(1234)))
        );
        // Written by a build that only knew one port, and by a platform that
        // reports no start time.
        assert_eq!(parse_pid_record("4242"), Some((4242, CHAT_PORT, None)));
        assert_eq!(parse_pid_record("4242 8092"), Some((4242, 8092, None)));
        assert_eq!(parse_pid_record("4242 not-a-port"), None);
        assert_eq!(parse_pid_record("4242 0"), None);
        assert_eq!(parse_pid_record("4242 8092 not-a-time"), None);
    }

    #[tokio::test]
    async fn an_orphan_on_a_fallback_port_is_still_reaped() {
        // The regression this guards: a previous run fell back to another port,
        // so a start that asked about its own port would find that one free and
        // forget the record, stranding the orphan. The reap asks about the port
        // the record names instead.
        let orphan = std::net::TcpListener::bind("127.0.0.1:0").expect("stand in for the orphan");
        let orphan_port = orphan.local_addr().expect("addr").port();
        assert_ne!(orphan_port, CHAT_PORT);

        let recorded = format!("4242 {orphan_port}");
        let (_, recorded_port, _) = parse_pid_record(&recorded).expect("record parses");
        assert_eq!(recorded_port, orphan_port);
        assert_eq!(
            record_disposition(&recorded, port_occupied(recorded_port).await, true),
            RecordDisposition::Kill(4242)
        );

        // Once it is gone, the number names nobody and must not be killed.
        drop(orphan);
        assert_eq!(
            record_disposition(&recorded, port_occupied(recorded_port).await, true),
            RecordDisposition::Forget
        );
    }

    #[tokio::test]
    async fn the_fallback_port_is_one_nothing_is_listening_on() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a port");
        let taken_port = taken.local_addr().expect("addr").port();
        assert!(port_occupied(taken_port).await);

        let free = free_loopback_port().expect("allocate a free port");
        assert_ne!(free, taken_port);
        // Nothing holds it, so it is bindable — which is all llama-server needs.
        std::net::TcpListener::bind(("127.0.0.1", free)).expect("the free port is free");
    }

    #[test]
    fn a_refused_bind_is_the_one_failure_another_port_can_fix() {
        assert!(failure_is_port_bind(
            "llama-server exited unexpectedly before becoming ready (status: exit status: 1):\n\
             srv    start: couldn't bind HTTP server socket, hostname: 127.0.0.1, port: 8090"
        ));
        assert!(!failure_is_port_bind(
            "Timed out waiting for llama-server to become healthy after 60s"
        ));
    }

    #[tokio::test]
    async fn a_port_held_by_a_server_that_is_not_ready_yet_still_counts_as_occupied() {
        // llama.cpp binds before it loads and answers /health with 503 until it
        // is ready. A listener that never answers HTTP at all is the extreme of
        // that case, and it is exactly the one the old health-status check read
        // as "nothing there" — stranding the orphan holding the port.
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(
            port_occupied(port).await,
            "a bound port is occupied whether or not anything answers HTTP on it"
        );

        drop(listener);
        assert!(
            !port_occupied(port).await,
            "and a released port is free again"
        );
    }

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
                None,
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
            chat_server_args(
                "/models/chat.gguf",
                Some("/models/mmproj.gguf"),
                8090,
                4096,
                99,
                false,
                "little-monkey-chat-nonce",
            ),
            [
                "-m",
                "/models/chat.gguf",
                "--mmproj",
                "/models/mmproj.gguf",
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
    fn write_minimal_gguf_with_context_length(
        path: &std::path::Path,
        architecture: &str,
        context_length: u32,
    ) {
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
