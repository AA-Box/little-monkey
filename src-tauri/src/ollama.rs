//! Lifecycle + API glue for the Ollama local daemon (ollama.com), a second,
//! sibling model provider alongside the local llama.cpp (`llama-server`)
//! provider managed by `llama.rs`.
//!
//! Unlike `llama-server`, Ollama is commonly already running on the host as
//! its own persistent background app/service (installed separately from
//! Little Monkey, from ollama.com). This module never spawns a duplicate `ollama
//! serve` if one is already reachable, and deliberately never stops it —
//! Little Monkey should never be the thing that kills a service other things on the
//! machine may depend on.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{AppHandle, Emitter, State};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use crate::profiles::ProfileScopedPaths;
use crate::AppState;

/// Base URL for Ollama's local daemon (native API + OpenAI-compatible
/// `/v1/chat/completions`).
pub const OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434";

/// A small set of well-known "cloud" model tags offered as quick-fill
/// suggestions. Not exhaustive — Ollama has no stable public API to
/// enumerate the full catalog, and it changes too often to hardcode
/// reliably. Users can pull any tag via the free-text input.
const OLLAMA_EXAMPLE_CLOUD_TAGS: &[&str] = &[
    "gpt-oss:20b-cloud",
    "gpt-oss:120b-cloud",
    "qwen3-coder:480b-cloud",
    "deepseek-v3.1:671b-cloud",
];

/// In-memory state for an `ollama serve` process Little Monkey itself spawned (only
/// when no existing daemon was reachable). No corresponding "stop" is ever
/// offered — see module docs.
#[derive(Default)]
pub struct OllamaState {
    pub process: Option<std::process::Child>,
    pub spawned_by_us: bool,
}

/// Snapshot of Ollama daemon reachability, returned by `ollama_status` and
/// emitted on the `ollama://status` event during `ollama_start`.
#[derive(Serialize, Clone)]
pub struct OllamaStatusPayload {
    pub reachable: bool,
    pub version: Option<String>,
    pub binary_found: bool,
}

/// Locate the `ollama` binary: first on PATH (via `which`), then in the
/// common install locations. Mirrors `llama.rs`'s binary discovery.
fn find_ollama_binary() -> Option<String> {
    if let Ok(output) = Command::new("which").arg("ollama").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() && Path::new(&path).exists() {
                return Some(path);
            }
        }
    }

    for candidate in ["/usr/local/bin/ollama", "/opt/homebrew/bin/ollama"] {
        if Path::new(candidate).exists() {
            return Some(candidate.to_string());
        }
    }

    None
}

/// GET `{OLLAMA_BASE_URL}/api/version` with a short timeout. Returns
/// `(reachable, version)` — never errors, since an unreachable daemon is a
/// normal, expected state rather than a failure.
async fn check_reachable() -> (bool, Option<String>) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()
    {
        Ok(c) => c,
        Err(_) => return (false, None),
    };

    match crate::egress::send(client.get(format!("{OLLAMA_BASE_URL}/api/version"))).await {
        Ok(resp) if resp.status().is_success() => {
            let version = resp.json::<serde_json::Value>().await.ok().and_then(|v| {
                v.get("version")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            });
            (true, version)
        }
        _ => (false, None),
    }
}

/// Emit an `ollama://status` event to all windows with the current status
/// snapshot.
fn emit_status(app: &AppHandle, reachable: bool, version: Option<String>, binary_found: bool) {
    let _ = app.emit(
        "ollama://status",
        OllamaStatusPayload {
            reachable,
            version,
            binary_found,
        },
    );
}

/// Return the current reachability of Ollama's local daemon, plus whether an
/// `ollama` binary could be located on this machine at all. Never returns
/// `Err` — an unreachable daemon is a normal state, not an error.
#[tauri::command]
pub async fn ollama_status() -> Result<OllamaStatusPayload, String> {
    let (reachable, version) = check_reachable().await;
    let binary_found = find_ollama_binary().is_some();
    Ok(OllamaStatusPayload {
        reachable,
        version,
        binary_found,
    })
}

/// Ensure Ollama's local daemon is reachable, starting it if necessary.
///
/// If it's already reachable (the common case — Ollama usually runs as its
/// own background service), this is a no-op: we never spawn a second
/// `ollama serve`, which would just fail with a port-already-in-use error
/// and could interfere with an existing instance other things depend on.
#[tauri::command]
pub async fn ollama_start(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let (reachable, version) = check_reachable().await;
    if reachable {
        emit_status(&app, true, version, true);
        return Ok(());
    }

    let binary = find_ollama_binary().ok_or_else(|| {
        "Could not find the `ollama` binary on your PATH or in common install locations.\n\n\
         Install Ollama from https://ollama.com, then try again."
            .to_string()
    })?;

    emit_status(&app, false, None, true);

    let spawn_result = Command::new(&binary)
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    let child = match spawn_result {
        Ok(child) => child,
        Err(e) => {
            emit_status(&app, false, None, true);
            return Err(format!("Failed to spawn `ollama serve`: {e}"));
        }
    };

    {
        let mut ollama = state.ollama.lock().map_err(|e| e.to_string())?;
        ollama.process = Some(child);
        ollama.spawned_by_us = true;
    }

    // Poll the version endpoint until it responds successfully or we hit the
    // 30s total timeout.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut ready = false;
    let mut ready_version = None;

    while Instant::now() < deadline {
        let (reachable, version) = check_reachable().await;
        if reachable {
            ready = true;
            ready_version = version;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    if ready {
        emit_status(&app, true, ready_version, true);
        Ok(())
    } else {
        emit_status(&app, false, None, true);
        Err("Timed out waiting for Ollama to become reachable after 30s".to_string())
    }
}

/// Metadata describing a single Ollama-managed model tag (pulled locally or
/// a cloud tag routed through the same daemon).
#[derive(Serialize)]
pub struct OllamaModelInfo {
    pub name: String,
    pub size_bytes: u64,
    pub is_cloud: bool,
    pub tool_calling: bool,
    pub vision: bool,
    pub modified_at: String,
}

/// One model currently resident in Ollama's memory, as reported by
/// `GET /api/ps`. This is intentionally a snapshot rather than an assertion
/// that the model will remain resident after the command returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OllamaRunningModelInfo {
    pub name: String,
    pub size_bytes: u64,
    pub size_vram_bytes: u64,
    pub digest: String,
    pub expires_at: String,
}

/// Minimal shape of a single entry in `GET /api/tags`'s `models` array.
/// Ollama's field naming has varied across versions/endpoints (`name` vs
/// `model`), so both are accepted defensively.
#[derive(Deserialize)]
struct RawTagEntry {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    modified_at: String,
}

#[derive(Deserialize)]
struct RawTagsResponse {
    #[serde(default)]
    models: Vec<RawTagEntry>,
}

/// Minimal shape of a single entry in `GET /api/ps`'s `models` array.
/// Accept both `name` and `model`, matching the defensive parsing used for
/// `/api/tags` above across Ollama versions.
#[derive(Deserialize)]
struct RawRunningModelEntry {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    size_vram: u64,
    #[serde(default)]
    digest: String,
    #[serde(default)]
    expires_at: String,
}

#[derive(Deserialize)]
struct RawRunningModelsResponse {
    #[serde(default)]
    models: Vec<RawRunningModelEntry>,
}

#[derive(Deserialize, Default)]
struct RawShowResponse {
    #[serde(default)]
    capabilities: Vec<String>,
}

/// Best-effort check of a model's advertised capabilities via `POST
/// /api/show`: `(tool_calling, vision)`. `tool_calling` defaults to `true`
/// (optimistic — most modern Ollama models support tool calling, and this is
/// just a UI hint) if the request fails or `capabilities` is absent;
/// `vision` defaults to `false` in that case, since guessing a model can see
/// images when it can't would be a worse failure mode than the reverse.
async fn check_capabilities(client: &reqwest::Client, name: &str) -> (bool, bool) {
    let resp = crate::egress::send(
        client
            .post(format!("{OLLAMA_BASE_URL}/api/show"))
            .json(&json!({ "model": name })),
    )
    .await;

    match resp {
        Ok(r) if r.status().is_success() => match r.json::<RawShowResponse>().await {
            Ok(show) if !show.capabilities.is_empty() => (
                show.capabilities.iter().any(|c| c == "tools"),
                show.capabilities.iter().any(|c| c == "vision"),
            ),
            _ => (true, false),
        },
        _ => (true, false),
    }
}

/// List locally-pulled Ollama models (including cloud tags, which are
/// ordinary tags once pulled), enriched with a best-effort tool-calling
/// hint fetched concurrently for all models.
#[tauri::command]
pub async fn ollama_list_models() -> Result<Vec<OllamaModelInfo>, String> {
    // Bounds `/api/tags` and every concurrent `/api/show` hint below, since
    // `check_capabilities` borrows this same client.
    let client = ollama_client(Duration::from_secs(10))?;

    let resp = crate::egress::send(client.get(format!("{OLLAMA_BASE_URL}/api/tags")))
        .await
        .map_err(|_| "Ollama isn't running — start it first".to_string())?;

    if !resp.status().is_success() {
        return Err("Ollama isn't running — start it first".to_string());
    }

    let parsed: RawTagsResponse = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse Ollama's model list: {e}"))?;

    let names: Vec<(String, u64, String)> = parsed
        .models
        .into_iter()
        .filter_map(|entry| {
            let name = entry.name.or(entry.model)?;
            Some((name, entry.size, entry.modified_at))
        })
        .collect();

    let capability_flags = futures_util::future::join_all(
        names
            .iter()
            .map(|(name, _, _)| check_capabilities(&client, name)),
    )
    .await;

    let models = names
        .into_iter()
        .zip(capability_flags)
        .map(
            |((name, size, modified_at), (tool_calling, vision))| OllamaModelInfo {
                is_cloud: name.to_lowercase().contains("cloud"),
                name,
                size_bytes: size,
                tool_calling,
                vision,
                modified_at,
            },
        )
        .collect();

    Ok(models)
}

/// Convert Ollama's version-tolerant `/api/ps` response into the stable shape
/// exposed to the frontend. Entries without either supported model-name field
/// are ignored because they cannot be targeted safely for an exact unload.
fn normalize_running_models(parsed: RawRunningModelsResponse) -> Vec<OllamaRunningModelInfo> {
    parsed
        .models
        .into_iter()
        .filter_map(|entry| {
            let name = entry.name.or(entry.model)?;
            Some(OllamaRunningModelInfo {
                name,
                size_bytes: entry.size,
                size_vram_bytes: entry.size_vram,
                digest: entry.digest,
                expires_at: entry.expires_at,
            })
        })
        .collect()
}

/// Fetch the currently resident Ollama models using a caller-provided client.
/// Keeping the HTTP operation here lets both residency commands use identical
/// parsing and error semantics.
async fn fetch_running_models(
    client: &reqwest::Client,
) -> Result<Vec<OllamaRunningModelInfo>, String> {
    let resp = crate::egress::send(client.get(format!("{OLLAMA_BASE_URL}/api/ps")))
        .await
        .map_err(|_| "Ollama isn't running — start it first".to_string())?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(if body.is_empty() {
            format!("Failed to list Ollama's running models (HTTP {status})")
        } else {
            format!("Failed to list Ollama's running models (HTTP {status}): {body}")
        });
    }

    let parsed: RawRunningModelsResponse = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse Ollama's running model list: {e}"))?;

    Ok(normalize_running_models(parsed))
}

/// A client for one of this file's calls to the local Ollama daemon, with
/// `total` as the deadline for the whole request.
///
/// A *total* deadline is the right shape here, unlike on a download path: every
/// caller reads a small, fully buffered JSON body from a loopback peer, so the
/// deadline can be proportionate to the response instead of racing it.
///
/// Exists because two commands had no deadline of any kind. reqwest's bare
/// constructor sets none, and a `#[tauri::command]` that never returns is worse
/// than one that fails: `ollama_list_models` and `ollama_remove_model` are both
/// `invoke`d from the UI with nothing racing them and no cancellation token, so a
/// daemon that accepted the connection and then went quiet left the caller
/// waiting forever with no error to show. The other calls in this file already
/// passed a budget; these two were simply missed, which is what the bare-client
/// ratchet in `egress.rs` now records as fixed rather than allowed.
///
/// Prose here avoids spelling that constructor out: the ratchet counts the exact
/// string across the tree, so a doc comment naming it would count as a site — the
/// same reason `egress.rs`'s own doc comments talk around it.
fn ollama_client(total: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(total)
        .build()
        .map_err(|e| format!("Failed to create Ollama HTTP client: {e}"))
}

fn residency_client() -> Result<reqwest::Client, String> {
    ollama_client(Duration::from_secs(10))
}

/// List the models currently loaded in Ollama memory. Callers can snapshot
/// this list before a comparison run and avoid unloading models that were
/// already resident before Little Monkey started work.
#[tauri::command]
pub async fn ollama_list_running_models() -> Result<Vec<OllamaRunningModelInfo>, String> {
    let client = residency_client()?;
    fetch_running_models(&client).await
}

fn unload_request_body(model: &str) -> serde_json::Value {
    json!({
        "model": model,
        "messages": [],
        "keep_alive": 0,
        "stream": false,
    })
}

fn contains_exact_running_model(models: &[OllamaRunningModelInfo], model: &str) -> bool {
    models.iter().any(|entry| entry.name == model)
}

/// Unload exactly one currently resident model without stopping the Ollama
/// daemon. The `/api/ps` preflight is deliberately case-sensitive and does
/// not resolve aliases: if that exact name is no longer resident, this is an
/// idempotent no-op. That avoids asking Ollama to load an absent model merely
/// to process a `keep_alive: 0` request.
#[tauri::command]
pub async fn ollama_unload_model(model: String) -> Result<(), String> {
    validate_tag(&model)?;
    let model = model.trim().to_string();
    let client = residency_client()?;

    let running = fetch_running_models(&client).await?;
    if !contains_exact_running_model(&running, &model) {
        return Ok(());
    }

    let resp = crate::egress::send(
        client
            .post(format!("{OLLAMA_BASE_URL}/api/chat"))
            .json(&unload_request_body(&model)),
    )
    .await
    .map_err(|e| format!("Failed to reach Ollama while unloading '{model}': {e}"))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(if body.is_empty() {
            format!("Failed to unload '{model}' (HTTP {status})")
        } else {
            format!("Failed to unload '{model}' (HTTP {status}): {body}")
        })
    }
}

/// Lightweight tag-name-only fetch for `server.rs`'s `GET /v1/models`
/// merging — reuses the same lenient `/api/tags` parse as
/// [`ollama_list_models`] but deliberately skips the per-model `/api/show`
/// capability probes that function does: those are the slow part (the
/// design doc's "Ollama model listing latency" risk note), and an API
/// client listing servable model ids has no use for the tool-calling/vision
/// UI hints those probes exist for.
pub async fn list_tag_names(client: &reqwest::Client) -> Result<Vec<String>, String> {
    list_tag_names_at(client, OLLAMA_BASE_URL).await
}

/// Endpoint-injected form of [`list_tag_names`]. Production callers use the
/// built-in loopback origin; the unified CLI compatibility harness supplies an
/// ephemeral loopback fake so its byte pins never depend on, or talk to, a
/// developer's real Ollama daemon.
pub async fn list_tag_names_at(
    client: &reqwest::Client,
    base_url: &str,
) -> Result<Vec<String>, String> {
    let resp =
        crate::egress::send(client.get(format!("{}/api/tags", base_url.trim_end_matches('/'))))
            .await
            .map_err(|_| "Ollama isn't running — start it first".to_string())?;

    if !resp.status().is_success() {
        return Err("Ollama isn't running — start it first".to_string());
    }

    let parsed: RawTagsResponse = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse Ollama's model list: {e}"))?;

    Ok(parsed
        .models
        .into_iter()
        .filter_map(|entry| entry.name.or(entry.model))
        .collect())
}

/// Returns the built-in list of example cloud model tags, so the frontend
/// carries zero hardcoded tag strings of its own.
#[tauri::command]
pub fn ollama_example_cloud_tags() -> Vec<String> {
    OLLAMA_EXAMPLE_CLOUD_TAGS
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Shape of `POST /api/embed`'s response — a batch of embedding vectors, one
/// per input string, in the same order.
#[derive(Deserialize)]
struct RawEmbedResponse {
    #[serde(default)]
    embeddings: Vec<Vec<f32>>,
}

/// Embed a batch of strings via Ollama's native `POST /api/embed` endpoint —
/// the Ollama half of `stacks.rs`'s `embed_batch` dispatch (the other half,
/// `llama.rs`'s managed `--embeddings` instance, goes through
/// `/v1/embeddings` directly since it needs no daemon-reachability check).
/// Not a `#[tauri::command]`: this is only ever called from Rust
/// (`stacks::embed_batch`), never invoked directly from the frontend.
pub async fn embed(model: &str, inputs: &[String]) -> Result<Vec<Vec<f32>>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

    let resp = crate::egress::send(
        client
            .post(format!("{OLLAMA_BASE_URL}/api/embed"))
            .json(&json!({ "model": model, "input": inputs })),
    )
    .await
    .map_err(|e| format!("Failed to reach Ollama for embeddings: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Ollama embedding request failed (HTTP {status}): {body}"
        ));
    }

    let parsed: RawEmbedResponse = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse Ollama's embedding response: {e}"))?;

    if parsed.embeddings.len() != inputs.len() {
        return Err(format!(
            "Ollama returned {} embeddings for {} inputs",
            parsed.embeddings.len(),
            inputs.len()
        ));
    }

    Ok(parsed.embeddings)
}

/// Validates a model tag: non-empty after trimming, and restricted to a
/// conservative charset. Defense in depth — `Command::arg` never invokes a
/// shell, so this isn't a real injection vector, but a malformed tag should
/// fail fast with a clear message rather than a confusing Ollama CLI error.
fn validate_tag(tag: &str) -> Result<(), String> {
    let trimmed = tag.trim();
    if trimmed.is_empty() {
        return Err("Invalid model tag".to_string());
    }
    let valid = trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'));
    if !valid {
        return Err("Invalid model tag".to_string());
    }
    Ok(())
}

/// Streams `stdout`/`stderr`'s combined output line-by-line as
/// `ollama://pull-progress` events tagged with `tag`, returning the last ~20
/// lines seen once both streams reach EOF — for composing a failure message.
/// Does not wait on the child itself; callers own that (see
/// [`stream_ollama_progress`] and [`ollama_pull_model`]).
async fn stream_process_output(
    app: &AppHandle,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    tag: &str,
) -> std::collections::VecDeque<String> {
    let last_lines = std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::VecDeque::<
        String,
    >::with_capacity(21)));

    let mut tasks = Vec::new();

    if let Some(stdout) = stdout {
        let app = app.clone();
        let tag = tag.to_string();
        let last_lines = last_lines.clone();
        tasks.push(tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = app.emit(
                    "ollama://pull-progress",
                    json!({ "tag": tag, "line": line }),
                );
                let mut buf = last_lines.lock().await;
                buf.push_back(line);
                if buf.len() > 20 {
                    buf.pop_front();
                }
            }
        }));
    }

    if let Some(stderr) = stderr {
        let app = app.clone();
        let tag = tag.to_string();
        let last_lines = last_lines.clone();
        tasks.push(tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = app.emit(
                    "ollama://pull-progress",
                    json!({ "tag": tag, "line": line }),
                );
                let mut buf = last_lines.lock().await;
                buf.push_back(line);
                if buf.len() > 20 {
                    buf.pop_front();
                }
            }
        }));
    }

    for task in tasks {
        let _ = task.await;
    }

    let result = last_lines.lock().await.clone();
    result
}

/// Spawns `child` (already configured with piped stdout/stderr) and streams
/// its combined output line-by-line as `ollama://pull-progress` events
/// tagged with `tag` — shared by `ollama_import_model` and
/// `ollama_create_from_modelfile` (both `ollama create`), since the
/// frontend's progress/error UI treats "getting a model named `tag` ready"
/// identically regardless of which CLI subcommand produced it. On failure,
/// the error is the last ~20 output lines joined, so auth/validation errors
/// (e.g. "you are not signed in") surface verbatim.
///
/// `ollama_pull_model` doesn't use this: it needs the `Child` reachable from
/// `state.ollama_pulls` for cancellation, so it calls
/// [`stream_process_output`] directly and waits on the child itself.
async fn stream_ollama_progress(
    app: &AppHandle,
    mut child: tokio::process::Child,
    tag: &str,
) -> Result<(), String> {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let last_lines = stream_process_output(app, stdout, stderr, tag).await;

    let status = child
        .wait()
        .await
        .map_err(|e| format!("Failed to wait for process: {e}"))?;

    if status.success() {
        Ok(())
    } else {
        let joined = last_lines.iter().cloned().collect::<Vec<_>>().join("\n");
        Err(if joined.is_empty() {
            format!("Failed (exit status: {status})")
        } else {
            joined
        })
    }
}

/// Pull a model tag via the `ollama` CLI, streaming its combined
/// stdout+stderr line-by-line as `ollama://pull-progress` events. Errors
/// (including auth-required errors like "you are not signed in") surface
/// verbatim via the last captured output lines.
///
/// Unlike [`stream_ollama_progress`]'s other two callers, this keeps the
/// spawned child in `state.ollama_pulls` (keyed by `tag`) for the duration of
/// the pull, so [`ollama_cancel_pull`] can kill it from a separate command
/// invocation — the frontend's Cancel button while a pull is in flight.
#[tauri::command]
pub async fn ollama_pull_model(
    app: AppHandle,
    state: State<'_, AppState>,
    tag: String,
) -> Result<(), String> {
    validate_tag(&tag)?;
    let tag = tag.trim().to_string();

    let binary = find_ollama_binary().unwrap_or_else(|| "ollama".to_string());

    let mut child = tokio::process::Command::new(&binary)
        .arg("pull")
        .arg(&tag)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn `ollama pull {tag}`: {e}"))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    {
        let mut pulls = state.ollama_pulls.lock().map_err(|e| e.to_string())?;
        pulls.insert(tag.clone(), child);
    }

    let last_lines = stream_process_output(&app, stdout, stderr, &tag).await;

    // If `ollama_cancel_pull` already removed and killed this entry, there's
    // nothing left to wait on — report the cancellation rather than a
    // misleading "Failed" from a status this task never observed.
    let removed = {
        let mut pulls = state.ollama_pulls.lock().map_err(|e| e.to_string())?;
        pulls.remove(&tag)
    };
    let Some(mut child) = removed else {
        return Err("Cancelled".to_string());
    };

    let status = child
        .wait()
        .await
        .map_err(|e| format!("Failed to wait for process: {e}"))?;

    if status.success() {
        Ok(())
    } else {
        let joined = last_lines.iter().cloned().collect::<Vec<_>>().join("\n");
        Err(if joined.is_empty() {
            format!("Failed (exit status: {status})")
        } else {
            joined
        })
    }
}

/// Cancels an in-flight [`ollama_pull_model`] pull for `tag`, if one is
/// running — kills the underlying `ollama pull` child process. A no-op
/// (`Ok`) if no pull for this tag is currently tracked, e.g. it already
/// finished or was already cancelled.
#[tauri::command]
pub async fn ollama_cancel_pull(state: State<'_, AppState>, tag: String) -> Result<(), String> {
    let tag = tag.trim().to_string();
    let child = {
        let mut pulls = state.ollama_pulls.lock().map_err(|e| e.to_string())?;
        pulls.remove(&tag)
    };
    let Some(mut child) = child else {
        return Ok(());
    };
    child
        .start_kill()
        .map_err(|e| format!("Failed to cancel pull: {e}"))?;
    let _ = child.wait().await;
    Ok(())
}

/// Import a local model — either a single `.gguf` file or a directory of
/// Safetensors weights (a Hugging Face-style repo checkout with
/// `config.json` + `*.safetensors` + tokenizer files) — into Ollama under
/// `name`, via `ollama create -f <Modelfile>`. Ollama itself performs any
/// Safetensors -> GGUF conversion needed; Little Monkey never touches model weights
/// directly here, just writes a one-line Modelfile pointing `FROM` at
/// `path` and shells out.
#[tauri::command]
pub async fn ollama_import_model(app: AppHandle, name: String, path: String) -> Result<(), String> {
    validate_tag(&name)?;
    let name = name.trim().to_string();

    let source = std::path::PathBuf::from(&path)
        .canonicalize()
        .map_err(|e| format!("Path not found: {path} ({e})"))?;

    let app_dir = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data directory: {e}"))?;
    std::fs::create_dir_all(&app_dir).map_err(|e| {
        format!(
            "Failed to create app data directory {}: {e}",
            app_dir.display()
        )
    })?;
    let modelfile_path = app_dir.join(format!("Modelfile.{}", uuid::Uuid::new_v4()));
    std::fs::write(&modelfile_path, format!("FROM {}\n", source.display()))
        .map_err(|e| format!("Failed to write Modelfile: {e}"))?;

    let binary = find_ollama_binary().unwrap_or_else(|| "ollama".to_string());

    let spawn_result = tokio::process::Command::new(&binary)
        .arg("create")
        .arg(&name)
        .arg("-f")
        .arg(&modelfile_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let result = match spawn_result {
        Ok(child) => stream_ollama_progress(&app, child, &name).await,
        Err(e) => Err(format!("Failed to spawn `ollama create {name}`: {e}")),
    };

    let _ = std::fs::remove_file(&modelfile_path);
    result
}

/// Create a model from a full, user-authored Modelfile — the "Modelfile
/// Studio" hardened counterpart to [`ollama_import_model`]'s throwaway
/// one-line `FROM <path>` writer above. `modelfile_text` is expected to have
/// already been previewed via `modelfile::modelfile_dry_run`, but this
/// command re-parses and re-validates it from scratch regardless — a
/// frontend claiming "already validated" is never trusted for a command that
/// writes a file and shells out. `short_name` and every path-shaped
/// instruction (`FROM`, `ADAPTER`) are validated/canonicalized before
/// anything touches disk; the actual `ollama create -f` invocation and
/// progress streaming are identical to `ollama_import_model`'s, and this
/// still never installs anything until this command is explicitly called —
/// the frontend only calls it after the user confirms a successful preview.
#[tauri::command]
pub async fn ollama_create_from_modelfile(
    app: AppHandle,
    short_name: String,
    modelfile_text: String,
) -> Result<(), String> {
    let short_name =
        crate::modelfile::validate_short_name(&short_name).map_err(|e| e.to_string())?;
    let parsed = crate::modelfile::parse_modelfile(&modelfile_text).map_err(|e| e.to_string())?;
    crate::modelfile::validate_modelfile(&parsed).map_err(|e| e.to_string())?;
    let resolved = parsed
        .with_canonicalized_paths()
        .map_err(|e| e.to_string())?;
    let rendered = resolved.render();

    let app_dir = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data directory: {e}"))?;
    std::fs::create_dir_all(&app_dir).map_err(|e| {
        format!(
            "Failed to create app data directory {}: {e}",
            app_dir.display()
        )
    })?;
    let modelfile_path = app_dir.join(format!("Modelfile.{}", uuid::Uuid::new_v4()));
    std::fs::write(&modelfile_path, rendered)
        .map_err(|e| format!("Failed to write Modelfile: {e}"))?;

    let binary = find_ollama_binary().unwrap_or_else(|| "ollama".to_string());

    let spawn_result = tokio::process::Command::new(&binary)
        .arg("create")
        .arg(&short_name)
        .arg("-f")
        .arg(&modelfile_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let result = match spawn_result {
        Ok(child) => stream_ollama_progress(&app, child, &short_name).await,
        Err(e) => Err(format!("Failed to spawn `ollama create {short_name}`: {e}")),
    };

    let _ = std::fs::remove_file(&modelfile_path);
    result
}

/// Remove a locally-pulled model tag via Ollama's native `DELETE /api/delete`
/// endpoint. Only forgets the tag from Ollama's own store — Little Monkey doesn't
/// manage where Ollama keeps its blobs.
#[tauri::command]
pub async fn ollama_remove_model(tag: String) -> Result<(), String> {
    validate_tag(&tag)?;
    let tag = tag.trim().to_string();

    // Longer than the read-only calls: Ollama unlinks the tag's blobs before it
    // answers, and a large model's blobs are large files.
    let client = ollama_client(Duration::from_secs(60))?;
    let resp = crate::egress::send(
        client
            .delete(format!("{OLLAMA_BASE_URL}/api/delete"))
            .json(&json!({ "model": tag })),
    )
    .await
    .map_err(|e| format!("Failed to reach Ollama: {e}"))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(if body.is_empty() {
            format!("Failed to remove '{tag}' (HTTP {status})")
        } else {
            format!("Failed to remove '{tag}': {body}")
        })
    }
}

/// Reads `reader` to end-of-stream, accumulating everything into a String.
/// Used only under an outer timeout in `ollama_signin`, since the process
/// being read from is expected to keep running well past that timeout.
async fn drain_to_string(mut reader: impl tokio::io::AsyncRead + Unpin) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).to_string()
}

/// Spawn `ollama signin` (a CLI-driven browser OAuth flow) as a detached,
/// unmanaged child process. Captures whatever it prints within a short
/// window and returns that text; the process is left running independently
/// afterward — Little Monkey doesn't manage its lifecycle.
#[tauri::command]
pub async fn ollama_signin(_app: AppHandle) -> Result<String, String> {
    let binary = find_ollama_binary().unwrap_or_else(|| "ollama".to_string());

    let mut child = tokio::process::Command::new(&binary)
        .arg("signin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn `ollama signin`: {e}"))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let combined = async move {
        let (out, err) = tokio::join!(
            async {
                if let Some(o) = stdout {
                    drain_to_string(o).await
                } else {
                    String::new()
                }
            },
            async {
                if let Some(e) = stderr {
                    drain_to_string(e).await
                } else {
                    String::new()
                }
            },
        );
        format!("{out}{err}")
    };

    let captured = tokio::time::timeout(Duration::from_secs(4), combined)
        .await
        .unwrap_or_default();

    // Deliberately do not kill or store `child` — `ollama signin` drives a
    // browser OAuth flow the user completes independently of Little Monkey, and
    // Little Monkey doesn't manage this process's lifecycle. Dropping `child` here
    // does not kill it since tokio's Child (like std's) only kills on drop
    // unless explicitly configured to (`kill_on_drop`), which we don't set.
    std::mem::drop(child);

    let trimmed = captured.trim();
    if trimmed.is_empty() {
        Ok("Sign-in started — a browser window should open. Complete it there, then try pulling the model again.".to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

#[cfg(test)]
mod residency_tests {
    use super::*;

    /// A daemon that accepts and then goes quiet must not hold a command open.
    ///
    /// Driven against a silent loopback listener with an injected 200ms budget,
    /// because the production budgets are 10 and 60 seconds and a test that waited
    /// one out would not be a test. The listener is *held*, not dropped: dropping
    /// sends `FIN`, which reqwest reports as a connection error, and this would
    /// then pass with no deadline configured at all — the exact bug it exists to
    /// catch.
    #[tokio::test]
    async fn a_daemon_that_accepts_and_goes_quiet_does_not_hold_a_command_open() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a silent peer");
        let address = listener.local_addr().expect("peer address");

        let held = std::thread::spawn(move || listener.accept().map(|(stream, _)| stream));

        let started = std::time::Instant::now();
        let result = ollama_client(Duration::from_millis(200))
            .expect("client builds")
            .get(format!("http://{address}/api/tags"))
            .send()
            .await;

        assert!(
            result.is_err(),
            "a daemon that never answers must not be waited on forever"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "gave up after {:?}, which is not the 200ms budget",
            started.elapsed()
        );
        // Proves the peer accepted, so the error above is the deadline rather
        // than a refused connection — `is_err()` alone cannot tell them apart.
        assert!(
            held.join().expect("accept thread joins").is_ok(),
            "the request must have reached the peer"
        );
    }

    fn parse_running_models(json: &str) -> Vec<OllamaRunningModelInfo> {
        let raw: RawRunningModelsResponse =
            serde_json::from_str(json).expect("valid /api/ps fixture");
        normalize_running_models(raw)
    }

    #[test]
    fn parses_running_models_across_name_field_variants() {
        let models = parse_running_models(
            r#"{
                "models": [
                    {
                        "name": "llama3.2:latest",
                        "size": 2100000000,
                        "size_vram": 1800000000,
                        "digest": "sha256:first",
                        "expires_at": "2026-07-13T12:00:00Z"
                    },
                    {
                        "model": "qwen3:8b",
                        "size": 5200000000,
                        "size_vram": 0,
                        "digest": "sha256:second"
                    },
                    {
                        "size": 123
                    }
                ]
            }"#,
        );

        assert_eq!(
            models,
            vec![
                OllamaRunningModelInfo {
                    name: "llama3.2:latest".to_string(),
                    size_bytes: 2_100_000_000,
                    size_vram_bytes: 1_800_000_000,
                    digest: "sha256:first".to_string(),
                    expires_at: "2026-07-13T12:00:00Z".to_string(),
                },
                OllamaRunningModelInfo {
                    name: "qwen3:8b".to_string(),
                    size_bytes: 5_200_000_000,
                    size_vram_bytes: 0,
                    digest: "sha256:second".to_string(),
                    expires_at: String::new(),
                },
            ]
        );
    }

    #[test]
    fn missing_models_array_is_an_empty_snapshot() {
        assert!(parse_running_models("{}").is_empty());
    }

    #[test]
    fn exact_residency_match_does_not_resolve_aliases_or_case() {
        let models = vec![OllamaRunningModelInfo {
            name: "llama3.2:latest".to_string(),
            size_bytes: 1,
            size_vram_bytes: 1,
            digest: String::new(),
            expires_at: String::new(),
        }];

        assert!(contains_exact_running_model(&models, "llama3.2:latest"));
        assert!(!contains_exact_running_model(&models, "llama3.2"));
        assert!(!contains_exact_running_model(&models, "LLAMA3.2:latest"));
    }

    #[test]
    fn unload_request_uses_empty_chat_and_zero_keep_alive() {
        assert_eq!(
            unload_request_body("llama3.2:latest"),
            json!({
                "model": "llama3.2:latest",
                "messages": [],
                "keep_alive": 0,
                "stream": false,
            })
        );
    }
}
