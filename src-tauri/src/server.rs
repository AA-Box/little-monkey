//! Local OpenAI-compatible API server (phase 1 of the local-model-hub
//! roadmap item — see `docs/roadmap/p1-local-api-server.md`).
//!
//! This is a *routing reverse proxy*, not a new inference engine: it runs a
//! small hyper-1 HTTP server on a tokio task, bound to `127.0.0.1` only (no
//! LAN bind — that's an explicitly later, separately-gated phase), and
//! exposes exactly four routes:
//!
//!   - `GET  /health`              — unauthenticated liveness probe
//!   - `GET  /v1/models`           — merged list of servable models
//!   - `POST /v1/chat/completions` — proxies to llama-server or Ollama
//!   - everything else            — `404`
//!
//! SECURITY: this surface is deliberately narrow. It must NEVER grow a route
//! that reaches the agent's tool-dispatch layer (`tool_run_shell` and
//! friends in `tools.rs`) — doing so would turn a local HTTP server into a
//! remote-code-execution surface for anything that can reach loopback. Any
//! future change to the `match` in [`handle_request`] must preserve that
//! invariant.
//!
//! Structured like `checkpoints.rs`/`web.rs`: an `AppHandle`-free,
//! independently testable core ([`handle_request`], [`route_model`]) plus a
//! thin `#[tauri::command]` layer (`api_server_start`/`_stop`/`_status`) that
//! owns the actual listening socket and `AppState` bookkeeping. `pub` (not
//! `mod`) so a future `lm-cli` `api-serve` subcommand (design doc phase 4)
//! can reuse [`handle_request`] directly, the same reasoning as
//! `web`/`prompts`/`rules` above it in `lib.rs`.
//!
//! Auth (phase 1 slice): a single bearer token, auto-generated fresh every
//! time the server starts and shown once in the Settings panel. Only its
//! SHA-256 digest is ever compared against — the plaintext lives in
//! [`ApiServerState`] purely so the UI can display/copy it, never written to
//! disk. The full multi-token `TokenEntry` model with scopes/backends and
//! `api_server.json` persistence is phase 2 — see the design doc.

use std::convert::Infallible;
use std::path::Path;
use std::sync::Arc;

use futures_util::StreamExt;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rand::Rng;
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::{ollama, AppState};

/// LM Studio-compatible default port, so drop-in clients that hardcode 1234
/// need no configuration at all.
const DEFAULT_PORT: u16 = 1234;

/// Prefix on every generated bearer token, so a leaked token is
/// self-describing (`lmk-<32 hex chars>`) the same way e.g. GitHub's
/// `ghp_`/OpenAI's `sk-` prefixes are.
const TOKEN_PREFIX: &str = "lmk-";

/// Boxed error type for [`ResponseBody`] — reqwest's streaming errors and
/// our own infallible bodies both erase to this.
type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Unified response body type: either a fully-buffered JSON payload ([`Full`])
/// or an SSE byte-stream passthrough ([`StreamBody`]), both boxed so
/// [`handle_request`] can return one concrete type regardless of which route
/// it took.
type ResponseBody = BoxBody<Bytes, BoxError>;

/// In-memory lifecycle state for the managed API server process — mirrors
/// `llama::LlamaState` field-for-field where the design doc specified it
/// (`shutdown`/`port`/`status`/`request_count`/`last_error`), plus two extra
/// fields the doc's literal struct listing didn't account for:
/// `token`/`token_sha256`. Phase 1 needs *somewhere* to hold the single
/// auto-generated bearer token so the Settings panel can display/copy it —
/// phase 2's full `TokenEntry`/`api_server.json` store doesn't exist yet.
/// Both are held only in memory (never written to disk) and regenerated
/// fresh on every start, cleared on stop.
pub struct ApiServerState {
    /// Present while the accept loop is running; `notify_one()`-ing this is
    /// how `api_server_stop` asks it to close the listening socket — same
    /// cancel idiom as `AppState::stream_cancels`/`tool_cancel`. Exactly one
    /// task (the accept loop) ever awaits this, so `notify_one` (not
    /// `notify_waiters`) is correct here.
    pub shutdown: Option<Arc<Notify>>,
    pub port: u16,
    /// `"stopped" | "starting" | "running" | "error"`.
    pub status: String,
    pub request_count: u64,
    pub last_error: Option<String>,
    /// Plaintext, in memory only — shown once (and re-shown while the
    /// server stays up) in the Settings panel's copy chip.
    pub token: Option<String>,
    /// SHA-256 hex digest of `token`, compared against every request's
    /// `Authorization: Bearer` header.
    pub token_sha256: Option<String>,
}

impl Default for ApiServerState {
    fn default() -> Self {
        ApiServerState {
            shutdown: None,
            port: DEFAULT_PORT,
            status: "stopped".to_string(),
            request_count: 0,
            last_error: None,
            token: None,
            token_sha256: None,
        }
    }
}

/// Snapshot broadcast on `apiserver://status` and returned by the three
/// commands below — mirrors `llama::emit_status`'s payload shape.
#[derive(Debug, Clone, Serialize)]
pub struct ApiServerStatusPayload {
    pub status: String,
    pub port: u16,
    pub request_count: u64,
    pub last_error: Option<String>,
    pub token: Option<String>,
}

fn status_payload(state: &ApiServerState) -> ApiServerStatusPayload {
    ApiServerStatusPayload {
        status: state.status.clone(),
        port: state.port,
        request_count: state.request_count,
        last_error: state.last_error.clone(),
        token: state.token.clone(),
    }
}

/// Emit an `apiserver://status` event to all windows, mirroring
/// `llama.rs`'s `emit_status`/`ollama.rs`'s `emit_status` convention.
fn emit_status(app: &AppHandle, payload: &ApiServerStatusPayload) {
    let _ = app.emit("apiserver://status", payload.clone());
}

// ---------------------------------------------------------------------
// Token generation + constant-time verification
// ---------------------------------------------------------------------

/// Generates a fresh `lmk-` + 32 hex char bearer token from the OS RNG
/// (`rand`'s thread-local `ThreadRng`, itself seeded from the OS CSPRNG).
fn generate_token() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{TOKEN_PREFIX}{hex}")
}

/// Lowercase hex-encoded SHA-256 digest of `input` — only this, never the
/// plaintext, is what [`authenticate`] compares against.
fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time comparison of two equal-length-when-valid hex digest
/// strings, so a mismatched bearer token can't be brute-forced faster via
/// early-exit timing differences. Both inputs here are always fixed-length
/// (64-char) SHA-256 hex digests in the real request path, so the length
/// check itself leaks nothing an attacker doesn't already know.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a_bytes, b_bytes) = (a.as_bytes(), b.as_bytes());
    if a_bytes.len() != b_bytes.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a_bytes.iter().zip(b_bytes.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------
// Model-id routing (pure, no I/O — directly unit-testable)
// ---------------------------------------------------------------------

/// Which upstream a `POST /v1/chat/completions` request's `model` field
/// should be routed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelRoute {
    Llama,
    Ollama,
    Unknown,
}

/// Pure routing decision, split out from [`handle_chat_completions`] so it's
/// directly unit-testable with no I/O — same `*_impl`-style extraction as
/// `web.rs::validate_fetch_url`. `model` is `Unknown` only when blank
/// (missing/empty `model` field); a non-empty id that isn't the exact
/// ready-llama filename stem is always assumed to be an Ollama tag — Ollama
/// itself is the source of truth for whether that tag actually exists, and
/// a wrong guess there surfaces as a `502` wrapping Ollama's own error
/// rather than a possibly-stale local "unknown model" guess. Cloud-provider
/// routing (`"{provider_id}/{model_id}"` ids) is phase 3 — not implemented
/// here.
pub fn route_model(model: &str, llama_ready: bool, llama_model_stem: Option<&str>) -> ModelRoute {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return ModelRoute::Unknown;
    }
    if llama_ready {
        if let Some(stem) = llama_model_stem {
            if stem == trimmed {
                return ModelRoute::Llama;
            }
        }
    }
    ModelRoute::Ollama
}

// ---------------------------------------------------------------------
// AppHandle-free request handling core
// ---------------------------------------------------------------------

/// Everything a single request needs to be routed/authenticated/served,
/// snapshotted fresh per request (cheap: one mutex lock + a couple of
/// clones — see [`build_deps`]) so llama's live port/status/model is never
/// stale mid-connection. No `AppHandle` here by design — this is what makes
/// [`handle_request`] directly unit-testable and, later, `lm-cli`-reusable.
#[derive(Clone)]
pub struct ServerDeps {
    pub llama_port: u16,
    pub llama_ready: bool,
    pub llama_model_stem: Option<String>,
    pub ollama_base_url: String,
    pub require_token: bool,
    pub token_sha256: Option<String>,
    pub client: reqwest::Client,
}

/// A decoded HTTP request. Deliberately not `hyper::Request<Incoming>`
/// itself, so [`handle_request`]'s tests (and any future CLI reuse) can
/// build one directly without a real hyper connection — [`serve_one_request`]
/// is the thin adapter that buffers a real `Incoming` body into `Bytes` and
/// builds this.
pub struct ServerRequest {
    pub method: Method,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

fn full_body(bytes: impl Into<Bytes>) -> ResponseBody {
    // `Full<Bytes>`'s `Error` is `Infallible` — map it into `BoxError` so
    // every response path shares one concrete body type.
    Full::new(bytes.into()).map_err(|never: Infallible| match never {}).boxed()
}

fn json_response(status: StatusCode, value: serde_json::Value) -> Response<ResponseBody> {
    let bytes = Bytes::from(value.to_string());
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(full_body(bytes))
        .expect("building a response from a fixed status + static header never fails")
}

/// OpenAI-shaped error body: `{"error":{"message","type","code"}}` — the
/// same envelope real OpenAI-compatible clients already parse.
fn error_response(status: StatusCode, message: &str, code: &str) -> Response<ResponseBody> {
    json_response(
        status,
        json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "code": code,
            }
        }),
    )
}

fn health_response() -> Response<ResponseBody> {
    json_response(StatusCode::OK, json!({ "status": "ok" }))
}

fn not_found_response() -> Response<ResponseBody> {
    error_response(StatusCode::NOT_FOUND, "Not Found", "not_found")
}

/// Bearer-token check for every route except `/health`. `Ok(())` means
/// authenticated (or auth is off); `Err(response)` is the exact response to
/// return immediately.
fn authenticate(deps: &ServerDeps, headers: &HeaderMap) -> Result<(), Response<ResponseBody>> {
    if !deps.require_token {
        return Ok(());
    }

    let Some(expected) = deps.token_sha256.as_deref() else {
        // No token configured at all (shouldn't happen in phase 1, where
        // `require_token` is always paired with a freshly generated token)
        // — fail closed rather than silently letting every request through.
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "No API token is configured for this server.",
            "invalid_api_key",
        ));
    };

    let provided = headers
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match provided {
        Some(token) if constant_time_eq(&sha256_hex(token), expected) => Ok(()),
        _ => Err(error_response(
            StatusCode::UNAUTHORIZED,
            "Incorrect API key provided. Find the current one in Little Monkey's Settings > API Server.",
            "invalid_api_key",
        )),
    }
}

async fn handle_models(deps: &ServerDeps) -> Response<ResponseBody> {
    let mut data = Vec::new();

    if deps.llama_ready {
        if let Some(stem) = &deps.llama_model_stem {
            data.push(json!({ "id": stem, "object": "model", "owned_by": "local" }));
        }
    }

    // `expose_ollama` hardcoded true for phase 1 (the config toggle, and
    // `expose_providers`, land in phases 2/3 per the design doc). A skipped
    // capability probe on purpose — `ollama::list_tag_names` fetches only
    // `/api/tags`, never `/api/show`, per the design doc's "Ollama model
    // listing latency" risk note.
    match ollama::list_tag_names(&deps.client).await {
        Ok(tags) => {
            for tag in tags {
                data.push(json!({ "id": tag, "object": "model", "owned_by": "ollama" }));
            }
        }
        Err(_) => {
            // Ollama being unreachable is a normal state (mirrors
            // `ollama.rs`'s own stance) — just omit its models rather than
            // failing the whole `/v1/models` call.
        }
    }

    json_response(StatusCode::OK, json!({ "object": "list", "data": data }))
}

async fn handle_chat_completions(deps: &ServerDeps, body: Bytes) -> Response<ResponseBody> {
    let parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid JSON body", "invalid_request_error"),
    };

    let model = parsed.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let stream = parsed.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    let upstream_url = match route_model(&model, deps.llama_ready, deps.llama_model_stem.as_deref()) {
        ModelRoute::Llama => format!("http://127.0.0.1:{}/v1/chat/completions", deps.llama_port),
        ModelRoute::Ollama => format!("{}/v1/chat/completions", deps.ollama_base_url),
        ModelRoute::Unknown => {
            // Mirrors OpenAI's own wording for a request with no `model`.
            return error_response(StatusCode::NOT_FOUND, "you must provide a model parameter", "model_not_found");
        }
    };

    let upstream = match deps
        .client
        .post(&upstream_url)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Failed to reach the local model server: {e}"),
                "upstream_unreachable",
            );
        }
    };

    let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = upstream
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(if stream { "text/event-stream" } else { "application/json" })
        .to_string();

    if stream {
        // Byte-level SSE passthrough: tool-call fragments, usage chunks, and
        // the `[DONE]` sentinel all survive untouched — no re-parsing.
        let byte_stream = upstream
            .bytes_stream()
            .map(|chunk| chunk.map(Frame::data).map_err(|e| Box::new(e) as BoxError));
        Response::builder()
            .status(status)
            .header(hyper::header::CONTENT_TYPE, content_type)
            .body(BodyExt::boxed(StreamBody::new(byte_stream)))
            .expect("building a streaming response from an upstream status + content-type never fails")
    } else {
        match upstream.bytes().await {
            Ok(bytes) => Response::builder()
                .status(status)
                .header(hyper::header::CONTENT_TYPE, content_type)
                .body(full_body(bytes))
                .expect("building a buffered response from an upstream status + content-type never fails"),
            Err(e) => error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Failed to read the local model server's response: {e}"),
                "upstream_error",
            ),
        }
    }
}

/// The core router: a plain `match` on method + path, no framework — see the
/// module doc's security note on why this surface must stay exactly these
/// four routes.
pub async fn handle_request(deps: &ServerDeps, req: ServerRequest) -> Response<ResponseBody> {
    let ServerRequest { method, path, headers, body } = req;

    // `/health` is the one unauthenticated route (a liveness probe has to
    // work before a caller has a token to hand it).
    if method == Method::GET && path == "/health" {
        return health_response();
    }

    if let Err(unauthorized) = authenticate(deps, &headers) {
        return unauthorized;
    }

    match (method, path.as_str()) {
        (Method::GET, "/v1/models") => handle_models(deps).await,
        (Method::POST, "/v1/chat/completions") => handle_chat_completions(deps, body).await,
        _ => not_found_response(),
    }
}

// ---------------------------------------------------------------------
// hyper accept loop (AppHandle-owning glue)
// ---------------------------------------------------------------------

/// The parts of [`ServerDeps`] that don't change for the lifetime of one
/// server run — built once in `api_server_start`, then cheaply cloned into
/// every accepted connection to assemble that connection's per-request
/// `ServerDeps` (which additionally reads `AppState::llama` live).
struct ServerRuntime {
    client: reqwest::Client,
    ollama_base_url: String,
    require_token: bool,
    token_sha256: String,
}

fn build_deps(app: &AppHandle, runtime: &ServerRuntime) -> ServerDeps {
    let state = app.state::<AppState>();
    let (llama_port, llama_ready, llama_model_stem) = {
        let llama = state.llama.lock().unwrap();
        let ready = llama.status == "ready";
        let stem = llama
            .model_path
            .as_deref()
            .and_then(|p| Path::new(p).file_stem())
            .map(|s| s.to_string_lossy().to_string());
        (llama.port, ready, stem)
    };
    ServerDeps {
        llama_port,
        llama_ready,
        llama_model_stem,
        ollama_base_url: runtime.ollama_base_url.clone(),
        require_token: runtime.require_token,
        token_sha256: Some(runtime.token_sha256.clone()),
        client: runtime.client.clone(),
    }
}

/// Buffers a real hyper request's body into a single `Bytes` (unbounded —
/// chat-completion request bodies are small JSON payloads, unlike
/// `web.rs::tool_web_fetch`'s arbitrary-page responses, which do need a cap)
/// and hands off to the `AppHandle`-free [`handle_request`] core.
async fn serve_one_request(deps: ServerDeps, req: Request<Incoming>) -> Result<Response<ResponseBody>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let headers = req.headers().clone();
    let body = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => Bytes::new(),
    };

    Ok(handle_request(&deps, ServerRequest { method, path, headers, body }).await)
}

fn bump_request_count(app: &AppHandle) {
    let state = app.state::<AppState>();
    let payload = {
        let Ok(mut s) = state.api_server.lock() else { return };
        s.request_count += 1;
        status_payload(&s)
    };
    // Emitted per-request rather than throttled — phase 1 traffic volume
    // (a human-driven external tool, not a benchmark loop) makes this a
    // non-issue; worth revisiting if that assumption changes.
    emit_status(app, &payload);
}

async fn run_accept_loop(app: AppHandle, listener: TcpListener, shutdown: Arc<Notify>, runtime: Arc<ServerRuntime>) {
    loop {
        tokio::select! {
            _ = shutdown.notified() => break,
            accepted = listener.accept() => {
                let (stream, _addr) = match accepted {
                    Ok(pair) => pair,
                    Err(_) => continue, // transient accept error — keep serving
                };
                let io = TokioIo::new(stream);
                let app_for_conn = app.clone();
                let runtime_for_conn = runtime.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req: Request<Incoming>| {
                        let app_for_req = app_for_conn.clone();
                        let deps = build_deps(&app_for_req, &runtime_for_conn);
                        async move {
                            let resp = serve_one_request(deps, req).await;
                            bump_request_count(&app_for_req);
                            resp
                        }
                    });
                    let _ = http1::Builder::new().serve_connection(io, service).await;
                });
            }
        }
    }

    if let Ok(mut s) = app.state::<AppState>().api_server.lock() {
        s.status = "stopped".to_string();
        s.shutdown = None;
    }
    if let Ok(s) = app.state::<AppState>().api_server.lock() {
        emit_status(&app, &status_payload(&s));
    }
}

/// Attempts to bind `port` on loopback only. Split out from
/// `api_server_start` so the "port already in use" failure path is directly
/// unit-testable without a `#[tauri::command]`/`AppHandle` — see
/// `tests::bind_conflict_surfaces_as_status_error`.
async fn bind_listener(port: u16) -> Result<TcpListener, String> {
    TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| format!("Failed to bind 127.0.0.1:{port} — {e}"))
}

fn record_bind_error(state: &mut ApiServerState, message: String) {
    state.status = "error".to_string();
    state.last_error = Some(message);
    state.shutdown = None;
    state.token = None;
    state.token_sha256 = None;
}

// ---------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------

/// Starts the server on `port`, stopping any previous instance first (so
/// re-Starting after an edited port field rebinds cleanly instead of
/// erroring on our own still-open old listener). A bind failure (most
/// commonly: something else already has `port`) surfaces synchronously as
/// `Err` *and* as `status: "error"` with `last_error` set — never a silent
/// no-op, never a panic.
#[tauri::command]
pub async fn api_server_start(app: AppHandle, state: State<'_, AppState>, port: u16) -> Result<ApiServerStatusPayload, String> {
    if let Ok(mut s) = state.api_server.lock() {
        if let Some(shutdown) = s.shutdown.take() {
            shutdown.notify_one();
        }
    }

    let listener = match bind_listener(port).await {
        Ok(listener) => listener,
        Err(message) => {
            let payload = {
                let mut s = state.api_server.lock().map_err(|e| e.to_string())?;
                record_bind_error(&mut s, message.clone());
                status_payload(&s)
            };
            emit_status(&app, &payload);
            return Err(message);
        }
    };

    let token = generate_token();
    let token_sha256 = sha256_hex(&token);
    let shutdown = Arc::new(Notify::new());

    let payload = {
        let mut s = state.api_server.lock().map_err(|e| e.to_string())?;
        s.shutdown = Some(shutdown.clone());
        s.port = port;
        s.status = "running".to_string();
        s.request_count = 0;
        s.last_error = None;
        s.token = Some(token.clone());
        s.token_sha256 = Some(token_sha256.clone());
        status_payload(&s)
    };
    emit_status(&app, &payload);

    // `require_token` hardcoded `true` for phase 1 — the full opt-out
    // toggle (with its explicit warning) is phase 2's `api_server.json`.
    let runtime = Arc::new(ServerRuntime {
        client: reqwest::Client::new(),
        ollama_base_url: ollama::OLLAMA_BASE_URL.to_string(),
        require_token: true,
        token_sha256,
    });

    tokio::spawn(run_accept_loop(app.clone(), listener, shutdown, runtime));

    Ok(payload)
}

/// Stops the server if running (a no-op, not an error, if it's already
/// stopped) and clears the current token from memory.
#[tauri::command]
pub fn api_server_stop(app: AppHandle, state: State<'_, AppState>) -> Result<ApiServerStatusPayload, String> {
    let payload = {
        let mut s = state.api_server.lock().map_err(|e| e.to_string())?;
        if let Some(shutdown) = s.shutdown.take() {
            shutdown.notify_one();
        }
        s.status = "stopped".to_string();
        s.token = None;
        s.token_sha256 = None;
        status_payload(&s)
    };
    emit_status(&app, &payload);
    Ok(payload)
}

/// Returns the current status snapshot — same shape as the
/// `apiserver://status` event, for the Settings panel's initial load.
#[tauri::command]
pub fn api_server_status(state: State<'_, AppState>) -> Result<ApiServerStatusPayload, String> {
    let s = state.api_server.lock().map_err(|e| e.to_string())?;
    Ok(status_payload(&s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn test_deps(ollama_base_url: String) -> ServerDeps {
        ServerDeps {
            llama_port: 8090,
            llama_ready: true,
            llama_model_stem: Some("qwen2.5-7b-instruct".to_string()),
            ollama_base_url,
            require_token: false,
            token_sha256: None,
            client: reqwest::Client::new(),
        }
    }

    fn get_request(path: &str) -> ServerRequest {
        ServerRequest {
            method: Method::GET,
            path: path.to_string(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
        }
    }

    fn post_request(path: &str, body: &str) -> ServerRequest {
        ServerRequest {
            method: Method::POST,
            path: path.to_string(),
            headers: HeaderMap::new(),
            body: Bytes::from(body.to_string()),
        }
    }

    async fn body_bytes(resp: Response<ResponseBody>) -> Bytes {
        resp.into_body().collect().await.unwrap().to_bytes()
    }

    #[test]
    fn route_model_matches_llama_exactly() {
        assert_eq!(route_model("qwen2.5-7b-instruct", true, Some("qwen2.5-7b-instruct")), ModelRoute::Llama);
    }

    #[test]
    fn route_model_falls_back_to_ollama_for_any_other_nonempty_id() {
        assert_eq!(route_model("llama3.1:8b", true, Some("qwen2.5-7b-instruct")), ModelRoute::Ollama);
        // Even when llama isn't ready, a non-empty id is assumed to be an
        // Ollama tag — Ollama is the source of truth for whether it exists.
        assert_eq!(route_model("qwen2.5-7b-instruct", false, Some("qwen2.5-7b-instruct")), ModelRoute::Ollama);
        assert_eq!(route_model("anything", true, None), ModelRoute::Ollama);
    }

    #[test]
    fn route_model_is_unknown_only_when_blank() {
        assert_eq!(route_model("", true, Some("qwen2.5-7b-instruct")), ModelRoute::Unknown);
        assert_eq!(route_model("   ", true, Some("qwen2.5-7b-instruct")), ModelRoute::Unknown);
    }

    #[test]
    fn generated_tokens_have_the_expected_shape_and_are_unique() {
        let a = generate_token();
        let b = generate_token();
        assert!(a.starts_with(TOKEN_PREFIX));
        assert_eq!(a.len(), TOKEN_PREFIX.len() + 32);
        assert!(a.chars().skip(TOKEN_PREFIX.len()).all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two generated tokens collided — RNG is broken");
    }

    #[test]
    fn sha256_hex_is_deterministic_and_constant_time_eq_agrees() {
        let digest1 = sha256_hex("lmk-abc");
        let digest2 = sha256_hex("lmk-abc");
        let digest3 = sha256_hex("lmk-different");
        assert_eq!(digest1, digest2);
        assert!(constant_time_eq(&digest1, &digest2));
        assert!(!constant_time_eq(&digest1, &digest3));
        assert!(!constant_time_eq("short", &digest1));
    }

    #[tokio::test]
    async fn health_requires_no_token_even_when_auth_is_on() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.token_sha256 = Some(sha256_hex("lmk-real-token"));

        let resp = handle_request(&deps, get_request("/health")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body_bytes(resp).await;
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["status"], "ok");
    }

    #[tokio::test]
    async fn missing_or_wrong_bearer_token_is_rejected_on_protected_routes() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.token_sha256 = Some(sha256_hex("lmk-real-token"));

        let resp = handle_request(&deps, get_request("/v1/models")).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let mut wrong_auth = get_request("/v1/models");
        wrong_auth.headers.insert(hyper::header::AUTHORIZATION, "Bearer lmk-not-it".parse().unwrap());
        let resp = handle_request(&deps, wrong_auth).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn correct_bearer_token_is_accepted() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.token_sha256 = Some(sha256_hex("lmk-real-token"));
        deps.llama_ready = false;

        let mut req = get_request("/v1/models");
        req.headers.insert(hyper::header::AUTHORIZATION, "Bearer lmk-real-token".parse().unwrap());
        let resp = handle_request(&deps, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn unmatched_routes_404() {
        let deps = test_deps("http://127.0.0.1:1".to_string());
        let resp = handle_request(&deps, get_request("/v1/embeddings")).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        // Never, ever a tool-dispatch route.
        let resp = handle_request(&deps, post_request("/v1/tool_run_shell", "{}")).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn chat_completions_with_blank_model_returns_404() {
        let deps = test_deps("http://127.0.0.1:1".to_string());
        let resp = handle_request(&deps, post_request("/v1/chat/completions", r#"{"messages":[]}"#)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn chat_completions_with_invalid_json_returns_400() {
        let deps = test_deps("http://127.0.0.1:1".to_string());
        let resp = handle_request(&deps, post_request("/v1/chat/completions", "not json")).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn models_endpoint_lists_the_ready_llama_model_as_local() {
        // Point "Ollama" at an address nothing listens on — `/v1/models`
        // must still succeed, just omitting Ollama's models (mirrors
        // `ollama.rs`'s own "unreachable is normal" stance).
        let deps = test_deps("http://127.0.0.1:1".to_string());
        let resp = handle_request(&deps, get_request("/v1/models")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body_bytes(resp).await;
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let data = value["data"].as_array().unwrap();
        assert!(data.iter().any(|m| m["id"] == "qwen2.5-7b-instruct" && m["owned_by"] == "local"));
    }

    /// Spins up a bare-bones raw-TCP "upstream" that writes back a fixed
    /// SSE response verbatim, then asserts `handle_request`'s streaming path
    /// reproduces those exact bytes with no re-framing/mutation — the
    /// design doc's "SSE passthrough fidelity" risk, exercised end to end
    /// through the real `StreamBody`/`Frame::data` plumbing.
    #[tokio::test]
    async fn sse_streaming_passthrough_preserves_upstream_bytes_exactly() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let canned: &[u8] = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";

        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    canned.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(canned);
            }
        });

        let deps = test_deps(format!("http://{addr}"));
        let resp = handle_request(&deps, post_request("/v1/chat/completions", r#"{"model":"llama3.1:8b","stream":true}"#)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body_bytes(resp).await;
        assert_eq!(&bytes[..], canned);

        handle.join().unwrap();
    }

    #[tokio::test]
    async fn chat_completions_502s_when_upstream_is_unreachable() {
        // Port 1 on loopback: nothing listens there, so the connection is
        // refused immediately — a deterministic "unreachable upstream".
        let deps = test_deps("http://127.0.0.1:1".to_string());
        let resp = handle_request(&deps, post_request("/v1/chat/completions", r#"{"model":"llama3.1:8b"}"#)).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn bind_conflict_surfaces_as_status_error_not_panic() {
        let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = blocker.local_addr().unwrap().port();

        let mut state = ApiServerState::default();
        match bind_listener(port).await {
            Ok(_) => panic!("expected a bind conflict against an already-bound port"),
            Err(message) => record_bind_error(&mut state, message),
        }

        assert_eq!(state.status, "error");
        assert!(state.last_error.is_some());
        assert!(state.shutdown.is_none());
        assert!(state.token.is_none());
    }

    #[tokio::test]
    async fn bind_listener_succeeds_on_an_available_port() {
        let listener = bind_listener(0).await.expect("binding port 0 (OS-assigned) should always succeed");
        assert!(listener.local_addr().unwrap().port() > 0);
    }
}
