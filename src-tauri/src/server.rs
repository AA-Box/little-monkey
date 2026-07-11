//! Local OpenAI-compatible API server (phases 1-3 of the local-model-hub
//! roadmap item — see `docs/roadmap/p1-local-api-server.md`).
//!
//! This is a *routing reverse proxy*, not a new inference engine: it runs a
//! small hyper-1 HTTP server on a tokio task, bound to `127.0.0.1` only (no
//! LAN bind — that's an explicitly later, separately-gated phase), and
//! exposes exactly five routes:
//!
//!   - `GET  /health`              — unauthenticated liveness probe
//!   - `GET  /v1/models`           — merged list of servable models
//!   - `POST /v1/chat/completions` — proxies to llama-server, Ollama, or a
//!                                   keychain-configured cloud provider
//!   - `POST /v1/embeddings`       — proxies to Ollama, or to llama-server
//!                                   only if it was started with `--embeddings`
//!   - `OPTIONS /v1/*`             — CORS preflight (unauthenticated)
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
//! thin `#[tauri::command]` layer that owns the actual listening socket and
//! `AppState` bookkeeping. `pub` (not `mod`) so a future `lm-cli` `api-serve`
//! subcommand (design doc phase 4) can reuse [`handle_request`] directly,
//! the same reasoning as `web`/`prompts`/`rules` above it in `lib.rs`.
//!
//! Auth (phase 2): a full multi-token model. Every [`TokenEntry`] is
//! generated as `lmk-` + 32 hex chars, shown in plaintext exactly once at
//! creation time (the command's return value), and persisted to
//! `api_server.json` as a SHA-256 digest only — the plaintext never touches
//! disk. Each token carries its own [`Scope`] (which routes it may call) and
//! [`Backend`] (which upstream it may be routed to) restrictions, enforced
//! per request in [`handle_models`]/[`handle_chat_completions`]/
//! [`handle_embeddings`] — critically, a token missing `Backend::Providers`
//! is rejected with `403` before a cloud-routed request is ever sent, even
//! when `expose_providers` is globally on (the global toggle and the
//! per-token scope are two independent gates, both must pass).
//!
//! Cloud provider routing (phase 3): `"{provider_id}/{model_id}"` ids route
//! through `providers::read_key` (keychain) + `providers::resolve_base_url`/
//! `providers::providers_list_presets`/`providers::read_custom_providers`
//! (base URL resolution) — reused verbatim, unmodified. One deviation from
//! the design doc's "drive `build_chat_request` verbatim" wording: that
//! helper reconstructs a narrow messages/tools/effort-only request body and
//! always forces `stream: true`, tailored to its one existing caller (the
//! app's own internal streaming chat proxy in `providers.rs`). An external
//! OpenAI-compatible client hitting this server sends an already-complete
//! OpenAI-shaped body — including its own `stream: false`/`true` choice and
//! whatever extra sampling parameters it wants — and dropping those to
//! reuse `build_chat_request`'s fixed shape would silently break both.
//! [`handle_chat_completions`] instead forwards the caller's body verbatim
//! (only rewriting the `model` field from `"{provider_id}/{model_id}"` to
//! the bare `model_id`) and reuses `providers::add_anthropic_headers` — a
//! small helper factored out of `build_chat_request`'s/`fetch_models`'s
//! previously-duplicated x-api-key/anthropic-version header logic — for the
//! same Anthropic quirk. See [`ModelRoute::Providers`] and the comment at
//! its construction site in [`handle_chat_completions`] for the full
//! reasoning.
//!
//! Hot vs. restart-gated config: [`ServerRuntime`] snapshots `require_token`/
//! `expose_ollama`/`expose_providers`/the bound port once, at
//! `api_server_start` time — [`api_server_set_config`] always restarts the
//! running server so those scalars can never silently drift from the
//! listening socket's actual behavior (the design doc calls this out
//! explicitly). The *token list*, by contrast, is re-read from
//! `api_server.json` fresh on every single request (see [`build_deps`]) —
//! creating or revoking a token must take effect immediately, without
//! forcing a restart that would drop every other in-flight connection just
//! to pick up one credential change.
use std::convert::Infallible;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use uuid::Uuid;

use crate::{ollama, providers, AppState};

/// LM Studio-compatible default port, so drop-in clients that hardcode 1234
/// need no configuration at all.
const DEFAULT_PORT: u16 = 1234;

/// Prefix on every generated bearer token, so a leaked token is
/// self-describing (`lmk-<32 hex chars>`) the same way e.g. GitHub's
/// `ghp_`/OpenAI's `sk-` prefixes are.
const TOKEN_PREFIX: &str = "lmk-";

/// Filename for the persisted server config under the app data directory —
/// same file-per-feature pattern as `providers.json`/`web_settings.json`.
const CONFIG_FILE: &str = "api_server.json";

/// Boxed error type for [`ResponseBody`] — reqwest's streaming errors and
/// our own infallible bodies both erase to this.
type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Unified response body type: either a fully-buffered JSON payload ([`Full`])
/// or an SSE byte-stream passthrough ([`StreamBody`]), both boxed so
/// [`handle_request`] can return one concrete type regardless of which route
/// it took.
type ResponseBody = BoxBody<Bytes, BoxError>;

/// In-memory lifecycle state for the managed API server process — mirrors
/// `llama::LlamaState` field-for-field. No token material lives here as of
/// phase 2 (that was a phase-1-only stopgap before `api_server.json`
/// existed) — tokens are minted/revoked/listed via their own commands below.
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
    /// Epoch milliseconds of the most recently completed request, or `None`
    /// if the server hasn't served one yet since it last started (reset to
    /// `None` on every `start`, same as `request_count` resetting to `0` —
    /// see [`start_server_core`]). Phase 4 addition: the design doc's
    /// "request counter/last-request display in the panel" parity item —
    /// `request_count` itself was already wired end to end since phase 1,
    /// this is the other half.
    pub last_request_at: Option<u64>,
    pub last_error: Option<String>,
}

impl Default for ApiServerState {
    fn default() -> Self {
        ApiServerState {
            shutdown: None,
            port: DEFAULT_PORT,
            status: "stopped".to_string(),
            request_count: 0,
            last_request_at: None,
            last_error: None,
        }
    }
}

/// Snapshot broadcast on `apiserver://status` and returned by
/// `api_server_start`/`_stop`/`_status` — mirrors `llama::emit_status`'s
/// payload shape.
#[derive(Debug, Clone, Serialize)]
pub struct ApiServerStatusPayload {
    pub status: String,
    pub port: u16,
    pub request_count: u64,
    pub last_request_at: Option<u64>,
    pub last_error: Option<String>,
}

fn status_payload(state: &ApiServerState) -> ApiServerStatusPayload {
    ApiServerStatusPayload {
        status: state.status.clone(),
        port: state.port,
        request_count: state.request_count,
        last_request_at: state.last_request_at,
        last_error: state.last_error.clone(),
    }
}

/// Emit an `apiserver://status` event to all windows, mirroring
/// `llama.rs`'s `emit_status`/`ollama.rs`'s `emit_status` convention.
fn emit_status(app: &AppHandle, payload: &ApiServerStatusPayload) {
    let _ = app.emit("apiserver://status", payload.clone());
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
/// early-exit timing differences.
///
/// Note on *what* needs to be constant-time here: [`authenticate`] never
/// compares the raw secret token byte-for-byte — it always hashes the
/// incoming bearer first (`sha256_hex`) and compares *digests*. A digest
/// compare's timing genuinely doesn't leak anything useful about the
/// pre-image: SHA-256's avalanche effect means an attacker who learns "the
/// first byte of the stored digest matches" gains no information about which
/// tokens might hash to it (finding one is exactly as hard as finding any
/// other preimage). So a naive `==` on the digests would already be safe in
/// practice. This function exists anyway because (a) it's essentially free —
/// two 64-byte compares — and (b) it keeps the invariant "every credential
/// compare in this file is constant-time" trivially true by inspection,
/// which is worth more than the few nanoseconds it costs, especially since a
/// future change here is exactly the kind of thing that's easy to get wrong
/// under review pressure. What would matter for real is if this ever
/// compared *unhashed* tokens directly — it must not start doing that.
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
// Token + config data model (persisted to `api_server.json`)
// ---------------------------------------------------------------------

/// Which routes a [`TokenEntry`] may call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Chat,
    Models,
    Embeddings,
}

/// Which upstream a [`TokenEntry`] may be routed to. Mirrors [`ModelRoute`]
/// one level up — see [`route_backend`] for the mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Local,
    Ollama,
    Providers,
}

fn backend_label(backend: Backend) -> &'static str {
    match backend {
        Backend::Local => "local",
        Backend::Ollama => "ollama",
        Backend::Providers => "providers",
    }
}

/// A single persisted bearer token. `sha256` is the only trace of the
/// secret that ever reaches disk — see [`mint_token`]. `#[serde(default)]`
/// throughout for the same hand-edited-file leniency as `CustomProviderEntry`/
/// `WebSettings`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub scopes: Vec<Scope>,
    #[serde(default)]
    pub backends: Vec<Backend>,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub last_used_at: Option<u64>,
}

/// Frontend-facing view of a [`TokenEntry`] with the digest stripped —
/// `api_server_list_tokens` never sends a hash to the WebView, even though
/// it's not the plaintext, on general "secrets stay on the Rust side"
/// principle.
#[derive(Debug, Clone, Serialize)]
pub struct TokenEntryView {
    pub id: String,
    pub label: String,
    pub scopes: Vec<Scope>,
    pub backends: Vec<Backend>,
    pub created_at: u64,
    pub last_used_at: Option<u64>,
}

impl From<&TokenEntry> for TokenEntryView {
    fn from(entry: &TokenEntry) -> Self {
        TokenEntryView {
            id: entry.id.clone(),
            label: entry.label.clone(),
            scopes: entry.scopes.clone(),
            backends: entry.backends.clone(),
            created_at: entry.created_at,
            last_used_at: entry.last_used_at,
        }
    }
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_true() -> bool {
    true
}

/// Persisted at `<app_data>/api_server.json`. Plain snake_case field names,
/// no `serde(rename)` — same hand-editable-file convention as
/// `providers.json`/`web_settings.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiServerConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default = "default_true")]
    pub require_token: bool,
    #[serde(default = "default_true")]
    pub expose_ollama: bool,
    #[serde(default)]
    pub expose_providers: bool,
    #[serde(default)]
    pub tokens: Vec<TokenEntry>,
}

impl Default for ApiServerConfig {
    fn default() -> Self {
        ApiServerConfig {
            port: DEFAULT_PORT,
            autostart: false,
            require_token: true,
            expose_ollama: true,
            expose_providers: false,
            tokens: Vec::new(),
        }
    }
}

/// The subset of [`ApiServerConfig`] the Settings panel gets/sets directly —
/// deliberately excludes `tokens` (managed via its own create/revoke/list
/// commands so the frontend never round-trips a whole token list, digests
/// included, just to flip a checkbox).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiServerConfigView {
    pub port: u16,
    pub autostart: bool,
    pub require_token: bool,
    pub expose_ollama: bool,
    pub expose_providers: bool,
}

impl From<&ApiServerConfig> for ApiServerConfigView {
    fn from(config: &ApiServerConfig) -> Self {
        ApiServerConfigView {
            port: config.port,
            autostart: config.autostart,
            require_token: config.require_token,
            expose_ollama: config.expose_ollama,
            expose_providers: config.expose_providers,
        }
    }
}

/// Resolves (and creates, if missing) `<app_data_dir>/api_server.json`'s
/// path — same shape as `providers.rs::providers_file_path`/
/// `web.rs::settings_file_path`. `pub` so a future `lm-cli` `api-serve`
/// subcommand (phase 4) can resolve the same path with its own
/// APP_IDENTIFIER, the same config-drift concern the design doc flags.
pub fn config_file_path(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data directory: {e}"))?;
    if !base.exists() {
        std::fs::create_dir_all(&base)
            .map_err(|e| format!("Failed to create app data directory {}: {e}", base.display()))?;
    }
    Ok(base.join(CONFIG_FILE))
}

/// Core load logic, parameterized by path so it needs no `AppHandle` —
/// directly unit-testable and reusable from `lm-cli`. A missing file (the
/// common case — nothing configured yet) is simply [`ApiServerConfig::default`],
/// never an error, same stance as `web.rs::load_settings_impl`.
pub fn load_config_impl(path: &Path) -> Result<ApiServerConfig, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(|e| format!("Corrupt api_server.json: {e}")),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(ApiServerConfig::default()),
        Err(e) => Err(format!("Failed to read api_server.json: {e}")),
    }
}

/// Core save logic: atomic sibling temp file + rename, same idiom as
/// `sessions.rs`'s `save_to` / `web.rs`'s `save_settings_impl`.
pub fn save_config_impl(path: &Path, config: &ApiServerConfig) -> Result<(), String> {
    let payload =
        serde_json::to_string_pretty(config).map_err(|e| format!("Failed to serialize api_server.json: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &payload).map_err(|e| format!("Failed to write api_server.json: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("Failed to finalize api_server.json: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------
// Model-id routing (pure, no I/O — directly unit-testable)
// ---------------------------------------------------------------------

/// Which upstream a `POST /v1/chat/completions` (or `/v1/embeddings`)
/// request's `model` field should be routed to. `Providers` carries the
/// split-out `"{provider_id}/{model_id}"` halves so the handler never has
/// to re-parse the original id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelRoute {
    Llama,
    Ollama,
    Providers { provider_id: String, model_id: String },
    Unknown,
}

/// A configured cloud provider's routing-relevant fields — `id` (used to
/// match a `"{provider_id}/..."` model id and to key `providers::read_key`)
/// and `base_url` (where its `/chat/completions`/`/models` live). Built once
/// per request from `providers::providers_list_presets` + the app's
/// `providers.json` custom list — see [`build_provider_catalog`]. Never
/// carries a key or a `has_key` probe: whether a provider is *usable* is
/// decided lazily via `providers::read_key` at the point of use, not here —
/// same "routing decision is separate from credential availability" stance
/// [`route_model`]'s doc comment already takes for Ollama tags.
#[derive(Debug, Clone)]
pub struct ProviderSummary {
    pub id: String,
    pub base_url: String,
}

/// Pure routing decision, split out from [`handle_chat_completions`] so it's
/// directly unit-testable with no I/O — same `*_impl`-style extraction as
/// `web.rs::validate_fetch_url`. `model` is `Unknown` only when blank
/// (missing/empty `model` field); a non-empty id that isn't the exact
/// ready-llama filename stem and isn't `"{known_provider_id}/..."` is always
/// assumed to be an Ollama tag — Ollama itself is the source of truth for
/// whether that tag actually exists, and a wrong guess there surfaces as a
/// `502` wrapping Ollama's own error rather than a possibly-stale local
/// "unknown model" guess. Whether a provider actually has a key saved is
/// deliberately not checked here (that's `handle_chat_completions`'
/// `providers::read_key` call, at the point the request would actually be
/// sent) — this function only decides which *upstream* a request targets,
/// mirroring how it never checks Ollama reachability either.
pub fn route_model(
    model: &str,
    llama_ready: bool,
    llama_model_stem: Option<&str>,
    known_providers: &[ProviderSummary],
) -> ModelRoute {
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
    if let Some((provider_id, model_id)) = trimmed.split_once('/') {
        if known_providers.iter().any(|p| p.id == provider_id) {
            return ModelRoute::Providers {
                provider_id: provider_id.to_string(),
                model_id: model_id.to_string(),
            };
        }
    }
    ModelRoute::Ollama
}

/// Maps a routing decision to the [`Backend`] a token's `backends` list is
/// checked against — `Unknown` never reaches here (handled earlier as a
/// 404).
fn route_backend(route: &ModelRoute) -> Option<Backend> {
    match route {
        ModelRoute::Llama => Some(Backend::Local),
        ModelRoute::Ollama => Some(Backend::Ollama),
        ModelRoute::Providers { .. } => Some(Backend::Providers),
        ModelRoute::Unknown => None,
    }
}

// ---------------------------------------------------------------------
// AppHandle-free request handling core
// ---------------------------------------------------------------------

/// A token that matched on the current request, stripped down to just what
/// route handlers need to enforce scope/backend restrictions — returned by
/// [`authenticate`].
#[derive(Clone)]
struct TokenAuth {
    id: String,
    scopes: Vec<Scope>,
    backends: Vec<Backend>,
}

/// The subset of a [`TokenEntry`] needed to authenticate one request —
/// deliberately not the full struct (no `label`/`created_at`), assembled
/// fresh from `api_server.json` in [`build_deps`] on every request so a
/// revoked token stops working immediately without a server restart.
#[derive(Clone)]
struct StoredToken {
    id: String,
    sha256: String,
    scopes: Vec<Scope>,
    backends: Vec<Backend>,
}

/// Everything a single request needs to be routed/authenticated/served,
/// snapshotted fresh per request (cheap: one mutex lock, a couple of clones,
/// and a small JSON file read — see [`build_deps`]) so llama's live
/// port/status/model *and* the current token list are never stale mid-
/// connection. No `AppHandle` here by design — this is what makes
/// [`handle_request`] directly unit-testable and, later, `lm-cli`-reusable.
#[derive(Clone)]
pub struct ServerDeps {
    pub llama_port: u16,
    pub llama_ready: bool,
    pub llama_model_stem: Option<String>,
    /// Whether the currently-running llama-server process was launched with
    /// `--embeddings` (`llama::LlamaState::embeddings_enabled`) — gates
    /// `POST /v1/embeddings` routing to it (see [`handle_embeddings`]).
    pub llama_embeddings_enabled: bool,
    pub ollama_base_url: String,
    pub require_token: bool,
    pub expose_ollama: bool,
    pub expose_providers: bool,
    /// Configured cloud providers (presets + custom), for model-id routing
    /// and `GET /v1/models` — loaded fresh per request (cheap: one static
    /// slice + one small JSON file read) in [`build_deps`], same "never
    /// stale" reasoning as `tokens` below. Populated regardless of
    /// `expose_providers` so routing decisions stay consistent whether or
    /// not the toggle is on (see [`route_model`]'s doc comment) — the toggle
    /// only gates whether a `Providers` route is actually served.
    pub providers: Vec<ProviderSummary>,
    tokens: Vec<StoredToken>,
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
        .header(header::CONTENT_TYPE, "application/json")
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

fn forbidden_response(message: &str) -> Response<ResponseBody> {
    error_response(StatusCode::FORBIDDEN, message, "insufficient_scope")
}

fn health_response() -> Response<ResponseBody> {
    json_response(StatusCode::OK, json!({ "status": "ok" }))
}

fn not_found_response() -> Response<ResponseBody> {
    error_response(StatusCode::NOT_FOUND, "Not Found", "not_found")
}

/// A bare `204` with the CORS headers a preflight `OPTIONS /v1/*` needs —
/// browser-based clients (a primary consumer per the design doc) can't even
/// send the real request otherwise.
fn cors_preflight_response() -> Response<ResponseBody> {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"))
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST, OPTIONS"))
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("Content-Type, Authorization"))
        .body(full_body(Bytes::new()))
        .expect("building a fixed-shape preflight response never fails")
}

/// Stamps `Access-Control-Allow-Origin: *` onto every response this server
/// returns (not just `/v1/*`) — a browser-based client fetching `/health`
/// benefits too, and there's nothing origin-sensitive being protected here
/// (the bearer token, not the browser's same-origin policy, is the actual
/// gate).
fn with_cors(mut resp: Response<ResponseBody>) -> Response<ResponseBody> {
    resp.headers_mut()
        .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    resp
}

/// Bearer-token check for every route except `/health`/`OPTIONS` preflight.
/// `Ok(None)` means the request may proceed with no further scope/backend
/// restriction (either `require_token` is off, or — not modeled today —
/// a future "admin" token type with no restrictions at all). `Ok(Some(auth))`
/// means the request is authenticated as that specific token, and route
/// handlers must still check `auth.scopes`/`auth.backends`. `Err(response)`
/// is the exact response to return immediately.
fn authenticate(deps: &ServerDeps, headers: &HeaderMap) -> Result<Option<TokenAuth>, Response<ResponseBody>> {
    if !deps.require_token {
        return Ok(None);
    }

    if deps.tokens.is_empty() {
        // Fail closed rather than silently letting every request through —
        // `require_token` is on but nothing's been minted yet.
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "No API tokens are configured for this server. Create one in Settings > API Server.",
            "invalid_api_key",
        ));
    }

    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let Some(token) = provided else {
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "Missing bearer token. Find or create one in Little Monkey's Settings > API Server.",
            "invalid_api_key",
        ));
    };

    let digest = sha256_hex(token);
    for stored in &deps.tokens {
        if constant_time_eq(&digest, &stored.sha256) {
            return Ok(Some(TokenAuth {
                id: stored.id.clone(),
                scopes: stored.scopes.clone(),
                backends: stored.backends.clone(),
            }));
        }
    }

    Err(error_response(
        StatusCode::UNAUTHORIZED,
        "Incorrect API key provided. Find the current one in Little Monkey's Settings > API Server.",
        "invalid_api_key",
    ))
}

async fn handle_models(deps: &ServerDeps, authed: Option<&TokenAuth>) -> Response<ResponseBody> {
    if let Some(auth) = authed {
        if !auth.scopes.contains(&Scope::Models) {
            return forbidden_response("This token isn't scoped for `models`.");
        }
    }

    let mut data = Vec::new();

    if deps.llama_ready {
        if let Some(stem) = &deps.llama_model_stem {
            data.push(json!({ "id": stem, "object": "model", "owned_by": "local" }));
        }
    }

    // A skipped capability probe on purpose — `ollama::list_tag_names`
    // fetches only `/api/tags`, never `/api/show`, per the design doc's
    // "Ollama model listing latency" risk note. Gated behind the config's
    // `expose_ollama` toggle: when it's off, `/v1/models` must only ever
    // advertise what will actually serve — see the design doc's "Jan
    // pitfall to avoid" note.
    if deps.expose_ollama {
        match ollama::list_tag_names(&deps.client).await {
            Ok(tags) => {
                for tag in tags {
                    data.push(json!({ "id": tag, "object": "model", "owned_by": "ollama" }));
                }
            }
            Err(_) => {
                // Ollama being unreachable is a normal state (mirrors
                // `ollama.rs`'s own stance) — just omit its models rather
                // than failing the whole `/v1/models` call.
            }
        }
    }

    // Cloud provider models, only when the money-spending switch is on (see
    // the design doc's Msty-reference note) — `owned_by` is set to the
    // provider id itself (e.g. "openai", "anthropic") rather than "local"/
    // "ollama", so a client immediately sees which entries in the merged
    // list can incur billing. A provider with no key saved (`read_key` err)
    // or unreachable (`fetch_models` err) is just omitted, same
    // "unreachable is normal, don't fail the whole list" stance as Ollama
    // above — a misconfigured provider shouldn't take `/v1/models` down for
    // every other backend.
    if deps.expose_providers {
        for provider in &deps.providers {
            let Ok(api_key) = providers::read_key(&provider.id) else { continue };
            if let Ok(models) = providers::fetch_models(&provider.base_url, &provider.id, &api_key).await {
                for model in models {
                    data.push(json!({
                        "id": format!("{}/{}", provider.id, model.id),
                        "object": "model",
                        "owned_by": provider.id,
                    }));
                }
            }
        }
    }

    json_response(StatusCode::OK, json!({ "object": "list", "data": data }))
}

async fn handle_chat_completions(deps: &ServerDeps, authed: Option<&TokenAuth>, body: Bytes) -> Response<ResponseBody> {
    if let Some(auth) = authed {
        if !auth.scopes.contains(&Scope::Chat) {
            return forbidden_response("This token isn't scoped for `chat`.");
        }
    }

    let parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid JSON body", "invalid_request_error"),
    };

    let model = parsed.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let stream = parsed.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    let route = route_model(&model, deps.llama_ready, deps.llama_model_stem.as_deref(), &deps.providers);

    if route == ModelRoute::Unknown {
        // Mirrors OpenAI's own wording for a request with no `model`.
        return error_response(StatusCode::NOT_FOUND, "you must provide a model parameter", "model_not_found");
    }

    // Same "only advertise/serve what's actually exposed" stance as
    // `handle_models` — an Ollama- or provider-routed id must 404 exactly
    // like an unknown one when its toggle is off, not silently proxy anyway.
    let backend_disabled = match &route {
        ModelRoute::Ollama => !deps.expose_ollama,
        ModelRoute::Providers { .. } => !deps.expose_providers,
        ModelRoute::Llama | ModelRoute::Unknown => false,
    };
    if backend_disabled {
        return error_response(StatusCode::NOT_FOUND, &format!("Unknown model '{model}'"), "model_not_found");
    }

    if let Some(auth) = authed {
        if let Some(backend) = route_backend(&route) {
            if !auth.backends.contains(&backend) {
                return forbidden_response(&format!(
                    "This token isn't scoped for the '{}' backend.",
                    backend_label(backend)
                ));
            }
        }
    }

    let request_builder = match &route {
        ModelRoute::Llama => deps
            .client
            .post(format!("http://127.0.0.1:{}/v1/chat/completions", deps.llama_port))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body),
        ModelRoute::Ollama => deps
            .client
            .post(format!("{}/v1/chat/completions", deps.ollama_base_url))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body),
        ModelRoute::Providers { provider_id, model_id } => {
            // `provider_id` is guaranteed to match an entry in
            // `deps.providers` — `route_model` only ever produces this
            // variant for a known provider id — but a defensive `NOT_FOUND`
            // beats an `unwrap` panic if that invariant is ever broken.
            let Some(base_url) = deps.providers.iter().find(|p| &p.id == provider_id).map(|p| p.base_url.clone()) else {
                return error_response(StatusCode::NOT_FOUND, &format!("Unknown model '{model}'"), "model_not_found");
            };
            let api_key = match providers::read_key(provider_id) {
                Ok(key) => key,
                Err(e) => return error_response(StatusCode::BAD_GATEWAY, &e, "provider_not_configured"),
            };
            // Forward the caller's own OpenAI-shaped body verbatim (their
            // `stream`/`temperature`/etc. survive untouched) — only the
            // `model` field is rewritten from `"{provider_id}/{model_id}"`
            // to the bare `model_id` the provider itself expects.
            // Deliberately NOT `providers::build_chat_request`: that helper
            // reconstructs a narrow messages/tools/effort-only body and
            // always forces `stream: true`, which fits its one existing
            // caller (the app's own internal streaming chat proxy) but would
            // silently drop an external caller's other fields and break
            // `stream: false` requests here — a documented deviation from
            // the design doc's "reuse build_chat_request verbatim" wording
            // (see this module's doc comment). `providers::read_key` and
            // `providers::add_anthropic_headers` (the x-api-key/
            // anthropic-version quirk `build_chat_request` also uses) are
            // reused verbatim, unmodified.
            let mut outgoing = parsed.clone();
            outgoing["model"] = json!(model_id);
            let request = deps.client.post(format!("{base_url}/chat/completions")).bearer_auth(&api_key).json(&outgoing);
            providers::add_anthropic_headers(request, provider_id, &api_key)
        }
        ModelRoute::Unknown => unreachable!("handled above"),
    };

    let upstream = match request_builder.send().await {
        Ok(resp) => resp,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Failed to reach the upstream model server: {e}"),
                "upstream_unreachable",
            );
        }
    };

    let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
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
            .header(header::CONTENT_TYPE, content_type)
            .body(BodyExt::boxed(StreamBody::new(byte_stream)))
            .expect("building a streaming response from an upstream status + content-type never fails")
    } else {
        match upstream.bytes().await {
            Ok(bytes) => Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, content_type)
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

/// `POST /v1/embeddings` — proxies to Ollama's OpenAI-compatible
/// `/v1/embeddings` for Ollama tags (when `expose_ollama`), or to
/// llama-server for the ready local model *only* if it was actually started
/// with `--embeddings` (`deps.llama_embeddings_enabled`) — routing there
/// otherwise would just surface llama-server's own less-clear error, so this
/// returns a `501` up front instead, per the design doc. Cloud-provider
/// embeddings aren't implemented (out of scope for this phase per the
/// design doc's endpoint description, which only calls out Ollama +
/// llama-server for `/v1/embeddings`) — a `"{provider_id}/{model_id}"` id
/// here also `501`s rather than silently 404ing, so a caller can tell "not
/// supported yet" apart from "unknown model". Not streamed — embeddings
/// responses are always a single buffered JSON payload, unlike chat
/// completions.
async fn handle_embeddings(deps: &ServerDeps, authed: Option<&TokenAuth>, body: Bytes) -> Response<ResponseBody> {
    if let Some(auth) = authed {
        if !auth.scopes.contains(&Scope::Embeddings) {
            return forbidden_response("This token isn't scoped for `embeddings`.");
        }
    }

    let parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid JSON body", "invalid_request_error"),
    };
    let model = parsed.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();

    let route = route_model(&model, deps.llama_ready, deps.llama_model_stem.as_deref(), &deps.providers);

    if route == ModelRoute::Unknown {
        return error_response(StatusCode::NOT_FOUND, "you must provide a model parameter", "model_not_found");
    }

    if let ModelRoute::Providers { .. } = &route {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "Embeddings via a cloud provider aren't supported yet — use a local llama-server model (started with --embeddings) or an Ollama tag.",
            "embeddings_not_supported",
        );
    }

    if route == ModelRoute::Ollama && !deps.expose_ollama {
        return error_response(StatusCode::NOT_FOUND, &format!("Unknown model '{model}'"), "model_not_found");
    }

    if route == ModelRoute::Llama && !deps.llama_embeddings_enabled {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            "This model wasn't started with embeddings support. Restart it with the \"Start with embeddings support\" option checked in the Models panel.",
            "embeddings_not_enabled",
        );
    }

    if let Some(auth) = authed {
        if let Some(backend) = route_backend(&route) {
            if !auth.backends.contains(&backend) {
                return forbidden_response(&format!(
                    "This token isn't scoped for the '{}' backend.",
                    backend_label(backend)
                ));
            }
        }
    }

    let upstream_url = match route {
        ModelRoute::Llama => format!("http://127.0.0.1:{}/v1/embeddings", deps.llama_port),
        ModelRoute::Ollama => format!("{}/v1/embeddings", deps.ollama_base_url),
        ModelRoute::Providers { .. } | ModelRoute::Unknown => unreachable!("handled above"),
    };

    let upstream = match deps
        .client
        .post(&upstream_url)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Failed to reach the upstream model server: {e}"),
                "upstream_unreachable",
            );
        }
    };

    let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    match upstream.bytes().await {
        Ok(bytes) => Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(full_body(bytes))
            .expect("building a buffered response from an upstream status + fixed content-type never fails"),
        Err(e) => error_response(
            StatusCode::BAD_GATEWAY,
            &format!("Failed to read the upstream model server's response: {e}"),
            "upstream_error",
        ),
    }
}

/// The core router: a plain `match` on method + path, no framework — see the
/// module doc's security note on why this surface must stay exactly these
/// five routes. Returns the response to send *and*, when a token
/// successfully authenticated the request (whether or not it then passed
/// its scope/backend checks), that token's id — [`serve_one_request`]'s
/// caller uses this to bump `last_used_at` without `handle_request` itself
/// needing any I/O or `AppHandle`.
pub async fn handle_request(deps: &ServerDeps, req: ServerRequest) -> (Response<ResponseBody>, Option<String>) {
    let ServerRequest { method, path, headers, body } = req;

    // `/health` is the one unauthenticated route (a liveness probe has to
    // work before a caller has a token to hand it).
    if method == Method::GET && path == "/health" {
        return (with_cors(health_response()), None);
    }

    // CORS preflight never carries a bearer token — browsers deliberately
    // don't attach one to an `OPTIONS` request.
    if method == Method::OPTIONS && path.starts_with("/v1/") {
        return (cors_preflight_response(), None);
    }

    let authed = match authenticate(deps, &headers) {
        Ok(authed) => authed,
        Err(response) => return (with_cors(response), None),
    };
    let matched_token_id = authed.as_ref().map(|a| a.id.clone());

    let response = match (method, path.as_str()) {
        (Method::GET, "/v1/models") => handle_models(deps, authed.as_ref()).await,
        (Method::POST, "/v1/chat/completions") => handle_chat_completions(deps, authed.as_ref(), body).await,
        (Method::POST, "/v1/embeddings") => handle_embeddings(deps, authed.as_ref(), body).await,
        _ => not_found_response(),
    };

    (with_cors(response), matched_token_id)
}

// ---------------------------------------------------------------------
// hyper accept loop (AppHandle-owning glue)
// ---------------------------------------------------------------------

/// The parts of [`ServerDeps`] that don't change for the lifetime of one
/// server run — built once in [`start_server_core`], then cheaply cloned
/// into every accepted connection to assemble that connection's per-request
/// `ServerDeps` (which additionally reads `AppState::llama`'s live status
/// and `api_server.json`'s live token list — see [`build_deps`]).
struct ServerRuntime {
    client: reqwest::Client,
    ollama_base_url: String,
    require_token: bool,
    expose_ollama: bool,
    expose_providers: bool,
}

/// Pure merge of built-in presets + a caller-supplied custom-provider list
/// into the routing catalog — split out from [`build_provider_catalog`] so
/// `lm-cli`'s `api-serve` subcommand (design doc phase 4) can build the same
/// catalog from its own `AppHandle`-free custom-provider loader
/// (`providers_cli::load_custom_providers`) without duplicating the
/// preset+custom merge logic. See [`run_cli_server`].
fn provider_catalog_from(custom: Vec<providers::CustomProviderEntry>) -> Vec<ProviderSummary> {
    let mut out: Vec<ProviderSummary> = providers::providers_list_presets()
        .into_iter()
        .map(|p| ProviderSummary { id: p.id, base_url: p.base_url })
        .collect();
    out.extend(custom.into_iter().map(|c| ProviderSummary { id: c.id, base_url: c.base_url }));
    out
}

/// Builds the routing catalog of configured cloud providers (built-in
/// presets + custom OpenAI-compatible endpoints from `providers.json`) —
/// `providers::providers_list_presets`/`providers::read_custom_providers`
/// reused verbatim. A failure to read `providers.json` (corrupt file,
/// permissions) just omits the custom entries rather than failing the
/// whole request, same "best-effort, don't take routing down" stance as
/// [`build_deps`]'s token load.
fn build_provider_catalog(app: &AppHandle) -> Vec<ProviderSummary> {
    provider_catalog_from(providers::read_custom_providers(app).unwrap_or_default())
}

/// Pure conversion from the persisted [`TokenEntry`] list to the
/// request-auth-only [`StoredToken`] shape — split out from [`build_deps`]
/// so [`run_cli_server`] can build the same auth view from a config it
/// loaded itself, without duplicating the field list.
fn tokens_from_config(config: &ApiServerConfig) -> Vec<StoredToken> {
    config
        .tokens
        .iter()
        .map(|t| StoredToken {
            id: t.id.clone(),
            sha256: t.sha256.clone(),
            scopes: t.scopes.clone(),
            backends: t.backends.clone(),
        })
        .collect()
}

fn build_deps(app: &AppHandle, runtime: &ServerRuntime) -> ServerDeps {
    let state = app.state::<AppState>();
    let (llama_port, llama_ready, llama_model_stem, llama_embeddings_enabled) = {
        let llama = state.llama.lock().unwrap();
        let ready = llama.status == "ready";
        let stem = llama
            .model_path
            .as_deref()
            .and_then(|p| Path::new(p).file_stem())
            .map(|s| s.to_string_lossy().to_string());
        (llama.port, ready, stem, llama.embeddings_enabled)
    };

    // Re-read fresh from disk on every request (not cached in `runtime`) so
    // a token created or revoked while the server is already running takes
    // effect immediately — see the module doc's "hot vs. restart-gated
    // config" note. A read failure (corrupt file, permissions) fails closed:
    // an empty token list means every authenticated route 401s rather than
    // silently accepting anything.
    let tokens = config_file_path(app)
        .and_then(|p| load_config_impl(&p))
        .map(|cfg| tokens_from_config(&cfg))
        .unwrap_or_default();

    ServerDeps {
        llama_port,
        llama_ready,
        llama_model_stem,
        llama_embeddings_enabled,
        ollama_base_url: runtime.ollama_base_url.clone(),
        require_token: runtime.require_token,
        expose_ollama: runtime.expose_ollama,
        expose_providers: runtime.expose_providers,
        providers: build_provider_catalog(app),
        tokens,
        client: runtime.client.clone(),
    }
}

/// Probes the managed llama-server process's readiness and reports the
/// model id it advertises — the CLI-context substitute for reading
/// `AppState::llama` in-process (which only exists inside the GUI). Used
/// exclusively by [`run_cli_server`]: `lm-cli api-serve` runs as its own OS
/// process with no Tauri `AppState`, but llama-server (if the GUI already
/// started it) is a plain independent TCP listener on `port` that anyone on
/// loopback can reach, so a `/health` + `/v1/models` probe is a faithful
/// (if slightly higher-latency) stand-in for the GUI's in-memory
/// `LlamaState::status`/`model_path`. Any failure (unreachable, non-200,
/// unexpected body) is reported as "not ready" — same "absence is normal,
/// don't fail routing" stance as [`handle_models`]'s Ollama/provider
/// omission.
async fn probe_llama_server(client: &reqwest::Client, port: u16) -> (bool, Option<String>) {
    let healthy = client
        .get(format!("http://127.0.0.1:{port}/health"))
        .send()
        .await
        .map(|resp| resp.status().is_success())
        .unwrap_or(false);
    if !healthy {
        return (false, None);
    }

    let model_id = client
        .get(format!("http://127.0.0.1:{port}/v1/models"))
        .send()
        .await
        .ok()
        .and_then(|resp| resp.error_for_status().ok())
        .map(|resp| async move { resp.json::<serde_json::Value>().await.ok() });
    let model_id = match model_id {
        Some(fut) => fut.await,
        None => None,
    };
    let model_id = model_id
        .and_then(|v| v.get("data").and_then(|d| d.as_array()).and_then(|arr| arr.first().cloned()))
        .and_then(|entry| entry.get("id").and_then(|id| id.as_str()).map(str::to_string));

    (true, model_id)
}

/// Buffers a real hyper request's body into a single `Bytes` (unbounded —
/// chat-completion request bodies are small JSON payloads, unlike
/// `web.rs::tool_web_fetch`'s arbitrary-page responses, which do need a cap)
/// and hands off to the `AppHandle`-free [`handle_request`] core.
async fn serve_one_request(deps: ServerDeps, req: Request<Incoming>) -> Result<(Response<ResponseBody>, Option<String>), Infallible> {
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
        s.last_request_at = Some(now_ms());
        status_payload(&s)
    };
    // Emitted per-request rather than throttled — phase 1 traffic volume
    // (a human-driven external tool, not a benchmark loop) makes this a
    // non-issue; worth revisiting if that assumption changes.
    emit_status(app, &payload);
}

/// Pure update: sets `last_used_at` on the matching token only. Split out
/// from [`record_token_used`] so it's testable against a plain `AppState`
/// with no `AppHandle` — same `*_with_state_impl` shape as
/// `mcp.rs::add_server_with_state_impl`.
fn record_token_used_with_state(state: &AppState, path: &Path, token_id: &str) {
    let Ok(_guard) = state.api_server_config_lock.lock() else { return };
    let Ok(mut config) = load_config_impl(path) else { return };
    if let Some(entry) = config.tokens.iter_mut().find(|t| t.id == token_id) {
        entry.last_used_at = Some(now_ms());
        let _ = save_config_impl(path, &config);
    }
}

/// Best-effort: a failure to record "last used" (corrupt file, race with a
/// concurrent revoke) never fails the request that's already been served.
fn record_token_used(app: &AppHandle, token_id: &str) {
    let Ok(path) = config_file_path(app) else { return };
    record_token_used_with_state(&app.state::<AppState>(), &path, token_id);
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
                            let (resp, matched_token_id) = serve_one_request(deps, req).await?;
                            if let Some(token_id) = matched_token_id {
                                record_token_used(&app_for_req, &token_id);
                            }
                            bump_request_count(&app_for_req);
                            Ok::<_, Infallible>(resp)
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
/// [`start_server_core`] so the "port already in use" failure path is
/// directly unit-testable without a `#[tauri::command]`/`AppHandle` — see
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
}

// ---------------------------------------------------------------------
// lm-cli `api-serve` (design doc phase 4): the SAME routing/proxy core
// (`ServerDeps`/`serve_one_request`/`handle_request`/`bind_listener`/
// `load_config_impl`) the GUI uses, with no `AppHandle`/`AppState` at all —
// only the surrounding bookkeeping differs (stdout/stderr logging instead
// of `apiserver://status` events, an `AtomicU64` instead of
// `AppState::api_server.request_count`, an HTTP probe instead of reading
// `AppState::llama` in-process). `lm-cli`'s `main.rs` resolves the
// `api_server.json`/`providers.json` paths itself (the same
// `APP_IDENTIFIER`-hardcoding technique `providers_cli.rs`/
// `checkpoints_cli.rs` already use) and hands them in here — see the design
// doc's "config drift" risk note: this deliberately reads the SAME
// `api_server.json` the GUI writes, so tokens and toggles set in Settings
// carry over to the CLI and vice versa.
// ---------------------------------------------------------------------

/// Runs the local API server as a blocking, headless accept loop — never
/// returns on success (Ctrl+C/SIGINT ends the process the same way `ollama
/// serve`'s passthrough does); returns `Err` only for a bind failure, so
/// `lm-cli`'s `main` can print it and exit non-zero exactly like every other
/// subcommand's error path (`fail()`).
///
/// `load_custom_providers` is re-invoked on every accepted connection (not
/// cached once at startup) for the same "never stale" reasoning
/// [`build_deps`] applies to tokens — a provider added via the GUI's
/// Settings while `api-serve` is already running becomes routable
/// immediately, no CLI restart needed.
pub async fn run_cli_server(
    port: u16,
    config_path: PathBuf,
    load_custom_providers: impl Fn() -> Vec<providers::CustomProviderEntry> + Send + Sync + 'static,
) -> Result<(), String> {
    let listener = bind_listener(port).await?;
    println!("Little Monkey API server listening on http://127.0.0.1:{port}/v1 (Ctrl+C to stop)");

    let client = reqwest::Client::new();
    let llama_port = crate::llama::LlamaState::default().port;
    let request_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let load_custom_providers = Arc::new(load_custom_providers);
    // Guards this process's own `api_server.json` read-modify-write cycles
    // for the `last_used_at` bump below — a fresh, CLI-local `AppState` is
    // enough to serialize concurrent requests *within this process*; it
    // does not (and can't, being an in-memory lock) protect against a
    // simultaneously-running GUI process also writing the same file. That
    // cross-process race is the same pre-existing "shared JSON file" risk
    // the design doc's "config drift" note already flags — the atomic
    // temp+rename write in `save_config_impl` bounds it to "last writer
    // wins", never a torn file.
    let cli_state = Arc::new(AppState::default());

    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => continue, // transient accept error — keep serving
        };
        let io = TokioIo::new(stream);
        let client = client.clone();
        let config_path = config_path.clone();
        let request_count = request_count.clone();
        let load_custom_providers = load_custom_providers.clone();
        let cli_state = cli_state.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req: Request<Incoming>| {
                let client = client.clone();
                let config_path = config_path.clone();
                let request_count = request_count.clone();
                let load_custom_providers = load_custom_providers.clone();
                let cli_state = cli_state.clone();
                async move {
                    let config = load_config_impl(&config_path).unwrap_or_default();
                    let (llama_ready, llama_model_stem) = probe_llama_server(&client, llama_port).await;
                    let deps = ServerDeps {
                        llama_port,
                        llama_ready,
                        llama_model_stem,
                        // The CLI can't know whether the GUI started
                        // llama-server with `--embeddings` (that flag lives
                        // only in the GUI's in-memory `LlamaState`, not on
                        // disk) — conservatively `false`, so
                        // `POST /v1/embeddings` 501s with a clear message
                        // instead of guessing.
                        llama_embeddings_enabled: false,
                        ollama_base_url: ollama::OLLAMA_BASE_URL.to_string(),
                        require_token: config.require_token,
                        expose_ollama: config.expose_ollama,
                        expose_providers: config.expose_providers,
                        providers: provider_catalog_from(load_custom_providers()),
                        tokens: tokens_from_config(&config),
                        client,
                    };

                    let (resp, matched_token_id) = serve_one_request(deps, req).await?;
                    let n = request_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if let Some(token_id) = &matched_token_id {
                        record_token_used_with_state(&cli_state, &config_path, token_id);
                    }
                    eprintln!("[api-serve] request #{n} {} -> {}", req_log_hint(matched_token_id.as_deref()), resp.status());
                    Ok::<_, Infallible>(resp)
                }
            });
            let _ = http1::Builder::new().serve_connection(io, service).await;
        });
    }
}

/// Tiny formatting helper for [`run_cli_server`]'s per-request log line —
/// split out purely so the `eprintln!` above stays readable.
fn req_log_hint(token_id: Option<&str>) -> String {
    match token_id {
        Some(id) => format!("(token {id})"),
        None => "(no token)".to_string(),
    }
}

// ---------------------------------------------------------------------
// AppHandle-owning core start/stop, shared by the commands below and by
// lib.rs's autostart setup hook + api_server_set_config's restart path.
// ---------------------------------------------------------------------

/// Starts the server using whatever's currently persisted in
/// `api_server.json` (port/require_token/expose_*), stopping any previous
/// instance first (so re-starting after an edited port field rebinds
/// cleanly instead of erroring on our own still-open old listener). A bind
/// failure (most commonly: something else already has the port) surfaces
/// synchronously as `Err` *and* as `status: "error"` with `last_error` set —
/// never a silent no-op, never a panic. No `State` parameter — callers
/// without one (the `setup` autostart hook, `api_server_set_config`'s
/// restart) can call this with just an `AppHandle`.
async fn start_server_core(app: &AppHandle) -> Result<ApiServerStatusPayload, String> {
    let state = app.state::<AppState>();
    let _ = stop_server_core(app);

    let config = load_config_impl(&config_file_path(app)?)?;

    let listener = match bind_listener(config.port).await {
        Ok(listener) => listener,
        Err(message) => {
            let payload = {
                let mut s = state.api_server.lock().map_err(|e| e.to_string())?;
                record_bind_error(&mut s, message.clone());
                status_payload(&s)
            };
            emit_status(app, &payload);
            return Err(message);
        }
    };

    let shutdown = Arc::new(Notify::new());

    let payload = {
        let mut s = state.api_server.lock().map_err(|e| e.to_string())?;
        s.shutdown = Some(shutdown.clone());
        s.port = config.port;
        s.status = "running".to_string();
        s.request_count = 0;
        s.last_request_at = None;
        s.last_error = None;
        status_payload(&s)
    };
    emit_status(app, &payload);

    let runtime = Arc::new(ServerRuntime {
        client: reqwest::Client::new(),
        ollama_base_url: ollama::OLLAMA_BASE_URL.to_string(),
        require_token: config.require_token,
        expose_ollama: config.expose_ollama,
        expose_providers: config.expose_providers,
    });

    tokio::spawn(run_accept_loop(app.clone(), listener, shutdown, runtime));

    Ok(payload)
}

/// Stops the server if running (a no-op, not an error, if it's already
/// stopped).
fn stop_server_core(app: &AppHandle) -> Result<ApiServerStatusPayload, String> {
    let state = app.state::<AppState>();
    let payload = {
        let mut s = state.api_server.lock().map_err(|e| e.to_string())?;
        if let Some(shutdown) = s.shutdown.take() {
            shutdown.notify_one();
        }
        s.status = "stopped".to_string();
        status_payload(&s)
    };
    emit_status(app, &payload);
    Ok(payload)
}

// ---------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------

#[tauri::command]
pub async fn api_server_start(app: AppHandle) -> Result<ApiServerStatusPayload, String> {
    start_server_core(&app).await
}

#[tauri::command]
pub fn api_server_stop(app: AppHandle) -> Result<ApiServerStatusPayload, String> {
    stop_server_core(&app)
}

/// Returns the current status snapshot — same shape as the
/// `apiserver://status` event, for the Settings panel's initial load.
#[tauri::command]
pub fn api_server_status(state: State<'_, AppState>) -> Result<ApiServerStatusPayload, String> {
    let s = state.api_server.lock().map_err(|e| e.to_string())?;
    Ok(status_payload(&s))
}

#[tauri::command]
pub fn api_server_get_config(app: AppHandle) -> Result<ApiServerConfigView, String> {
    let config = load_config_impl(&config_file_path(&app)?)?;
    Ok(ApiServerConfigView::from(&config))
}

/// Persists `config` (merged onto the existing file's `tokens`, which this
/// view never carries) and reports whether the caller must gracefully
/// restart the running server — decided purely from `state.api_server`'s
/// in-memory status, so it's directly unit-testable without a real
/// `AppHandle`/listening socket (see `mcp.rs::add_server_with_state_impl`'s
/// doc comment for the same rationale).
fn set_config_with_state_impl(
    state: &AppState,
    path: &Path,
    config: ApiServerConfigView,
) -> Result<(ApiServerConfig, bool), String> {
    if config.port == 0 {
        return Err("Port must be between 1 and 65535".to_string());
    }

    let updated = {
        let _guard = state
            .api_server_config_lock
            .lock()
            .map_err(|_| "API server config lock poisoned".to_string())?;
        let mut existing = load_config_impl(path)?;
        existing.port = config.port;
        existing.autostart = config.autostart;
        existing.require_token = config.require_token;
        existing.expose_ollama = config.expose_ollama;
        existing.expose_providers = config.expose_providers;
        save_config_impl(path, &existing)?;
        existing
    };

    let needs_restart = {
        let s = state.api_server.lock().map_err(|e| e.to_string())?;
        s.status == "running" || s.status == "starting"
    };

    Ok((updated, needs_restart))
}

/// Updates the persisted config. Any change — port, autostart,
/// `require_token`, or either `expose_*` toggle — triggers a graceful
/// restart if the server is currently running, so the listening socket's
/// actual behavior can never silently drift from what the panel displays.
/// The `expose_providers` toggle is the "money-spending switch" per the
/// design doc; the explicit confirm for it lives in the frontend (this
/// command just persists whatever it's told).
#[tauri::command]
pub async fn api_server_set_config(
    app: AppHandle,
    state: State<'_, AppState>,
    config: ApiServerConfigView,
) -> Result<ApiServerConfigView, String> {
    let (updated, needs_restart) = set_config_with_state_impl(state.inner(), &config_file_path(&app)?, config)?;
    if needs_restart {
        stop_server_core(&app)?;
        start_server_core(&app).await?;
    }
    Ok(ApiServerConfigView::from(&updated))
}

/// Builds a fresh token: `lmk-` + 32 hex chars, plus the [`TokenEntry`] that
/// will be persisted (digest only). Split out from
/// [`create_token_with_state_impl`] so the "plaintext never ends up in the
/// persisted entry" property is testable with no file/lock/`AppState` at all
/// — see `tests::creating_a_token_never_persists_its_plaintext`.
fn mint_token(label: &str, scopes: Vec<Scope>, backends: Vec<Backend>) -> Result<(String, TokenEntry), String> {
    let label = label.trim().to_string();
    if label.is_empty() {
        return Err("Label is required".to_string());
    }
    if scopes.is_empty() {
        return Err("Select at least one scope".to_string());
    }
    if backends.is_empty() {
        return Err("Select at least one backend".to_string());
    }

    let token = generate_token();
    let entry = TokenEntry {
        id: Uuid::new_v4().to_string(),
        label,
        sha256: sha256_hex(&token),
        scopes,
        backends,
        created_at: now_ms(),
        last_used_at: None,
    };
    Ok((token, entry))
}

fn create_token_with_state_impl(
    state: &AppState,
    path: &Path,
    label: &str,
    scopes: Vec<Scope>,
    backends: Vec<Backend>,
) -> Result<(String, TokenEntry), String> {
    let (token, entry) = mint_token(label, scopes, backends)?;
    let _guard = state
        .api_server_config_lock
        .lock()
        .map_err(|_| "API server config lock poisoned".to_string())?;
    let mut config = load_config_impl(path)?;
    config.tokens.push(entry.clone());
    save_config_impl(path, &config)?;
    Ok((token, entry))
}

fn revoke_token_with_state_impl(state: &AppState, path: &Path, id: &str) -> Result<(), String> {
    let _guard = state
        .api_server_config_lock
        .lock()
        .map_err(|_| "API server config lock poisoned".to_string())?;
    let mut config = load_config_impl(path)?;
    let before = config.tokens.len();
    config.tokens.retain(|t| t.id != id);
    if config.tokens.len() == before {
        return Err(format!("Unknown token '{id}'"));
    }
    save_config_impl(path, &config)
}

/// The plaintext token, returned exactly once — the caller (the Settings
/// panel) must show/copy it now, since only [`TokenEntry::sha256`] is ever
/// persisted.
#[derive(Debug, Clone, Serialize)]
pub struct CreateTokenResult {
    pub token: String,
    pub entry: TokenEntryView,
}

#[tauri::command]
pub fn api_server_create_token(
    app: AppHandle,
    state: State<'_, AppState>,
    label: String,
    scopes: Vec<Scope>,
    backends: Vec<Backend>,
) -> Result<CreateTokenResult, String> {
    let (token, entry) = create_token_with_state_impl(state.inner(), &config_file_path(&app)?, &label, scopes, backends)?;
    Ok(CreateTokenResult { token, entry: TokenEntryView::from(&entry) })
}

#[tauri::command]
pub fn api_server_revoke_token(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<(), String> {
    revoke_token_with_state_impl(state.inner(), &config_file_path(&app)?, &id)
}

/// Never exposes `sha256` to the frontend — see [`TokenEntryView`].
#[tauri::command]
pub fn api_server_list_tokens(app: AppHandle) -> Result<Vec<TokenEntryView>, String> {
    let config = load_config_impl(&config_file_path(&app)?)?;
    Ok(config.tokens.iter().map(TokenEntryView::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn test_provider(id: &str, base_url: &str) -> ProviderSummary {
        ProviderSummary { id: id.to_string(), base_url: base_url.to_string() }
    }

    fn test_deps(ollama_base_url: String) -> ServerDeps {
        ServerDeps {
            llama_port: 8090,
            llama_ready: true,
            llama_model_stem: Some("qwen2.5-7b-instruct".to_string()),
            llama_embeddings_enabled: false,
            ollama_base_url,
            require_token: false,
            expose_ollama: true,
            expose_providers: false,
            providers: vec![test_provider("openai", "https://api.openai.com/v1"), test_provider("anthropic", "https://api.anthropic.com/v1")],
            tokens: Vec::new(),
            client: reqwest::Client::new(),
        }
    }

    fn stored_token(id: &str, plaintext: &str, scopes: Vec<Scope>, backends: Vec<Backend>) -> StoredToken {
        StoredToken { id: id.to_string(), sha256: sha256_hex(plaintext), scopes, backends }
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

    fn with_bearer(mut req: ServerRequest, token: &str) -> ServerRequest {
        req.headers.insert(header::AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
        req
    }

    async fn body_bytes(resp: Response<ResponseBody>) -> Bytes {
        resp.into_body().collect().await.unwrap().to_bytes()
    }

    fn temp_config_path() -> PathBuf {
        // Nanos alone can collide across parallel test threads — the atomic
        // counter guarantees uniqueness within the process (same idiom as
        // `prompts.rs::tests::temp_file`).
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("little_monkey_api_server_test_{}_{}_{}.json", std::process::id(), n, nanos))
    }

    fn no_providers() -> Vec<ProviderSummary> {
        Vec::new()
    }

    fn two_providers() -> Vec<ProviderSummary> {
        vec![test_provider("openai", "https://api.openai.com/v1"), test_provider("anthropic", "https://api.anthropic.com/v1")]
    }

    #[test]
    fn route_model_matches_llama_exactly() {
        assert_eq!(
            route_model("qwen2.5-7b-instruct", true, Some("qwen2.5-7b-instruct"), &no_providers()),
            ModelRoute::Llama
        );
    }

    #[test]
    fn route_model_falls_back_to_ollama_for_any_other_nonempty_id() {
        assert_eq!(route_model("llama3.1:8b", true, Some("qwen2.5-7b-instruct"), &no_providers()), ModelRoute::Ollama);
        // Even when llama isn't ready, a non-empty id is assumed to be an
        // Ollama tag — Ollama is the source of truth for whether it exists.
        assert_eq!(route_model("qwen2.5-7b-instruct", false, Some("qwen2.5-7b-instruct"), &no_providers()), ModelRoute::Ollama);
        assert_eq!(route_model("anything", true, None, &no_providers()), ModelRoute::Ollama);
    }

    #[test]
    fn route_model_is_unknown_only_when_blank() {
        assert_eq!(route_model("", true, Some("qwen2.5-7b-instruct"), &no_providers()), ModelRoute::Unknown);
        assert_eq!(route_model("   ", true, Some("qwen2.5-7b-instruct"), &no_providers()), ModelRoute::Unknown);
    }

    #[test]
    fn route_model_routes_a_known_provider_prefixed_id_to_providers() {
        assert_eq!(
            route_model("openai/gpt-4o", true, Some("qwen2.5-7b-instruct"), &two_providers()),
            ModelRoute::Providers { provider_id: "openai".to_string(), model_id: "gpt-4o".to_string() }
        );
        assert_eq!(
            route_model("anthropic/claude-opus-4-8", false, None, &two_providers()),
            ModelRoute::Providers { provider_id: "anthropic".to_string(), model_id: "claude-opus-4-8".to_string() }
        );
    }

    #[test]
    fn route_model_falls_back_to_ollama_for_a_slash_id_with_an_unknown_provider_prefix() {
        // "library/llama3" isn't a configured provider id, so it's treated
        // as an Ollama tag (Ollama namespaced tags can themselves contain a
        // slash) — exactly the design doc's "otherwise treat as Ollama tag"
        // fallback.
        assert_eq!(route_model("library/llama3", true, Some("qwen2.5-7b-instruct"), &two_providers()), ModelRoute::Ollama);
    }

    #[test]
    fn route_backend_maps_every_known_route_but_not_unknown() {
        assert_eq!(route_backend(&ModelRoute::Llama), Some(Backend::Local));
        assert_eq!(route_backend(&ModelRoute::Ollama), Some(Backend::Ollama));
        assert_eq!(
            route_backend(&ModelRoute::Providers { provider_id: "openai".to_string(), model_id: "gpt-4o".to_string() }),
            Some(Backend::Providers)
        );
        assert_eq!(route_backend(&ModelRoute::Unknown), None);
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

    /// Substantiates the reasoning in `constant_time_eq`'s doc comment: the
    /// function must reject a mismatch regardless of *where* in the digest
    /// the difference falls (a naive early-exit `==` would behave
    /// identically in terms of *correctness* here — this only pins down
    /// that behavior, since real timing can't be asserted in a unit test).
    #[test]
    fn constant_time_eq_rejects_mismatches_at_every_position() {
        let base = sha256_hex("lmk-fixed-value");
        let mut first_byte_flipped = base.clone();
        first_byte_flipped.replace_range(0..1, if &base[0..1] == "0" { "1" } else { "0" });
        let mut last_byte_flipped = base.clone();
        let last = base.len() - 1;
        last_byte_flipped.replace_range(last..last + 1, if &base[last..last + 1] == "0" { "1" } else { "0" });

        assert!(!constant_time_eq(&base, &first_byte_flipped));
        assert!(!constant_time_eq(&base, &last_byte_flipped));
        assert!(constant_time_eq(&base, &base.clone()));
    }

    #[tokio::test]
    async fn health_requires_no_token_even_when_auth_is_on() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.tokens = vec![stored_token("t1", "lmk-real-token", vec![Scope::Chat], vec![Backend::Local])];

        let (resp, matched) = handle_request(&deps, get_request("/health")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(matched.is_none());
        let bytes = body_bytes(resp).await;
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["status"], "ok");
    }

    #[tokio::test]
    async fn missing_or_wrong_bearer_token_is_rejected_on_protected_routes() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.tokens = vec![stored_token("t1", "lmk-real-token", vec![Scope::Models], vec![Backend::Local, Backend::Ollama])];

        let (resp, matched) = handle_request(&deps, get_request("/v1/models")).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(matched.is_none());

        let wrong_auth = with_bearer(get_request("/v1/models"), "lmk-not-it");
        let (resp, matched) = handle_request(&deps, wrong_auth).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(matched.is_none());
    }

    #[tokio::test]
    async fn no_tokens_configured_fails_closed_even_with_a_bearer_header() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        // `deps.tokens` deliberately left empty.
        let req = with_bearer(get_request("/v1/models"), "lmk-anything");
        let (resp, _) = handle_request(&deps, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn correct_bearer_token_is_accepted_and_reports_its_id_for_last_used_tracking() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.llama_ready = false;
        deps.tokens = vec![stored_token("tok-1", "lmk-real-token", vec![Scope::Models], vec![Backend::Local, Backend::Ollama])];

        let req = with_bearer(get_request("/v1/models"), "lmk-real-token");
        let (resp, matched) = handle_request(&deps, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(matched.as_deref(), Some("tok-1"));
    }

    /// Phase 4 addition: `request_count` was already wired end to end since
    /// phase 1 — this pins down the other half of the design doc's
    /// "request counter/last-request display" parity item, `last_request_at`,
    /// mirroring exactly what [`bump_request_count`] does to `ApiServerState`
    /// (a fresh `AppHandle` isn't available in a unit test — see this
    /// module's other `AppState`-only, `AppHandle`-free tests for why).
    #[test]
    fn last_request_at_starts_none_and_is_set_once_a_request_is_recorded() {
        let mut state = ApiServerState::default();
        assert_eq!(state.last_request_at, None);
        assert_eq!(status_payload(&state).last_request_at, None);

        state.request_count += 1;
        state.last_request_at = Some(now_ms());

        let payload = status_payload(&state);
        assert_eq!(payload.request_count, 1);
        assert!(payload.last_request_at.is_some());
    }

    #[tokio::test]
    async fn token_missing_the_required_scope_is_rejected_with_403() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.tokens = vec![stored_token("tok-models-only", "lmk-models-only", vec![Scope::Models], vec![Backend::Local, Backend::Ollama])];

        let req = with_bearer(post_request("/v1/chat/completions", r#"{"model":"qwen2.5-7b-instruct"}"#), "lmk-models-only");
        let (resp, matched) = handle_request(&deps, req).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        // Still authenticated as a real token, even though this particular
        // route is out of scope for it — its id is still reported.
        assert_eq!(matched.as_deref(), Some("tok-models-only"));
    }

    #[tokio::test]
    async fn token_scoped_to_local_backend_is_rejected_when_the_request_routes_to_ollama() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.tokens = vec![stored_token("tok-local-only", "lmk-local-only", vec![Scope::Chat], vec![Backend::Local])];

        // "llama3.1:8b" isn't the ready llama stem, so `route_model` sends
        // it to Ollama — a token scoped to `local` only must be rejected.
        let req = with_bearer(post_request("/v1/chat/completions", r#"{"model":"llama3.1:8b"}"#), "lmk-local-only");
        let (resp, _) = handle_request(&deps, req).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn token_scoped_to_the_matching_backend_is_accepted() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.tokens = vec![stored_token("tok-ollama", "lmk-ollama-scoped", vec![Scope::Chat], vec![Backend::Ollama])];

        let req = with_bearer(post_request("/v1/chat/completions", r#"{"model":"llama3.1:8b"}"#), "lmk-ollama-scoped");
        let (resp, matched) = handle_request(&deps, req).await;
        // 502, not 403: the scope/backend check passed, and it proceeded to
        // (unsuccessfully) proxy to the dummy unreachable address.
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(matched.as_deref(), Some("tok-ollama"));
    }

    #[tokio::test]
    async fn unmatched_routes_404() {
        let deps = test_deps("http://127.0.0.1:1".to_string());
        let (resp, _) = handle_request(&deps, get_request("/v1/embeddings")).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        // Never, ever a tool-dispatch route.
        let (resp, _) = handle_request(&deps, post_request("/v1/tool_run_shell", "{}")).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn chat_completions_with_blank_model_returns_404() {
        let deps = test_deps("http://127.0.0.1:1".to_string());
        let (resp, _) = handle_request(&deps, post_request("/v1/chat/completions", r#"{"messages":[]}"#)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn chat_completions_with_invalid_json_returns_400() {
        let deps = test_deps("http://127.0.0.1:1".to_string());
        let (resp, _) = handle_request(&deps, post_request("/v1/chat/completions", "not json")).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn models_endpoint_lists_the_ready_llama_model_as_local() {
        // Point "Ollama" at an address nothing listens on — `/v1/models`
        // must still succeed, just omitting Ollama's models (mirrors
        // `ollama.rs`'s own "unreachable is normal" stance).
        let deps = test_deps("http://127.0.0.1:1".to_string());
        let (resp, _) = handle_request(&deps, get_request("/v1/models")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body_bytes(resp).await;
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let data = value["data"].as_array().unwrap();
        assert!(data.iter().any(|m| m["id"] == "qwen2.5-7b-instruct" && m["owned_by"] == "local"));
    }

    #[tokio::test]
    async fn models_endpoint_omits_ollama_entirely_when_expose_ollama_is_off() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.expose_ollama = false;
        let (resp, _) = handle_request(&deps, get_request("/v1/models")).await;
        let bytes = body_bytes(resp).await;
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let data = value["data"].as_array().unwrap();
        assert!(data.iter().all(|m| m["owned_by"] != "ollama"));
    }

    #[tokio::test]
    async fn chat_completions_404s_for_an_ollama_routed_model_when_expose_ollama_is_off() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.expose_ollama = false;
        let (resp, _) = handle_request(&deps, post_request("/v1/chat/completions", r#"{"model":"llama3.1:8b"}"#)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn chat_completions_404s_for_a_provider_routed_model_when_expose_providers_is_off() {
        // `test_deps` defaults `expose_providers` to `false` — a
        // provider-prefixed id must 404 exactly like an unexposed Ollama tag,
        // never silently proxy anyway.
        let deps = test_deps("http://127.0.0.1:1".to_string());
        let (resp, _) = handle_request(&deps, post_request("/v1/chat/completions", r#"{"model":"openai/gpt-4o"}"#)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// The core phase-3 security property: a token missing `Backend::Providers`
    /// must be rejected with `403` on a provider-routed request, even when
    /// `expose_providers` is globally on — the global toggle and the
    /// per-token scope are two independent gates. Critically, this check
    /// must happen *before* any keychain lookup — the assertion here doesn't
    /// depend on whether a real key happens to be configured for "openai" on
    /// the machine running this test.
    #[tokio::test]
    async fn token_without_providers_backend_is_rejected_even_when_expose_providers_is_globally_on() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.expose_providers = true;
        deps.require_token = true;
        deps.tokens = vec![stored_token(
            "tok-no-providers",
            "lmk-no-providers",
            vec![Scope::Chat],
            vec![Backend::Local, Backend::Ollama],
        )];

        let req = with_bearer(post_request("/v1/chat/completions", r#"{"model":"openai/gpt-4o"}"#), "lmk-no-providers");
        let (resp, matched) = handle_request(&deps, req).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(matched.as_deref(), Some("tok-no-providers"));
    }

    /// The positive control for the test above: a token that *does* carry
    /// `Backend::Providers` gets past the scope/backend gate (never a `403`)
    /// once `expose_providers` is on — whatever happens next depends on
    /// keychain state (a real key configured -> attempts the network call;
    /// none configured -> `502 provider_not_configured`), but it must never
    /// be blocked by scope.
    #[tokio::test]
    async fn token_with_providers_backend_is_not_blocked_by_scope_when_expose_providers_is_on() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.expose_providers = true;
        deps.require_token = true;
        // A provider id that should never realistically have a real
        // keychain entry on the machine running this test, so the outcome
        // here is deterministic regardless of what's actually configured.
        deps.providers = vec![test_provider("zzz-test-provider-no-key", "https://example.invalid/v1")];
        deps.tokens = vec![stored_token(
            "tok-with-providers",
            "lmk-with-providers",
            vec![Scope::Chat],
            vec![Backend::Providers],
        )];

        let req = with_bearer(
            post_request("/v1/chat/completions", r#"{"model":"zzz-test-provider-no-key/some-model"}"#),
            "lmk-with-providers",
        );
        let (resp, matched) = handle_request(&deps, req).await;
        assert_ne!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY, "no key is configured for this fabricated provider id, so it must fail as 'not configured', not silently succeed");
        assert_eq!(matched.as_deref(), Some("tok-with-providers"));
    }

    #[tokio::test]
    async fn chat_completions_provider_route_reports_provider_not_configured_with_no_token_required() {
        // A provider id with no saved key deterministically 502s before ever
        // sending a request — this only exercises the routing decision (and
        // that it's reachable with `require_token: false`, unlike the
        // token-scoped variant above), not the actual proxying, which needs
        // a real network call to a mock upstream to exercise.
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.expose_providers = true;
        deps.providers = vec![test_provider("zzz-test-provider-no-key", "https://example.invalid/v1")];

        let (resp, _) = handle_request(
            &deps,
            post_request("/v1/chat/completions", r#"{"model":"zzz-test-provider-no-key/some-model"}"#),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let bytes = body_bytes(resp).await;
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "provider_not_configured");
    }

    #[tokio::test]
    async fn models_endpoint_omits_provider_models_when_no_key_is_configured() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.expose_providers = true;
        deps.providers = vec![test_provider("zzz-test-provider-no-key", "https://example.invalid/v1")];

        let (resp, _) = handle_request(&deps, get_request("/v1/models")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = body_bytes(resp).await;
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let data = value["data"].as_array().unwrap();
        assert!(data.iter().all(|m| m["owned_by"] != "zzz-test-provider-no-key"));
    }

    #[tokio::test]
    async fn embeddings_501s_when_llama_wasnt_started_with_embeddings() {
        // `test_deps` defaults `llama_embeddings_enabled` to `false`.
        let deps = test_deps("http://127.0.0.1:1".to_string());
        let (resp, _) =
            handle_request(&deps, post_request("/v1/embeddings", r#"{"model":"qwen2.5-7b-instruct"}"#)).await;
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
        let bytes = body_bytes(resp).await;
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "embeddings_not_enabled");
    }

    #[tokio::test]
    async fn embeddings_proxies_to_llama_when_embeddings_enabled() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let body = r#"{"object":"list","data":[]}"#;
                let response =
                    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                let _ = stream.write_all(response.as_bytes());
            }
        });

        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.llama_port = addr.port();
        deps.llama_embeddings_enabled = true;
        let (resp, _) =
            handle_request(&deps, post_request("/v1/embeddings", r#"{"model":"qwen2.5-7b-instruct"}"#)).await;
        assert_eq!(resp.status(), StatusCode::OK);

        handle.join().unwrap();
    }

    #[tokio::test]
    async fn embeddings_501s_for_a_provider_routed_model() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.expose_providers = true;
        let (resp, _) = handle_request(&deps, post_request("/v1/embeddings", r#"{"model":"openai/text-embedding-3-small"}"#)).await;
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
        let bytes = body_bytes(resp).await;
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "embeddings_not_supported");
    }

    #[tokio::test]
    async fn embeddings_requires_the_embeddings_scope() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.tokens = vec![stored_token("tok-chat-only", "lmk-chat-only", vec![Scope::Chat], vec![Backend::Local, Backend::Ollama])];

        let req = with_bearer(post_request("/v1/embeddings", r#"{"model":"qwen2.5-7b-instruct"}"#), "lmk-chat-only");
        let (resp, _) = handle_request(&deps, req).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn embeddings_with_blank_model_returns_404() {
        let deps = test_deps("http://127.0.0.1:1".to_string());
        let (resp, _) = handle_request(&deps, post_request("/v1/embeddings", r#"{}"#)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn options_preflight_on_v1_routes_returns_cors_headers_and_needs_no_token() {
        let mut deps = test_deps("http://127.0.0.1:1".to_string());
        deps.require_token = true;
        deps.tokens = vec![stored_token("t1", "lmk-real-token", vec![Scope::Chat], vec![Backend::Local])];

        let req = ServerRequest {
            method: Method::OPTIONS,
            path: "/v1/chat/completions".to_string(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
        };
        let (resp, matched) = handle_request(&deps, req).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(resp.headers().get("access-control-allow-origin").unwrap(), "*");
        assert!(resp.headers().get("access-control-allow-methods").is_some());
        assert!(matched.is_none());
    }

    #[tokio::test]
    async fn every_response_carries_the_cors_allow_origin_header() {
        let deps = test_deps("http://127.0.0.1:1".to_string());
        let (resp, _) = handle_request(&deps, get_request("/health")).await;
        assert_eq!(resp.headers().get("access-control-allow-origin").unwrap(), "*");

        let (resp, _) = handle_request(&deps, get_request("/v1/models")).await;
        assert_eq!(resp.headers().get("access-control-allow-origin").unwrap(), "*");

        let (resp, _) = handle_request(&deps, get_request("/v1/nope")).await;
        assert_eq!(resp.headers().get("access-control-allow-origin").unwrap(), "*");
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
        let (resp, _) = handle_request(&deps, post_request("/v1/chat/completions", r#"{"model":"llama3.1:8b","stream":true}"#)).await;
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
        let (resp, _) = handle_request(&deps, post_request("/v1/chat/completions", r#"{"model":"llama3.1:8b"}"#)).await;
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
    }

    #[tokio::test]
    async fn bind_listener_succeeds_on_an_available_port() {
        let listener = bind_listener(0).await.expect("binding port 0 (OS-assigned) should always succeed");
        assert!(listener.local_addr().unwrap().port() > 0);
    }

    #[test]
    fn creating_a_token_never_persists_its_plaintext() {
        let (token, entry) = mint_token("CI", vec![Scope::Chat], vec![Backend::Local]).unwrap();
        assert_ne!(entry.sha256, token, "the persisted entry must never contain the plaintext token");
        assert_eq!(entry.sha256, sha256_hex(&token));

        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains(&token), "serialized TokenEntry must never contain the plaintext token");
    }

    #[test]
    fn mint_token_rejects_blank_label_or_empty_scopes_or_backends() {
        assert!(mint_token("", vec![Scope::Chat], vec![Backend::Local]).is_err());
        assert!(mint_token("   ", vec![Scope::Chat], vec![Backend::Local]).is_err());
        assert!(mint_token("ok", vec![], vec![Backend::Local]).is_err());
        assert!(mint_token("ok", vec![Scope::Chat], vec![]).is_err());
        assert!(mint_token("ok", vec![Scope::Chat], vec![Backend::Local]).is_ok());
    }

    #[test]
    fn token_entry_view_never_serializes_the_digest() {
        let entry = TokenEntry {
            id: "a".to_string(),
            label: "A".to_string(),
            sha256: sha256_hex("lmk-secret-value"),
            scopes: vec![Scope::Chat],
            backends: vec![Backend::Local],
            created_at: 1,
            last_used_at: None,
        };
        let view = TokenEntryView::from(&entry);
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("sha256"));
        assert!(!json.contains(&entry.sha256));
    }

    #[test]
    fn load_config_defaults_when_file_is_missing() {
        let path = temp_config_path();
        let config = load_config_impl(&path).unwrap();
        assert_eq!(config.port, DEFAULT_PORT);
        assert!(!config.autostart);
        assert!(config.require_token);
        assert!(config.expose_ollama);
        assert!(!config.expose_providers);
        assert!(config.tokens.is_empty());
    }

    #[test]
    fn config_round_trips_through_save_and_load() {
        let path = temp_config_path();
        let mut config = ApiServerConfig::default();
        config.port = 4444;
        config.autostart = true;
        config.tokens.push(TokenEntry {
            id: "a".to_string(),
            label: "CI".to_string(),
            sha256: sha256_hex("lmk-ci"),
            scopes: vec![Scope::Chat, Scope::Models],
            backends: vec![Backend::Local],
            created_at: 1,
            last_used_at: None,
        });

        save_config_impl(&path, &config).unwrap();
        let loaded = load_config_impl(&path).unwrap();

        assert_eq!(loaded.port, 4444);
        assert!(loaded.autostart);
        assert_eq!(loaded.tokens.len(), 1);
        assert_eq!(loaded.tokens[0].sha256, sha256_hex("lmk-ci"));
        assert!(!path.with_extension("json.tmp").exists());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn set_config_reports_restart_needed_only_when_the_server_is_running() {
        let path = temp_config_path();
        let state = AppState::default();

        let view = ApiServerConfigView { port: 1234, autostart: false, require_token: true, expose_ollama: true, expose_providers: false };
        let (_, needs_restart) = set_config_with_state_impl(&state, &path, view).unwrap();
        assert!(!needs_restart, "server is stopped — no restart needed");

        {
            let mut s = state.api_server.lock().unwrap();
            s.status = "running".to_string();
        }
        let view = ApiServerConfigView { port: 5555, autostart: false, require_token: true, expose_ollama: true, expose_providers: false };
        let (updated, needs_restart) = set_config_with_state_impl(&state, &path, view).unwrap();
        assert!(needs_restart, "server is running — a config change must trigger a restart");
        assert_eq!(updated.port, 5555);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn set_config_rejects_a_zero_port() {
        let path = temp_config_path();
        let state = AppState::default();
        let view = ApiServerConfigView { port: 0, autostart: false, require_token: true, expose_ollama: true, expose_providers: false };
        assert!(set_config_with_state_impl(&state, &path, view).is_err());
    }

    #[test]
    fn create_and_revoke_token_round_trip_through_the_config_file() {
        let path = temp_config_path();
        let state = AppState::default();

        let (token, entry) =
            create_token_with_state_impl(&state, &path, "My IDE", vec![Scope::Chat, Scope::Models], vec![Backend::Local]).unwrap();
        assert!(token.starts_with(TOKEN_PREFIX));

        let loaded = load_config_impl(&path).unwrap();
        assert_eq!(loaded.tokens.len(), 1);
        assert_eq!(loaded.tokens[0].id, entry.id);
        assert_eq!(loaded.tokens[0].sha256, sha256_hex(&token));

        revoke_token_with_state_impl(&state, &path, &entry.id).unwrap();
        let loaded = load_config_impl(&path).unwrap();
        assert!(loaded.tokens.is_empty());

        assert!(revoke_token_with_state_impl(&state, &path, &entry.id).is_err(), "revoking an already-gone id must error, not silently succeed");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn record_token_used_sets_last_used_at_on_the_matching_entry_only() {
        let path = temp_config_path();
        let mut config = ApiServerConfig::default();
        config.tokens.push(TokenEntry {
            id: "a".to_string(),
            label: "A".to_string(),
            sha256: sha256_hex("lmk-a"),
            scopes: vec![],
            backends: vec![],
            created_at: 1,
            last_used_at: None,
        });
        config.tokens.push(TokenEntry {
            id: "b".to_string(),
            label: "B".to_string(),
            sha256: sha256_hex("lmk-b"),
            scopes: vec![],
            backends: vec![],
            created_at: 1,
            last_used_at: None,
        });
        save_config_impl(&path, &config).unwrap();

        let state = AppState::default();
        record_token_used_with_state(&state, &path, "b");

        let reloaded = load_config_impl(&path).unwrap();
        assert!(reloaded.tokens.iter().find(|t| t.id == "a").unwrap().last_used_at.is_none());
        assert!(reloaded.tokens.iter().find(|t| t.id == "b").unwrap().last_used_at.is_some());

        let _ = std::fs::remove_file(&path);
    }

    /// Phase 4 regression test: phases 2-3 layered `api_server_set_config`'s
    /// stop-then-start restart path on top of phase 1's bind-conflict
    /// handling — this confirms that sequence (not just a bare fresh start)
    /// still surfaces a conflicting port as `status: "error"` promptly,
    /// never a hang or a panic. Exercises the exact primitives
    /// `start_server_core`/`stop_server_core` use (`ApiServerState`,
    /// `bind_listener`, `record_bind_error`) rather than the commands
    /// themselves, since those need a real `AppHandle` whose mocked
    /// `app_data_dir()` resolves to the developer's actual OS config
    /// directory (see this module's doc comment on why every other I/O path
    /// here is tested through a `*_with_state_impl`/`*_impl` taking an
    /// explicit `&Path` instead) — `tokio::time::timeout` is the hang guard.
    #[tokio::test]
    async fn config_triggered_restart_onto_an_already_taken_port_surfaces_status_error_without_hanging() {
        let mut state = ApiServerState::default();

        // Simulates a healthy running server on an OS-assigned port, exactly
        // what `start_server_core` leaves behind on a successful bind.
        let first_listener = bind_listener(0).await.unwrap();
        state.port = first_listener.local_addr().unwrap().port();
        state.status = "running".to_string();
        state.shutdown = Some(Arc::new(Notify::new()));

        // Something else holds the port the new config wants — e.g. LM
        // Studio, or the port field was edited to collide with another app.
        let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let conflicting_port = blocker.local_addr().unwrap().port();

        // `api_server_set_config`'s restart path: stop (notify + clear the
        // handle) immediately followed by a fresh bind attempt on the new
        // port — the same order `start_server_core`'s own leading
        // `stop_server_core` call plus `api_server_set_config`'s explicit
        // one produce.
        if let Some(shutdown) = state.shutdown.take() {
            shutdown.notify_one();
        }
        state.status = "stopped".to_string();

        let bind_result = tokio::time::timeout(std::time::Duration::from_secs(2), bind_listener(conflicting_port))
            .await
            .expect("bind_listener must not hang when the target port is already taken");

        match bind_result {
            Ok(_) => panic!("expected a bind conflict against an already-bound port"),
            Err(message) => record_bind_error(&mut state, message),
        }

        assert_eq!(state.status, "error");
        assert!(state.last_error.is_some());
        assert!(state.shutdown.is_none());

        drop(first_listener);
        drop(blocker);
    }

    // -------------------------------------------------------------
    // Phase 4: lm-cli `api-serve` reuse (`provider_catalog_from`,
    // `tokens_from_config`, `probe_llama_server`)
    // -------------------------------------------------------------

    #[test]
    fn provider_catalog_from_merges_presets_and_custom_entries() {
        let custom = vec![providers::CustomProviderEntry {
            id: "my-local-router".to_string(),
            label: "My Router".to_string(),
            base_url: "http://127.0.0.1:9999/v1".to_string(),
        }];
        let catalog = provider_catalog_from(custom);

        // Every built-in preset id is still present...
        assert!(catalog.iter().any(|p| p.id == "openai"));
        assert!(catalog.iter().any(|p| p.id == "anthropic"));
        // ...alongside the custom entry.
        assert!(catalog.iter().any(|p| p.id == "my-local-router" && p.base_url == "http://127.0.0.1:9999/v1"));
    }

    #[test]
    fn provider_catalog_from_with_no_custom_entries_is_just_the_presets() {
        let presets_len = providers::providers_list_presets().len();
        assert_eq!(provider_catalog_from(Vec::new()).len(), presets_len);
    }

    #[test]
    fn tokens_from_config_carries_every_field_needed_for_auth() {
        let mut config = ApiServerConfig::default();
        config.tokens.push(TokenEntry {
            id: "tok-1".to_string(),
            label: "CI".to_string(),
            sha256: sha256_hex("lmk-cli-token"),
            scopes: vec![Scope::Chat, Scope::Embeddings],
            backends: vec![Backend::Local, Backend::Ollama],
            created_at: 1,
            last_used_at: None,
        });

        let tokens = tokens_from_config(&config);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].id, "tok-1");
        assert_eq!(tokens[0].sha256, sha256_hex("lmk-cli-token"));
        assert_eq!(tokens[0].scopes, vec![Scope::Chat, Scope::Embeddings]);
        assert_eq!(tokens[0].backends, vec![Backend::Local, Backend::Ollama]);
    }

    #[tokio::test]
    async fn probe_llama_server_reports_not_ready_when_unreachable() {
        // Port 1 on loopback: nothing listens there.
        let client = reqwest::Client::new();
        let (ready, model_id) = probe_llama_server(&client, 1).await;
        assert!(!ready);
        assert!(model_id.is_none());
    }

    #[tokio::test]
    async fn probe_llama_server_reports_ready_and_model_id_when_healthy() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let handle = std::thread::spawn(move || {
            // /health
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let body = r#"{"status":"ok"}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
            // /v1/models
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let body = r#"{"object":"list","data":[{"id":"qwen2.5-7b-instruct","object":"model"}]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });

        let client = reqwest::Client::new();
        let (ready, model_id) = probe_llama_server(&client, addr.port()).await;
        assert!(ready);
        assert_eq!(model_id.as_deref(), Some("qwen2.5-7b-instruct"));

        handle.join().unwrap();
    }

    /// Regression guard for the phase-4 CLI reuse: `run_cli_server` must
    /// still surface a conflicting port as an `Err` (so `lm-cli`'s `fail()`
    /// prints it and exits non-zero) rather than hanging or panicking —
    /// mirrors `config_triggered_restart_onto_an_already_taken_port_...`
    /// above, but through the actual public entry point `lm-cli` calls.
    #[tokio::test]
    async fn run_cli_server_reports_a_bind_conflict_as_an_error() {
        let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = blocker.local_addr().unwrap().port();
        let path = temp_config_path();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run_cli_server(port, path.clone(), Vec::new),
        )
        .await
        .expect("run_cli_server must not hang on a bind conflict");

        assert!(result.is_err());
        drop(blocker);
        let _ = std::fs::remove_file(&path);
    }
}
